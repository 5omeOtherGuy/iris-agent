//! System-clipboard writes for `/copy` (Tier 3, presentation-only).
//!
//! Mirrors pi-mono's clipboard chain (`packages/coding-agent/src/utils/clipboard.ts`)
//! without a native clipboard addon: prefer the platform's daemonizing clipboard
//! tool (it keeps selection ownership after Iris exits), then fall back to the
//! OSC 52 terminal escape, which is also emitted for remote (SSH) sessions so
//! the clipboard lands on the local machine, not the remote host.
//!
//! No tool probing: each candidate is spawned directly and a missing binary
//! (`ErrorKind::NotFound`) simply advances to the next candidate.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use base64::Engine;

/// Cap on the base64-encoded OSC 52 payload, mirroring pi-mono: terminals cap
/// the escape-sequence length, and an oversized payload can desynchronize
/// rendering instead of failing cleanly.
const MAX_OSC52_ENCODED_LEN: usize = 100_000;

/// How a successful copy reached the clipboard, so the caller can phrase an
/// honest notice (OSC 52 depends on terminal support Iris cannot verify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyMethod {
    /// A platform clipboard tool (pbcopy, wl-copy, xclip, xsel, termux) took the
    /// text; the system clipboard is populated.
    NativeTool,
    /// The OSC 52 escape was written to the terminal; it lands in the clipboard
    /// only when the terminal supports (and permits) OSC 52.
    Osc52,
}

/// Copy `text` to the system clipboard. Tries the platform tools first; on a
/// remote session (or when every tool fails/is missing) falls back to OSC 52.
/// Errors only when no path accepted the text.
pub(crate) fn copy(text: &str) -> Result<CopyMethod> {
    let remote = is_remote_session();
    let native = copy_with_native_tool(text);
    // A remote session's native tools write the REMOTE host's clipboard, which
    // is almost never what the user wants; emit OSC 52 too so the terminal can
    // populate the local clipboard (pi-mono parity).
    if native && !remote {
        return Ok(CopyMethod::NativeTool);
    }
    if emit_osc52(text)? {
        return Ok(if native {
            CopyMethod::NativeTool
        } else {
            CopyMethod::Osc52
        });
    }
    if native {
        return Ok(CopyMethod::NativeTool);
    }
    bail!("no clipboard tool accepted the text (install wl-copy, xclip, or xsel)")
}

/// Whether this process runs over a remote shell, where a local clipboard write
/// must travel through the terminal (OSC 52) rather than the remote host.
fn is_remote_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
}

/// Try each platform clipboard tool in order; true when one accepted the text.
fn copy_with_native_tool(text: &str) -> bool {
    for argv in candidate_tools(&env_snapshot()) {
        match pipe_to_command(argv, text) {
            Ok(()) => return true,
            Err(error) => {
                tracing::debug!(tool = argv[0], error = %format!("{error:#}"), "clipboard tool failed");
            }
        }
    }
    false
}

/// Display-server/environment facts that pick the Linux tool order. Snapshotted
/// into a plain struct so the candidate policy is a pure, testable function.
struct EnvSnapshot {
    termux: bool,
    wayland: bool,
    x11: bool,
}

fn env_snapshot() -> EnvSnapshot {
    let set = |var: &str| std::env::var_os(var).is_some_and(|value| !value.is_empty());
    EnvSnapshot {
        termux: set("TERMUX_VERSION"),
        wayland: set("WAYLAND_DISPLAY"),
        x11: set("DISPLAY"),
    }
}

/// The ordered clipboard tool invocations for this platform. Linux ordering
/// mirrors pi-mono: Termux first (its env implies no wl/x tools), then Wayland's
/// wl-copy, then the X11 tools -- xclip preferred, xsel as its fallback.
fn candidate_tools(env: &EnvSnapshot) -> Vec<&'static [&'static str]> {
    if cfg!(target_os = "macos") {
        return vec![&["pbcopy"]];
    }
    let mut tools: Vec<&'static [&'static str]> = Vec::new();
    if env.termux {
        tools.push(&["termux-clipboard-set"]);
    }
    if env.wayland {
        tools.push(&["wl-copy"]);
    }
    if env.x11 {
        tools.push(&["xclip", "-selection", "clipboard"]);
        tools.push(&["xsel", "--clipboard", "--input"]);
    }
    tools
}

/// Spawn `argv` with `text` on stdin and stdout/stderr discarded, then wait.
/// Discarding the output pipes matters for wl-copy: it forks a child that keeps
/// clipboard ownership, and inherited pipes would make a wait hang until that
/// child exits (the execSync hang pi-mono works around).
fn pipe_to_command(argv: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))?;
    child
        .stdin
        .take()
        .context("clipboard tool stdin unavailable")?
        .write_all(text.as_bytes())
        .with_context(|| format!("failed to pipe text to {}", argv[0]))?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {}", argv[0]))?;
    if !status.success() {
        bail!("{} exited with {status}", argv[0]);
    }
    Ok(())
}

/// Write the OSC 52 clipboard escape to the terminal. Returns false (without
/// writing) when the encoded payload exceeds the terminal-safe cap. Safe in the
/// raw-mode TUI: OSC sequences move no cursor and paint no cells.
fn emit_osc52(text: &str) -> Result<bool> {
    let Some(sequence) = osc52_sequence(text) else {
        return Ok(false);
    };
    let mut stdout = std::io::stdout();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .context("failed to write OSC 52 escape to the terminal")?;
    Ok(true)
}

/// The OSC 52 set-clipboard escape for `text`, or `None` when the encoded
/// payload exceeds [`MAX_OSC52_ENCODED_LEN`].
fn osc52_sequence(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LEN {
        return None;
    }
    Some(format!("\x1b]52;c;{encoded}\x07"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_encodes_text_and_caps_payload() {
        assert_eq!(osc52_sequence("hi"), Some("\x1b]52;c;aGk=\x07".to_string()));
        // 100k encoded chars correspond to 75k input bytes; exceed that.
        let oversized = "x".repeat(MAX_OSC52_ENCODED_LEN);
        assert_eq!(osc52_sequence(&oversized), None);
    }

    #[test]
    fn linux_candidates_follow_display_server_order() {
        let names = |env: &EnvSnapshot| {
            candidate_tools(env)
                .iter()
                .map(|argv| argv[0])
                .collect::<Vec<_>>()
        };
        if cfg!(target_os = "macos") {
            let env = EnvSnapshot {
                termux: false,
                wayland: true,
                x11: true,
            };
            assert_eq!(names(&env), vec!["pbcopy"]);
            return;
        }
        let both = EnvSnapshot {
            termux: false,
            wayland: true,
            x11: true,
        };
        assert_eq!(names(&both), vec!["wl-copy", "xclip", "xsel"]);
        let x_only = EnvSnapshot {
            termux: false,
            wayland: false,
            x11: true,
        };
        assert_eq!(names(&x_only), vec!["xclip", "xsel"]);
        let termux = EnvSnapshot {
            termux: true,
            wayland: false,
            x11: false,
        };
        assert_eq!(names(&termux), vec!["termux-clipboard-set"]);
        let headless = EnvSnapshot {
            termux: false,
            wayland: false,
            x11: false,
        };
        assert!(names(&headless).is_empty());
    }

    #[test]
    fn pipe_to_command_reports_missing_binary_and_nonzero_exit() {
        assert!(pipe_to_command(&["iris-definitely-not-a-binary"], "x").is_err());
        assert!(pipe_to_command(&["false"], "x").is_err());
        assert!(pipe_to_command(&["cat"], "x").is_ok());
    }
}
