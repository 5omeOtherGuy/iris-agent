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

## Slice 5 — reactive recovery

Status: ready to merge.

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
