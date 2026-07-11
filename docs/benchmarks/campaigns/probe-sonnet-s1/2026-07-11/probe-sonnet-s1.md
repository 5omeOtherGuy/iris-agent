# Campaign probe-sonnet-s1 report

Verdict: PASS (exclusions 0 / budget 1). Notional prices: built-in table + campaign [prices.*] overrides (as of 2026-07-10).

| cell | run | requests | input | output | cache_read | notional_usd | outcome |
| --- | --- | --- | --- | --- | --- | --- | --- |
| S1::anthropic/claude-sonnet-4-6@low::s72-h90-k8000-w120000-subagent-5m | 0 | 7 | 132272 | 1135 | 91908 | 0.1959 | Pass |

Row schema: one row per provider request; see the design doc (docs/... compaction-live-harness) and `metrics.rs::Row`.
