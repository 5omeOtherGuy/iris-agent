//! Shared terminal-environment detector for the rich TUI (issue #322).
//!
//! Both the screen-mode policy ([`super::screen_mode`]) and the
//! `/terminal-setup` doctor ([`super::terminal_doctor`]) need the same
//! environment facts -- `$TERM`, tmux presence, Zellij, dumb/GNU-screen
//! terminals -- and both used to spawn their own
//! `tmux display-message -p '#{client_control_mode}'` subprocess. This module
//! reads the env once and runs ONE timeout-guarded control-mode probe, then
//! hands each consumer a plain-data snapshot they interpret their own way.
//!
//! The control-mode probe result is kept RAW ([`ControlModeProbe`]) rather than
//! pre-reduced to a bool, because the two consumers deliberately fail in
//! opposite directions when the probe cannot answer: the screen-mode policy
//! fails TOWARD inline (treats an unavailable probe as control mode, never
//! risking a broken alt screen), while the doctor reports an unavailable probe
//! as plain tmux (unknown). Reduction lives in
//! [`TerminalEnv::tmux_control_mode_for_screen`] /
//! [`TerminalEnv::tmux_control_mode_for_doctor`] so both mappings stay pure and
//! table-testable without a real tmux or TTY.

/// Outcome of the single `tmux display-message -p '#{client_control_mode}'`
/// probe. Raw on purpose: each consumer applies its own failure direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlModeProbe {
    /// `$TMUX` was unset, so the probe was never run.
    NotInTmux,
    /// The probe succeeded; the trimmed `#{client_control_mode}` value
    /// (`"0"` outside control mode, non-`"0"` inside it).
    Reported(String),
    /// The probe failed, errored, produced no output, or timed out.
    Unavailable,
}

/// Snapshot of the shared terminal environment. Plain data so the derived
/// helpers stay pure and unit-testable; only [`TerminalEnv::detect`] touches
/// the real process environment.
#[derive(Debug, Clone)]
pub(crate) struct TerminalEnv {
    /// `$TERM`, with empty values normalized to `None`.
    pub(crate) term: Option<String>,
    /// `$TMUX` is set.
    pub(crate) tmux: bool,
    /// `$ZELLIJ` is set.
    pub(crate) zellij: bool,
    /// Raw control-mode probe result.
    pub(crate) control_mode: ControlModeProbe,
}

impl TerminalEnv {
    /// Snapshot the real environment: read the env vars once and run the one
    /// timeout-guarded control-mode probe (only when `$TMUX` is set, so the
    /// `tmux` binary exists in any healthy environment).
    pub(crate) fn detect() -> Self {
        let term = std::env::var("TERM").ok().filter(|value| !value.is_empty());
        let tmux = std::env::var_os("TMUX").is_some();
        let control_mode = if tmux {
            match tmux_probe(&["display-message", "-p", "#{client_control_mode}"]) {
                Some(value) => ControlModeProbe::Reported(value),
                None => ControlModeProbe::Unavailable,
            }
        } else {
            ControlModeProbe::NotInTmux
        };
        Self {
            term,
            tmux,
            zellij: std::env::var_os("ZELLIJ").is_some(),
            control_mode,
        }
    }

    /// `TERM=dumb`: no alt screen, no capabilities worth probing.
    pub(crate) fn term_is_dumb(&self) -> bool {
        self.term.as_deref() == Some("dumb")
    }

    /// GNU `screen` (a `screen*` `$TERM` that is not tmux): alt screen and
    /// OSC 52 are best-effort there.
    pub(crate) fn gnu_screen(&self) -> bool {
        self.term
            .as_deref()
            .is_some_and(|term| term.starts_with("screen"))
            && !self.tmux
    }

    /// Screen-mode reading: control mode iff the probe positively reports
    /// non-`"0"` OR could not answer. An unavailable probe fails TOWARD inline
    /// -- detection must never select a broken alt screen.
    pub(crate) fn tmux_control_mode_for_screen(&self) -> bool {
        match &self.control_mode {
            ControlModeProbe::NotInTmux => false,
            ControlModeProbe::Reported(value) => value != "0",
            ControlModeProbe::Unavailable => true,
        }
    }

    /// Doctor reading: control mode iff the probe positively reports non-`"0"`.
    /// An unavailable probe reads as plain tmux (unknown), matching the
    /// pre-refactor doctor, which never claimed control mode on probe failure.
    pub(crate) fn tmux_control_mode_for_doctor(&self) -> bool {
        matches!(&self.control_mode, ControlModeProbe::Reported(value) if value != "0")
    }
}

/// Best-effort tmux query with a hard timeout; `None` on any failure. Shared by
/// the control-mode probe and the doctor's clipboard queries.
pub(crate) fn tmux_probe(args: &[&str]) -> Option<String> {
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
    probe_command("tmux", args, PROBE_TIMEOUT)
}

/// Run `program args`, returning its trimmed stdout on success or `None` on any
/// failure or timeout. The child is spawned (not `output()`d) so a wedged
/// server can never wedge us: a helper thread drains stdout while the main
/// thread waits under one deadline that covers the WHOLE child lifetime --
/// stdout drain AND process exit. A child that closes stdout and then hangs is
/// still killed at the deadline. On any timeout we `kill()` + `wait()` the
/// child; killing closes the pipe, so the reader thread hits EOF and exits --
/// no leaked thread and no leaked/zombie process per probe. (Assumes the small
/// single-line output of a tmux query, so a full pipe can never deadlock the
/// child before it exits.)
fn probe_command(program: &str, args: &[&str], timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = std::time::Instant::now() + timeout;
    let kill_and_reap = |child: &mut std::process::Child| {
        // Kill closes the pipe so the reader unblocks, then reap.
        let _ = child.kill();
        let _ = child.wait();
    };
    match rx.recv_timeout(timeout) {
        Ok(buf) => {
            // Reader hit EOF (child closed stdout). The child may still hang, so
            // poll its exit only until the same deadline, then kill it. 5ms poll
            // is fine for a rare startup/doctor probe.
            let success = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status.success(),
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Ok(None) | Err(_) => {
                        kill_and_reap(&mut child);
                        break false;
                    }
                }
            };
            let _ = reader.join();
            if !success {
                return None;
            }
            let value = String::from_utf8_lossy(&buf).trim().to_string();
            if value.is_empty() { None } else { Some(value) }
        }
        Err(_) => {
            kill_and_reap(&mut child);
            let _ = reader.join();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(term: Option<&str>, tmux: bool, control_mode: ControlModeProbe) -> TerminalEnv {
        TerminalEnv {
            term: term.map(str::to_string),
            tmux,
            zellij: false,
            control_mode,
        }
    }

    #[test]
    fn term_derivations_parse_dumb_and_gnu_screen() {
        assert!(env_with(Some("dumb"), false, ControlModeProbe::NotInTmux).term_is_dumb());
        assert!(
            !env_with(Some("xterm-256color"), false, ControlModeProbe::NotInTmux).term_is_dumb()
        );
        assert!(!env_with(None, false, ControlModeProbe::NotInTmux).term_is_dumb());

        assert!(
            env_with(
                Some("screen.xterm-256color"),
                false,
                ControlModeProbe::NotInTmux
            )
            .gnu_screen()
        );
        // A `screen*` TERM inside tmux is tmux, not GNU screen.
        assert!(
            !env_with(
                Some("screen"),
                true,
                ControlModeProbe::Reported("0".to_string())
            )
            .gnu_screen()
        );
        assert!(!env_with(Some("xterm"), false, ControlModeProbe::NotInTmux).gnu_screen());
    }

    #[test]
    fn control_mode_probe_maps_per_consumer() {
        // Positive report: both consumers agree it is control mode.
        let reported_on = env_with(
            Some("xterm"),
            true,
            ControlModeProbe::Reported("1".to_string()),
        );
        assert!(reported_on.tmux_control_mode_for_screen());
        assert!(reported_on.tmux_control_mode_for_doctor());

        // Explicit "0": both agree it is NOT control mode.
        let reported_off = env_with(
            Some("xterm"),
            true,
            ControlModeProbe::Reported("0".to_string()),
        );
        assert!(!reported_off.tmux_control_mode_for_screen());
        assert!(!reported_off.tmux_control_mode_for_doctor());

        // Not in tmux: both false.
        let outside = env_with(Some("xterm"), false, ControlModeProbe::NotInTmux);
        assert!(!outside.tmux_control_mode_for_screen());
        assert!(!outside.tmux_control_mode_for_doctor());
    }

    #[test]
    fn unavailable_probe_fails_toward_inline_for_screen_only() {
        // The one intentional behavior split: a probe that cannot answer
        // (failure or 500ms timeout) reads as control mode for screen-mode
        // (fail toward inline) but as unknown/plain-tmux for the doctor.
        let unavailable = env_with(Some("xterm"), true, ControlModeProbe::Unavailable);
        assert!(unavailable.tmux_control_mode_for_screen());
        assert!(!unavailable.tmux_control_mode_for_doctor());
    }

    // The probe tests exercise `probe_command` with ordinary commands instead
    // of a real tmux, matching this module's tmux-free test style.

    #[cfg(unix)]
    #[test]
    fn probe_command_kills_and_reaps_child_on_timeout() {
        // `sleep 5` far outlives the 50ms timeout. If the timeout path did not
        // kill AND reap the child, the internal `wait()` would block for the
        // full 5s; returning quickly proves the child was killed and reaped and
        // the reader thread unblocked. Guards against the pre-fix leak where the
        // helper thread stayed blocked in `output()` and the child was orphaned.
        let start = std::time::Instant::now();
        let result = probe_command("sleep", &["5"], std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();
        assert_eq!(result, None, "timed-out probe must be Unavailable");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "probe did not kill+reap the child within budget: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_command_kills_child_that_closes_stdout_then_hangs() {
        // The deadline must cover the whole child lifetime, not just stdout
        // EOF: a child that closes fd 1 and then sleeps would block a naive
        // `wait()` after the successful drain. Returning quickly proves the
        // post-EOF exit poll kills it at the deadline.
        let start = std::time::Instant::now();
        let result = probe_command(
            "sh",
            &["-c", "exec >&-; sleep 5"],
            std::time::Duration::from_millis(50),
        );
        let elapsed = start.elapsed();
        assert_eq!(result, None, "killed-at-deadline probe must be Unavailable");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "probe did not enforce the deadline past stdout EOF: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_command_returns_trimmed_stdout_on_success() {
        let result = probe_command("printf", &["hello\n"], std::time::Duration::from_secs(5));
        assert_eq!(result.as_deref(), Some("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_command_empty_output_is_none() {
        // Exit success but no output trims to empty -> None (unavailable).
        let result = probe_command("true", &[], std::time::Duration::from_secs(5));
        assert_eq!(result, None);
    }

    #[cfg(unix)]
    #[test]
    fn probe_command_nonzero_exit_is_none() {
        let result = probe_command("false", &[], std::time::Duration::from_secs(5));
        assert_eq!(result, None);
    }

    #[test]
    fn probe_command_missing_program_is_none() {
        let result = probe_command(
            "iris-nonexistent-probe-binary",
            &[],
            std::time::Duration::from_secs(5),
        );
        assert_eq!(result, None);
    }
}
