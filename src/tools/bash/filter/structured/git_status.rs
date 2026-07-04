//! Structured filter for `git status` (long format).
//!
//! Keeps the branch, a compacted ahead/behind/diverged tracking note, section
//! headers, and per-file state; drops advice hints, blank lines, and the
//! "no changes added to commit" trailer. Porcelain/short output (or anything
//! that does not start like long-format status) declines to raw.

use std::sync::OnceLock;

use regex::Regex;

use super::super::engine::strip_ansi;

fn tracking_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^Your branch is (up to date with|ahead of|behind) '([^']+)'",
            r"(?: by (\d+) commits?)?",
        ))
        .expect("static regex")
    })
}

fn diverged_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^Your branch and '([^']+)' have diverged").expect("static regex")
    })
}

fn diverged_counts_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^and have (\d+) and (\d+) different commits? each").expect("static regex")
    })
}

/// Advice/hint lines and trailers that carry no state.
fn is_hint(line: &str) -> bool {
    let t = line.trim_start();
    (t.starts_with('(') && t.ends_with(')'))
        || line.starts_with("no changes added to commit")
        || line.starts_with("nothing added to commit but untracked files")
}

/// Entry states in long-format `git status` sections. Only these are
/// re-spaced; anything else after the indent is kept as-is (a filename
/// containing `:` must never be split).
const ENTRY_STATES: &[&str] = &[
    "modified",
    "new file",
    "deleted",
    "renamed",
    "copied",
    "typechange",
    "unmerged",
    "both modified",
    "both added",
    "both deleted",
    "added by us",
    "added by them",
    "deleted by us",
    "deleted by them",
];

/// Per-file entry: git indents entries with a tab (or spaces) inside a
/// section; normalize to two spaces and a single space after the state.
fn compact_entry(line: &str) -> Option<String> {
    let rest = line.strip_prefix('\t').or_else(|| {
        line.strip_prefix("        ")
            .or_else(|| line.strip_prefix("    "))
    })?;
    match rest.split_once(':') {
        Some((state, path)) if ENTRY_STATES.contains(&state.trim()) && !path.trim().is_empty() => {
            Some(format!("  {}: {}", state.trim(), path.trim()))
        }
        _ => Some(format!("  {}", rest.trim())),
    }
}

pub(super) fn apply(output: &str, _exit_ok: bool) -> Option<String> {
    let text = strip_ansi(output);
    let mut lines = text.lines().peekable();
    let first = loop {
        match lines.peek() {
            Some(l) if l.trim().is_empty() => {
                lines.next();
            }
            Some(l) => break *l,
            None => return None,
        }
    };
    // Long-format status always opens with one of these; anything else
    // (porcelain, short format, errors) declines.
    if !(first.starts_with("On branch ")
        || first.starts_with("HEAD detached ")
        || first.starts_with("Not currently on any branch")
        || first.starts_with("interactive rebase in progress")
        || first.starts_with("rebase in progress"))
    {
        return None;
    }

    let mut out: Vec<String> = Vec::new();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        // Entries before hints: a tab-indented untracked file named e.g.
        // `(notes)` is state, not advice.
        if let Some(entry) = compact_entry(line) {
            out.push(entry);
            continue;
        }
        if is_hint(line) {
            continue;
        }
        if let Some(c) = tracking_re().captures(line) {
            // Fold the tracking note into the branch line.
            let note = match (&c[1], c.get(3)) {
                ("up to date with", _) => format!("up to date with '{}'", &c[2]),
                (rel, Some(n)) => format!("{rel} '{}' by {}", &c[2], n.as_str()),
                (rel, None) => format!("{rel} '{}'", &c[2]),
            };
            match out.last_mut() {
                Some(branch) => {
                    branch.push_str(&format!(" ({note})"));
                }
                None => out.push(line.to_string()),
            }
            continue;
        }
        if diverged_re().is_match(line) {
            // Two- or three-line diverged note; compact when the second line
            // parses, otherwise keep the lines verbatim.
            let upstream = diverged_re().captures(line).map(|c| c[1].to_string());
            if let (Some(upstream), Some(next)) = (upstream, lines.peek())
                && let Some(c) = diverged_counts_re().captures(next)
            {
                let note = format!(
                    "diverged from '{}': {} local, {} remote",
                    upstream, &c[1], &c[2]
                );
                lines.next();
                if lines.peek().is_some_and(|l| l.trim() == "respectively.") {
                    lines.next();
                }
                match out.last_mut() {
                    Some(branch) => branch.push_str(&format!(" ({note})")),
                    None => out.push(note),
                }
                continue;
            }
            out.push(line.to_string());
            continue;
        }
        // Section headers, in-progress state, "nothing to commit", and any
        // unrecognized line: keep verbatim.
        out.push(line.to_string());
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPICAL: &str = "\
On branch feat/thing
Your branch is ahead of 'origin/main' by 2 commits.
  (use \"git push\" to publish your local commits)

Changes to be committed:
  (use \"git restore --staged <file>...\" to unstage)
\tmodified:   src/lib.rs
\tnew file:   src/new.rs

Changes not staged for commit:
  (use \"git add <file>...\" to update what will be committed)
  (use \"git restore <file>...\" to discard changes in working directory)
\tmodified:   src/main.rs

Untracked files:
  (use \"git add <file>...\" to include in what will be committed)
\tnotes.txt

no changes added to commit (use \"git add\" and/or \"git commit -a\")
";

    #[test]
    fn typical_status_compacts() {
        let out = apply(TYPICAL, true).expect("parses");
        assert_eq!(
            out,
            "On branch feat/thing (ahead of 'origin/main' by 2)\n\
             Changes to be committed:\n\
             \x20 modified: src/lib.rs\n\
             \x20 new file: src/new.rs\n\
             Changes not staged for commit:\n\
             \x20 modified: src/main.rs\n\
             Untracked files:\n\
             \x20 notes.txt"
        );
    }

    #[test]
    fn clean_tree_keeps_state() {
        let raw = "\
On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean
";
        let out = apply(raw, true).expect("parses");
        assert_eq!(
            out,
            "On branch main (up to date with 'origin/main')\n\
             nothing to commit, working tree clean"
        );
    }

    #[test]
    fn diverged_note_compacts() {
        let raw = "\
On branch main
Your branch and 'origin/main' have diverged,
and have 2 and 3 different commits each, respectively.
  (use \"git pull\" if you want to integrate the remote branch with yours)

nothing to commit, working tree clean
";
        let out = apply(raw, true).expect("parses");
        assert!(
            out.starts_with("On branch main (diverged from 'origin/main': 2 local, 3 remote)"),
            "{out}"
        );
    }

    #[test]
    fn unmerged_conflict_state_survives() {
        let raw = "\
On branch main
You have unmerged paths.
  (fix conflicts and run \"git commit\")

Unmerged paths:
  (use \"git add <file>...\" to mark resolution)
\tboth modified:   src/conflict.rs
";
        let out = apply(raw, false).expect("parses");
        assert!(out.contains("You have unmerged paths."), "{out}");
        assert!(out.contains("  both modified: src/conflict.rs"), "{out}");
        assert!(!out.contains("fix conflicts and run"), "{out}");
    }

    #[test]
    fn parenthesized_untracked_filename_is_not_a_hint() {
        let raw = "\
On branch main

Untracked files:
  (use \"git add <file>...\" to include in what will be committed)
\t(notes)

nothing added to commit but untracked files present (use \"git add\" to track)
";
        let out = apply(raw, true).expect("parses");
        assert!(out.contains("  (notes)"), "{out}");
        assert!(!out.contains("use \"git add"), "{out}");
    }

    #[test]
    fn porcelain_and_garbage_decline() {
        assert_eq!(apply(" M src/lib.rs\n?? notes.txt\n", true), None);
        assert_eq!(apply("fatal: not a git repository\n", false), None);
        assert_eq!(apply("random text\n", true), None);
    }
}
