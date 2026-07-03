//! `/terminal-setup` capability doctor (ADR-0029, Milestone 6 S5).
//!
//! Reports the terminal capabilities the pager depends on -- multiplexer,
//! SSH, kitty keyboard protocol, OSC 52 clipboard, Shift+Enter -- with exact
//! fix lines where a capability is off. The report builder is a pure function
//! over an environment snapshot so every line is table-testable; only
//! [`detect`] touches the real environment (env vars + best-effort `tmux
//! show` probes).
//!
//! Line grammar: state symbol + label (never color alone), one capability per
//! line, fix lines indented beneath the finding they repair.

use crate::ui::symbols::{CANCELLED, DONE, REVIEW};

/// Environment snapshot the report is built from.
#[derive(Debug, Default)]
pub(crate) struct DoctorEnv {
    pub(crate) term: Option<String>,
    pub(crate) term_program: Option<String>,
    pub(crate) tmux: bool,
    pub(crate) tmux_control_mode: bool,
    pub(crate) zellij: bool,
    pub(crate) gnu_screen: bool,
    pub(crate) ssh: bool,
    /// Whether the kitty keyboard protocol was actually negotiated this
    /// session (the TUI probes and pushes the flags at startup).
    pub(crate) kitty_keyboard: bool,
    /// Whether the alt-screen pager renders this session.
    pub(crate) pager_active: bool,
    /// `tmux show -gv set-clipboard`, when inside tmux and the probe worked.
    pub(crate) tmux_set_clipboard: Option<String>,
    /// `tmux show -gv allow-passthrough` (tmux >= 3.3), likewise.
    pub(crate) tmux_allow_passthrough: Option<String>,
}

/// Build the full report as transcript notice lines.
pub(crate) fn report(env: &DoctorEnv) -> Vec<String> {
    let mut lines = Vec::new();

    // Terminal identity: environment-reported (an XTVERSION query needs a
    // response read outside the event loop; TERM/TERM_PROGRAM is the honest
    // no-roundtrip answer).
    let term = env.term.as_deref().unwrap_or("unknown");
    match env.term_program.as_deref() {
        Some(program) => lines.push(format!("{DONE} terminal: {program} (TERM={term})")),
        None => lines.push(format!("{DONE} terminal: TERM={term}")),
    }

    // Screen mode.
    if env.pager_active {
        lines.push(format!("{DONE} screen mode: pager (alternate screen)"));
    } else {
        lines.push(format!(
            "{CANCELLED} screen mode: inline (native scrollback; pager off or degraded)"
        ));
    }

    // Multiplexer.
    if env.tmux_control_mode {
        lines.push(format!(
            "{REVIEW} multiplexer: tmux control mode (iTerm2 -CC); pager degrades to inline"
        ));
    } else if env.tmux {
        lines.push(format!("{DONE} multiplexer: tmux"));
    } else if env.zellij {
        lines.push(format!(
            "{REVIEW} multiplexer: Zellij; pager degrades to inline"
        ));
    } else if env.gnu_screen {
        lines.push(format!(
            "{REVIEW} multiplexer: GNU screen; alt screen and OSC 52 are best-effort"
        ));
    } else {
        lines.push(format!("{DONE} multiplexer: none"));
    }

    // SSH.
    if env.ssh {
        lines.push(format!(
            "{REVIEW} ssh session: clipboard copies travel via OSC 52 through your terminal"
        ));
        lines.push("    (macOS Terminal.app does not support OSC 52; use iTerm2/kitty/WezTerm for copy over SSH)".to_string());
    } else {
        lines.push(format!(
            "{DONE} ssh: not detected (local clipboard tools available)"
        ));
    }

    // Kitty keyboard protocol + Shift+Enter.
    if env.kitty_keyboard {
        lines.push(format!(
            "{DONE} kitty keyboard protocol: negotiated (Shift+Enter and modified keys are distinct)"
        ));
    } else {
        lines.push(format!(
            "{REVIEW} kitty keyboard protocol: not supported; Shift+Enter may equal Enter"
        ));
        lines.push(
            "    newline fallback: use Ctrl+J to insert a newline in the composer".to_string(),
        );
        if env.tmux {
            lines.push("    tmux: set -g extended-keys on".to_string());
            lines.push("    tmux: set -as terminal-features ',xterm*:extkeys'".to_string());
        }
    }

    // OSC 52 / tmux clipboard plumbing.
    if env.tmux {
        match env.tmux_set_clipboard.as_deref() {
            Some("on") => lines.push(format!("{DONE} tmux set-clipboard: on (OSC 52 copy works)")),
            Some(value) => {
                lines.push(format!(
                    "{REVIEW} tmux set-clipboard: {value}; OSC 52 clipboard passthrough is limited"
                ));
                lines.push("    fix: set -g set-clipboard on".to_string());
            }
            None => {
                lines.push(format!(
                    "{REVIEW} tmux set-clipboard: unknown (probe failed)"
                ));
                lines.push("    fix: set -g set-clipboard on".to_string());
            }
        }
        match env.tmux_allow_passthrough.as_deref() {
            Some("on" | "all") => {
                lines.push(format!("{DONE} tmux allow-passthrough: on"));
            }
            Some(value) => {
                lines.push(format!(
                    "{REVIEW} tmux allow-passthrough: {value}; nested escape passthrough is off"
                ));
                lines.push("    fix: set -g allow-passthrough on".to_string());
            }
            None => {
                lines.push(format!(
                    "{CANCELLED} tmux allow-passthrough: unknown (tmux < 3.3 or probe failed)"
                ));
            }
        }
    } else {
        lines.push(format!(
            "{DONE} clipboard: platform tools first, OSC 52 fallback (`/copy`)"
        ));
    }

    lines
}

/// Snapshot the real environment. `kitty_keyboard` and `pager_active` come
/// from the live TUI (they reflect what was actually negotiated at startup,
/// not a re-probe).
pub(crate) fn detect(kitty_keyboard: bool, pager_active: bool) -> DoctorEnv {
    let term = std::env::var("TERM").ok().filter(|value| !value.is_empty());
    let tmux = std::env::var_os("TMUX").is_some();
    DoctorEnv {
        term_program: std::env::var("TERM_PROGRAM")
            .ok()
            .filter(|value| !value.is_empty()),
        tmux,
        tmux_control_mode: tmux
            && tmux_probe(&["display-message", "-p", "#{client_control_mode}"])
                .is_some_and(|value| value != "0"),
        zellij: std::env::var_os("ZELLIJ").is_some(),
        gnu_screen: term
            .as_deref()
            .is_some_and(|term| term.starts_with("screen"))
            && !tmux,
        ssh: ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty())),
        kitty_keyboard,
        pager_active,
        tmux_set_clipboard: if tmux {
            tmux_probe(&["show", "-gv", "set-clipboard"])
        } else {
            None
        },
        tmux_allow_passthrough: if tmux {
            tmux_probe(&["show", "-gv", "allow-passthrough"])
        } else {
            None
        },
        term,
    }
}

/// Best-effort tmux query with a hard timeout; `None` on any failure. The
/// probe runs on a helper thread so a wedged tmux server can never block the
/// TUI event loop; on timeout the thread is abandoned (it exits when the
/// child does) and the capability reports as unknown.
fn tmux_probe(args: &[&str]) -> Option<String> {
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::process::Command::new("tmux").args(&args).output();
        let _ = tx.send(result);
    });
    let output = rx.recv_timeout(PROBE_TIMEOUT).ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> DoctorEnv {
        DoctorEnv {
            term: Some("xterm-256color".to_string()),
            kitty_keyboard: true,
            pager_active: true,
            ..DoctorEnv::default()
        }
    }

    #[test]
    fn healthy_plain_terminal_reports_all_green() {
        let lines = report(&plain());
        let all = lines.join("\n");
        assert!(all.contains("TERM=xterm-256color"));
        assert!(all.contains("screen mode: pager"));
        assert!(all.contains("multiplexer: none"));
        assert!(all.contains("kitty keyboard protocol: negotiated"));
        assert!(
            !all.contains("fix:"),
            "healthy env needs no fix lines: {all}"
        );
        // Every finding line leads with a state symbol.
        for line in lines.iter().filter(|line| !line.starts_with("    ")) {
            assert!(
                [DONE, REVIEW, CANCELLED]
                    .iter()
                    .any(|symbol| line.starts_with(symbol)),
                "line missing state symbol: {line:?}"
            );
        }
    }

    #[test]
    fn tmux_with_clipboard_off_prints_exact_fix_lines() {
        let env = DoctorEnv {
            tmux: true,
            tmux_set_clipboard: Some("off".to_string()),
            tmux_allow_passthrough: Some("off".to_string()),
            kitty_keyboard: false,
            ..plain()
        };
        let all = report(&env).join("\n");
        assert!(all.contains("fix: set -g set-clipboard on"));
        assert!(all.contains("fix: set -g allow-passthrough on"));
        assert!(all.contains("set -g extended-keys on"));
        assert!(all.contains("set -as terminal-features ',xterm*:extkeys'"));
        assert!(all.contains("use Ctrl+J to insert a newline"));
    }

    #[test]
    fn degraded_environments_are_reported_honestly() {
        let control = DoctorEnv {
            tmux: true,
            tmux_control_mode: true,
            pager_active: false,
            ..plain()
        };
        let all = report(&control).join("\n");
        assert!(all.contains("tmux control mode"));
        assert!(all.contains("screen mode: inline"));

        let ssh = DoctorEnv {
            ssh: true,
            ..plain()
        };
        let all = report(&ssh).join("\n");
        assert!(all.contains("OSC 52"));
        assert!(all.contains("macOS Terminal.app does not support OSC 52"));

        let zellij = DoctorEnv {
            zellij: true,
            ..plain()
        };
        assert!(report(&zellij).join("\n").contains("Zellij"));
    }
}
