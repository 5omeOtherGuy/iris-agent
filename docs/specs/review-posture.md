# SPEC — The REVIEW posture

Status: approved for implementation · Surface: bottom statusline (§9.3), the
composer frame (§9.2), the approval moment (§8.5).
Complexity: MEDIUM-LOW. Iteration passes required after it works: 1.

---

## 0 · Goal

While a gated tool waits for the user's decision, the pane must read as
**attentive, not stalled** — and the user's eye, which rests at the composer,
must be redirected to the decision without any new motion or chrome.

Testable outcome: while `awaiting_approval` is true, (a) the bottom
statusline's leading segment renders `▲ REVIEW` in place of `◉ MODE` and the
rest of the statusline dims; (b) the composer's frame renders in the orange
accent; (c) an empty composer shows a decision-echo placeholder built from
the same affordance data as the gated block's footer; and (d) every one of
these reverts on approve/deny/cancel. Zero new animation; the tick loop stays
CPU-idle during the wait exactly as today.

## 1 · Why

Today the machine deliberately goes still during review (ticks stop, the
indicator steps aside, the composer defocuses) — correct CPU behavior, but
visually indistinguishable from a hang. The decision affordance lives only in
the gated block's footer. An instrument waiting on its operator shows a
steady attention lamp at the operator's hands. The composer IS the operator's
hands: its frame is the only hard chrome on screen (§6), which makes it the
one honest place to show "input surface re-purposed: decision wanted".
Everything here is a static state swap — the closed motion set (§6) is not
touched.

## 2 · The three cues (all static, all reverting)

### 2.1 Statusline: the posture segment

`◉ MODE ─ MODEL EFFORT ─ <policy>` becomes, while awaiting approval:

```
▲ REVIEW ─ MODEL EFFORT ─ ▲ on-request
```

- `▲` is the house REVIEW symbol (`symbols::REVIEW`), orange; `REVIEW` bold
  uppercase like MODE. This is a **state readout, not a new vocabulary** —
  the same symbol+label the gated block's footer already shows, echoed at the
  eye's resting place.
- Every other statusline segment (model button, effort, policy) renders
  **dim** while the posture is REVIEW — the line has one subject. The model
  "button" underline drops for the duration (it is not clickable while
  frozen anyway — verify; if it still responds, leave the underline).
- Narrow-width drop order unchanged; `▲ REVIEW` occupies the `◉ MODE` slot
  and is never dropped (it inherits MODE's minimum-form position).
- No flash on the transition. Ticks are stopped during the wait by design
  (CPU-idle, screen.rs `tick()`); a flash could never decay. The swap itself
  is the news. Do NOT re-enable ticking.

### 2.2 Composer frame: the bezel lamp

The composer's top edge — the only hard chrome on screen — renders in the
**orange accent** while awaiting approval (both the top border and the
internal rule above the statusline, whichever of the two the composer draws
as its frame; keep them consistent). Reverts to the normal frame tone on any
resolution. Color is reinforcement here, not the sole signal (the REVIEW text
carries state — monochrome test passes); one accent, no fill, no extra rows.

### 2.3 Composer placeholder: the decision echo

When the input buffer is empty, the placeholder (`Give Iris a task...`)
becomes a dim decision echo assembled from the **same affordance the block
footer offers** — never a hardcoded key list (keymap honesty; `a`/`p` appear
only when the loop actually offers them):

```
review waiting ┊ y approve ┊ n deny
review waiting ┊ y approve ┊ n deny ┊ a always ┊ p project
```

- Sentence-register, dim, `┊` separators (house).
- If the user had already typed text (queued steering), their text stays —
  never overwrite a buffer.
- On resolution the normal placeholder returns.

## 3 · Plumbing notes

- `Screen` already has `awaiting_approval: bool` (`show_approval` /
  `clear_approval` / the turn-error clears at screen.rs:1652,1679) — key all
  three cues on it; no new state machine. The affordance key set must travel
  into the screen at `show_approval` time (extend it to accept the offered
  decision set; the loop knows it when it renders the block footer — single
  source, pass the same struct/slice).
- All three cues must revert on EVERY exit path: approve, deny, cancel,
  turn-error cleanup (the two clear sites), and `/new`-style session resets.
  Grep every place `awaiting_approval` is set false and cover each with a
  test.
- Statusline renderer: `statusline_left` / the §9.3 composer chrome path in
  `screen.rs` (~2552 and `render_editor_chrome`).

## 4 · Design-language amendments (same change)

- §9.3: add the REVIEW posture row to the statusline spec (segment table +
  the "rest of the line dims" rule + never-dropped note).
- §9.2: one sentence — the composer frame is the machine's bezel lamp: normal
  tone at rest, accent while a review waits.
- §8.5: one cross-reference sentence (the statusline and composer echo the
  block's affordance while the decision is pending).

## 5 · Acceptance criteria

1. `awaiting_approval` true → statusline leading segment is `▲ REVIEW`
   (symbol orange, label bold), MODEL/EFFORT/policy segments all dim; false →
   exact prior rendering (byte-identical spans in a before/after test).
2. Composer frame style is the accent while waiting; normal otherwise; both
   the top edge and internal rule agree.
3. Empty buffer → placeholder echoes exactly the offered affordance set
   (test with {y,n} and {y,n,a,p}); non-empty buffer untouched.
4. Every `awaiting_approval = false` site (approve, deny, both cleanup paths)
   restores all three cues — one test per site.
5. `Screen::tick()` still returns false immediately while waiting (CPU-idle
   preserved — assert unchanged).
6. Narrow width: minimum statusline form is `▲ REVIEW ─ MODEL`.
7. Golden frame: composer + statusline in the waiting state.
8. `bash scripts/gate.sh` passes; §9.2/§9.3/§8.5 amended.

## 6 · Out of scope

- Any change to the gated block itself (§8.5 is right).
- Re-enabling ticks/animation during the wait.
- Sounds, bells, OSC 9 notifications (worth a future spec, not this one).
