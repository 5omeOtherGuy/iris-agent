//! Benchmark and quality-loss corpus for read's skim mode (issue #337,
//! ADR-0036 rule 5: "reduction is measured").
//!
//! Fixtures under `corpus/` are real source files, committed verbatim:
//! comment-heavy Rust (openai/codex `command_exec.rs`, Apache-2.0),
//! comment-heavy TypeScript (earendil-works/pi-mono `agent/src/types.ts`,
//! MIT), docstring-heavy Python (CPython 3.12 `bisect.py`, PSF), one
//! comment-light TypeScript file (pi-mono `ai/src/cli.ts`, MIT — honesty
//! sample: skim helps little there), and one JSON file (pi-mono
//! `package.json` — data formats are never skimmed).
//!
//! Everything is measured through the production seam: `read::execute` with
//! and without `skim: true`, so the guards (never-worse, emptied-non-empty,
//! data-format passthrough) and the rendered line numbers are inside the
//! measurement. Three quality contracts hold on every sample:
//! - comment-heavy classes hit a >= 50% token-reduction bar;
//! - every rendered skim line is byte-identical to the original file line it
//!   is numbered as (only removals, never rewrites), and every signature
//!   line in the raw fixture survives verbatim;
//! - per-call skim overhead stays under 10 ms.
//!
//! `skim_benchmark_report` prints the table committed to
//! `docs/benchmarks/issue-337-read-skim-tokens.md`; regenerate with
//! `cargo test skim_benchmark_report -- --nocapture`.

use serde_json::json;

use crate::tools::ObservedFiles;
use crate::tools::bench_support;
use crate::tools::test_support::{root_of, temp_dir};

struct Sample {
    /// Corpus class for the report.
    class: &'static str,
    /// File name (with real extension) written into the temp workspace;
    /// drives the extension-based language detection.
    file_name: &'static str,
    /// The committed real source file.
    raw: &'static str,
    /// Required minimum token reduction (percent) — comment-heavy classes.
    min_reduction: Option<u32>,
    /// Hand-picked lines that must survive skim verbatim.
    must_contain: &'static [&'static str],
    /// Data formats: skim output must be byte-identical to the full read.
    expect_passthrough: bool,
    /// Trimmed-line prefixes marking signatures; every raw line starting
    /// with one must survive skim verbatim.
    signature_prefixes: &'static [&'static str],
}

const RUST_SIGNATURES: &[&str] = &[
    "pub fn ",
    "fn ",
    "pub struct ",
    "struct ",
    "pub enum ",
    "enum ",
    "impl ",
    "pub trait ",
    "use ",
];
const TS_SIGNATURES: &[&str] = &[
    "export ",
    "function ",
    "const ",
    "interface ",
    "class ",
    "import ",
];
const PY_SIGNATURES: &[&str] = &["def ", "class ", "import ", "from "];

fn samples() -> Vec<Sample> {
    vec![
        Sample {
            class: "rust (comment-heavy)",
            file_name: "command_exec.rs",
            raw: include_str!("corpus/rust-comment-heavy.txt"),
            min_reduction: Some(50),
            must_contain: &[
                "pub struct CommandExecParams {",
                "pub struct CommandExecTerminalSize {",
            ],
            expect_passthrough: false,
            signature_prefixes: RUST_SIGNATURES,
        },
        Sample {
            class: "typescript (comment-heavy)",
            file_name: "types.ts",
            raw: include_str!("corpus/typescript-comment-heavy.txt"),
            min_reduction: Some(50),
            must_contain: &[
                "export type ToolExecutionMode = \"sequential\" | \"parallel\";",
                "export interface BeforeToolCallResult {",
            ],
            expect_passthrough: false,
            signature_prefixes: TS_SIGNATURES,
        },
        Sample {
            class: "python (docstring-heavy)",
            file_name: "bisect.py",
            raw: include_str!("corpus/python-comment-heavy.txt"),
            min_reduction: Some(50),
            must_contain: &[
                "def insort_right(a, x, lo=0, hi=None, *, key=None):",
                "def bisect_left(a, x, lo=0, hi=None, *, key=None):",
            ],
            expect_passthrough: false,
            signature_prefixes: PY_SIGNATURES,
        },
        Sample {
            class: "typescript (comment-light)",
            file_name: "cli.ts",
            raw: include_str!("corpus/typescript-comment-light.txt"),
            // Honesty sample: little to strip, so no bar; the guards decide
            // whether skim applies at all.
            min_reduction: None,
            must_contain: &["const AUTH_FILE = \"auth.json\";"],
            expect_passthrough: false,
            signature_prefixes: TS_SIGNATURES,
        },
        Sample {
            class: "json (data format)",
            file_name: "package.json",
            raw: include_str!("corpus/package-json.txt"),
            min_reduction: None,
            must_contain: &["pi-monorepo"],
            expect_passthrough: true,
            signature_prefixes: &[],
        },
    ]
}

/// Run one sample through the production seam: `read::execute` on a real
/// file in a temp workspace, once full and once with `skim: true`. Returns
/// `(full rendering, skim rendering, metadata.skim)`.
fn read_full_and_skim(sample: &Sample) -> (String, String, String) {
    let dir = temp_dir();
    std::fs::write(dir.path.join(sample.file_name), sample.raw).unwrap();
    let root = root_of(&dir);
    let full = crate::tools::read::execute(
        &root,
        &json!({"path": sample.file_name}),
        &mut ObservedFiles::new(),
    )
    .unwrap();
    let skim = crate::tools::read::execute(
        &root,
        &json!({"path": sample.file_name, "skim": true}),
        &mut ObservedFiles::new(),
    )
    .unwrap();
    let meta = skim
        .metadata
        .get("skim")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    (full.content, skim.content, meta)
}

#[test]
fn corpus_comment_heavy_classes_hit_reduction_bar() {
    for sample in samples() {
        let Some(min) = sample.min_reduction else {
            continue;
        };
        let (full, skim, meta) = read_full_and_skim(&sample);
        assert_eq!(meta, "applied", "[{}] expected skim to apply", sample.class);
        bench_support::assert_min_reduction(sample.class, &full, &skim, min);
    }
}

#[test]
fn corpus_every_skim_line_matches_the_original_file_line() {
    // The "zero quality loss" contract, half one: skim only removes lines.
    // Every rendered line must be byte-identical to the original file line it
    // is numbered as, so line numbers are true and nothing is rewritten.
    for sample in samples() {
        let (_, skim, _) = read_full_and_skim(&sample);
        let raw_lines: Vec<&str> = sample.raw.lines().collect();
        let mut rendered = 0usize;
        for line in skim.lines() {
            let Some((num, text)) = line.split_once('\u{2192}') else {
                continue; // blank separator or the skim/truncation notice
            };
            let Ok(n) = num.trim().parse::<usize>() else {
                continue;
            };
            assert_eq!(
                raw_lines[n - 1],
                text,
                "[{}] rendered line {n} differs from the file",
                sample.class
            );
            rendered += 1;
        }
        assert!(rendered > 0, "[{}] no rendered lines", sample.class);
    }
}

#[test]
fn corpus_signatures_and_needles_survive_verbatim() {
    // The "zero quality loss" contract, half two: every signature line in
    // the raw fixture (and each hand-picked needle) survives skim verbatim.
    for sample in samples() {
        let (_, skim, _) = read_full_and_skim(&sample);
        bench_support::assert_survives_verbatim(sample.class, &skim, sample.must_contain);
        let signatures: Vec<&str> = sample
            .raw
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                sample
                    .signature_prefixes
                    .iter()
                    .any(|prefix| trimmed.starts_with(prefix))
            })
            .collect();
        assert!(
            sample.signature_prefixes.is_empty() || !signatures.is_empty(),
            "[{}] fixture has no signature lines to check",
            sample.class
        );
        bench_support::assert_survives_verbatim(sample.class, &skim, &signatures);
    }
}

#[test]
fn corpus_data_formats_pass_through_untouched() {
    for sample in samples() {
        if !sample.expect_passthrough {
            continue;
        }
        let (full, skim, meta) = read_full_and_skim(&sample);
        assert_eq!(
            skim, full,
            "[{}] data format must render identically under skim",
            sample.class
        );
        assert_eq!(
            meta, "full (file type is never skimmed)",
            "[{}]",
            sample.class
        );
    }
}

#[test]
fn corpus_skim_overhead_under_10ms_per_call() {
    for sample in samples() {
        let dir = temp_dir();
        std::fs::write(dir.path.join(sample.file_name), sample.raw).unwrap();
        let root = root_of(&dir);
        let args = json!({"path": sample.file_name, "skim": true});
        // Warm the page cache with one read.
        let _ = crate::tools::read::execute(&root, &args, &mut ObservedFiles::new()).unwrap();
        bench_support::assert_call_overhead_under(
            sample.class,
            std::time::Duration::from_millis(10),
            || {
                let _ =
                    crate::tools::read::execute(&root, &args, &mut ObservedFiles::new()).unwrap();
            },
        );
    }
}

#[test]
fn skim_benchmark_report() {
    // Prints the table committed to
    // docs/benchmarks/issue-337-read-skim-tokens.md (run with --nocapture).
    println!("{}", bench_support::report_header());
    for sample in samples() {
        let (full, skim, meta) = read_full_and_skim(&sample);
        let via = if meta == "applied" {
            "skim"
        } else {
            meta.as_str()
        };
        println!(
            "{}",
            bench_support::report_row(sample.class, &full, &skim, via)
        );
    }
}
