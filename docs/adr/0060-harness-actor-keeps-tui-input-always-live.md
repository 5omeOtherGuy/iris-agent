# ADR-0060: Own turns in a harness actor so TUI input stays always live

**Date**: 2026-07-12
**Status**: proposed
**Deciders**: Iris maintainers, Pi agent session

## Context

Users must be able to open slash commands and the `/settings` faceplate at all
times: idle, provider streaming, tool execution, approval review, and compaction.
This is tracked by issue #594. The current TUI cannot do that cleanly because
`run_harness_op` polls `Harness::submit_turn` inline and holds an exclusive
`&mut Harness` for the whole operation. While that borrow is live, the input
loop cannot also run the normal modal/settings action path that needs the
harness.

That has produced two UI modes:

- idle/modal mode, where commands can mutate harness state;
- running mode, where a second key handler keeps only selected controls live,
  makes dropdowns read-only, and defers `/settings` until the turn ends.

The split makes `src/ui/tui_loop.rs` heavy and makes always-live controls a
bolt-on. It also diverges from both references:

- Codex keeps the TUI loop separate from agent execution. `App::run` selects over
  terminal events, app events, active-thread events, and app-server events; the
  UI never drives the model loop inline.
- pi-mono keeps one input path and treats `isStreaming` as state, not as a
  separate mode. Mid-stream input is routed as steering/follow-up or immediate UI
  actions.

ADR-0006 still applies: do not vendor Codex's TUI stack or app-server product
surface. Borrow the actor/event-loop shape, implemented in Iris terms.

## Decision

Move turn execution behind a Tier-3/Tier-2 harness actor boundary. The TUI owns
terminal input, focus, overlays, and rendering. The harness actor owns the
`Harness`, provider turn execution, compaction execution, approval request
parking, and safe-boundary application of harness-bound changes.

The TUI loop becomes one event loop over:

- terminal events;
- render ticks;
- harness actor events;
- UI-internal events such as delayed redraws and background git-status refreshes.

The TUI must not await `Harness::submit_turn` or `Harness::compact_now*` directly
while routing terminal input. It sends commands to the actor and renders the
actor's typed events.

### Actor commands

Introduce an internal command enum in the TUI/session-driver layer. Names may
change during implementation, but the contract should cover these shapes:

- `SubmitTurn { text }` — start a user turn when no turn is active.
- `RequestCompaction { focus }` — start manual compaction when safe, or report
  why it cannot start.
- `CancelActive` — cancel the active turn/compaction token.
- `Approve { decision }` — answer the current approval request.
- `ApplySettings { action, origin }` — apply a settings/model/login/task action
  now if safe, or record it as pending.
- `QueueSteering { text, mode }` — deliver steering/follow-up while a turn is
  active.
- `RefreshUiState` — ask the actor for selection, settings, task, permission,
  and compaction state needed by overlays.
- `Shutdown` — stop the actor after the UI has decided to exit.

Approval decisions remain Nexus policy decisions. The actor only parks the
request and forwards the user's answer through the existing `ApprovalGate` seam.

### Actor events

The actor emits typed events back to the TUI:

- existing `UiEvent` values mapped from Nexus `AgentEvent`;
- `TurnStarted`, `TurnFinished`, `TurnFailed`, `CompactionStarted`,
  `CompactionFinished`;
- `ApprovalRequested { offered_decisions, call, reason }` and
  `ApprovalCleared`;
- `SettingsApplied { lines }`;
- `SettingsQueued { label, reason }`;
- `PendingSettingsApplied { labels, lines }`;
- `ActorState { active_kind, selection, queued_counts, permission_mode,
  compaction_state, task_state }`.

The TUI may collapse these into existing screen methods. The important rule is
ownership: runtime facts come from the actor; the TUI does not fabricate harness
state.

### Settings semantics

Settings and slash commands are always navigable. Effects fall into three
classes:

1. **UI-only effects** apply immediately in the TUI: palette focus, modal
   navigation, folds, transcript focus, mouse mode, visual readouts, and other
   presentation state.
2. **Actor-safe effects** apply immediately through `ApplySettings` if they do
   not invalidate the in-flight provider/tool operation. Examples include global
   config writes that only affect future turns and safe readout refreshes.
3. **Safe-boundary effects** are accepted, displayed as pending, and applied by
   the actor when the active turn/compaction reaches a boundary. Examples include
   provider/model/reasoning switches, login/logout changes that require provider
   rebuilds, session resume/new-session actions, task settlement, and mutation
   safety changes that would invalidate an in-flight tool decision.

The settings faceplate must show queued status next to pending rows. When the
boundary lands, the actor applies pending changes in input order, emits the same
notice/audit lines the idle path emits today, and refreshes `ActorState`.

### Input and focus rules

There is one key-routing path. It branches on current focus and actor state,
not on idle-vs-running phases.

- Modal and palette focus win over editor focus.
- Approval keys win when an approval is pending, but non-conflicting scroll,
  transcript focus, palette/settings navigation, and read-only inspection remain
  live.
- Enter in the editor submits a new turn when idle, queues steering when a turn
  is active, and queues follow-up on the existing follow-up chord.
- `/settings`, the settings shortcut, and the start-page settings action open the
  same faceplate even while a turn is active. They must not enqueue model
  steering.
- Escape closes the focused overlay first, denies approval if approval owns the
  key, and only then cancels the active turn.

### Implementation boundaries

Keep dependency direction unchanged:

- Nexus remains UI-neutral and keeps approval/tool enforcement.
- Wayland keeps owning the harness, session, compaction, settings application,
  and execution environment.
- Iris TUI owns terminal input/rendering and the actor adapter.

Prefer a local actor over a process or network app server. Codex's app-server is
the reference shape, not a dependency or copied subsystem. The first slice should
be in-process Tokio channels on the current-thread runtime, using local tasks
where `!Send` state requires them.

## Alternatives Considered

### Keep the inline `&mut Harness` turn future and add a live settings overlay
- **Pros**: Smaller initial patch.
- **Cons**: Harness-mutating actions still cannot run while the turn future holds
  the borrow; the UI keeps two input modes and more deferred special cases.
- **Why not**: It preserves the cause of the bug.

### Use `Rc<RefCell<Harness>>` and borrow around each mutation
- **Pros**: Closer to pi-mono's single-threaded mutable-session model; smaller
  than an actor if all awaits are carefully split.
- **Cons**: Runtime borrow failures become possible; every harness method must be
  audited to avoid holding a borrow across `.await`; command ordering and pending
  settings are implicit.
- **Why not**: Rust gives us typed channels and explicit ownership. Use them.

### Copy Codex's app-server stack
- **Pros**: Mature event-loop shape, request/notification model, and many solved
  edge cases.
- **Cons**: Product-specific protocol, cloud/server/plugin assumptions, and more
  code than Iris needs. Reference repos are for consultation, not vendoring.
- **Why not**: Iris needs Codex's actor boundary, not Codex's app server.

### Defer all settings until turns end, but open the panel read-only
- **Pros**: Very small behavior change.
- **Cons**: The user can view but not adjust settings during the agent loop;
  conflicts with the product requirement.
- **Why not**: Always accessible means actionable where possible, not read-only.

## Consequences

### Positive

- Slash palette and settings faceplate stay available throughout agent work.
- `tui_loop.rs` can collapse toward one event loop and one input path.
- Harness ownership becomes explicit: one actor serializes runtime mutations.
- Safe-boundary behavior is visible and testable instead of scattered through
  running-mode exceptions.
- The design aligns with Codex's Rust event-loop architecture while preserving
  Iris's tier split.

### Negative

- The first refactor has a larger blast radius than changing `/settings` alone.
- Actor command/event contracts add surface area.
- Tests must move from direct phase-function assertions toward actor-loop
  behavior tests.

### Risks

- Event ordering regressions can corrupt transcript/approval display. Mitigate
  with deterministic channel tests for provider events, approval requests,
  cancellation, and settings application.
- Pending settings can surprise users if not shown clearly. Mitigate by rendering
  pending status in the faceplate and emitting boundary-apply notices.
- Cancellation could leave an approval reply parked. Mitigate by making
  `CancelActive` deny any parked approval before cancelling, matching today's
  behavior.
- A local actor can become a second god object. Mitigate by keeping the actor a
  small owner/dispatcher and leaving policy in Nexus/Wayland/settings modules.

## Definition of Done

- The TUI has one primary input/event loop for idle and active turns. There is no
  separate running key handler with a different slash/settings path.
- The UI loop no longer awaits `Harness::submit_turn` or manual compaction while
  holding `&mut Harness`; those operations are driven by the harness actor.
- `/settings` and the slash palette open during provider streaming, tool
  execution, compaction, and approval review.
- A settings change made during an active turn either applies immediately or is
  visibly queued and then applied automatically at the next safe boundary.
- Approval decisions still work, keep precedence over conflicting keys, and deny
  safely on cancellation/EOF.
- Ctrl-C/Escape cancellation still cancels active provider/tool work and leaves a
  valid transcript.
- Existing slash/model/reasoning/session/task/settings commands preserve their
  idle behavior.
- Automated tests cover: mid-turn settings open, mid-turn slash palette open,
  `/settings` not sent as steering, queued model/reasoning switch applied after a
  turn, immediate UI-only setting application, approval precedence while settings
  is open, cancellation with pending approval, and actor event ordering.
- `bash scripts/gate.sh` passes.
