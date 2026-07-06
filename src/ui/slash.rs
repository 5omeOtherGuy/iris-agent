//! Slash-command registry and palette filtering (Tier 3, presentation-only).
//!
//! A small data-driven registry replaces the former two-string `is_exit_command`
//! match: the TUI palette filters this list as the user types `/`, and the
//! non-TTY text path consults [`is_exit`] for the same commands. Commands are
//! registered here ONLY when a real backing action exists; `/exit` and `/quit`
//! end the session, and every `Submit` command routes to a real handler --
//! `/model`/`/reasoning` to the shared model-switch handler
//! (`crate::cli::handle_model_command`), `/copy`/`/session` to the shared line
//! builders in `crate::cli`, `/compact` to the harness's on-demand compaction,
//! and the rest to their TUI pickers/handlers in
//! `crate::ui::tui_loop::route_command`. Adding a command with no action would
//! lie to the user, so the list stays honest and short.

/// What accepting a slash command does. The loop owns dispatch; this enum is the
/// neutral contract so the registry never reaches into the event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlashAction {
    /// End the interactive session.
    Exit,
    /// Submit the command name as a line so the shared command handler routes it
    /// (e.g. `/model`, `/reasoning`). The handler parses any arguments the user
    /// typed after the name; a palette-accepted bare command submits just the
    /// name (read-only / usage view).
    Submit,
}

/// One registered command: the literal token the user types, a one-line
/// description shown in the palette, and the action it dispatches.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: SlashAction,
}

/// The full command registry. Keep entries backed by a real action.
pub(crate) const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/exit",
        description: "End the session",
        action: SlashAction::Exit,
    },
    SlashCommand {
        name: "/quit",
        description: "End the session (alias of /exit)",
        action: SlashAction::Exit,
    },
    SlashCommand {
        name: "/model",
        description: "Show or switch provider/model",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/reasoning",
        description: "Set reasoning effort [off|minimal|low|medium|high|xhigh]",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/resume",
        description: "Resume a prior session for this directory",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/new",
        description: "Start a fresh session",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/session",
        description: "Show session info (file, id, messages, tokens)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/tasks",
        description: "Open the task surface (active + recoverable tasks; adopt/inspect)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/sessions",
        description: "List sessions that worked a task id (deterministic lookup)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/copy",
        description: "Copy the last assistant reply to the clipboard",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/compact",
        description: "Summarize older context to free the window before a switch",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/context",
        description: "Show the context breakdown (system, conversation, summaries, folds, headroom)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/debug",
        description: "Write a debug snapshot (screen + transcript) to a file",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/scoped-models",
        description: "Enable/disable models for Ctrl+P cycling",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/settings",
        description: "Open settings menu",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/approval",
        description: "Set the approval preset [strict|auto|never]",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/trust",
        description: "View or edit this project's permission policy",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/permissions",
        description: "Alias for /trust project permissions",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/login",
        description: "Configure provider authentication",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/logout",
        description: "Remove provider authentication",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/find",
        description: "Search the transcript (n/N navigate; /find clears) (pager mode)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/terminal-setup",
        description: "Check terminal capabilities (multiplexer, OSC 52, keys) with fixes",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/mouse",
        description: "Toggle mouse capture (off = terminal-native select/copy) (pager mode)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/git",
        description: "Open the git console (switch, worktrees, settle the task)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/tree",
        description: "Open the directory tree (↵ references a file in the composer)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/diff",
        description: "Show the current task's net diff (Iris's own changes)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/rollback",
        description: "List or restore a task checkpoint (undoes Iris's own work)",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/accept",
        description: "Accept the current Iris changes and settle the task",
        action: SlashAction::Submit,
    },
    SlashCommand {
        name: "/checkpoint",
        description: "Save an explicit checkpoint and settle the task",
        action: SlashAction::Submit,
    },
];

/// Whether `input` is a command line: a single line beginning with `/`. A
/// multi-line buffer (a real message that happens to start with `/`) is not a
/// command, so the palette never hijacks a pasted block.
pub(crate) fn is_command_line(input: &str) -> bool {
    input.starts_with('/') && !input.contains('\n')
}

/// Registry commands whose name starts with `input` (case-insensitive). `/`
/// alone returns every command; `/ex` narrows to `/exit`. Returns empty when no
/// command matches, so the caller can fall back to sending the raw text.
pub(crate) fn matches(input: &str) -> Vec<&'static SlashCommand> {
    if !is_command_line(input) {
        return Vec::new();
    }
    let needle = input.trim_end().to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|cmd| cmd.name.to_ascii_lowercase().starts_with(&needle))
        .collect()
}

/// Whether a submitted prompt is an exit command, by registry lookup rather than
/// a bare string match. Used by the non-TTY text path, which has no palette.
pub(crate) fn is_exit(prompt: &str) -> bool {
    let trimmed = prompt.trim();
    COMMANDS
        .iter()
        .any(|cmd| cmd.name.eq_ignore_ascii_case(trimmed) && cmd.action == SlashAction::Exit)
}

/// Palette selection state: which filtered row is highlighted. `open` mirrors
/// whether the current editor input is a command line; the loop syncs it after
/// every edit so navigation and rendering agree on one source of truth.
#[derive(Debug, Default)]
pub(crate) struct Palette {
    open: bool,
    selected: usize,
}

impl Palette {
    /// Recompute open-state and clamp the selection after the input changed.
    pub(crate) fn sync(&mut self, input: &str) {
        self.open = is_command_line(input);
        let count = matches(input).len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Whether the palette should be shown: open AND at least one match.
    pub(crate) fn is_active(&self, input: &str) -> bool {
        self.open && !matches(input).is_empty()
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// Move the highlight up one row (saturating at the top).
    pub(crate) fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down one row (saturating at the last match).
    pub(crate) fn down(&mut self, input: &str) {
        let count = matches(input).len();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    /// The currently highlighted command for `input`, if the palette is active.
    pub(crate) fn accept(&self, input: &str) -> Option<&'static SlashCommand> {
        if !self.open {
            return None;
        }
        matches(input).get(self.selected).copied()
    }

    /// Force the palette closed (Esc), keeping the editor text intact.
    pub(crate) fn dismiss(&mut self) {
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_requires_leading_slash_single_line() {
        assert!(is_command_line("/ex"));
        assert!(is_command_line("/"));
        assert!(!is_command_line("hello"));
        assert!(!is_command_line("/multi\nline"));
    }

    #[test]
    fn matches_filters_by_prefix_case_insensitively() {
        assert_eq!(matches("/").len(), COMMANDS.len());
        let ex = matches("/EX");
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].name, "/exit");
        assert_eq!(matches("/q")[0].name, "/quit");
        assert_eq!(matches("/m")[0].name, "/model");
        assert_eq!(matches("/rea")[0].name, "/reasoning");
        assert_eq!(matches("/c")[0].name, "/copy");
        assert_eq!(matches("/d")[0].name, "/debug");
        // `/se` narrows to the session/sessions/settings set, registry order
        // preserved.
        let se = matches("/se");
        assert_eq!(se.len(), 3);
        assert_eq!(se[0].name, "/session");
        assert_eq!(se[1].name, "/sessions");
        assert_eq!(se[2].name, "/settings");
        assert!(matches("/zzz").is_empty());
        assert!(matches("hello").is_empty());
    }

    #[test]
    fn tasks_command_is_registered_and_submits() {
        // Regression: `/tasks` has a real handler in `route_command` (the
        // ADR-0031 task surface); it must be in the registry so the palette
        // makes it discoverable, like every other backed command.
        let cmd = COMMANDS
            .iter()
            .find(|c| c.name == "/tasks")
            .expect("/tasks must be registered");
        assert_eq!(cmd.action, SlashAction::Submit);
        // `/task` (and the full `/tasks`) narrow to it in the palette.
        assert_eq!(matches("/task")[0].name, "/tasks");
    }

    #[test]
    fn is_exit_is_registry_backed() {
        assert!(is_exit("/exit"));
        // Case-insensitive so a /EXIT typed past the palette still exits.
        assert!(is_exit("/EXIT"));
        // pi-mono parity: /quit is a registered exit alias.
        assert!(is_exit("/quit"));
        assert!(!is_exit("/export"));
        assert!(!is_exit("exit"));
    }

    #[test]
    fn palette_navigation_clamps_and_accepts() {
        let mut p = Palette::default();
        p.sync("/");
        assert!(p.is_active("/"));
        assert_eq!(p.selected(), 0);
        // Down walks to the last row, then clamps there.
        for _ in 0..COMMANDS.len() + 6 {
            p.down("/");
        }
        assert_eq!(p.selected(), COMMANDS.len() - 1);
        assert_eq!(
            p.accept("/").unwrap().name,
            COMMANDS[COMMANDS.len() - 1].name
        );
        // Up returns toward the second-to-last row.
        p.up();
        assert_eq!(
            p.accept("/").unwrap().name,
            COMMANDS[COMMANDS.len() - 2].name
        );
    }

    #[test]
    fn narrowing_input_reclamps_selection() {
        let mut p = Palette::default();
        p.sync("/");
        p.down("/");
        assert_eq!(p.selected(), 1);
        // Typing narrows to a single match; selection clamps back to 0.
        p.sync("/ex");
        assert_eq!(p.selected(), 0);
        assert_eq!(p.accept("/ex").unwrap().name, "/exit");
    }

    #[test]
    fn dismiss_closes_until_resynced() {
        let mut p = Palette::default();
        p.sync("/ex");
        assert!(p.is_active("/ex"));
        p.dismiss();
        assert!(!p.is_active("/ex"));
        assert!(p.accept("/ex").is_none());
        // A later edit reopens it.
        p.sync("/exi");
        assert!(p.is_active("/exi"));
    }
}
