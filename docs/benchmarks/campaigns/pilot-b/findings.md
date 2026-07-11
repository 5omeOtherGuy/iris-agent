# Pilot B findings (openai-codex lane, 2026-07-11)

Campaign files: `../pilot-b.toml` (terra), `../probe-sol.toml`, `../pilot-b-s1-tuned.toml`.
All runs via `IRIS_BENCH_CAMPAIGN_FILE`; first campaigns executed through the
config loader (PR #569) with zero code edits.

## Results

- `gpt-5.6-luna`: unavailable API-side. All 6 runs 404
  (`invalid_request_error`, `/codex/responses`); identical path works for
  sol/terra, and session history shows no Luna request has ever succeeded.
  Recorded verbatim in `pilot-b` rows from the first (discarded) attempt;
  retry later is a one-line lane edit.
- `probe-sol` (S3, 1 run): PASS. Loader + codex lane proven end-to-end.
- `pilot-b` (terra low, S1/S3/S4-small, n=2): verdict FAIL by fail-loud rule.
  - S1 run0: no compaction -- terra tokenizes the identical S1 payload ~25%
    lighter than sonnet (seed 14,130 vs 18,905 tokens), so 6 round-trips top
    out at 26.4k, under hard (29.5k). S1's 20% margin was sonnet-calibrated;
    live margins are model-family-specific.
  - S1 run1: PASS behavior -- terra's own retries drove context to 32.8k;
    two compaction generations applied (subagent origin) with the prefix-break
    signature (cache reads 30.2k -> 1.5k). First cross-provider compaction
    economics rows.
- `pilot-b-s1-tuned` (S1 with `budget = 24576`): both runs FAIL identically --
  terra performs one read then ends the turn (2 boundaries < 3). Open
  investigation; folds ruled out (fold_flushes=0 on every row) and the
  runtime injects no model-visible warn notice.

## Resolutions (2026-07-11, probe campaigns `probe-terra-28k`, `probe-terra-32k`, `probe-sol-s1`, `probe-sonnet-s1`, `probe-terra-diag`)

1. Early-stop: NOT budget, folds, warn-tier, #574 code, or environment. The
   probe ladder eliminated each: fails at 24k/28k/32k, on pre- and post-#574
   code (bisect worktree at 3d8f2ff), on terra AND sol -- while sonnet passes
   S1 on the same binary. Time series: 2/2 pass -> 7/7 early-stop (~13h later)
   -> 1/1 pass (2h after that). Conclusion: time-varying `/codex/responses`
   endpoint behavior (models intermittently refuse repetitive sequential-read
   instructions). Mitigations shipped: S1 drive prompt hardened to
   mandatory-verification language (this PR), and #577 transcripts record the
   model's stated stop reason for any recurrence.
2. Cache writes: settled = endpoint. Raw usage capture (#577,
   `RUST_LOG=iris::usage_raw=debug`, 24/24 requests in `probe-terra-diag`)
   shows the endpoint always sends `"cache_write_tokens": 0` -- including
   requests straddling two compaction generations with certain prefix
   re-writes. #557 parsing is correct; the subscription `/codex/responses`
   lane does not meter cache writes. `write_unreported` already encodes this.
