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

## Open questions

1. Terra early-stop at budget 24576: next probe budget 28672; harness gap --
   rows do not record assistant text, so behavioral stops cannot be diagnosed
   from artifacts alone.
2. Codex lane reports no `cache_write_tokens` live (null on all 19 usage rows;
   cache reads flow fine) despite GPT-5.6 documentation and the #557 parsing.
   Endpoint-vs-parsing undetermined; needs one raw usage JSON capture.
