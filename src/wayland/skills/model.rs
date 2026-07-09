// SPDX-License-Identifier: Apache-2.0
// Derived from OpenAI Codex core-skills/src/model.rs; reduced to Iris's native
// filesystem skill surface and in-crate Wayland ownership.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SkillScope {
    Repo,
    User,
    System,
    Admin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillPolicy {
    pub(crate) allow_implicit_invocation: bool,
    pub(crate) products: Vec<String>,
}

impl Default for SkillPolicy {
    fn default() -> Self {
        Self {
            allow_implicit_invocation: true,
            products: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SkillInterface {
    pub(crate) display_name: Option<String>,
    pub(crate) short_description: Option<String>,
    pub(crate) icon_small: Option<PathBuf>,
    pub(crate) icon_large: Option<PathBuf>,
    pub(crate) brand_color: Option<String>,
    pub(crate) default_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDependencies {
    pub(crate) tools: Vec<SkillToolDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillToolDependency {
    pub(crate) kind: String,
    pub(crate) value: String,
    pub(crate) description: Option<String>,
    pub(crate) transport: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) short_description: Option<String>,
    pub(crate) interface: Option<SkillInterface>,
    pub(crate) dependencies: Option<SkillDependencies>,
    pub(crate) path: PathBuf,
    pub(crate) scope: SkillScope,
    pub(crate) policy: SkillPolicy,
}

impl SkillMetadata {
    pub(crate) fn display_name(&self) -> &str {
        self.interface
            .as_ref()
            .and_then(|interface| interface.display_name.as_deref())
            .unwrap_or(&self.name)
    }

    pub(crate) fn display_description(&self) -> &str {
        self.interface
            .as_ref()
            .and_then(|interface| interface.short_description.as_deref())
            .or(self.short_description.as_deref())
            .unwrap_or(&self.description)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillError {
    pub(crate) path: PathBuf,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SkillLoadOutcome {
    pub(crate) skills: Vec<SkillMetadata>,
    pub(crate) errors: Vec<SkillError>,
}
