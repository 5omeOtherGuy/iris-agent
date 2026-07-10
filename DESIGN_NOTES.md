# TUI polish notes

Status: complete. Baseline: `origin/main` at `29a4dff`. Final repository gate: pass.

## Operating rules

1. Motion acknowledges cause. A launch, keystroke, state transition, or live data may move; an idle screen may not. Acknowledgments attack on the next frame, settle in 100–200 ms, and remain interruptible.
2. Terminal easing is quantized. Iris uses fast attack, a short hold, and a bounded release on the existing 100 ms beat. It does not add interpolation, a second timer, or ambient loops.
3. Reduced motion is immediate. Enabling it settles every active chase, detent, peak, after-image, paced stream, and start-page sequence in the same interaction. Data keeps updating.
4. State outranks decoration. REVIEW, RUNNING, ERROR, DENIED, and the current keyboard focus must remain identifiable in monochrome from a symbol, label, and stable position.
5. Tool blocks answer five questions in two settled rows: family, target, elapsed time, outcome, and measured cost. A failure also keeps its concise cause visible while the body is folded.
6. Truncation is honest. User, model, path, command, query, and tool text truncates only at grapheme boundaries and ends in `…`. Safety caps never discard content silently.
7. Width is display width. CJK, emoji, variation selectors, ZWJ sequences, and combining marks are measured as rendered clusters. Byte and scalar counts never drive layout.
8. Density follows state. Live and review surfaces expose bounded detail; settled history collapses; idle chrome stays minimal. A disclosure that clips content reports the hidden count.
9. Focus owns one unmistakable readout. The composer caret, selected menu row, REVIEW posture, or SCROLLBACK posture says both where keys land and the essential keys available there.
10. Overlays do not leak input. A visible job card, search/follow readout, menu, or composer row consumes its own hit region; clicks never reach transcript content painted underneath.
11. Scroll is an operator decision. Appends and fast streaming preserve an anchored viewport. Follow mode alone tracks the live tail. Resize may reflow text but may not tear a frame.
12. Palette capability is explicit. Truecolor keeps the selected theme, 256-color terminals receive xterm-indexed approximations, and 16-color terminals receive semantic ANSI roles with diff tones removed before they become saturated fills.
13. Rendering is atomic. Pager frames stay inside synchronized updates; inline frames keep the existing append/diff/replay contract. The polish layer does not introduce another terminal driver.
14. Narrow degradation is monotonic. Secondary diagnostics and hints drop as whole fields. Identity, state, focus, and decision controls survive first.
15. Disclosure marks disclose. Every non-redacted THINKING block has a real closed and open state; a `▸`/`▾` never decorates content that cannot change.
16. Consequential evidence stays open. Diff-bearing writes/edits and failed shell runs remain expanded after finalization unless the operator explicitly folds them.

## Phase 1 audit

The audit covered the canonical design language, TUI ADRs, both render backends, transcript/wrap/text engines, tool renderers and lifecycle replacement paths, pager scroll/focus/search/sticky-header composition, composer/statusline, overlays, start page, settings faceplate, terminal capability handling, and the existing UI test suite. Baseline result: 847 UI tests passed; two timing benchmarks were intentionally ignored.

1. **Idle motion violates the brief.** The start-page IrisMark enters a perpetual ping-pong sweep after its one-shot lamp test. It redraws an otherwise idle screen and is decorative rather than reactive.
2. **The faceplate's reduced-motion switch is not live.** `SaveSetting(ReducedMotion)` persists JSON, but the modal action path never calls `Screen::set_reduced_motion`; existing motion continues until restart.
3. **Entering reduced motion does not fully settle.** The spinner freezes on whichever chase cell was active, detent/exhale counters keep rendering, the flow peak waits for another tick, and an existing start page retains its motion posture.
4. **Palette depth is assumed.** Named themes always emit RGB and the adaptive theme uses 256-color indices for stdout, selection, and diff tones. There is no `COLORTERM`/`TERM` depth classification or semantic 16-color fallback.
5. **Tool header truncation is silent.** `frameless_header_line` hard-clips the command/path span. The documented `…` never appears, so two distinct long commands can render with the same unexplained cutoff.
6. **The tool safety cap is silent.** Command and output lines are cut at 2,000 grapheme clusters with no ellipsis before wrapping. The cap is correct; the missing disclosure is not.
7. **Folded failures lose their cause.** Finalization collapses tool bodies by design, but generic, EDIT, and SHELL error footers keep only `■ ERROR`; the concise error text exists solely inside the folded body.
8. **Pager scrollback focus can be invisible.** Tab removes the composer cursor, but with no selectable tool header there is no replacement focus cue or key legend. The unchanged `◉ CODE` statusline implies the composer still owns input.
9. **Pager readouts bypass the theme and clip queries.** Search/follow indicators use the terminal DIM modifier instead of the themed muted role. A long `/find` query is hard-clipped by the ratatui buffer, often removing match position and navigation keys without an ellipsis.
10. **The expanded job card can cover active focus.** The sticky prompt yields only when a selection/search match is exactly at the viewport top. A match on any later row covered by an expanded card is painted, highlighted, then overwritten.
11. **Pager overlays leak pointer input.** Header hit-testing does not stop at the transcript viewport. Clicks on composer rows, the search/follow row, or expanded job-card continuation rows can toggle a foldable transcript header underneath.
12. **Expanded job-card clipping is unreported.** When the prompt is taller than the available body, `rows.truncate(max_rows)` removes the tail and closing rule without a hidden-row count.
13. **Fast-stream anchoring lacks a frame-level regression pin.** `ScrollState` unit tests prove offsets survive total-row growth, but no composed-frame test appends a burst while scrolled away from the tail and asserts the same top row remains visible.
14. **The minimum-size contract lacks one integrated frame.** Individual tests cover 80×24, Unicode width, tool states, resize, and focus, but no single frame test combines a long Unicode tool lifecycle, live resize, and bottom chrome at the declared minimum.
15. **The design document contains stale behavior.** It sanctions the idle IrisMark sweep and a thinking-only live caret, describes short reasoning as non-foldable, and says every finalized tool body collapses—each contradicting the reactive-motion and consequential-evidence rules now required.
16. **Summary-only THINKING is not collapsible.** The rail exposes a fold anchor only when a distinct raw-reasoning channel exists. Ordinary reasoning summaries, including long ones, cannot be folded at all.
17. **The live print head is asymmetric.** THINKING appends an orange `▋` to generated text while the assistant answer stream does not. The glyph reads as a model-output cursor but describes only one of two model-output channels.
18. **Completed EDIT diffs collapse at the moment of consequence.** EDIT previews open for review, then the applied/error result rebuild defaults to folded. The exact write evidence disappears precisely when the mutation becomes real; final task-diff panels already use the correct expanded posture.
19. **Failed shell runs collapse their diagnostic body.** The running output is visible, but finalization rebuilds an errored SHELL block with the ordinary compact default. The failure state remains, while the command output that explains it vanishes.

## Implementation scope

The first implementation slice addresses findings 1–9 and 16–19 and adds direct tests. The three required iteration passes then re-audit the assembled surface and address findings 10–15 with evidence from rendered frames. This keeps the change concentrated in existing TUI seams: theme roles, text truncation, tool footer composition, lifecycle fold policy, screen posture, pager composition/hit-testing, and tests.

## Iteration pass 1

Theme: protect the operator's center of attention, then sharpen the two highest-value tool families.

1. **Found:** an expanded sticky job card yielded only when the selected or matched transcript line was its first row. A match on a continuation or closing-rule row was highlighted during transcript rendering and then painted over during frame composition. **Why it mattered:** search and keyboard focus appeared to disappear after Iris had already moved the viewport to the requested evidence. **Changed:** the overlay now calculates its complete rendered footprint and yields as one unit when any selected or matched line intersects that footprint. A composed 80×24 regression places a search match specifically under the expanded card's rule row.
2. **Found:** task-level multi-file diffs discarded both file headers and hunk headers. Change colors remained, but a 40-message history could not reliably answer which file or location a row belonged to. **Why it mattered:** the highest-consequence evidence in the session had less provenance than routine output. **Changed:** DIFF panels now render a quiet `FILE  path` section lane, retain dim `@@` location anchors, separate file sections by one blank rail row, suppress only duplicate raw `---`/`+++` headers, and remain expanded. Single-target EDIT keeps the target in its header and retains hunk anchors without repeating the path.
3. **Found:** an expanded SHELL block repeated the complete command in both its header and command row, while an empty running stream added a synthetic `$ █` row. Real output began without a clear transition from invocation to result. **Why it mattered:** the densest live tool looked cramped, duplicated its strongest text, and took longer to scan than its payload justified. **Changed:** folded history keeps the command in the header; the expanded posture moves it to one bright `$` invocation row, removes the synthetic prompt, and gives the first real output row a quiet `└` connector with continuation rows aligned beneath it. State, exit, test summary, elapsed time, and diagnostics retain their fixed rails.
4. **Verification:** the DIFF slice passed 52/52, the SHELL slice passed 60/60, the exact sticky-overlay regression passed, and the assembled TUI slice passed 512/512 with two intentional timing benchmarks ignored. A live 120×40 session confirmed the open and folded SHELL postures during a real command; no duplicate command or body row appeared.

## Iteration pass 2

Theme: make the composed frame—not hidden layout arithmetic—the source of interaction and spatial continuity.

1. **Found:** pager header clicks reconstructed a transcript line from `terminal::size()`, the scroll top, and the session-bar height. That second layout calculation continued through sticky continuations, search/follow readouts, filler, and composer rows. A real foldable header painted underneath any of those rows could toggle invisibly. **Why it mattered:** pointer input acted on something other than the surface under the pointer, and a resize could make the arithmetic disagree with the last visible frame. **Changed:** composition now records physical-row targets only for transcript headers that survive every overlay. Sticky disclosure owns its own recorded target; composer, search, follow, filler, tail chrome, sticky continuations, and the closing rule register no transcript hit. Regressions deliberately align real headers under each excluded row.
2. **Found:** live THINKING had no leading separator after a tool footer; commit inserted one and shifted the pane by a row. Its settled `RailEnd` already rendered one trailing blank, but the next block and the late-reasoning splice each added another. **Why it mattered:** the reasoning lifecycle visibly jumped at finalization and left inconsistent one/two-row voids—the exact instability reported from a live session screenshot. **Changed:** live reasoning mounts the same conditional leading separator as its committed form; `RailEnd` is now recognized as the block separator it renders; the late splice no longer appends a redundant blank. Three regressions cover live→settled parity, reasoning→tool, and late reasoning→existing answer.
3. **Found:** the collapsed sticky job card had no lower boundary. Its compact prompt sat between the session-bar rule and transcript content and was difficult to distinguish at a glance. **Why it mattered:** the governing task is the reader's orientation anchor, but its common collapsed posture receded into unrelated rows. **Changed:** both collapsed and expanded postures now close with the same quiet inset hairline. The compact posture remains one prompt row plus the rule, and the full two-row footprint participates in overlay yielding and pointer ownership.
4. **Found:** scroll offset behavior was unit-tested, but the assembled frame still lacked a burst/finalization pin. **Why it mattered:** hundreds of chunks can arrive in one loop interval; any implicit return to follow mode would look like rapid scrolling. **Changed:** a frame-level regression leaves follow mode, captures the top row, applies 600 immediate stream chunks, finalizes, and asserts the same top offset and rendered anchor through both frames.
5. **Verification:** all overlay-hit regressions, all THINKING boundary regressions, the sticky-band slice, and the 600-chunk anchor test passed. The assembled TUI slice then passed 519/519 with two intentional timing benchmarks ignored.

## Iteration pass 3

Theme: make every degradation explicit, then prove the complete minimum-frame lifecycle.

1. **Found:** expanded sticky prompts ended with `rows.truncate(max_rows)`. A long governing prompt silently lost its tail and could lose the closing rule, leaving neither a count nor a boundary. **Why it mattered:** the card claimed to be expanded while withholding unknown content, and its geometry changed at the viewport edge. **Changed:** a clipped band reserves its penultimate row for `… +N rows` and its last row for the hairline. Defensive one/two-row budgets still disclose loss instead of truncating silently. A five-row regression verifies identity, exact budget, disclosure, and boundary.
2. **Found:** the named edge cases were covered by separate unit tests, but no single minimum frame exercised a live high-value tool through Unicode output, width changes, interruption, and focus transfer. **Why it mattered:** individually correct widgets can still compete for the same 80×24 rows. **Changed:** an integrated test runs a long CJK/emoji/combining SHELL at 80×24, resizes live to 121×31 and back, appends after the resize, cancels, and transfers focus to scrollback. It asserts exact frame height, width safety, one-copy output, intact clusters, terminal cancellation state, and the visible focus/key readout.
3. **Found:** the canonical design language still specified an ambient IrisMark sweep, a thinking-only output caret, non-foldable short settled reasoning, universal finalized-tool collapse, command duplication between open SHELL header/body, and a top-row-only sticky yield. **Why it mattered:** implementation and its governing document prescribed different interaction grammars. **Changed:** the document now defines the static settled datum, no-caret output grammar, real settled THINKING disclosures, consequential-evidence fold policy, state-aware SHELL density, file/hunk DIFF lanes, frame-owned pointer targets, honest clipped job cards, and explicit truecolor/256/16-color degradation.
4. **Found at the acceptance gate:** the first frame-hit map resolved each viewport row through `panel_header_rows()`, rebuilding and scanning the complete transcript. The 10k-row benchmark crossed its 8× ceiling by 0.06×. **Why it mattered:** interaction correctness had reintroduced transcript-length frame cost, violating the pager's central O(viewport) contract. **Changed:** cumulative visible-line counts in the wrap cache now locate the owning logical row by binary search. The exact benchmark fell from roughly 5.3 seconds under the failed gate to 0.45 seconds in isolation; all overlay-hit regressions remain green.
5. **Verification:** palette-depth tests passed 4/4; text-engine Unicode/width tests passed 14/14; the clipping and integrated minimum-frame tests passed. A real 80×24 tmux session confirmed the settled start page, collapsed and expanded sticky rails, expanded SHELL command/output transition, single-gap THINKING rhythm, and SCROLLBACK key posture. The assembled TUI slice passed 521/521 with two intentional timing benchmarks ignored.

## Verification matrix

| Contract | Evidence |
|---|---|
| Minimum terminal and live resize | `minimum_frame_handles_unicode_shell_resize_cancel_and_focus`: 80×24 → 121×31 → 80×24 during a live SHELL; exact height and width bounds remain valid. |
| Long lines and honest truncation | Header/footer/search/job-card regressions preserve identity/state rails and end every real cut in `…`; tool safety caps disclose the cut. |
| CJK, emoji, combining marks, ZWJ | `cargo test ui::textengine --lib`: 14/14; integrated shell lifecycle verifies logical cluster survival before terminal cell expansion. |
| Truecolor, 256-color, 16-color | `cargo test ui::palette --lib`: 4/4; RGB quantizes at 256 colors and semantic ANSI roles replace RGB at 16 colors, with diff backgrounds reset. |
| Empty and first-run | Startup/empty-document tests plus a settled live 80×24 start page; idle ticks are inert after the one-shot lamp test. |
| Errors, denied, interrupted, cancelled | Generic/SHELL/EDIT failure footer tests, failed-SHELL open-state test, denial lifecycle tests, and the integrated cancelled live SHELL. |
| Fast streaming without tearing | A composed frame survives 600 immediate chunks and finalization with an unchanged scroll top and anchor; collector tests keep split multibyte graphemes intact. |
| Scrollback preservation | Offset-from-top scroll tests, burst regression, resize regression, and search reveal with a reserved indicator row. |
| Focus and keys | SCROLLBACK owns the statusline with or without a selected header; REVIEW outranks it; narrow hints drop as whole fields. Live 80×24 capture confirmed the posture. |
| Reduced motion | Immediate live setting application plus a single regression that settles spinner, detents, exhale, flow peak, startup, modal flash, and both stream escapements. |
| Inline and pager renderers | Incremental/full-replay parity tests cover the inline terminal surface; composed-frame/TestBackend tests and live tmux cover pager mode. |
| Transcript-length frame cost | `frame_cost_is_independent_of_transcript_length` passes after O(log rows) hit lookup; measured isolated run completed in 0.45 seconds. |
| DIFF and SHELL | `cargo test diff --lib`: 52/52; `cargo test shell_ --lib`: 60/60; assembled TUI suite: 521/521 with two intentional timing benchmarks ignored. |
| Repository acceptance | `bash scripts/gate.sh`: `fmt OK, clippy OK, test OK`. One unrelated compaction timing test failed once under full-suite load, passed immediately in isolation, and the unchanged full gate passed on retry. |

## Director's commentary

1. **DIFF became evidence, not decoration.** Keeping file lanes and hunk anchors while suppressing redundant git headers is the smallest change that makes a multi-file result trustworthy at scan speed. Leaving mutation diffs open completes the lifecycle: the write is most visible when it becomes real.
2. **SHELL now has one unmistakable reading path.** Folded history names the command; opening it moves the command to one bright invocation row, then `└` hands the eye to output. Removing the duplicate header command and fake prompt made the block faster without adding chrome.
3. **THINKING no longer changes the floor plan.** The live and settled forms share boundaries, the channel-specific caret is gone, and every recoverable trace has a real disclosure. The screenshot-derived one/two-gap bugs were small in code and large in perceived stability.
4. **The frame owns interaction.** Recording hit targets after overlays are composed is more faithful than reconstructing geometry during input. The follow-up performance correction matters equally: tactile correctness cannot cost O(history) per frame.
5. **Degradation is designed, not tolerated.** Grapheme-aware ellipses, 16/256-color roles, a truthful clipped job card, live reduced-motion settlement, and the integrated 80×24 lifecycle make narrow or limited terminals feel intentional.

## Acceptance self-grade

- [x] Zero known visual regressions; focused suites, assembled TUI tests, live inspection, clippy, and the full repository gate pass.
- [x] Every animation is interruptible and reduced motion settles or bypasses it immediately.
- [x] State changes acknowledge within the existing 100 ms beat; identity, state, and focus survive every narrow form.
- [x] Tool-call history is compact, while DIFF, failed SHELL, active output, errors, and review evidence remain visually dominant.
- [x] Every requested edge case is mapped to a test or live capture above.
- [x] All three iteration passes contain documented defects and material corrections.
