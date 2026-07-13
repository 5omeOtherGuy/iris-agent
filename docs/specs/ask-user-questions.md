# SPEC — Ask-user-questions: model-driven multiple-choice clarification

Status: Iris implementation target, informed by the claude-code reference at
`~/vendor/claude-code` @ `6ba4060` (leaked tree; consult, do not vendor). Reference
behavior was captured 2026-07-13; the Iris goal and acceptance gate below are
prescriptive.

## Implementation goal

Give the model one read-only `AskUserQuestion` tool that pauses every call for a
human response and gives the user an Iris-native docked dialog for 1–4 structured
questions. Users can choose one or several labeled options, enter an automatic
`Other` answer, compare option previews, review multi-question answers, submit,
cancel, or hand the exchange back through `Chat about this`. Submitted answers
return as a successful tool result; conversational hand-back remains a rejected
result carrying model-visible feedback. Permission presets, including
never-ask and dangerous skip-permissions, must not bypass required interaction.

The first Iris slice deliberately excludes reference-only capabilities Iris does
not otherwise have: plan mode/interview skip, image attachments, HTML preview
hosts, analytics, and external-editor notes. Markdown preview text is supported;
the dialog follows Iris's docked-overlay grammar rather than copying Ink layout.

## Definition of done

- The live tool registry and generated provider declarations include
  `AskUserQuestion` with a strict schema: 1–4 unique questions, 2–4 unique option
  labels per question, 12-character headers, optional markdown previews,
  `multiSelect`, and dialog-populated answers/annotations.
- Model guidance states when to ask, how `Other`, `multiSelect`, recommendations,
  and previews work, and prevents use as a generic permission prompt.
- Nexus models required interaction separately from ordinary approval. Strict,
  auto, never-ask, and dangerous skip-permissions all wait for the user; cancel
  yields an ordinary denied result and `Chat about this` yields a denied result
  with bounded model-visible feedback.
- The TUI supports single-question auto-submit, multi-question navigation and
  review, single- and multi-select answers, free-text `Other`, focused markdown
  preview display, cancel, and `Chat about this`. The plain-text fallback can
  complete the same answer and feedback paths without a TTY.
- Automated tests cover schema boundaries/uniqueness, registration and guidance,
  answer/result encoding, permission-mode non-bypass, feedback relay, dialog
  navigation, `Other`, multi-select, preview rendering, cancel, and actor routing.
- Live checks exercise a single-select fast path, a multi-question review, an
  `Other` response, a preview, cancel, and `Chat about this` in a real tmux TUI.
  Captures must show the expected dialog/result state with no stuck turn.
- `bash scripts/gate.sh` passes with no skipped feature tests.

## Reference behavior

Purpose: document exactly how the reference implements a tool that lets the
model pause execution and ask the user 1–4 structured multiple-choice
questions, so Iris behavior stays grounded in observed behavior rather than
guesswork. Sections below describe the reference; the goal and definition of
done above define the Iris commitment.

Reference files:
- Tool: `src/tools/AskUserQuestionTool/AskUserQuestionTool.tsx`, `prompt.ts`
- Dialog: `src/components/permissions/AskUserQuestionPermissionRequest/*`
- State: `.../use-multiple-choice-state.ts`
- Routing: `src/components/permissions/PermissionRequest.tsx`,
  `src/hooks/toolPermission/handlers/interactiveHandler.ts`

---

## 0 · One-paragraph model

The model calls a tool named `AskUserQuestion` with a set of questions, each
carrying 2–4 labeled options. The tool never runs logic — it always returns
`behavior: 'ask'`, which routes to an interactive TUI dialog. The user picks
options (or types free text via a synthetic "Other" entry), optionally adds
notes/images, and submits. Submission resolves the tool call as an *allow*
whose result string relays the chosen answers back to the model. The dialog
also offers non-answer exits ("Chat about this", plan-interview skip, cancel)
that resolve the call as a *reject* carrying feedback text instead. The tool
is read-only and requires a human at the keyboard.

## 1 · Input schema

Top level (`z.strictObject`, refined):

- `questions`: array, **min 1, max 4**.
- `answers`: `Record<string,string>`, optional — populated by the dialog, not
  the model.
- `annotations`: `Record<questionText, {preview?, notes?}>`, optional.
- `metadata.source`: optional string, analytics only, not shown to the user.

Refinement (`UNIQUENESS_REFINE`): question texts must be unique across the
set, and option labels must be unique within each question. Violation fails
schema validation.

Each **question**:

- `question`: full text, should end with `?`. If `multiSelect`, phrase as a
  "which do you want to enable?" style prompt.
- `header`: chip/tab label, **max 12 chars** (`ASK_USER_QUESTION_TOOL_CHIP_WIDTH`).
- `options`: array, **min 2, max 4**. No `Other` option — it is injected
  automatically.
- `multiSelect`: bool, default `false`.

Each **option**:

- `label`: display text, 1–5 words.
- `description`: explanation / trade-off shown under the label.
- `preview`: optional artifact for side-by-side comparison (see §5).
  Single-select only.

## 2 · Tool contract

| Property | Value | Effect |
|---|---|---|
| `name` | `AskUserQuestion` | |
| `userFacingName()` | `''` | no tool-header line rendered |
| `isReadOnly()` | `true` | |
| `isConcurrencySafe()` | `true` | |
| `requiresUserInteraction()` | `true` | skipped by channel/relay approval paths |
| `shouldDefer` | `true` | deferred in the tool run loop |
| `maxResultSizeChars` | `100_000` | |
| `checkPermissions()` | always `{behavior:'ask'}` | always opens the dialog |
| `toAutoClassifierInput()` | question texts joined by ` \| ` | |

`isEnabled()`: returns `false` when a channels feature (`KAIROS` /
`KAIROS_CHANNELS`) is active with ≥1 allowed channel — the dialog would hang
with nobody watching the TUI, and `requiresUserInteraction` tools are skipped
by the channel relay, so there is no alternate approval path. Otherwise `true`.

`validateInput()`: no-op unless the host opted into `previewFormat: 'html'`,
in which case each option's `preview` is checked by `validateHtmlPreview`
(fragment only — rejects `<html>/<body>/<!doctype>`, rejects `<script>/<style>`,
requires at least one tag). Markdown previews are not validated here.

`prompt()`: base guidance always; appends preview guidance only when a preview
format is configured (SDK consumers that never render previews get none).

## 3 · Prompt guidance given to the model

From `prompt.ts` (`ASK_USER_QUESTION_TOOL_PROMPT`):

- Use to gather preferences, clarify ambiguity, get implementation decisions,
  or offer directional choices mid-execution.
- The user can always pick "Other" for free-text input.
- Set `multiSelect: true` when choices are not mutually exclusive.
- To recommend an option, make it first and append `(Recommended)` to its
  label.
- **Plan mode**: use to clarify requirements / choose approaches *before*
  finalizing a plan. Do NOT ask "Is my plan ready?" / "Should I proceed?" —
  that is `ExitPlanMode`'s job. Do not reference "the plan" in questions
  (the user cannot see it until `ExitPlanMode` is called).

Tool description string: "Asks the user multiple choice questions to gather
information, clarify ambiguity, understand preferences, make decisions or
offer them choices."

## 4 · Dialog behavior (single/multi, no preview)

Routing: `PermissionRequest.tsx` maps `AskUserQuestionTool ->
AskUserQuestionPermissionRequest`. The body parses `toolUseConfirm.input`
through the tool's own input schema; a parse failure yields an empty question
list (dialog renders nothing rather than crashing).

Layout per question (`QuestionView`):

- **Navigation bar** (`QuestionNavigationBar`): one chip per question showing
  `header` (or `Q{n}`) with a checkbox (`checkboxOn` once answered), the
  current chip inverse-highlighted, bracketed by `←`/`→`, and a trailing
  `✓ Submit` tab. Widths adapt to terminal columns; the current tab keeps up
  to half the available width, others share the rest (min 6 cols), truncated
  to fit.
- **Title**: the question text.
- **Options**: rendered via `Select` (single, `compact-vertical`) or
  `SelectMulti` (multi). Each real option plus a synthetic trailing
  `Other` entry of `type: 'input'` (label "Other") that opens an inline text
  field.
- **Footer choices**: `{n}. Chat about this`; in plan mode a second line
  `{n+1}. Skip interview and plan immediately`. Down-arrow from the last
  option focuses the footer; up-arrow returns.
- **Help line**: "Enter to select · {Tab/Arrow | ↑/↓} to navigate · [ctrl+g
  to edit in {editor} when Other focused] · Esc to cancel".

Navigation state (`use-multiple-choice-state.ts`, `useReducer`):
`currentQuestionIndex`, `answers` (by question text), per-question
`questionStates` (`selectedValue`, `textInputValue`), `isInTextInput`.
Actions: next/prev question, update-question-state, set-answer (optionally
advancing), set-text-input-mode. Tab keybindings are disabled while a text
input is focused (except in the submit view).

Answer encoding (`handleQuestionAnswer`):

- Single select → the option `label`.
- Multi select → selected labels joined by `, ` (plus the "Other" text if the
  Other box is checked).
- "Other" text → the typed string; if an image is attached, `"{text} (Image
  attached)"`, or `"(Image attached)"` when only an image is present.

**Fast path**: a single, non-multiSelect question hides the Submit tab
(`hideSubmitTab`) and auto-submits the instant an option is chosen — no
review step.

Otherwise, advancing past the last question lands on the **Submit view**
(`SubmitQuestionsView`): titled "Review your answers", lists each
`question -> answer`, warns "You have not answered all questions" when
incomplete, and offers `Submit answers` / `Cancel`.

## 5 · Preview mode (`PreviewQuestionView`)

Triggered when a single-select question has any option with a `preview`.
Layout switches to side-by-side: left = numbered vertical option list
(focused option marked, selected option shows `✓`), right = preview box
(markdown rendered in a monospace box; or HTML fragment for SDK hosts) plus a
notes field.

Keys: `↑/↓` (or `ctrl+p/n`) navigate options, `1`–`9` jump to an option,
`Enter` selects, `n` enters the notes input, `Esc` cancels, `ctrl+g` edits
notes in the external editor. Notes are stored as the question's
`textInputValue` and surface as an annotation. Content height/width is sized
dynamically from the longest preview (min height 12, min width 40, ~11 lines
of chrome overhead reserved; truncation past the fit).

## 6 · Outcomes and result relay

All resolutions run through `interactiveHandler.ts`'s queue callbacks
(`onAllow` / `onReject` / `onAbort`), guarded by a single-claim latch so only
the first resolver wins (local user, hook, classifier, bridge, or channel).

**Submit** → `toolUseConfirm.onAllow(updatedInput, [], undefined, imageBlocks)`
where `updatedInput = {...input, answers, annotations?}`. The tool's
`call()` echoes `{questions, answers, annotations?}`;
`mapToolResultToToolResultBlockParam` renders the tool_result content as:

```
User has answered your questions: "Q1"="A1", "Q2"="A2". You can now continue with the user's answers in mind.
```

Per-answer annotations append inline: `selected preview:\n{preview}` and
`user notes: {notes}`. Pasted images ride along as image content blocks
(resized/downsampled first).

**Chat about this** → `onReject(feedback, imageBlocks)` with feedback text:
"The user wants to clarify these questions… Start by asking them what they
would like to clarify. Questions asked: …" (each question with its answer or
"(No answer provided)"). This is a reject, not an allow — the model is asked
to reformulate.

**Skip interview and plan immediately** (plan mode only) →
`onReject(feedback)` telling the model to stop asking and finish the plan with
what it has.

**Cancel / Esc / abort** → `onReject()` with no feedback; the tool renders
"User declined to answer questions". Abort resolves via `cancelAndAbort`.

Transcript rendering after allow: a "User answered Claude's questions:" block
listing `· {question} → {answer}` per entry. `renderToolUseMessage`,
progress, and error messages all return `null` (the dialog is the UI).

## 7 · Analytics

Emitted only when `metadata.source` is set: `tengu_ask_user_question_accepted`
(with `answerCount`), `_rejected`, `_respond_to_claude`,
`_finish_plan_interview` — each tagged with `source`, `questionCount`,
`isInPlanMode`, and `interviewPhaseEnabled`.

## 8 · Notes for an Iris port

Observations, not commitments (Iris sequencing follows `docs/ROADMAP.md`):

- The "always ask" contract keeps the tool trivial and pushes all behavior
  into the approval/dialog layer — consistent with Iris keeping enforcement
  and interaction in the CLI tier, not the runtime.
- Reject-with-feedback as a first-class outcome (not just allow/deny) is the
  load-bearing idea: "Chat about this" turns a question into a conversational
  hand-back. Iris's tool-result/error encoding and approval handling would
  need a feedback channel on reject.
- The synthetic "Other" free-text option and the single-question auto-submit
  fast path are the two highest-value UX affordances.
- Preview mode, image paste, external-editor notes, plan-interview skip, and
  channel-relay gating are separable follow-ons, not core.
