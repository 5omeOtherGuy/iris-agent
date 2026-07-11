# Campaign probe-terra-diag report

Verdict: PASS (exclusions 0 / budget 1). Notional prices: built-in table + campaign [prices.*] overrides (as of 2026-07-10).

Note: `notional_usd` is `null` for any lane whose model has no price. Supply `[prices.<model-id>]` in the campaign config to price it.

| cell | run | requests | input | output | cache_read | notional_usd | outcome |
| --- | --- | --- | --- | --- | --- | --- | --- |
| S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 0 | 22 | 674044 | 1215 | 497152 | null | Pass |

Row schema: one row per provider request; see the design doc (docs/... compaction-live-harness) and `metrics.rs::Row`.
