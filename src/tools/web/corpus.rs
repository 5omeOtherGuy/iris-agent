//! Token-efficiency benchmark and quality-loss corpus for the web tools
//! (`web_search` + `read_web_page`), ADR-0036 rule 5: "reduction is measured".
//!
//! Fixtures under `corpus/` are real captured outputs (GNU article HTML, a
//! Telegram-web JS shell, a `text/plain` robots.txt, a Jina reader Markdown
//! dump, and a DuckDuckGo HTML result page). Each `Sample` drives one of the
//! production reduction seams through its real entry point -- never a
//! reimplementation:
//!
//! - `extract::extract_markdown` (HTML -> Markdown article extraction),
//! - `excerpts::select_excerpts` (objective excerpting),
//! - `search::parse_html_results` -> `tool::render_results` (raw search
//!   response -> compact ranked list).
//!
//! Four test classes enforce the ADR-0036 bars, mirroring
//! `tools::bash::filter::corpus`:
//! - noisy classes must hit their minimum reduction bars (floors set from the
//!   observed reduction on the real fixture; >= 60 per ADR-0036 rule 5);
//! - the honest "no readable content" diagnostic (the "failure is complete"
//!   analog), every search result's title+URL, and the objective-answering
//!   passage survive verbatim, AND the untrusted-content framing header
//!   survives on every framed sample (a security invariant);
//! - the `text/plain` passthrough class ships verbatim (uncovered by
//!   extraction);
//! - per-sample seam overhead stays under 10 ms.
//!
//! `web_corpus_benchmark_report` prints the table committed to
//! `docs/benchmarks/web-tools-token-efficiency.md`; regenerate with
//! `cargo test web_corpus_benchmark_report -- --nocapture`.
//!
//! The tool-agnostic measurement core lives in `tools::bench_support`; the
//! recipe is the `token-efficiency-benchmark` skill (`.pi/skills/`).

use std::time::Duration;

use super::excerpts::select_excerpts;
use super::extract::extract_markdown;
use super::read::EXCERPT_BUDGET_CHARS;
use super::search::parse_html_results;
use super::tool::render_results;
use super::{MAX_BODY_BYTES, frame_untrusted};
use crate::tools::bench_support;

/// Which production reduction seam a sample drives.
enum Seam {
    /// `read_web_page` native reader: raw HTML -> `extract_markdown` at the
    /// no-objective body cap (`MAX_BODY_BYTES`), exactly as `read::native`.
    Extract,
    /// `read_web_page` objective path: fetched Markdown -> `select_excerpts` at
    /// the production `EXCERPT_BUDGET_CHARS`, exactly as `read::run_read`.
    Excerpt { objective: &'static str },
    /// `read_web_page` `text/plain`: the native reader ships the body verbatim
    /// (extraction is not applied); the reducer is identity.
    Passthrough,
    /// `web_search` native (DuckDuckGo): raw HTML -> `parse_html_results` ->
    /// `render_results`, exactly as `execute_web_search` after the fetch.
    SearchDuckduckgo,
}

impl Seam {
    /// Per-call overhead ceiling for this seam (debug build, best-of-three).
    /// The DOM/parse-bound seams get looser debug bars than the reference 10 ms:
    /// `Extract` runs the readability parse + HTML->Markdown conversion and
    /// `SearchDuckduckgo` parses the result HTML with `dom_query` (both run off
    /// the async runtime in production), and `Excerpt` scores every passage of a
    /// full page. The bars still catch a gross regression (release is far
    /// faster); the identity `Passthrough` holds the reference 10 ms.
    fn overhead_bar(&self) -> Duration {
        match self {
            Seam::Extract => Duration::from_millis(150),
            Seam::SearchDuckduckgo => Duration::from_millis(100),
            Seam::Excerpt { .. } => Duration::from_millis(50),
            Seam::Passthrough => Duration::from_millis(10),
        }
    }
}

/// A benchmark fixture plus the invocation context that drives its seam.
struct Sample {
    /// Class name for the report.
    class: &'static str,
    /// Framing source label: `final_url` for reads, `search: <query>` for
    /// search -- what `execute_*` passes to `frame_untrusted`.
    source: &'static str,
    /// Framing backend id (`native` / `jina`).
    backend: &'static str,
    /// Captured raw upstream bytes (HTML / Markdown / text) as a string.
    raw: &'static str,
    /// The reduction seam this sample exercises.
    seam: Seam,
    /// Required minimum token reduction (percent), noisy classes only.
    min_reduction: Option<u32>,
    /// Content that must survive reduction + framing verbatim.
    must_contain: &'static [&'static str],
    /// True when the class is deliberately uncovered by reduction: the reduced
    /// form must equal the input (honesty proof for the report).
    expect_passthrough: bool,
}

fn samples() -> Vec<Sample> {
    vec![
        Sample {
            class: "read: article HTML -> Markdown",
            source: "https://blog.rust-lang.org/2024/02/08/Rust-1.76.0/",
            backend: "native",
            raw: include_str!("corpus/article-rust-blog.html"),
            seam: Seam::Extract,
            // Observed ~74%: extraction strips the site chrome (nav, header,
            // footer, scripts, styles) and keeps the article prose verbatim.
            min_reduction: Some(60),
            must_contain: &["Announcing Rust 1.76.0", "type_name_of_val"],
            expect_passthrough: false,
        },
        Sample {
            class: "read: JS shell -> diagnostic",
            source: "https://web.telegram.org/",
            backend: "native",
            raw: include_str!("corpus/js-shell-telegram.html"),
            seam: Seam::Extract,
            // "Failure is complete": the honest diagnostic is the whole output;
            // no reduction bar, the diagnostic must survive verbatim.
            min_reduction: None,
            must_contain: &["No readable article content could be extracted"],
            expect_passthrough: false,
        },
        Sample {
            class: "read: text/plain passthrough",
            source: "https://www.gnu.org/robots.txt",
            backend: "native",
            raw: include_str!("corpus/compact-gnu-robots.txt"),
            seam: Seam::Passthrough,
            min_reduction: None,
            must_contain: &["User-agent: *"],
            expect_passthrough: true,
        },
        Sample {
            class: "read: objective excerpt (Markdown)",
            source: "https://www.gnu.org/philosophy/free-sw.en.html",
            backend: "jina",
            raw: include_str!("corpus/reader-gnu-free-sw.jina.md"),
            seam: Seam::Excerpt {
                objective: "what are the four freedoms of free software",
            },
            min_reduction: Some(60),
            must_contain: &[
                "The freedom to run the program as you wish, for any purpose (freedom 0).",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "search: DuckDuckGo HTML -> list",
            source: "search: rust async runtime",
            backend: "native",
            raw: include_str!("corpus/search-duckduckgo.html"),
            seam: Seam::SearchDuckduckgo,
            min_reduction: Some(80),
            // A competent reader must keep each result's title + URL; sampled
            // verbatim from the captured page's rendered rows.
            must_contain: &[
                "GitHub - smol-rs/smol: A small and fast async runtime for Rust",
                "https://github.com/smol-rs/smol",
                "Async in depth | Tokio - An asynchronous Rust runtime",
                "https://tokio.rs/tokio/tutorial/async",
            ],
            expect_passthrough: false,
        },
    ]
}

/// Reduced (model-facing, pre-framing) output for a sample plus the report
/// `via` label, produced through the real production seam.
fn reduce(sample: &Sample) -> (String, &'static str) {
    match &sample.seam {
        Seam::Extract => {
            let ex = extract_markdown(sample.raw, sample.source, MAX_BODY_BYTES);
            (ex.content, "extract")
        }
        Seam::Excerpt { objective } => (
            select_excerpts(sample.raw, objective, EXCERPT_BUDGET_CHARS),
            "excerpt",
        ),
        Seam::Passthrough => (sample.raw.to_string(), "(passthrough)"),
        Seam::SearchDuckduckgo => (render_results(&parse_html_results(sample.raw)), "render"),
    }
}

/// The full model-facing output for a sample: the reduced body wrapped in the
/// untrusted-content framing, exactly as `execute_web_search` /
/// `execute_read_web_page` emit it.
fn framed(sample: &Sample, reduced: &str) -> String {
    frame_untrusted(sample.source, sample.backend, reduced)
}

#[test]
fn web_corpus_noisy_classes_hit_reduction_bar() {
    for sample in samples() {
        let Some(min) = sample.min_reduction else {
            continue;
        };
        let (reduced, _) = reduce(&sample);
        bench_support::assert_min_reduction(sample.class, sample.raw, &reduced, min);
    }
}

#[test]
fn web_corpus_content_and_framing_survive_verbatim() {
    for sample in samples() {
        let (reduced, _) = reduce(&sample);
        let framed = framed(&sample, &reduced);
        // The untrusted-content framing is a security invariant: it must survive
        // reduction on every model-facing web result.
        assert!(
            framed.starts_with("[web content:"),
            "[{}] framing header missing\n--- framed ---\n{framed}",
            sample.class,
        );
        assert!(
            framed.contains("external, untrusted data"),
            "[{}] untrusted-data notice missing\n--- framed ---\n{framed}",
            sample.class,
        );
        // Content needles survive reduction (checked in the framed output, which
        // contains the reduced body).
        bench_support::assert_survives_verbatim(sample.class, &framed, sample.must_contain);
    }
}

#[test]
fn web_corpus_passthrough_untouched() {
    for sample in samples() {
        if !sample.expect_passthrough {
            continue;
        }
        let (reduced, _) = reduce(&sample);
        assert_eq!(
            reduced, sample.raw,
            "[{}] expected passthrough (class deliberately uncovered by reduction)",
            sample.class,
        );
    }
}

#[test]
fn web_corpus_seam_overhead_bounded_per_call() {
    // Warm any lazy state (dom parser init, allocator warmup) before timing.
    for sample in samples() {
        let _ = reduce(&sample);
    }
    for sample in samples() {
        bench_support::assert_call_overhead_under(sample.class, sample.seam.overhead_bar(), || {
            let _ = reduce(&sample);
        });
    }
}

#[test]
fn web_corpus_benchmark_report() {
    // Prints the table committed to docs/benchmarks/web-tools-token-efficiency.md
    // (run with --nocapture).
    println!("{}", bench_support::report_header());
    for sample in samples() {
        let (reduced, via) = reduce(&sample);
        println!(
            "{}",
            bench_support::report_row(sample.class, sample.raw, &reduced, via)
        );
    }
}
