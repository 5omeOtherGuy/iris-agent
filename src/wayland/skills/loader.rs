// SPDX-License-Identifier: Apache-2.0
// Derived from OpenAI Codex core-skills/src/loader.rs; adapted to Iris's config,
// filesystem, scope, and synchronous turn-boundary loading model.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[cfg(not(test))]
use std::env;

use serde::Deserialize;

use super::model::{SkillError, SkillLoadOutcome, SkillMetadata, SkillPolicy, SkillScope};

const SKILL_FILENAME: &str = "SKILL.md";
const MAX_NAME_LEN: usize = 64;
const MAX_METADATA_FIELD_LEN: usize = 1024;
const MAX_SCAN_DEPTH: usize = 6;
const MAX_SKILL_DIRS_PER_ROOT: usize = 2_000;

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: SkillFrontmatterMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatterMetadata {
    #[serde(default, rename = "short-description")]
    short_description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiMetadataFile {
    #[serde(default)]
    interface: Option<OpenAiInterface>,
    #[serde(default)]
    dependencies: Option<OpenAiDependencies>,
    #[serde(default)]
    policy: Option<OpenAiPolicy>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiInterface {
    display_name: Option<String>,
    short_description: Option<String>,
    icon_small: Option<PathBuf>,
    icon_large: Option<PathBuf>,
    brand_color: Option<String>,
    default_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiDependencies {
    #[serde(default)]
    tools: Vec<OpenAiToolDependency>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiToolDependency {
    #[serde(rename = "type")]
    kind: Option<String>,
    value: Option<String>,
    description: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiPolicy {
    #[serde(default)]
    allow_implicit_invocation: Option<bool>,
    #[serde(default)]
    products: Vec<String>,
}

#[derive(Debug, Default)]
struct LoadedMetadata {
    interface: Option<super::model::SkillInterface>,
    dependencies: Option<super::model::SkillDependencies>,
    policy: SkillPolicy,
}

#[derive(Debug, Clone)]
struct SkillRoot {
    path: PathBuf,
    scope: SkillScope,
}

#[derive(Debug, Default, Deserialize)]
struct CodexConfigFile {
    #[serde(default)]
    skills: CodexSkillsConfig,
}

#[derive(Debug, Deserialize)]
struct CodexSkillsConfig {
    #[serde(default = "default_true")]
    include_instructions: bool,
    #[serde(default)]
    config: Vec<CodexSkillRule>,
}

impl Default for CodexSkillsConfig {
    fn default() -> Self {
        Self {
            include_instructions: true,
            config: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CodexSkillRule {
    path: Option<PathBuf>,
    name: Option<String>,
    enabled: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug)]
pub(super) struct LoadedSkills {
    pub(super) outcome: SkillLoadOutcome,
    pub(super) include_instructions: bool,
}

pub(super) fn load(workspace: &Path) -> LoadedSkills {
    let (roots, config) = roots_and_config(workspace);
    let mut outcome = load_from_roots(roots);
    apply_config_rules(&mut outcome.skills, &config.config);
    LoadedSkills {
        outcome,
        include_instructions: config.include_instructions,
    }
}

fn load_from_roots(roots: Vec<SkillRoot>) -> SkillLoadOutcome {
    let mut outcome = SkillLoadOutcome::default();
    let mut seen_skills = HashSet::new();

    for root in roots {
        discover_root(&root, &mut outcome, &mut seen_skills);
    }

    outcome.skills.sort_by(|a, b| {
        loader_scope_rank(a.scope)
            .cmp(&loader_scope_rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    outcome
}

fn roots_and_config(workspace: &Path) -> (Vec<SkillRoot>, CodexSkillsConfig) {
    let roots = repo_skill_roots(workspace);
    #[cfg(test)]
    let config = CodexSkillsConfig::default();
    #[cfg(not(test))]
    let (roots, config) = {
        let mut roots = roots;
        let mut config = CodexSkillsConfig::default();
        if let Some(home) = home_dir() {
            roots.push(SkillRoot {
                path: home.join(".agents/skills"),
                scope: SkillScope::User,
            });
            // Codex keeps this deprecated location for compatibility. Reading
            // it lets an existing Codex skill install work without copying.
            let codex_home = env::var_os("CODEX_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            config = load_codex_config(&codex_home);
            roots.push(SkillRoot {
                path: codex_home.join("skills"),
                scope: SkillScope::User,
            });
            roots.push(SkillRoot {
                path: codex_home.join("skills/.system"),
                scope: SkillScope::System,
            });
            roots.push(SkillRoot {
                path: home.join(".iris/skills"),
                scope: SkillScope::User,
            });
        }
        roots.push(SkillRoot {
            path: PathBuf::from("/etc/codex/skills"),
            scope: SkillScope::Admin,
        });
        roots.push(SkillRoot {
            path: PathBuf::from("/etc/iris/skills"),
            scope: SkillScope::Admin,
        });
        (roots, config)
    };
    (dedupe_roots(roots), config)
}

#[cfg(not(test))]
fn load_codex_config(codex_home: &Path) -> CodexSkillsConfig {
    let path = codex_home.join("config.toml");
    let Ok(contents) = fs::read_to_string(&path) else {
        return CodexSkillsConfig::default();
    };
    match toml::from_str::<CodexConfigFile>(&contents) {
        Ok(config) => config.skills,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring invalid Codex skills config");
            CodexSkillsConfig::default()
        }
    }
}

fn apply_config_rules(skills: &mut Vec<SkillMetadata>, rules: &[CodexSkillRule]) {
    let mut disabled = HashSet::new();
    for rule in rules {
        match (rule.path.as_ref(), rule.name.as_deref()) {
            (Some(path), None) => {
                let path = path.canonicalize().unwrap_or_else(|_| path.clone());
                if rule.enabled {
                    disabled.remove(&path);
                } else {
                    disabled.insert(path);
                }
            }
            (None, Some(name)) if !name.trim().is_empty() => {
                for path in skills
                    .iter()
                    .filter(|skill| skill.name == name.trim())
                    .map(|skill| skill.path.clone())
                {
                    if rule.enabled {
                        disabled.remove(&path);
                    } else {
                        disabled.insert(path);
                    }
                }
            }
            _ => tracing::warn!("ignoring Codex skill config rule without exactly one selector"),
        }
    }
    skills.retain(|skill| !disabled.contains(&skill.path));
}

fn repo_skill_roots(workspace: &Path) -> Vec<SkillRoot> {
    let cwd = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let project_root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.clone());
    let mut dirs = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(&project_root))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    dirs.reverse();
    let mut roots = dirs
        .into_iter()
        .map(|dir| SkillRoot {
            path: dir.join(".agents/skills"),
            scope: SkillScope::Repo,
        })
        .collect::<Vec<_>>();
    // Codex retains project `.codex/skills` as its older project-layer
    // location. Load it after `.agents` so existing installations carry over.
    roots.push(SkillRoot {
        path: project_root.join(".codex/skills"),
        scope: SkillScope::Repo,
    });
    roots
}

#[cfg(not(test))]
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn dedupe_roots(roots: Vec<SkillRoot>) -> Vec<SkillRoot> {
    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|root| {
            let identity = root
                .path
                .canonicalize()
                .unwrap_or_else(|_| root.path.clone());
            seen.insert(identity)
        })
        .collect()
}

fn discover_root(
    root: &SkillRoot,
    outcome: &mut SkillLoadOutcome,
    seen_skills: &mut HashSet<PathBuf>,
) {
    if !root.path.is_dir() {
        return;
    }

    let identity = root
        .path
        .canonicalize()
        .unwrap_or_else(|_| root.path.clone());
    let mut visited = HashSet::from([identity.clone()]);
    let mut queue = VecDeque::from([(identity, 0usize)]);
    let mut truncated = false;

    while let Some((dir, depth)) = queue.pop_front() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "failed to read skills directory");
                continue;
            }
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            if file_name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "failed to inspect skills path");
                    continue;
                }
            };

            if file_type.is_dir() || (file_type.is_symlink() && path.is_dir()) {
                // Codex does not follow aliases in its embedded system cache;
                // user/repo/admin roots do follow directory aliases.
                if file_type.is_symlink() && root.scope == SkillScope::System {
                    continue;
                }
                if depth >= MAX_SCAN_DEPTH || visited.len() >= MAX_SKILL_DIRS_PER_ROOT {
                    truncated = visited.len() >= MAX_SKILL_DIRS_PER_ROOT;
                    continue;
                }
                let resolved = path.canonicalize().unwrap_or(path);
                if visited.insert(resolved.clone()) {
                    queue.push_back((resolved, depth + 1));
                }
                continue;
            }

            if file_type.is_file() && file_name == SKILL_FILENAME {
                let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
                if !seen_skills.insert(identity.clone()) {
                    continue;
                }
                match parse_skill(&identity, root.scope) {
                    Ok(skill) => outcome.skills.push(skill),
                    Err(message) if root.scope != SkillScope::System => {
                        outcome.errors.push(SkillError {
                            path: identity,
                            message,
                        })
                    }
                    Err(_) => {}
                }
            }
        }
    }

    if truncated {
        tracing::warn!(
            path = %root.path.display(),
            limit = MAX_SKILL_DIRS_PER_ROOT,
            "skills scan reached its directory limit"
        );
    }
}

fn parse_skill(path: &Path, scope: SkillScope) -> Result<SkillMetadata, String> {
    let contents =
        fs::read_to_string(path).map_err(|error| format!("failed to read file: {error}"))?;
    let frontmatter = extract_frontmatter(&contents)
        .ok_or_else(|| "missing YAML frontmatter delimited by ---".to_string())?;
    let parsed: SkillFrontmatter = match serde_yaml_ng::from_str(frontmatter) {
        Ok(parsed) => parsed,
        Err(original_error) => match repair_frontmatter_scalar_fields(frontmatter) {
            Some(repaired) => serde_yaml_ng::from_str(&repaired)
                .map_err(|_| format!("invalid YAML: {original_error}"))?,
            None => return Err(format!("invalid YAML: {original_error}")),
        },
    };

    let name = parsed
        .name
        .as_deref()
        .map(sanitize_single_line)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .map(sanitize_single_line)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "skill".to_string());
    let description = parsed
        .description
        .as_deref()
        .map(sanitize_single_line)
        .unwrap_or_default();
    validate_field(&name, MAX_NAME_LEN, "name")?;
    if description.is_empty() {
        return Err("missing field `description`".to_string());
    }
    let short_description = parsed
        .metadata
        .short_description
        .as_deref()
        .map(sanitize_single_line)
        .filter(|value| !value.is_empty());
    let metadata = load_metadata(path);

    Ok(SkillMetadata {
        name,
        description,
        short_description,
        interface: metadata.interface,
        dependencies: metadata.dependencies,
        path: path.to_path_buf(),
        scope,
        policy: metadata.policy,
    })
}

fn load_metadata(skill_path: &Path) -> LoadedMetadata {
    let Some(skill_dir) = skill_path.parent() else {
        return LoadedMetadata::default();
    };
    let path = skill_dir.join("agents/openai.yaml");
    let Ok(contents) = fs::read_to_string(&path) else {
        return LoadedMetadata::default();
    };
    match serde_yaml_ng::from_str::<OpenAiMetadataFile>(&contents) {
        Ok(metadata) => resolve_metadata(skill_dir, metadata),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring invalid optional skill metadata");
            LoadedMetadata::default()
        }
    }
}

fn resolve_metadata(skill_dir: &Path, metadata: OpenAiMetadataFile) -> LoadedMetadata {
    let interface = metadata.interface.and_then(|interface| {
        let resolved = super::model::SkillInterface {
            display_name: optional_field(interface.display_name, MAX_NAME_LEN),
            short_description: optional_field(interface.short_description, MAX_METADATA_FIELD_LEN),
            icon_small: resolve_asset_path(skill_dir, interface.icon_small),
            icon_large: resolve_asset_path(skill_dir, interface.icon_large),
            brand_color: optional_field(interface.brand_color, MAX_NAME_LEN),
            default_prompt: optional_field(interface.default_prompt, MAX_METADATA_FIELD_LEN),
        };
        (resolved != super::model::SkillInterface::default()).then_some(resolved)
    });
    let dependencies = metadata.dependencies.and_then(|dependencies| {
        let tools = dependencies
            .tools
            .into_iter()
            .filter_map(|tool| {
                Some(super::model::SkillToolDependency {
                    kind: required_field(tool.kind, MAX_NAME_LEN)?,
                    value: required_field(tool.value, MAX_METADATA_FIELD_LEN)?,
                    description: optional_field(tool.description, MAX_METADATA_FIELD_LEN),
                    transport: optional_field(tool.transport, MAX_NAME_LEN),
                    command: optional_field(tool.command, MAX_METADATA_FIELD_LEN),
                    url: optional_field(tool.url, MAX_METADATA_FIELD_LEN),
                })
            })
            .collect::<Vec<_>>();
        (!tools.is_empty()).then_some(super::model::SkillDependencies { tools })
    });
    let policy = metadata
        .policy
        .map(|policy| SkillPolicy {
            allow_implicit_invocation: policy.allow_implicit_invocation.unwrap_or(true),
            products: policy.products,
        })
        .unwrap_or_default();
    LoadedMetadata {
        interface,
        dependencies,
        policy,
    }
}

fn optional_field(value: Option<String>, max_len: usize) -> Option<String> {
    value
        .as_deref()
        .map(sanitize_single_line)
        .filter(|value| !value.is_empty() && value.chars().count() <= max_len)
}

fn required_field(value: Option<String>, max_len: usize) -> Option<String> {
    optional_field(value, max_len)
}

fn resolve_asset_path(skill_dir: &Path, path: Option<PathBuf>) -> Option<PathBuf> {
    let path = path?;
    if path.is_absolute() || path.as_os_str().is_empty() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            _ => return None,
        }
    }
    if !normalized.starts_with("assets") {
        return None;
    }
    Some(skill_dir.join(normalized))
}

fn extract_frontmatter(contents: &str) -> Option<&str> {
    let rest = contents.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed == "---" {
            return (offset > 0).then(|| &rest[..offset]);
        }
        offset += line.len();
    }
    None
}

/// Repair the common third-party form `description: Build for AWS: ECS`
/// without accepting unrelated malformed YAML. This is intentionally the same
/// line-oriented compatibility repair used by Codex.
fn repair_frontmatter_scalar_fields(frontmatter: &str) -> Option<String> {
    let mut changed = false;
    let mut block_scalar_indent: Option<usize> = None;
    let mut repaired_lines = Vec::new();
    for line in frontmatter.lines() {
        let indent = line
            .chars()
            .take_while(|character| *character == ' ')
            .count();
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent > block_indent {
                repaired_lines.push(line.to_string());
                continue;
            }
            block_scalar_indent = None;
        }

        let Some((key, value)) = line.split_once(':') else {
            repaired_lines.push(line.to_string());
            continue;
        };
        if key.trim().is_empty() || !value.chars().next().is_none_or(char::is_whitespace) {
            repaired_lines.push(line.to_string());
            continue;
        }

        let trimmed_start = value.trim_start();
        let leading_whitespace = &value[..value.len() - trimmed_start.len()];
        let mut scalar = trimmed_start;
        let mut comment = "";
        for (index, character) in trimmed_start.char_indices() {
            if character == '#'
                && (index == 0
                    || trimmed_start[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
            {
                let comment_start = trimmed_start[..index].trim_end().len();
                scalar = &trimmed_start[..comment_start];
                comment = &trimmed_start[comment_start..];
                break;
            }
        }

        let scalar = scalar.trim_end();
        let Some(first_char) = scalar.chars().next() else {
            repaired_lines.push(line.to_string());
            continue;
        };
        if matches!(first_char, '|' | '>') {
            block_scalar_indent = Some(indent);
            repaired_lines.push(line.to_string());
            continue;
        }
        if matches!(first_char, '\'' | '"') {
            repaired_lines.push(line.to_string());
            continue;
        }
        let mut chars = scalar.chars().peekable();
        let mut has_colon_separator = false;
        while let Some(character) = chars.next() {
            if character == ':'
                && matches!(chars.peek(), Some(next_character) if next_character.is_whitespace())
            {
                has_colon_separator = true;
                break;
            }
        }
        let invalid_flow_like_scalar = matches!(first_char, '[' | '{' | '@' | '`')
            && serde_yaml_ng::from_str::<serde_yaml_ng::Value>(scalar).is_err();
        if !has_colon_separator && !invalid_flow_like_scalar {
            repaired_lines.push(line.to_string());
            continue;
        }

        let quoted_scalar = format!("'{}'", scalar.replace('\'', "''"));
        repaired_lines.push(format!(
            "{key}:{leading_whitespace}{quoted_scalar}{comment}"
        ));
        changed = true;
    }
    changed.then(|| repaired_lines.join("\n"))
}

fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_field(value: &str, max_len: usize, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("missing field `{field}`"));
    }
    if value.chars().count() > max_len {
        return Err(format!(
            "invalid {field}: exceeds maximum length of {max_len} characters"
        ));
    }
    Ok(())
}

fn loader_scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Repo => 0,
        SkillScope::User => 1,
        SkillScope::System => 2,
        SkillScope::Admin => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::temp_dir;

    fn write_skill(root: &Path, dir: &str, body: &str) -> PathBuf {
        let dir = root.join(dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SKILL_FILENAME);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parses_and_sorts_valid_skills_and_reports_invalid_ones() {
        let dir = temp_dir();
        write_skill(
            &dir.path,
            "zeta",
            "---\nname: zeta\ndescription: Zeta workflow.\n---\nDo it.\n",
        );
        write_skill(
            &dir.path,
            "alpha",
            "---\nname: alpha\ndescription: Alpha workflow.\n---\nDo it.\n",
        );
        write_skill(&dir.path, "broken", "no frontmatter");

        let outcome = load_from_roots(vec![SkillRoot {
            path: dir.path.clone(),
            scope: SkillScope::Admin,
        }]);

        assert_eq!(
            outcome
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0]
                .message
                .contains("missing YAML frontmatter")
        );
    }

    #[test]
    fn falls_back_to_directory_name_but_requires_description() {
        let dir = temp_dir();
        write_skill(
            &dir.path,
            "folder-name",
            "---\ndescription: Folder-named skill.\n---\nBody.\n",
        );
        write_skill(&dir.path, "missing-description", "---\nname: nope\n---\n");

        let outcome = load_from_roots(vec![SkillRoot {
            path: dir.path.clone(),
            scope: SkillScope::User,
        }]);

        assert_eq!(outcome.skills[0].name, "folder-name");
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0]
                .message
                .contains("missing field `description`")
        );
    }

    #[test]
    fn optional_policy_can_disable_implicit_invocation() {
        let dir = temp_dir();
        let path = write_skill(
            &dir.path,
            "explicit-only",
            "---\nname: explicit-only\ndescription: Explicit workflow.\n---\nBody.\n",
        );
        let metadata_dir = path.parent().unwrap().join("agents");
        fs::create_dir_all(&metadata_dir).unwrap();
        fs::write(
            metadata_dir.join("openai.yaml"),
            "policy:\n  allow_implicit_invocation: false\n",
        )
        .unwrap();

        let outcome = load_from_roots(vec![SkillRoot {
            path: dir.path.clone(),
            scope: SkillScope::User,
        }]);

        assert!(!outcome.skills[0].policy.allow_implicit_invocation);
    }

    #[test]
    fn codex_config_rules_disable_by_name_and_allow_later_override() {
        let dir = temp_dir();
        let path = write_skill(
            &dir.path,
            "review",
            "---\nname: review\ndescription: Review changes.\n---\nBody.\n",
        );
        let mut outcome = load_from_roots(vec![SkillRoot {
            path: dir.path.clone(),
            scope: SkillScope::User,
        }]);

        apply_config_rules(
            &mut outcome.skills,
            &[
                CodexSkillRule {
                    path: Some(path.clone()),
                    name: None,
                    enabled: false,
                },
                CodexSkillRule {
                    path: None,
                    name: Some("review".to_string()),
                    enabled: true,
                },
            ],
        );

        assert_eq!(outcome.skills.len(), 1);
    }

    #[test]
    fn parses_codex_skill_config_without_requiring_other_config_fields() {
        let config: CodexConfigFile = toml::from_str(
            "model = 'gpt-5'\n[skills]\ninclude_instructions = false\n[[skills.config]]\nname = 'review'\nenabled = false\n",
        )
        .unwrap();

        assert!(!config.skills.include_instructions);
        assert_eq!(config.skills.config[0].name.as_deref(), Some("review"));
        assert!(!config.skills.config[0].enabled);
    }

    #[test]
    fn parses_codex_metadata_and_repairs_unquoted_colon_prose() {
        let dir = temp_dir();
        let path = write_skill(
            &dir.path,
            "deploy",
            "---\nname: deploy\ndescription: Deploy to AWS: ECS\nmetadata:\n  short-description: Ship it\n---\nBody.\n",
        );
        let metadata_dir = path.parent().unwrap().join("agents");
        fs::create_dir_all(&metadata_dir).unwrap();
        fs::write(
            metadata_dir.join("openai.yaml"),
            "interface:\n  display_name: Deploy\n  short_description: Deploy services\n  default_prompt: Deploy this service\ndependencies:\n  tools:\n    - type: mcp\n      value: aws\npolicy:\n  products: [codex]\n",
        )
        .unwrap();

        let outcome = load_from_roots(vec![SkillRoot {
            path: dir.path.clone(),
            scope: SkillScope::System,
        }]);
        let skill = &outcome.skills[0];
        assert_eq!(skill.description, "Deploy to AWS: ECS");
        assert_eq!(skill.short_description.as_deref(), Some("Ship it"));
        assert_eq!(skill.display_name(), "Deploy");
        assert_eq!(skill.display_description(), "Deploy services");
        assert_eq!(skill.dependencies.as_ref().unwrap().tools[0].kind, "mcp");
        assert_eq!(skill.policy.products, ["codex"]);
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinked_skill_directories_without_looping() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir();
        let target = temp_dir();
        write_skill(
            &target.path,
            "linked",
            "---\nname: linked\ndescription: Linked workflow.\n---\nBody.\n",
        );
        symlink(target.path.join("linked"), dir.path.join("linked")).unwrap();
        symlink(&dir.path, dir.path.join("loop")).unwrap();

        let outcome = load_from_roots(vec![SkillRoot {
            path: dir.path.clone(),
            scope: SkillScope::Repo,
        }]);

        assert_eq!(outcome.skills.len(), 1);
        assert_eq!(outcome.skills[0].name, "linked");
    }

    #[cfg(unix)]
    #[test]
    fn system_root_ignores_symlinks_and_invalid_bundled_metadata() {
        use std::os::unix::fs::symlink;

        let root = temp_dir();
        let target = temp_dir();
        write_skill(
            &target.path,
            "linked",
            "---\nname: linked\ndescription: Linked workflow.\n---\nBody.\n",
        );
        symlink(target.path.join("linked"), root.path.join("linked")).unwrap();
        write_skill(&root.path, "broken", "not frontmatter");

        let outcome = load_from_roots(vec![SkillRoot {
            path: root.path.clone(),
            scope: SkillScope::System,
        }]);

        assert!(outcome.skills.is_empty());
        assert!(outcome.errors.is_empty());
    }
}
