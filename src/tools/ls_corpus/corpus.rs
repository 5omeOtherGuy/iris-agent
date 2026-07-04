//! Token-efficiency benchmark for `ls` output (issue #339, ADR-0036 rule 5:
//! "reduction is measured").
//!
//! The issue's claim is that Iris's default `ls` (names-only, dirs first, `/`
//! suffix) is "at or above RTK's result": RTK parses `ls -la`, drops `.`/`..`,
//! and compacts perms to octal and sizes to human-readable, while Iris never
//! emits that noise in the first place. This corpus proves it with numbers over
//! captured real `ls -la` listings under `ls_corpus/`:
//!
//! - `raw`  — the captured `ls -la` output, what a naive relay would return.
//! - `rtk`  — the RTK-cleaned form: `<octal-perms> <human-size> <name>` per
//!   entry, `total`/`.`/`..` dropped (modeled here, not shelling out to RTK).
//! - `iris` — the real tool output over a reconstructed directory, measured
//!   through the production seam (`super::ls`).
//!
//! Three contracts hold on every fixture:
//! - `iris` is never larger than `rtk` (parity-or-better) — the "at or above
//!   RTK" claim, asserted rather than asserted-by-prose;
//! - `iris` cuts the raw `ls -la` listing by a wide margin (noisy-class bar);
//! - sampled real entry names survive verbatim in `iris` (zero quality loss).
//!
//! `ls_benchmark_report` prints the table committed to
//! `docs/benchmarks/issue-339-ls-tokens.md`; regenerate with
//! `cargo test --bin iris ls_benchmark_report -- --nocapture`.

use std::time::Duration;

use super::{LsInput, human_size, ls};
use crate::tools::bench_support;
use crate::tools::test_support::{TestDir, root_of, temp_dir};

/// A benchmark fixture: a captured `ls -la` listing plus entry names that must
/// survive verbatim in the Iris output.
struct Fixture {
    class: &'static str,
    raw: &'static str,
    needles: &'static [&'static str],
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            class: "ls -la (flat, many .toml)",
            raw: include_str!("flat_many.txt"),
            needles: &["ansible-playbook.toml", "LICENSE-APACHE-2.0", "NOTICE.md"],
        },
        Fixture {
            class: "ls -la (mixed dirs+files)",
            raw: include_str!("mixed.txt"),
            needles: &["bash/", "edit.rs", "ls.rs"],
        },
    ]
}

/// One parsed `ls -la` row.
struct ParsedEntry {
    perms: String,
    size: u64,
    name: String,
    is_dir: bool,
}

/// Parse a captured `ls -la` block into entries, dropping the `total` header and
/// the `.`/`..` self/parent links (exactly what RTK strips).
fn parse(raw: &str) -> Vec<ParsedEntry> {
    raw.lines()
        .filter(|l| !l.starts_with("total "))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let perms = it.next()?;
            let _links = it.next()?;
            let _owner = it.next()?;
            let _group = it.next()?;
            let size: u64 = it.next()?.parse().ok()?;
            let _month = it.next()?;
            let _day = it.next()?;
            let _time = it.next()?;
            let name = it.collect::<Vec<_>>().join(" ");
            if name.is_empty() || name == "." || name == ".." {
                return None;
            }
            Some(ParsedEntry {
                is_dir: perms.starts_with('d'),
                perms: perms.to_string(),
                size,
                name,
            })
        })
        .collect()
}

/// `drwxrwxr-x` -> `0775`: the octal permission RTK compacts the symbolic
/// string to.
fn octal(perms: &str) -> String {
    let bits: Vec<char> = perms.chars().skip(1).take(9).collect();
    if bits.len() < 9 {
        return "?".to_string();
    }
    let triple = |o: usize| {
        let r = u32::from(bits[o] != '-') * 4;
        let w = u32::from(bits[o + 1] != '-') * 2;
        let x = u32::from(bits[o + 2] != '-');
        r + w + x
    };
    format!("0{}{}{}", triple(0), triple(3), triple(6))
}

/// RTK-cleaned rendering: octal perms, human-readable size, and the name. No
/// `/` suffix is added (RTK preserves the bare name), which keeps this a fair,
/// conservative baseline for the "iris is at or above RTK" comparison.
fn rtk(entries: &[ParsedEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let size = if e.is_dir {
                "-".to_string()
            } else {
                human_size(e.size)
            };
            format!("{} {} {}", octal(&e.perms), size, e.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reconstruct a directory from the parsed entries and return the real Iris
/// `ls` output over it (default flat, names-only). The `TestDir` is returned so
/// the caller keeps it alive for the duration of any timing loop.
fn iris_listing(entries: &[ParsedEntry]) -> (TestDir, String) {
    let dir = temp_dir();
    for e in entries {
        if e.is_dir {
            std::fs::create_dir_all(dir.path.join(&e.name)).unwrap();
        } else {
            std::fs::write(dir.path.join(&e.name), b"").unwrap();
        }
    }
    let root = root_of(&dir);
    let out = ls(
        &root,
        &LsInput {
            path: None,
            limit: None,
            recursive: false,
            depth: None,
            long: false,
        },
    )
    .unwrap()
    .content;
    (dir, out)
}

#[test]
fn corpus_iris_is_at_or_above_rtk() {
    // The headline claim: Iris's default listing is never larger than RTK's
    // cleaned output, because Iris drops the octal + size columns RTK keeps.
    for f in fixtures() {
        let entries = parse(f.raw);
        assert!(!entries.is_empty(), "[{}] fixture parsed empty", f.class);
        let rtk = rtk(&entries);
        let (_dir, iris) = iris_listing(&entries);
        bench_support::assert_parity_or_better(f.class, &rtk, &iris);
        bench_support::assert_survives_verbatim(f.class, &iris, f.needles);
    }
}

#[test]
fn corpus_iris_cuts_raw_ls_la() {
    // Against the raw `ls -la` a naive relay would emit, the names-only listing
    // is a large, measured reduction with every name preserved.
    for f in fixtures() {
        let entries = parse(f.raw);
        let (_dir, iris) = iris_listing(&entries);
        bench_support::assert_min_reduction(f.class, f.raw, &iris, 60);
    }
}

#[test]
fn corpus_overhead_under_10ms_per_call() {
    // Measures the shipped default path (flat, names-only). The recursive walk's
    // resource bound is covered separately by `ls_scan_budget_bounds_recursive_collection`
    // in `super`, which caps collection regardless of tree size.
    for f in fixtures() {
        let entries = parse(f.raw);
        let (dir, _) = iris_listing(&entries);
        let root = root_of(&dir);
        let input = LsInput {
            path: None,
            limit: None,
            recursive: false,
            depth: None,
            long: false,
        };
        // Warm the OS page cache before timing.
        let _ = ls(&root, &input).unwrap();
        bench_support::assert_call_overhead_under(f.class, Duration::from_millis(10), || {
            let _ = ls(&root, &input).unwrap();
        });
    }
}

#[test]
fn ls_benchmark_report() {
    // Prints the table committed to docs/benchmarks/issue-339-ls-tokens.md.
    // Regenerate with: cargo test --bin iris ls_benchmark_report -- --nocapture
    use bench_support::{report_header, report_row};
    println!("== raw `ls -la` -> Iris default (names-only) ==");
    println!("{}", report_header());
    for f in fixtures() {
        let entries = parse(f.raw);
        let (_dir, iris) = iris_listing(&entries);
        println!("{}", report_row(f.class, f.raw, &iris, "names-only"));
    }

    println!("\n== RTK-cleaned -> Iris default (the \"at or above RTK\" claim) ==");
    println!("{}", report_header());
    for f in fixtures() {
        let entries = parse(f.raw);
        let (_dir, iris) = iris_listing(&entries);
        let rtk = rtk(&entries);
        println!("{}", report_row(f.class, &rtk, &iris, "drop perms+size"));
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn octal_converts_symbolic_perms() {
        assert_eq!(octal("drwxrwxr-x"), "0775");
        assert_eq!(octal("-rw-rw-r--"), "0664");
        assert_eq!(octal("-rwxr-xr-x"), "0755");
        assert_eq!(octal("short"), "?");
    }

    #[test]
    fn parse_drops_total_and_self_parent() {
        let raw = "total 8\n\
                   drwxrwxr-x 2 me me 4096 Jul  5 01:24 .\n\
                   drwxrwxr-x 5 me me 4096 Jul  5 01:24 ..\n\
                   drwxrwxr-x 2 me me 4096 Jul  5 01:24 sub\n\
                   -rw-rw-r-- 1 me me 1696 Jul  5 01:24 a.toml\n";
        let entries = parse(raw);
        assert_eq!(entries.len(), 2, "self/parent/total dropped");
        assert!(entries[0].is_dir && entries[0].name == "sub");
        assert!(!entries[1].is_dir && entries[1].name == "a.toml");
        assert_eq!(entries[1].size, 1696);
    }
}
