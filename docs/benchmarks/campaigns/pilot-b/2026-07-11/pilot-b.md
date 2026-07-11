# Campaign pilot-b report

Verdict: FAIL (exclusions 0 / budget 1). Notional prices: built-in table + campaign [prices.*] overrides (as of 2026-07-10).

Note: `notional_usd` is `null` for any lane whose model has no price. Supply `[prices.<model-id>]` in the campaign config to price it.

| cell | run | requests | input | output | cache_read | notional_usd | outcome |
| --- | --- | --- | --- | --- | --- | --- | --- |
| S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 0 | 7 | 141485 | 563 | 89600 | null | Fail |
| S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 1 | 13 | 239646 | 705 | 178688 | null | Pass |
| S3::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 0 | 1 | 4572 | 33 | 0 | null | Pass |
| S3::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 1 | 1 | 4572 | 16 | 4096 | null | Pass |
| S4-small::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 0 | 6 | 32125 | 137 | 22016 | null | Pass |
| S4-small::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 1 | 6 | 26823 | 157 | 15360 | null | Pass |

## Scenario failures

A run that completed without a provider error but did not exercise its target behavior is a hard failure, recorded verbatim here.

- `S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m#run0`: S1 produced no compaction

Row schema: one row per provider request; see the design doc (docs/... compaction-live-harness) and `metrics.rs::Row`.
