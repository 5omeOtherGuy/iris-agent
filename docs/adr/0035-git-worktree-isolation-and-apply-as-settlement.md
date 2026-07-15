# ADR-0035: Git worktree isolation — Tier 0 of the ADR-0028 guarantee model, apply as guarded parent mutation

**Date**: 2026-07-03
**Status**: accepted — amended by
[ADR-0052](0052-task-workflow-v2-opt-in-guard-and-integrated-settlement.md)
(the future subagent feature must not be named `task`; task records are
per-worktree while checkpoint refs live in the shared common ref store), and
superseded in part by
[ADR-0063](0063-extract-subagent-runtime-and-centralize-worker-scheduling.md)
(the reusable crate owns generic scheduling/worktree infrastructure; managed
control and git-admin markers replace the child-tree marker; Btrfs, pooling,
restore, and best-of-N slices are activated)
**Deciders**: operator + agent design review (epic [#261](https://github.com/5omeOtherGuy/iris-agent/issues/261), issue [#267](https://github.com/5omeOtherGuy/iris-agent/issues/267))

## Context

Milestone 5 (Git-Centered Workflow) needs an isolation primitive: a way to run a
session, session fork, or subagent against a private working directory whose
edits never reach the user's checkout until an explicit apply. Grok Build ships
such a subsystem; its reverse-engineered surface is captured in
[`.iris-reference/grok-worktree-subsystem-spec.md`](../../.iris-reference/grok-worktree-subsystem-spec.md)
as a reference, not an Iris decision.

[ADR-0028](0028-git-workflow-dirty-tree-safety-and-task-checkpointing.md) already
settled the in-place safety model: baseline capture, an attribution ledger,
`refs/iris/*` checkpoints, index protection, per-file per-task approvals, and the
tiered guarantee table. This ADR must conform to ADR-0028, never contradict it.
It records the design for the worktree isolation slice so the implementation
(issue [#271](https://github.com/5omeOtherGuy/iris-agent/issues/271)) does not
re-derive or diverge from it. The dirty-tree machinery (#262),
checkpoint/rollback (#263), and final diff (#264) are reused rather than
duplicated.

Settled framing this ADR builds on (not re-derived here):

- **Isolation is Tier 0 of the ADR-0028 model.** A linked worktree leaves the
  parent tree untouched *by construction* until apply. Isolation and in-place
  safety compose; they are not alternatives. ADR-0028 Tiers 1-3 still govern the
  parent workspace at apply time.
- **Apply = guarded parent mutation; task settlement when durable workflow is on.**
  `apply` is always the first mutation of a parent-workspace task and routes
  through the same tool choke point, dirty-baseline check, and per-file per-task
  approvals as any edit (#262 machinery). When the opt-in durable task workflow
  is enabled, apply also settles the worktree task. This is the answer to the
  conflict question the Grok spec leaves open (spec section 10), amended by
  ADR-0052's workflow opt-in rule.
- **Final diff (#264) is the apply review artifact.** The net-diff engine takes a
  source-tree parameter so the worktree slice reuses it to render the child's
  changes for review.
- **Checkpoints work unchanged inside a worktree.** Linked worktrees share the
  object store and common `.git`; `refs/iris/*` chains and plumbing writes
  function identically from a worktree HEAD.
- **Task records and checkpoint refs have different boundaries.** The unsettled
  task record is stored under the current worktree's absolute git dir. In a
  linked worktree that is the per-worktree admin dir; `refs/iris/*` checkpoint
  refs live in the shared common ref store. Reanchor and apply flows must not
  infer ref ownership from the record directory.

## Decision

### First backend slice for mutable subagents

This worktree service is the first backend slice for read-write subagents. The
subagent backend contract and read-only worker path have already shipped (#460),
but any subagent allowed to mutate files must run behind this isolation/apply
boundary. No in-place read-write subagent backend ships first, because that would
encode the weaker safety model as the default and make the later Grok-style path
an adapter instead of the foundation.

The reusable backend seam is therefore:

- `isolation = none` — only for non-mutating workers or explicitly in-place parent
  work.
- `isolation = worktree` — the first mutable backend: linked worktree, registry,
  lifecycle, explicit apply, and guarded removal.

The future model-facing subagent tool must not be named `task` (ADR-0052), but
its isolation field should map directly onto this service. The service remains
Wayland-owned infrastructure; Nexus sees only provider-neutral tool contracts,
events, and approval enforcement.

### First slice: linked worktrees; later slices target Grok-parity smoothness

The minimum useful mutable slice is a linked `git worktree add`. That remains the
first major implementation step because the safety semantics must be correct
before optimization layers are added.

The following are **wanted**, but intentionally later slices after the linked
semantics and apply boundary are proven correct:

- Snapshot fast paths: Btrfs subvolume, overlay, and delegated snapshot creation
  before falling back to linked git worktree creation.
- Worktree pooling and adoptable-candidate reuse after crashes or dead owner
  processes.
- Remote session/codebase restore into a fresh worktree.
- A richer query surface if JSONL becomes insufficient; SQLite is the reference
  tradeoff, not the first storage choice.

Still out of scope for this ADR unless a later ADR supersedes it:

- Standalone `.git/`-copy worktrees.
- ACP-style extension surface.
- Repo-remote memory identity.

Everything below that describes concrete behavior describes the linked-worktree
first slice. The wanted later slices must preserve the same apply and guarded
deletion invariants.

### Tier 0: isolation composes with in-place safety, it does not replace it

A worktree gives a child agent its own working directory and its own HEAD. The
parent checkout is not read, locked, or written while the child runs, so ADR-0028
Tiers 1-3 have nothing to defend in the parent during the child's turns — the
parent is safe by construction. This is **Tier 0**: prevention by absence of
shared state.

Inside the worktree, ADR-0028 applies unchanged. The worktree is itself a git
working tree, so dirty-baseline checks, the ledger, `refs/iris/*` checkpoints,
index protection, and approvals all operate on the worktree exactly as they do on
a primary checkout. Tier 0 does not weaken any inner-tier guarantee; it adds an
outer layer that keeps the parent out of scope until apply.

Limitation to state plainly: `isolation: worktree` requires a valid git
repository. In a non-git directory there is no worktree to create, so isolation
is unavailable. Parent sessions may still run in-place under ADR-0028's degraded
(plain-snapshot) guarantees, but mutable subagents must fail closed rather than
falling back to parent-workspace mutation. Bare repositories and invalid git
structures are rejected at create time.

### Apply is a guarded parent mutation and conditional settlement

Apply is the only boundary crossing from child worktree to parent workspace. It
has two identities and both are enforced when present:

1. **The first mutation of a parent-workspace task.** Writing the child's diff
   into the parent is an edit of the parent tree. It routes through the #262 tool
   choke point: each parent path the apply would touch is checked against the
   parent's dirty baseline, and any pre-existing uncommitted change at that path
   triggers the per-file per-task approval prompt before it is overwritten. Apply
   holds no exemption from the mutation gate.
2. **Settlement of the worktree task when durable workflow is enabled.** Per
   ADR-0052, durable task records are opt-in. When a worktree task exists, apply
   freezes the worktree's ledger and closes that task. When the durable workflow
   is off, apply is still a guarded parent mutation but does not invent hidden
   durable task state.

**Granularity: file-level apply with review.** Apply computes the child's net
changes with the #264 diff engine (invoked with the worktree as the source-tree
parameter), presents them as the review artifact, and applies accepted changes by
writing files into the parent working tree. This mirrors the Grok evidence
(writes/merges files, spec section 10) and keeps apply inside the edit/write
contract Iris already enforces.

**Apply does not stage or commit by default.** ADR-0028 makes the index protected
state: a user's staged selection is expensive-to-recreate work Iris must not
mutate without approval. Auto-staging applied files would silently rewrite that
protected state, so apply writes working-tree files only and leaves `git add` /
commit to the user. This also matches the Grok evidence ("writes/merges files,
not committing by default"). A future approved auto-stage feature is already
contemplated by ADR-0028 (Iris-authored index changes go in the ledger); apply
would opt into that path, not bypass it.

**Base-drift precondition.** The dirty-baseline check protects only uncommitted
parent state; it says nothing about committed drift. If the parent `HEAD` has
moved since the worktree was created (pull, checkout, commit), a "clean" parent
path may still hold committed content that differs from the child's base, and
writing the child's bytes over it would silently revert committed work. Apply
therefore records the worktree's base (`head_commit`) at creation and, before
writing, compares each touched path's blob in the current parent `HEAD` against
the same path in the recorded base. Paths whose base blob is unchanged apply
under the dirty-file rule below. Paths that drifted are surfaced as conflicts in
the apply review -- per-path: skip, or overwrite with explicit per-file approval
(the same prompt shape as the dirty-file rule; never silent). Apply never
rebases or merges content in this slice.

Conflict handling therefore reduces to the ADR-0028 dirty-file rule plus the
base-drift precondition: a parent path that is clean and un-drifted is written
directly; a parent path that carries an unapproved pre-existing change, or whose
committed base drifted, prompts before overwrite; the user resolves. No bespoke
three-way merge is introduced in this slice.

### Registry: session-sibling JSONL, not SQLite

Iris needs a durable inventory of worktrees so `list`/`show`/`rm`/`gc` operate on
records, not just `git worktree list` output. The record shape follows Grok's
`worktrees` table (spec section 7):

| Field | Meaning |
|---|---|
| `id` | Stable worktree id; primary CLI handle. |
| `path` | Absolute worktree directory; unique. |
| `source_repo` | Source repository root. |
| `repo_name` | Display/filter name for the source repo. |
| `kind` | Worktree purpose. Slice value: `session`. |
| `creation_mode` | Creation strategy. Slice value: `linked`. |
| `git_ref` | Branch/ref associated with the worktree. |
| `head_commit` | Commit at creation. |
| `session_id` | Owning Iris session, when any. |
| `creator_pid` | Process that created the record; used by liveness/GC. |
| `created_at` | Unix ms. |
| `last_accessed_at` | Unix ms; nullable. |
| `status` | Lifecycle status: `alive` \| `dead`. |
| `metadata` | JSON string (e.g. `label`). |

**Storage: a single append-only JSONL registry, latest-line-wins per `id`,** at
`IRIS_WORKTREE_DIR` if set, else `~/.iris/worktrees/registry.jsonl`. This is the
same store shape Iris already ships:

- [`session.rs`](../../src/session.rs) is an append-only JSONL log flushed per
  line so a crash leaves a valid prefix; the compaction entry already uses
  latest-wins-over-a-range semantics. A worktree status change (`alive` ->
  `dead`) is the same append-latest-wins update.
- [`handles.rs`](../../src/handles.rs) stores per-session artifacts in a
  session-sibling directory. Worktree directories live under the same
  `~/.iris/worktrees/` root as the registry.
- The registry is a cache, not the source of truth: `git worktree list` plus a
  scan of the storage root reconstructs it (`db rebuild` equivalent). A JSONL log
  that can be rebuilt from the filesystem does not need a relational store.

Reuse-before-handroll picks JSONL: SQLite would add a `rusqlite` dependency and a
second storage paradigm to a codebase whose established durable-state pattern is
JSONL + filesystem rebuild. Indexed queries and multi-writer transactions (the
reasons Grok uses SQLite) do not pay off at the linked-slice scale — the record
count is bounded by live worktrees, and cross-process appends are made
crash-safe by the same per-line-flush `O_APPEND` discipline `SessionLog` uses.
The SQLite tradeoff is revisited in Alternatives.

### Lifecycle: create / list / show / rm / gc

- **create** — resolve and validate the source as a git repo/worktree; run
  `git worktree add` (linked, with checkout) under the safety defaults below;
  register a record with `status = alive` and `creator_pid`.
- **list** — read registry records (filterable by repo/kind), reconciled against
  `git worktree list`.
- **show** — one record with its path, id, source repo, creation mode, ref, head,
  timestamps, session id, creator pid, and status.
- **rm** — accept exactly one selector (id or path). Non-force reports failure
  without a destructive fallback; force may fall back to
  `git worktree remove --force`. On success, deregister and coordinate
  `git worktree prune` on the source repo.
- **gc** — remove dead records; remove expired worktrees bounded by a max-age;
  **skip worktrees whose `creator_pid` is still alive unless forced**;
  `--dry-run` reports the plan without deleting.

**Guarded deletion (security-relevant).** Deletion never trusts a registry
`path` field. The rules, all of which must hold before any recursive removal:

1. **Canonicalize first.** The candidate path is canonicalized (symlinks and
   `..` resolved) and the storage root is canonicalized; the containment check
   runs on the canonical forms, component-wise, never as a lexical prefix
   comparison.
2. **Id-derived shape only.** The only deletable shape is
   `<storage-root>/<worktree-id>` where `<worktree-id>` matches the registry
   record's id exactly (single path component, no separators). Arbitrary deeper
   or shallower paths from a record are treated as corrupt, never removed.
3. **Ownership marker.** Worktree creation writes a marker file
   (`.iris-worktree` carrying the worktree id) inside the directory; deletion
   verifies the marker matches the record before removing. A directory without
   a matching marker is reported, not deleted.
4. **Constrained root.** `IRIS_WORKTREE_DIR` is refused if it canonicalizes to
   `/`, the user's home directory itself, the source repository, or any
   ancestor of either; the default remains `~/.iris/worktrees/`.

A registry record failing any rule is treated as corrupt: surfaced, skipped,
and left on disk. This strengthens Grok's "deletion is constrained to known
storage" invariant (spec section 14) against misconfigured roots, symlinked
components, and corrupt records.

**Prune coordination.** After removing a linked worktree directory, run
`git worktree prune` on the source repo to clear the stale
`.git/worktrees/<name>` administrative entry. Skip prune while any adoptable
candidate exists — not relevant in this slice (no pooling) but recorded as the
rule for when pooling arrives.

### Git subprocess safety defaults (standard for all Iris git subprocesses)

Every Iris git subprocess — worktree operations and all others — sets:

- `GIT_TERMINAL_PROMPT=0` — never block on an interactive credential prompt.
- `GIT_SSH_COMMAND=ssh -o BatchMode=yes` — never block on an SSH prompt.
- `GIT_LFS_SKIP_SMUDGE=1` — do not fetch LFS blobs during tree operations.
- `--no-optional-locks` — avoid taking optional git locks that can contend with
  a concurrent user git process.

These are recorded here as the repo-wide standard, matching the Grok defaults
(spec section 8.4). A hung git subprocess waiting on a hidden prompt is a
denial-of-service on the agent loop; batch/noninteractive mode is the safe
default.

### Session and subagent seams

- **Session-in-worktree.** A session may be created against a fresh worktree
  instead of the primary checkout. The session store is unchanged: the transcript
  still lives under `~/.iris/sessions/...`; only the working directory the tools
  operate on differs. The registry's `session_id` links the two.
- **Fork-into-worktree.** A session fork may materialize into a fresh worktree so
  the fork's edits stay isolated from the parent session's checkout until apply.
- **Subagent-tool schema reservation (#216, Milestone 4).** Reserve — but do not
  yet implement beyond `none` — the child-isolation contract on the future
  subagent tool:
  - `isolation: none | worktree`, default `none`. Only `none` is implemented in
    this slice; `worktree` is reserved.
  - `cwd` XOR `isolation`: supplying both is a validation error. `cwd` points a
    child at an existing directory; `isolation: worktree` creates a new isolated
    one; they are mutually exclusive (Grok spec section 5, rule 3).

  Reserving the field and the validation rule now keeps the Milestone 4 schema
  forward-compatible so enabling `worktree` later is additive, not a breaking
  schema change.

### jj interop

Per ADR-0028's interop note, if the workspace is a jj repo (`.jj/` present), jj
owns the working-copy lifecycle. A jj-backed isolated workspace is removed with
`jj workspace forget` followed by directory removal — never treated blindly as a
git worktree (Grok spec sections 9.2, 14). Detecting jj and constraining worktree
operations accordingly is required; the detailed jj create path is out of scope
for this slice (jj workspace creation semantics are deferred with the rest of the
non-linked backends).

### Naming

The subsystem is infrastructure with no character analogue, so per
[`NAMING.md`](../NAMING.md) rule 4 it takes a descriptive, non-mythological name,
as Nexus does: the **worktree registry** (durable record store) and the
**worktree service** (the create/list/show/rm/gc coordinator below the agent
loop). Worktree ids follow the existing session-id style (stable, opaque,
filesystem-safe). A mythological tier label can be assigned later if the
subsystem is ever promoted to a first-class tier; coining one now would be
premature.

Tier placement (per [`ARCHITECTURE.md`](../ARCHITECTURE.md)): the worktree service
and registry are **Wayland (Tier 2)** — they own execution-environment state and
durable session-adjacent storage, the same tier as the session store, path
safety, and the handle store. Nexus (Tier 1) gains no worktree knowledge; the
subagent-tool `isolation` field is surfaced through the existing tool contract,
and the CLI `iris worktree` commands live in Iris (Tier 3).

## Alignment with Grok Build CLI 0.2.82

Source: the local reference
[`grok-worktree-subsystem-spec.md`](../../.iris-reference/grok-worktree-subsystem-spec.md).
This review compares the planned linked-worktree slice, not a future full
snapshot/pooling system.

| Area | Grok Build CLI | Iris planned slice | Assessment |
|---|---|---|---|
| Model-facing contract | Subagent input has isolation, capability mode, background execution, resume, cwd, and typed subagent kinds. | #460 shipped the read-only backend contract; mutable worker schema still needs the future subagent tool surface. | **Partial / gap** until mutable subagent enablement. |
| Isolation primitive | Worktree isolation; parent is untouched until apply. | Same semantic target: linked worktree, parent mutation only through apply. | **Aligned.** |
| Apply boundary | Explicit worktree apply; exact conflict algorithm unverified. | File-level apply with review, dirty-parent gate, and base-drift conflict detection. | **Better specified** than observed reference evidence. |
| Registry | Durable registry with id/path/source/session/pid/status and indexed management queries. | Append-only JSONL registry, latest-line-wins, rebuildable from filesystem and git worktree state. | **Simpler / weaker at scale**, but aligned with Iris storage patterns; revisit SQLite only if volume/concurrency requires it. |
| Lifecycle CLI | list/show/rm/gc-style management; live-pid skip; dry-run; guarded remove paths. | `list/show/rm/gc`; live-pid skip; dry-run; marker + canonical containment deletion guard. | **Aligned**, with **stronger deletion guard** specified. |
| Creation backends | Overlay, Btrfs, delegate, fast linked copy, standalone copy, git checkout fallback. | Plain linked `git worktree add` first; snapshot fast paths are wanted later slices. | **First slice weaker; roadmap target aligned.** |
| Worktree pooling/adoption | Pool cleanup, dead PID detection, adoptable candidates. | Basic lifecycle first; pooling/adoption are wanted later slices. | **First slice weaker; roadmap target aligned.** |
| Session resume/fork | Resume/fork into worktree and remote restore paths. | Local session/worktree link first; remote restore is a wanted later slice. | **First slice gap; roadmap target aligned.** |
| jj support | Removal uses `jj workspace forget`; create path not fully recovered. | Same removal requirement; jj create deferred. | **Aligned.** |
| Safety defaults | Noninteractive git env, LFS smudge skip, batch SSH, optional-lock avoidance. | Adopted as standard for git subprocesses. | **Aligned.** |

Overall: Iris is **aligned on the core safety model** and **better specified on
apply conflict handling and deletion guards**. The first implementation is
deliberately smaller than Grok on smoothness and scale, but snapshot fast paths,
pooling/adoption, and remote restore are **desired follow-up slices**, not rejected
features. The remaining true gap is mutable subagent enablement on top of the
backend contract. The first mutable implementation must keep the boundary clean:
linked worktree, durable record, explicit apply, guarded remove, and no mutable
subagent path that bypasses it.

## Alternatives Considered


### Alternative 0: Ship in-place read-write subagents first
- **Pros**: Fastest path to visible mutable delegation; no git-worktree
  lifecycle, apply, or registry work needed up front.
- **Cons**: Makes the parent checkout the first shared mutation surface for child
  agents, so the default backend has the weakest safety semantics. It also makes
  later worktree support look like an optional adapter instead of the core
  primitive, fragmenting the runner around two mutation models.
- **Why not**: The reference demonstrates that the clean systems boundary is child
  mutation in an isolated worktree followed by explicit apply. Iris should make
  that the first mutable backend and keep in-place behavior for parent work or
  explicitly non-isolated modes only.

### Alternative 1: SQLite registry (Grok's choice)
- **Pros**: Indexed lookups by id/path/session, atomic multi-writer
  transactions, a single-file inventory with `json_extract` label queries — the
  exact shape Grok ships and proven at its scale.
- **Cons**: Adds a `rusqlite`/bundled-SQLite dependency and a second durable-state
  paradigm to a codebase whose established pattern is append-only JSONL plus
  filesystem rebuild; heavier than the linked slice needs.
- **Why not**: The registry is a rebuildable cache over `git worktree list` and a
  storage-root scan; record volume is bounded by live worktrees. JSONL with
  per-line-flush appends matches `session.rs` and `handles.rs`, reuses existing
  patterns, and adds no dependency. Revisit SQLite if pooling or high-volume
  best-of-N makes indexed multi-writer access pay for itself.

### Alternative 2: Detached checkout instead of a linked worktree
- **Pros**: No `.git/worktrees/<name>` administrative entries; a single working
  tree to reason about.
- **Cons**: A detached checkout mutates the one shared working tree and HEAD — the
  precise entanglement Tier 0 exists to prevent. It cannot give a child an
  isolated tree without disturbing the parent, and it breaks parallelism
  (best-of-N, concurrent subagents each need their own tree).
- **Why not**: Isolation-by-construction requires a separate working directory.
  Linked worktrees provide that while sharing the object store, so checkpoints and
  `refs/iris/*` keep working; detached checkout provides neither isolation nor
  parallelism.

### Alternative 3: Apply via merge / cherry-pick instead of file-level apply
- **Pros**: Records provenance as commits; git's merge machinery resolves
  three-way conflicts; history is preserved.
- **Cons**: Requires the child to commit and the parent to accept commits into
  its history and index — mutating HEAD, the index, and refs, all ADR-0028
  protected state, and committing on the user's behalf without approval. It also
  imposes a branching model on users who may not want the child's commits in their
  history.
- **Why not**: Apply is defined as an edit of the parent working tree under the
  #262 mutation gate, deliberately leaving staging and commit to the user. Merge/
  cherry-pick would bypass the index-protection contract and force a commit
  workflow. File-level apply keeps the reviewable-diff-then-write model Iris
  already enforces; a merge-based apply can be reconsidered once commit-authoring
  is an approved, ledgered capability.

## Consequences

### Positive
- The parent checkout is untouched by construction while an isolated child runs;
  Tier 0 composes cleanly on top of ADR-0028's in-place tiers.
- Apply reuses the #262 mutation gate and the #264 diff engine, so isolation adds
  a boundary event, not a parallel safety system.
- The registry reuses the JSONL + filesystem-rebuild pattern already in the
  codebase; no new storage dependency.
- Reserving the future subagent `isolation`/`cwd` contract now makes enabling
  worktree isolation later an additive change, not a schema break.
- Git subprocess safety defaults are fixed repo-wide, closing the hung-prompt
  denial-of-service surface for all git calls.

### Negative
- Linked worktrees write `.git/worktrees/<name>` administrative state into the
  user's repo (bounded, prune-coordinated on removal — but visible to git
  tooling).
- The JSONL registry trades indexed queries and transactional multi-writer
  updates for append-latest-wins simplicity; large inventories or heavy
  concurrent creation would eventually favor SQLite.
- Only linked worktrees ship; users on Btrfs/overlay filesystems get no O(1)
  snapshot fast path in this slice.
- `isolation: worktree` is unavailable in non-git directories.

### Risks
- Cross-process appends to one JSONL registry rely on atomic `O_APPEND`
  line writes and per-line flush; a partial line or interleave needs the
  rebuild-from-filesystem path as the recovery net (tested).
- Guarded deletion must be airtight: a corrupt or attacker-influenced `path`
  record must never drive an `rm -rf` outside the storage root. This is
  workspace-fence-class safety and needs explicit tests.
- Apply-into-a-dirty-parent depends on the #262 machinery landing with the
  semantics assumed here; if #262's per-file approval contract shifts, apply's
  conflict story shifts with it.
- jj detection must precede any git worktree operation in a colocated repo, or
  Iris could corrupt jj's working-copy invariants.

## Open items (deferred, not decided here)
- Exact worktree path and branch naming rules.
- The jj workspace *create* path (only `forget`-based removal is settled).
- Snapshot backends (Btrfs/overlay/delegate), pooling, remote restore, ACP
  surface, and repo-remote memory identity — all explicitly out of scope.
- Whether a future approved auto-stage capability extends apply to `git add`.
