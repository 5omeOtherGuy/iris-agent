//! Display-only task projection (Tier 3, ADR-0031).
//!
//! One read-only value that represents either the ACTIVE (live, unsettled) task
//! or a RECOVERABLE (crashed-orphan / legacy) task uniformly, so the unified
//! `/tasks` surface renders both from a single shape instead of scattering task
//! state across pickers and slash-command text. Built at the Wayland/UI boundary
//! from the git-safety display DTOs ([`ActiveTaskDisplay`], [`RecoverableTask`])
//! plus the git-status snapshot the UI already holds.
//!
//! It carries only opaque display payload (`body`/`sessions`, ADR-0031) and the
//! attribution counts already computed for the session bar; it never affects
//! enforcement or recovery (those consult the task record + lease only). Pure
//! and disk-free, so the projection is unit-tested without a repo.

use std::time::Duration;

use crate::session;
use crate::wayland::git_safety::{ActiveTaskDisplay, RecoverableTask};

/// Which kind of task a [`TaskCard`] projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskKind {
    /// The live, unsettled task owned by this process (shown, never adoptable).
    Active,
    /// A lease-free crashed orphan safe to adopt.
    Recoverable,
    /// A record predating the lease protocol (ADR-0030): adoptable only by
    /// explicit selection, shown with a legacy marker.
    Legacy,
}

/// The display body for a task's opaque `body` payload: the trimmed body, or
/// the placeholder when none was recorded (or it is blank). Shared by the task
/// cards and the adoption notice so both render an unrecorded body identically.
pub(crate) fn body_preview(body: Option<&str>) -> String {
    match body.map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => "(no description recorded)".to_string(),
    }
}

/// A read-only projection of one task for the unified task UI. `body`/`sessions`
/// are opaque display copy (ADR-0031). `iris_files`/`user_files`/`age` are
/// populated "where available" -- from the git-status snapshot for the active
/// task, `None` on recovery rows whose counts are not surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskCard {
    pub(crate) task_id: String,
    pub(crate) kind: TaskKind,
    pub(crate) body: Option<String>,
    pub(crate) sessions: Vec<String>,
    pub(crate) age: Option<Duration>,
    pub(crate) iris_files: Option<u32>,
    pub(crate) user_files: Option<u32>,
}

impl TaskCard {
    /// Project a recoverable/legacy record. Age comes from the record; per-file
    /// attribution counts are not surfaced on recovery rows (`None`).
    pub(crate) fn from_recoverable(task: &RecoverableTask) -> Self {
        TaskCard {
            task_id: task.task_id.clone(),
            kind: if task.is_legacy() {
                TaskKind::Legacy
            } else {
                TaskKind::Recoverable
            },
            body: task.body.clone(),
            sessions: task.sessions.clone(),
            age: Some(task.age),
            iris_files: None,
            user_files: None,
        }
    }

    /// Project the active task, enriched with the git-status file counts + age
    /// when the caller matched the snapshot's task id (else `None`).
    pub(crate) fn active(
        display: &ActiveTaskDisplay,
        age: Option<Duration>,
        iris_files: Option<u32>,
        user_files: Option<u32>,
    ) -> Self {
        TaskCard {
            task_id: display.task_id.clone(),
            kind: TaskKind::Active,
            body: display.body.clone(),
            sessions: display.sessions.clone(),
            age,
            iris_files,
            user_files,
        }
    }

    /// The body cell: the opaque body, or the legacy placeholder when none was
    /// recorded (or it is blank). Same wording as the adoption notice.
    pub(crate) fn body_preview(&self) -> String {
        body_preview(self.body.as_deref())
    }

    /// A short, display-friendly prefix of the opaque task id (first 8 chars),
    /// so headers/notices stay readable without losing uniqueness for the user.
    pub(crate) fn short_id(&self) -> String {
        self.task_id.chars().take(8).collect()
    }

    /// The linked-session count summary (`1 session` / `N sessions`).
    pub(crate) fn session_summary(&self) -> String {
        match self.sessions.len() {
            1 => "1 session".to_string(),
            n => format!("{n} sessions"),
        }
    }

    /// A human-relative age string (`5m ago`), or empty when age is unknown.
    /// Reuses the resume picker's formatter, measuring the age as a delta from
    /// an epoch of zero.
    pub(crate) fn age_label(&self) -> String {
        self.age
            .map(|age| session::relative_age(age.as_millis(), 0))
            .unwrap_or_default()
    }

    /// Whether this card can be adopted: only recoverable/legacy rows. The
    /// active task is already owned by this process, so it is shown but never
    /// adoptable (ADR-0031: adoption is for crashed orphans).
    pub(crate) fn is_adoptable(&self) -> bool {
        !matches!(self.kind, TaskKind::Active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recoverable(
        id: &str,
        body: Option<&str>,
        sessions: &[&str],
        age: Duration,
    ) -> RecoverableTask {
        RecoverableTask::for_test(id, age, body, sessions)
    }

    #[test]
    fn from_recoverable_projects_body_sessions_age_and_kind() {
        let card = TaskCard::from_recoverable(&recoverable(
            "taskaaaa1111",
            Some("  fix the parser  "),
            &["s1", "s2"],
            Duration::from_secs(3600),
        ));
        assert_eq!(card.task_id, "taskaaaa1111");
        assert_eq!(card.short_id(), "taskaaaa");
        assert_eq!(card.kind, TaskKind::Recoverable);
        assert_eq!(card.body_preview(), "fix the parser", "body is trimmed");
        assert_eq!(card.session_summary(), "2 sessions");
        assert_eq!(card.age_label(), "1h ago");
        // Recovery rows do not surface attribution counts.
        assert_eq!(card.iris_files, None);
        assert_eq!(card.user_files, None);
        assert!(card.is_adoptable(), "a recoverable orphan is adoptable");
    }

    #[test]
    fn legacy_record_projects_placeholder_and_legacy_kind() {
        let card = TaskCard::from_recoverable(&RecoverableTask::for_test_legacy(
            "legacy00",
            Duration::ZERO,
        ));
        assert_eq!(card.kind, TaskKind::Legacy);
        assert_eq!(
            card.body_preview(),
            "(no description recorded)",
            "a legacy record with no body shows the placeholder"
        );
        assert_eq!(card.session_summary(), "0 sessions");
        assert!(
            card.is_adoptable(),
            "a legacy record is adoptable by selection"
        );
    }

    #[test]
    fn active_card_carries_counts_and_is_not_adoptable() {
        let display = ActiveTaskDisplay {
            task_id: "activetask01".to_string(),
            body: Some("investigate the leak".to_string()),
            sessions: vec!["only-session".to_string()],
        };
        let card = TaskCard::active(&display, Some(Duration::from_secs(120)), Some(3), Some(1));
        assert_eq!(card.kind, TaskKind::Active);
        assert!(!card.is_adoptable(), "the active task is never adoptable");
        assert_eq!(card.body_preview(), "investigate the leak");
        assert_eq!(card.session_summary(), "1 session");
        assert_eq!(card.age_label(), "2m ago");
        assert_eq!(card.iris_files, Some(3));
        assert_eq!(card.user_files, Some(1));
    }
}
