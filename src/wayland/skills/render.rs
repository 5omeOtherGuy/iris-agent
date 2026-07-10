// SPDX-License-Identifier: Apache-2.0
// Derived from OpenAI Codex core-skills/src/render.rs; adapted to Iris's token
// estimator and provider-neutral contextual-message pipeline.

use super::model::{SkillLoadOutcome, SkillMetadata, SkillScope};

const DEFAULT_METADATA_CHAR_BUDGET: usize = 8_000;
const CONTEXT_WINDOW_PERCENT: u64 = 2;

const SKILLS_INTRO: &str = "A skill is a set of instructions provided through a `SKILL.md` source. Below is the list of skills that can be used. Each entry includes a name, description, and source locator. `file` locators are on the host filesystem.";

const SKILLS_HOW_TO_USE: &str = r#"- Discovery: The list above is the skills available in this session (name + description + source locator). `file` entries live on the host filesystem.
- Trigger rules: If the user names a skill (with `$SkillName` or plain text) OR the task clearly matches a skill's description shown above, you must use that skill for that turn. Multiple mentions mean use them all. Do not carry skills across turns unless re-mentioned.
- Missing/blocked: If a named skill isn't in the list or its source can't be read, say so briefly and continue with the best fallback.
- How to use a skill (progressive disclosure):
  1) After deciding to use a skill, open and read its `SKILL.md` completely before taking task actions. If a read is truncated or paginated, continue until EOF.
  2) When `SKILL.md` references another resource, resolve relative paths against the directory containing that `SKILL.md`.
  3) If `SKILL.md` points to extra folders such as `references/`, use its routing instructions to identify the resources required for the task. Read each required instruction or reference file before acting on it.
  4) Prefer running or patching provided scripts instead of retyping large code blocks.
  5) Reuse provided assets or templates instead of recreating them.
- Coordination and sequencing:
  - If multiple skills apply, choose the minimal set that covers the request and state the order you'll use them.
  - Announce which skill(s) you're using and why. If you skip an obvious skill, say why.
- Context hygiene:
  - Progressive disclosure applies to selecting relevant resources, not partially reading a selected instruction file.
  - Avoid deep reference-chasing: prefer resources directly linked from `SKILL.md` unless blocked.
  - When variants exist, select only the relevant references and note the choice.
- Safety and fallback: If a skill can't be applied cleanly, state the issue, choose the best alternative, and continue."#;

#[derive(Debug, Clone, Copy)]
enum Budget {
    Tokens(usize),
    Characters(usize),
}

impl Budget {
    fn limit(self) -> usize {
        match self {
            Self::Tokens(value) | Self::Characters(value) => value,
        }
    }

    fn cost(self, value: &str) -> usize {
        match self {
            Self::Tokens(_) => value.len().div_ceil(4),
            Self::Characters(_) => value.chars().count(),
        }
    }

    fn approximate_chars(self, units: usize) -> usize {
        match self {
            Self::Tokens(_) => units.saturating_mul(4),
            Self::Characters(_) => units,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenderedSkills {
    pub(super) instructions: Option<String>,
    pub(super) warning: Option<String>,
}

pub(super) fn render(outcome: &SkillLoadOutcome, context_budget: Option<u64>) -> RenderedSkills {
    let mut skills = outcome
        .skills
        .iter()
        .filter(|skill| skill.policy.allow_implicit_invocation)
        .collect::<Vec<_>>();
    skills.sort_by(|a, b| {
        prompt_scope_rank(a.scope)
            .cmp(&prompt_scope_rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    if skills.is_empty() {
        return RenderedSkills {
            instructions: None,
            warning: None,
        };
    }

    let budget = context_budget
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .map(|value| {
            Budget::Tokens(
                value
                    .saturating_mul(CONTEXT_WINDOW_PERCENT as usize)
                    .saturating_div(100)
                    .max(1),
            )
        })
        .unwrap_or(Budget::Characters(DEFAULT_METADATA_CHAR_BUDGET));
    let full = skills
        .iter()
        .map(|skill| skill_line(skill, Some(&skill.description)))
        .collect::<Vec<_>>();
    let full_cost = lines_cost(&full, budget);

    let (lines, omitted, truncated) = if full_cost <= budget.limit() {
        (full, 0usize, false)
    } else {
        fit_lines(&skills, budget)
    };

    let body = format!(
        "<skills_instructions>\n## Skills\n{SKILLS_INTRO}\n### Available skills\n{}\n### How to use skills\n{SKILLS_HOW_TO_USE}\n</skills_instructions>",
        lines.join("\n")
    );
    let warning = if omitted > 0 {
        Some(format!(
            "Exceeded skills context budget of 2%. {omitted} additional {} not included in the model-visible skills list.",
            if omitted == 1 {
                "skill was"
            } else {
                "skills were"
            }
        ))
    } else if truncated {
        Some(
            "Skill descriptions were shortened to fit the 2% skills context budget. The model can still see every skill."
                .to_string(),
        )
    } else {
        None
    };

    RenderedSkills {
        instructions: Some(body),
        warning,
    }
}

fn fit_lines(skills: &[&SkillMetadata], budget: Budget) -> (Vec<String>, usize, bool) {
    let minimum = skills
        .iter()
        .map(|skill| skill_line(skill, None))
        .collect::<Vec<_>>();
    let minimum_cost = lines_cost(&minimum, budget);
    if minimum_cost > budget.limit() {
        let mut included = Vec::new();
        let mut used = 0usize;
        for line in minimum {
            let cost = budget.cost(&line).saturating_add(1);
            if used.saturating_add(cost) <= budget.limit() {
                used = used.saturating_add(cost);
                included.push(line);
            }
        }
        let omitted = skills.len().saturating_sub(included.len());
        return (included, omitted, true);
    }

    let remaining = budget.approximate_chars(budget.limit().saturating_sub(minimum_cost));
    let per_skill = remaining.checked_div(skills.len()).unwrap_or(0);
    let mut description_lengths = skills
        .iter()
        .map(|skill| skill.description.chars().count().min(per_skill))
        .collect::<Vec<_>>();
    let mut lines = render_with_lengths(skills, &description_lengths);
    let mut cost = lines_cost(&lines, budget);
    while cost > budget.limit() {
        let Some(index) = description_lengths.iter().rposition(|length| *length > 0) else {
            break;
        };
        description_lengths[index] -= 1;
        lines[index] = skill_line_prefix(skills[index], description_lengths[index]);
        cost = lines_cost(&lines, budget);
    }
    let truncated = skills
        .iter()
        .zip(description_lengths)
        .any(|(skill, kept)| kept < skill.description.chars().count());
    (lines, 0, truncated)
}

fn render_with_lengths(skills: &[&SkillMetadata], lengths: &[usize]) -> Vec<String> {
    skills
        .iter()
        .zip(lengths)
        .map(|(skill, length)| skill_line_prefix(skill, *length))
        .collect()
}

fn skill_line_prefix(skill: &SkillMetadata, description_chars: usize) -> String {
    let description = skill
        .description
        .chars()
        .take(description_chars)
        .collect::<String>();
    skill_line(
        skill,
        (!description.is_empty()).then_some(description.as_str()),
    )
}

fn skill_line(skill: &SkillMetadata, description: Option<&str>) -> String {
    let path = skill.path.display();
    match description {
        Some(description) if !description.is_empty() => {
            format!("- {}: {} (file: {path})", skill.name, description)
        }
        _ => format!("- {}: (file: {path})", skill.name),
    }
}

fn lines_cost(lines: &[String], budget: Budget) -> usize {
    lines
        .iter()
        .map(|line| budget.cost(line).saturating_add(1))
        .fold(0usize, usize::saturating_add)
}

fn prompt_scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::System => 0,
        SkillScope::Admin => 1,
        SkillScope::Repo => 2,
        SkillScope::User => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::wayland::skills::model::SkillPolicy;

    fn skill(name: &str, description: &str, implicit: bool) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: description.to_string(),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            scope: SkillScope::Repo,
            policy: SkillPolicy {
                allow_implicit_invocation: implicit,
                products: Vec::new(),
            },
            short_description: None,
            interface: None,
            dependencies: None,
        }
    }

    #[test]
    fn renders_codex_style_progressive_disclosure_context() {
        let outcome = SkillLoadOutcome {
            skills: vec![skill("review", "Review a patch.", true)],
            errors: vec![],
        };

        let rendered = render(&outcome, Some(128_000));
        let body = rendered.instructions.unwrap();

        assert!(body.starts_with("<skills_instructions>\n## Skills"));
        assert!(body.contains("- review: Review a patch. (file: /skills/review/SKILL.md)"));
        assert!(body.contains("with `$SkillName`"));
        assert!(body.ends_with("</skills_instructions>"));
        assert_eq!(rendered.warning, None);
    }

    #[test]
    fn omits_explicit_only_skills_from_model_visible_catalog() {
        let outcome = SkillLoadOutcome {
            skills: vec![skill("manual", "Only by name.", false)],
            errors: vec![],
        };

        assert_eq!(render(&outcome, Some(128_000)).instructions, None);
    }

    #[test]
    fn budget_removes_descriptions_before_skills() {
        let outcome = SkillLoadOutcome {
            skills: vec![
                skill("one", &"a".repeat(500), true),
                skill("two", &"b".repeat(500), true),
            ],
            errors: vec![],
        };

        let rendered = render(&outcome, Some(2_000));
        let body = rendered.instructions.unwrap();

        assert!(body.contains("- one:"));
        assert!(body.contains("- two:"));
        assert!(rendered.warning.is_some());
    }
}
