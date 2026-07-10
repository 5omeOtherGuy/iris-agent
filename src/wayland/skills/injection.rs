// SPDX-License-Identifier: Apache-2.0
// Derived from OpenAI Codex core-skills/src/injection.rs; adapted to Iris's
// plain-text prompt input and Wayland contextual-message seam.

use std::collections::{HashMap, HashSet};
use std::fs;

use super::model::SkillMetadata;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SkillInjections {
    pub(crate) messages: Vec<String>,
    pub(crate) warnings: Vec<String>,
}

pub(super) fn build(prompt: &str, skills: &[SkillMetadata]) -> SkillInjections {
    let mentions = extract_mentions(prompt);
    if mentions.names.is_empty() && mentions.paths.is_empty() {
        return SkillInjections::default();
    }

    let name_counts = skills.iter().fold(HashMap::new(), |mut counts, skill| {
        *counts.entry(skill.name.as_str()).or_insert(0usize) += 1;
        counts
    });
    let mut selected = Vec::new();
    let mut seen_paths = HashSet::new();

    for skill in skills {
        let path = skill.path.to_string_lossy();
        if mentions.paths.contains(path.as_ref()) && seen_paths.insert(skill.path.clone()) {
            selected.push(skill);
        }
    }
    for skill in skills {
        if seen_paths.contains(&skill.path)
            || !mentions.plain_names.contains(skill.name.as_str())
            || name_counts.get(skill.name.as_str()) != Some(&1)
        {
            continue;
        }
        seen_paths.insert(skill.path.clone());
        selected.push(skill);
    }

    let mut result = SkillInjections::default();
    for skill in selected {
        match fs::read_to_string(&skill.path) {
            Ok(contents) => result.messages.push(format!(
                "<skill>\n<name>{}</name>\n<path>{}</path>\n{}\n</skill>",
                skill.name,
                skill.path.display(),
                contents
            )),
            Err(error) => result.warnings.push(format!(
                "Failed to load skill {} at {}: {error}",
                skill.name,
                skill.path.display()
            )),
        }
    }
    result
}

struct Mentions<'a> {
    names: HashSet<&'a str>,
    paths: HashSet<&'a str>,
    plain_names: HashSet<&'a str>,
}

fn extract_mentions(text: &str) -> Mentions<'_> {
    let bytes = text.as_bytes();
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    let mut plain_names = HashSet::new();
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some((name, path, end)) = parse_linked_mention(text, bytes, index)
        {
            if !is_common_env_var(name) {
                names.insert(name);
                paths.insert(path.strip_prefix("skill://").unwrap_or(path));
            }
            index = end;
            continue;
        }
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let Some(first) = bytes.get(start) else {
            break;
        };
        if !is_mention_char(*first) {
            index += 1;
            continue;
        }
        let mut end = start + 1;
        while bytes.get(end).is_some_and(|byte| is_mention_char(*byte)) {
            end += 1;
        }
        let name = &text[start..end];
        if !is_common_env_var(name) {
            names.insert(name);
            plain_names.insert(name);
        }
        index = end;
    }

    Mentions {
        names,
        paths,
        plain_names,
    }
}

fn parse_linked_mention<'a>(
    text: &'a str,
    bytes: &[u8],
    start: usize,
) -> Option<(&'a str, &'a str, usize)> {
    if bytes.get(start + 1) != Some(&b'$') {
        return None;
    }
    let name_start = start + 2;
    if !bytes
        .get(name_start)
        .is_some_and(|byte| is_mention_char(*byte))
    {
        return None;
    }
    let mut name_end = name_start + 1;
    while bytes
        .get(name_end)
        .is_some_and(|byte| is_mention_char(*byte))
    {
        name_end += 1;
    }
    if bytes.get(name_end) != Some(&b']') {
        return None;
    }
    let mut open = name_end + 1;
    while bytes.get(open).is_some_and(u8::is_ascii_whitespace) {
        open += 1;
    }
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut close = open + 1;
    while bytes.get(close).is_some_and(|byte| *byte != b')') {
        close += 1;
    }
    if bytes.get(close) != Some(&b')') {
        return None;
    }
    let path = text[open + 1..close].trim();
    if path.is_empty() {
        return None;
    }
    Some((&text[name_start..name_end], path, close + 1))
}

fn is_mention_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':')
}

fn is_common_env_var(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "PWD"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "LANG"
            | "TERM"
            | "XDG_CONFIG_HOME"
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::wayland::skills::model::{SkillPolicy, SkillScope};

    fn skill(name: &str, path: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: "description".to_string(),
            short_description: None,
            interface: None,
            dependencies: None,
            path: PathBuf::from(path),
            scope: SkillScope::Repo,
            policy: SkillPolicy::default(),
        }
    }

    #[test]
    fn extracts_plain_and_linked_mentions_without_treating_env_vars_as_skills() {
        let mentions =
            extract_mentions("Use $review and [$duplicate](skill:///tmp/two/SKILL.md), not $HOME.");

        assert!(mentions.plain_names.contains("review"));
        assert!(!mentions.plain_names.contains("HOME"));
        assert!(mentions.paths.contains("/tmp/two/SKILL.md"));
    }

    #[test]
    fn plain_duplicate_is_ambiguous_but_linked_path_is_exact() {
        let skills = vec![
            skill("duplicate", "/tmp/one/SKILL.md"),
            skill("duplicate", "/tmp/two/SKILL.md"),
        ];

        assert!(build("$duplicate", &skills).messages.is_empty());
        let selected = build("[$duplicate](skill:///tmp/two/SKILL.md)", &skills);
        // The path does not need to exist to prove selection; the read warning
        // names only the exact linked skill.
        assert_eq!(selected.warnings.len(), 1);
        assert!(selected.warnings[0].contains("/tmp/two/SKILL.md"));
    }
}
