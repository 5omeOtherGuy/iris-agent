# Architecture Decision Records

| ADR | Title | Status | Date |
|-----|-------|--------|------|
| [0001](0001-keep-nexus-wayland-iris-as-in-crate-tiers.md) | Keep Nexus, Wayland, and Iris as in-crate tiers | accepted | 2026-06-17 |
| [0002](0002-use-tokio-async-streaming-and-cancellation-in-nexus.md) | Use Tokio async streaming and cancellation in Nexus | accepted | 2026-06-17 |
| [0003](0003-keep-provider-adapters-and-auth-in-mimir.md) | Keep provider adapters and auth in Mimir | accepted | 2026-06-17 |
| [0004](0004-build-jsonl-session-store-foundation-before-resume-compaction.md) | Build JSONL session store foundation before resume and compaction | accepted | 2026-06-17 |
| [0005](0005-nexus-owns-tool-approval-and-execution-policy.md) | Nexus owns tool approval and execution policy | accepted | 2026-06-17 |
| [0006](0006-use-stable-ratatui-crossterm-and-selectively-borrow-codex-tui-patterns.md) | Use stable Ratatui/Crossterm and selectively borrow Codex TUI patterns | accepted | 2026-06-17 |
| [0007](0007-use-native-trusted-tools-with-read-before-mutate-safety.md) | Use native trusted tools with read-before-mutate safety | accepted | 2026-06-17 |
| [0008](0008-harden-bash-with-process-groups-persistent-sessions-and-landlock.md) | Harden bash with process groups, persistent sessions, and Landlock | accepted | 2026-06-17 |
| [0009](0009-persist-compaction-as-a-session-entry-and-rebuild-context-through-the-summary.md) | Persist compaction as a session entry and rebuild context through the summary | accepted | 2026-06-17 |
| [0010](0010-mutating-and-effectful-tools-opt-out-of-persistent-allow-always.md) | Mutating and effectful tools opt out of persistent allow-always | accepted | 2026-06-17 |
| [0011](0011-store-oversized-tool-outputs-behind-session-scoped-handles.md) | Store oversized tool outputs behind session-scoped handles | accepted | 2026-06-17 |
| [0012](0012-harness-owned-fragment-slot-system-prompt-assembly.md) | Harness-owned fragment/slot system-prompt assembly | accepted | 2026-06-18 |
| [0013](0013-shared-selector-schema-for-dynamic-prompt-and-tool-assembly.md) | Shared selector schema for dynamic system-prompt and tool-surface assembly | proposed | 2026-06-18 |
| [0014](0014-tool-visibility-is-not-authorization.md) | Tool visibility is not authorization | accepted | 2026-06-18 |
| [0015](0015-assign-fragments-to-config-defined-named-slots.md) | Assign fragments to config-defined named slots instead of numeric slots | proposed | 2026-06-18 |
| [0016](0016-preserve-provider-reasoning-continuity-in-flattened-transcripts.md) | Preserve provider reasoning continuity in flattened transcripts | accepted | 2026-06-20 |
| [0017](0017-centralize-model-selection-and-switch-at-turn-boundaries.md) | Centralize model selection and switch at turn boundaries | accepted | 2026-06-21 |
| [0019](0019-formalize-correlation-ids.md) | Formalize correlation IDs | accepted | 2026-06-21 |
| [0020](0020-expand-typed-runtime-events.md) | Expand typed runtime events | accepted | 2026-06-21 |
| [0021](0021-structured-tool-result-contracts.md) | Define structured tool-result contracts without a schema platform | accepted | 2026-06-21 |
| [0022](0022-use-default-short-provider-native-cache-and-context-management.md) | Use default-short provider-native cache and default-off context-management controls | accepted | 2026-06-22 |
| [0023](0023-project-tool-declarations-per-provider-and-prefer-native-tools.md) | Project tool declarations per provider and prefer native tools | accepted | 2026-06-21 |
| [0024](0024-introduce-tui-component-container-overlay-focus-abstraction.md) | Introduce a reusable TUI Component/Container/overlay/focus abstraction | accepted | 2026-06-26 |
| [0025](0025-expose-stored-reasoning-as-a-display-event.md) | Expose stored reasoning as a display event | accepted | 2026-06-26 |
| [0026](0026-make-system-prompt-fragments-fully-internal.md) | Make system-prompt fragments fully internal | proposed | 2026-07-02 |
| [0027](0027-repurpose-trust-store-as-per-cwd-project-permission-policy.md) | Repurpose the trust store as a per-cwd project permission policy | proposed | 2026-07-02 |
| [0028](0028-git-workflow-dirty-tree-safety-and-task-checkpointing.md) | Git workflow — dirty-tree safety, task checkpointing, and rollback semantics | accepted | 2026-07-03 |
| [0029](0029-adopt-alt-screen-pager-tui.md) | Adopt an alt-screen pager TUI with an Iris-owned scrollback pane | accepted | 2026-07-03 |
| [0030](0030-git-safety-task-ownership-lease-and-mutation-lock.md) | Git-safety task ownership — per-task lease and repo mutation lock | accepted | 2026-07-03 |
| [0031](0031-task-identity-session-linkage-and-resumable-tasks.md) | Task identity — opaque body, session linkage, and explicit task resumption | accepted | 2026-07-03 |
| [0032](0032-approval-presets-auto-and-safety-floors.md) | Approval presets, auto mode, and non-bypassable safety floors | accepted | 2026-07-04 |
| [0033](0033-ratatui-native-adoption-boundary.md) | Define the ratatui-native adoption boundary for the TUI | accepted | 2026-07-04 |
| [0034](0034-run-blocking-tool-bodies-off-the-ui-executor.md) | Run blocking tool bodies off the UI executor with channel-bridged streaming | accepted | 2026-07-04 |
| [0035](0035-git-worktree-isolation-and-apply-as-settlement.md) | Git worktree isolation — Tier 0 of the ADR-0028 guarantee model, apply = settlement | proposed | 2026-07-03 |
| [0036](0036-tools-are-token-efficient-by-design.md) | Tools are token-efficient by design | accepted | 2026-07-04 |
| [0037](0037-native-output-filtering-for-bash-pass-through.md) | Native output filtering for bash pass-through commands | proposed | 2026-07-04 |
| [0038](0038-per-model-edit-surfaces-share-one-mutation-core.md) | Per-model edit surfaces share one mutation core | proposed | 2026-07-04 |
| [0039](0039-freeform-tool-input-deltas-are-display-only.md) | Freeform tool-input deltas are display-only | accepted | 2026-07-04 |
| [0040](0040-classified-tool-errors-carry-machine-readable-metadata.md) | Classified tool errors carry machine-readable metadata | accepted | 2026-07-04 |
| [0041](0041-token-efficient-model-switching-and-provider-summaries.md) | Token-efficient model switching and provider-backed compaction summaries | accepted | 2026-07-02 |
| [0042](0042-opt-in-named-themes-behind-a-theme-trait.md) | Opt-in named color themes behind a Theme trait, terminal-relative by default | proposed | 2026-07-04 |
| [0043](0043-provider-wait-visibility-and-stream-idle-guard.md) | Provider waits are visible and bounded by provider-event idleness | accepted | 2026-07-05 |
| [0044](0044-carry-structured-state-across-compaction.md) | Carry structured state across compaction, separate from the prose summary | accepted | 2026-07-04 |
| [0045](0045-benchmark-compaction-on-task-success-and-retention.md) | Benchmark compaction on task success and load-bearing-detail retention | accepted | 2026-07-04 |
| [0046](0046-recall-compacted-originals-mid-session.md) | Recall compacted originals mid-session | accepted | 2026-07-04 |
| [0047](0047-count-compaction-generations.md) | Count and surface a compaction generation ordinal | accepted | 2026-07-04 |
| [0048](0048-fold-spent-tool-results-behind-handles.md) | Fold spent tool results behind handles (opt-in microcompaction) | accepted | 2026-07-04 |
| [0049](0049-dangerously-skip-permissions-mode.md) | `--dangerously-skip-permissions` bypasses the approval gate | accepted | 2026-07-05 |
| [0050](0050-stream-reasoning-summary-deltas.md) | Stream reasoning summary deltas as a display event | accepted | 2026-07-05 |
| [0051](0051-cache-aware-fold-flush-scheduling.md) | Cache-aware fold flush scheduling | accepted | 2026-07-05 |
| [0052](0052-task-workflow-v2-opt-in-guard-and-integrated-settlement.md) | Task workflow v2 - opt-in workflow, always-on guard, and integrated settlement | accepted | 2026-07-07 |
| [0053](0053-load-codex-skills-as-contextual-messages.md) | Load Codex skills as contextual messages | accepted | 2026-07-09 |
| [0054](0054-use-model-aware-auto-compaction-trigger-ladder.md) | Use a model-aware auto-compaction trigger ladder | accepted | 2026-07-10 |
| [0055](0055-govern-context-between-provider-round-trips.md) | Govern context between provider round trips | accepted | 2026-07-10 |
| [0056](0056-persist-portable-summaries-beside-provider-compaction-blocks.md) | Persist portable summaries beside provider compaction blocks | accepted | 2026-07-10 |
| [0057](0057-cover-the-current-turn-under-hard-pressure-and-escalate-fallback.md) | Cover the current turn under hard pressure and escalate the fallback ladder | accepted | 2026-07-10 |
| [0058](0058-configure-mutation-safety-and-require-native-jj-consent.md) | Configure mutation safety and require native jj consent | accepted | 2026-07-11 |
| [0059](0059-web-search-returns-a-snippet-rich-list-not-a-server-summary.md) | web_search returns a snippet-rich result list, not a server summary | accepted | 2026-07-12 |
| [0060](0060-harness-actor-keeps-tui-input-always-live.md) | Own turns in a harness actor so TUI input stays always live | proposed | 2026-07-12 |

Note: ADR-0018 was not committed in repository history; numbering resumes at
ADR-0019 to preserve existing file names and cross-references.
