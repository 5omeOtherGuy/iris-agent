# ADR-0028: Git workflow — dirty-tree safety, task checkpointing, and rollback semantics

**Date**: 2026-07-03
**Status**: accepted — amended by
[ADR-0030](0030-git-safety-task-ownership-lease-and-mutation-lock.md)
(recovery ownership: lease + mutation lock; auto-adopt superseded by explicit
adoption) and
[ADR-0031](0031-task-identity-session-linkage-and-resumable-tasks.md)
(task records carry opaque body + session links; `taskLifecycle` session
entries), and
[ADR-0052](0052-task-workflow-v2-opt-in-guard-and-integrated-settlement.md)
(always-on guard split from opt-in durable task workflow; added settlement
signals; checkpoint and cleanup semantics)
**Deciders**: operator + agent design review (epic [#261](https://github.com/5omeOtherGuy/iris-agent/issues/261))

## Context

Milestone 5 (Git-Centered Workflow) requires dirty-tree behavior, rollback
semantics, and approval requirements to be specified before any git automation
is implemented (roadmap gate). Epic #261 splits the work into dirty-tree safety
(#262), checkpoint/rollback (#263), final diff summary (#264), and the
verification loop (#265). This ADR records the settled design so later sessions
do not re-derive or diverge from it.

The central contract: **Iris must never silently damage or entangle a user's
uncommitted work.** Enforcement must be deterministic — runtime mechanisms, not
system-prompt instructions. The system prompt steers model behavior; it carries
zero enforcement weight.

## Decision

### Task boundaries are settlement-based, not message-based

- A **task starts** at the first mutating tool call after the previous
  settlement (or session start). Lazy capture: pure Q&A turns take no snapshot.
- A **task continues** across any number of turns, including follow-ups and
  corrections.
- A **task settles** on an explicit user action: accept/commit the final diff,
  roll it back, or an explicit checkpoint command. Settlement freezes the
  ledger; the next mutation opens a new task with a fresh baseline.
- Both boundary events are runtime-observable (a tool call arriving, a user
  action). The model never declares or decides task boundaries.

### Baseline and attribution ledger

- At task start the runtime captures a **baseline**: `git status --porcelain`
  plus content hashes of every dirty and untracked file, and the index state
  (`git ls-files --stage`).
- Every mutation Iris performs is recorded in a **ledger** with path and
  before/after hashes. "Iris-authored" = the set of paths whose changes trace
  to tool calls in the task. Rollback (#263) and the final diff (#264) scope to
  ledger paths only.
- **Ambiguity rule (TOCTOU):** any write that cannot be attributed with
  certainty (for example a user edit concurrent with a bash command) is
  attributed to the **user** and protected. Worst case is a spurious approval
  prompt, never lost work.

### Checkpoints are an op-log-shaped chain under `refs/iris/*`

- Checkpoints are real git commit objects anchored by refs in a hidden
  namespace (`refs/iris/checkpoints/<task-id>/...`), built with plumbing
  (`hash-object`, `write-tree` against a temporary index, `update-ref`). Never
  the user's index, `HEAD`, stash, or visible refs.
- Each checkpoint carries a tree snapshot, a parent pointer to the previous
  checkpoint, and metadata (turn, tool call, timestamp) — modeled on jj's
  operation log. Ledger and checkpoints are one structure.
- Ref-anchored means GC-safe and crash-safe by construction.
- Using git tree objects gives deletes, renames, mode changes, and binary
  files correct restore semantics for free. Rollback = materialize ledger
  paths from a checkpoint tree; user paths are never touched.
- **Unsettled diff + new mutating work** auto-checkpoints silently; rollback
  offers the chain as multiple restore points. Amended by ADR-0052: settlement
  destroys checkpoint refs once the durable task record is removed.
- **Non-git directories:** degrade to plain content snapshots of protected
  files in the session directory, with documented reduced guarantees. The
  feature announces itself as degraded rather than pretending.

### The index is protected state

A user's staged-but-uncommitted selection is expensive-to-recreate work.
Capture `git ls-files --stage` at baseline, detect index mutations in the
post-command check, restore staging state on rollback. Iris-authored index
changes (future approved auto-stage features) go in the ledger like file
changes. If restoration is unsafe in exotic states (mid-merge, mid-rebase),
degrade to detect-and-warn for those states and say so.

### Enforcement points

- **edit/write path — deterministic prevention.** The mutation choke point
  (tool registry + approval gate) checks: is this path in the dirty baseline
  and not yet approved this task? If yes, route through the approval prompt.
  The model cannot bypass this; it is the same tier as workspace path safety.
- **bash path — deterministic detection plus policy gating.** Commands are not
  classified as mutating/non-mutating (any binary can write; classification is
  only an optimization). Instead:
  1. Snapshot protected-file contents before execution (recovery guaranteed).
  2. Policy-gate recoverability-destroying commands (`git reset --hard`,
     `git checkout -- ...`, `git clean`, `rm -rf`, …) via the shell approval
     policy.
  3. After every command, re-hash the protected set (milliseconds; N known
     files). On violation: halt the loop, surface it, offer one-keystroke
     restore from snapshot.
  4. Diff `git status` before/after to attribute other changes to the ledger.
- **OS sandboxing** (Landlock/Seatbelt, #253) later upgrades bash from
  detection to prevention; detect-and-restore remains the fallback.

### Approval granularity

Per-file, per-task: the first touch of each dirty file prompts once; approval
covers that file until settlement. The prompt offers an explicit escalation
("all dirty files this task"). The `/trust` project policy (#260, ADR-0027)
sets the ceiling (always-ask / never-allow); the runtime prompt operates under
it. Approvals expire at settlement with the baseline they were judged against.

### Performance policy

- The protected-set hash check **always blocks** — it is the safety contract
  and is cheap by construction.
- The attribution `git status` scan runs **async**, with hard sync barriers at
  every ledger-consuming point: settlement, rollback, final diff, and the start
  of the next mutating tool call.
- Above a repo-size threshold, recommend enabling git fsmonitor once (with the
  exact command); never require it, never silently skip the scan. Degrade mode
  is slower-but-correct with a stated reason.

### Session end, resume, and crash recovery

- Session end does **not** settle a task. Passive actions never make safety
  decisions.
- **Amended by ADR-0030/0031:** recovery no longer auto-adopts when multiple
  records or a live foreign task exist; adoption is lease-aware and explicit.
  The reconciliation below (divergence snapshot, notice, expiry) is unchanged.
- On resume (or a new session in the same repo), apply the jj
  stale-working-copy pattern: compare the recorded op-log state against disk;
  if they diverged (crash, `^C`, external edits), synthesize a **recovery
  snapshot** of actual disk state and append it to the chain, then reconcile
  before offering rollback. Surface a one-line notice: unsettled diff from
  <when> — view / accept / roll back / ignore.
- Unsettled tasks untouched for an expiry window (default 30 days) auto-settle
  as **accepted** and their refs are GC'd: by then the changes are the user's
  de facto working state; expiring toward rollback would revert code the user
  has lived with.

### Guarantee tiers (advertising must match)

| Tier | Surface | Guarantee |
| --- | --- | --- |
| 1 | edit/write tools | Prevention. A dirty file is never modified without approval. Hard claim. |
| 2 | Foreground bash inside the repo | Recoverable, not untouchable: violations are detected after one command and restorable from snapshot. Residual gaps: attribution races with concurrent user edits; files the user creates mid-task before re-observation; background processes writing after the post-command scan. |
| 3 | Outside the fence | No guarantee until sandboxing (#253): writes outside the repo, symlinks out of the workspace, `.git` internals, hook execution. |

The advertised claim is: "Iris never **silently** loses your uncommitted work:
every touch of a pre-existing change is either blocked, explicitly approved,
or restorable from a snapshot." Never claim "Iris cannot touch your files."
User-facing docs derive from this table, not from aspiration.

### Interop note

If the workspace contains `.jj/`, jj owns the working-copy lifecycle
(auto-snapshotting, detached HEAD, ignored index); Iris git automation must
detect this and constrain itself. Details are settled in #262 implementation.

## Alternatives Considered

### Alternative 1: Adopt jj (`jj-lib`) as the checkpoint/undo engine
- **Pros**: Working-copy-as-commit, operation log, and undo are already built,
  battle-tested, and in Rust (Apache-2.0). Exactly the op-log shape this ADR
  adopts.
- **Cons**: jj explicitly ignores the git staging area (this ADR protects it);
  colocated mode detaches HEAD and requires a `.jj/` directory in the user's
  repo — the safety layer would itself entangle the user's environment; no
  submodule/`.gitattributes`/hook support; known jj↔git interleaving bugs.
- **Why not**: jj assumes it owns the repo; Iris must be a guest in the user's
  repo. We take jj's architecture (op-log, snapshot-on-operation, stale
  working-copy recovery) and build the minimal Iris-scoped version on plain
  git plumbing.

### Alternative 2: Dangling commits (`git stash create` style) for checkpoints
- **Pros**: Zero ref footprint, invisible to all tooling.
- **Cons**: Dangling objects are GC-bait; `git gc` can reap a checkpoint
  mid-task or before a delayed rollback — losing the safety net exactly when
  it is needed, unexplainably.
- **Why not**: The dominant failure mode is checkpoint loss; ref-anchoring
  eliminates it for the price of a namespaced, self-cleaning ref area.

### Alternative 3: Plain file-copy snapshots (no git involvement)
- **Pros**: Works everywhere including non-git dirs; zero `.git` footprint.
- **Cons**: Hand-rolls what git tree objects already do (dedup, atomicity,
  renames, modes, binaries); violates reuse-before-handroll; drifts from
  git-centered workflow as the spine.
- **Why not**: Kept only as the degraded fallback for non-git directories.

### Alternative 4: Per-turn or semantic ("what the user asked") task boundaries
- **Pros**: Per-turn is trivially mechanical; semantic matches user intuition.
- **Cons**: Per-turn misattributes Iris's own turn-1 edits as user work in
  turn 2. Semantic boundaries require model judgment — enforcement via prompt
  and hope.
- **Why not**: Settlement boundaries are the only definition that is both
  mechanical (runtime-observable events) and attribution-correct across
  multi-turn work.

### Alternative 5: Auto-settle on new request or on session exit
- **Pros**: No unsettled-state bookkeeping.
- **Cons**: Converts ambiguous or passive signals (a follow-up message,
  closing a terminal) into irreversible safety decisions; guessing wrong
  forfeits rollback.
- **Why not**: Auto-checkpointing accumulates restore points and defers the
  decision to rollback time, when the user actually knows what they want.

## Consequences

### Positive
- The safety contract is deterministic end-to-end; no load-bearing prompt.
- One op-log structure serves attribution, checkpointing, rollback, the final
  diff, and crash recovery.
- Index protection and honest tiered guarantees differentiate Iris from
  agents that ignore both.
- Every #262/#263 design question needed to start implementation is decided.

### Negative
- `refs/iris/*` writes into the user's `.git` (bounded, namespaced,
  self-cleaning — but visible to ref-enumerating tools).
- Post-bash attribution adds machinery (async scan + sync barriers) whose
  correctness needs careful tests.
- Tier 2/3 gaps remain until OS sandboxing (#253).

### Risks
- Monorepo `git status` latency if fsmonitor guidance is ignored — mitigated
  by the blocking/async split.
- Index restoration in exotic repo states (mid-merge/rebase) may need the
  detect-and-warn degrade path more often than hoped.
- Ref accumulation if settlement cleanup has bugs — needs a test and an expiry
  sweep.
