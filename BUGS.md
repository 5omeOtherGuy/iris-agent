# TUI polish bug ledger

Scope: showcase audit and three iteration passes on `feat/tui-polish-showcase-codex`.

| ID | Defect | Cause | Fix and regression |
|---|---|---|---|
| B01 | The idle start page redrew forever. | The IrisMark transitioned from its one-shot lamp test into a ping-pong sweep with a comet trail. | Settle on one static center datum; `settled_page_has_no_idle_animation` proves ticks stay inert. |
| B02 | Changing reduced motion in Settings required a restart. | `SaveSetting(ReducedMotion)` persisted JSON but never updated the active `Screen`. | Apply successful reduced-motion, scroll-speed, and theme saves to the live TUI after persistence succeeds. |
| B03 | Entering reduced motion left motion artifacts active. | Spinner phase, detents, exhale, flow peak/hold, start-page motion, modal flash, and paced stream buffers had independent settle paths. | `Screen::set_reduced_motion` now settles or flushes every source in the same interaction; dedicated tests cover the complete state set. |
| B04 | Named themes assumed truecolor and 16-color diffs could become saturated fills. | Palette roles emitted RGB/indexed colors without classifying terminal capability. | Detect `COLORTERM`/`TERM` once; retain RGB, quantize to xterm-256, or map to semantic ANSI roles. Remove diff backgrounds at 16 colors. |
| B05 | Long tool targets ended at an unexplained hard edge. | Header composition hard-clipped spans after reserving the elapsed rail. | Preserve family identity and elapsed, then grapheme-ellipsize target metadata in the remaining cells. |
| B06 | Defensive 2,000-cluster caps silently discarded tool text. | The cap used raw cluster truncation without a disclosure marker. | Route capped command/output text through honest grapheme truncation ending in `…`. |
| B07 | A folded failure showed `ERROR` but not why. | Error detail existed only in the foldable body; footer extras omitted it. | Keep the first cleaned cause line in generic, SHELL, and EDIT footers. |
| B08 | Tab could move focus away from the composer without a visible new owner. | SCROLLBACK posture was conditional on selecting a foldable header. | SCROLLBACK always owns the composer statusline while focused and advertises only keys that fit. REVIEW still has higher priority. |
| B09 | Search/follow readouts ignored the theme and lost navigation state on narrow panes. | They used terminal `DIM` and relied on buffer clipping. | Use the themed muted role, grapheme-ellipsize the query, preserve the closing quote and match rail, and degrade hints by whole fields. |
| B10 | An expanded sticky job card painted over a selected or searched continuation/rule row. | Overlay yielding checked only whether the interactive line equaled the viewport top. | Compare selection/search lines with the entire rendered band footprint and yield the band as one stable unit. |
| B11 | Clicks on pinned chrome could fold a hidden transcript header. | Header hit-testing recomputed a logical line from terminal size and scroll offset after overlays had replaced rows. | Record physical header/sticky targets from the final composed frame; composer, filler, search/follow, sticky continuation/rule, and tail rows own no hidden target. |
| B12 | A tall expanded sticky prompt lost content and its closing rule silently. | `rows.truncate(max_rows)` discarded the tail without reserving disclosure or boundary rows. | Reserve `… +N rows` and the closing hairline inside the exact viewport budget. |
| B13 | Summary-only settled THINKING could not collapse. | Foldability required a distinct raw-reasoning channel. | Every non-redacted settled trace is a real disclosure: summary-only closes to its header and expands to the full summary; redacted traces remain non-foldable. |
| B14 | Live THINKING had an output caret while live assistant text did not. | The reasoning preview appended a channel-specific `▋`. | Remove the caret; the header lamp, elapsed rail, and paced text carry liveness for the same no-caret model-output grammar. |
| B15 | An applied EDIT hid its diff at finalization. | Rebuilding a preview/running edit used the generic settled-collapse default. | Keep every diff-backed EDIT expanded through preview, review, running, done, denied, and error unless the operator explicitly folds it. |
| B16 | A failed SHELL hid the output that explained the failure. | Error finalization used the successful settled-history collapse policy. | Failed SHELL stays expanded; explicit user fold state still survives rebuilds. |
| B17 | Multi-file task DIFF rows lost file and location provenance. | The shared parser suppressed raw file headers and hunk headers for both single-target and task diffs. | Retain dim hunk anchors; add `FILE  path` lanes and inter-file breathing rows for task DIFF while suppressing duplicate raw headers. |
| B18 | Expanded SHELL was cramped and duplicated its command; empty live output added a fake prompt row. | Header meta stayed mounted in the open posture, and the running placeholder synthesized `$ █`. | Folded history keeps command meta; open SHELL renders one `$` invocation, no fake row, and a `└` transition into real output with aligned continuations. |
| B19 | Live THINKING touched the preceding tool footer, then jumped down one row on commit. | The transient preview omitted the conditional leading separator that committed `push_blank` inserted. | Mount the same leading boundary in the first live frame; live→settled parity is regression-tested. |
| B20 | Settled THINKING left two blank rows before the next tool or an existing answer. | `RailEnd` already rendered one blank, but `begin_block` did not treat it as a separator and the late-reasoning splice appended another. | Make `RailEnd` the rendered separator and remove the redundant splice row. |
| B21 | A collapsed sticky prompt was difficult to distinguish from transcript content. | Only the expanded posture rendered the lower hairline. | Close both postures with the same quiet inset session-bar rule; collapsed remains one prompt row plus the boundary. |
| B22 | Error cleanup could weld separate lines before selecting the footer cause. | Control cleanup ran before newline splitting. | Split first, clean each candidate line, and retain the first meaningful cause. |
| B23 | The canonical design language contradicted implemented motion, THINKING, fold, SHELL, DIFF, sticky, and palette behavior. | Examples and invariants were not updated as interaction rules changed. | Revise `docs/TUI_DESIGN_LANGUAGE.md` to match the tested grammar and capability fallbacks. |
| B24 | The first composed-frame hit map made pager frame cost scale with transcript length. | Each visible body row called `panel_header_rows()`, allocating and scanning every logical row. | Resolve the owning row through cached cumulative visible counts in O(log rows); the 500-vs-10k benchmark and overlay regressions pass. |

## Verification gaps closed

| ID | Gap | Closure |
|---|---|---|
| V01 | No composed-frame regression for a reader anchored during a provider burst. | Apply 600 immediate stream chunks and finalize; the top offset and rendered anchor remain byte-identical. |
| V02 | No integrated minimum-frame lifecycle. | Run a long Unicode SHELL at 80×24, resize live to 121×31 and back, append, cancel, and transfer focus to SCROLLBACK. |
| V03 | No exact regression for a sticky focus hit beneath an expanded rule row. | Place the current search match on the card's continuation/rule footprint and assert its highlight survives. |
