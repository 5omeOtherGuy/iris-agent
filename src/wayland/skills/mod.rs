// SPDX-License-Identifier: Apache-2.0
// Derived from OpenAI Codex core-skills manager/injection composition; adapted
// to Iris's in-crate Wayland harness and refresh-at-turn-boundary lifecycle.

//! Codex-compatible native skills for the Wayland harness.
//!
//! Discovery, validation, metadata budgeting, and selected-body loading live in
//! Tier 2. Nexus sees only provider-neutral contextual messages; the terminal
//! layer sees only catalog metadata for its picker.

mod injection;
mod loader;
mod model;
mod render;

use std::path::Path;

pub(crate) use model::{SkillMetadata, SkillScope};

use injection::SkillInjections;
use model::SkillLoadOutcome;

#[derive(Debug, Clone)]
pub(crate) struct SkillCatalog {
    outcome: SkillLoadOutcome,
    available_instructions: Option<String>,
    warnings: Vec<String>,
}

impl SkillCatalog {
    pub(crate) fn load(workspace: &Path, context_budget: Option<u64>) -> Self {
        let loaded = loader::load(workspace);
        let outcome = loaded.outcome;
        let rendered = if loaded.include_instructions {
            render::render(&outcome, context_budget)
        } else {
            render::RenderedSkills {
                instructions: None,
                warning: None,
            }
        };
        let mut warnings = outcome
            .errors
            .iter()
            .map(|error| {
                format!(
                    "Failed to load skill at {}: {}",
                    error.path.display(),
                    error.message
                )
            })
            .collect::<Vec<_>>();
        if let Some(warning) = rendered.warning {
            warnings.push(warning);
        }
        Self {
            outcome,
            available_instructions: rendered.instructions,
            warnings,
        }
    }

    pub(crate) fn skills(&self) -> &[SkillMetadata] {
        &self.outcome.skills
    }

    pub(crate) fn available_instructions(&self) -> Option<&str> {
        self.available_instructions.as_deref()
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn resource_roots(&self) -> Vec<std::path::PathBuf> {
        let mut roots = self
            .outcome
            .skills
            .iter()
            .filter_map(|skill| skill.path.parent())
            .filter_map(|path| path.canonicalize().ok())
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();
        roots
    }

    pub(crate) fn injections(&self, prompt: &str) -> SkillInjections {
        injection::build(prompt, &self.outcome.skills)
    }
}
