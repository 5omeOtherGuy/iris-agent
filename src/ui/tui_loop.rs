//! The persistent async event loop that drives the full-screen TUI (Tier 3).
//!
//! One `tokio::select!` on the existing current-thread runtime multiplexes four
//! sources: terminal input (a dedicated OS thread feeding a channel, since
//! ratatui's crossterm re-export does not enable the `event-stream` feature),
//! the agent's `AgentEvent`s (pushed through [`LoopBridge`] into a channel), a
//! render tick that animates the spinner, and -- while a turn runs -- the
//! approval request channel. The turn itself is a single pinned
//! `harness.submit_turn` future polled by the same select, so the loop stays
//! responsive (scroll, spinner, approval) while the agent works.
//!
//! Cancellation: raw mode delivers Ctrl-C as a key event, not SIGINT. Because a
//! synchronous tool (`bash`) can block the executor thread, the input thread --
//! not the select loop -- cancels the active turn's [`CancellationToken`] the
//! moment it reads Ctrl-C, the same external-thread cancellation the old
//! per-turn watcher provided. The select loop then resolves any pending
//! approval as Deny so the turn unblocks and Nexus aborts it.
//!
//! Nexus is untouched: this loop only consumes its `AgentObserver` /
//! `ApprovalGate` seams via [`LoopBridge`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui_textarea::CursorMove;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::nexus::{
    AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ChatProvider, ToolCall,
};
use crate::ui::UiEvent;
use crate::ui::slash::{self, SlashAction};
use crate::ui::tui::{Screen, TuiUi};
use crate::wayland::Harness;

/// Spinner cadence. Input redraws are immediate (channel-driven), so this paces
/// only the spinner animation, not input latency; a 100ms beat is a smooth,
/// CPU-cheap spinner with a redraw only when the frame actually advances.
const TICK: Duration = Duration::from_millis(100);

/// The active turn's cancellation token, shared with the input thread so a raw
/// Ctrl-C cancels even while a synchronous tool blocks the executor thread.
type CurrentTurn = Arc<Mutex<Option<CancellationToken>>>;

/// Run the interactive full-screen session to completion on `runtime`, then
/// restore the terminal. `tui` already owns the alternate screen + raw mode.
pub(crate) fn run<P: ChatProvider>(
    harness: &mut Harness<P>,
    runtime: &Runtime,
    mut tui: TuiUi,
) -> Result<()> {
    let result = runtime.block_on(session_loop(harness, &mut tui));
    tui.shutdown();
    result
}

/// Outcome of the idle (between-turns) input phase.
enum IdleOutcome {
    Submit(String),
    Exit,
}

/// Per-key outcome inside the idle phase.
enum IdleKey {
    /// Handled with a visible state change: redraw.
    Continue,
    /// Event ignored (mouse move, key release): no redraw, stay CPU-idle.
    Ignore,
    Submit(String),
    Exit,
}

/// A gated tool waiting for the user's decision: the reply channel back into the
/// turn future plus whether "always" is on offer.
struct PendingApproval {
    reply: oneshot::Sender<ApprovalDecision>,
    allow_always: bool,
}

/// A review request crossing from the turn future into the loop.
struct ApprovalRequest {
    call: ToolCall,
    allow_always: bool,
    reply: oneshot::Sender<ApprovalDecision>,
}

async fn session_loop<P: ChatProvider>(harness: &mut Harness<P>, tui: &mut TuiUi) -> Result<()> {
    let (input_tx, mut input_rx) = unbounded_channel::<Event>();
    let current_turn: CurrentTurn = Arc::new(Mutex::new(None));
    spawn_input_thread(input_tx, current_turn.clone());

    let mut tick = interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tui.screen.apply_event(UiEvent::SessionStarted);
    tui.draw()?;

    loop {
        match idle_phase(tui, &mut input_rx, &mut tick).await? {
            IdleOutcome::Exit => break,
            IdleOutcome::Submit(prompt) => {
                let prompt = prompt.trim().to_string();
                if prompt.is_empty() {
                    continue;
                }
                // Safety net: a `/exit` typed after dismissing the palette still
                // exits via the registry, never reaching the model.
                if slash::is_exit(&prompt) {
                    break;
                }
                tui.screen.commit_user(&prompt);
                tui.screen.follow_bottom();
                tui.screen.start_turn();
                tui.draw()?;
                run_turn(
                    harness,
                    tui,
                    &mut input_rx,
                    &mut tick,
                    &current_turn,
                    &prompt,
                )
                .await?;
                tui.screen.end_turn();
                tui.draw()?;
            }
        }
    }
    Ok(())
}

/// Read and edit until the user submits a non-empty prompt or exits. The spinner
/// is idle here, so a tick redraws nothing.
async fn idle_phase(
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
) -> Result<IdleOutcome> {
    loop {
        tokio::select! {
            maybe = input_rx.recv() => {
                // The input thread only ends if terminal reads fail; treat as EOF.
                let Some(event) = maybe else { return Ok(IdleOutcome::Exit); };
                match handle_idle_event(&mut tui.screen, event) {
                    IdleKey::Continue => tui.draw()?,
                    IdleKey::Ignore => {}
                    IdleKey::Submit(text) => return Ok(IdleOutcome::Submit(text)),
                    IdleKey::Exit => return Ok(IdleOutcome::Exit),
                }
            }
            _ = tick.tick() => {}
        }
    }
}

/// Drive one agent turn, staying responsive to input, agent events, approval
/// requests, and the spinner tick. Returns when the turn future completes.
async fn run_turn<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    current_turn: &CurrentTurn,
    prompt: &str,
) -> Result<()> {
    let (event_tx, mut event_rx) = unbounded_channel::<UiEvent>();
    let (appr_tx, mut appr_rx) = unbounded_channel::<ApprovalRequest>();
    let bridge = LoopBridge { event_tx, appr_tx };

    // Clear any stale interrupt before arming, then publish the token so the
    // input thread can cancel this turn on Ctrl-C.
    crate::signals::reset();
    let token = CancellationToken::new();
    *current_turn.lock().expect("turn token lock poisoned") = Some(token.clone());

    let mut pending: Option<PendingApproval> = None;
    // Cleared once terminal input reaches EOF so the closed channel is no longer
    // polled (a closed `recv()` is always ready and would otherwise busy-loop).
    let mut input_open = true;

    let result = {
        let mut turn = std::pin::pin!(harness.submit_turn(prompt, &bridge, &bridge, &token));
        loop {
            tokio::select! {
                res = &mut turn => {
                    // The turn may finish in one poll after emitting a burst of
                    // events; drain them so none are lost.
                    while let Ok(event) = event_rx.try_recv() {
                        tui.screen.apply_event(event);
                    }
                    break res;
                }
                Some(event) = event_rx.recv() => {
                    tui.screen.apply_event(event);
                    tui.draw()?;
                }
                Some(request) = appr_rx.recv() => {
                    tui.screen.show_approval(&request.call, request.allow_always);
                    pending = Some(PendingApproval {
                        reply: request.reply,
                        allow_always: request.allow_always,
                    });
                    tui.draw()?;
                }
                maybe = input_rx.recv(), if input_open => {
                    match maybe {
                        Some(event) => {
                            // Authoritatively cancel here too: a Ctrl-C delivered
                            // in the submit/arm gap is read by the input thread
                            // while `current_turn` is None, so it never cancels.
                            // The event is still queued here, and a turn always
                            // opens with a cancel-biased, *yielding* provider
                            // stream before any executor-blocking tool (see
                            // nexus stream_turn), so this arm runs and cancels
                            // the token before bash can start. Cancel is
                            // idempotent with the input thread's own cancel.
                            if is_ctrl_c(&event) {
                                token.cancel();
                            }
                            if handle_running_event(&mut tui.screen, event, &mut pending) {
                                tui.draw()?;
                            }
                        }
                        None => {
                            // Terminal input ended (EOF): stop polling the closed
                            // channel and unblock the turn so it can complete
                            // instead of awaiting an answer that can never come.
                            input_open = false;
                            resolve_input_eof(&mut tui.screen, &mut pending, &token);
                        }
                    }
                }
                _ = tick.tick() => {
                    if tui.screen.tick() {
                        tui.draw()?;
                    }
                }
            }
        }
    };

    *current_turn.lock().expect("turn token lock poisoned") = None;
    // Any approval still pending here means the turn ended without resolving it
    // (cancellation); its receiver is already gone, so just drop it.
    drop(pending);

    if let Err(error) = result {
        tui.screen.apply_event(UiEvent::from_turn_error(&error));
    }
    tui.screen.clear_approval();
    Ok(())
}

/// Spawn the blocking terminal-read thread. It forwards every event to the loop
/// and, on a raw Ctrl-C while a turn is active, cancels that turn's token from
/// this OS thread (the executor thread may be blocked in a synchronous tool).
fn spawn_input_thread(tx: UnboundedSender<Event>, current_turn: CurrentTurn) {
    std::thread::spawn(move || {
        // Ends when terminal reads fail or the loop drops the receiver.
        while let Ok(event) = event::read() {
            if is_ctrl_c(&event) {
                // Hold the lock across the cancel so the turn cannot end and a
                // new one begin in between (which would leak a stale interrupt
                // and cancel the wrong turn). Matches the old watcher: set the
                // interrupt flag (a repeat reaps bash child groups), then cancel.
                let guard = current_turn.lock().expect("turn token lock poisoned");
                if let Some(token) = guard.as_ref() {
                    crate::signals::interrupt_from_terminal();
                    token.cancel();
                }
            }
            if tx.send(event).is_err() {
                break;
            }
        }
    });
}

fn is_ctrl_c(event: &Event) -> bool {
    matches!(event, Event::Key(key)
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && (key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat))
}

/// Insert pasted text as real lines (the multiline editor keeps newlines now,
/// unlike the old single-row flatten). `\r\n` is normalized to `\n`.
fn insert_paste(screen: &mut Screen, text: &str) {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            screen.editor.insert_newline();
        }
        screen.editor.insert_str(line.trim_end_matches('\r'));
    }
}

/// Idle-phase key map: edits the `TextArea`, drives the slash palette, scrolls
/// the transcript, submits, or exits. See the module docs for the binding list.
fn handle_idle_event(screen: &mut Screen, event: Event) -> IdleKey {
    let key = match event {
        Event::Paste(text) => {
            insert_paste(screen, &text);
            screen.sync_palette();
            return IdleKey::Continue;
        }
        Event::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::ScrollUp => screen.scroll_up(3),
                MouseEventKind::ScrollDown => screen.scroll_down(3),
                // Pointer motion and other mouse events change nothing; ignoring
                // them keeps an idle session from redrawing on every wiggle.
                _ => return IdleKey::Ignore,
            }
            return IdleKey::Continue;
        }
        Event::Resize(..) => return IdleKey::Continue,
        Event::Key(key) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
            key
        }
        _ => return IdleKey::Ignore,
    };

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let input = screen.editor_text();

    // Palette navigation takes priority while it is open with matches.
    if screen.palette.is_active(&input) {
        match key.code {
            KeyCode::Up => {
                screen.palette.up();
                return IdleKey::Continue;
            }
            KeyCode::Down => {
                screen.palette.down(&input);
                return IdleKey::Continue;
            }
            KeyCode::Esc => {
                screen.palette.dismiss();
                return IdleKey::Continue;
            }
            KeyCode::Tab => {
                if let Some(cmd) = screen.palette.accept(&input) {
                    screen.set_editor(cmd.name);
                }
                return IdleKey::Continue;
            }
            KeyCode::Enter if !alt => {
                if let Some(cmd) = screen.palette.accept(&input) {
                    return dispatch_action(cmd.action);
                }
                return IdleKey::Continue;
            }
            _ => {}
        }
    }

    match key.code {
        // --- control flow ---
        KeyCode::Char('c') if ctrl => {
            if screen.editor_is_empty() {
                return IdleKey::Exit;
            }
            screen.clear_editor();
            return IdleKey::Continue;
        }
        KeyCode::Char('d') if ctrl => {
            if screen.editor_is_empty() {
                return IdleKey::Exit;
            }
            return IdleKey::Continue;
        }
        KeyCode::Char('u') if ctrl => {
            screen.clear_editor();
            return IdleKey::Continue;
        }
        KeyCode::Enter if alt => screen.editor.insert_newline(),
        KeyCode::Enter => {
            let text = screen.submit();
            if text.trim().is_empty() {
                return IdleKey::Continue;
            }
            return IdleKey::Submit(text);
        }

        // --- transcript scroll ---
        KeyCode::PageUp => screen.scroll_up(10),
        KeyCode::PageDown => screen.scroll_down(10),
        KeyCode::Home if ctrl => screen.scroll_up(u16::MAX),
        KeyCode::End if ctrl => screen.follow_bottom(),

        // --- kill-ring / undo / redo ---
        KeyCode::Char('k') if ctrl => {
            screen.editor.delete_line_by_end();
        }
        KeyCode::Char('w') if ctrl => {
            screen.editor.delete_word();
        }
        KeyCode::Char('d') if alt => {
            screen.editor.delete_next_word();
        }
        KeyCode::Char('y') if ctrl => {
            screen.editor.paste();
        }
        KeyCode::Char('z') if ctrl => {
            screen.editor.undo();
        }
        KeyCode::Char('r') if ctrl => {
            screen.editor.redo();
        }

        // --- cursor / word navigation ---
        KeyCode::Char('a') if ctrl => screen.editor.move_cursor(CursorMove::Head),
        KeyCode::Char('e') if ctrl => screen.editor.move_cursor(CursorMove::End),
        KeyCode::Char('b') if alt => screen.editor.move_cursor(CursorMove::WordBack),
        KeyCode::Char('f') if alt => screen.editor.move_cursor(CursorMove::WordForward),
        KeyCode::Left if ctrl || alt => screen.editor.move_cursor(CursorMove::WordBack),
        KeyCode::Right if ctrl || alt => screen.editor.move_cursor(CursorMove::WordForward),
        KeyCode::Left => screen.editor.move_cursor(CursorMove::Back),
        KeyCode::Right => screen.editor.move_cursor(CursorMove::Forward),
        KeyCode::Up => screen.editor.move_cursor(CursorMove::Up),
        KeyCode::Down => screen.editor.move_cursor(CursorMove::Down),
        KeyCode::Home => screen.editor.move_cursor(CursorMove::Head),
        KeyCode::End => screen.editor.move_cursor(CursorMove::End),

        // --- deletion / insertion ---
        KeyCode::Backspace if alt => {
            screen.editor.delete_word();
        }
        KeyCode::Backspace => {
            screen.editor.delete_char();
        }
        KeyCode::Delete => {
            screen.editor.delete_next_char();
        }
        KeyCode::Tab => {
            screen.editor.insert_str("    ");
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            screen.editor.insert_char(c);
        }
        _ => {}
    }

    screen.sync_palette();
    IdleKey::Continue
}

fn dispatch_action(action: SlashAction) -> IdleKey {
    match action {
        SlashAction::Exit => IdleKey::Exit,
    }
}

/// Terminal input reached EOF while a turn is running: cancel the turn and
/// release any pending approval as Deny, so a turn awaiting an answer that can
/// no longer come still completes instead of spinning on the tick forever.
fn resolve_input_eof(
    screen: &mut Screen,
    pending: &mut Option<PendingApproval>,
    token: &CancellationToken,
) {
    token.cancel();
    if let Some(p) = pending.take() {
        let _ = p.reply.send(ApprovalDecision::Deny);
        screen.clear_approval();
    }
}

/// While a turn runs, the editor is frozen: handle only scroll, Ctrl-C, and (if
/// a tool is awaiting) the approval keys. Returns whether a redraw is needed.
fn handle_running_event(
    screen: &mut Screen,
    event: Event,
    pending: &mut Option<PendingApproval>,
) -> bool {
    match event {
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                screen.scroll_up(3);
                true
            }
            MouseEventKind::ScrollDown => {
                screen.scroll_down(3);
                true
            }
            _ => false,
        },
        Event::Resize(..) => true,
        Event::Key(key) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
                // The input thread already cancelled the token; unblock a pending
                // approval as Deny so Nexus observes the cancellation and aborts.
                if let Some(p) = pending.take() {
                    let _ = p.reply.send(ApprovalDecision::Deny);
                    screen.clear_approval();
                }
                return true;
            }
            // Scrolling works whether or not an approval is pending, so the
            // user can review transcript context before deciding.
            match key.code {
                KeyCode::PageUp => {
                    screen.scroll_up(10);
                    return true;
                }
                KeyCode::PageDown => {
                    screen.scroll_down(10);
                    return true;
                }
                KeyCode::Home if ctrl => {
                    screen.scroll_up(u16::MAX);
                    return true;
                }
                KeyCode::End if ctrl => {
                    screen.follow_bottom();
                    return true;
                }
                _ => {}
            }
            // Approval decision keys, only while a tool is awaiting one.
            if let Some(p) = pending.as_ref() {
                let decision = match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Allow),
                    KeyCode::Char('a') | KeyCode::Char('A') if p.allow_always => {
                        Some(ApprovalDecision::AllowAlways)
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                        Some(ApprovalDecision::Deny)
                    }
                    _ => None,
                };
                if let Some(decision) = decision {
                    let p = pending.take().expect("pending approval present");
                    let _ = p.reply.send(decision);
                    screen.clear_approval();
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Tier-3 adapter that backs Nexus's two front-end seams with the loop's
/// channels: events are pushed to the render channel, and a review request is
/// sent with a oneshot the loop resolves from the user's keypress.
struct LoopBridge {
    event_tx: UnboundedSender<UiEvent>,
    appr_tx: UnboundedSender<ApprovalRequest>,
}

impl AgentObserver for LoopBridge {
    fn on_event(&self, event: crate::nexus::AgentEvent) -> Result<()> {
        // The loop drives the turn, so the receiver outlives every send; a send
        // error would only mean the loop is gone, in which case dropping is fine.
        let _ = self.event_tx.send(UiEvent::from_agent_event(event));
        Ok(())
    }
}

impl ApprovalGate for LoopBridge {
    fn review<'a>(&'a self, call: &'a ToolCall, allow_always: bool) -> ApprovalFuture<'a> {
        let appr_tx = self.appr_tx.clone();
        let call = call.clone();
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            if appr_tx
                .send(ApprovalRequest {
                    call,
                    allow_always,
                    reply,
                })
                .is_err()
            {
                return Ok(ApprovalDecision::Deny);
            }
            // Safe-by-default: if the loop drops the reply, deny.
            Ok(rx.await.unwrap_or(ApprovalDecision::Deny))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tui::Screen;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn idle_typing_then_enter_submits() {
        let mut screen = Screen::new();
        for c in "hello".chars() {
            assert!(matches!(
                handle_idle_event(&mut screen, key(KeyCode::Char(c))),
                IdleKey::Continue
            ));
        }
        match handle_idle_event(&mut screen, key(KeyCode::Enter)) {
            IdleKey::Submit(text) => assert_eq!(text, "hello"),
            _ => panic!("expected submit"),
        }
        assert!(screen.editor_is_empty(), "editor cleared after submit");
    }

    #[test]
    fn alt_enter_inserts_newline_without_submitting() {
        let mut screen = Screen::new();
        handle_idle_event(&mut screen, key(KeyCode::Char('a')));
        handle_idle_event(&mut screen, key_mod(KeyCode::Enter, KeyModifiers::ALT));
        handle_idle_event(&mut screen, key(KeyCode::Char('b')));
        assert_eq!(screen.editor_text(), "a\nb");
    }

    #[test]
    fn ctrl_c_exits_on_empty_and_clears_on_nonempty() {
        let mut screen = Screen::new();
        // Non-empty: clears.
        handle_idle_event(&mut screen, key(KeyCode::Char('x')));
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
        assert!(screen.editor_is_empty());
        // Empty: exits.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            IdleKey::Exit
        ));
    }

    #[test]
    fn slash_enter_runs_exit_command() {
        let mut screen = Screen::new();
        handle_idle_event(&mut screen, key(KeyCode::Char('/')));
        handle_idle_event(&mut screen, key(KeyCode::Char('e')));
        assert!(screen.palette.is_active(&screen.editor_text()));
        // Enter accepts the highlighted /exit command.
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::Exit
        ));
    }

    #[test]
    fn slash_tab_completes_then_esc_dismisses() {
        let mut screen = Screen::new();
        handle_idle_event(&mut screen, key(KeyCode::Char('/')));
        handle_idle_event(&mut screen, key(KeyCode::Char('q')));
        // Tab completes to the full command.
        handle_idle_event(&mut screen, key(KeyCode::Tab));
        assert_eq!(screen.editor_text(), "/quit");
        // Esc dismisses; a later Enter then submits the literal text, which the
        // session loop routes to exit via the registry.
        handle_idle_event(&mut screen, key(KeyCode::Esc));
        assert!(!screen.palette.is_active(&screen.editor_text()));
        match handle_idle_event(&mut screen, key(KeyCode::Enter)) {
            IdleKey::Submit(text) => assert!(slash::is_exit(&text)),
            _ => panic!("expected submit of /quit"),
        }
    }

    #[test]
    fn kill_word_and_yank_via_keymap() {
        let mut screen = Screen::new();
        for c in "alpha beta".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        // Ctrl-W kills "beta".
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "alpha ");
        // Ctrl-Y yanks it back.
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('y'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "alpha beta");
        // Ctrl-Z undoes the yank.
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('z'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "alpha ");
    }

    #[test]
    fn running_event_approval_keys_resolve_oneshot() {
        let mut screen = Screen::new();
        // Allow.
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            reply: tx,
            allow_always: true,
        });
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Char('y')),
            &mut pending,
        ));
        assert!(pending.is_none());
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Allow);

        // Deny via 'n'.
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            reply: tx,
            allow_always: false,
        });
        handle_running_event(&mut screen, key(KeyCode::Char('n')), &mut pending);
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Deny);

        // 'a' is ignored when always is not on offer.
        let (tx, mut rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            reply: tx,
            allow_always: false,
        });
        assert!(!handle_running_event(
            &mut screen,
            key(KeyCode::Char('a')),
            &mut pending,
        ));
        assert!(pending.is_some());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn running_ctrl_c_denies_pending_approval() {
        let mut screen = Screen::new();
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            reply: tx,
            allow_always: true,
        });
        assert!(handle_running_event(
            &mut screen,
            key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut pending,
        ));
        assert!(pending.is_none());
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Deny);
    }

    #[test]
    fn running_scroll_keys_redraw_without_approval() {
        let mut screen = Screen::new();
        let mut pending: Option<PendingApproval> = None;
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::PageUp),
            &mut pending
        ));
        // A bare char while computing is ignored (editor frozen).
        assert!(!handle_running_event(
            &mut screen,
            key(KeyCode::Char('x')),
            &mut pending,
        ));
    }

    #[test]
    fn scroll_works_while_approval_pending() {
        let mut screen = Screen::new();
        let (tx, _rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            reply: tx,
            allow_always: true,
        });
        // PageUp scrolls without consuming the pending approval.
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::PageUp),
            &mut pending
        ));
        assert!(pending.is_some(), "scrolling does not answer the approval");
    }

    #[test]
    fn input_eof_cancels_turn_and_denies_pending() {
        let mut screen = Screen::new();
        let token = CancellationToken::new();
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            reply: tx,
            allow_always: true,
        });
        resolve_input_eof(&mut screen, &mut pending, &token);
        assert!(token.is_cancelled(), "EOF cancels the turn token");
        assert!(pending.is_none(), "EOF takes the pending approval");
        assert_eq!(
            rx.blocking_recv().unwrap(),
            ApprovalDecision::Deny,
            "EOF resolves the pending approval as Deny"
        );
    }

    #[test]
    fn input_eof_without_pending_just_cancels() {
        let mut screen = Screen::new();
        let token = CancellationToken::new();
        let mut pending: Option<PendingApproval> = None;
        resolve_input_eof(&mut screen, &mut pending, &token);
        assert!(token.is_cancelled());
    }

    #[test]
    fn is_ctrl_c_matches_press_and_repeat_only() {
        use ratatui::crossterm::event::KeyEvent;
        let press = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(is_ctrl_c(&press));
        let upper = Event::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL));
        assert!(is_ctrl_c(&upper));
        // Plain 'c' and Ctrl with another key are not Ctrl-C.
        assert!(!is_ctrl_c(&Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        ))));
        assert!(!is_ctrl_c(&Event::Key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL
        ))));
        // A key Release is not an interrupt.
        let mut release = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        release.kind = KeyEventKind::Release;
        assert!(!is_ctrl_c(&Event::Key(release)));
    }
}
