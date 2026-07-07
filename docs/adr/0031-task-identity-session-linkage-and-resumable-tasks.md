# ADR-0031: Task identity — opaque body, session linkage, and explicit task resumption

**Date**: 2026-07-03
**Status**: accepted — amended by
[ADR-0052](0052-task-workflow-v2-opt-in-guard-and-integrated-settlement.md)
(task identity/session linkage belongs to the opt-in durable workflow; the
always-on dirty-tree guard is not itself a resumable task)
**Deciders**: operator + agent design review (epic [#286](https://github.com/5omeOtherGuy/iris-agent/issues/286))

## Context

ADR-0028 kept task records headless: no description, no session linkage.
That was right for enforcement — boundaries stay runtime-observable and the
model never declares them — but it makes recovery unactionable for humans. A
resume-task picker (ADR-0030 replaces auto-adopt with explicit selection) needs
labeled rows; a session picking up an orphaned task needs to know what the task
was; nothing joins a task to the sessions that worked on it once the record is
deleted at settlement.

Sessions (`src/session.rs`, JSONL per cwd-slug) and git-safety tasks
(`src/wayland/git_safety/`) are both Wayland-owned but fully decoupled today.

## Decision

Promote the task's **metadata**, not the task itself. The task stays the
ADR-0028 safety artifact; there is no parallel "work unit" subsystem.

### Opaque payload on the task record

`PersistedTask` gains serde-default fields `body: Option<String>` and
`sessions: Vec<String>`.

**Invariant: enforcement never reads them.** `body` and `sessions` are
pass-through display payload; no git-safety code path may parse or branch on
them. Gating, checkpointing, rollback, recovery classification, and expiry are
unchanged. One record, one atomic temp+rename write — no second metadata store.

### The seam: harness-owned concrete methods, Tier-1 contract unchanged

The Tier-1 `MutationGuard` trait does not grow an intent parameter. The Wayland
`Harness` owns the concrete `GitSafety` and sees the user prompt, so the
plumbing is Wayland-internal:

- `GitSafety::set_turn_context(preview)` — called by the harness before each
  turn; `note_mutation` stamps it as `body` if it opens a task, clears it
  otherwise.
- `GitSafety::set_session_id(id)` — stamped on open; rehydrate/adopt appends it
  (ordered, consecutive-deduped) under the ADR-0030 mutation lock.
- `GitSafety::current_task_id()` — polled by the harness post-turn to observe
  "task opened this turn"; settle paths return the settled task id.

### Body policy

`body` is the prompt preview (`preview_line()`) of the turn whose first
mutation opened the task. Deterministic, captured once, never rewritten.
Follow-up turns joining an unsettled task leave it unchanged; a follow-up after
settlement opens a fresh task and captures that turn's prompt. No accumulated
running note. No model-generated titles (see Alternatives).

### Session linkage: two directions, two lifetimes

- Task record `sessions` vec — the **live join**. Exists only while unsettled
  (settlement deletes the record). The only linkage input recovery UX may use.
- Session-log `taskLifecycle` entries — `TaskOpened{task_id, body}` /
  `TaskSettled{task_id, disposition}` — the **historical audit**, the only
  place the join survives settlement. Modeled on `modelSelection`: append-only,
  in the leaf chain (`scan_for_resume` must chain through it), skipped by
  context rebuild (`read_messages`), never provider-visible.

**Consistency rule: enforcement and recovery consult only the task record +
lease. The session log is never an enforcement or recovery input.** Every
crash skew is then benign by construction:

| Crash between | Result | Rule |
| --- | --- | --- |
| record saved, before `TaskOpened` append | picker works; session view misses one event | record is authoritative |
| `TaskOpened` appended, before record save | dangling audit event | no record ⇒ display "settled or expired" |
| record removed at settle, before `TaskSettled` append | task gone from picker; session shows open-without-settle | same display rule |

No repair pass. Expiry removes record + refs and leaves session logs untouched.

### Resume-task picker and adoption semantics

Tier 3 renders a picker over ADR-0030's `recoverable_tasks()` (body preview,
relative age, workspace, linked-session count); selection calls
`adopt_task(task_id)` at the safe inter-turn boundary. Policy: zero recoverable
⇒ nothing; exactly one lease-free ⇒ auto-adopt + notice; more than one or
legacy ⇒ picker.

**Adopting a task never implicitly resumes a session.** Adoption rehydrates
the checkpoint chain and shows body + linked sessions. If exactly one linked
session exists, "also resume its session" is offered as an explicit second
action; multiple linked sessions are never guessed between; zero (legacy)
adopts with "(no description recorded)".

### Session lookup, v1 deterministic

`SessionStore::sessions_for_task(cwd, task_id)` scans the cwd-slug directory
for `taskLifecycle` entries — bounded by directory, no index. Read-back is
deterministic extraction (header, user-message previews, lifecycle events).
Surfaced as slash commands first; a model-facing tool is a follow-up slice and
must return bounded excerpts with full transcripts offloaded through the
handle store. Deferred to Milestone 4: subagent-backed summarization. Out of
scope permanently: cross-project search and a query DSL.

### Relation to ADR-0028

Amends ADR-0028: records carry opaque display payload; a new session entry
kind exists; auto-adopt-on-resume is superseded (jointly with ADR-0030) by
lease-aware explicit adoption. Unchanged: settlement boundaries, baseline and
attribution rules, TOCTOU attribution, checkpoint chain, index protection,
expiry, guarantee tiers.

## Alternatives Considered

### Alternative 1: Separate Wayland-side task-metadata store keyed by task id
- **Pros**: git-safety records stay byte-identical to today's.
- **Cons**: Two files per task create a crash-consistency problem beside a
  subsystem whose job is crash consistency; every skew needs reconciliation.
- **Why not**: Serde-default fields on one atomically-written record get the
  same purity via the never-read-by-enforcement invariant, with no second file.

### Alternative 2: Pass intent through the `MutationGuard` trait
- **Pros**: No new concrete methods; works for any guard impl.
- **Cons**: Leaks user-prompt semantics into the Tier-1 contract; every future
  guard must carry an intent string it does not need.
- **Why not**: The intent lives in the same tier as `GitSafety` (the harness
  owns both); a concrete Wayland-internal method needs no contract change.

### Alternative 3: Model-generated task body or title
- **Pros**: Nicer picker rows.
- **Cons**: The only nondeterministic write near the safety subsystem; a
  background provider call and failure mode for one line of UI; the prompt
  preview already exists and is proven in the resume-session picker.
- **Why not**: Cut. Revisit at Milestone 4 as display-time enrichment only,
  never a task-record write.

### Alternative 4: Adopting a task auto-resumes its linked session
- **Pros**: One gesture restores full context.
- **Cons**: Couples a deterministic safety operation to a session swap
  (provider rebuild, screen reset, cache re-key); a load failure strands a
  half-adopted task; multiple linked sessions have no unambiguous choice — the
  newest is often the crash-recovery session, not the intent-bearing one.
- **Why not**: Explicit second action for the single-session case; never guess.

### Alternative 5: Port pi-mmr's ampi-history (index, query DSL, redaction, model reader)
- **Pros**: Feature-complete history search exists as reference code.
- **Cons**: Built for cross-project global lookup; Iris's `SessionStore` +
  cwd-slug layout already provide the scoped 80%; the rest is machinery Iris
  does not need.
- **Why not**: Reuse the local store; keep v1 deterministic.

## Consequences

### Positive
- Recoverable tasks are labeled, attributable to sessions, and explicitly
  adoptable; the safety artifact's semantics are untouched.
- All new fields and entry kinds are additive and serde-default/skip-tolerant:
  old records and old transcripts read cleanly.
- The picker shape carries `workspace`, so worktree isolation (#267/#271)
  multiplying concurrent tasks needs no schema change.

### Negative
- One more session entry kind that `scan_for_resume` and `read_messages` must
  handle; missing either corrupts resume chaining or leaks audit entries into
  provider context (covered by tests in #287).
- The harness gains per-turn bookkeeping (turn context, post-turn task-id poll).

### Risks
- `sessions_for_task` scans a directory's files; pathological session counts in
  one cwd could make it slow — acceptable at local scale, and bounded by the
  cwd slug; an index is explicitly not built until proven necessary.
- Body previews may contain sensitive prompt text persisted into
  `<git-dir>/iris/tasks/`; same trust domain as the transcript itself, but
  documentation must not claim the record is content-free anymore.
