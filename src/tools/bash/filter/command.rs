//! Effective-command extraction for filter dispatch (ADR-0037).
//!
//! Filters are keyed on the parsed program + subcommand of the command that
//! produced the output. The command string the model sends can carry shell
//! plumbing around that program: `cd x && cargo test`, `VAR=1 cargo test`,
//! `sudo systemctl status`, `cargo test 2>&1 | tail`. This module reduces the
//! command string to the last top-level pipeline/sequence segment (the one
//! whose output dominates what was captured) with leading environment
//! assignments and wrapper programs removed.
//!
//! The parse is deliberately conservative: quote- and substitution-aware
//! splitting only, no real shell grammar. Anything ambiguous (unbalanced
//! quotes, empty result) returns `None` and the output passes through
//! unfiltered -- dispatch must never guess.

/// Wrapper programs that run another program and relay its output; skipped
/// (along with their leading flags/assignments) when locating the effective
/// program.
const WRAPPERS: &[&str] = &["sudo", "env", "command", "time", "nohup", "nice", "stdbuf"];

/// Reduce a shell command string to the effective command for filter matching:
/// the last top-level segment, minus env assignments and wrapper programs,
/// re-joined with single spaces. `None` when nothing can be extracted safely.
pub(super) fn effective_command(command: &str) -> Option<String> {
    let segment = last_segment(command)?;
    let mut tokens = segment.split_whitespace().peekable();
    // Skip leading `VAR=value` assignments and wrapper programs (with their
    // flags and, for `env`, their own assignments).
    loop {
        let tok = tokens.peek()?;
        if is_assignment(tok) {
            tokens.next();
        } else if WRAPPERS.contains(tok) {
            tokens.next();
            // Consume the wrapper's own flags (`sudo -u user`, `nice -n 10`,
            // `stdbuf -o0`). Flag arguments that don't start with `-` (like the
            // `10` in `nice -n 10`) are not consumed; that conservatively
            // yields a non-match rather than a wrong match.
            while tokens.peek().is_some_and(|t| t.starts_with('-')) {
                tokens.next();
            }
        } else {
            break;
        }
    }
    let rest: Vec<&str> = tokens.collect();
    if rest.is_empty() {
        return None;
    }
    Some(rest.join(" "))
}

fn is_assignment(token: &str) -> bool {
    let Some(eq) = token.find('=') else {
        return false;
    };
    let name = &token[..eq];
    !name.is_empty()
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

/// Split at top-level `;`, `&`, `|`, and newlines (which covers `&&`, `||`,
/// and pipes as empty-segment noise) and return the last non-empty segment.
/// Quote-, escape-, and substitution-aware; `None` on unbalanced quoting.
fn last_segment(command: &str) -> Option<String> {
    let mut segments: Vec<String> = vec![String::new()];
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0usize; // $( ... ) and ( ... ) nesting
    let mut in_backtick = false;
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            push(&mut segments, c);
            prev = Some(c);
            continue;
        }
        match c {
            '\\' => {
                push(&mut segments, c);
                prev = chars.next().inspect(|&next| push(&mut segments, next));
                continue;
            }
            '\'' if !in_double => {
                in_single = true;
                push(&mut segments, c);
            }
            '"' => {
                in_double = !in_double;
                push(&mut segments, c);
            }
            '`' => {
                in_backtick = !in_backtick;
                push(&mut segments, c);
            }
            '(' => {
                depth += 1;
                push(&mut segments, c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                push(&mut segments, c);
            }
            // `&` in a redirection is not a separator: `2>&1`, `>&2`, `<&0`,
            // `&>log`, `&>>log` all keep the segment intact.
            '&' if !in_double
                && !in_backtick
                && depth == 0
                && (matches!(prev, Some('>' | '<')) || matches!(chars.peek(), Some('>'))) =>
            {
                push(&mut segments, c);
            }
            ';' | '&' | '|' | '\n' if !in_double && !in_backtick && depth == 0 => {
                segments.push(String::new());
            }
            _ => push(&mut segments, c),
        }
        prev = Some(c);
    }
    if in_single || in_double || in_backtick {
        return None; // unbalanced quoting: refuse to guess
    }
    let seg = segments
        .into_iter()
        .rev()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())?;
    // A parenthesized subshell is transparent for dispatch: recurse into it.
    if let Some(inner) = seg.strip_prefix('(') {
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        return last_segment(inner);
    }
    Some(seg)
}

fn push(segments: &mut [String], c: char) {
    if let Some(last) = segments.last_mut() {
        last.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_command_passes_through() {
        assert_eq!(effective_command("cargo test"), Some("cargo test".into()));
    }

    #[test]
    fn cd_prefix_is_dropped() {
        assert_eq!(
            effective_command("cd /some/path && cargo test --workspace"),
            Some("cargo test --workspace".into())
        );
    }

    #[test]
    fn fd_redirections_do_not_split_the_segment() {
        assert_eq!(
            effective_command("cargo test 2>&1"),
            Some("cargo test 2>&1".into())
        );
        assert_eq!(
            effective_command("cargo build &> build.log"),
            Some("cargo build &> build.log".into())
        );
        assert_eq!(effective_command("echo hi >&2"), Some("echo hi >&2".into()));
        // A real background `&` still separates.
        assert_eq!(
            effective_command("sleep 5 & echo done"),
            Some("echo done".into())
        );
        // ... and `&&` still separates.
        assert_eq!(
            effective_command("cd x && cargo test 2>&1"),
            Some("cargo test 2>&1".into())
        );
    }

    #[test]
    fn pipe_tail_wins() {
        // The model already reduced the output itself; the last segment is
        // `tail`, which no filter matches -> passthrough.
        assert_eq!(
            effective_command("cargo test 2>&1 | tail -20"),
            Some("tail -20".into())
        );
    }

    #[test]
    fn env_assignments_are_skipped() {
        assert_eq!(
            effective_command("RUST_BACKTRACE=1 CARGO_TERM_COLOR=never cargo test"),
            Some("cargo test".into())
        );
    }

    #[test]
    fn wrappers_are_skipped() {
        assert_eq!(
            effective_command("sudo systemctl status nginx"),
            Some("systemctl status nginx".into())
        );
        assert_eq!(
            effective_command("env FOO=1 time cargo build"),
            Some("cargo build".into())
        );
    }

    #[test]
    fn wrapper_flag_values_yield_safe_nonmatch() {
        // `-u admin`: the flag's value is not consumed (no per-wrapper flag
        // tables). The leftover prefix matches no filter -> safe passthrough,
        // never a wrong match.
        assert_eq!(
            effective_command("sudo -u admin systemctl status nginx"),
            Some("admin systemctl status nginx".into())
        );
    }

    #[test]
    fn operators_inside_quotes_do_not_split() {
        assert_eq!(
            effective_command("echo \"a && cargo test\""),
            Some("echo \"a && cargo test\"".into())
        );
        assert_eq!(
            effective_command("grep 'foo|bar' file.txt"),
            Some("grep 'foo|bar' file.txt".into())
        );
    }

    #[test]
    fn operators_inside_substitution_do_not_split() {
        assert_eq!(
            effective_command("echo $(git status | wc -l)"),
            Some("echo $(git status | wc -l)".into())
        );
    }

    #[test]
    fn subshell_last_command_is_found() {
        assert_eq!(
            effective_command("(cd sub; cargo test)"),
            Some("cargo test".into())
        );
    }

    #[test]
    fn semicolon_sequence_takes_last() {
        assert_eq!(
            effective_command("git status; git log --oneline"),
            Some("git log --oneline".into())
        );
    }

    #[test]
    fn unbalanced_quote_refuses_to_guess() {
        assert_eq!(effective_command("echo 'unclosed"), None);
    }

    #[test]
    fn empty_and_operator_only_commands_yield_none() {
        assert_eq!(effective_command(""), None);
        assert_eq!(effective_command(" ; ; "), None);
        assert_eq!(effective_command("FOO=bar"), None);
    }
}
