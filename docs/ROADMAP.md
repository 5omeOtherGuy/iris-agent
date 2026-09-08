# Iris — Roadmap to v1.0

Status checked 2026-09-08 against `b420985` (main, 2026-08-03).
Latest published release checked: `v0.3.7` (2026-07-14).

Iris is a pre-1.0 terminal coding agent with a working local coding loop,
persistent sessions, compaction, and delegated workers. The next phase is to
harden those paths, test them together, and optimize from measured task outcomes.
A v1.0 release does not require every feature in the backlog.

This document defines build order and acceptance gates, not delivery dates or a
new implementation specification. The v1.0 scope below is the release target;
none of its open gates is satisfied merely by this documentation update.
[FEATURES.md](FEATURES.md) inventories capabilities. The
[documentation audit](DOCUMENTATION_AUDIT.md) records evidence and coverage limits.

## Current baseline

| Area | Implemented | Constraint or unfinished work |
| --- | --- | --- |
| Agent loop | Async provider/tool loop, streaming, approvals, safe-parallel execution, cancellation, steering, headless mode | Blocking provider reads and plain-terminal approval reads still have cancellation limits. |
| Providers | Codex, OpenAI API, Anthropic, Antigravity, OpenAI-compatible routes; OAuth/API-key auth; runtime selection | Support depends on route/model capabilities and credentials. Fast/service-tier routing and startup prewarm remain planned. |
| Tools | Native file/search/shell tools, output reduction, `read_output`, `recall`, user questions, opt-in web tools | Image/PDF/notebook reads and provider-native patch editing are not implemented. |
| Sessions | JSONL persistence, continue/resume, round-trip flushes, compaction/fold rebuild, task linkage | Conversation branching is not exposed. Session export and structured event CLI output remain planned. |
| Context | Background/manual/reactive compaction, model-aware pressure, durable recall/carry, opt-in tool-result folding | Ready summaries wait until hard pressure. Apply-on-ready and safer configuration are specified but not implemented. |
| Change safety | Read-before-mutate, atomic writes, dirty-tree guard, reviewed diff; opt-in tasks/checkpoints/rollback/verification | Main-session path and Linux shell confinement require `IRIS_SECURITY_OPT_IN=1`; macOS shell execution is unconfined. |
| Delegation | `general`/`explore`/`review` manifests, authenticated model routing, filtered tools, durable background workers, isolated mutation and reviewed apply | Nested delegation and a manifest editor are deferred. Best-of-N is dormant runtime support, not a model-facing workflow. |
| Terminal | Pager/inline/plain renderers, live composer, Markdown/reasoning, settings, goals, search, tree/Git/delegation controls | Platform and real-terminal acceptance must be repeated for releases; Windows is unsupported. |
| Distribution | Linux/macOS archives, installer, updater, crates.io release flow | Release publication is operator-only. Session/config compatibility and release acceptance need a v1.0 contract. |

The old roadmap's kernel/local-agent milestones and the first Git-workflow and
pager slices have landed. They are not the active blockers. The broader context
planner, modes, and GitHub automation did not become implemented just because
later delegation and UI slices shipped out of order. The
[historical roadmap at the audit baseline](https://github.com/5omeOtherGuy/iris-agent/blob/b420985581f5b3c75b4f09991e343b674c77b45e/docs/ROADMAP.md)
retains the original milestone narrative and earlier acceptance records.

## Evidence baseline

- **Compaction:** the [live-loop report](benchmarks/auto-compaction-live-loop.md)
  records the July redesign protocol, including exclusions and rejected probes.
  [Retention tests](benchmarks/issue-372-compaction-retention-slice-b.md) and
  `src/wayland/compaction_property_tests.rs` guard retained facts, pair safety,
  and replay equivalence. These are recorded results, not September reruns.
- **Task economics:** [issue #210](https://github.com/5omeOtherGuy/iris-agent/issues/210)
  is complete as a measurement task. The
  [90-session headline campaign](benchmarks/campaigns/legacy-headline-matrix/2026-07-05/headline-matrix-2026-07-05.md)
  found no task-success regression, but baseline used fewer tokens in six of nine
  cells. No universal tokens-per-completed-task savings claim is supported.
- **Recent integration:** [PR #667](https://github.com/5omeOtherGuy/iris-agent/pull/667)
  records the manifest-driven spawn migration and a passing full gate.
  [PR #668](https://github.com/5omeOtherGuy/iris-agent/pull/668) adds immediate
  stale-reused-WebSocket recovery. Neither is a current full-release validation.
- **Open release risk:** the latest inspected scheduled dependency audit on the
  baseline failed ([run](https://github.com/5omeOtherGuy/iris-agent/actions/runs/34112493767)).
  Cause was not established in this documentation audit. Triage it; do not infer
  a specific vulnerability or silently waive the check.

## Milestone sequence

| Milestone | Status | Depends on | Exit artifact |
| --- | --- | --- | --- |
| M0 — Reconciled baseline | Open; documentation audit merged; baseline gate pending | None | Agreed release scope, current CI/audit diagnosis, reproducible baseline |
| M1 — Compaction hardening | Open; existing engine works, overhaul pending | M0 baseline | Safe lifecycle/config migration plus deterministic and live evidence |
| M2 — Safety and recovery | Open; controls partly implemented | M0; may run beside M1 | Failure-path and platform safety matrix with explicit guarantees |
| M3 — Release regression suite | Open; many unit/integration tests exist | M1 + M2 for final acceptance | Repeatable cross-subsystem and supported-platform results |
| M4 — Measured optimization | Open; corpus and campaign harnesses exist | M3 baseline; profiling may start earlier | Reproducible before/after cost, latency, memory and success report |
| M5 — v1.0 release candidate | Open | M0–M4 | Compatibility contract, candidate artifacts and operator sign-off |

### M0 — Reconciled baseline

**Gate**

- Current manuals distinguish implemented, opt-in, constrained, planned, and
  research behavior. Source links and operator commands resolve. **Done:**
  the [documentation audit](DOCUMENTATION_AUDIT.md) reconciled all 161
  documentation files against source at `b420985`.
- Define the supported provider/platform matrix and v1.0 non-goals. Every open
  blocker has a reproduction, owner, and milestone; stale checkboxes are not a
  substitute for issue triage.
- Run the full code gate on the candidate baseline and diagnose dependency/security
  check failures. Record exclusions with reasons and owners; never hide failures.

**Next**

1. Use the audit findings to reconcile active docs and issue tracking. Preserve
   dated ADRs, changelogs, and benchmark measurements as history.
2. Classify release-blocking defects separately from optional features.
3. Capture compiler/platform/provider versions and deterministic baseline results.

**Evidence to attach:** baseline commit, exact commands/results, unresolved CI
failures, and the approved support/compatibility matrix. This documentation
change does not close the platform/provider baseline gate, even with a local
gate pass.

### M1 — Compaction hardening

**Status:** automatic, manual, reactive, and recoverable tool-result compaction
exist. The open [overhaul #658](https://github.com/5omeOtherGuy/iris-agent/issues/658)
is the normative implementation spec; do not re-derive it here.

**Implementation order**

1. [#659](https://github.com/5omeOtherGuy/iris-agent/issues/659): one async,
   cancellation-raced bounded hard-tier wait at every call site; explicit
   `TimedOut` lifecycle state; retire the legacy budget/wait path.
2. [#660](https://github.com/5omeOtherGuy/iris-agent/issues/660): apply ready
   summaries at the next safe boundary, stamp covered entries and newer-tail
   precedence, reject over-aged plans, and record age/interim-mass telemetry.
3. [#661](https://github.com/5omeOtherGuy/iris-agent/issues/661): safe
   `compaction.mode`, clamped advanced overrides, old-key migration notices,
   effective-config visibility, and confirmed reset. May run beside the wait
   work, but coordinate legacy `contextTokenBudget` removal.
4. [#662](https://github.com/5omeOtherGuy/iris-agent/issues/662): operator-approved
   live economics and staleness-quality A/B after the deterministic twin passes.

**Gate**

- The apply-on-ready default changes only after its deterministic twin passes.
- Normal/start/hard/manual/reactive paths preserve complete tool pairs, newer
  history, durable-before-replace ordering, recall, and byte-equal resume.
- Test running/ready/stale/failed/timed-out/cancelled jobs, model changes, repeated
  compactions, small windows, and worker/native-provider rejection.
- Hard pressure always has finite deterministic relief without a live model.
- Test every deprecated key, invalid mode, clamp, source precedence, and reset
  cancellation. A proposed setting is not documented as an existing command.
- Live quality cells mechanically score retained facts and superseded goals;
  report cache reads/writes, worker cost, summary age, and task success separately.
  A failed retention/replay gate invalidates the change, not just the sample.

**Evidence to attach:** focused tests, full gate, updated ADR-0055 or superseding
ADR, migration fixtures, and the paid campaign report with raw artifacts and
exclusions. Follow the repository `compaction-tuning` skill; no automatic spend.

### M2 — Safety and recovery

**Status:** approvals, file freshness, dirty-tree protection, task recovery,
worker capability ceilings, and reviewed apply exist. Their guarantees differ
by mode and platform.

**Next**

- Resolve and document the v1.0 default confinement posture. Main-session
  security is currently opt-in; approval prompts are not a shell sandbox.
  macOS Seatbelt is [#253](https://github.com/5omeOtherGuy/iris-agent/issues/253).
  Shipping macOS without it requires an explicit constrained-support decision,
  not a claim of cross-platform confinement.
- Close or bound cancellation gaps in provider HTTP reads
  ([#34](https://github.com/5omeOtherGuy/iris-agent/issues/34)) and plain-terminal
  approval input ([#33](https://github.com/5omeOtherGuy/iris-agent/issues/33)).
- Reconcile permission modes/persistence
  ([#488](https://github.com/5omeOtherGuy/iris-agent/issues/488),
  [#316](https://github.com/5omeOtherGuy/iris-agent/issues/316)) without weakening
  destructive-action floors or allowing project settings to grant permissions.
- Harden task recovery/cleanup
  ([#453](https://github.com/5omeOtherGuy/iris-agent/issues/453)), concurrent
  ownership, worker cancellation, orphan adoption and reviewed apply.
- Audit privacy claims: credentials/diagnostics are not the same as arbitrary
  transcript secret redaction. Document storage and export boundaries honestly.

**Gate**

- Negative tests cover traversal/symlink escape, stale writes, malicious project
  settings, shell-policy bypass, unauthorized worker grants, and unreviewed apply.
- Interrupt streaming, approval, shell execution, compaction and workers. Show
  bounded return, child cleanup, valid transcripts, and no duplicate tool effects.
- Exercise truncated/corrupt logs, credential refresh races, stale sockets,
  process crashes, disk/persistence errors, concurrent sessions, dirty/untracked
  user files, index preservation, and stale apply plans.
- Platform tests prove the advertised guarantee or an explicit fail-closed or
  user-visible constrained outcome. A security claim cannot rely on a mock alone.

**Evidence to attach:** fault-injection results, platform/kernel matrix, recovery
fixtures and a reviewed safety/permission contract. Correctness defects are
release blockers; optional convenience controls are not.

### M3 — Release regression suite

**Status:** focused tests, property tests, fake-provider integration tests, frame
assertions and opt-in live harnesses exist. Build release-level coverage on those
seams rather than inventing a second runtime.

**Next**

- Compose realistic search/edit/test tasks across tools, approvals, verification,
  checkpoints, long sessions, repeated compactions, resume and delegated apply.
- Cover pager, inline and plain/headless paths; ensure non-TTY operation never
  hangs on a required interactive approval.
- Establish provider-contract fixtures for all supported routes, with opt-in
  live smokes for authentication, streaming, tool use, recovery and model switches.
- Automate high-value gaps first. Keep paid/provider-dependent checks outside
  the default deterministic gate; maintain an explicit release smoke checklist.

**Gate**

- `bash scripts/gate.sh` passes on a code-change candidate. Its docs-only fast
  path is not runtime verification. Include the runtime crate's tests explicitly.
- Mechanical success checks compare final diff/answer, preserved user changes,
  approval decisions, tool-pair validity, durable rebuild and worker isolation.
- Test fixtures include normal, boundary and failure paths; meaningful failures
  fail CI. Flaky exclusions name the cause, limit, and follow-up.
- Linux and macOS acceptance results identify architecture, terminal path and
  dependency/toolchain versions. Real-TTY smokes supplement frame assertions.

**Existing starting points**

```sh
cargo test --locked compaction
cargo test --locked compaction_bench
cargo test --locked epic_261_acceptance_end_to_end
cargo test --locked -p iris-subagent-runtime
bash scripts/gate.sh
```

`src/wayland/incremental_persistence_tests.rs`,
`src/wayland/compaction_property_tests.rs`, `src/nexus_tests.rs`,
`src/mimir/providers/openai_codex_responses_tests.rs`, and `src/ui/tui/` contain
existing regression seams. See [TUI live testing](TUI_LIVE_TESTING.md) for
operator checks after pane-rendering changes.

### M4 — Measured optimization

**Goal:** improve completed-task cost and responsiveness without reducing success,
retention, safety or recovery. Smaller tool output alone is not the objective.

**Next**

1. Profile the baseline: fixed prompt/tool-schema mass, turn count, cache prefix
   divergence, summarizer/worker spend, first-token and tool latency, task-workflow
   overhead, peak memory and Rust build cost.
2. Prioritize measured bottlenecks: cache-aware tool surfaces
   ([#639](https://github.com/5omeOtherGuy/iris-agent/issues/639)), task overhead
   ([#452](https://github.com/5omeOtherGuy/iris-agent/issues/452)), build memory
   ([#591](https://github.com/5omeOtherGuy/iris-agent/issues/591)), and optional
   Codex startup prewarm ([#621](https://github.com/5omeOtherGuy/iris-agent/issues/621)).
   These are candidates, not pre-approved default changes.
3. Use the existing `iris-bench`/live-harness migration; retire the legacy suite
   only after the replacement campaign validates it
   ([#573](https://github.com/5omeOtherGuy/iris-agent/issues/573)).
4. Evaluate one isolated change at a time with deterministic twins before paid
   confirmation. Separate output reduction from changes in model strategy/turns.

**Gate**

- Reports pin revisions, settings, workload, model/effort, sample count and
  mechanical success criteria. Compare identical safety/approval postures.
- Record completed-task tokens, actual reported cache flows, worker cost,
  latency distributions, memory, tool turns and success. Do not invent cache
  writes on write-blind routes or current prices from old reports.
- No task-success, retention, replay or safety regression is accepted as a
  performance win. Publish ties, negative results and uncertainty.
- Ship only supported improvements; remove/defer speculative optimizations.
  v1.0 does not require a universal savings percentage or superiority over every
  competing harness.

**Evidence to attach:** reproducible before/after report under
`docs/benchmarks/`, deterministic regression assertions, raw-run references,
provider spend authorization and default-change rationale. Follow
[BENCHMARK_PLAN.md](BENCHMARK_PLAN.md) and the token-efficiency benchmark skill.

### M5 — v1.0 release candidate

**Next**

- Document supported CLI/settings, deprecation windows, session/config migration,
  data retention, recovery and platform limitations. Keep internal architecture
  separate from a public SDK promise.
- Resolve release-blocking security, data-loss, correctness and compatibility
  defects. Freeze feature additions while the candidate is verified.
- Run [RELEASING.md](RELEASING.md), `bash scripts/validate-dist.sh`, install/update
  checks, and real-provider/TTY acceptance against the actual candidate artifacts.

**Gate**

- M0–M4 evidence is linked to the candidate or explicitly revalidated after drift.
- Deterministic CI, dependency/security review and supported-platform checks pass;
  any permitted non-blocking exception has a written rationale and owner.
- Old supported sessions/settings reopen or migrate with tested, documented
  outcomes. Failed update/download/checksum paths preserve the installed binary.
- Install, authenticate, edit/test/review, compact/resume, cancel/recover and
  delegate/apply work through the supported distribution paths.
- Release notes state known constraints and measured claims. The operator approves
  and performs tag/publish/release actions; preparation is not permission to ship.

## Planned features outside the v1.0 critical path

These are not implemented product features unless the boundary column says a
foundation exists. Reprioritization requires explicit scope and acceptance gates.

| Area | Planned capability | Existing boundary / tracking |
| --- | --- | --- |
| Context engine | Token-budget planner, reason-based context ledger/eviction, diff-aware file context | Estimates, summaries and cache hints exist; no general planner/ledger. |
| Durable memory | Decision records with recall references; hierarchical/project memory; file-change freshness | Carry/recall exist; [#476](https://github.com/5omeOtherGuy/iris-agent/issues/476); general memory remains research. |
| Handles | Browser, indexing/search, reference-aware lifecycle and richer summaries | `read_output` and compaction `recall` are already implemented. |
| Sessions | Conversation branching/fork navigation, branch-aware compaction, richer search, Markdown/HTML export | Linear resume and task rollback exist; [#122](https://github.com/5omeOtherGuy/iris-agent/issues/122), [#214](https://github.com/5omeOtherGuy/iris-agent/issues/214). |
| Modes | Named parent-session profiles, mode-aware prompt/tool/context policy | Runtime model/effort switching and worker manifests exist; [#216](https://github.com/5omeOtherGuy/iris-agent/issues/216). |
| Delegation | Manifest editor, nested delegation, curated context forwarding, guardrailed multi-step workflows | Manifest `allowed_children` is validated but inactive; native model-facing best-of-N was removed, not promised for return (ADR-0065). |
| Editing | OpenAI/Codex `apply_patch`/V4A and live patch-input preview | Exact-string `edit` exists; [#344](https://github.com/5omeOtherGuy/iris-agent/issues/344), [#345](https://github.com/5omeOtherGuy/iris-agent/issues/345). Content-hash anchors remain research. |
| Read/repository map | Images first, then PDF/notebook reads; ranked tree-sitter repo map; optional FFF tools | Text tools exist; [#215](https://github.com/5omeOtherGuy/iris-agent/issues/215), [#20](https://github.com/5omeOtherGuy/iris-agent/issues/20). |
| Git | Per-hunk staging, pre-commit review, explicitly approved auto-commit | Diff/checkpoint/rollback/worktree apply exist; [#269](https://github.com/5omeOtherGuy/iris-agent/issues/269), [#270](https://github.com/5omeOtherGuy/iris-agent/issues/270). |
| GitHub | Issue/PR/review/CI/stacked-PR workflows | No integrated product workflow; external CLI use is not native integration. |
| Provider catalog | Multi-provider registry/discovery and keychain migration; Fast/service-tier routing | Five routes and authenticated worker model selection exist; [#254](https://github.com/5omeOtherGuy/iris-agent/issues/254), [#666](https://github.com/5omeOtherGuy/iris-agent/issues/666). |
| Prompt system | Named slots, selector-driven assembly, prompt templates, general hooks | Compiled fragments and native skills exist; [#73](https://github.com/5omeOtherGuy/iris-agent/issues/73), [#76](https://github.com/5omeOtherGuy/iris-agent/issues/76), [#57](https://github.com/5omeOtherGuy/iris-agent/issues/57), [#59](https://github.com/5omeOtherGuy/iris-agent/issues/59). |
| CLI ergonomics | Structured JSON event output, configurable keybindings/hotkeys, shell passthrough and external editor | Print mode, file mentions, focus and terminal doctor exist; [#212](https://github.com/5omeOtherGuy/iris-agent/issues/212), [#213](https://github.com/5omeOtherGuy/iris-agent/issues/213), [#208](https://github.com/5omeOtherGuy/iris-agent/issues/208). |
| Platforms | Native Windows/Git Bash support; broader network confinement | Linux/macOS only; [#613](https://github.com/5omeOtherGuy/iris-agent/issues/613). macOS safety posture must still be resolved in M2. |
| Maintainability | Further Cargo workspace modularization | `iris-bench` and `iris-subagent-runtime` are already separate; [#650](https://github.com/5omeOtherGuy/iris-agent/issues/650), [design proposal](MODULARIZATION.md). Split only with measured/ownership justification. |

**Research, not a release commitment:** plugins/WASM/subprocess extension runtimes,
plugin identity approvals, general project memory, content-hash edit syntax and
production cloud worktree restore. MCP, RPC/embedding and an extension marketplace
are not v1.0 product goals. The public worker-runtime crate does not make the Iris
agent an embedding SDK.

## Keeping this roadmap current

When a gate changes, update its status and evidence together. Implemented source
and passing tests outrank old issue checkboxes. A merged implementation is not a
passing live campaign; an accepted ADR is not shipped code. Preserve negative
benchmark results and mark historical reports as historical instead of rewriting
their measurements. Revisit this roadmap at each release candidate.
