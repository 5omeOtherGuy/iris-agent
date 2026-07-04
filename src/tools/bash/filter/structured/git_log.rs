//! Structured filter for `git log` (default long format).
//!
//! One compact line per commit -- short hash, ISO date when parseable,
//! subject, decorations, merge marker -- plus a trailing `N commits` summary.
//! Author and date appear per line only when they vary across the log;
//! a single author or single day is stated once in the summary. Bodies and
//! trailers are dropped (the subject is the signal).
//!
//! Strict grammar: anything that is not `commit`/header/body-indent lines
//! (e.g. `git log -p` patches, `--stat` tables, `--oneline`) declines to raw.

use std::sync::OnceLock;

use regex::Regex;

use super::super::engine::strip_ansi;

struct Commit {
    hash: String,
    decorations: Option<String>,
    author: Option<String>,
    date: Option<String>,
    merge: bool,
    subject: Option<String>,
}

fn commit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^commit ([0-9a-f]{7,40})(?: \((.+)\))?$").expect("static regex"))
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z-]*:\s").expect("static regex"))
}

/// `Sat Jul 4 16:39:13 2026 +0200` or an ISO `2026-07-04 ...` date -> ISO day.
fn compact_date(date: &str) -> Option<String> {
    let tokens: Vec<&str> = date.split_whitespace().collect();
    if let Some(first) = tokens.first()
        && first.len() == 10
        && first.chars().filter(|c| *c == '-').count() == 2
    {
        return Some((*first).to_string());
    }
    // [weekday] [month] [day] [time] [year] [tz]
    if tokens.len() < 5 {
        return None;
    }
    let month = match tokens[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u32 = tokens[2].parse().ok()?;
    let year: u32 = tokens[4].parse().ok()?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// `Name <email>` -> `Name`.
fn author_name(raw: &str) -> String {
    match raw.split_once('<') {
        Some((name, _)) if !name.trim().is_empty() => name.trim().to_string(),
        _ => raw.trim().to_string(),
    }
}

pub(super) fn apply(output: &str, _exit_ok: bool) -> Option<String> {
    let text = strip_ansi(output);
    let mut commits: Vec<Commit> = Vec::new();
    for line in text.lines() {
        if let Some(c) = commit_re().captures(line) {
            commits.push(Commit {
                hash: c[1].chars().take(8).collect(),
                decorations: c.get(2).map(|d| d.as_str().to_string()),
                author: None,
                date: None,
                merge: false,
                subject: None,
            });
            continue;
        }
        let Some(current) = commits.last_mut() else {
            if line.trim().is_empty() {
                continue; // leading blank
            }
            return None; // does not start with a commit line
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("    ") {
            // Body line: the first one is the subject.
            if current.subject.is_none() {
                current.subject = Some(rest.trim_end().to_string());
            }
            continue;
        }
        if let Some(author) = line.strip_prefix("Author: ") {
            current.author = Some(author_name(author));
        } else if let Some(date) = line.strip_prefix("Date:") {
            current.date = compact_date(date.trim());
        } else if line.starts_with("Merge: ") {
            current.merge = true;
        } else if header_re().is_match(line) {
            // Other headers (Commit:, AuthorDate:, Reflog:, ...): ignored.
        } else {
            // Patch text, stat tables, or anything unexpected: decline.
            return None;
        }
    }
    if commits.is_empty() {
        return None;
    }

    let authors: std::collections::BTreeSet<&str> =
        commits.iter().filter_map(|c| c.author.as_deref()).collect();
    let multi_author = authors.len() > 1;
    let dates: std::collections::BTreeSet<&str> =
        commits.iter().filter_map(|c| c.date.as_deref()).collect();
    let multi_date = dates.len() > 1;

    let mut out: Vec<String> = Vec::with_capacity(commits.len() + 1);
    for c in &commits {
        let mut line = c.hash.clone();
        if multi_date && let Some(d) = &c.date {
            line.push(' ');
            line.push_str(d);
        }
        line.push(' ');
        line.push_str(c.subject.as_deref().unwrap_or("(no subject)"));
        if c.merge {
            line.push_str(" (merge)");
        }
        if let Some(deco) = &c.decorations {
            line.push_str(&format!(" ({deco})"));
        }
        if multi_author && let Some(a) = &c.author {
            line.push_str(&format!(" [{a}]"));
        }
        out.push(line);
    }
    let mut summary = format!(
        "{} commit{}",
        commits.len(),
        if commits.len() == 1 { "" } else { "s" }
    );
    if authors.len() == 1 {
        summary.push_str(&format!(" by {}", authors.first().expect("one author")));
    } else if multi_author {
        summary.push_str(&format!(" ({} authors)", authors.len()));
    }
    if dates.len() == 1 {
        summary.push_str(&format!(" on {}", dates.first().expect("one date")));
    }
    out.push(summary);
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
commit c8e2dbba9106ea208a97f5aad6789817b7b646b4 (HEAD -> main, origin/main)
Author: alice <alice@example.com>
Date:   Sat Jul 4 16:39:13 2026 +0200

    docs(adr): record the decision (#342)

commit 3081fc1b623adb5e87c66810753c454aea57cc34
Author: alice <alice@example.com>
Date:   Fri Jul 3 09:01:02 2026 +0200

    feat(tui): syntax-highlighted code blocks

    A longer body line that should be dropped.

    Co-authored-by: bob <bob@example.com>
";

    #[test]
    fn default_log_compacts_to_one_line_per_commit() {
        let out = apply(LOG, true).expect("parses");
        assert_eq!(
            out,
            "c8e2dbba 2026-07-04 docs(adr): record the decision (#342) (HEAD -> main, origin/main)\n\
             3081fc1b 2026-07-03 feat(tui): syntax-highlighted code blocks\n\
             2 commits by alice"
        );
    }

    #[test]
    fn single_day_log_states_date_once_in_summary() {
        let log = LOG.replace("Fri Jul 3 09:01:02 2026", "Sat Jul 4 10:00:00 2026");
        let out = apply(&log, true).expect("parses");
        assert!(
            out.contains("3081fc1b feat(tui): syntax-highlighted code blocks"),
            "per-line date must be dropped when all commits share a day: {out}"
        );
        assert!(out.ends_with("2 commits by alice on 2026-07-04"), "{out}");
    }

    #[test]
    fn multi_author_logs_carry_author_per_line() {
        let log = LOG.replace(
            "commit 3081fc1b623adb5e87c66810753c454aea57cc34\nAuthor: alice",
            "commit 3081fc1b623adb5e87c66810753c454aea57cc34\nAuthor: bob",
        );
        let out = apply(&log, true).expect("parses");
        assert!(out.contains("[alice]"), "{out}");
        assert!(out.contains("[bob]"), "{out}");
        assert!(out.contains("2 commits (2 authors)"), "{out}");
    }

    #[test]
    fn merge_commits_are_marked() {
        let log = "\
commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
Merge: 1111111 2222222
Author: alice <a@x>
Date:   Sat Jul 4 10:00:00 2026 +0200

    Merge branch 'feature'
";
        let out = apply(log, true).expect("parses");
        assert!(out.contains("Merge branch 'feature' (merge)"), "{out}");
    }

    #[test]
    fn patch_and_stat_output_declines() {
        let with_patch = format!("{LOG}\ndiff --git a/x b/x\n+++ b/x\n");
        assert_eq!(apply(&with_patch, true), None);
        let with_stat = format!("{LOG}\n src/x.rs | 5 +++--\n");
        assert_eq!(apply(&with_stat, true), None);
    }

    #[test]
    fn oneline_and_garbage_decline() {
        assert_eq!(
            apply("c8e2dbba docs: something\n3081fc1b feat: x\n", true),
            None
        );
        assert_eq!(
            apply("fatal: your current branch has no commits yet\n", false),
            None
        );
        assert_eq!(apply("random text", true), None);
    }
}
