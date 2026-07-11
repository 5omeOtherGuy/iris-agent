# Campaign probe-terra-28k report

Verdict: FAIL (exclusions 0 / budget 1). Notional prices: built-in table + campaign [prices.*] overrides (as of 2026-07-10).

Note: `notional_usd` is `null` for any lane whose model has no price. Supply `[prices.<model-id>]` in the campaign config to price it.

| cell | run | requests | input | output | cache_read | notional_usd | outcome |
| --- | --- | --- | --- | --- | --- | --- | --- |
| S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 0 | 2 | 30181 | 183 | 13824 | null | Fail |
| S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m | 1 | 2 | 30182 | 94 | 13824 | null | Fail |

## Scenario failures

A run that completed without a provider error but did not exercise its target behavior is a hard failure, recorded verbatim here.

- `S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m#run0`: S1 produced no compaction: only 2 boundaries (< 3 required); the turn did not drive enough mid-turn round-trips
- `S1::openai-codex/gpt-5.6-terra@low::s72-h90-k8000-w120000-subagent-5m#run1`: S1 produced no compaction: only 2 boundaries (< 3 required); the turn did not drive enough mid-turn round-trips

Row schema: one row per provider request; see the design doc (docs/... compaction-live-harness) and `metrics.rs::Row`.
