//! Opt-in skim filter for `read` (issue #337, ADR-0036): language-aware
//! stripping of comments, docstrings, and blank lines for exploration reads.
//!
//! The filter is a per-line keep/strip mask over the *original* file lines,
//! never a rewrite: `read` renders the kept lines with their true line
//! numbers, so offsets and follow-up full reads stay coherent. Design ported
//! from RTK's `MinimalFilter` (rtk-ai/rtk, Apache-2.0) with two deliberate
//! deviations: doc comments and docstrings are stripped too (skim is for
//! exploration, not API reading), and a line is only treated as a comment
//! when the *trimmed line starts with* the marker — mid-line markers never
//! strip code, so every code line survives verbatim.
//!
//! Data formats and unknown extensions map to no rules ([`skim_mask`] returns
//! `None`) and are never stripped. Edit-safety (a skim read not satisfying
//! read-before-mutate) is enforced by `read`, not here.

/// Whole-line comment syntax for one language family. Data, not a framework:
/// extension -> rules, resolved once per read.
struct Rules {
    /// A line whose trimmed text starts with one of these is a comment line.
    /// Doc comments (`///`, `//!`, `#!` in Ruby) share the prefix and are
    /// stripped too; shebang lines are exempted in [`skim_mask`].
    line: &'static [&'static str],
    /// Block comment delimiters, entered only when the trimmed line *starts*
    /// with the opener.
    block: Option<(&'static str, &'static str)>,
    /// Python-style triple-quoted docstrings (`"""` / `'''`).
    docstrings: bool,
}

const C_STYLE: Rules = Rules {
    line: &["//"],
    block: Some(("/*", "*/")),
    docstrings: false,
};
const PYTHON: Rules = Rules {
    line: &["#"],
    block: None,
    docstrings: true,
};
const RUBY: Rules = Rules {
    line: &["#"],
    block: Some(("=begin", "=end")),
    docstrings: false,
};
const HASH: Rules = Rules {
    line: &["#"],
    block: None,
    docstrings: false,
};

/// Comment syntax by file extension (lowercased). `None` means "never strip":
/// data formats (JSON/YAML/TOML/XML/CSV), prose, and unknown extensions all
/// pass through untouched — a comment-shaped line in data is data.
fn rules(extension: &str) -> Option<&'static Rules> {
    match extension.to_ascii_lowercase().as_str() {
        "rs" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "go" | "c" | "h" | "cpp" | "cc"
        | "cxx" | "hpp" | "hh" | "java" => Some(&C_STYLE),
        "py" | "pyw" => Some(&PYTHON),
        "rb" => Some(&RUBY),
        "sh" | "bash" | "zsh" => Some(&HASH),
        _ => None,
    }
}

enum State {
    Code,
    Block(&'static str),
    Docstring(&'static str),
}

/// Which lines of `content` survive a skim read, indexed by original line
/// number (0-based, split on `\n` like the renderer). `None` when the
/// extension has no comment syntax to strip (data formats, unknown files).
///
/// Safety-first biases: when a block comment or docstring closes with code
/// after the delimiter on the same line, the whole line is kept; and a
/// triple-quote only opens a docstring at module start or right after a line
/// ending in `:` (a `def`/`class` header), so multi-line string *literals*
/// and the code around them are never swallowed.
pub(super) fn skim_mask(content: &str, extension: Option<&str>) -> Option<Vec<bool>> {
    let rules = rules(extension?)?;
    let mut state = State::Code;
    // Whether the last kept code line ends with `:` (None before any kept
    // line). Gates docstring detection to docstring positions.
    let mut prev_ends_colon: Option<bool> = None;
    let mut mask = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        let keep = match state {
            State::Block(end) => match trimmed.find(end) {
                Some(pos) => {
                    state = State::Code;
                    // Code after the closing delimiter: keep the line.
                    !trimmed[pos + end.len()..].trim().is_empty()
                }
                None => false,
            },
            State::Docstring(delim) => match trimmed.find(delim) {
                Some(pos) => {
                    state = State::Code;
                    !trimmed[pos + delim.len()..].trim().is_empty()
                }
                None => false,
            },
            State::Code => 'code: {
                if trimmed.is_empty() {
                    break 'code false;
                }
                // Shebangs carry meaning; never strip line 1's `#!`.
                if idx == 0 && trimmed.starts_with("#!") {
                    break 'code true;
                }
                if let Some((start, end)) = rules.block
                    && let Some(rest) = trimmed.strip_prefix(start)
                {
                    break 'code match rest.find(end) {
                        // Closes on the same line: keep only if code
                        // follows the closing delimiter.
                        Some(pos) => !rest[pos + end.len()..].trim().is_empty(),
                        None => {
                            state = State::Block(end);
                            false
                        }
                    };
                }
                if rules.docstrings
                    && prev_ends_colon.is_none_or(|colon| colon)
                    && let Some((delim, rest)) = ["\"\"\"", "'''"]
                        .iter()
                        .find_map(|d| trimmed.strip_prefix(d).map(|rest| (*d, rest)))
                {
                    break 'code match rest.find(delim) {
                        Some(pos) => !rest[pos + delim.len()..].trim().is_empty(),
                        None => {
                            state = State::Docstring(delim);
                            false
                        }
                    };
                }
                !rules.line.iter().any(|marker| trimmed.starts_with(marker))
            }
        };
        if keep {
            prev_ends_colon = Some(trimmed.ends_with(':'));
        }
        mask.push(keep);
    }
    Some(mask)
}

#[cfg(test)]
mod corpus;

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(content: &str, ext: &str) -> Vec<usize> {
        skim_mask(content, Some(ext))
            .expect("skimmable extension")
            .iter()
            .enumerate()
            .filter_map(|(i, keep)| keep.then_some(i + 1))
            .collect()
    }

    #[test]
    fn rust_strips_comments_doc_comments_and_blanks() {
        let src = "//! module doc\n\n/// doc comment\nfn main() {\n    // inline note\n    println!(\"hi\"); // trailing comment kept with its code\n}\n";
        assert_eq!(kept(src, "rs"), vec![4, 6, 7]);
    }

    #[test]
    fn rust_block_comment_spanning_lines_is_stripped() {
        let src = "/* start\n   middle\n   end */\nfn f() {}\n";
        assert_eq!(kept(src, "rs"), vec![4]);
    }

    #[test]
    fn block_comment_closing_line_with_code_is_kept() {
        let src = "/* comment\n*/ let x = 1;\n";
        assert_eq!(kept(src, "rs"), vec![2]);
    }

    #[test]
    fn mid_line_block_marker_never_strips_code() {
        let src = "let glob = \"packages/*\"; /* trailing */\nlet y = 2;\n";
        assert_eq!(kept(src, "rs"), vec![1, 2]);
    }

    #[test]
    fn python_strips_hash_comments_and_docstrings() {
        let src = "# comment\ndef f():\n    \"\"\"Docstring.\n\n    More doc.\n    \"\"\"\n    return 1\n";
        assert_eq!(kept(src, "py"), vec![2, 7]);
    }

    #[test]
    fn python_single_line_docstring_is_stripped() {
        let src = "def f():\n    '''one-liner'''\n    return 1\n";
        assert_eq!(kept(src, "py"), vec![1, 3]);
    }

    #[test]
    fn python_string_literal_is_not_a_docstring() {
        // A triple-quoted *literal* (assignment, not a docstring position):
        // nothing may be swallowed, including the bare closing quote line.
        let src = "x = \"\"\"not a docstring\nstill string\n\"\"\"\ny = 1\n";
        assert_eq!(kept(src, "py"), vec![1, 2, 3, 4]);
    }

    #[test]
    fn python_module_docstring_is_stripped() {
        let src = "\"\"\"Module doc.\n\nMore.\n\"\"\"\nimport os\n";
        assert_eq!(kept(src, "py"), vec![5]);
    }

    #[test]
    fn shell_keeps_shebang_strips_comments() {
        let src = "#!/bin/sh\n# setup\necho hi\n";
        assert_eq!(kept(src, "sh"), vec![1, 3]);
    }

    #[test]
    fn ruby_strips_begin_end_blocks() {
        let src = "=begin\nblock doc\n=end\nputs 1\n";
        assert_eq!(kept(src, "rb"), vec![4]);
    }

    #[test]
    fn typescript_jsdoc_is_stripped() {
        let src = "/** JSDoc\n * @param x\n */\nexport function f(x: number) {}\n";
        assert_eq!(kept(src, "ts"), vec![4]);
    }

    #[test]
    fn data_formats_and_unknown_extensions_are_never_stripped() {
        for ext in [
            "json", "yaml", "yml", "toml", "xml", "csv", "md", "txt", "lock", "weird",
        ] {
            assert!(
                skim_mask("// looks like a comment\n", Some(ext)).is_none(),
                "{ext} must not be skimmed"
            );
        }
        assert!(skim_mask("x\n", None).is_none(), "no extension: no skim");
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert!(skim_mask("// c\n", Some("RS")).is_some());
    }
}
