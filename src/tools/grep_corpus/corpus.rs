//! Benchmark and quality-loss corpus for grep's content-mode output (issue
//! #338, ADR-0036 rule 5: "reduction is measured").
//!
//! Fixtures under `grep_corpus/` are real source files copied verbatim from
//! this repo (tool modules and the generated codemap index), covering the
//! three representative classes the issue calls out:
//! - high-match: many matches in one file (per-file-cap territory);
//! - many-files: matches spread across several files (grouping dedups paths);
//! - long-lines: a generated file with lines past the 500-char clamp.
//!
//! Everything is measured through the production seam (`super::grep`), so the
//! grouping, clamping, and per-file cap all sit inside the measurement. Three
//! quality contracts hold on every sample:
//! - grouped output is never larger than the ungrouped `path:line:content`
//!   baseline (`render_flat`) -- parity-or-better, the "grouped only if
//!   smaller" question answered with numbers;
//! - the exact total match count and every matched file path survive in the
//!   grouped output (no silent drops);
//! - the per-file cap shrinks a high-match file while still accounting for
//!   every omitted match by count.
//!
//! `grep_benchmark_report` prints the table committed to
//! `docs/benchmarks/issue-338-grep-output-tokens.md`; regenerate with
//! `cargo test grep_benchmark_report -- --nocapture`.

use super::{GrepInput, OutputMode, collect_for_bench, grep, render_flat};
use crate::tools::bench_support;
use crate::tools::test_support::{root_of, temp_dir};

/// A benchmark class: one or more real fixture files written into a temp
/// workspace, plus the pattern the search runs and the per-file cap used for
/// the cap-effect row.
struct Sample {
    class: &'static str,
    files: &'static [(&'static str, &'static str)],
    pattern: &'static str,
    /// Per-file cap for the cap-effect measurement (content mode).
    per_file_cap: usize,
    /// Lines that must survive verbatim in the grouped output.
    needles: &'static [&'static str],
}

fn samples() -> Vec<Sample> {
    vec![
        Sample {
            class: "high-match (one file)",
            files: &[(
                "high_match_source.rs",
                include_str!("high_match_source.txt"),
            )],
            pattern: r"self",
            per_file_cap: 5,
            needles: &["fn matched(&mut self"],
        },
        Sample {
            class: "many-files",
            files: &[
                (
                    "bench_support.rs",
                    include_str!("manyfiles_bench_support.txt"),
                ),
                ("text.rs", include_str!("manyfiles_text.txt")),
                ("path.rs", include_str!("manyfiles_path.txt")),
                ("ls.rs", include_str!("manyfiles_ls.txt")),
                ("find.rs", include_str!("manyfiles_find.txt")),
                ("observe.rs", include_str!("manyfiles_observe.txt")),
            ],
            pattern: r"fn ",
            per_file_cap: 20,
            needles: &["bench_support.rs", "observe.rs"],
        },
        Sample {
            class: "long-lines",
            files: &[("long_lines.md", include_str!("long_lines.txt"))],
            pattern: r"src/",
            per_file_cap: 20,
            needles: &["long_lines.md"],
        },
    ]
}

/// Build a content-mode input for `pattern` with the given per-file cap.
fn mk_input(pattern: &str, max_per_file: Option<usize>) -> GrepInput {
    GrepInput {
        pattern: pattern.into(),
        path: None,
        glob: None,
        ignore_case: false,
        literal: false,
        context: None,
        max_per_file,
        // Raise the global limit so the class's full match set is measured;
        // the per-file cap, not the global limit, is what this corpus exercises.
        limit: Some(100_000),
        output_mode: OutputMode::Content,
        head_limit: None,
        offset: None,
    }
}

/// Write a sample's fixtures into a fresh temp workspace and return its root.
fn workspace(sample: &Sample) -> (crate::tools::test_support::TestDir, std::path::PathBuf) {
    let dir = temp_dir();
    for (name, content) in sample.files {
        std::fs::write(dir.path.join(name), content).unwrap();
    }
    let root = root_of(&dir);
    (dir, root)
}

/// Grouped (production) output and the ungrouped flat baseline for a sample,
/// both uncapped, plus the exact total match count.
fn grouped_and_flat(sample: &Sample) -> (String, String, usize) {
    let (_dir, root) = workspace(sample);
    let grouped = grep(&root, &mk_input(sample.pattern, None), true)
        .unwrap()
        .0;
    let (files, total) = collect_for_bench(&root, &mk_input(sample.pattern, None));
    (grouped, render_flat(&files), total)
}

#[test]
fn corpus_grouping_is_parity_or_better_vs_flat() {
    // The "grouped only if smaller" question: grep grouping must never produce
    // more tokens than the ungrouped path:line:content baseline.
    for sample in samples() {
        let (grouped, flat, _) = grouped_and_flat(&sample);
        bench_support::assert_parity_or_better(sample.class, &flat, &grouped);
    }
}

#[test]
fn corpus_reports_exact_total_and_every_file_path() {
    // No silent drops: the exact total match count and every matched file path
    // appear in the grouped output.
    for sample in samples() {
        let (grouped, _, total) = grouped_and_flat(&sample);
        assert!(
            grouped.contains(&format!("{total} match")),
            "[{}] total {total} missing from header: {}",
            sample.class,
            grouped.lines().next().unwrap_or_default()
        );
        for (name, _) in sample.files {
            assert!(
                grouped.contains(name),
                "[{}] matched file {name} missing from output",
                sample.class
            );
        }
        bench_support::assert_survives_verbatim(sample.class, &grouped, sample.needles);
    }
}

#[test]
fn corpus_per_file_cap_shrinks_and_accounts_for_every_match() {
    for sample in samples() {
        let (_dir, root) = workspace(&sample);
        let uncapped = grep(&root, &mk_input(sample.pattern, None), true)
            .unwrap()
            .0;
        let capped = grep(
            &root,
            &mk_input(sample.pattern, Some(sample.per_file_cap)),
            true,
        )
        .unwrap()
        .0;

        // The cap never enlarges output, and both forms report the same total.
        assert!(
            bench_support::est_tokens(&capped) <= bench_support::est_tokens(&uncapped),
            "[{}] cap enlarged output",
            sample.class
        );

        // Every omitted match is accounted for by a count line: shown match
        // lines + summed omitted counts == the header total.
        let (_, total) = collect_for_bench(&root, &mk_input(sample.pattern, None));
        let shown = capped.lines().filter(|l| l.starts_with("> ")).count();
        let omitted: usize = capped
            .lines()
            .filter_map(|l| {
                l.trim_start()
                    .strip_prefix("\u{2026} ")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|n| n.parse::<usize>().ok())
            })
            .sum();
        assert_eq!(
            shown + omitted,
            total,
            "[{}] shown {shown} + omitted {omitted} != total {total}",
            sample.class
        );
    }
}

#[test]
fn corpus_overhead_under_10ms_per_call() {
    for sample in samples() {
        let (_dir, root) = workspace(&sample);
        let input = mk_input(sample.pattern, None);
        // Warm the OS page cache.
        let _ = grep(&root, &input, true).unwrap();
        bench_support::assert_call_overhead_under(
            sample.class,
            std::time::Duration::from_millis(10),
            || {
                let _ = grep(&root, &input, true).unwrap();
            },
        );
    }
}

#[test]
fn grep_benchmark_report() {
    // Prints the tables committed to
    // docs/benchmarks/issue-338-grep-output-tokens.md (run with --nocapture).
    println!("== grouping vs ungrouped baseline ==");
    println!("{}", bench_support::report_header());
    for sample in samples() {
        let (grouped, flat, _) = grouped_and_flat(&sample);
        println!(
            "{}",
            bench_support::report_row(sample.class, &flat, &grouped, "group")
        );
    }

    println!("\n== per-file cap effect (grouped, uncapped -> capped) ==");
    println!("{}", bench_support::report_header());
    for sample in samples() {
        let (_dir, root) = workspace(&sample);
        let uncapped = grep(&root, &mk_input(sample.pattern, None), true)
            .unwrap()
            .0;
        let capped = grep(
            &root,
            &mk_input(sample.pattern, Some(sample.per_file_cap)),
            true,
        )
        .unwrap()
        .0;
        let via = format!("cap={}", sample.per_file_cap);
        println!(
            "{}",
            bench_support::report_row(sample.class, &uncapped, &capped, &via)
        );
    }

    println!("\n== context-line default audit (high-match, grouped tokens) ==");
    let sample = &samples()[0];
    let (_dir, root) = workspace(sample);
    for ctx in [0usize, 1, 2, 3] {
        let mut input = mk_input(sample.pattern, None);
        input.context = Some(ctx);
        let out = grep(&root, &input, true).unwrap().0;
        println!(
            "context={ctx}: {} est tokens",
            bench_support::est_tokens(&out)
        );
    }
}
