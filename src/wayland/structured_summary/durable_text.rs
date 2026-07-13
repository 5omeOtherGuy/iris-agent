//! Deterministic durable-summary text renderer (issue #475, ADR-0061). Turns
//! a validated [`CompactionSummary`] into the plain text `append_compaction`
//! persists. Pure function: never persists raw provider JSON, never mutates
//! the session log (that stays `CompactionEngine::apply_summary`'s job in a
//! later wiring slice).

use super::schema::CompactionSummary;

/// **`preserved_identifiers` placement decision**: rendered as its own
/// trailing section (below), omitted entirely when empty. Rationale --
/// keeping credential-shaped facts the user explicitly asked to keep
/// (ADR-0061 F17) in a distinct, clearly labeled section makes them easy to
/// locate and audit, rather than diluting them into an undifferentiated
/// `Key facts` bucket where a future reader (or a future scrub pass) cannot
/// tell "the user asked to keep this" from an ordinary fact. Omitting the
/// section entirely when empty also means a summary with nothing to preserve
/// renders byte-identical to the plain five-section #475 format.
const PRESERVED_IDENTIFIERS_HEADER: &str = "Preserved identifiers";

/// Render `summary` into the deterministic durable text `append_compaction`
/// persists: `Goal`/`State`/`Decisions`/`Key facts`/`Next steps`, in that
/// order, plus the optional trailing `Preserved identifiers` section. Empty
/// list fields render their header with no bullets (issue #475: empty
/// `decisions` is allowed "when no durable choices are evident").
pub(crate) fn render_durable_summary(summary: &CompactionSummary) -> String {
    let mut out = String::new();
    out.push_str("Goal\n");
    out.push_str(summary.goal.trim());
    push_section(&mut out, "State", &summary.state);
    push_section(&mut out, "Decisions", &summary.decisions);
    push_section(&mut out, "Key facts", &summary.key_facts);
    push_section(&mut out, "Next steps", &summary.next_steps);
    if !summary.preserved_identifiers.is_empty() {
        push_section(
            &mut out,
            PRESERVED_IDENTIFIERS_HEADER,
            &summary.preserved_identifiers,
        );
    }
    out
}

fn push_section(out: &mut String, header: &str, items: &[String]) {
    out.push_str("\n\n");
    out.push_str(header);
    for item in items {
        out.push_str("\n- ");
        out.push_str(item.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> CompactionSummary {
        CompactionSummary {
            goal: "Ship #475 structured summaries".to_string(),
            state: vec![
                "renderer written".to_string(),
                "validator written".to_string(),
            ],
            decisions: vec![
                "native structured output first, forced-tool fallback second".to_string(),
            ],
            key_facts: vec!["src/wayland/structured_summary/ holds the new modules".to_string()],
            next_steps: vec!["wire provider request plumbing".to_string()],
            preserved_identifiers: vec!["DEPLOY-KEY-AB12CD34".to_string()],
        }
    }

    #[test]
    fn renders_all_sections_in_order_with_expected_headers() {
        let rendered = render_durable_summary(&summary());
        assert_eq!(
            rendered,
            "Goal\n\
             Ship #475 structured summaries\n\
             \n\
             State\n\
             - renderer written\n\
             - validator written\n\
             \n\
             Decisions\n\
             - native structured output first, forced-tool fallback second\n\
             \n\
             Key facts\n\
             - src/wayland/structured_summary/ holds the new modules\n\
             \n\
             Next steps\n\
             - wire provider request plumbing\n\
             \n\
             Preserved identifiers\n\
             - DEPLOY-KEY-AB12CD34"
        );
    }

    #[test]
    fn empty_decisions_renders_header_with_no_bullets() {
        let mut summary = summary();
        summary.decisions = Vec::new();
        let rendered = render_durable_summary(&summary);
        assert!(rendered.contains("\n\nDecisions\n\nKey facts"));
    }

    #[test]
    fn empty_preserved_identifiers_omits_the_section_entirely() {
        let mut summary = summary();
        summary.preserved_identifiers = Vec::new();
        let rendered = render_durable_summary(&summary);
        assert!(!rendered.contains("Preserved identifiers"));
        assert!(rendered.ends_with("wire provider request plumbing"));
    }

    #[test]
    fn never_persists_raw_json() {
        let rendered = render_durable_summary(&summary());
        assert!(!rendered.trim_start().starts_with('{'));
        assert!(!rendered.contains("\"goal\""));
    }

    #[test]
    fn is_deterministic() {
        let summary = summary();
        assert_eq!(
            render_durable_summary(&summary),
            render_durable_summary(&summary)
        );
    }
}
