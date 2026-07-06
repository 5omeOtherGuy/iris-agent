//! SHELL-panel-only structured command display.
//!
//! Splits a raw `bash` `command` string into prompt / continuation / heredoc
//! payload rows for the SHELL panel's command region, per the SHELL output
//! spec (continuation lines align under the command body, not under `$`; the
//! heredoc body renders as a labelled payload section).
//!
//! This is display-only and deliberately conservative. It is NOT a shell
//! parser: it recognizes top-level `&&`/`||`/`;`/`|` separators outside simple
//! quotes and a single common heredoc, and it UNDER-splits (falls back to plain
//! command rows) on anything ambiguous or unterminated. It never affects what
//! is sent to the model, nor the approval / text / denied summaries (which keep
//! using `tool_display::run_target`).

use crate::tool_display::shorten_paths_in_text;

/// A structured, display-only view of a shell invocation for the SHELL panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ShellCommand {
    /// Invocation segments up to (and including) any heredoc opener. The first
    /// entry is the `$` prompt row; the rest are `  …` continuation rows. Each
    /// continuation keeps its leading operator (`&& cargo fmt`).
    pub(super) command: Vec<String>,
    /// The heredoc payload, when a single terminated heredoc was recognized.
    pub(super) payload: Option<Payload>,
    /// Commands after the heredoc's closing delimiter, as continuation rows.
    pub(super) trailing: Vec<String>,
}

/// A recognized heredoc body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Payload {
    /// Inferred payload language label (`python`, `javascript`, `heredoc`, …).
    pub(super) lang: String,
    /// The heredoc body lines, verbatim.
    pub(super) body: Vec<String>,
    /// The closing delimiter token (e.g. `PY`).
    pub(super) closing: String,
}

/// Build the structured command display from a raw `bash` command string.
pub(super) fn build(command: &str) -> ShellCommand {
    if let Some(hd) = find_heredoc(command) {
        return ShellCommand {
            command: segment_rows(hd.prelude),
            payload: Some(Payload {
                lang: infer_language(&hd.opener_segment),
                body: hd.body,
                closing: hd.delimiter,
            }),
            trailing: segment_rows(&hd.trailing),
        };
    }
    ShellCommand {
        command: segment_rows(command),
        payload: None,
        trailing: Vec::new(),
    }
}

/// Flatten a (possibly multi-line) command string into display segments: each
/// non-blank line is split at top-level operators, operators stay attached to
/// the following segment, and every segment is trimmed.
fn segment_rows(text: &str) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows.extend(split_segments(line));
    }
    rows
}

/// Split one logical line at top-level `&&`, `||`, `;`, and `|` (not `||`),
/// keeping each operator attached to the segment that follows it. Operators
/// inside single or double quotes are ignored so `echo "a && b"` stays whole.
fn split_segments(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut single = false;
    let mut double = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            // A backslash outside single quotes escapes the next byte, so an
            // escaped quote (`\"`) does not toggle quote state.
            b'\\' if !single => {
                i += 2;
                continue;
            }
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            _ if single || double => {}
            b'&' if bytes.get(i + 1) == Some(&b'&') => {
                push_segment(&mut segments, &line[start..i]);
                start = i;
                i += 2;
                continue;
            }
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                push_segment(&mut segments, &line[start..i]);
                start = i;
                i += 2;
                continue;
            }
            b'|' => {
                push_segment(&mut segments, &line[start..i]);
                start = i;
            }
            b';' => {
                push_segment(&mut segments, &line[start..i]);
                start = i;
            }
            _ => {}
        }
        i += 1;
    }
    push_segment(&mut segments, &line[start..]);
    segments
}

fn push_segment(segments: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        segments.push(shorten_paths_in_text(trimmed));
    }
}

/// A recognized, terminated heredoc split out of the raw command.
struct HeredocSplit<'a> {
    /// Command text from the start through the end of the opener line.
    prelude: &'a str,
    /// The opener segment containing `<<DELIM` (drives language inference).
    opener_segment: String,
    /// The heredoc delimiter token.
    delimiter: String,
    /// Body lines between the opener and the closing delimiter.
    body: Vec<String>,
    /// Command text after the closing delimiter line.
    trailing: String,
}

/// Recognize a single, terminated heredoc. Returns `None` (caller falls back to
/// plain command rows) when there is no top-level `<<DELIM`, when it is a `<<<`
/// here-string, when the delimiter cannot be parsed, or when no closing
/// delimiter line is found.
fn find_heredoc(command: &str) -> Option<HeredocSplit<'_>> {
    let open = top_level_heredoc(command)?;
    let (delimiter, _) = parse_delimiter(&command[open + 2..])?;

    // The opener line is the line containing the `<<` operator.
    let line_start = command[..open].rfind('\n').map_or(0, |nl| nl + 1);
    let opener_line_end = command[open..]
        .find('\n')
        .map_or(command.len(), |rel| open + rel);
    let prelude = &command[..opener_line_end];
    let opener_segment_raw = &command[line_start..opener_line_end];
    // Language inference looks at the interpreter segment only (the one that
    // opened the heredoc), so a leading `cd "..."` cannot mislead it.
    let opener_segment = split_segments(opener_segment_raw)
        .into_iter()
        .next_back()
        .unwrap_or_else(|| opener_segment_raw.to_string());

    // Body runs until a line whose trimmed content equals the delimiter.
    let after = command.get(opener_line_end..).unwrap_or("");
    let after = after.strip_prefix('\n').unwrap_or(after);
    let mut body = Vec::new();
    let mut closed = false;
    let mut consumed = 0usize;
    for line in after.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        consumed += line.len();
        if content.trim() == delimiter {
            closed = true;
            break;
        }
        body.push(content.to_string());
    }
    if !closed {
        return None;
    }
    let trailing = after.get(consumed..).unwrap_or("").to_string();
    Some(HeredocSplit {
        prelude,
        opener_segment,
        delimiter,
        body,
        trailing,
    })
}

/// Byte index of a top-level heredoc `<<` (outside quotes, not a `<<<`
/// here-string), if any.
fn top_level_heredoc(command: &str) -> Option<usize> {
    let bytes = command.as_bytes();
    let mut i = 0usize;
    let mut single = false;
    let mut double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !single => {
                i += 2;
                continue;
            }
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            _ if single || double => {}
            b'<' if bytes.get(i + 1) == Some(&b'<') => {
                if bytes.get(i + 2) == Some(&b'<') {
                    // `<<<` here-string: skip, not a heredoc.
                    i += 3;
                    continue;
                }
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Parse the heredoc delimiter immediately after `<<`: an optional `-`, optional
/// whitespace, then a quoted (`'PY'` / `"PY"`) or bare (`EOF`) word token.
/// Returns the delimiter and the byte length consumed.
fn parse_delimiter(rest: &str) -> Option<(String, usize)> {
    let mut chars = rest.char_indices().peekable();
    let mut consumed = 0usize;
    // Optional `<<-`.
    if let Some(&(_, '-')) = chars.peek() {
        chars.next();
        consumed += 1;
    }
    // Optional whitespace.
    while let Some(&(idx, c)) = chars.peek() {
        if c == ' ' || c == '\t' {
            chars.next();
            consumed = idx + c.len_utf8();
        } else {
            break;
        }
    }
    let quote = matches!(rest[consumed..].chars().next(), Some('\'' | '"'));
    let word: String = if quote {
        let q = rest[consumed..].chars().next()?;
        rest[consumed + q.len_utf8()..]
            .chars()
            .take_while(|&c| c != q)
            .collect()
    } else {
        rest[consumed..]
            .chars()
            .take_while(|&c| c.is_ascii_alphanumeric() || c == '_')
            .collect()
    };
    if word.is_empty() {
        return None;
    }
    Some((word, consumed))
}

/// Infer the payload language from the heredoc opener command. Conservative:
/// only the interpreter command is inspected; unknown shapes get `heredoc`.
fn infer_language(opener_segment: &str) -> String {
    let s = opener_segment;
    let lang = if s.contains("python3") || s.contains("python") {
        "python"
    } else if s.contains("node") {
        "javascript"
    } else if s.contains("ruby") {
        "ruby"
    } else if s.contains("cat ") || s.starts_with("cat") {
        "text"
    } else {
        "heredoc"
    };
    lang.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_command_is_one_prompt_row() {
        let cmd = build("git status --short");
        assert_eq!(cmd.command, vec!["git status --short"]);
        assert!(cmd.payload.is_none());
        assert!(cmd.trailing.is_empty());
    }

    #[test]
    fn and_operator_splits_into_prompt_and_continuation() {
        let cmd = build("cd \"/abs/path\" && cargo fmt");
        assert_eq!(cmd.command, vec!["cd \"/abs/path\"", "&& cargo fmt"]);
        assert!(cmd.payload.is_none());
    }

    #[test]
    fn command_rows_shorten_absolute_home_paths() {
        let cwd = std::env::current_dir().unwrap();
        let sibling = cwd.parent().unwrap().join("iris-other");
        let cmd = build(&format!("cd {} && cargo test", sibling.display()));
        assert_eq!(cmd.command, vec!["cd ../iris-other", "&& cargo test"]);
        assert!(!cmd.command.join(" ").contains("/home/"));
    }

    #[test]
    fn operator_inside_quotes_does_not_split() {
        let cmd = build("echo \"a && b\" && echo done");
        assert_eq!(cmd.command, vec!["echo \"a && b\"", "&& echo done"]);
    }

    #[test]
    fn pipe_and_semicolon_split_but_double_pipe_stays_one_operator() {
        assert_eq!(build("a | b").command, vec!["a", "| b"]);
        assert_eq!(build("a ; b").command, vec!["a", "; b"]);
        assert_eq!(build("a || b").command, vec!["a", "|| b"]);
    }

    #[test]
    fn multiline_without_heredoc_flattens_to_continuations() {
        let cmd = build("set -e\nnpm install");
        assert_eq!(cmd.command, vec!["set -e", "npm install"]);
    }

    #[test]
    fn heredoc_python_splits_prelude_payload_and_trailing() {
        let raw = "cd \"/abs\" && python3 - <<'PY'\nfrom pathlib import Path\np = Path('x')\nPY\ncargo fmt";
        let cmd = build(raw);
        assert_eq!(cmd.command, vec!["cd \"/abs\"", "&& python3 - <<'PY'"]);
        let payload = cmd.payload.expect("payload");
        assert_eq!(payload.lang, "python");
        assert_eq!(payload.closing, "PY");
        assert_eq!(
            payload.body,
            vec!["from pathlib import Path", "p = Path('x')"]
        );
        assert_eq!(cmd.trailing, vec!["cargo fmt"]);
    }

    #[test]
    fn heredoc_language_inference_covers_known_interpreters() {
        assert_eq!(
            build("node - <<'JS'\nx\nJS").payload.unwrap().lang,
            "javascript"
        );
        assert_eq!(build("ruby - <<'RB'\nx\nRB").payload.unwrap().lang, "ruby");
        assert_eq!(
            build("cat > file <<'EOF'\nx\nEOF").payload.unwrap().lang,
            "text"
        );
        assert_eq!(
            build("frobnicate <<'ZZ'\nx\nZZ").payload.unwrap().lang,
            "heredoc"
        );
    }

    #[test]
    fn bare_and_dash_delimiters_are_recognized() {
        assert_eq!(build("cat <<EOF\nx\nEOF").payload.unwrap().closing, "EOF");
        let dash = build("cat <<-END\n\tx\nEND");
        assert_eq!(dash.payload.unwrap().closing, "END");
    }

    #[test]
    fn unterminated_heredoc_falls_back_to_plain_rows() {
        let cmd = build("python3 - <<'PY'\nprint(1)");
        assert!(cmd.payload.is_none());
        // Under-split: the whole thing renders as plain command rows.
        assert_eq!(
            cmd.command.first().map(String::as_str),
            Some("python3 - <<'PY'")
        );
    }

    #[test]
    fn escaped_quote_keeps_string_open_so_operator_inside_does_not_split() {
        // The `\"` is escaped, so the double quote stays open across it and the
        // `&&` is still inside the string.
        let cmd = build("echo \"a \\\" b && c\"");
        assert_eq!(cmd.command, vec!["echo \"a \\\" b && c\""]);
    }

    #[test]
    fn escaped_quote_then_real_operator_splits_after_the_closed_string() {
        let cmd = build("echo \"a \\\" b\" && c");
        assert_eq!(cmd.command, vec!["echo \"a \\\" b\"", "&& c"]);
    }

    #[test]
    fn here_string_is_not_treated_as_heredoc() {
        let cmd = build("grep foo <<< \"bar\"");
        assert!(cmd.payload.is_none());
    }

    #[test]
    fn heredoc_delimiter_inside_quotes_is_ignored() {
        // The `<<` is inside a double-quoted string, so no heredoc is detected.
        let cmd = build("echo \"a << b\"");
        assert!(cmd.payload.is_none());
        assert_eq!(cmd.command, vec!["echo \"a << b\""]);
    }
}
