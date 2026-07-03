//! Screen-mode policy for the rich TUI (ADR-0029): decide between the
//! alt-screen pager and the inline terminal-surface renderer.
//!
//! Precedence: `--no-alt-screen` CLI flag, then `IRIS_NO_ALT_SCREEN`, then the
//! `tui.altScreen` setting (`"auto" | "always" | "never"`). `auto` degrades to
//! inline (with a one-line notice) in tmux control mode, Zellij, dumb
//! terminals, and non-TTY stdio -- detection failures always fail TOWARD the
//! inline fallback, never toward a broken alt screen. The resolution core is a
//! pure function over an environment snapshot so the whole policy table is
//! unit-testable without a TTY.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// Configured alt-screen policy (`tui.altScreen` in settings.json).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AltScreenConfig {
    /// Pager on plain terminals and normal tmux; inline in tmux control mode,
    /// Zellij, and dumb terminals.
    Auto,
    /// Pager whenever the terminal can host one (only non-TTY stdio and
    /// `TERM=dumb` still degrade); multiplexer heuristics are overridden.
    Always,
    /// Never enter the alt screen; always the inline renderer.
    Never,
}

/// Built-in default until the pager is feature-complete (flips to `Auto` once
/// mouse + clipboard land -- Milestone 6 S4).
pub(crate) const DEFAULT_ALT_SCREEN: AltScreenConfig = AltScreenConfig::Never;

impl AltScreenConfig {
    /// Parse a settings value. `None` for anything but the three documented
    /// strings, so the caller can warn and fall back to the default loudly
    /// instead of silently guessing.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// The render backend the session will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenMode {
    /// Alt-screen pager (full-frame render, Iris-owned scrollback).
    Pager,
    /// Inline terminal-surface renderer (native scrollback; today's behavior).
    Inline,
}

/// Resolved mode plus any one-line degradation/config notices to surface in
/// the transcript.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolution {
    pub(crate) mode: ScreenMode,
    pub(crate) notices: Vec<String>,
}

impl Resolution {
    fn inline() -> Self {
        Self {
            mode: ScreenMode::Inline,
            notices: Vec::new(),
        }
    }
}

/// Snapshot of the terminal environment the policy consults. Kept as plain
/// data so [`resolve`] stays pure and table-testable.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TerminalEnv {
    pub(crate) stdout_tty: bool,
    pub(crate) term_dumb: bool,
    pub(crate) tmux_control_mode: bool,
    pub(crate) zellij: bool,
}

/// Pure mode resolution. `no_alt_screen` carries the CLI flag / env opt-out.
pub(crate) fn resolve(
    config: AltScreenConfig,
    no_alt_screen: bool,
    env: &TerminalEnv,
) -> Resolution {
    if no_alt_screen || config == AltScreenConfig::Never {
        return Resolution::inline();
    }
    // Hard blockers: a pager cannot work at all here, so even `always`
    // degrades (fail toward inline, never toward a broken alt screen).
    if !env.stdout_tty || env.term_dumb {
        let mut resolution = Resolution::inline();
        if config == AltScreenConfig::Always {
            resolution
                .notices
                .push("alt screen unavailable on this terminal; running inline".to_string());
        }
        return resolution;
    }
    if config == AltScreenConfig::Always {
        return Resolution {
            mode: ScreenMode::Pager,
            notices: Vec::new(),
        };
    }
    // `auto`: multiplexer heuristics degrade honestly, with a notice.
    if env.tmux_control_mode {
        return Resolution {
            mode: ScreenMode::Inline,
            notices: vec!["tmux control mode detected; running in inline mode".to_string()],
        };
    }
    if env.zellij {
        return Resolution {
            mode: ScreenMode::Inline,
            notices: vec!["Zellij detected; running in inline mode".to_string()],
        };
    }
    Resolution {
        mode: ScreenMode::Pager,
        notices: Vec::new(),
    }
}

/// `--no-alt-screen` was passed on the command line (recorded once at startup
/// by `main::dispatch`, before the positional command table runs).
static NO_ALT_SCREEN_CLI: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_no_alt_screen_cli() {
    NO_ALT_SCREEN_CLI.store(true, Ordering::Relaxed);
}

fn no_alt_screen_requested() -> bool {
    NO_ALT_SCREEN_CLI.load(Ordering::Relaxed)
        || crate::config::iris_flag_enabled("IRIS_NO_ALT_SCREEN")
}

/// Resolve the screen mode for an interactive startup from the raw
/// `tui.altScreen` settings value. An unrecognized value is reported (never
/// silently reinterpreted) and falls back to the built-in default. Detection
/// subprocess work (the tmux control-mode probe) only runs when the config can
/// actually select the pager.
pub(crate) fn resolve_for_startup(alt_screen_setting: Option<&str>) -> Resolution {
    let mut config_notices = Vec::new();
    let config = match alt_screen_setting {
        None => DEFAULT_ALT_SCREEN,
        Some(raw) => match AltScreenConfig::parse(raw) {
            Some(config) => config,
            None => {
                config_notices.push(format!(
                    "ignoring invalid tui.altScreen value {raw:?} (expected \"auto\", \"always\", or \"never\")"
                ));
                DEFAULT_ALT_SCREEN
            }
        },
    };
    if no_alt_screen_requested() || config == AltScreenConfig::Never {
        return Resolution {
            mode: ScreenMode::Inline,
            notices: config_notices,
        };
    }
    let mut resolution = resolve(config, false, &detect_environment());
    let mut notices = config_notices;
    notices.append(&mut resolution.notices);
    resolution.notices = notices;
    resolution
}

fn detect_environment() -> TerminalEnv {
    let tmux = std::env::var_os("TMUX").is_some();
    TerminalEnv {
        stdout_tty: std::io::stdout().is_terminal(),
        term_dumb: std::env::var("TERM").is_ok_and(|term| term == "dumb"),
        tmux_control_mode: tmux && tmux_control_mode_probe(),
        zellij: std::env::var_os("ZELLIJ").is_some(),
    }
}

/// Best-effort tmux control-mode probe. Only called when `$TMUX` is set, so
/// the `tmux` binary exists in any healthy environment. A failed or
/// unparseable probe reports control mode (-> inline): detection must fail
/// toward the inline fallback, never toward a broken alt screen.
fn tmux_control_mode_probe() -> bool {
    match std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{client_control_mode}"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() != "0"
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_tty() -> TerminalEnv {
        TerminalEnv {
            stdout_tty: true,
            term_dumb: false,
            tmux_control_mode: false,
            zellij: false,
        }
    }

    #[test]
    fn parse_accepts_the_three_documented_values_case_insensitively() {
        assert_eq!(AltScreenConfig::parse("auto"), Some(AltScreenConfig::Auto));
        assert_eq!(
            AltScreenConfig::parse("Always"),
            Some(AltScreenConfig::Always)
        );
        assert_eq!(
            AltScreenConfig::parse(" never "),
            Some(AltScreenConfig::Never)
        );
        assert_eq!(AltScreenConfig::parse("fullscreen"), None);
        assert_eq!(AltScreenConfig::parse(""), None);
    }

    #[test]
    fn resolution_table_covers_flag_config_and_environment() {
        use AltScreenConfig::*;
        use ScreenMode::*;
        let control = TerminalEnv {
            tmux_control_mode: true,
            ..plain_tty()
        };
        let zellij = TerminalEnv {
            zellij: true,
            ..plain_tty()
        };
        let dumb = TerminalEnv {
            term_dumb: true,
            ..plain_tty()
        };
        let no_tty = TerminalEnv {
            stdout_tty: false,
            ..plain_tty()
        };
        // (config, no_alt_screen, env, expected mode, expects notice)
        let table: &[(AltScreenConfig, bool, &TerminalEnv, ScreenMode, bool)] = &[
            // The opt-out flag wins over everything, silently.
            (Always, true, &plain_tty(), Inline, false),
            (Auto, true, &plain_tty(), Inline, false),
            // `never` is inline everywhere, silently.
            (Never, false, &plain_tty(), Inline, false),
            (Never, false, &control, Inline, false),
            // `always` forces the pager past multiplexer heuristics...
            (Always, false, &plain_tty(), Pager, false),
            (Always, false, &control, Pager, false),
            (Always, false, &zellij, Pager, false),
            // ...but hard blockers still degrade, with a notice.
            (Always, false, &dumb, Inline, true),
            (Always, false, &no_tty, Inline, true),
            // `auto`: pager on plain terminals, inline + notice on degrade.
            (Auto, false, &plain_tty(), Pager, false),
            (Auto, false, &control, Inline, true),
            (Auto, false, &zellij, Inline, true),
            (Auto, false, &dumb, Inline, false),
            (Auto, false, &no_tty, Inline, false),
        ];
        for (config, no_alt, env, mode, notice) in table {
            let resolution = resolve(*config, *no_alt, env);
            assert_eq!(
                resolution.mode, *mode,
                "mode for config={config:?} no_alt={no_alt} env={env:?}"
            );
            assert_eq!(
                !resolution.notices.is_empty(),
                *notice,
                "notice for config={config:?} no_alt={no_alt} env={env:?}: {:?}",
                resolution.notices
            );
        }
    }

    #[test]
    fn auto_in_normal_tmux_selects_the_pager() {
        // Normal tmux is NOT control mode; only the control-mode probe result
        // matters, never the mere presence of tmux.
        let normal_tmux = plain_tty();
        assert_eq!(
            resolve(AltScreenConfig::Auto, false, &normal_tmux).mode,
            ScreenMode::Pager
        );
    }
}
