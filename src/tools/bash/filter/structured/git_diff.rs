//! Structured filter for `git diff`.
//!
//! Emits a per-file `+added/-removed` stat summary, keeps every source-file
//! section verbatim (hunks are the signal), and elides only machine-generated
//! lockfile churn (`Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`) down
//! to its stat line. A result that would not be smaller than the raw diff
//! declines, so a pure-source diff never pays for the added header.

use super::super::engine::strip_ansi;

/// Lockfiles whose hunks are machine-generated churn (scope of #336 PR 2).
const LOCKFILES: &[&str] = &["Cargo.lock", "package-lock.json", "pnpm-lock.yaml"];

struct FileSection {
    /// Line range in the raw output (header line included).
    start: usize,
    end: usize,
    name: String,
    added: usize,
    removed: usize,
    binary: bool,
}

impl FileSection {
    fn is_lockfile(&self) -> bool {
        let base = self.name.rsplit('/').next().unwrap_or(&self.name);
        LOCKFILES.contains(&base)
    }
}

/// File name from a `diff --git a/... b/...` header (post-image side).
fn file_name(header: &str) -> Option<String> {
    let after = header.rsplit_once(" b/")?.1.trim();
    let name = after.trim_end_matches('"');
    (!name.is_empty()).then(|| name.to_string())
}

pub(super) fn apply(output: &str, _exit_ok: bool) -> Option<String> {
    let text = strip_ansi(output);
    let lines: Vec<&str> = text.lines().collect();
    let first_content = lines.iter().position(|l| !l.trim().is_empty())?;
    if !lines[first_content].starts_with("diff --git ") {
        return None; // not unified diff output (--stat, fatal:, ...)
    }

    let mut sections: Vec<FileSection> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            if let Some(prev) = sections.last_mut() {
                prev.end = i;
            }
            sections.push(FileSection {
                start: i,
                end: lines.len(),
                name: file_name(line).unwrap_or_else(|| "(unknown)".to_string()),
                added: 0,
                removed: 0,
                binary: false,
            });
        } else if let Some(current) = sections.last_mut() {
            if line.starts_with('+') && !line.starts_with("+++") {
                current.added += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                current.removed += 1;
            } else if line.starts_with("Binary files ") {
                current.binary = true;
            }
        }
    }
    if sections.is_empty() {
        return None;
    }

    let total_added: usize = sections.iter().map(|s| s.added).sum();
    let total_removed: usize = sections.iter().map(|s| s.removed).sum();
    let mut out: Vec<String> = Vec::new();
    out.push(format!(
        "{} file{} changed, +{total_added}/-{total_removed}",
        sections.len(),
        if sections.len() == 1 { "" } else { "s" },
    ));
    for s in &sections {
        let mut stat = format!("  {} +{}/-{}", s.name, s.added, s.removed);
        if s.binary {
            stat.push_str(" (binary)");
        } else if s.is_lockfile() {
            stat.push_str(" (lockfile, hunks omitted)");
        }
        out.push(stat);
    }
    for s in &sections {
        if s.is_lockfile() && !s.binary {
            continue;
        }
        out.push(String::new());
        out.extend(lines[s.start..s.end].iter().map(|l| (*l).to_string()));
    }
    let result = out.join("\n");
    // Never-worse guard: the stat header must pay for itself.
    if result.len() >= text.trim_end_matches('\n').len() {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn keep() {}
-fn old() {}
+fn new_one() {}
+fn extra() {}
diff --git a/Cargo.lock b/Cargo.lock
index 3333333..4444444 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,20 +10,60 @@
 [[package]]
-name = \"old-dep\"
-version = \"1.0.0\"
-source = \"registry+https://github.com/rust-lang/crates.io-index\"
-checksum = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"
+name = \"new-dep\"
+version = \"2.0.0\"
+source = \"registry+https://github.com/rust-lang/crates.io-index\"
+checksum = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"
+dependencies = [
+ \"serde\",
+]
";

    #[test]
    fn lockfile_hunks_reduce_to_stat_line_source_hunks_survive() {
        let out = apply(MIXED, true).expect("parses");
        assert!(out.contains("2 files changed, +9/-5"), "{out}");
        assert!(
            out.contains("  Cargo.lock +7/-4 (lockfile, hunks omitted)"),
            "{out}"
        );
        assert!(out.contains("  src/lib.rs +2/-1"), "{out}");
        // Source hunks verbatim.
        assert!(out.contains("@@ -1,3 +1,4 @@"), "{out}");
        assert!(out.contains("-fn old() {}"), "{out}");
        assert!(out.contains("+fn new_one() {}"), "{out}");
        // Lockfile churn gone.
        assert!(!out.contains("checksum"), "{out}");
        assert!(!out.contains("new-dep"), "{out}");
    }

    #[test]
    fn pure_source_diff_declines_rather_than_grow() {
        let source_only: String = MIXED.lines().take(9).collect::<Vec<_>>().join("\n");
        assert_eq!(apply(&source_only, true), None);
    }

    #[test]
    fn binary_sections_are_kept_and_marked() {
        let diff = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ
diff --git a/pnpm-lock.yaml b/pnpm-lock.yaml
index 3333333..4444444 100644
--- a/pnpm-lock.yaml
+++ b/pnpm-lock.yaml
@@ -1,4 +1,9 @@
+lockfileVersion: '9.0'
+settings:
+  autoInstallPeers: true
+  excludeLinksFromLockfile: false
+packages:
-old: content
";
        let out = apply(diff, true).expect("parses");
        assert!(out.contains("  img.png +0/-0 (binary)"), "{out}");
        assert!(
            out.contains("Binary files a/img.png and b/img.png differ"),
            "{out}"
        );
        assert!(
            out.contains("  pnpm-lock.yaml +5/-1 (lockfile, hunks omitted)"),
            "{out}"
        );
        assert!(!out.contains("autoInstallPeers"), "{out}");
    }

    #[test]
    fn stat_output_and_garbage_decline() {
        assert_eq!(
            apply(
                " src/lib.rs | 4 +++-\n 1 file changed, 3 insertions(+)\n",
                true
            ),
            None
        );
        assert_eq!(apply("fatal: ambiguous argument 'nope'\n", false), None);
        assert_eq!(apply("random text", true), None);
    }
}
