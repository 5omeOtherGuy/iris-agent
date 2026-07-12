# Auto-compaction implementation notebook

Running record for the 2026-07-10 auto-compaction specification. Entries are
append-only by slice. PR descriptions remain the authoritative review summary;
this file keeps implementation issues and decisions that span slices.

## Completed slices

- Slice 0: engine extraction, worker usage/origin persistence, and the two-lane
  live-loop instrument merged in PR #523.
- Slice 1: model-aware window, hybrid measurement, trigger ladder, breaker,
  legacy mapping, and `/context` labels merged in PR #524.
- Slice 2: incremental provider-round-trip persistence and crash-mid-turn resume
  equivalence merged in PR #525.
- Slice 3: provider-neutral context governor, mid-turn apply, fold freeze, and
  active G1 timing merged in PR #526.
- Slice 4: transcript worker default, finite shrink-retry, dedicated worker
  routing, manual attach/focus, and cache-hit reporting merged in PR #527.
  Both live lanes passed 10 sessions with no exclusions. Preliminary Codex
  probes hit `usage_limit_reached`; the quota later reopened and the full run
  passed.
- Slice 5: typed overflow classification, bounded reactive resend, deterministic
  recovery ladder, and induced-overflow live coverage merged in PR #530.
- Slice 6: durable compaction inspection, lifecycle/chip states, and the live TUI
  milestone merged in PR #534.
- Slice 7: portable provider-block persistence, Anthropic compact adapter, and
  probe-gated OpenAI v2 capability merged in PR #537.
- Slice 8: default-off model-requested compaction tool merged in PR #538.
- Slice 9: benchmark extensions and evidence-tuned trigger defaults merged in
  PR #539.

## Slice 5 — reactive recovery

Status: merged in PR #530.

Initial seam decision: Nexus owns a provider-neutral overflow/retry guard;
Wayland owns deterministic recovery and durable mutation; Mimir classifies wire
errors. A completed overflow with visible partial output is not resent because
that could duplicate user-visible content. It ends with the same bounded,
actionable error as a second overflow.

Issue: the first induced-overflow live attempt used a 20,000-token retained
tail against a ~21,798-token seed. No pair-safe prefix remained coverable, so
both lanes correctly returned the honest error without resending. Decision:
the induced variant uses the manual 1,000-token keep target. This changes only
the measuring instrument and guarantees the variant exercises deterministic
rewrite plus one real resend.

Issue: the deep-cut regression fixture made the nominally retained message too
large for the first keep target, so the first excerpts pass consumed it and no
second durable range remained. The fixture now places that message between the
3,000- and 1,000-token targets; it proves two ordered excerpt applies.

Decision: context overflow after any visible assistant text, reasoning, or tool
call is not resent. Replaying could duplicate user-visible output or effects.
The runtime returns the same measured, actionable error used for a second
overflow.

Measurement follow-up for slice 9: live tables currently report maximum
post-apply context divided by the start threshold, not actual pre-apply size.
Record pre-apply tokens and reclaimed ratios before default retuning. The
normal path deliberately retains a hot tail and stops below `start`; reactive
recovery escalates to a 1,000-token tail only while still hard.

Verification: the deterministic gate passed. The induced-overflow pair passed
on both lanes with one injected overflow, one durable excerpts apply, one
resend, a real read/tool-result round trip, complete metadata, and byte-exact
resume. The normal Haiku loop passed 10/10 sessions with 23 compactions and no
exclusions. The normal Codex loop passed all nine evaluated sessions with 18
compactions and one permitted exclusion recorded verbatim:
`provider stream produced no events for 90s`. All evaluated rows passed G1–G5
and real-read checks. Worst G1 was 3.5 ms on Haiku and 22.4 ms on Codex.

Notable live issue: Codex session 02 spent about 25 minutes in a real provider
path before the worker surfaced its 90-second idle error. The process remained
alive with its network reactor active. This is end-to-end provider/worker
latency, not compaction main-loop blocking; no number was fabricated and the
session was the run's sole exclusion.

## Slice 6 — inspectability

Status: merged in PR #534.

Decision: `/compaction [n]` treats `n` as the durable 1-based generation
ordinal; omission selects the latest. The viewer derives covered message count
and original token mass from raw JSONL message rows, so it works after resume
without relying on process-local events. Missing legacy fields degrade to
explicit defaults; recall handles are extracted only from persisted markers.

Decision: the TUI viewer is one foldable transcript panel, not a modal or a new
first-class pane. The `compacting…` chip is muted volatile composer chrome. It
appears on `Running` and clears on `Ready` or any terminal lifecycle state.

Issue: a frozen fold can have zero estimated reclaimable tokens when its
deterministic recall stub is as large as a tiny result. The diagnostics report
the honest zero; only the frozen count is an invariant.

Live TUI milestone: passed on `anthropic/claude-haiku-4-5` with the compaction
worker pinned to `anthropic/claude-opus-4-6` at medium reasoning. A real `read`
turn started the worker over 5 messages (~3,864 tokens). The composer stayed
usable and showed `compacting…`; the next short turn observed
Running -> Ready -> Applied, cleared the chip, and applied a 205-token summary.
`/compaction` displayed generation 1, entry/range metadata, `Cargo.toml` carry,
the recall handle, and worker usage (8,193 input, 176 output, 8,369 total).
Post-apply `/context` reported the job idle and ~3,864 -> ~205 summarized.

Observation: the TUI top rail continued to show the model catalog's 200k native
window while `/context` showed the 32,768-token effective override. These are
different measurements by design; the detailed diagnostic is authoritative for
compaction thresholds.

Issue after rebasing onto the v0.2.0 release: the full parallel test suite
starved two background worker threads past their 500 ms polling allowance and
scheduled a zero-hard-wait assertion past its 100 ms wall-clock allowance. All
three passed immediately in isolation. The tests now retain bounded deadlines
but allow 5 seconds for a worker scheduled under load and 2 seconds for the
zero-wait path; state, lifecycle ordering, cancellation, and deterministic
fallback assertions remain unchanged.

Notable repository event: PRs #522 and #529 landed during slice 5's live run.
Slice 5 rebased cleanly. The primary checkout remains intentionally dirty with
unrelated operator changes, so primary sync refuses; each later slice continues
from fetched `origin/main` in its own worktree.

## Slice 7 — provider-native route

Status: merged in PR #537.

Decision: provider blocks are additive continuity hints, never the portable
truth. Every native entry persists self-sufficient text, one opaque adapter
envelope, usage, and `providerNative` origin. Rebuild is byte-identical before
translation. Only Mimir decides whether the envelope matches the exact adapter
and model; a selection change discards an in-flight native job.

Decision: `compaction.providerNative` is global-only and defaults to `off`.
Explicit `auto` opts into capability-gated native compaction across both primary
and hard-tier fallback routes and emits a startup warning about model-switch
behavior. The legacy Anthropic compact field stays rejected so one reducer has
one control. Anthropic's adapter remains available to the live probe, but
advertises no native capability after the Claude Code OAuth lane returned
`400 invalid_request_error`; an explicit `auto` still selects the portable worker
without a known failed request. OpenAI advertises native support and caches
rejected models for the process.

Issue: the first real Anthropic probe panicked before the request completed:
`Cannot drop a runtime in a context where blocking is not allowed. This happens
when a runtime is dropped from within an asynchronous context.` The dedicated
native worker polled a blocking adapter inside a Tokio runtime. A regression
test now requires the provider future to run without a Tokio handle, and the
worker uses the runtime-free futures executor on its already dedicated OS
thread.

Live capability result: the corrected request reached Anthropic, but
`anthropic/claude-haiku-4-5` rejected compact. The lifecycle error was recorded
verbatim:
`background compaction failed; using deterministic fallback: Anthropic native
compaction request failed (status=400, error_type=invalid_request_error)`.
The same session applied excerpts, retained the needle, and rebuilt exactly.
This agrees with the public compact documentation's current supported-model
list, which does not include Haiku 4.5. The route remains implemented and
probe-gated but does not clear the slice's native-live exit criterion on this
lane; no success is fabricated.

Decision: the OpenAI v2 `compaction_trigger` shape is pinned by deterministic
request/response tests and independently double-gated real probes. The live
`openai-codex/gpt-5.4-mini` subscription backend returns exactly one encrypted
compaction block. Iris now obtains a separate OpenAI-authored portable summary,
combines usage, persists both forms, and advertises the route. The full native
lifecycle preserves the needle and rebuilds byte-exactly.

Measurement follow-up: the native test's first run is excluded because of the
runtime panic, and the corrected run is an explicit provider capability failure,
not a passing native row. The ordinary two-lane LVP remains the behavioral gate
for the portable path in this slice.

Ordinary-LVP issue: the first slice 7 Codex attempt returned
`Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini
error_type=usage_limit_reached]` for sessions 00 and 01. Two exclusions already
made the run ineligible, so the remaining eight redundant calls were cancelled.
This aborted attempt is recorded separately and will be regenerated after the
quota reopens; it contributes no metrics.

A one-session Codex retry after the deterministic gate returned the same
`usage_limit_reached` error before any evaluated session. It contributes no
metrics; the quota condition remains external and verbatim-recorded.

Portable-path Haiku result: 10/10 sessions passed with 22 real compactions and
no exclusions. Worst non-hard blocking was 16.2 ms; worst post-apply context was
17,103/21,299. G2–G5 and the real-read check passed in every session. Three
summary workers reported a 0.999 cache-read/input ratio and seven reported
0.000. Every worker was `anthropic/claude-opus-4-6` at medium thinking.

## Slice 8 — model tool

Status: merged in PR #538.

Decision: `request_compaction` is a concrete, default-off Tier-3 tool, not a
Nexus special case. `compaction.modelTool` is project-tunable because the tool
grants no provider, filesystem, shell, or approval capability. The production
tool registry appends it only when enabled, including in bash-tool mode.

Decision: the tool accepts exactly an empty object and returns:
`Compaction is scheduled for the next safe boundary; it has not happened yet.`
Extra or malformed arguments fail before the flag is set. The wording prevents
the model from assuming its current context was already rewritten.

Decision: the tool and engine share one session-local `Arc<AtomicBool>` owned
through `ToolState` and `CompactionEngine`. Tool execution only sets it. The
governor consumes it once at the next continuing, pair-closed boundary. This
adds no tool-name branch to Nexus and preserves parent-owned apply.

Edge case: a model request is independent of automatic pressure thresholds.
With `compaction.enabled=false`, the next governed boundary still runs the
normal configured compaction route. If the context is already at start/hard
pressure, the ordinary ladder owns the stronger action. If no pair-safe range
exists, Iris emits an honest notice and makes no entry.

Verification: focused tests prove default-off/project opt-in, registry
visibility in both tool modes, empty-argument validation, scheduled-not-done
output, no entry before the boundary, one durable apply at the boundary with
automatic thresholds off, and one-shot consumption. The standard LVP does not
enable or call this default-off tool, so live provider traffic is not applicable
for this slice; the final post-slice-9 protocols cover the unchanged default
path.

## Slice 9 — benchmark extension and default tuning

Status: merged in PR #539; final protocol closeout remains in progress.

Issue: live tables showed post-apply context near 17k after triggering near
20k, which looked like the summary worker removed little. The first hypothesis
was fixed prompt/tool overhead. Code tracing disproved it:
`context_tokens_after_apply` is message history only. Decision: reconstruct
true pre-apply context as `after + (covered original - summary)` and report both
covered-range and total-context reduction. This correction is now pinned by a
unit test and carried in every live row.

Finding: the provisional worker reduced one covered slice by 83.4%, but the
slice represented little of the total context. The shallowest live apply was
19,820 -> 18,319, only 7.6% total reclamation. Pair safety and the 20k retained
tail protected recent large tool-turn groups; the 0.65 start threshold launched
before the eligible prefix grew. The summary was not the limiting factor.

Decision: select 0.60/0.72/0.90 with an 8k retained tail. A deterministic
four-generation lane improved average total reduction from 48.5% to 58.3%,
improved the shallowest generation from 41.2% to 54.6%, used four generations
instead of six, and preserved the planted fact, recall-loop hit, and one marker
per generation. A live Haiku probe's shallowest apply was 30,428 -> 15,487:
49.1% total reclamation with two compactions and all gates passing. A 6k
candidate removed 51.5% but retained 1,678 fewer recent tokens; the small
additional reduction did not justify the smaller hot tail.

Issue: the first deterministic investigator arm silently fell back to excerpts
because the fake provider did not recognize the final embedded-transcript
request. The retention assertion still passed, invalidating the comparison.
Decision: fix the fake request shape and require `origin=subagent` for every
model-worker arm so fallback cannot masquerade as the intended arm.

Decision: extend cache accounting at the final LVP seam. Worker cache hit comes
from durable `workerUsage`. Anthropic parent amplification uses reported cache
writes around each apply. Codex does not report writes, so its separately
labeled metric is derived fresh input (`input - cache_read`); no write number is
fabricated.

Verification so far: deterministic worker, start/hard/reactive boundary, focus,
long-horizon, recall-loop, and cache-pairing tests pass. Focus retention was 0/5
control and 5/5 focused. Background transcript and investigator arms blocked
0.0 ms in the deterministic event timeline; the manual-await comparator blocked
38.8 ms.

Selected-default Haiku full protocol: 10/10 sessions, 23 compactions, no
exclusions, worst G1 16.7 ms, worst post/start 19,446/23,592, and all G2–G5 plus
real-read checks passed. The shallowest total-context reduction was 24.8% on a
third-generation apply; that apply still reduced its covered range by 91.7%.
Parent cache writes totaled 45,012 before versus 442,524 after paired applies
(9.831×). This is the cache rewrite cost and is intentionally reported beside,
not as, context reclamation.

Gate issue: the initial worker-arm fixture assumed a 40 ms tool delay was long
enough for the 20 ms fake worker to be scheduled. Under the full parallel suite,
the worker thread lost that race and no mid-turn apply occurred. Decision:
replace elapsed-time luck with a bounded worker-completion signal. The delay
tool yields until ready or five seconds; the apply-to-next-request measurement
still excludes tool time. The focused regression and the full 2,198-test gate
then passed.

Live issue: a selected-default Codex smoke produced no eligible measurement and
returned verbatim:
`Codex request failed [status=429 endpoint=/codex/responses model=gpt-5.4-mini
error_type=usage_limit_reached]`. It is not counted as a pass. The final two
full protocol pairs remain open until the external quota allows evaluated rows.

Follow-up issue after merge: the Codex quota reopened and the first evaluated
smoke applied once, passed every individual gate, but failed the protocol's
two-compaction minimum. The second Opus job was still active when the
instrument's fixed 30-second wait ended. This was an instrument timing
assumption, not a compaction failure: the apply was 29,484 -> 14,553 (50.6%
total reduction), G1 was 0.9 ms, and recall, carry, metadata, read, and exact
resume all passed.

Decision: keep sending bounded real-read, pair-closed boundaries while a job is
active, up to 14 filler turns, with five-second yields; allow one final bounded
60-second interval before the needle probe. The next Codex smoke forced two
compactions and passed: G1 29.6 ms, worst post/start 11,394/23,592, shallowest
26,263 -> 11,394 (56.6% total, 96.9% covered), worker hit 0.999, derived fresh
parent input 5,249 -> 22,158 (4.221×), and all G2–G5/read checks.

## Goal closeout

Status: complete on merged commit `f82fc7f`.

Two consecutive full protocol runs completed without an intervening code or
settings change. Run 1 passed 10/10 sessions on both lanes with zero exclusions:
Haiku applied 25 compactions with worst G1 18.5 ms; Codex applied 20 with worst
G1 24.6 ms. Run 2 passed Haiku 10/10 with 25 compactions and worst G1 19.9 ms.
Codex evaluated 9/9 after the single permitted flaky exclusion; the 18 counted
compactions had worst G1 88.8 ms. Every evaluated session passed G2–G5, real
read, planted-needle recall/carry, and byte-exact resume.

Excluded run-2 Codex session 08: G1 measured 1,540.6 ms while G2–G5/read all
passed. The other 38 closeout sessions were below 89 ms and the immediately
following session measured 12.9 ms. The harness error and raw row are preserved
verbatim in `docs/benchmarks/auto-compaction-live-loop.md`; the row is not
averaged into any run metric.

Final measurement lesson: the original 20k -> 17k observation conflated
post-apply size with reduction. True before size is reconstructed as
`after + original - summary`. Minimal whole-context reduction can still occur
on a later generation when pair-safe range selection exposes only a small
prefix: run-1 Haiku session 04 reduced that prefix by 83.3% but moved total
message context only 21,611 -> 20,118 (6.9%). The tuned default removes roughly
half the total context in the common two-compaction rows while preserving an 8k
hot tail; it does not promise every later generation will expose a large range.

All slices and follow-ups used per-task worktrees, green deterministic gates,
PRs, squash merges, and cleanup. Live summaries stayed pinned to
`anthropic/claude-opus-4-6` with medium thinking.
