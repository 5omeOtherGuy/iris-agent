# Compaction live-measurement harness

The single user entry point for the `live_harness` module: run compaction
measurements against any provider/model with a config file, no Rust edits.

Every number in an artifact comes from a real `ProviderUsage` record or a
session-log lifecycle entry. The token estimator's value appears only as the
`estimate_error` diagnostic column; it never feeds a reported metric.

## What it is

A lane x scenario x settings-cell x n matrix runner. It drives synthetic coding
sessions through the real Harness/compaction surfaces against a live provider,
records one JSONL row per provider request, and writes a per-run aggregate report
plus a resumable manifest. A new experiment is a config cell, not new code.

- A **lane** is a (provider, model, effort) tuple.
- A **scenario** (S1-S4) is a deterministic session driver that isolates one
  compaction behavior.
- A **cell** is a (scenario, settings) pair, optionally with size-knob overrides.
- A **campaign** is a set of lanes x cells x runs-per-cell, defined in one config
  file (or a built-in like `pilot-a`).

## Safety model

Double-gated, exactly like the legacy per-experiment live bench:

1. The only live entry point, `live_campaign`, is `#[ignore]`, so `cargo test`
   and CI never run it.
2. It is additionally guarded by `IRIS_BENCH_LIVE=1`. Even
   `cargo test -- --ignored live_campaign` is a clean no-op unless that variable
   is set.

`cargo test --locked` (the gate) issues no provider call. The whole `live_harness`
tree is `#[cfg(test)]`.

**Live traffic consumes real subscription rate limits.** A campaign runs real
requests against your Claude Code / Codex subscription. Start pilot-sized:
`pilot-a` is one lane, low effort, n=2, three small cells. Widen the matrix only
after a pilot has validated plumbing and cost against your account.

Runs execute sequentially (one request in flight at a time) to stay rate-limit
friendly. An unavailable lane is skipped with a reason, never a panic; a run that
errors is recorded as an exclusion, never a fabricated number.

## Credential prerequisites

Availability is discovered at runtime per lane (no network call). A lane whose
credential is absent is skipped and its skip reason is printed; the campaign
continues.

- **`anthropic` lane** needs the Claude Code OAuth credential. Run
  `iris login anthropic` (or let Iris bootstrap from
  `~/.claude/.credentials.json`). This is the only lane that reports cache writes
  (with the 5m/1h split) and provider-native compaction.
- **`codex` lane** needs the OpenAI Codex OAuth credential. Run
  `iris login openai-codex`. Since PR #557 the Codex adapter reports cache
  writes; it has no native-compaction rung.

Full credential setup lives in the project README ("Credentials and provider
selection") and is not duplicated here.

## Quickstart

1. Authenticate the lane you want (see above).
2. Copy the pilot config and, if benching a different model, change the lane
   `model` (and `provider`/`effort` as needed):

   ```bash
   cp docs/benchmarks/campaigns/pilot-a.toml docs/benchmarks/campaigns/my-run.toml
   # edit [[lanes]].model, add [prices.<model-id>] if it is not built in
   ```

3. Run it (opt-in, consumes rate limits):

   ```bash
   IRIS_BENCH_LIVE=1 \
   IRIS_BENCH_CAMPAIGN_FILE=docs/benchmarks/campaigns/my-run.toml \
     cargo test --release -- --ignored live_campaign --nocapture
   ```

Select a campaign with exactly one of:

- `IRIS_BENCH_CAMPAIGN=<name>` -- a built-in campaign (`pilot-a`).
- `IRIS_BENCH_CAMPAIGN_FILE=<path>` -- a config-file campaign.

Setting both, or neither, is a named error, not a silent default.

## Config schema reference

Versioned TOML (`schema = 1`). Validation is a system boundary: every rejection
names the field, the offending value, and the accepted range or set. An unknown
key is a parse error, not a silent ignore.

### `[campaign]`

| field | type | default | notes |
| --- | --- | --- | --- |
| `name` | string | (required) | non-empty; names the artifact folder |
| `runs` | integer | `2` | runs per cell; `>= 1` |
| `exclusion_budget` | integer | `1` | flaky/error exclusions tolerated before the verdict fails |

### `[[lanes]]` (>= 1)

| field | value | notes |
| --- | --- | --- |
| `provider` | `anthropic` \| `codex` | unknown providers are rejected, listing the set |
| `model` | any string | passed verbatim to the provider constructor |
| `effort` | `low` \| `medium` \| `high` | reasoning effort |

### `[[cells]]` (>= 1)

`scenario` is required; every other field is an optional override. Knob overrides
resize the scenario; settings overrides change the compaction posture (layered on
the shipped `CellSettings::defaults()`).

| field | type | default | accepted range |
| --- | --- | --- | --- |
| `scenario` | string | (required) | `S1` \| `S2` \| `S3` \| `S4` \| `S4-small` |
| `budget` | integer | scenario default | `>= 8192` |
| `round_trips` | integer | scenario default | `1..=16` (S1) |
| `seed_repeat` | integer | scenario default | `>= 1` (S1) |
| `result_repeat` | integer | scenario default | `>= 1` (S1, S3) |
| `start` | float | `0.72` | `0.1..=0.95` |
| `hard` | float | `0.90` | `start < hard <= 0.99` |
| `keep_tail_tokens` | integer | `8000` | `>= 1` |
| `hard_wait_ms` | integer | `120000` | (shipped `DEFAULT_COMPACTION_HARD_WAIT_MS`) |
| `summarizer` | string | `subagent` | `subagent` \| `provider` \| `excerpts` |
| `retention` | string | `5m` | `5m` \| `1h` |

Knobs only apply where a scenario carries them (listed above); a knob a scenario
does not use is ignored by that scenario.

### `[prices.<model-id>]` (optional)

Extends the built-in notional price table by model id. A model with no price
entry is not an error: its `notional_usd` is reported as `null` with a report
note. Subscription lanes bill against rate limits, not dollars -- these numbers
are a single comparable optimization score, not a billing figure.

| field | type | notes |
| --- | --- | --- |
| `input` | float | USD per million fresh-input tokens; `>= 0` |
| `output` | float | USD per million output tokens; `>= 0` |
| `cache_read` | float | USD per million cache-read tokens; `>= 0` |
| `cache_write` | float | USD per million cache-write tokens; `>= 0` |
| `as_of` | string | optional; advisory date |

## Scenario catalog

Each scenario declares the compaction posture that isolates its behavior and
carries its own fail-loud success criteria where relevant.

- **S1 aggressive-fill** -- one mega-turn that drives several sequential
  tool-call round-trips, crossing `start` then `hard` mid-turn so auto-compaction
  fires at a continuing boundary. Measures: mid-turn compaction under a runaway
  single turn (regression sentinel for #552). Knobs: `round_trips`, `seed_repeat`,
  `result_repeat`, `budget`. Use when tuning when/whether a long turn compacts.
  Fail-loud: a run that observes < 3 boundaries or never compacts is a Fail
  (`S1 produced no compaction`), not a green pass.
- **S2 multi-turn grind** -- n tool-heavy user turns applying steady pressure
  that crosses the tiers over the run. Measures: compaction cadence across turns.
  Knobs: `budget`. Use for steady-state cadence.
- **S3 fold-dominant** -- many superseded reads of one target, run with
  auto-compaction OFF so microcompaction (folds) is isolated. Measures: fold
  reclamation before compaction. Knobs: `result_repeat`, `budget`. Use to study
  folds without compaction noise.
- **S4 cache-churn** (`S4-small` is the small pilot) -- alternating hot-prefix
  and forced-churn turns. Measures: the cache break-even of an apply. Knobs:
  `budget`. Use for cache-economics / break-even studies.

## Reading the artifacts

A run writes into `docs/benchmarks/campaigns/<name>/<date>/`:

- `<name>.jsonl` -- one `Row` per provider request.
- `<name>.md` -- verdict, per-cell headline table, scenario failures, row-schema
  reference.
- `<name>.manifest` -- completed run keys (resume bookkeeping).

### Row columns (`metrics.rs::Row`)

| column | meaning |
| --- | --- |
| `campaign`, `cell_id`, `lane`, `scenario` | provenance; `cell_id` groups a cell's runs and is stable across campaigns for diffing |
| `run_seq`, `request_seq` | run index and request index within the run |
| `kind` | `turn` \| `summary` \| `native_compact` \| `probe` |
| `ts`, `wall_ms` | request timestamp and the gap to the next request |
| `input_tokens`, `output_tokens` | realized provider usage |
| `cache_read` | cache-read input tokens |
| `cache_write_5m`, `cache_write_1h` | Anthropic's write split; on the Codex lane the flat write sits in `5m` with `1h = 0`, or both are `null` when blind |
| `write_unreported` | `true` only on a Codex row that reported a **zero** cache write. Since #557 Codex reports writes, so a nonzero write sets this `false` and preserves the write. Residual ambiguity: a Codex zero-write cannot distinguish "wrote nothing" from "the endpoint did not surface a write", so it is conservatively flagged. The Anthropic lane is never write-blind. |
| `context_measured_tokens` | provider-reported input for this request |
| `context_estimate_tokens` | the pure estimator's count of this request's exact payload |
| `estimate_error` | `measured - estimate`, per request. **Diagnostic only** -- catches estimator drift; never a reported metric. A small consistent drift is the honest residual (estimator does not count system-prompt/tool-schema/framing mass). |
| `boundary_index`, `tier` | applies seen so far; pressure tier (`none`/`warn`/`start`/`hard`) in effect |
| `lifecycle` | compaction generation applied + origin, fold flushes + reclaimed estimate, breaker state -- counts/enums only, never folded content |
| `settings` | the cell's settings fingerprint, stamped on every row |
| `error` | verbatim provider error, or `null` |

### Report table and fail-loud

The `.md` report shows the verdict (flaky-exclusion rule #545), a per-cell
headline row (`requests`, `input`, `output`, `cache_read`, `notional_usd`,
`outcome`), and -- when present -- a `## Scenario failures` block. A run that
completed without a provider error but did not exercise its target behavior is a
**hard failure**, recorded verbatim, not a silent green. `notional_usd` prints
`null` for any lane whose model has no price.

## Resume semantics

The manifest records each completed run key and is persisted per run, so an
interrupted campaign resumes past finished runs instead of restarting. **To
re-run a campaign from scratch, archive (move/rename) its `.manifest`** in the
date folder; the next invocation sees no completed keys and runs the full plan.

## Tune your model: a walkthrough

Goal: pick `start`/`hard`/`keep_tail_tokens` for a model by reading the cache
break-even of a compaction apply.

1. **Establish the break-cycle.** Run an S1 or S4 cell and read the first request
   after a compaction boundary (`boundary_index` increments). An apply breaks the
   warm prefix: cache reads collapse and a re-write is charged at the write rate.
   Pilot-A run 3 (S1, boundary 4) is a concrete reading:
   - cache reads: `25.7k -> 2.2k` (the warm prefix is dropped)
   - re-write: `9.7-12.2k` at the `1.25x` write multiplier
   - fresh input: `25.7k -> 11.9k` on the next request
   - payback: roughly one request -- the input saved on the next request offsets
     the one-time re-write.
2. **Sweep the thresholds.** Copy the config into cells that vary one knob:
   - lower `start` -> compaction fires earlier, more often, more re-writes;
   - higher `hard` -> later, riskier deterministic backstop;
   - larger `keep_tail_tokens` -> more recent context survives each apply, so the
     post-apply payload (and its re-write) is larger but recall is better.
   Give each cell a distinct settings fingerprint so the rows group cleanly.
3. **Read the ledger per cell.** Compare `post_apply_rewrite_mass`,
   `cache_hit_ratio`, and `notional_usd` across cells. A threshold that pays back
   within ~1 request and keeps the hit ratio high without churning re-writes is
   the target.
4. **Pick and pin.** Set the winning `start`/`hard`/`keep_tail_tokens` in your
   config, re-run n>=2 for stability, and keep the artifact folder as the record.

The `estimate_error` column is your sanity check throughout: if it grows beyond a
small consistent drift, the estimator is diverging from the provider and any
threshold tuned against the estimator alone will fire late live (pilot-A finding
2 root-caused exactly this).
