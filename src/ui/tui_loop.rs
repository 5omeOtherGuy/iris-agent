//! The persistent async event loop that drives the terminal-surface TUI (Tier 3).
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

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui_textarea::CursorMove;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::cli::{LoadedSource, ModelSwitch, SessionLoader, SessionSource, StartupUi};
use crate::git::status::{GitStatusCache, VcsStatus};
use crate::mimir::auth::storage::AuthStore;
use crate::mimir::model_catalog;
use crate::mimir::selection::ModelSelection;
use crate::nexus::{
    AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ChatProvider, PermissionMode,
    ReviewContext, ToolCall,
};
use crate::tool_display::approval_dirty_note;
use crate::ui::UiEvent;
use crate::ui::login::{self, LoginBackend, LoginOutcome, LoginUpdate, OAuthLoginBackend};
use crate::ui::modal::{LoginDialog, Modal, ModalAction, ModalKey, ModalOutcome};
use crate::ui::picker::{self, ActionResult, ModelCommand};
use crate::ui::settings_menu;
use crate::ui::slash::{self, SlashAction, SlashCommand};
use crate::ui::steering::SteeringQueue;
use crate::ui::tui::{
    ApprovalPolicy, FocusTarget, GitMenu, JjMenu, MenuAction, MenuKey, MenuOutcome, Screen,
    SessionMenu, StartAction, SwitchCacheStatus, SwitchStatus, TreeMenu, TuiUi,
};
use crate::wayland::Harness;
use crate::wayland::git_safety::RecoveryOutcome;
use crate::wayland::git_safety::git as git_cmd;

/// Spinner cadence. Input redraws are immediate (channel-driven), so this paces
/// only the spinner animation, not input latency; a 100ms beat is a smooth,
/// CPU-cheap spinner with a redraw only when the frame actually advances.
const TICK: Duration = Duration::from_millis(100);

/// Minimum interval between coalesced renders during an active turn (~60fps).
/// Mirrors pi-mono's 16ms render budget: an active turn can emit a burst of
/// agent events, and drawing on each one is wasteful and causes flicker.
const MIN_RENDER_INTERVAL: Duration = Duration::from_millis(16);

/// Trailing-edge debounce for width-changing resize redraws, idle and
/// mid-turn. A width change forces a full document replay, so drag storms
/// (tmux pane drags resize continuously) collapse to one redraw after the
/// terminal settles. Height-only resizes remain immediate unless a width
/// replay is already pending.
const RESIZE_REDRAW_DEBOUNCE: Duration = Duration::from_millis(50);

/// Debounced idle git-status poll interval: the session bar's git segment is
/// refreshed in the background at most this often while idle (event triggers
/// -- turn completion, tool terminal states, dropdown open -- refresh sooner).
const GIT_POLL: Duration = Duration::from_secs(10);

/// What a [`RenderScheduler`] decides to do for a pending render request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderAction {
    /// A render is due now; the caller should draw and call [`RenderScheduler::mark_drawn`].
    DrawNow,
    /// A render is pending but not yet due; the caller should wait until this
    /// instant, then draw.
    Wait(Instant),
    /// Nothing pending; stay idle (no timer wakeups).
    Idle,
}

/// Coalesces a burst of render requests to roughly one draw per
/// [`MIN_RENDER_INTERVAL`]. The first request after a draw (or after idle) draws
/// immediately; subsequent requests within the interval are deferred to a single
/// flush at the interval boundary. Pure and clock-injectable so the coalescing
/// policy is unit-tested without real-time sleeps.
struct RenderScheduler {
    last_draw: Option<Instant>,
    pending: bool,
    /// Trailing-edge hold: no draw before this instant. Set on width-changing
    /// resizes so a tmux pane-drag storm collapses to one full replay after the
    /// terminal settles instead of one per coalescing window.
    hold: Option<Instant>,
}

impl RenderScheduler {
    fn new() -> Self {
        Self {
            last_draw: None,
            pending: false,
            hold: None,
        }
    }

    /// Mark a render as wanted. Coalesces: many requests collapse into one
    /// pending flag until the next draw.
    fn request(&mut self) {
        self.pending = true;
    }

    /// Defer any pending or future draw to at least `until`. Repeated calls
    /// extend the hold (trailing edge), never shorten it.
    fn hold_until(&mut self, until: Instant) {
        self.hold = Some(self.hold.map_or(until, |hold| hold.max(until)));
    }

    /// Decide what to do for the current pending state at `now`.
    fn poll(&self, now: Instant) -> RenderAction {
        if !self.pending {
            return RenderAction::Idle;
        }
        // First draw after idle is immediate (startup/forced responsiveness);
        // afterwards pace to the coalescing window. An active hold defers both.
        let mut next = match self.last_draw {
            None => now,
            Some(last) => last + MIN_RENDER_INTERVAL,
        };
        if let Some(hold) = self.hold {
            next = next.max(hold);
        }
        if now >= next {
            RenderAction::DrawNow
        } else {
            RenderAction::Wait(next)
        }
    }

    /// Record that a draw just happened at `now`, clearing the pending flag and
    /// resetting the pacing window (and any hold the draw satisfied). Use for
    /// both coalesced and forced draws.
    fn mark_drawn(&mut self, now: Instant) {
        self.pending = false;
        self.last_draw = Some(now);
        self.hold = self.hold.filter(|hold| *hold > now);
    }
}

/// The active turn's cancellation token, shared with the input thread so raw
/// Ctrl-C (and Esc when no higher-priority UI owns it) cancels even while a
/// synchronous tool blocks the executor thread.
struct RunningTurn {
    token: CancellationToken,
    esc_cancels: bool,
}

type CurrentTurn = Arc<Mutex<Option<RunningTurn>>>;

fn set_esc_cancel_enabled(current_turn: &CurrentTurn, enabled: bool) {
    if let Some(turn) = current_turn
        .lock()
        .expect("turn token lock poisoned")
        .as_mut()
    {
        turn.esc_cancels = enabled;
    }
}

/// Run the interactive terminal-surface session to completion on `runtime`, then
/// restore the terminal. `tui` already owns raw mode and paste/key flags.
pub(crate) fn run<P: ChatProvider>(
    harness: &mut Harness<P>,
    runtime: &Runtime,
    mut tui: TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    swap: &SessionLoader<'_>,
    startup: StartupUi,
) -> Result<()> {
    let result = runtime.block_on(session_loop(harness, &mut tui, switch, swap, startup));
    let receipt = tui.screen.session_receipt();
    tui.shutdown();
    // The exit receipt: one dim, measured line printed AFTER teardown so it
    // lands on the normal screen in both modes — in pager mode it is the only
    // trace of the run left in scrollback; inline it closes the transcript.
    if let Some(receipt) = receipt {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            println!("\x1b[2m{receipt}\x1b[0m");
        } else {
            println!("{receipt}");
        }
    }
    result
}

/// What routing a submitted `/` command decided. `Consumed` = handled (a modal
/// may now be open); `Fall` = not a command, run it as a normal turn;
/// `Swap` = perform an in-session session swap at the boundary.
enum RouteOutcome {
    Consumed,
    Fall,
    Swap(SessionSource),
    /// Run an on-demand compaction at the boundary (driven like a turn, so the
    /// provider-backed summarizer stays cancellable and the spinner runs).
    Compact(String),
}

/// Outcome of the idle (between-turns) input phase.
enum IdleOutcome {
    Submit(String),
    Exit,
    /// Ctrl+L: open the model picker.
    OpenModelPicker,
    /// `$`: open the Codex-compatible skill mention picker.
    OpenSkillPicker,
    /// Ctrl+P (forward) / Shift+Ctrl+P (backward): cycle the model.
    CycleModel(bool),
    /// Shift+Tab: cycle the thinking/effort level.
    CycleEffort,
    /// Start-page launcher: open the resume-session picker.
    OpenResumePicker,
    /// Start-page launcher (`ctrl-t`): open the unified task surface (`/tasks`).
    OpenTasks,
    /// Start-page launcher: open the settings picker.
    OpenSettings,
    /// Toggle the git console dropdown (ctrl-g / click / `/git`).
    ToggleGitMenu,
    /// Toggle the directory tree dropdown (`@`-entry / click / `/tree`).
    /// `true` = open directly in filter mode (the `@` file-reference idiom).
    ToggleTreeMenu(bool),
    /// A dropdown emitted a side effect for the loop to execute.
    Menu(MenuAction),
}

/// Per-key outcome inside the idle phase.
enum IdleKey {
    /// Handled with a visible state change: redraw.
    Continue,
    /// Event ignored (mouse move, key release): no redraw, stay CPU-idle.
    Ignore,
    Submit(String),
    Exit,
    OpenModelPicker,
    OpenSkillPicker,
    CycleModel(bool),
    CycleEffort,
    OpenResumePicker,
    OpenTasks,
    OpenSettings,
    ToggleGitMenu,
    ToggleTreeMenu(bool),
    Menu(MenuAction),
}

/// A gated tool waiting for the user's decision: the reply channel back into the
/// turn future plus whether "always" is on offer.
struct PendingApproval {
    call: ToolCall,
    reply: oneshot::Sender<ApprovalDecision>,
    allow_always: bool,
    allow_project: bool,
}

/// A review request crossing from the turn future into the loop.
struct ApprovalRequest {
    call: ToolCall,
    allow_always: bool,
    allow_project: bool,
    /// Whether the `always` choice is the dirty-tree variant (`a all dirty
    /// files (this task)`), so the composer's decision echo renders the same
    /// affordance label the block footer does.
    dirty_gate: bool,
    reply: oneshot::Sender<ApprovalDecision>,
}

/// A short, danger-toned caution for the in-block `▲ REVIEW` footer, built from
/// the structured review facts (never model-authored copy): the destructive
/// floor (ADR-0010), pre-existing dirty-tree paths the call would touch
/// (ADR-0028), and — for a shell call on a platform with no kernel sandbox — an
/// `unsandboxed` posture. `None` when the call is unremarkable.
fn review_reason(call: &ToolCall, ctx: &ReviewContext) -> Option<String> {
    let mut parts = Vec::new();
    if ctx.destructive {
        parts.push("destructive".to_string());
    }
    if let Some(note) = approval_dirty_note(&ctx.dirty_paths, 96) {
        parts.push(note);
    }
    if call.name == "bash" && !crate::tools::platform_can_sandbox() {
        parts.push("unsandboxed".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn effective_approval_policy<P: ChatProvider>(harness: &Harness<P>) -> ApprovalPolicy {
    if harness.skip_permissions() {
        ApprovalPolicy::SkipPermissions
    } else {
        ApprovalPolicy::from(harness.approval_mode())
    }
}

async fn session_loop<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    swap: &SessionLoader<'_>,
    startup: StartupUi,
) -> Result<()> {
    let StartupUi {
        modal: startup_modal,
        mut followup_modal,
        start_page,
        resumed_session,
    } = startup;
    let (input_tx, mut input_rx) = unbounded_channel::<Event>();
    let current_turn: CurrentTurn = Arc::new(Mutex::new(None));

    // Mid-run steering/follow-up queue, shared with the harness so a turn drains
    // what the user types while it runs. Installed once for the session; the
    // loop keeps its own `Rc` clone to enqueue from the input arm. `Rc` is fine:
    // the whole session runs on the current-thread runtime.
    let steering = Rc::new(SteeringQueue::default());
    harness.set_steering_source(steering.clone());

    let mut tick = interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tui.screen.apply(UiEvent::SessionStarted);
    // Show the effective approval posture: skip-permissions overrides the
    // normal approval preset because it bypasses every prompt.
    tui.screen
        .set_approval_policy(effective_approval_policy(harness));
    // Async git-status snapshots for the session bar + dropdowns: kick the
    // first capture at session start; last-known values paint until it lands.
    let git_cache = GitStatusCache::with_task_workflow(harness.task_workflow_enabled());
    let mut git_generation = 0u64;
    git_cache.request_refresh(std::env::current_dir().unwrap_or_default());
    // On startup, reconcile any crashed/unsettled Iris task in this repo and
    // expire stale ones (issue #263, ADR-0028): auto-adopt the single orphan
    // (notice) or note the >1/legacy case (#288, ADR-0031). The recoverable
    // count feeds the start page's `Tasks` badge; a picker is never forced over
    // the home menu -- the user opens Tasks (or `/tasks`) to review or adopt.
    let recovery = resumed_session.as_deref().map_or_else(
        || harness.recover_checkpoints(),
        |session_id| harness.recover_checkpoints_for_resumed_session(session_id),
    );
    let recoverable = recovery.recoverable_count();
    // The start page (IrisMark + launcher) shows only for an interactive launch
    // with no task and no resume target; a startup resume picker supersedes it.
    if start_page && startup_modal.is_none() {
        let punctuation_chords = tui.keyboard_enhanced();
        tui.screen.show_start_page(recoverable, punctuation_chords);
    }
    refresh_footer(tui, switch);
    apply_recovery(recovery, tui);
    // `iris resume` (no id) on a rich TTY opens the resume picker on start by
    // handing a pre-built modal here. Open it before the first draw and before
    // the blocking input reader starts, so the first key acts on a visible
    // picker.
    if let Some(modal) = startup_modal {
        tui.screen.open_modal(modal);
    }
    // Startup initialization is settled: arm the detent flashes so that from
    // the first frame on, a changed statusline segment / meter LED announces
    // itself with one quantized blink.
    tui.screen.arm_detents();
    tui.draw()?;
    // Draw once before starting the blocking input reader so the banner/picker
    // is visible immediately and the terminal surface has its initial dimensions.
    spawn_input_thread(input_tx, current_turn.clone());

    // The production OAuth backend; `/login` drives it on a blocking task.
    let login_backend: Arc<dyn LoginBackend> = Arc::new(OAuthLoginBackend);

    loop {
        // Keep the status footer current: a model/effort change handled in the
        // previous iteration (chord, picker, or modal) is reflected before the
        // next idle draw.
        refresh_footer(tui, switch);
        sync_git_status(tui, &git_cache, &mut git_generation);
        // Run any open picker/dialog first: the startup resume picker, or one a
        // command/keybinding opened in the previous iteration. A `/resume`
        // selection returns the chosen session to swap to at this safe boundary.
        if tui.screen.focus() == FocusTarget::Modal {
            let requested = run_modal_phase(
                harness,
                tui,
                &mut input_rx,
                &mut tick,
                switch,
                &login_backend,
                &current_turn,
                steering.as_ref(),
                &git_cache,
                &mut git_generation,
            )
            .await?;
            if let Some(source) = requested {
                perform_swap(&source, swap, harness, tui, switch)?;
            }
            if tui.screen.focus() != FocusTarget::Modal
                && let Some(modal) = followup_modal.take()
            {
                tui.screen.open_modal(modal);
            }
            refresh_footer(tui, switch);
            tui.draw()?;
            continue;
        }
        match idle_phase(
            tui,
            &mut input_rx,
            &mut tick,
            &git_cache,
            &mut git_generation,
        )
        .await?
        {
            IdleOutcome::Exit => break,
            IdleOutcome::ToggleGitMenu => {
                toggle_git_menu(&mut tui.screen, &git_cache);
            }
            IdleOutcome::ToggleTreeMenu(filter) => {
                toggle_tree_menu(&mut tui.screen, &git_cache, filter);
            }
            IdleOutcome::Menu(action) => {
                execute_menu_action(action, harness, tui, &git_cache);
            }
            IdleOutcome::OpenModelPicker => {
                if let Some(sw) = switch.as_mut() {
                    let before = sw.selection().clone();
                    match picker::model_command("", harness, sw) {
                        ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                        ModelCommand::Lines(lines) => {
                            let after = sw.selection().clone();
                            apply_model_switch_lines(
                                tui,
                                harness,
                                Some(&before),
                                Some(&after),
                                lines,
                            );
                        }
                    }
                }
            }
            IdleOutcome::OpenSkillPicker => {
                if harness.skills().is_empty() {
                    apply_notices(tui, vec!["No skills are installed.".to_string()]);
                } else {
                    tui.screen
                        .open_modal(Modal::Skills(crate::ui::modal::SkillPicker::new(
                            harness.skills(),
                        )));
                }
            }
            IdleOutcome::CycleModel(forward) => {
                if let Some(sw) = switch.as_mut() {
                    let before = sw.selection().clone();
                    match picker::cycle_model(forward, harness, sw) {
                        ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                        ModelCommand::Lines(lines) => {
                            let after = sw.selection().clone();
                            apply_model_switch_lines(
                                tui,
                                harness,
                                Some(&before),
                                Some(&after),
                                lines,
                            );
                        }
                    }
                }
            }
            IdleOutcome::CycleEffort => {
                if let Some(sw) = switch.as_mut() {
                    let before = sw.selection().clone();
                    let lines = picker::cycle_effort(harness, sw);
                    let after = sw.selection().clone();
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                }
            }
            IdleOutcome::OpenResumePicker => {
                let cwd = std::env::current_dir().unwrap_or_default();
                match picker::open_resume(&cwd) {
                    Some(modal) => tui.screen.open_modal(modal),
                    None => apply_notices(
                        tui,
                        vec!["No prior sessions to resume for this directory.".to_string()],
                    ),
                }
            }
            IdleOutcome::OpenTasks => {
                if !harness.task_workflow_enabled() {
                    apply_notices(tui, vec![crate::cli::TASK_WORKFLOW_OFF_NOTICE.to_string()]);
                    continue;
                }
                // The unified task surface (ADR-0031): the active (unsettled) task
                // plus this workspace's recoverable Iris tasks. Reached from the
                // `Tasks` home entry / `ctrl-t` as well as `/tasks`.
                match picker::build_tasks_modal(harness, tui.screen.footer_git()) {
                    Some(modal) => tui.screen.open_modal(modal),
                    None => apply_notices(
                        tui,
                        vec!["No active task or tasks to resume in this workspace.".to_string()],
                    ),
                }
            }
            IdleOutcome::OpenSettings => {
                if let Some(sw) = switch.as_mut() {
                    tui.screen.open_modal(picker::open_settings(harness, sw));
                }
            }
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
                // The SessionBar dropdowns open at this safe boundary. Like
                // every other slash command they are consumed silently -- the
                // command text is never echoed into the transcript.
                if prompt == "/git" || prompt == "/tree" {
                    if prompt == "/git" {
                        toggle_git_menu(&mut tui.screen, &git_cache);
                    } else {
                        toggle_tree_menu(&mut tui.screen, &git_cache, false);
                    }
                    refresh_footer(tui, switch);
                    tui.draw()?;
                    continue;
                }
                // Picker/model/reasoning/session commands are handled at this
                // safe inter-turn boundary and never start a turn.
                match route_command(&prompt, harness, tui, switch, &git_cache)? {
                    RouteOutcome::Swap(source) => {
                        perform_swap(&source, swap, harness, tui, switch)?;
                    }
                    // Consumed: a modal may now be open; the top-of-loop modal
                    // phase runs it on the next iteration.
                    RouteOutcome::Consumed => {}
                    RouteOutcome::Compact(focus) => {
                        tui.screen.start_turn();
                        tui.draw()?;
                        run_harness_op(
                            harness,
                            tui,
                            &mut input_rx,
                            &mut tick,
                            &current_turn,
                            HarnessOp::Compact(&focus),
                            steering.as_ref(),
                            &git_cache,
                            &mut git_generation,
                        )
                        .await?;
                        tui.screen.end_background_work();
                        open_deferred_settings(harness, tui, switch, steering.as_ref());
                        tui.draw()?;
                    }
                    RouteOutcome::Fall => {
                        tui.screen.commit_user(&prompt);
                        tui.screen.start_turn();
                        tui.draw()?;
                        run_harness_op(
                            harness,
                            tui,
                            &mut input_rx,
                            &mut tick,
                            &current_turn,
                            HarnessOp::Turn(&prompt),
                            steering.as_ref(),
                            &git_cache,
                            &mut git_generation,
                        )
                        .await?;
                        tui.screen.end_turn();
                        open_deferred_settings(harness, tui, switch, steering.as_ref());
                        // Turn completion is a refresh trigger: the turn may
                        // have mutated the tree or task state.
                        git_cache.request_refresh(std::env::current_dir().unwrap_or_default());
                        tui.draw()?;
                    }
                }
            }
        }
        open_deferred_settings(harness, tui, switch, steering.as_ref());
        // A model/effort switch (Ctrl+P, Shift+Tab, or a `/model` `/reasoning`
        // command) lands in this iteration; refresh the footer so the trailing
        // draw reflects the new selection immediately, not on the next keypress.
        refresh_footer(tui, switch);
        tui.draw()?;
    }
    Ok(())
}

/// Swap the live session at the safe inter-turn boundary. Loads the target
/// session (fresh transcript or a resumed session's messages) via the app
/// loader, rebuilds the provider so the new session id keys it, installs the new
/// transcript log and messages on the harness, and resets the on-screen
/// transcript. A load/rebuild failure leaves the current session untouched and
/// surfaces a notice.
fn perform_swap<P: ChatProvider>(
    source: &SessionSource,
    swap: &SessionLoader<'_>,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
) -> Result<()> {
    // The loader updates the shared session-id cell and opens/creates the target
    // log before returning; the provider rebuild below then reads the new id.
    let mut loaded: LoadedSource = match swap(source) {
        Ok(loaded) => loaded,
        Err(error) => {
            apply_notices(tui, vec![format!("could not switch session: {error:#}")]);
            return Ok(());
        }
    };
    if let Some(sw) = switch.as_ref() {
        match sw.rebuild_provider() {
            Ok(provider) => harness.replace_provider(provider),
            Err(error) => {
                apply_notices(tui, vec![format!("could not switch session: {error:#}")]);
                return Ok(());
            }
        }
    }
    loaded.session_id.commit();
    let resumed = loaded.resumed;
    harness.swap_session(
        loaded.session_log,
        loaded.messages,
        loaded.entry_ids,
        resumed,
    );
    harness.set_approval_mode(loaded.approval_mode);
    if harness.skip_permissions() != loaded.skip_permissions {
        harness.set_skip_permissions(loaded.skip_permissions);
    }
    tui.reset_screen();
    tui.screen.apply(UiEvent::SessionStarted);
    tui.screen
        .set_approval_policy(effective_approval_policy(harness));
    // The fresh Screen starts with disarmed detents (startup rule); this swap
    // IS the swapped session's startup, so re-arm once its chrome is settled —
    // the loop's trailing refresh_footer sets the first footer (never a
    // flash), and later changes announce themselves again.
    tui.screen.arm_detents();
    let notice = match source {
        SessionSource::Fresh => "Started a new session.".to_string(),
        SessionSource::Resume(_) => {
            format!("Resumed session ({resumed} message(s) restored).")
        }
    };
    apply_notices(tui, vec![notice]);
    // A session swap is a safe boundary to reconcile a crashed/unsettled task in
    // this repo and expire stale ones (issue #263, ADR-0028). When the swap
    // explicitly resumes a session, prefer the session-linked task offer if
    // exactly one recoverable task points at that session; otherwise fall back
    // to the normal workspace recovery policy.
    let recovery = match source {
        SessionSource::Resume(id) => harness.recover_checkpoints_for_resumed_session(id),
        SessionSource::Fresh => harness.recover_checkpoints(),
    };
    apply_recovery(recovery, tui);
    Ok(())
}

/// Apply a [`RecoveryOutcome`] at a safe boundary (#288, ADR-0031): nothing for
/// `None`, the single-orphan auto-adopt notice for `Notice`, and a muted pointer
/// notice for `Picker` (the >1/legacy case). The task surface is never forced
/// open over the home menu or mid-session: recoverable tasks are reached from
/// the `Tasks` home entry (badged with the count) or `/tasks`.
fn apply_recovery(outcome: RecoveryOutcome, tui: &mut TuiUi) {
    match outcome {
        RecoveryOutcome::None => {}
        RecoveryOutcome::Notice(notice) => apply_notices(tui, vec![notice]),
        RecoveryOutcome::ResumeLinked(task) => {
            apply_notices(
                tui,
                vec!["This session has one linked Iris task to resume.".to_string()],
            );
            tui.screen.open_modal(picker::linked_task_offer(&task));
        }
        RecoveryOutcome::Picker(tasks) => {
            let n = tasks.len();
            let plural = if n == 1 { "task" } else { "tasks" };
            apply_notices(
                tui,
                vec![format!(
                    "{n} Iris {plural} to resume in this workspace — open Tasks (/tasks) to review or resume."
                )],
            );
        }
    }
}

/// Open a `/settings` request that was typed while a turn/compact operation was
/// running. The running phase records only an intent; the modal opens here at a
/// safe boundary, where model/settings actions already route.
fn open_deferred_settings<P: ChatProvider>(
    harness: &Harness<P>,
    tui: &mut TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    steering: &SteeringQueue,
) {
    if steering.take_settings()
        && let Some(sw) = switch.as_mut()
    {
        tui.screen.open_modal(picker::open_settings(harness, sw));
    }
}

/// Request a coalesced render during an active turn: draw immediately when the
/// pacing window allows, otherwise leave the request pending for the loop's
/// flush branch to draw at the [`MIN_RENDER_INTERVAL`] boundary.
fn request_render(sched: &mut RenderScheduler, tui: &mut TuiUi) -> Result<()> {
    sched.request();
    let now = Instant::now();
    if matches!(sched.poll(now), RenderAction::DrawNow) {
        tui.draw()?;
        sched.mark_drawn(now);
    }
    Ok(())
}

/// Append status/notice lines to the transcript (no draw).
fn apply_notices(tui: &mut TuiUi, lines: Vec<String>) {
    for line in lines {
        tui.screen.apply(UiEvent::Notice(line));
    }
}

/// In the TUI, successful model/reasoning switches are volatile chrome: the
/// footer already shows the active selection, and this chip carries predicted
/// cache/context impact until the next provider turn replaces it with realized
/// usage. Errors and persistence failures still go to the transcript.
fn apply_model_switch_lines<P: ChatProvider>(
    tui: &mut TuiUi,
    harness: &Harness<P>,
    before: Option<&ModelSelection>,
    after: Option<&ModelSelection>,
    lines: Vec<String>,
) {
    let switched = lines.iter().any(|line| is_switch_confirmation(line));
    let compact_recommended = lines.iter().any(|line| is_switch_advisory(line));
    if switched && let Some(selection) = after {
        tui.screen.set_switch_status(SwitchStatus::new(
            selection.model.clone(),
            selection.reasoning.map(|effort| {
                crate::mimir::model_capabilities::display_level(
                    selection.provider,
                    &selection.model,
                    effort,
                )
                .to_string()
            }),
            harness.context_token_estimate(),
            switch_cache_status(before, selection),
            compact_recommended,
        ));
    }

    let notices = switch_notice_lines(lines);
    if !notices.is_empty() {
        apply_notices(tui, notices);
    }
}

fn switch_notice_lines(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|line| !is_switch_confirmation(line) && !is_switch_advisory(line))
        .collect()
}

fn is_switch_confirmation(line: &str) -> bool {
    line.starts_with("switched to ")
}

fn is_switch_advisory(line: &str) -> bool {
    line.starts_with("carrying ~") && line.contains("prompt cache starts cold")
}

fn switch_cache_status(
    before: Option<&ModelSelection>,
    after: &ModelSelection,
) -> SwitchCacheStatus {
    match before {
        Some(before) if before.provider != after.provider || before.model != after.model => {
            SwitchCacheStatus::Cold
        }
        Some(before) if before.reasoning != after.reasoning => SwitchCacheStatus::Warm,
        Some(_) => SwitchCacheStatus::Unchanged,
        None => SwitchCacheStatus::Cold,
    }
}

/// Refresh the idle status footer from the live model selection. A no-op when
/// no model switch is wired (the footer then stays unset and the keybind hint
/// shows instead).
fn refresh_footer<P: ChatProvider>(tui: &mut TuiUi, switch: &Option<ModelSwitch<'_, P>>) {
    let Some(sw) = switch.as_ref() else {
        return;
    };
    let selection = sw.selection();
    let effort = selection.reasoning.map(|effort| {
        crate::mimir::model_capabilities::display_level(
            selection.provider,
            &selection.model,
            effort,
        )
        .to_string()
    });
    let qualified_model = format!("{}/{}", selection.provider.as_str(), selection.model);
    let context = if selection.provider == crate::mimir::selection::ProviderId::OpenAiCompatible {
        selection
            .open_ai_compatible
            .context_window
            .map(model_catalog::context_window_label)
    } else {
        model_catalog::ctx_label(&qualified_model).map(str::to_string)
    };
    tui.screen
        .set_footer_with_context(selection.model.clone(), effort, context, footer_cwd());
}

/// The working directory for the footer, home-relativized to `~`/`~/sub`.
///
/// Presentation-only: this reads the process working directory, which equals the
/// workspace root today, so it is not a tier boundary concern (it enforces
/// nothing). If Iris later supports remote/alternate workspace roots, switch this
/// to a read-only display accessor on `Harness`.
fn footer_cwd() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
        && let Ok(rel) = cwd.strip_prefix(std::path::Path::new(&home))
    {
        if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rel.display())
        }
    } else {
        cwd.display().to_string()
    }
}

/// Sync the last-known git snapshot from the async cache into the screen when
/// a newer refresh landed. Returns whether the snapshot changed (redraw).
fn sync_git_status(tui: &mut TuiUi, cache: &GitStatusCache, last_generation: &mut u64) -> bool {
    let generation = cache.generation();
    if generation == *last_generation {
        return false;
    }
    *last_generation = generation;
    tui.screen.set_footer_vcs(cache.latest());
    true
}

/// Toggle the git console dropdown. Opening always kicks a fresh background
/// refresh (paint last known meanwhile). Returns whether anything changed.
fn toggle_git_menu(screen: &mut Screen, cache: &GitStatusCache) -> bool {
    if matches!(
        screen.session_menu,
        Some(SessionMenu::Git(_)) | Some(SessionMenu::Jj(_))
    ) {
        screen.close_session_menu();
        return true;
    }
    let Some(status) = screen.footer_vcs().cloned() else {
        screen.apply(UiEvent::Notice(
            "no git or jj repository here — the VCS console needs a worktree".to_string(),
        ));
        return true;
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    match status {
        VcsStatus::Git(status) => {
            let main_root = status
                .worktrees
                .first()
                .map(|wt| wt.path.clone())
                .unwrap_or_else(|| cwd.clone());
            let worktree_root = crate::config::Settings::load(&cwd)
                .map(|settings| settings.worktree_root(&main_root))
                .unwrap_or_else(|_| main_root.join("../wt"));
            screen.open_session_menu(SessionMenu::Git(Box::new(GitMenu::new(
                status,
                worktree_root,
            ))));
        }
        VcsStatus::Jj(status) => {
            screen.open_session_menu(SessionMenu::Jj(JjMenu::new(status)));
        }
    }
    cache.request_refresh(cwd);
    true
}

/// Toggle the directory tree dropdown (`filter` = open in filter mode).
fn toggle_tree_menu(screen: &mut Screen, cache: &GitStatusCache, filter: bool) -> bool {
    if matches!(screen.session_menu, Some(SessionMenu::Tree(_))) {
        screen.close_session_menu();
        return true;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    screen.open_session_menu(SessionMenu::Tree(TreeMenu::new(cwd.clone(), filter)));
    cache.request_refresh(cwd);
    true
}

/// Translate a crossterm key into the dropdowns' neutral [`MenuKey`].
fn to_menu_key(code: KeyCode, ctrl: bool) -> Option<MenuKey> {
    Some(match code {
        KeyCode::Up => MenuKey::Up,
        KeyCode::Down => MenuKey::Down,
        KeyCode::Left => MenuKey::Left,
        KeyCode::Right => MenuKey::Right,
        KeyCode::Enter => MenuKey::Enter,
        KeyCode::Esc => MenuKey::Esc,
        KeyCode::Tab => MenuKey::Tab,
        KeyCode::Backspace => MenuKey::Backspace,
        KeyCode::Char('w') | KeyCode::Char('W') if ctrl => MenuKey::CtrlW,
        KeyCode::Char(c) if !ctrl => MenuKey::Char(c),
        _ => return None,
    })
}

/// Fold a dropdown key outcome into the idle-phase key result.
fn menu_outcome_key(screen: &mut Screen, outcome: MenuOutcome) -> IdleKey {
    match outcome {
        MenuOutcome::Ignore => IdleKey::Ignore,
        MenuOutcome::Redraw => IdleKey::Continue,
        MenuOutcome::Close => {
            screen.close_session_menu();
            IdleKey::Continue
        }
        MenuOutcome::Action(action) => IdleKey::Menu(action),
    }
}

/// Pager-mode mouse targets for the session bar and an open dropdown: a click
/// on the cwd/git segment toggles its dropdown (performed here); a click on a
/// dropdown row selects it, and a second click activates. `None` = not a
/// session-bar click (fall through to wheel handling).
fn session_bar_click(
    screen: &mut Screen,
    mouse: &ratatui::crossterm::event::MouseEvent,
    cache: &GitStatusCache,
) -> Option<IdleKey> {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    if !screen.pager_active || !screen.mouse_capture {
        return None;
    }
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return None;
    }
    if mouse.row == 0 {
        let width = ratatui::crossterm::terminal::size()
            .map(|(width, _)| width)
            .unwrap_or(80);
        return match crate::ui::tui::session_bar_hit(screen, width, mouse.column) {
            Some(crate::ui::tui::BarSegment::Cwd) => {
                toggle_tree_menu(screen, cache, false);
                Some(IdleKey::Continue)
            }
            Some(crate::ui::tui::BarSegment::Git) => {
                toggle_git_menu(screen, cache);
                Some(IdleKey::Continue)
            }
            None => Some(IdleKey::Ignore),
        };
    }
    if screen.session_menu.is_some() {
        let line = usize::from(mouse.row) - 1;
        let readonly = screen.menu_readonly();
        let outcome = screen
            .session_menu
            .as_mut()
            .map(|menu| menu.click_line(line, readonly))
            .unwrap_or(MenuOutcome::Ignore);
        return Some(menu_outcome_key(screen, outcome));
    }
    None
}

/// Execute a dropdown side effect at the idle boundary. Mutating git/task
/// state under a running turn is impossible by construction: dropdowns are
/// read-only while a turn runs, and this runs only from the idle phase.
fn open_session_at<P: ChatProvider>(
    path: std::path::PathBuf,
    branch: Option<String>,
    carry_active_task: bool,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
) -> bool {
    if !carry_active_task && harness.reanchor_requires_task_decision() {
        apply_notices(
            tui,
            vec![
                "accept, undo, or explicitly carry the active task before opening another worktree"
                    .to_string(),
            ],
        );
        tui.screen.close_session_menu();
        return false;
    }
    let target = match path.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            apply_notices(
                tui,
                vec![format!(
                    "{} could not open {}: {error}",
                    crate::ui::symbols::ERROR,
                    path.display()
                )],
            );
            tui.screen.close_session_menu();
            return false;
        }
    };
    if let Err(error) = std::env::set_current_dir(&target) {
        apply_notices(
            tui,
            vec![format!(
                "{} could not open {}: {error}",
                crate::ui::symbols::ERROR,
                target.display()
            )],
        );
        tui.screen.close_session_menu();
        return false;
    }
    if carry_active_task {
        harness.reanchor_workspace_carrying_task(&target);
    } else if harness.reanchor_workspace(&target).is_err() {
        apply_notices(
            tui,
            vec![
                "accept, undo, or explicitly carry the active task before opening another worktree"
                    .to_string(),
            ],
        );
        tui.screen.close_session_menu();
        return false;
    }
    let branch_label = branch.unwrap_or_else(|| "detached".to_string());
    apply_notices(
        tui,
        vec![format!(
            "{} session moved to {} — {branch_label}",
            crate::ui::symbols::SEP,
            target.display()
        )],
    );
    // Arriving in a worktree tells you what Iris left unsettled there.
    apply_recovery(harness.recover_checkpoints(), tui);
    tui.screen.close_session_menu();
    if harness.mutation_safety_enabled()
        && harness.native_jj_available()
        && crate::wayland::trust::native_jj(harness.workspace()).is_none()
    {
        tui.screen.open_modal(crate::ui::modal::jj_setup());
    }
    true
}

fn execute_menu_action<P: ChatProvider>(
    action: MenuAction,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    cache: &GitStatusCache,
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let checkout = |tui: &mut TuiUi, branch: &str| -> bool {
        match git_cmd::git_stdout(&cwd, &["checkout", branch]) {
            Ok(_) => {
                apply_notices(
                    tui,
                    vec![format!("{} switched to {branch}", crate::ui::symbols::DONE)],
                );
                true
            }
            Err(_) => {
                apply_notices(
                    tui,
                    vec![format!(
                        "{} checkout blocked — conflicting changes",
                        crate::ui::symbols::ERROR
                    )],
                );
                false
            }
        }
    };
    match action {
        MenuAction::Accept => {
            let notice = match harness.accept_checkpoint() {
                Some(summary) => format!("{} {summary}", crate::ui::symbols::DONE),
                None => "no unreviewed Iris changes to accept".to_string(),
            };
            apply_notices(tui, vec![notice]);
            tui.screen.close_session_menu();
        }
        MenuAction::AcceptThenCheckout { branch } => {
            let notice = match harness.accept_checkpoint() {
                Some(summary) => format!("{} {summary}", crate::ui::symbols::DONE),
                None => "no unreviewed Iris changes to accept".to_string(),
            };
            apply_notices(tui, vec![notice]);
            checkout(tui, &branch);
            tui.screen.close_session_menu();
        }
        MenuAction::LoadRestorePoints => {
            let points: Vec<(u64, String)> = harness
                .checkpoint_restore_points()
                .into_iter()
                .map(|point| (point.seq, point.label))
                .collect();
            if points.is_empty() {
                apply_notices(tui, vec!["no restore points for this task".to_string()]);
                tui.screen.close_session_menu();
            } else if let Some(SessionMenu::Git(menu)) = &mut tui.screen.session_menu {
                menu.set_restore_points(points);
            }
        }
        MenuAction::LoadRestorePointsForOpenSessionAt { path, branch } => {
            let points: Vec<(u64, String)> = harness
                .checkpoint_restore_points()
                .into_iter()
                .map(|point| (point.seq, point.label))
                .collect();
            if points.is_empty() {
                apply_notices(tui, vec!["no restore points for this task".to_string()]);
                tui.screen.close_session_menu();
            } else if let Some(SessionMenu::Git(menu)) = &mut tui.screen.session_menu {
                menu.set_restore_points_for_open_session(points, path, branch);
            }
        }
        MenuAction::Rollback { seq } => {
            match harness.rollback_checkpoint(seq) {
                Ok(outcome) => {
                    let mut lines =
                        vec![format!("{} {}", crate::ui::symbols::DONE, outcome.summary)];
                    lines.extend(outcome.preserved_notices);
                    if let Some(warning) = outcome.index_warning {
                        lines.push(format!("{} {warning}", crate::ui::symbols::REVIEW));
                    }
                    apply_notices(tui, lines);
                }
                Err(error) => {
                    apply_notices(
                        tui,
                        vec![format!(
                            "{} rollback failed: {error:#}",
                            crate::ui::symbols::ERROR
                        )],
                    );
                }
            }
            tui.screen.close_session_menu();
        }
        MenuAction::RollbackThenOpenSessionAt { seq, path, branch } => {
            match harness.rollback_checkpoint(seq) {
                Ok(outcome) => {
                    let mut lines =
                        vec![format!("{} {}", crate::ui::symbols::DONE, outcome.summary)];
                    lines.extend(outcome.preserved_notices);
                    if let Some(warning) = outcome.index_warning {
                        lines.push(format!("{} {warning}", crate::ui::symbols::REVIEW));
                    }
                    apply_notices(tui, lines);
                    open_session_at(path, branch, false, harness, tui);
                }
                Err(error) => {
                    apply_notices(
                        tui,
                        vec![format!(
                            "{} rollback failed: {error:#}",
                            crate::ui::symbols::ERROR
                        )],
                    );
                    tui.screen.close_session_menu();
                }
            }
        }
        MenuAction::Checkout { branch } => {
            checkout(tui, &branch);
            tui.screen.close_session_menu();
        }
        MenuAction::StashCheckout { branch } => {
            match git_cmd::git_stdout(&cwd, &["stash", "push"]) {
                Ok(_) => {
                    if checkout(tui, &branch) {
                        apply_notices(
                            tui,
                            vec!["changes stashed — git stash pop to restore".to_string()],
                        );
                    }
                }
                Err(error) => {
                    apply_notices(
                        tui,
                        vec![format!(
                            "{} stash failed: {error:#}",
                            crate::ui::symbols::ERROR
                        )],
                    );
                }
            }
            tui.screen.close_session_menu();
        }
        MenuAction::CreateBranch { name, base } => {
            match git_cmd::git_stdout(&cwd, &["checkout", "-b", &name, &base]) {
                Ok(_) => {
                    apply_notices(
                        tui,
                        vec![format!(
                            "{} branch {name} created from {base}",
                            crate::ui::symbols::DONE
                        )],
                    );
                }
                Err(error) => {
                    apply_notices(
                        tui,
                        vec![format!(
                            "{} could not create branch: {error:#}",
                            crate::ui::symbols::ERROR
                        )],
                    );
                }
            }
            tui.screen.close_session_menu();
        }
        MenuAction::CreateWorktree { name, base, path } => {
            let path_arg = path.to_string_lossy().into_owned();
            match git_cmd::git_stdout(&cwd, &["worktree", "add", &path_arg, "-b", &name, &base]) {
                Ok(_) => {
                    // Stay open: the in-dropdown confirm offers `↵ open
                    // session there ┊ esc stay`.
                    if let Some(SessionMenu::Git(menu)) = &mut tui.screen.session_menu {
                        menu.worktree_ready(path);
                    }
                }
                Err(error) => {
                    apply_notices(
                        tui,
                        vec![format!(
                            "{} could not create worktree: {error:#}",
                            crate::ui::symbols::ERROR
                        )],
                    );
                    tui.screen.close_session_menu();
                }
            }
        }
        MenuAction::OpenSessionAt { path, branch } => {
            open_session_at(path, branch, false, harness, tui);
        }
        MenuAction::CarryOpenSessionAt { path, branch } => {
            open_session_at(path, branch, true, harness, tui);
        }
        MenuAction::AcceptThenOpenSessionAt { path, branch } => {
            let notice = match harness.accept_checkpoint() {
                Some(summary) => format!("{} {summary}", crate::ui::symbols::DONE),
                None => "no unreviewed Iris changes to accept".to_string(),
            };
            apply_notices(tui, vec![notice]);
            open_session_at(path, branch, false, harness, tui);
        }
        MenuAction::InsertReference(path) => {
            tui.screen.editor.insert_str(format!("@{path} "));
            tui.screen.sync_palette();
            tui.screen.close_session_menu();
        }
    }
    cache.request_refresh(std::env::current_dir().unwrap_or_default());
}

/// Route a submitted `/` command to its picker/handler. Returns a
/// [`RouteOutcome`]: `Consumed` (handled, a modal may be open), `Fall` (not a
/// command; run it as a turn), or `Swap` (perform a session swap at the
/// boundary). `/login`/`/logout` with arguments are intentionally not recognized
/// (pi-mono parity) and fall through to a normal turn.
/// Build the `/context` breakdown (issue #400, design §5.1): system+tools /
/// raw conversation / summarized / folded-reclaimed / pending-fold mass /
/// free headroom, plus per-batch fold lines tagged with their trigger class.
/// Everything is an estimate from data that already exists (the harness
/// message estimates, the budget, the runtime fold/compaction events and the
/// pending set); nothing is fabricated. System+tools are labeled as included
/// when provider usage anchors the total and display-only for local estimates.
fn context_breakdown_lines<P: ChatProvider>(
    harness: &crate::wayland::Harness<P>,
    switch: Option<&ModelSwitch<'_, P>>,
    accounting: &super::tui::ContextAccounting,
) -> Vec<String> {
    use crate::session::estimate_tokens;
    let local_total = harness.context_token_estimate();
    let mut lines = Vec::new();
    match harness.context_diagnostics() {
        Some(diagnostics) => {
            let total = diagnostics.measured;
            let budget = diagnostics.ladder.effective_window;
            let pct = (total.saturating_mul(100)).checked_div(budget).unwrap_or(0);
            let source = match diagnostics.source {
                crate::nexus::ContextMeasurementSource::ProviderReportedPlusLocal => {
                    "provider-reported + local"
                }
                crate::nexus::ContextMeasurementSource::Estimated => "estimated",
            };
            lines.push(format!(
                "context: ~{total} of {budget} tokens ({pct}% of effective window; {source})"
            ));
            lines.push(format!(
                "  free headroom      ~{} tokens",
                budget.saturating_sub(total)
            ));
            let state = if diagnostics.automatic_enabled {
                "on"
            } else {
                "off"
            };
            let job = if diagnostics.background_running {
                "running"
            } else {
                "idle"
            };
            lines.push(format!(
                "  compaction         {state}; warn {} / start {} / hard {}; summarizer {}/{}; job {job}",
                diagnostics.ladder.warn,
                diagnostics.ladder.start,
                diagnostics.ladder.hard,
                diagnostics.summarizer.as_str(),
                diagnostics.worker_input.as_str(),
            ));
            if let Some(background) = diagnostics.background_job {
                let tier = background
                    .trigger_tier
                    .map(|tier| tier.as_str())
                    .unwrap_or("manual");
                lines.push(format!(
                    "  background job     running {}s; covering {} message(s) (~{} tokens); job {}; origin {}; trigger {tier}",
                    background.elapsed_secs,
                    background.covered_messages,
                    background.original_tokens_estimate,
                    background.job_id,
                    background.origin.as_str(),
                ));
            }
        }
        None => lines.push(format!(
            "context: ~{local_total} tokens (no compaction window)"
        )),
    }
    // System prompt + tool declarations: sent with every request but not part
    // of the conversation estimate the budget covers.
    if let Some(sw) = switch {
        let system = estimate_tokens(sw.system_prompt());
        let tools: u64 = harness
            .agent
            .tools()
            .iter()
            .map(|tool| {
                estimate_tokens(tool.name())
                    .saturating_add(estimate_tokens(tool.description()))
                    .saturating_add(estimate_tokens(&tool.parameters().to_string()))
            })
            .fold(0, u64::saturating_add);
        let inclusion = harness.context_diagnostics().map_or(
            "display-only; no window configured",
            |diagnostics| match diagnostics.source {
                crate::nexus::ContextMeasurementSource::ProviderReportedPlusLocal => {
                    "included in provider-reported total"
                }
                crate::nexus::ContextMeasurementSource::Estimated => {
                    "not included in conversation-only estimate"
                }
            },
        );
        lines.push(format!(
            "  system + tools     ~{} tokens ({inclusion})",
            system.saturating_add(tools)
        ));
    }
    // Split the conversation estimate into raw turns vs summary stand-ins.
    let messages = harness.messages();
    let summarized: u64 = messages
        .iter()
        .filter(|m| {
            m.content.starts_with("[compacted summary")
                || m.content.starts_with("[auto-compacted summary")
        })
        .map(|m| estimate_tokens(&m.content))
        .fold(0, u64::saturating_add);
    let folded_stubs = messages
        .iter()
        .filter(|m| m.content.starts_with("[folded]"))
        .count();
    lines.push(format!(
        "  raw conversation   ~{} tokens",
        local_total.saturating_sub(summarized)
    ));
    let (original_total, summary_total) = accounting
        .compactions
        .iter()
        .fold((0u64, 0u64), |(original, summary), (o, s)| {
            (original.saturating_add(*o), summary.saturating_add(*s))
        });
    if summarized > 0 || !accounting.compactions.is_empty() {
        let mut line = format!("  summarized         ~{summarized} tokens in context");
        if !accounting.compactions.is_empty() {
            line.push_str(&format!(
                " ({} compaction(s) this session: ~{original_total} -> ~{summary_total})",
                accounting.compactions.len()
            ));
        }
        lines.push(line);
    }
    if folded_stubs > 0 || !accounting.fold_batches.is_empty() {
        lines.push(format!(
            "  folded-reclaimed   {folded_stubs} stub(s) in context; ~{} tokens reclaimed this session",
            accounting.folded_reclaimed()
        ));
        for (trigger, folds, reclaimed) in &accounting.fold_batches {
            lines.push(format!(
                "    folded {folds} result(s) \u{2014} reclaimed ~{reclaimed} tokens [{trigger}]"
            ));
        }
    }
    let (pending, reclaimable) = harness.pending_fold_stats();
    let (frozen, frozen_reclaimable) = harness.frozen_fold_stats();
    if pending > 0 {
        lines.push(format!(
            "  pending folds      {pending} detected, ~{reclaimable} tokens reclaimable (holding for a free cache break)"
        ));
    }
    if frozen > 0 {
        lines.push(format!(
            "  frozen folds       {frozen} under active compaction job, ~{frozen_reclaimable} tokens reclaimable after apply"
        ));
    }
    lines
}

/// Whether a submitted line is the `/settings` command (the leading token,
/// ignoring any trailing arguments), matching `route_command`'s own token
/// split. Used by the running phase to divert `/settings` from steering into a
/// deferred picker-open at the next safe boundary (issue #489).
fn is_settings_command(text: &str) -> bool {
    let trimmed = text.trim();
    let cmd = trimmed
        .split_once(char::is_whitespace)
        .map_or(trimmed, |(cmd, _)| cmd);
    cmd == "/settings"
}

/// Apply the session-scoped focus-mode command. `None` means ordinary input;
/// `Some` means the line was consumed and carries the honest readout to show.
/// This is safe during a running turn because it changes presentation only.
fn apply_focus_command(screen: &mut Screen, text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };
    if cmd != "/focus" {
        return None;
    }
    let enabled = match rest {
        "" => screen.toggle_focus_mode(),
        "on" => {
            screen.set_focus_mode(true);
            true
        }
        "off" => {
            screen.set_focus_mode(false);
            false
        }
        _ => return Some("usage: /focus [on|off]".to_string()),
    };
    Some(if enabled {
        "focus mode on".to_string()
    } else {
        "focus mode automatic \u{2014} activates at 12 rows".to_string()
    })
}

fn route_command<P: ChatProvider>(
    prompt: &str,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    git_cache: &GitStatusCache,
) -> Result<RouteOutcome> {
    let trimmed = prompt.trim();
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };
    match cmd {
        "/model" => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            let before = sw.selection().clone();
            match picker::model_command(rest, harness, sw) {
                ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                ModelCommand::Lines(lines) => {
                    let after = sw.selection().clone();
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                }
            }
            Ok(RouteOutcome::Consumed)
        }
        "/scoped-models" => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_settings_expanded(
                harness,
                sw,
                settings_menu::HatchTarget::Scope,
            ));
            Ok(RouteOutcome::Consumed)
        }
        "/settings" => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_settings(harness, sw));
            Ok(RouteOutcome::Consumed)
        }
        "/skills" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            if harness.skills().is_empty() {
                apply_notices(tui, vec!["No skills are installed.".to_string()]);
            } else {
                tui.screen
                    .open_modal(Modal::Skills(crate::ui::modal::SkillPicker::new(
                        harness.skills(),
                    )));
            }
            Ok(RouteOutcome::Consumed)
        }
        "/approval" => {
            // Permission mode (ADR-0032 + ADR-0049). Changing it at this
            // inter-turn boundary is safe: the harness forwards it to Nexus,
            // which owns enforcement. The statusline posture is kept in lockstep
            // so the label never claims a mode the runtime is not in.
            tui.screen.commit_user(prompt);
            let lines = if rest.is_empty() {
                vec![format!(
                    "approval mode: {} (use /approval {})",
                    crate::cli::current_permission_token(harness),
                    crate::cli::APPROVAL_USAGE
                )]
            } else {
                match PermissionMode::parse(rest) {
                    Some(mode) => {
                        let lines = crate::cli::apply_permission_mode(harness, mode);
                        tui.screen
                            .set_approval_policy(effective_approval_policy(harness));
                        lines
                    }
                    None => vec![format!(
                        "unknown approval mode `{rest}` (use {})",
                        crate::cli::APPROVAL_USAGE
                    )],
                }
            };
            apply_notices(tui, lines);
            Ok(RouteOutcome::Consumed)
        }
        "/trust" | "/permissions" if rest.is_empty() => {
            // Modal actions dispatch through picker::apply_action, which takes
            // the switch; keep the same guard as the other pickers.
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_settings_expanded(
                harness,
                sw,
                settings_menu::HatchTarget::Permissions,
            ));
            Ok(RouteOutcome::Consumed)
        }
        "/resume" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            let cwd = std::env::current_dir().unwrap_or_default();
            match picker::open_resume(&cwd) {
                Some(modal) => tui.screen.open_modal(modal),
                None => apply_notices(
                    tui,
                    vec!["No prior sessions to resume for this directory.".to_string()],
                ),
            }
            Ok(RouteOutcome::Consumed)
        }
        "/tasks" => {
            // Open the unified task surface (ADR-0031): the active (unsettled)
            // task as a header plus this workspace's recoverable Iris tasks.
            // Selection adopts a recoverable task at the inter-turn boundary;
            // adoption never implicitly resumes a session. The active card is
            // enriched with the git-status snapshot the session bar already holds.
            tui.screen.commit_user(prompt);
            if let Some(lines) = crate::cli::handle_tasks_command(prompt, harness) {
                git_cache.set_task_workflow_enabled(harness.task_workflow_enabled());
                git_cache.request_refresh(std::env::current_dir().unwrap_or_default());
                apply_notices(tui, lines);
                return Ok(RouteOutcome::Consumed);
            }
            match picker::build_tasks_modal(harness, tui.screen.footer_git()) {
                Some(modal) => tui.screen.open_modal(modal),
                None => apply_notices(
                    tui,
                    vec!["No active task or tasks to resume in this workspace.".to_string()],
                ),
            }
            Ok(RouteOutcome::Consumed)
        }
        "/task" => {
            tui.screen.commit_user(prompt);
            if let Some(lines) = crate::cli::handle_task_command(prompt, harness) {
                apply_notices(tui, lines);
            }
            Ok(RouteOutcome::Consumed)
        }
        "/new" if rest.is_empty() => {
            // Start a fresh session at this safe boundary (new id, empty
            // transcript, fresh log) without restarting the process.
            tui.screen.commit_user(prompt);
            Ok(RouteOutcome::Swap(SessionSource::Fresh))
        }
        "/session" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            apply_notices(tui, crate::cli::session_info_lines(harness, switch));
            Ok(RouteOutcome::Consumed)
        }
        "/context" if rest.is_empty() => {
            // Context-accounting breakdown (issue #400, design §5.1): every
            // token attributed to a category, every reduction itemized with
            // its trigger tag. Display-only; all numbers come from the harness
            // estimates and the session's runtime events, never fabricated.
            tui.screen.commit_user(prompt);
            let lines =
                context_breakdown_lines(harness, switch.as_ref(), &tui.screen.context_accounting);
            apply_notices(tui, lines);
            Ok(RouteOutcome::Consumed)
        }
        "/compaction" => {
            tui.screen.commit_user(prompt);
            match crate::cli::selected_compaction(harness, rest) {
                Ok(entry) => {
                    let (title, detail, summary) = crate::cli::compaction_panel_parts(&entry);
                    tui.screen.apply(UiEvent::CompactionInspection {
                        title,
                        detail,
                        summary,
                    });
                }
                Err(message) => apply_notices(tui, vec![message]),
            }
            Ok(RouteOutcome::Consumed)
        }
        "/sessions" => {
            // Deterministic session lookup by task id (ADR-0031): with no arg,
            // default to the active task; else a usage line. No modal, no model
            // call -- display-only audit text.
            tui.screen.commit_user(prompt);
            let task_id = if rest.is_empty() {
                harness.current_task_id()
            } else {
                Some(rest.to_string())
            };
            let lines = match task_id {
                Some(task_id) => crate::cli::sessions_for_task_lines(harness.workspace(), &task_id),
                None => vec!["usage: /sessions <task-id>".to_string()],
            };
            apply_notices(tui, lines);
            Ok(RouteOutcome::Consumed)
        }
        "/compact" => {
            tui.screen.commit_user(prompt);
            Ok(RouteOutcome::Compact(rest.to_string()))
        }
        "/copy" => {
            tui.screen.commit_user(prompt);
            apply_notices(tui, crate::cli::copy_command_lines(harness, rest));
            Ok(RouteOutcome::Consumed)
        }
        // pi-mono spells it `/debug`; `/dbug` is accepted as an unlisted alias.
        "/debug" | "/dbug" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            let lines = match write_debug_snapshot(harness, tui) {
                Ok(path) => vec![format!("debug snapshot written to {}", path.display())],
                Err(error) => vec![format!("could not write debug snapshot: {error:#}")],
            };
            apply_notices(tui, lines);
            Ok(RouteOutcome::Consumed)
        }
        "/login" if rest.is_empty() => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_settings_expanded(
                harness,
                sw,
                settings_menu::HatchTarget::Login,
            ));
            Ok(RouteOutcome::Consumed)
        }
        "/logout" if rest.is_empty() => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_settings_expanded(
                harness,
                sw,
                settings_menu::HatchTarget::Logout,
            ));
            Ok(RouteOutcome::Consumed)
        }
        "/reasoning" if rest.is_empty() => {
            // Model and reasoning are ONE selector: a bare `/reasoning` opens
            // the same unified picker as `/model` (rows pick the model, ←→
            // clicks the effort detent) instead of a second bespoke list.
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            let before = sw.selection().clone();
            match picker::model_command("", harness, sw) {
                ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                ModelCommand::Lines(lines) => {
                    let after = sw.selection().clone();
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                }
            }
            Ok(RouteOutcome::Consumed)
        }
        "/reasoning" => {
            // `/reasoning <level>` stays the typed fast path (a compatible
            // alias through the text driver, like the CLI).
            tui.screen.commit_user(prompt);
            let before = switch.as_ref().map(|sw| sw.selection().clone());
            if let Some(lines) = crate::cli::handle_model_command(prompt, harness, switch) {
                let after = switch.as_ref().map(|sw| sw.selection().clone());
                apply_model_switch_lines(tui, harness, before.as_ref(), after.as_ref(), lines);
            }
            Ok(RouteOutcome::Consumed)
        }
        "/terminal-setup" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            let env = crate::ui::terminal_doctor::detect(
                tui.keyboard_enhanced(),
                tui.screen.pager_active,
            );
            apply_notices(tui, crate::ui::terminal_doctor::report(&env));
            Ok(RouteOutcome::Consumed)
        }
        "/find" => {
            // Deliberately NOT committed to the transcript: the command row
            // would otherwise always match its own query. The indicator row
            // carries the query while the search is active.
            if !tui.screen.pager_active {
                apply_notices(
                    tui,
                    vec!["transcript search is a pager-mode feature".to_string()],
                );
                return Ok(RouteOutcome::Consumed);
            }
            match tui.screen.start_search(rest) {
                None => apply_notices(tui, vec!["search cleared".to_string()]),
                Some(0) => apply_notices(tui, vec![format!("no matches for {rest:?}")]),
                Some(_) => {}
            }
            Ok(RouteOutcome::Consumed)
        }
        "/focus" => {
            let notice = apply_focus_command(&mut tui.screen, prompt)
                .expect("/focus match must be consumed by focus command parser");
            apply_notices(tui, vec![notice]);
            Ok(RouteOutcome::Consumed)
        }
        "/mouse" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            let notice = if tui.screen.pager_active {
                if tui.screen.toggle_mouse() {
                    "mouse reporting on \u{2014} wheel scrolls the transcript (Ctrl+T toggles)"
                } else {
                    "mouse reporting off \u{2014} terminal-native select/copy active (Ctrl+T re-enables)"
                }
            } else {
                "mouse capture is a pager-mode feature; the inline renderer never captures the mouse"
            };
            apply_notices(tui, vec![notice.to_string()]);
            Ok(RouteOutcome::Consumed)
        }
        "/diff" if rest.is_empty() => {
            // The final task diff (issue #264): render the net diff on demand
            // through the diff colorizer at this safe boundary.
            tui.screen.commit_user(prompt);
            tui.screen.apply(crate::cli::task_diff_event(harness));
            Ok(RouteOutcome::Consumed)
        }
        "/rollback" | "/accept" | "/checkpoint" => {
            // Checkpoint/accept/rollback commands at this safe boundary.
            tui.screen.commit_user(prompt);
            if let Some(lines) = crate::cli::handle_checkpoint_command(prompt, harness) {
                apply_notices(tui, lines);
            }
            Ok(RouteOutcome::Consumed)
        }
        _ => Ok(RouteOutcome::Fall),
    }
}

/// `/debug`: write a snapshot of the rendered document and the provider-visible
/// context to `~/.iris/iris-debug.log` (pi-mono's debug dump shape: every
/// rendered line with its visible width, then the messages as JSONL). Returns
/// the written path for the confirmation notice.
fn write_debug_snapshot<P: ChatProvider>(
    harness: &Harness<P>,
    tui: &mut TuiUi,
) -> Result<std::path::PathBuf> {
    use anyhow::Context;
    let path = crate::config::debug_log_path()
        .context("cannot resolve the debug log path: HOME is not set")?;
    let (size, rendered) = tui.debug_render_lines()?;
    let frame_stats = tui.frame_stats_lines();
    let contents = debug_snapshot_contents(
        size.width,
        size.height,
        &rendered,
        &frame_stats,
        harness.messages(),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Assemble the `/debug` snapshot body. Pure so the shape is unit-testable.
fn debug_snapshot_contents(
    width: u16,
    height: u16,
    rendered: &[String],
    frame_stats: &[String],
    messages: &[crate::nexus::Message],
) -> String {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(rendered.len() + frame_stats.len() + messages.len() + 8);
    out.push(format!(
        "Iris {} debug snapshot at unix-ms {unix_ms}",
        env!("CARGO_PKG_VERSION")
    ));
    out.push(format!("Terminal: {width}x{height}"));
    out.push(format!("Total lines: {}", rendered.len()));
    out.push(String::new());
    out.push("=== Frame timing (compose vs flush) ===".to_string());
    if frame_stats.is_empty() {
        out.push("(no frames drawn yet)".to_string());
    } else {
        out.extend(frame_stats.iter().cloned());
    }
    out.push(String::new());
    out.push("=== Rendered lines with visible widths ===".to_string());
    out.extend(rendered.iter().cloned());
    out.push(String::new());
    out.push("=== Context messages (JSONL) ===".to_string());
    out.extend(
        messages
            .iter()
            .map(|message| crate::session::message_body(message).to_string()),
    );
    out.push(String::new());
    out.join("\n")
}

/// Read and edit until the user submits a non-empty prompt or exits. The spinner
/// is idle here, so a tick redraws nothing.
async fn idle_phase(
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    git_cache: &GitStatusCache,
    git_generation: &mut u64,
) -> Result<IdleOutcome> {
    let mut last_resize_width = ratatui::crossterm::terminal::size()
        .ok()
        .map(|(width, _)| width);
    let mut pending_width_resize: Option<Instant> = None;
    let mut next_git_poll = Instant::now() + GIT_POLL;
    loop {
        let resize_deadline =
            pending_width_resize.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            maybe = input_rx.recv() => {
                // The input thread only ends if terminal reads fail; treat as EOF.
                let Some(event) = maybe else { return Ok(IdleOutcome::Exit); };
                let mut draw_now = false;
                let mut defer_width_resize = false;
                let mut event = Some(event);
                loop {
                    let event = match event.take() {
                        Some(event) => event,
                        None => match input_rx.try_recv() {
                            Ok(event) => event,
                            Err(_) => break,
                        },
                    };
                    let resize_width_changed = match &event {
                        Event::Resize(width, _) => {
                            let changed = last_resize_width.is_some_and(|last| last != *width)
                                || last_resize_width.is_none();
                            last_resize_width = Some(*width);
                            Some(changed)
                        }
                        _ => None,
                    };
                    match handle_idle_event(&mut tui.screen, event, git_cache) {
                        IdleKey::Continue => match resize_width_changed {
                            Some(true) => defer_width_resize = true,
                            Some(false) if pending_width_resize.is_some() || defer_width_resize => {
                                defer_width_resize = true;
                            }
                            Some(false) => draw_now = true,
                            None => draw_now = true,
                        },
                        IdleKey::Ignore => {}
                        IdleKey::Submit(text) => return Ok(IdleOutcome::Submit(text)),
                        IdleKey::Exit => return Ok(IdleOutcome::Exit),
                        IdleKey::OpenModelPicker => return Ok(IdleOutcome::OpenModelPicker),
                        IdleKey::OpenSkillPicker => return Ok(IdleOutcome::OpenSkillPicker),
                        IdleKey::CycleModel(forward) => return Ok(IdleOutcome::CycleModel(forward)),
                        IdleKey::CycleEffort => return Ok(IdleOutcome::CycleEffort),
                        IdleKey::OpenResumePicker => return Ok(IdleOutcome::OpenResumePicker),
                        IdleKey::OpenTasks => return Ok(IdleOutcome::OpenTasks),
                        IdleKey::OpenSettings => return Ok(IdleOutcome::OpenSettings),
                        IdleKey::ToggleGitMenu => return Ok(IdleOutcome::ToggleGitMenu),
                        IdleKey::ToggleTreeMenu(filter) => {
                            return Ok(IdleOutcome::ToggleTreeMenu(filter));
                        }
                        IdleKey::Menu(action) => return Ok(IdleOutcome::Menu(action)),
                    }
                }
                if draw_now {
                    pending_width_resize = None;
                    tui.draw()?;
                } else if defer_width_resize {
                    pending_width_resize = Some(Instant::now() + RESIZE_REDRAW_DEBOUNCE);
                }
            }
            _ = sleep_until(resize_deadline), if pending_width_resize.is_some() => {
                pending_width_resize = None;
                tui.draw()?;
            }
            _ = tick.tick() => {
                // Idle animation: the start page's IrisMark sweep. `tick`
                // reports false while no start page is shown (the spinner is
                // idle here) and under reduced motion, so a plain idle session
                // stays redraw-free.
                if tui.screen.tick() {
                    tui.draw()?;
                }
                // Debounced idle git poll; a landed refresh repaints the bar.
                let now = Instant::now();
                if now >= next_git_poll {
                    next_git_poll = now + GIT_POLL;
                    git_cache.request_refresh(std::env::current_dir().unwrap_or_default());
                }
                if sync_git_status(tui, git_cache, git_generation) {
                    tui.draw()?;
                }
            }
        }
    }
}

/// Which cancellable harness operation the shared driver runs: a normal agent
/// turn, or an on-demand compaction (`/compact`, whose provider-backed
/// summarizer awaits a model request and deserves the same spinner/cancel
/// treatment as a turn).
enum HarnessOp<'a> {
    Turn(&'a str),
    Compact(&'a str),
}

/// Drive one cancellable harness operation (a turn or an on-demand
/// compaction), staying responsive to input, agent events, approval requests,
/// and the spinner tick. Returns when the operation future completes.
#[allow(clippy::too_many_arguments)]
async fn run_harness_op<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    current_turn: &CurrentTurn,
    op: HarnessOp<'_>,
    steering: &SteeringQueue,
    git_cache: &GitStatusCache,
    git_generation: &mut u64,
) -> Result<bool> {
    let (event_tx, mut event_rx) = unbounded_channel::<UiEvent>();
    let (appr_tx, mut appr_rx) = unbounded_channel::<ApprovalRequest>();
    let bridge = LoopBridge { event_tx, appr_tx };

    // Clear any stale interrupt before arming, then publish the token so the
    // input thread can cancel this turn on Ctrl-C/Esc.
    crate::signals::reset();
    let token = CancellationToken::new();
    *current_turn.lock().expect("turn token lock poisoned") = Some(RunningTurn {
        token: token.clone(),
        esc_cancels: true,
    });

    let mut pending: Option<PendingApproval> = None;
    // Cleared once terminal input reaches EOF so the closed channel is no longer
    // polled (a closed `recv()` is always ready and would otherwise busy-loop).
    let mut input_open = true;

    // Coalesce the burst of agent events a turn emits to ~one draw per 16ms.
    // The caller already drew immediately before this turn, so seed the pacing
    // window as "just drawn" and let the first in-burst event defer to the flush.
    let mut sched = RenderScheduler::new();
    sched.mark_drawn(Instant::now());
    // Width-changing resizes mid-turn (tmux pane drags) hold the scheduler so a
    // storm settles into one full replay instead of one per coalescing window.
    let mut last_resize_width = ratatui::crossterm::terminal::size()
        .ok()
        .map(|(width, _)| width);

    // A compaction is not a turn, so any Enter/Alt-Enter text the user typed
    // into the steering queue while the `/compact` spinner ran must not be
    // carried forward: `compact_now` never drains steering, so it would
    // otherwise be silently merged into the next real prompt.
    let is_compact = matches!(op, HarnessOp::Compact(_));
    let result = {
        let mut turn: futures::future::LocalBoxFuture<'_, Result<()>> = match op {
            HarnessOp::Turn(prompt) => Box::pin(async {
                harness
                    .submit_turn(prompt, &bridge, &bridge, &token)
                    .await
                    .map(|_| ())
            }),
            HarnessOp::Compact(focus) => Box::pin(harness.compact_now_with_focus(
                &bridge,
                &token,
                (!focus.is_empty()).then_some(focus),
            )),
        };
        loop {
            // Compute the next coalesced-draw deadline. When nothing is pending
            // the branch is disabled, so the loop stays CPU-idle (no timer).
            let flush_at: Option<Instant> = match sched.poll(Instant::now()) {
                RenderAction::Idle => None,
                RenderAction::DrawNow => Some(Instant::now()),
                RenderAction::Wait(at) => Some(at),
            };
            let flush_deadline =
                flush_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                res = &mut turn => {
                    // The turn may finish in one poll after emitting a burst of
                    // events; drain them so none are lost.
                    while let Ok(event) = event_rx.try_recv() {
                        tui.screen.apply(event);
                    }
                    break res;
                }
                Some(event) = event_rx.recv() => {
                    // A tool call reaching a terminal state may have mutated
                    // the tree: refresh the git snapshot in the background.
                    if matches!(
                        event,
                        UiEvent::ToolResult { .. }
                            | UiEvent::ToolError { .. }
                            | UiEvent::ToolCancelled(_)
                    ) {
                        git_cache.request_refresh(std::env::current_dir().unwrap_or_default());
                    }
                    tui.screen.apply(event);
                    // A drained (injected) steering/follow-up message lowers the
                    // queued count; refresh it from the live queue before redraw.
                    tui.screen.set_queued(steering.len());
                    request_render(&mut sched, tui)?;
                }
                Some(request) = appr_rx.recv() => {
                    // The same offered decision set the loop uses for the block
                    // footer travels into the screen — including the dirty-gate
                    // variant of `always` — so the REVIEW posture's decision
                    // echo cannot diverge from the block footer.
                    tui.screen.show_approval(
                        request.allow_always,
                        request.allow_project,
                        request.dirty_gate,
                    );
                    pending = Some(PendingApproval {
                        call: request.call.clone(),
                        reply: request.reply,
                        allow_always: request.allow_always,
                        allow_project: request.allow_project,
                    });
                    set_esc_cancel_enabled(current_turn, false);
                    request_render(&mut sched, tui)?;
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
                            if let Event::Resize(width, _) = &event
                                && last_resize_width != Some(*width)
                            {
                                last_resize_width = Some(*width);
                                sched.hold_until(Instant::now() + RESIZE_REDRAW_DEBOUNCE);
                            }
                            if handle_running_event(
                                &mut tui.screen,
                                event,
                                &mut pending,
                                steering,
                                git_cache,
                                &token,
                            ) {
                                // Reflect any just-enqueued (or cleared) steering
                                // input on the working indicator.
                                tui.screen.set_queued(steering.len());
                                request_render(&mut sched, tui)?;
                            }
                            set_esc_cancel_enabled(
                                current_turn,
                                pending.is_none() && tui.screen.session_menu.is_none(),
                            );
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
                        request_render(&mut sched, tui)?;
                    }
                    // Paced commit tick: migrate newly-stable streamed assistant
                    // lines from the mutable tail into scrollback (issue #87).
                    if tui.screen.has_stream_work()
                        && tui.screen.commit_stream_tick(std::time::Instant::now())
                    {
                        request_render(&mut sched, tui)?;
                    }
                    // A landed git refresh repaints the bar (readout dropdowns
                    // keep painting last-known values while the turn runs).
                    if sync_git_status(tui, git_cache, git_generation) {
                        request_render(&mut sched, tui)?;
                    }
                }
                _ = sleep_until(flush_deadline), if flush_at.is_some() => {
                    // Flush a render coalesced earlier in the burst.
                    tui.draw()?;
                    sched.mark_drawn(Instant::now());
                }
            }
        }
    };

    *current_turn.lock().expect("turn token lock poisoned") = None;
    // On cancellation, drop any still-queued steering/follow-up input even if
    // the turn future won the select before the input arm processed the Ctrl-C
    // event (`handle_running_event` clears the queue on the keystroke; this
    // covers the race where that event is never observed here). Idempotent with
    // that path. A compaction always clears too: its typed input is not
    // steering for a turn and must not leak into the next real prompt.
    if token.is_cancelled() || is_compact {
        steering.clear();
        tui.screen.set_queued(0);
    }
    // Any approval still pending here means the turn ended without resolving it
    // (cancellation); its receiver is already gone, so just drop it.
    drop(pending);

    let succeeded = match result {
        Ok(()) => true,
        Err(error) => {
            tui.screen.apply(UiEvent::from_turn_error(&error));
            false
        }
    };
    // The turn is unwinding on an error; not an approval-to-run. (The error
    // event applied just above already set the Finishing phase, so this is a
    // guarded no-op for the phase, but stays honest about the outcome.)
    tui.screen.clear_approval(false);
    Ok(succeeded)
}

/// Drive an open picker/dialog to completion: route keys to the modal, apply the
/// outcomes (model/effort switches, scoped edits, login/logout) at this safe
/// inter-turn boundary, and return when the modal closes (or input ends).
/// Returns the session to swap to when the `/resume` picker selected one, so the
/// caller performs the swap with harness + switch + loader in scope.
#[allow(clippy::too_many_arguments)]
async fn run_modal_phase<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    switch: &mut Option<ModelSwitch<'_, P>>,
    login_backend: &Arc<dyn LoginBackend>,
    current_turn: &CurrentTurn,
    steering: &SteeringQueue,
    git_cache: &GitStatusCache,
    git_generation: &mut u64,
) -> Result<Option<SessionSource>> {
    // The faceplate is home. Its ports are hatches, not doors — but three
    // genuine dialog-guards (the large-context advisory, the OAuth login
    // dialog, the API-key dialog) still overlay it. When one does, stash the
    // panel's view; when the guard resolves — any path — refresh the snapshot
    // (a login can grow the catalog) and reopen the faceplate expanded, cursor
    // intact, BEFORE the next draw so the dock never collapses for a frame
    // (§2.5, the invariant that killed the jank in fa93453).
    let mut settings_stash: Option<crate::ui::settings_menu::PanelView> = None;
    while tui.screen.focus() == FocusTarget::Modal {
        tokio::select! {
            maybe = input_rx.recv() => {
                let Some(event) = maybe else {
                    // Terminal input ended: close the picker and return.
                    tui.screen.close_modal();
                    break;
                };
                // Track focus even while a modal is open so later focus-change
                // reports are coalesced consistently.
                match &event {
                    Event::FocusGained => {
                        tui.screen.set_terminal_focused(true);
                    }
                    Event::FocusLost => {
                        tui.screen.set_terminal_focused(false);
                    }
                    _ => {}
                }
                // Capture the faceplate's view before the key is handled, in
                // case the action about to fire hands off to a dialog-guard.
                let from_settings_view = match &tui.screen.modal {
                    Some(Modal::Settings(panel)) => Some(panel.view()),
                    _ => None,
                };
                let outcome = if let Event::Paste(text) = &event {
                    match tui.screen.modal.as_mut() {
                        Some(modal) => modal.paste_text(text),
                        None => break,
                    }
                } else {
                    match to_modal_key(&event) {
                        Some(key) => match tui.screen.modal.as_mut() {
                            Some(modal) => modal.handle_key(key),
                            None => break,
                        },
                        None => ModalOutcome::Ignore,
                    }
                };
                if let (Some(view), ModalOutcome::Emit(action)) = (&from_settings_view, &outcome)
                    && leaves_faceplate_for_guard(action)
                {
                    settings_stash = Some(view.clone());
                }
                let requested = apply_modal_outcome(
                    outcome,
                    harness,
                    tui,
                    input_rx,
                    tick,
                    switch,
                    login_backend,
                    current_turn,
                    steering,
                    git_cache,
                    git_generation,
                )
                .await?;
                if requested.is_some() {
                    return Ok(requested);
                }
                // The faceplate is home: when a dialog-guard it handed off to has
                // resolved (focus left the modal region), refresh the snapshot
                // and reopen the panel expanded BEFORE drawing, so the dock never
                // collapses for a frame on the way back.
                if tui.screen.focus() != FocusTarget::Modal
                    && let Some(view) = settings_stash.take()
                    && let Some(sw) = switch.as_mut()
                {
                    tui.screen
                        .open_modal(picker::refresh_settings_panel(view, None, harness, sw));
                }
                // Once the panel itself is in front again, nothing is pending.
                if matches!(tui.screen.modal, Some(Modal::Settings(_))) {
                    settings_stash = None;
                }
                // The picker may have switched model/effort; refresh the
                // footer before drawing so it never shows a stale model.
                refresh_footer(tui, switch);
                tui.draw()?;
            }
            _ = tick.tick() => {
                // Keep the tick grid live while a modal is open: the settings
                // panel's detent flash settles here, and the start page's
                // IrisMark keeps sweeping behind a docked picker.
                if tui.screen.tick() {
                    tui.draw()?;
                }
            }
        }
    }
    Ok(None)
}

/// Whether a faceplate action hands off to a dialog-guard that overlays the
/// panel, so the modal phase should stash the view and reopen the faceplate when
/// the guard resolves (§2.5). Mutation-safety saves are included because enabling
/// can conditionally open the jj consent guard. Model select/cycle only leave when
/// they trip the large-context advisory; otherwise they refresh the panel in place
/// (a Keep or a Replace that is still the faceplate), and the "panel in front"
/// check clears the stash harmlessly.
fn leaves_faceplate_for_guard(action: &ModalAction) -> bool {
    matches!(
        action,
        ModalAction::SelectModel { .. }
            | ModalAction::ConfirmModelSwitch { .. }
            | ModalAction::CycleModel { .. }
            | ModalAction::SaveSetting {
                field: crate::ui::settings_menu::Field::MutationSafety,
                ..
            }
            | ModalAction::BeginLogin(_)
            | ModalAction::OpenApiKeyDialog(_)
    )
}

/// Interpret one [`ModalOutcome`]. Returns a requested session swap (from the
/// `/resume` picker) for the caller to perform.
#[allow(clippy::too_many_arguments)]
async fn apply_modal_outcome<P: ChatProvider>(
    outcome: ModalOutcome,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    switch: &mut Option<ModelSwitch<'_, P>>,
    login_backend: &Arc<dyn LoginBackend>,
    current_turn: &CurrentTurn,
    steering: &SteeringQueue,
    git_cache: &GitStatusCache,
    git_generation: &mut u64,
) -> Result<Option<SessionSource>> {
    match outcome {
        ModalOutcome::Ignore | ModalOutcome::Redraw => Ok(None),
        ModalOutcome::Close => {
            tui.screen.close_modal();
            Ok(None)
        }
        ModalOutcome::Emit(action) => {
            dispatch_action(
                action,
                harness,
                tui,
                input_rx,
                tick,
                switch,
                login_backend,
                current_turn,
                steering,
                git_cache,
                git_generation,
            )
            .await
        }
    }
}

/// Apply a [`ModalAction`]: model/scoped/effort actions go through the picker;
/// login/logout actions are handled here (they need the auth store / backend);
/// a `/resume` selection is returned up as the session to swap to.
#[allow(clippy::too_many_arguments)]
async fn dispatch_action<P: ChatProvider>(
    action: ModalAction,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    switch: &mut Option<ModelSwitch<'_, P>>,
    login_backend: &Arc<dyn LoginBackend>,
    current_turn: &CurrentTurn,
    steering: &SteeringQueue,
    git_cache: &GitStatusCache,
    git_generation: &mut u64,
) -> Result<Option<SessionSource>> {
    match action {
        ModalAction::ConfirmModelSwitch {
            id,
            effort,
            save_default,
            compact_first: true,
        } => {
            tui.screen.close_modal();
            tui.screen.start_turn();
            tui.draw()?;
            let compact_ok = run_harness_op(
                harness,
                tui,
                input_rx,
                tick,
                current_turn,
                HarnessOp::Compact(""),
                steering,
                git_cache,
                git_generation,
            )
            .await?;
            tui.screen.end_background_work();
            if !compact_ok {
                open_deferred_settings(harness, tui, switch, steering);
                return Ok(None);
            }
            let action = ModalAction::ConfirmModelSwitch {
                id,
                effort,
                save_default,
                compact_first: false,
            };
            let Some(sw) = switch.as_mut() else {
                return Ok(None);
            };
            let before = sw.selection().clone();
            // Front is the confirm prompt (not the faceplate): no view to
            // preserve here — the run_modal_phase stash reopens the faceplate.
            let sink = SettingsEventSink::default();
            let result = picker::apply_action(action, None, harness, sw, &sink);
            for event in sink.drain() {
                tui.screen.apply(event);
            }
            let after = sw.selection().clone();
            match result {
                ActionResult::Close(lines) => {
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                    tui.screen.close_modal();
                }
                ActionResult::Keep(lines) => {
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                }
                ActionResult::Replace(modal, lines) => {
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                    tui.screen.open_modal(*modal);
                }
            }
            open_deferred_settings(harness, tui, switch, steering);
        }
        ModalAction::SetNativeJj(enabled) => {
            let previous = harness.native_jj_enabled();
            let master = harness.mutation_safety_enabled();
            let lines = match harness.configure_mutation_safety(master, enabled) {
                Err(error) => vec![error.to_string()],
                Ok(()) => {
                    match crate::wayland::trust::set_native_jj(harness.workspace(), enabled) {
                        Ok(()) => vec![format!(
                            "native jj integration {} for this workspace",
                            if enabled { "enabled" } else { "disabled" }
                        )],
                        Err(error) => {
                            let _ = harness.configure_mutation_safety(master, previous);
                            vec![format!("could not save native jj preference: {error:#}")]
                        }
                    }
                }
            };
            tui.screen.close_modal();
            apply_notices(tui, lines);
        }
        ModalAction::ResumeSession(id) => {
            // Close the picker and hand the chosen session up to the loop, which
            // performs the swap at the safe inter-turn boundary.
            tui.screen.close_modal();
            return Ok(Some(SessionSource::Resume(id)));
        }
        ModalAction::AdoptTask(id) => {
            // Adopt the recoverable task at this safe inter-turn boundary (#288,
            // ADR-0031): rehydrate its checkpoint chain so `/rollback` /
            // `/accept` / `/checkpoint` operate on the real chain. Adoption never
            // implicitly resumes a session; when exactly one session is linked we
            // open an explicit "also resume" offer (a second, separate action).
            tui.screen.close_modal();
            match harness.adopt_task(&id) {
                Ok(adopted) => {
                    let (lines, resume) = picker::adopt_notice(&adopted);
                    apply_notices(tui, lines);
                    if let Some(session_id) = resume {
                        tui.screen.open_modal(picker::resume_offer(&session_id));
                    }
                }
                Err(crate::wayland::git_safety::AdoptError::TaskActive) => apply_notices(
                    tui,
                    vec!["accept or undo the active task before resuming another one".to_string()],
                ),
                Err(crate::wayland::git_safety::AdoptError::Unavailable) => apply_notices(
                    tui,
                    vec![format!(
                        "could not resume task {id}: it may have been accepted, undone, or claimed by another process."
                    )],
                ),
            }
        }
        ModalAction::ViewTaskSessions(id) => {
            // Show the task's linked sessions in the modal's detail view
            // (ADR-0031 session lookup): the deterministic, bounded, cwd-scoped
            // extraction, read for display/audit only -- never a recovery input.
            // Rebuild the task modal (so leaving the detail returns to the list)
            // and attach the fetched lines.
            let lines = crate::cli::sessions_for_task_lines(harness.workspace(), &id);
            match picker::build_tasks_modal(harness, tui.screen.footer_git()) {
                Some(Modal::Tasks(mut picker)) => {
                    picker.show_detail(&id, lines);
                    tui.screen.open_modal(Modal::Tasks(picker));
                }
                // The task vanished (settled/adopted elsewhere) between opening
                // the modal and here: surface the detail as notices and close.
                _ => {
                    tui.screen.close_modal();
                    apply_notices(tui, lines);
                }
            }
        }
        ModalAction::AcceptTask => {
            tui.screen.close_modal();
            let lines = crate::cli::handle_checkpoint_command("/accept", harness)
                .unwrap_or_else(|| vec!["no unreviewed Iris changes to accept".to_string()]);
            apply_notices(tui, lines);
        }
        ModalAction::ShowTaskDiff => {
            tui.screen.close_modal();
            tui.screen.apply(crate::cli::task_diff_event(harness));
        }
        ModalAction::ListTaskRollback => {
            tui.screen.close_modal();
            let lines = crate::cli::handle_checkpoint_command("/rollback", harness)
                .unwrap_or_else(|| vec!["no unreviewed Iris changes to roll back".to_string()]);
            apply_notices(tui, lines);
        }
        // Providers hatch → OAuth/subscription login (a dialog-guard; the
        // run_modal_phase stash reopens the faceplate expanded on return).
        ModalAction::BeginLogin(provider) => {
            run_login(provider, tui, input_rx, tick, login_backend).await?;
        }
        // Providers hatch → API-key dialog (a dialog-guard).
        ModalAction::OpenApiKeyDialog(provider_id) => {
            tui.screen
                .open_modal(login::open_api_key_dialog(&provider_id));
        }
        ModalAction::SaveApiKey(provider_id) => {
            let key = match tui.screen.modal.as_mut() {
                Some(Modal::ApiKeyDialog(dialog)) => dialog.take_input(),
                _ => String::new(),
            };
            let lines = match AuthStore::from_env() {
                Ok(auth) => login::apply_api_key_login(&provider_id, &key, &auth),
                Err(error) => vec![format!("auth unavailable: {error:#}")],
            };
            apply_notices(tui, lines);
            // Resolving the API-key dialog leaves the modal region; the stash
            // reopens the faceplate expanded on the providers hatch.
            tui.screen.close_modal();
        }
        ModalAction::Logout(id) => {
            // The providers hatch's `x`: log out immediately and rebuild the
            // faceplate in place so the row drops to `○ · —` and the catalog /
            // scope rows refresh (a logout shrinks the model catalog).
            let view = match &tui.screen.modal {
                Some(Modal::Settings(panel)) => Some(panel.view()),
                _ => None,
            };
            let flash = view.as_ref().map(|view| view.cursor());
            let lines = match AuthStore::from_env() {
                Ok(auth) => login::apply_logout(&id, &auth),
                Err(error) => vec![format!("auth unavailable: {error:#}")],
            };
            apply_notices(tui, lines);
            match (view, switch.as_ref()) {
                (Some(view), Some(sw)) => {
                    tui.screen
                        .open_modal(picker::refresh_settings_panel(view, flash, harness, sw));
                }
                _ => tui.screen.close_modal(),
            }
        }
        ModalAction::InsertSkillMention { name, path } => {
            tui.screen.close_modal();
            tui.screen
                .editor
                .insert_str(format!("[${name}](skill://{path}) "));
            tui.screen.sync_palette();
        }
        // Model / scoped / effort / settings / policy actions.
        other => {
            let live_setting = match &other {
                ModalAction::SaveSetting { field, value } => Some((*field, value.clone())),
                _ => None,
            };
            // Capture the faceplate's view before the action so a snapshot-
            // refreshing action rebuilds it in place without losing position.
            let view = match &tui.screen.modal {
                Some(Modal::Settings(panel)) => Some(panel.view()),
                _ => None,
            };
            let Some(sw) = switch.as_mut() else {
                tui.screen.close_modal();
                return Ok(None);
            };
            let before = sw.selection().clone();
            // A settings apply runs with no `LoopBridge`; collect any
            // harness-owned event (the settings-off compaction `Cancelled`
            // lifecycle) and drain it onto the screen once the action returns.
            let sink = SettingsEventSink::default();
            let result = picker::apply_action(other, view, harness, sw, &sink);
            for event in sink.drain() {
                tui.screen.apply(event);
            }
            // The picker reports a successful in-place setting save as an empty
            // Keep. Apply Tier-3 settings to this live screen only after that
            // write succeeds; a failed save rebuilds from disk and must not
            // leave the running UI in an unpersisted posture.
            if matches!(&result, ActionResult::Keep(lines) if lines.is_empty())
                && let Some((field, value)) = live_setting
            {
                apply_live_tui_setting(&mut tui.screen, field, value.as_deref());
            }
            tui.screen
                .set_approval_policy(effective_approval_policy(harness));
            // A faceplate change can cancel the in-flight background compaction
            // (turning automatic compaction off): reconcile the status chip with
            // the live harness so it clears at the out-of-turn settings write.
            tui.screen.set_compaction_running(
                harness
                    .context_diagnostics()
                    .is_some_and(|diag| diag.background_running),
            );
            let after = sw.selection().clone();
            match result {
                ActionResult::Close(lines) => {
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                    tui.screen.close_modal();
                }
                ActionResult::Keep(lines) => {
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                }
                ActionResult::Replace(modal, lines) => {
                    apply_model_switch_lines(tui, harness, Some(&before), Some(&after), lines);
                    tui.screen.open_modal(*modal);
                }
            }
        }
    }
    Ok(None)
}

/// Mirror successfully-persisted Tier-3 settings onto the current screen. The
/// remaining fields are runtime/config concerns and already have their own live
/// application paths in the picker or harness.
fn apply_live_tui_setting(screen: &mut Screen, field: settings_menu::Field, value: Option<&str>) {
    match field {
        settings_menu::Field::ReducedMotion => screen.set_reduced_motion(value == Some("true")),
        settings_menu::Field::ScrollSpeed => {
            if let Some(speed) = value.and_then(|value| value.parse::<u16>().ok()) {
                screen.scroll_speed = speed.clamp(1, 100);
            }
        }
        settings_menu::Field::Theme => screen.sync_palette(),
        _ => {}
    }
}

/// Resolution of the blocking login task.
enum LoginResolution {
    Done(std::result::Result<Result<LoginOutcome>, tokio::task::JoinError>),
    Cancelled,
}

/// Run a blocking OAuth login on a blocking task while keeping the dialog live:
/// auth-URL / progress updates flow over a channel; Esc/Ctrl+C cancels and the
/// shared [`CancellationToken`] is tripped so the bounded, non-blocking callback
/// helper notices it within a poll tick, returns, and releases the port. The
/// loop then awaits the blocking task before reporting "cancelled", so a late
/// browser callback can never persist credentials behind a dismissed dialog.
/// Anthropic additionally accepts a pasted authorization code / redirect URL,
/// forwarded to the helper over a manual-input channel.
async fn run_login(
    provider: crate::mimir::selection::ProviderId,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    login_backend: &Arc<dyn LoginBackend>,
) -> Result<()> {
    use crate::mimir::selection::ProviderId;

    let name = provider.display_name().to_string();
    let manual = matches!(provider, ProviderId::Anthropic);
    let mut dialog = LoginDialog::new(&name, manual);
    tui.screen.open_modal(Modal::LoginDialog(dialog.clone()));
    tui.draw()?;

    let (upd_tx, mut upd_rx) = unbounded_channel::<LoginUpdate>();
    let (manual_tx, manual_rx) = std::sync::mpsc::channel::<String>();
    let cancel = CancellationToken::new();
    let backend = Arc::clone(login_backend);
    let task_cancel = cancel.clone();
    let join = tokio::task::spawn_blocking(move || {
        backend.login(provider, &task_cancel, Some(&manual_rx), &move |update| {
            let _ = upd_tx.send(update);
        })
    });
    let mut join = std::pin::pin!(join);

    let resolution = loop {
        tokio::select! {
            res = &mut join => break LoginResolution::Done(res),
            Some(update) = upd_rx.recv() => {
                apply_login_update(&mut dialog, update);
                tui.screen.open_modal(Modal::LoginDialog(dialog.clone()));
                tui.draw()?;
            }
            maybe = input_rx.recv() => {
                match maybe {
                    Some(event) if is_modal_cancel(&event) => break LoginResolution::Cancelled,
                    Some(event) => {
                        if handle_login_input_event(&mut dialog, &event, &manual_tx) {
                            tui.screen.open_modal(Modal::LoginDialog(dialog.clone()));
                            tui.draw()?;
                        }
                    }
                    None => break LoginResolution::Cancelled,
                }
            }
            _ = tick.tick() => {}
        }
    };

    let lines = match resolution {
        LoginResolution::Done(Ok(Ok(outcome))) => login::login_complete_lines(provider, &outcome),
        LoginResolution::Done(Ok(Err(error))) => {
            vec![format!("Failed to login to {name}: {error:#}")]
        }
        LoginResolution::Done(Err(join_error)) => vec![format!("Login task failed: {join_error}")],
        LoginResolution::Cancelled => {
            // Trip the token so the bounded callback helper stops waiting and
            // releases the port, then await the task so a late callback cannot
            // persist credentials after we report "cancelled".
            cancel.cancel();
            dialog.push_line("Cancelling...".to_string());
            tui.screen.open_modal(Modal::LoginDialog(dialog.clone()));
            tui.draw()?;
            let _ = join.await;
            vec!["Login cancelled".to_string()]
        }
    };
    apply_notices(tui, lines);
    // Close the login dialog but do NOT draw here: run_login is reached only from
    // the providers hatch, so the `run_modal_phase` stash owns the return — it
    // refreshes the snapshot (a login can grow the catalog) and reopens the
    // faceplate expanded BEFORE the next draw. A draw here would paint one frame
    // with no modal (the dock collapsed) between close and reopen (§7 criterion 7).
    tui.screen.close_modal();
    Ok(())
}

fn handle_login_input_event(
    dialog: &mut LoginDialog,
    event: &Event,
    manual_tx: &std::sync::mpsc::Sender<String>,
) -> bool {
    if dialog.accepts_manual_input() {
        let _ = apply_manual_key(dialog, event, manual_tx);
    }
    true
}

/// Apply a keystroke to the manual-paste buffer of an Anthropic login dialog.
/// Returns whether the dialog changed (and so should be redrawn). On Enter the
/// buffered code/redirect URL is sent to the blocking helper.
fn apply_manual_key(
    dialog: &mut LoginDialog,
    event: &Event,
    manual_tx: &std::sync::mpsc::Sender<String>,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return false;
    }
    match key.code {
        KeyCode::Enter => {
            let input = dialog.take_input();
            if input.trim().is_empty() {
                return true;
            }
            let _ = manual_tx.send(input);
            dialog.push_line("Submitting pasted authorization...".to_string());
            true
        }
        KeyCode::Backspace => {
            dialog.backspace();
            true
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            dialog.push_char(ch);
            true
        }
        _ => false,
    }
}

/// Update the login dialog body from a backend callback.
fn apply_login_update(dialog: &mut LoginDialog, update: LoginUpdate) {
    match update {
        LoginUpdate::AuthUrl { url, hint } => {
            // The modal cannot carry a clickable hyperlink and a long URL wraps
            // in the box, so open the browser for the user; the wrapped URL stays
            // as a copy/paste fallback.
            crate::ui::login::open_in_browser(&url);
            dialog.set_lines(vec![format!("Open: {url}"), hint]);
        }
        LoginUpdate::Progress(line) => dialog.push_line(line),
    }
}

/// Whether an event is a modal cancel (Esc or Ctrl+C).
fn is_modal_cancel(event: &Event) -> bool {
    matches!(event, Event::Key(key)
        if (key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat)
            && (matches!(key.code, KeyCode::Esc)
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')))))
}

/// Translate a crossterm event into the modal's neutral key, or `None` for keys
/// a picker does not consume.
fn to_modal_key(event: &Event) -> Option<ModalKey> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    Some(match key.code {
        KeyCode::Up if alt => ModalKey::AltUp,
        KeyCode::Down if alt => ModalKey::AltDown,
        KeyCode::Up => ModalKey::Up,
        KeyCode::Down => ModalKey::Down,
        KeyCode::Left if !ctrl && !alt => ModalKey::Left,
        KeyCode::Right if !ctrl && !alt => ModalKey::Right,
        KeyCode::Enter => ModalKey::Enter,
        KeyCode::Esc => ModalKey::Esc,
        KeyCode::Backspace => ModalKey::Backspace,
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => ModalKey::CtrlC,
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => ModalKey::CtrlA,
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => ModalKey::CtrlX,
        KeyCode::Char('p') | KeyCode::Char('P') if ctrl => ModalKey::CtrlP,
        KeyCode::Char('s') | KeyCode::Char('S') if ctrl => ModalKey::CtrlS,
        KeyCode::Char(c) if !ctrl && !alt => ModalKey::Char(c),
        _ => return None,
    })
}

/// Spawn the blocking terminal-read thread. It forwards every event to the loop
/// and, on a raw Ctrl-C while a turn is active (or Esc when no higher-priority
/// UI owns it), cancels that turn's token from this OS thread (the executor
/// thread may be blocked in a synchronous tool).
fn spawn_input_thread(tx: UnboundedSender<Event>, current_turn: CurrentTurn) {
    std::thread::spawn(move || {
        // Ends when terminal reads fail or the loop drops the receiver.
        while let Ok(event) = event::read() {
            if is_ctrl_c(&event) || is_esc_key(&event) {
                // Hold the lock across the cancel so the turn cannot end and a
                // new one begin in between (which would leak a stale interrupt
                // and cancel the wrong turn). Ctrl-C also sets the interrupt
                // flag (a repeat reaps bash child groups); Esc is a soft turn
                // cancel and only applies when the loop says no menu/approval
                // owns Esc.
                let guard = current_turn.lock().expect("turn token lock poisoned");
                if let Some(turn) = guard.as_ref() {
                    if is_ctrl_c(&event) {
                        crate::signals::interrupt_from_terminal();
                        turn.token.cancel();
                    } else if turn.esc_cancels {
                        turn.token.cancel();
                    }
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

fn is_esc_key(event: &Event) -> bool {
    matches!(event, Event::Key(key)
        if matches!(key.code, KeyCode::Esc)
            && (key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat))
}

/// Insert pasted text as real lines (the multiline editor keeps newlines now,
/// unlike the old single-row flatten). `\r\n` is normalized to `\n`.
fn insert_paste(screen: &mut Screen, text: &str) {
    screen.reset_prompt_history_cursor();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            screen.editor.insert_newline();
        }
        screen.editor.insert_str(line.trim_end_matches('\r'));
    }
}

fn prompt_history_key(screen: &mut Screen, code: KeyCode, ctrl: bool, alt: bool) -> bool {
    if ctrl || alt {
        return false;
    }
    match code {
        KeyCode::Up if screen.editor_is_empty() || screen.browsing_prompt_history() => {
            screen.prompt_history_previous()
        }
        KeyCode::Down if screen.browsing_prompt_history() => screen.prompt_history_next(),
        _ => false,
    }
}

/// Idle-phase key map: edits the `TextArea`, drives the slash palette, scrolls
/// the transcript, submits, or exits. See the module docs for the binding list.
fn handle_idle_event(screen: &mut Screen, event: Event, git_cache: &GitStatusCache) -> IdleKey {
    let key = match event {
        Event::Paste(text) => {
            insert_paste(screen, &text);
            screen.sync_palette();
            return IdleKey::Continue;
        }
        // Pager mode captures the mouse: the wheel scrolls the Iris-owned
        // scrollback. Inline mode never enables capture, so no Mouse events
        // arrive there and the terminal owns scroll/select/copy natively.
        // Clicks target the session bar's cwd/git segments and dropdown rows.
        Event::Mouse(mouse) => {
            if let Some(key) = session_bar_click(screen, &mouse, git_cache) {
                return key;
            }
            if sticky_prompt_click(screen, &mouse) {
                return IdleKey::Continue;
            }
            if header_click(screen, &mouse) {
                return IdleKey::Continue;
            }
            if pager_link_click(screen, &mouse) {
                return IdleKey::Continue;
            }
            return if pager_wheel(screen, &mouse) {
                IdleKey::Continue
            } else {
                IdleKey::Ignore
            };
        }
        Event::Resize(..) => return IdleKey::Continue,
        // Focus reports are tracked only to coalesce duplicate focus changes; a
        // regain redraws once so a pane switched back to is visually current.
        Event::FocusGained => {
            return if screen.set_terminal_focused(true) {
                IdleKey::Continue
            } else {
                IdleKey::Ignore
            };
        }
        Event::FocusLost => {
            screen.set_terminal_focused(false);
            return IdleKey::Ignore;
        }
        Event::Key(key) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
            key
        }
        _ => return IdleKey::Ignore,
    };

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let input = screen.editor_text();

    // Explicit focus routing (Editor < Palette < SessionMenu < Modal). Modals
    // run in their own phase, so idle focus is Editor, Palette, or SessionMenu
    // here. Reuse the input snapshot instead of re-joining the editor buffer.
    let focus = screen.focus_for(&input);

    // SessionBar dropdown routing: while open, the dropdown owns keys (the
    // list-state law: no free typing while a list has focus; input rows make
    // printable keys text via the menu's own state machine). `esc` closes here
    // and never reaches any other path.
    if focus == FocusTarget::SessionMenu {
        if ctrl && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G')) {
            return IdleKey::ToggleGitMenu;
        }
        let Some(menu_key) = to_menu_key(key.code, ctrl) else {
            return IdleKey::Ignore;
        };
        let readonly = screen.menu_readonly();
        let outcome = screen
            .session_menu
            .as_mut()
            .map(|menu| menu.handle_key(menu_key, readonly))
            .unwrap_or(MenuOutcome::Ignore);
        return menu_outcome_key(screen, outcome);
    }

    // Pager scroll keys act before editor routing (scrollback has implicit
    // focus for the nav keys); typing/other keys fall through to the composer.
    if pager_scroll_key(screen, key.code, ctrl, alt) {
        return IdleKey::Continue;
    }
    // Tab focus toggle + focused-scrollback entry navigation (ADR-0029).
    // Never on the start page (no transcript to focus) and never while the
    // slash palette is open (Tab/arrows stay palette keys there).
    if !screen.start_page_active()
        && focus != FocusTarget::Palette
        && scrollback_focus_key(screen, key.code, ctrl, alt)
    {
        return IdleKey::Continue;
    }

    // Any key completes the lamp test immediately, then keeps its normal
    // meaning. Startup is a visual ritual, never an input gate.
    if let Some(page) = screen.start_page.as_mut() {
        page.skip_boot();
    }

    // Start-page launcher routing: the listed ctrl-chords activate directly,
    // and while the composer is empty ↑/↓/↵ drive the launcher selection. The
    // composer stays live throughout — typing a task and submitting it starts
    // the session (the palette keeps priority for `/` input).
    if screen.start_page_active() && focus != FocusTarget::Palette {
        if ctrl {
            match key.code {
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    screen.leave_start_page();
                    return IdleKey::Continue;
                }
                // Only while the composer is empty: once the user is typing a
                // task, ctrl-r stays the editor's redo.
                KeyCode::Char('r') | KeyCode::Char('R') if screen.editor_is_empty() => {
                    return IdleKey::OpenResumePicker;
                }
                KeyCode::Char('t') | KeyCode::Char('T') => return IdleKey::OpenTasks,
                KeyCode::Char(',') => return IdleKey::OpenSettings,
                KeyCode::Char('q') | KeyCode::Char('Q') => return IdleKey::Exit,
                _ => {}
            }
        }
        if screen.editor_is_empty() && !ctrl && !alt {
            match key.code {
                KeyCode::Up => {
                    if let Some(page) = screen.start_page.as_mut() {
                        page.up();
                    }
                    return IdleKey::Continue;
                }
                KeyCode::Down => {
                    if let Some(page) = screen.start_page.as_mut() {
                        page.down();
                    }
                    return IdleKey::Continue;
                }
                KeyCode::Enter => {
                    let action = screen
                        .start_page
                        .as_ref()
                        .map(|page| page.selected_action());
                    return match action {
                        Some(StartAction::NewSession) => {
                            screen.leave_start_page();
                            IdleKey::Continue
                        }
                        Some(StartAction::ResumeSession) => IdleKey::OpenResumePicker,
                        Some(StartAction::Tasks) => IdleKey::OpenTasks,
                        Some(StartAction::Settings) => IdleKey::OpenSettings,
                        Some(StartAction::Quit) => IdleKey::Exit,
                        None => IdleKey::Continue,
                    };
                }
                _ => {}
            }
        }
    }

    // Global picker chords work regardless of editor contents (but not while the
    // slash palette is steering Up/Down/Enter): Ctrl+L opens the model picker,
    // Ctrl+P / Shift+Ctrl+P cycle models, Shift+Tab cycles effort.
    if focus != FocusTarget::Palette {
        match key.code {
            KeyCode::Char('l') | KeyCode::Char('L') if ctrl => {
                return IdleKey::OpenModelPicker;
            }
            KeyCode::Char('p') | KeyCode::Char('P') if ctrl => {
                return IdleKey::CycleModel(!shift);
            }
            KeyCode::Char('t') | KeyCode::Char('T') if ctrl && screen.pager_active => {
                screen.toggle_mouse();
                return IdleKey::Continue;
            }
            KeyCode::Char('o') | KeyCode::Char('O') if ctrl => {
                // ctrl+o has one meaning everywhere: toggle transcript folds. The
                // pinned prompt band has its own toggle (click, or `o` in pager
                // mode) so it never pre-empts a reader's collapsed blocks.
                screen.toggle_all_panels();
                return IdleKey::Continue;
            }
            KeyCode::Char('g') | KeyCode::Char('G') if ctrl => {
                return IdleKey::ToggleGitMenu;
            }
            KeyCode::BackTab => return IdleKey::CycleEffort,
            _ => {}
        }
    }

    // Palette navigation takes priority while it is open with matches.
    if focus == FocusTarget::Palette {
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
            KeyCode::Enter if !alt && !ctrl && !shift => {
                if let Some(cmd) = screen.palette.accept(&input) {
                    return dispatch_command(screen, cmd);
                }
                return IdleKey::Continue;
            }
            _ => {}
        }
    }

    if prompt_history_key(screen, key.code, ctrl, alt) {
        return IdleKey::Continue;
    }

    match key.code {
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
            screen.editor.delete_next_char();
            return IdleKey::Continue;
        }
        // Transcript scrolling is handled natively by the terminal over its
        // scrollback (no in-app scroll offset), so PageUp/PageDown fall through.
        KeyCode::Enter if shift || ctrl => {
            screen.reset_prompt_history_cursor();
            screen.editor.insert_newline();
        }
        KeyCode::Enter => {
            let text = screen.submit();
            if text.trim().is_empty() {
                return IdleKey::Continue;
            }
            return IdleKey::Submit(text);
        }
        // `@` as the FIRST character of an empty composer is the file-reference
        // idiom: it opens the directory tree directly in filter mode instead
        // of typing.
        KeyCode::Char('@') if !ctrl && !alt && screen.editor_is_empty() => {
            return IdleKey::ToggleTreeMenu(true);
        }
        // Codex skill-mention idiom: `$` opens the searchable picker instead of
        // inserting a literal sigil. Selecting a row inserts a path-qualified
        // mention at the current cursor.
        KeyCode::Char('$') if !ctrl && !alt => return IdleKey::OpenSkillPicker,
        // Everything else is pure text editing, shared with the running phase so
        // the composer behaves identically whether or not a turn is in flight.
        code => {
            apply_editor_key(screen, code, ctrl, alt);
        }
    }

    screen.sync_palette();
    IdleKey::Continue
}

/// Apply one pure text-editing key to the composer. Shared by the idle phase and
/// the running (steering) phase so the composer edits identically in both;
/// control-flow keys (submit, exit, palette, global chords, steer/follow-up) are
/// resolved by the callers before they delegate here. Returns whether the key
/// was an editing key (and thus a redraw is warranted).
fn apply_editor_key(screen: &mut Screen, code: KeyCode, ctrl: bool, alt: bool) -> bool {
    if !matches!(code, KeyCode::Up | KeyCode::Down) {
        screen.reset_prompt_history_cursor();
    }
    // Several `TextArea` edit methods return a bool (whether they mutated); the
    // arms are wrapped in blocks so every arm evaluates to `()` and the caller's
    // redraw flag is driven by whether a key matched, not by that return.
    match code {
        // --- kill-ring / undo / redo ---
        KeyCode::Char('j') if ctrl => {
            screen.editor.insert_newline();
        }
        KeyCode::Char('u') if ctrl => {
            screen.editor.delete_line_by_head();
        }
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
        KeyCode::Char('-') if ctrl => {
            screen.editor.undo();
        }
        KeyCode::Char('r') if ctrl => {
            screen.editor.redo();
        }

        // --- cursor / word navigation ---
        KeyCode::Char('a') if ctrl => screen.editor.move_cursor(CursorMove::Head),
        KeyCode::Char('e') if ctrl => screen.editor.move_cursor(CursorMove::End),
        KeyCode::Char('b') if ctrl => screen.editor.move_cursor(CursorMove::Back),
        KeyCode::Char('f') if ctrl => screen.editor.move_cursor(CursorMove::Forward),
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
        KeyCode::Delete if alt => {
            screen.editor.delete_next_word();
        }
        KeyCode::Delete => {
            screen.editor.delete_next_char();
        }
        KeyCode::Tab => {
            screen.editor.insert_str("    ");
        }
        KeyCode::Char('\n') => {
            screen.editor.insert_newline();
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            screen.editor.insert_char(c);
        }
        _ => return false,
    }
    true
}

/// Map a palette-accepted command to its idle outcome. `Exit` ends the session;
/// `Submit` submits the command name as a line so the shared model-switch
/// handler routes it (the user may then add args; a bare submit is the
/// read-only / usage view).
fn dispatch_command(screen: &mut Screen, cmd: &SlashCommand) -> IdleKey {
    screen.clear_editor();
    match cmd.action {
        SlashAction::Exit => IdleKey::Exit,
        SlashAction::Submit => IdleKey::Submit(cmd.name.to_string()),
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
        screen.clear_approval(false);
    }
}

/// Handle one terminal event while a turn runs. The composer stays live so the
/// user can queue a steering message (Enter) or a follow-up (Alt+Enter) without
/// interrupting the turn; Ctrl-C aborts and Ctrl-O toggles the latest panel.
/// While a tool is awaiting approval the composer is frozen and only the
/// approval keys (plus Ctrl-C/-O) act, so a `y`/`n` can't be both an answer and
/// typed text. Returns whether a redraw is needed.
/// Tab focus toggle and focused-scrollback entry keys (ADR-0029): arrows
/// select entries (falling back to line scroll with none), Left/Right
/// fold/reveal, Enter toggles the fold. Typing a printable character returns
/// focus to the prompt WITHOUT being consumed, so it lands in the composer;
/// Esc is never a focus or nav key.
fn scrollback_focus_key(screen: &mut Screen, code: KeyCode, ctrl: bool, alt: bool) -> bool {
    if !screen.pager_active || ctrl || alt {
        return false;
    }
    match code {
        KeyCode::Tab => {
            screen.toggle_scrollback_focus();
            true
        }
        _ if !screen.scrollback_focus => false,
        KeyCode::Up => {
            screen.move_selection(-1);
            true
        }
        KeyCode::Down => {
            screen.move_selection(1);
            true
        }
        KeyCode::Left => {
            screen.set_selected_expanded(false);
            true
        }
        KeyCode::Right => {
            screen.set_selected_expanded(true);
            true
        }
        KeyCode::Enter => {
            screen.toggle_selected_entry();
            true
        }
        // Search navigation while a `/find` is active: n = older, N = newer.
        // Checked before the type-through fallthrough so the two letters
        // navigate instead of stealing focus back to the prompt.
        KeyCode::Char('n') if screen.search.is_some() => {
            screen.search_step(-1);
            true
        }
        KeyCode::Char('N') if screen.search.is_some() => {
            screen.search_step(1);
            true
        }
        // `o` toggles the pinned prompt band (the job card) -- legal here because
        // the scrollback list, not the composer, holds focus (the list-state law).
        // It consumes only when a band is actually pinned; otherwise it types like
        // any other letter. ctrl+o is unrelated -- it toggles transcript folds.
        KeyCode::Char('o') | KeyCode::Char('O') => {
            if screen.toggle_sticky_prompt() {
                true
            } else {
                screen.focus_prompt();
                false
            }
        }
        // Typing always returns to the prompt; the key falls through and is
        // handled by the composer (it types). Esc keeps its cancel/clear
        // semantics untouched.
        KeyCode::Char(_) | KeyCode::Backspace => {
            screen.focus_prompt();
            false
        }
        _ => false,
    }
}

/// Pager-mode sticky-prompt disclosure click: a left-button-down on the pinned
/// prompt header row toggles THAT sticky prompt. `false` = not a sticky-prompt
/// click (fall through to transcript headers, links, and wheel handling). Only
/// fires under pager mouse capture.
fn sticky_prompt_click(screen: &mut Screen, mouse: &ratatui::crossterm::event::MouseEvent) -> bool {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    if !screen.pager_active || !screen.mouse_capture {
        return false;
    }
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    screen.toggle_sticky_prompt_at_screen_row(mouse.row)
}

/// Pager-mode disclosure click: a left-button-down on a foldable block's
/// header row toggles THAT block. `None`/`false` = not a header click (fall
/// through to wheel handling). Only fires under pager mouse capture.
fn header_click(screen: &mut Screen, mouse: &ratatui::crossterm::event::MouseEvent) -> bool {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    if !screen.pager_active || !screen.mouse_capture {
        return false;
    }
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    screen.toggle_header_at_screen_row(mouse.row)
}

/// Pager-mode hyperlink click: a left-button-down over a rendered OSC 8 link
/// region opens the target. Web URLs launch the browser via the existing
/// `open_in_browser` seam; workspace `file:line` references surface a notice
/// (opening an editor is out of scope for this slice). `false` = not a link
/// click (fall through to header/wheel handling). Only fires under pager mouse
/// capture.
fn pager_link_click(screen: &mut Screen, mouse: &ratatui::crossterm::event::MouseEvent) -> bool {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    if !screen.pager_active || !screen.mouse_capture {
        return false;
    }
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let Some(uri) = screen
        .pager_link_at(mouse.row, mouse.column)
        .map(str::to_owned)
    else {
        return false;
    };
    if crate::ui::hyperlink::is_web_url(&uri) {
        crate::ui::login::open_in_browser(&uri);
    } else {
        screen.apply(crate::ui::UiEvent::Notice(format!("link: {uri}")));
    }
    true
}

/// Pager-mode wheel scrolling: ±`scroll_speed` lines per wheel tick. Only the
/// wheel is consumed; clicks/drags are ignored (in-app selection is a later
/// slice -- the Ctrl+T toggle restores terminal-native selection until then).
fn pager_wheel(screen: &mut Screen, mouse: &ratatui::crossterm::event::MouseEvent) -> bool {
    // Gate on capture INTENT too: after Ctrl+T / `/mouse` turns capture off,
    // queued events (or events still arriving because the disable write
    // failed) must not scroll a transcript whose UI says native selection is
    // active.
    if !screen.pager_active || !screen.mouse_capture {
        return false;
    }
    let step = usize::from(screen.scroll_speed.max(1));
    match mouse.kind {
        ratatui::crossterm::event::MouseEventKind::ScrollUp => {
            screen.scroll.scroll_up(step);
            true
        }
        ratatui::crossterm::event::MouseEventKind::ScrollDown => {
            screen.scroll.scroll_down(step);
            true
        }
        _ => false,
    }
}

/// Pager-mode scrollback navigation (ADR-0029). Consumes the key when it
/// scrolled: PageUp/PageDown page, Alt+Up/Alt+Down scroll one line (Ctrl+J/K
/// stay editor kill-ring keys), and Home/End jump to the ends -- but only
/// while the composer is empty, so editing keeps its line-start/end keys.
/// Inline mode (`pager_active == false`) never consumes anything.
fn pager_scroll_key(screen: &mut Screen, code: KeyCode, ctrl: bool, alt: bool) -> bool {
    if !screen.pager_active || ctrl {
        return false;
    }
    match code {
        KeyCode::PageUp => screen.scroll.page_up(),
        KeyCode::PageDown => screen.scroll.page_down(),
        KeyCode::Up if alt => screen.scroll.scroll_up(1),
        KeyCode::Down if alt => screen.scroll.scroll_down(1),
        KeyCode::Home if !alt && screen.editor_is_empty() => screen.scroll.jump_to_start(),
        KeyCode::End if !alt && screen.editor_is_empty() => screen.scroll.follow_latest(),
        _ => return false,
    }
    true
}

fn handle_running_event(
    screen: &mut Screen,
    event: Event,
    pending: &mut Option<PendingApproval>,
    steering: &SteeringQueue,
    git_cache: &GitStatusCache,
    token: &CancellationToken,
) -> bool {
    match event {
        // Paste composes into the live editor (but not while an approval is
        // pending, when the composer is frozen).
        Event::Paste(text) if pending.is_none() => {
            insert_paste(screen, &text);
            true
        }
        // Pager mode: wheel scrolls the Iris-owned scrollback while a turn
        // runs; clicks still target the session bar (dropdowns open as
        // readouts -- click activation is a no-op under readonly).
        Event::Mouse(mouse) => {
            if pending.is_none()
                && let Some(key) = session_bar_click(screen, &mouse, git_cache)
            {
                return !matches!(key, IdleKey::Ignore | IdleKey::Menu(_));
            }
            if sticky_prompt_click(screen, &mouse) {
                return true;
            }
            if header_click(screen, &mouse) {
                return true;
            }
            if pager_link_click(screen, &mouse) {
                return true;
            }
            pager_wheel(screen, &mouse)
        }
        // Resize still triggers a redraw of the terminal surface.
        Event::Resize(..) => true,
        // Focus reports pause/resume the spinner's tick redraws; a regain
        // redraws once to catch the frozen animation and elapsed time up.
        Event::FocusGained => screen.set_terminal_focused(true),
        Event::FocusLost => {
            screen.set_terminal_focused(false);
            false
        }
        Event::Key(key) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            if ctrl && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O')) {
                // ctrl+o always toggles transcript folds; the pinned prompt band
                // toggles via click or the `o` key, never by pre-empting ctrl+o.
                screen.toggle_all_panels();
                return true;
            }
            if ctrl
                && screen.pager_active
                && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
            {
                screen.toggle_mouse();
                return true;
            }
            if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
                // The input thread already cancelled the token. Aborting also
                // discards anything the user queued, and unblocks a pending
                // approval as Deny so Nexus observes the cancellation and aborts.
                steering.clear();
                if let Some(p) = pending.take() {
                    let _ = p.reply.send(ApprovalDecision::Deny);
                    screen.clear_approval(false);
                }
                return true;
            }
            // SessionBar dropdowns while a turn runs: readouts. `ctrl-g`
            // toggles, navigation + esc stay live, every mutating key is a
            // no-op inside the menu. Never while an approval is pending (the
            // approval owns input then).
            if pending.is_none() {
                if ctrl && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G')) {
                    toggle_git_menu(screen, git_cache);
                    return true;
                }
                if screen.session_menu.is_some() {
                    let Some(menu_key) = to_menu_key(key.code, ctrl) else {
                        return false;
                    };
                    let outcome = screen
                        .session_menu
                        .as_mut()
                        .map(|menu| menu.handle_key(menu_key, true))
                        .unwrap_or(MenuOutcome::Ignore);
                    return !matches!(
                        menu_outcome_key(screen, outcome),
                        IdleKey::Ignore | IdleKey::Menu(_)
                    );
                }
            }
            // Pager scroll keys stay live while a turn runs -- including while
            // an approval is pending (scrolling history is exactly what a
            // reviewer needs before deciding); they never collide with the
            // y/a/p/n approval keys and never edit or steer.
            if pager_scroll_key(screen, key.code, ctrl, alt) {
                return true;
            }
            // While a tool is awaiting approval, the composer is frozen: only the
            // approval keys act, and any other key is ignored (never typed).
            if let Some(p) = pending.as_ref() {
                let decision = match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Allow),
                    KeyCode::Char('a') | KeyCode::Char('A') if p.allow_always => {
                        Some(ApprovalDecision::AllowAlways)
                    }
                    KeyCode::Char('p') | KeyCode::Char('P') if p.allow_project => {
                        Some(ApprovalDecision::AllowProject)
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                        Some(ApprovalDecision::Deny)
                    }
                    _ => None,
                };
                if let Some(decision) = decision {
                    let p = pending.take().expect("pending approval present");
                    // The decision folds into the gated tool block's own footer
                    // (approve → muted note; deny → the block flips to DENIED via
                    // the ToolDenied event). No separate approval panel.
                    screen.note_approval(&p.call, decision);
                    let _ = p.reply.send(decision);
                    let approved = matches!(
                        decision,
                        ApprovalDecision::Allow
                            | ApprovalDecision::AllowAlways
                            | ApprovalDecision::AllowProject
                    );
                    screen.clear_approval(approved);
                    return true;
                }
                return false;
            }
            // Tab focus + entry selection stay available while a turn runs
            // (after the approval block, so approval keys always win).
            if scrollback_focus_key(screen, key.code, ctrl, alt) {
                return true;
            }
            if prompt_history_key(screen, key.code, ctrl, alt) {
                return true;
            }
            // No approval pending: the composer is live for steering. Enter
            // queues a steering message (injected before the next provider
            // request), Alt+Enter a follow-up (injected when the agent would
            // otherwise stop); everything else edits the composer.
            match key.code {
                KeyCode::Enter if alt => {
                    let text = screen.submit();
                    if !text.trim().is_empty() {
                        steering.enqueue_follow_up(text);
                    }
                    true
                }
                KeyCode::Enter if shift || ctrl => {
                    screen.reset_prompt_history_cursor();
                    screen.editor.insert_newline();
                    true
                }
                KeyCode::Enter => {
                    let text = screen.submit();
                    if let Some(notice) = apply_focus_command(screen, &text) {
                        screen.apply(UiEvent::Notice(notice));
                    } else if is_settings_command(text.trim()) {
                        // `/settings` is a UI command, not model input: defer
                        // opening the settings picker to the next safe boundary
                        // instead of steering the literal text into the turn
                        // (issue #489). The loop drains this after the turn ends.
                        steering.request_settings();
                        screen.apply(UiEvent::Notice(
                            "settings will open when this turn ends".to_string(),
                        ));
                    } else if !text.trim().is_empty() {
                        steering.enqueue_steering(text);
                    }
                    true
                }
                // Esc with nothing higher-priority to consume it (no pending
                // approval, no open dropdown, not a pager/scrollback key)
                // cancels the running turn (issue #511). Mirrors Ctrl-C's
                // abort: drop any queued steering and cancel the token so
                // Nexus aborts the turn. Cancel is idempotent with the input
                // thread's own Ctrl-C cancel.
                KeyCode::Esc => {
                    steering.clear();
                    token.cancel();
                    true
                }
                code => apply_editor_key(screen, code, ctrl, alt),
            }
        }
        _ => false,
    }
}

/// Collects harness-owned events emitted during an out-of-turn settings apply
/// (e.g. the `Cancelled` compaction lifecycle when automatic compaction is
/// disabled mid-job). The modal phase has no [`LoopBridge`], so the caller
/// drains the collected events onto the screen once the action returns — the
/// harness stays the event owner and the UI never fabricates the event inline.
#[derive(Default)]
struct SettingsEventSink {
    events: std::cell::RefCell<Vec<UiEvent>>,
}

impl SettingsEventSink {
    fn drain(&self) -> Vec<UiEvent> {
        std::mem::take(&mut self.events.borrow_mut())
    }
}

impl AgentObserver for SettingsEventSink {
    fn on_event(&self, event: crate::nexus::AgentEvent) -> Result<()> {
        self.events
            .borrow_mut()
            .push(UiEvent::from_agent_event(event));
        Ok(())
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
    fn review<'a>(
        &'a self,
        call: &'a ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        let appr_tx = self.appr_tx.clone();
        let call = call.clone();
        let dirty_gate = !ctx.dirty_paths.is_empty();
        // Render the in-block `▲ REVIEW` state (with its footer affordance)
        // before blocking on the decision, so the gated tool block is visible
        // while the composer is frozen. Sent on `event_tx` — FIFO after any
        // `DiffPreview` for the same call — so the review adopts the edit
        // preview instead of racing it.
        let _ = self.event_tx.send(UiEvent::ToolReview {
            call: call.clone(),
            allow_always,
            allow_project,
            dirty_gate,
            reason: review_reason(&call, &ctx),
        });
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            if appr_tx
                .send(ApprovalRequest {
                    call,
                    allow_always,
                    allow_project,
                    dirty_gate,
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
    use crate::nexus::SteeringSource;
    use crate::ui::tui::Screen;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    /// Test shims: the production handlers take the async git-status cache;
    /// these keep the existing key-routing tests cache-free (an explicit item
    /// shadows the glob import).
    fn handle_idle_event(screen: &mut Screen, event: Event) -> IdleKey {
        super::handle_idle_event(screen, event, &GitStatusCache::default())
    }

    fn handle_running_event(
        screen: &mut Screen,
        event: Event,
        pending: &mut Option<PendingApproval>,
        steering: &SteeringQueue,
    ) -> bool {
        super::handle_running_event(
            screen,
            event,
            pending,
            steering,
            &GitStatusCache::default(),
            &CancellationToken::new(),
        )
    }

    #[test]
    fn model_switch_notices_drop_routine_confirmation_but_keep_failures() {
        let lines = vec![
            "switched to openai-codex/gpt-5.5 (reasoning: high)".to_string(),
            "carrying ~42000 tokens of context to gpt-5.5; its prompt cache starts cold, so the next request re-reads all of it -- /compact first to hand over a short summary instead.".to_string(),
            "(default not saved: config is read-only)".to_string(),
        ];

        assert_eq!(
            switch_notice_lines(lines),
            vec!["(default not saved: config is read-only)".to_string()]
        );
    }

    #[test]
    fn persisted_tui_settings_apply_to_the_live_screen() {
        let mut screen = Screen::new();
        screen.show_start_page(0, true);
        if let Some(page) = screen.start_page.as_mut() {
            page.advance_for_test();
            page.advance_for_test();
            assert_eq!(page.head(), 2);
        }

        apply_live_tui_setting(
            &mut screen,
            settings_menu::Field::ReducedMotion,
            Some("true"),
        );
        assert!(
            screen.start_page.as_mut().is_some_and(|page| !page.tick()),
            "reduced motion applies without restart"
        );

        apply_live_tui_setting(&mut screen, settings_menu::Field::ScrollSpeed, Some("500"));
        assert_eq!(screen.scroll_speed, 100, "live value uses persisted clamp");
    }

    #[test]
    fn screen_accumulates_fold_and_compaction_accounting_from_events() {
        // The /context breakdown's session-scoped totals come straight from
        // the display-event stream (issue #400, design §5.1).
        let mut screen = Screen::new();
        screen.apply(crate::ui::UiEvent::FoldApplied {
            folds: 2,
            semantic_dedupe_folds: 2,
            tool_clearing_folds: 0,
            reclaimed_tokens_estimate: 900,
            trigger: crate::nexus::FoldTrigger::SelectionSwitch,
        });
        screen.apply(crate::ui::UiEvent::FoldApplied {
            folds: 1,
            semantic_dedupe_folds: 0,
            tool_clearing_folds: 1,
            reclaimed_tokens_estimate: 300,
            trigger: crate::nexus::FoldTrigger::Watermark,
        });
        screen.apply(crate::ui::UiEvent::CompactionApplied {
            compaction_id: "c1".into(),
            covered_from: "1".into(),
            covered_to: "5".into(),
            covered_messages: 5,
            original_tokens_estimate: 4000,
            summary_tokens_estimate: 400,
            budget: 8000,
        });
        let accounting = &screen.context_accounting;
        assert_eq!(accounting.fold_batches, vec![("A2", 2, 900), ("C", 1, 300)]);
        assert_eq!(accounting.folded_reclaimed(), 1200);
        assert_eq!(accounting.compactions, vec![(4000, 400)]);
    }

    #[test]
    fn context_breakdown_reports_categories_and_trigger_tags() {
        // A scripted context: a compaction summary stand-in, a folded stub, a
        // superseded read the scheduler holds as pending, and a normal tail.
        // The breakdown must attribute each category with accurate estimates
        // and show trigger-tagged fold lines. In-memory harness: pending
        // folds require a durable session, so pending is 0 here; the pending
        // row is covered by the wayland pending_fold_stats tests.
        use crate::nexus::{Agent, Message};
        use crate::session::estimate_tokens;
        let summary = "[compacted summary of 5 earlier message(s)]\nGoal: ship the fold work.";
        let stub = "[folded] The `read` result for `a.rs` was superseded and folded.";
        let messages = vec![
            Message::user(summary),
            Message::tool_result("c1", "read", stub),
            Message::user("continue"),
            Message::assistant("ok"),
        ];
        let agent = Agent::resumed(NullChat, crate::tools::built_in_tools(), messages.clone());
        let harness = crate::wayland::Harness::new(
            agent,
            std::env::temp_dir(),
            crate::tools::ToolState::new(),
            None,
            Some(10_000),
        );
        let mut accounting = crate::ui::tui::ContextAccounting::default();
        accounting.fold_batches.push(("A4", 1, 700));
        accounting.compactions.push((4000, 400));

        let lines = context_breakdown_lines(&harness, None, &accounting).join("\n");
        let total = harness.context_token_estimate();
        let summarized = estimate_tokens(summary);
        assert!(
            lines.contains(&format!("~{total} of 10000 tokens")),
            "{lines}"
        );
        assert!(
            lines.contains(&format!("~{} tokens", 10_000 - total)),
            "headroom: {lines}"
        );
        assert!(
            lines.contains(&format!(
                "raw conversation   ~{} tokens",
                total - summarized
            )),
            "{lines}"
        );
        assert!(
            lines.contains(&format!("summarized         ~{summarized} tokens")),
            "{lines}"
        );
        assert!(
            lines.contains("(1 compaction(s) this session: ~4000 -> ~400)"),
            "{lines}"
        );
        assert!(
            lines.contains("folded-reclaimed   1 stub(s) in context; ~700 tokens reclaimed"),
            "{lines}"
        );
        assert!(
            lines.contains("reclaimed ~700 tokens [A4]"),
            "fold line carries its trigger tag: {lines}"
        );
    }

    /// Provider stub for breakdown tests: never called (display-only path).
    struct NullChat;
    impl crate::nexus::ChatProvider for NullChat {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [crate::nexus::Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<crate::nexus::ProviderStream<'a>> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn skip_permissions_overrides_statusline_approval_policy() {
        use crate::nexus::{Agent, ApprovalMode};
        let mut agent =
            Agent::new(NullChat, crate::tools::built_in_tools()).with_skip_permissions(true);
        agent.set_approval_mode(ApprovalMode::Strict);
        let harness = crate::wayland::Harness::new(
            agent,
            std::env::temp_dir(),
            crate::tools::ToolState::new(),
            None,
            None,
        );

        assert_eq!(
            effective_approval_policy(&harness),
            ApprovalPolicy::SkipPermissions
        );
    }

    #[test]
    fn debug_snapshot_contents_carry_size_rendered_lines_and_messages() {
        let rendered = vec!["[0] (w=2) \"hi\"".to_string(), "[1] (w=0) \"\"".to_string()];
        let messages = vec![
            crate::nexus::Message::user("question"),
            crate::nexus::Message::assistant("answer"),
        ];
        let frame_stats = vec![
            "Frames sampled: 3 (ring holds last 512)".to_string(),
            "  total   p50=1.000ms p99=2.000ms max=2.000ms".to_string(),
        ];
        let contents = debug_snapshot_contents(80, 24, &rendered, &frame_stats, &messages);
        assert!(contents.contains("Iris "), "{contents}");
        assert!(contents.contains("Terminal: 80x24"), "{contents}");
        assert!(contents.contains("Total lines: 2"), "{contents}");
        assert!(
            contents.contains("=== Frame timing (compose vs flush) ==="),
            "{contents}"
        );
        assert!(contents.contains("Frames sampled: 3"), "{contents}");
        assert!(contents.contains("[0] (w=2) \"hi\""), "{contents}");
        assert!(
            contents.contains("=== Context messages (JSONL) ==="),
            "{contents}"
        );
        assert!(
            contents.contains(r#"{"content":"question","role":"user"}"#),
            "{contents}"
        );
        assert!(
            contents.contains(r#"{"content":"answer","role":"assistant"}"#),
            "{contents}"
        );
    }

    #[test]
    fn debug_snapshot_notes_when_no_frames_have_been_drawn() {
        let contents = debug_snapshot_contents(80, 24, &[], &[], &[]);
        assert!(
            contents.contains("=== Frame timing (compose vs flush) ==="),
            "{contents}"
        );
        assert!(contents.contains("(no frames drawn yet)"), "{contents}");
    }

    #[test]
    fn scheduler_first_request_after_idle_draws_immediately() {
        let mut sched = RenderScheduler::new();
        let t0 = Instant::now();
        // Nothing pending -> idle, no timer wakeups.
        assert_eq!(sched.poll(t0), RenderAction::Idle);
        // First request with no prior draw -> draw immediately.
        sched.request();
        assert_eq!(sched.poll(t0), RenderAction::DrawNow);
    }

    #[test]
    fn scheduler_coalesces_burst_within_interval_then_flushes() {
        let mut sched = RenderScheduler::new();
        let t0 = Instant::now();
        sched.mark_drawn(t0);

        // A request 5ms into the window is not yet due: defer to the boundary.
        sched.request();
        let t_burst = t0 + Duration::from_millis(5);
        match sched.poll(t_burst) {
            RenderAction::Wait(at) => assert_eq!(at, t0 + MIN_RENDER_INTERVAL),
            other => panic!("expected Wait, got {other:?}"),
        }
        // More requests in the same window stay coalesced (still one pending).
        sched.request();
        assert!(matches!(sched.poll(t_burst), RenderAction::Wait(_)));

        // At/after the interval boundary the coalesced render is due.
        assert_eq!(sched.poll(t0 + MIN_RENDER_INTERVAL), RenderAction::DrawNow);
    }

    #[test]
    fn scheduler_idle_after_draw_until_next_request() {
        let mut sched = RenderScheduler::new();
        let t0 = Instant::now();
        sched.request();
        assert_eq!(sched.poll(t0), RenderAction::DrawNow);
        sched.mark_drawn(t0);
        // No new request -> idle even long after the interval (no busy wakeups).
        assert_eq!(sched.poll(t0 + Duration::from_secs(1)), RenderAction::Idle);
    }

    #[test]
    fn scheduler_hold_defers_draws_until_it_expires() {
        let mut sched = RenderScheduler::new();
        let t0 = Instant::now();
        let hold = t0 + RESIZE_REDRAW_DEBOUNCE;
        sched.hold_until(hold);
        // Even a first-after-idle request (normally immediate) waits the hold out.
        sched.request();
        assert_eq!(sched.poll(t0), RenderAction::Wait(hold));
        // Past the hold, the pending draw flushes.
        assert_eq!(sched.poll(hold), RenderAction::DrawNow);
        sched.mark_drawn(hold);
        // The satisfied hold is cleared: the next request paces normally.
        sched.request();
        assert_eq!(
            sched.poll(hold + MIN_RENDER_INTERVAL),
            RenderAction::DrawNow
        );
    }

    #[test]
    fn scheduler_hold_extends_on_repeat_never_shortens() {
        let mut sched = RenderScheduler::new();
        let t0 = Instant::now();
        let first = t0 + RESIZE_REDRAW_DEBOUNCE;
        let later = first + RESIZE_REDRAW_DEBOUNCE;
        sched.request();
        // A resize storm keeps pushing the trailing edge out...
        sched.hold_until(first);
        sched.hold_until(later);
        // ...and an out-of-order earlier deadline never pulls it back in.
        sched.hold_until(first);
        assert_eq!(sched.poll(first), RenderAction::Wait(later));
        assert_eq!(sched.poll(later), RenderAction::DrawNow);
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// A screen with a git snapshot on the footer (branch `main`).
    fn git_screen() -> Screen {
        let mut screen = Screen::new();
        screen.set_footer_with_context("gpt-5.5".to_string(), None, None, "~/repo".to_string());
        screen.set_footer_git(Some(crate::git::status::GitStatus {
            branch: Some("main".to_string()),
            recent_branches: vec![crate::git::status::BranchInfo {
                name: "main".to_string(),
                age: None,
                worktree: None,
            }],
            ..Default::default()
        }));
        screen
    }

    #[test]
    fn ctrl_g_routes_to_the_git_menu_toggle_and_toggle_opens_and_closes() {
        let mut screen = git_screen();
        let outcome = handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL),
        );
        assert!(matches!(outcome, IdleKey::ToggleGitMenu));

        let cache = GitStatusCache::default();
        assert!(toggle_git_menu(&mut screen, &cache));
        assert!(matches!(screen.session_menu, Some(SessionMenu::Git(_))));
        // Toggle again closes.
        assert!(toggle_git_menu(&mut screen, &cache));
        assert!(screen.session_menu.is_none());

        // Without a git snapshot the toggle degrades to an honest notice.
        let mut plain = Screen::new();
        plain.set_footer_with_context("m".to_string(), None, None, "~/x".to_string());
        assert!(toggle_git_menu(&mut plain, &cache));
        assert!(plain.session_menu.is_none());
    }

    #[test]
    fn vcs_toggle_opens_readonly_jj_menu_for_jj_status() {
        let mut screen = Screen::new();
        screen.set_footer_with_context("gpt-5.5".to_string(), None, None, "~/repo".to_string());
        screen.set_footer_vcs(Some(crate::git::status::VcsStatus::Jj(
            crate::git::status::JjStatus {
                change_id: "abcdefgh".to_string(),
                description: "draft status work".to_string(),
                total_changed: 1,
                ..Default::default()
            },
        )));
        let cache = GitStatusCache::default();
        assert!(toggle_git_menu(&mut screen, &cache));
        assert!(matches!(screen.session_menu, Some(SessionMenu::Jj(_))));
        assert!(toggle_git_menu(&mut screen, &cache));
        assert!(screen.session_menu.is_none());
    }

    #[test]
    fn up_down_recall_submitted_prompt_history() {
        let mut screen = git_screen();
        screen.editor.insert_str("first");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::Submit(_)
        ));
        screen.editor.insert_str("second");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::Submit(_)
        ));

        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Up)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "second");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Up)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "first");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Down)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "second");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Down)),
            IdleKey::Continue
        ));
        assert!(screen.editor_is_empty());
    }

    #[test]
    fn typing_after_recall_exits_prompt_history_browsing() {
        let mut screen = git_screen();
        screen.editor.insert_str("first");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::Submit(_)
        ));

        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Up)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "first");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Char('!'))),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "first!");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Down)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "first!");
    }

    #[test]
    fn up_in_nonempty_fresh_editor_keeps_normal_cursor_motion() {
        let mut screen = git_screen();
        screen.editor.insert_str("first");
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::Submit(_)
        ));
        screen.editor.insert_str("draft");

        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Up)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "draft");
    }

    #[test]
    fn at_as_first_character_of_an_empty_composer_opens_the_tree_filter() {
        let mut screen = git_screen();
        let outcome = handle_idle_event(&mut screen, key(KeyCode::Char('@')));
        assert!(matches!(outcome, IdleKey::ToggleTreeMenu(true)));
        assert!(screen.editor_is_empty(), "the @ is not typed");

        // Mid-text `@` is plain typing (the file-reference idiom applies only
        // to the first character of an empty composer).
        screen.editor.insert_str("see ");
        let outcome = handle_idle_event(&mut screen, key(KeyCode::Char('@')));
        assert!(matches!(outcome, IdleKey::Continue));
        assert_eq!(screen.editor_text(), "see @");
    }

    #[test]
    fn open_dropdown_owns_keys_and_esc_closes_without_reaching_other_paths() {
        let mut screen = git_screen();
        let cache = GitStatusCache::default();
        toggle_git_menu(&mut screen, &cache);
        assert_eq!(screen.focus(), FocusTarget::SessionMenu);

        // Typing a printable in list state is inert (no free typing) and never
        // lands in the composer.
        let outcome = handle_idle_event(&mut screen, key(KeyCode::Char('x')));
        assert!(matches!(outcome, IdleKey::Ignore));
        assert!(screen.editor_is_empty());

        // Esc closes the dropdown (and is consumed here).
        let outcome = handle_idle_event(&mut screen, key(KeyCode::Esc));
        assert!(matches!(outcome, IdleKey::Continue));
        assert!(screen.session_menu.is_none());
    }

    #[test]
    fn running_turn_makes_the_dropdown_a_readout() {
        let mut screen = git_screen();
        screen.start_turn();
        let steering = SteeringQueue::default();
        let mut pending = None;

        // ctrl-g still opens (as a readout).
        assert!(handle_running_event(
            &mut screen,
            key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &mut pending,
            &steering,
        ));
        assert!(screen.session_menu.is_some());
        assert!(screen.menu_readonly());

        // Enter (a mutating key) is a no-op readout-side.
        assert!(!handle_running_event(
            &mut screen,
            key(KeyCode::Enter),
            &mut pending,
            &steering,
        ));
        assert!(screen.session_menu.is_some());

        // An open dropdown consumes Esc: it closes the readout and never falls
        // through to the turn-cancel path (issue #511 preserves menu Esc).
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Esc),
            &mut pending,
            &steering,
        ));
        assert!(screen.session_menu.is_none());
    }

    #[test]
    fn pager_wheel_scrolls_by_scroll_speed_and_gates_on_pager_mode() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        fn wheel(kind: MouseEventKind) -> MouseEvent {
            MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }
        }
        let mut screen = Screen::new();
        // Inline mode never consumes mouse events.
        assert!(!pager_wheel(&mut screen, &wheel(MouseEventKind::ScrollUp)));

        screen.pager_active = true;
        screen.scroll.sync(100, 20);
        assert!(pager_wheel(&mut screen, &wheel(MouseEventKind::ScrollUp)));
        assert!(!screen.scroll.is_following());
        // Default step is 3 lines per tick (max_top 80 -> 77).
        screen.scroll_speed = 5;
        assert!(pager_wheel(&mut screen, &wheel(MouseEventKind::ScrollUp)));
        // Clicks/drags are ignored until in-app selection lands.
        assert!(!pager_wheel(
            &mut screen,
            &wheel(MouseEventKind::Down(MouseButton::Left))
        ));
        // Wheel-down past the bottom re-engages follow.
        for _ in 0..100 {
            let _ = pager_wheel(&mut screen, &wheel(MouseEventKind::ScrollDown));
        }
        assert!(screen.scroll.is_following());
        // Capture toggled off: queued/late wheel events are ignored.
        screen.mouse_capture = false;
        assert!(!pager_wheel(&mut screen, &wheel(MouseEventKind::ScrollUp)));
        assert!(screen.scroll.is_following());
    }

    #[test]
    fn ctrl_t_toggles_mouse_capture_only_in_pager_mode() {
        let mut screen = Screen::new();
        assert!(screen.mouse_capture);
        // Inline mode: Ctrl+T falls through (not consumed as a toggle).
        let outcome = handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(matches!(outcome, IdleKey::Ignore | IdleKey::Continue));
        assert!(screen.mouse_capture, "inline mode never toggles capture");

        screen.pager_active = true;
        let outcome = handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL),
        );
        assert!(matches!(outcome, IdleKey::Continue));
        assert!(!screen.mouse_capture);
    }

    #[test]
    fn tab_toggles_scrollback_focus_and_typing_returns_to_prompt() {
        let mut screen = Screen::new();
        // Inline mode: Tab is not a focus key (editor keeps it).
        assert!(!scrollback_focus_key(
            &mut screen,
            KeyCode::Tab,
            false,
            false
        ));

        screen.pager_active = true;
        assert!(scrollback_focus_key(
            &mut screen,
            KeyCode::Tab,
            false,
            false
        ));
        assert!(screen.scrollback_focus);
        // Arrows are consumed as selection/scroll while focused.
        assert!(scrollback_focus_key(&mut screen, KeyCode::Up, false, false));
        // Esc is never a focus/nav key: not consumed, focus unchanged.
        assert!(!scrollback_focus_key(
            &mut screen,
            KeyCode::Esc,
            false,
            false
        ));
        assert!(screen.scrollback_focus);
        // A printable character returns focus to the prompt WITHOUT being
        // consumed, so it still types into the composer.
        assert!(!scrollback_focus_key(
            &mut screen,
            KeyCode::Char('h'),
            false,
            false
        ));
        assert!(!screen.scrollback_focus);
        // Tab toggles back out too.
        assert!(scrollback_focus_key(
            &mut screen,
            KeyCode::Tab,
            false,
            false
        ));
        assert!(scrollback_focus_key(
            &mut screen,
            KeyCode::Tab,
            false,
            false
        ));
        assert!(!screen.scrollback_focus);
    }

    #[test]
    fn pager_scroll_keys_gate_on_pager_mode_and_composer_state() {
        let mut screen = Screen::new();
        // Inline mode: nothing is consumed, editor keeps every key.
        assert!(!pager_scroll_key(
            &mut screen,
            KeyCode::PageUp,
            false,
            false
        ));
        assert!(!pager_scroll_key(&mut screen, KeyCode::Home, false, false));

        screen.pager_active = true;
        screen.scroll.sync(100, 20);
        // PageUp scrolls and disengages follow.
        assert!(pager_scroll_key(&mut screen, KeyCode::PageUp, false, false));
        assert!(!screen.scroll.is_following());
        // Alt+Down line-scrolls; plain Down stays with the editor.
        assert!(pager_scroll_key(&mut screen, KeyCode::Down, false, true));
        assert!(!pager_scroll_key(&mut screen, KeyCode::Down, false, false));
        // Ctrl chords never scroll (Ctrl+J/K stay editor kill-ring keys).
        assert!(!pager_scroll_key(&mut screen, KeyCode::PageUp, true, false));
        // Home/End scroll only while the composer is empty.
        assert!(pager_scroll_key(&mut screen, KeyCode::Home, false, false));
        screen.set_editor("draft");
        assert!(!pager_scroll_key(&mut screen, KeyCode::Home, false, false));
        assert!(!pager_scroll_key(&mut screen, KeyCode::End, false, false));
        screen.clear_editor();
        assert!(pager_scroll_key(&mut screen, KeyCode::End, false, false));
        assert!(screen.scroll.is_following());
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": "echo hi" }),
        }
    }

    #[test]
    fn start_page_launcher_navigates_wraps_and_activates() {
        let mut screen = Screen::new();
        screen.show_start_page(0, true);

        // ↓ moves to Resume session; ↵ activates it (opens the resume picker).
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Down)),
            IdleKey::Continue
        ));
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::OpenResumePicker
        ));

        // ↑ wraps from the top row to Quit; ↵ exits.
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Up)),
            IdleKey::Continue
        ));
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Up)),
            IdleKey::Continue
        ));
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::Exit
        ));
    }

    #[test]
    fn start_page_tasks_entry_opens_the_task_surface() {
        let mut screen = Screen::new();
        screen.show_start_page(0, true);
        // New session → Resume session → Tasks, then ↵ opens the task surface —
        // the surface is a home entry now, not a picker forced open on launch.
        handle_idle_event(&mut screen, key(KeyCode::Down));
        handle_idle_event(&mut screen, key(KeyCode::Down));
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Enter)),
            IdleKey::OpenTasks
        ));
        // ctrl-t is the direct chord for the same entry.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL)
            ),
            IdleKey::OpenTasks
        ));
    }

    #[test]
    fn start_page_ctrl_chords_activate_directly() {
        let mut screen = Screen::new();
        screen.show_start_page(0, true);
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)
            ),
            IdleKey::OpenResumePicker
        ));
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char(','), KeyModifiers::CONTROL)
            ),
            IdleKey::OpenSettings
        ));
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            IdleKey::Exit
        ));
        // ctrl-n enters the (already fresh) session: launcher dismissed.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
        assert!(!screen.start_page_active());
        // Off the start page, ctrl-r is the editor's redo again.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
    }

    #[test]
    fn start_page_ctrl_r_stays_redo_once_the_composer_has_text() {
        let mut screen = Screen::new();
        screen.show_start_page(0, true);
        handle_idle_event(&mut screen, key(KeyCode::Char('x')));
        // A non-empty composer keeps ctrl-r as the editor's redo binding.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
        assert!(screen.start_page_active(), "launcher stays visible");
    }

    #[test]
    fn start_page_composer_stays_live_and_submit_starts_the_session() {
        let mut screen = Screen::new();
        screen.show_start_page(0, true);
        assert!(
            screen
                .start_page
                .as_ref()
                .is_some_and(|page| page.booting())
        );
        // Typing goes to the composer and settles the lamp test; the triggering
        // key is not consumed.
        for c in "fix the bug".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        assert!(
            screen
                .start_page
                .as_ref()
                .is_some_and(|page| !page.booting()),
            "the first key settles startup"
        );
        assert_eq!(screen.editor_text(), "fix the bug");
        // With a non-empty composer, ↑/↓/↵ belong to the editor/submit path.
        match handle_idle_event(&mut screen, key(KeyCode::Enter)) {
            IdleKey::Submit(text) => assert_eq!(text, "fix the bug"),
            _ => panic!("expected submit"),
        }
        // Entering the session (turn start) replaces the launcher.
        screen.start_turn();
        assert!(!screen.start_page_active());
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
    fn modified_enter_inserts_newline_without_submitting() {
        let mut screen = Screen::new();
        handle_idle_event(&mut screen, key(KeyCode::Char('a')));
        handle_idle_event(&mut screen, key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        handle_idle_event(&mut screen, key(KeyCode::Char('b')));
        handle_idle_event(&mut screen, key_mod(KeyCode::Enter, KeyModifiers::CONTROL));
        handle_idle_event(&mut screen, key(KeyCode::Char('c')));
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('j'), KeyModifiers::CONTROL),
        );
        handle_idle_event(&mut screen, key(KeyCode::Char('d')));
        handle_idle_event(&mut screen, key(KeyCode::Char('\n')));
        handle_idle_event(&mut screen, key(KeyCode::Char('e')));
        assert_eq!(screen.editor_text(), "a\nb\nc\nd\ne");
    }

    #[test]
    fn alt_enter_submits_like_pi_when_idle() {
        let mut screen = Screen::new();
        for c in "hello".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        match handle_idle_event(&mut screen, key_mod(KeyCode::Enter, KeyModifiers::ALT)) {
            IdleKey::Submit(text) => assert_eq!(text, "hello"),
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn ctrl_o_routes_to_toggle_all_when_idle() {
        // ctrl+o drives toggle-all; the full direction/multi-block behavior is
        // covered in ui::tui::tests::ctrl_o_toggle_all_expands_then_collapses.
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call(),
            content: (0..20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            exit_code: None,
            duration: None,
        });
        // Compact by default: the finalized block arrives collapsed.
        assert!(screen.latest_panel_collapsed());
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
        // ctrl+o expanded it.
        assert!(!screen.latest_panel_collapsed());
    }

    #[test]
    fn ctrl_o_toggles_folds_not_the_band_when_a_prompt_is_pinned() {
        // The re-route regression (spec §5.3): with a governing prompt pinned as
        // the sticky band AND a collapsed block below it, ctrl+o expands the
        // block (its one meaning everywhere) and leaves the band alone. The old
        // pre-emption that trapped a reader's folds behind the band is gone.
        let mut screen = Screen::new();
        screen.pager_active = true;
        // A collapsed tool block.
        screen.apply(UiEvent::ToolResult {
            call: call(),
            content: (0..20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            exit_code: None,
            duration: None,
        });
        assert!(screen.latest_panel_collapsed());
        // A governing prompt scrolled above a small viewport.
        screen.commit_user("the governing prompt");
        for i in 0..40 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let total = screen.transcript_visible_total(80);
        screen.scroll.sync(total, 5);
        // Precondition: a band IS a viable toggle target (probe, then restore to
        // collapsed) -- so the old code path really would have diverged here.
        assert!(screen.toggle_sticky_prompt(), "a sticky prompt is pinned");
        assert!(screen.toggle_sticky_prompt());
        assert!(!screen.sticky_prompt_expanded);

        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
        assert!(
            !screen.latest_panel_collapsed(),
            "ctrl+o expanded the collapsed block"
        );
        assert!(
            !screen.sticky_prompt_expanded,
            "ctrl+o did not touch the pinned band"
        );
    }

    #[test]
    fn o_key_toggles_the_pinned_band_only_under_scrollback_focus() {
        // The band's keyboard toggle (spec §5.2/§5.4): `o` expands/collapses the
        // pinned prompt, but only while the scrollback list holds focus (the
        // list-state law). Without that focus it types like any other letter, so
        // it never collides with composing a message that starts with `o`.
        let mut screen = Screen::new();
        screen.pager_active = true;
        screen.commit_user("the governing prompt");
        for i in 0..40 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let total = screen.transcript_visible_total(80);
        screen.scroll.sync(total, 5);

        // Composer focus: `o` is not consumed (types) and the band is untouched.
        assert!(!scrollback_focus_key(
            &mut screen,
            KeyCode::Char('o'),
            false,
            false
        ));
        assert!(!screen.sticky_prompt_expanded);

        // Focus the scrollback list, then `o` toggles the band both ways.
        assert!(scrollback_focus_key(
            &mut screen,
            KeyCode::Tab,
            false,
            false
        ));
        assert!(screen.scrollback_focus);
        assert!(scrollback_focus_key(
            &mut screen,
            KeyCode::Char('o'),
            false,
            false
        ));
        assert!(screen.sticky_prompt_expanded);
        assert!(scrollback_focus_key(
            &mut screen,
            KeyCode::Char('o'),
            false,
            false
        ));
        assert!(!screen.sticky_prompt_expanded);
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
    fn slash_enter_submits_model_command_for_the_shared_handler() {
        let mut screen = Screen::new();
        // Type `/model`, then Enter accepts the highlighted command and submits
        // its name as a line for the shared handler to route (not Exit).
        for c in "/model".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        assert!(screen.palette.is_active(&screen.editor_text()));
        match handle_idle_event(&mut screen, key(KeyCode::Enter)) {
            IdleKey::Submit(text) => assert_eq!(text, "/model"),
            _ => panic!("expected submit of /model"),
        }
        assert!(
            screen.editor_is_empty(),
            "editor cleared after palette submit"
        );
    }

    #[test]
    fn slash_tab_completes_then_esc_dismisses() {
        let mut screen = Screen::new();
        handle_idle_event(&mut screen, key(KeyCode::Char('/')));
        handle_idle_event(&mut screen, key(KeyCode::Char('e')));
        // Tab completes to the full command.
        handle_idle_event(&mut screen, key(KeyCode::Tab));
        assert_eq!(screen.editor_text(), "/exit");
        // Esc dismisses; a later Enter then submits the literal text, which the
        // session loop routes to exit via the registry.
        handle_idle_event(&mut screen, key(KeyCode::Esc));
        assert!(!screen.palette.is_active(&screen.editor_text()));
        match handle_idle_event(&mut screen, key(KeyCode::Enter)) {
            IdleKey::Submit(text) => assert!(slash::is_exit(&text)),
            _ => panic!("expected submit of /exit"),
        }
    }

    #[test]
    fn pi_editor_key_aliases_work() {
        let mut screen = Screen::new();
        for c in "ab".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        handle_idle_event(&mut screen, key(KeyCode::Char('X')));
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('f'), KeyModifiers::CONTROL),
        );
        handle_idle_event(&mut screen, key(KeyCode::Char('Y')));
        assert_eq!(screen.editor_text(), "aXbY");

        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "XbY");

        screen.clear_editor();
        for c in "alpha beta".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        handle_idle_event(&mut screen, key_mod(KeyCode::Delete, KeyModifiers::ALT));
        assert_eq!(screen.editor_text(), " beta");

        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('-'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "alpha beta");

        screen.clear_editor();
        for c in "abc".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        handle_idle_event(&mut screen, key_mod(KeyCode::Enter, KeyModifiers::SHIFT));
        for c in "def".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "abc\n");
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
        // Pi's undo shortcut is Ctrl+-.
        handle_idle_event(
            &mut screen,
            key_mod(KeyCode::Char('-'), KeyModifiers::CONTROL),
        );
        assert_eq!(screen.editor_text(), "alpha ");
    }

    #[test]
    fn focus_events_route_to_the_screen_in_idle_and_running_phases() {
        // Running phase: losing focus needs no immediate redraw, regaining focus
        // redraws once, and repeats are no-ops. Ticks keep animating while the
        // pane is inactive so visible adjacent panes stay live.
        let mut screen = Screen::new();
        screen.start_turn();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        assert!(!handle_running_event(
            &mut screen,
            Event::FocusLost,
            &mut pending,
            &steering,
        ));
        assert!(screen.tick(), "inactive pane keeps animating");
        assert!(handle_running_event(
            &mut screen,
            Event::FocusGained,
            &mut pending,
            &steering,
        ));
        assert!(!handle_running_event(
            &mut screen,
            Event::FocusGained,
            &mut pending,
            &steering,
        ));
        assert!(screen.tick(), "refocused pane is still animating");

        // Idle phase: focus reports never submit/exit, and only a focus regain
        // that changed state asks for a redraw.
        let mut screen = Screen::new();
        assert!(matches!(
            handle_idle_event(&mut screen, Event::FocusLost),
            IdleKey::Ignore
        ));
        assert!(matches!(
            handle_idle_event(&mut screen, Event::FocusGained),
            IdleKey::Continue
        ));
        assert!(matches!(
            handle_idle_event(&mut screen, Event::FocusGained),
            IdleKey::Ignore
        ));
    }

    #[test]
    fn running_event_approval_keys_resolve_oneshot() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        // Allow.
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: true,
            allow_project: false,
        });
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Char('y')),
            &mut pending,
            &steering,
        ));
        assert!(pending.is_none());
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Allow);

        // Deny via 'n'.
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: false,
            allow_project: false,
        });
        handle_running_event(
            &mut screen,
            key(KeyCode::Char('n')),
            &mut pending,
            &steering,
        );
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Deny);

        // 'a' is ignored when always is not on offer (and not typed: the composer
        // is frozen while an approval is pending).
        let (tx, mut rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: false,
            allow_project: false,
        });
        assert!(!handle_running_event(
            &mut screen,
            key(KeyCode::Char('a')),
            &mut pending,
            &steering,
        ));
        assert!(pending.is_some());
        assert!(rx.try_recv().is_err());
        // The frozen-composer key did not leak into the editor.
        assert!(screen.editor_is_empty());
    }

    #[test]
    fn running_ctrl_c_denies_pending_approval_and_clears_queue() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        steering.enqueue_steering("queued".to_string());
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: true,
            allow_project: false,
        });
        assert!(handle_running_event(
            &mut screen,
            key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut pending,
            &steering,
        ));
        assert!(pending.is_none());
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Deny);
        // Aborting also discards what the user had queued.
        assert_eq!(steering.len(), 0);
    }

    #[test]
    fn running_esc_cancels_turn_when_nothing_higher_priority_consumes_it() {
        // Esc with no approval pending and no dropdown open cancels the running
        // turn (issue #511): the token is cancelled and queued steering dropped.
        let mut screen = Screen::new();
        screen.start_turn();
        let steering = SteeringQueue::default();
        steering.enqueue_steering("queued".to_string());
        let token = CancellationToken::new();
        let mut pending: Option<PendingApproval> = None;
        assert!(super::handle_running_event(
            &mut screen,
            key(KeyCode::Esc),
            &mut pending,
            &steering,
            &GitStatusCache::default(),
            &token,
        ));
        assert!(token.is_cancelled());
        assert_eq!(steering.len(), 0);
    }

    #[test]
    fn running_esc_denies_pending_approval_without_cancelling_turn() {
        // A pending approval owns Esc: it denies (preserved) and must not cancel
        // the turn's token (issue #511 must not regress approval Esc).
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: true,
            allow_project: false,
        });
        let token = CancellationToken::new();
        assert!(super::handle_running_event(
            &mut screen,
            key(KeyCode::Esc),
            &mut pending,
            &steering,
            &GitStatusCache::default(),
            &token,
        ));
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Deny);
        assert!(
            !token.is_cancelled(),
            "approval Esc denies, it does not cancel the turn"
        );
    }

    #[test]
    fn running_esc_with_open_dropdown_closes_it_without_cancelling() {
        // An open dropdown consumes Esc (closes the readout) and leaves the turn
        // running (issue #511 preserves menu Esc).
        let mut screen = git_screen();
        screen.start_turn();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        let token = CancellationToken::new();
        super::handle_running_event(
            &mut screen,
            key_mod(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &mut pending,
            &steering,
            &GitStatusCache::default(),
            &token,
        );
        assert!(screen.session_menu.is_some());
        assert!(super::handle_running_event(
            &mut screen,
            key(KeyCode::Esc),
            &mut pending,
            &steering,
            &GitStatusCache::default(),
            &token,
        ));
        assert!(screen.session_menu.is_none());
        assert!(
            !token.is_cancelled(),
            "menu Esc closes the dropdown, not the turn"
        );
    }

    #[test]
    fn running_enter_queues_steering_and_clears_editor() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        // Type some text, then Enter queues it as a steering message.
        for ch in "go left".chars() {
            handle_running_event(&mut screen, key(KeyCode::Char(ch)), &mut pending, &steering);
        }
        assert_eq!(screen.editor_text(), "go left");
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Enter),
            &mut pending,
            &steering,
        ));
        assert_eq!(steering.take_steering(), vec!["go left"]);
        // The composer is cleared, ready for more input.
        assert!(screen.editor_is_empty());
    }

    #[test]
    fn running_alt_enter_queues_follow_up() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        for ch in "then test".chars() {
            handle_running_event(&mut screen, key(KeyCode::Char(ch)), &mut pending, &steering);
        }
        assert!(handle_running_event(
            &mut screen,
            key_mod(KeyCode::Enter, KeyModifiers::ALT),
            &mut pending,
            &steering,
        ));
        assert!(
            steering.take_steering().is_empty(),
            "Alt+Enter is follow-up"
        );
        assert_eq!(steering.take_follow_up(), vec!["then test"]);
        assert!(screen.editor_is_empty());
    }

    #[test]
    fn running_empty_enter_does_not_queue() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        handle_running_event(&mut screen, key(KeyCode::Enter), &mut pending, &steering);
        assert_eq!(steering.len(), 0, "a blank submit queues nothing");
    }

    #[test]
    fn running_settings_command_defers_picker_without_steering() {
        // Issue #489: `/settings` typed mid-turn is a UI command, not model
        // input. Enter must request the deferred picker, never steer the text
        // into the turn.
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        for ch in "/settings".chars() {
            handle_running_event(&mut screen, key(KeyCode::Char(ch)), &mut pending, &steering);
        }
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Enter),
            &mut pending,
            &steering,
        ));
        assert_eq!(steering.len(), 0, "/settings is not steered to the model");
        assert!(steering.take_settings(), "the picker is requested");
        // Drained once: a second boundary check does not re-open it.
        assert!(!steering.take_settings());
        assert!(screen.editor_is_empty(), "the composer is cleared");
    }

    #[test]
    fn running_focus_command_toggles_ui_without_steering() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        for ch in "/focus on".chars() {
            handle_running_event(&mut screen, key(KeyCode::Char(ch)), &mut pending, &steering);
        }
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Enter),
            &mut pending,
            &steering,
        ));
        assert_eq!(steering.len(), 0, "/focus is never model input");
        assert!(
            screen.editor_is_empty(),
            "the composer collapses after submit"
        );
        assert_eq!(
            apply_focus_command(&mut screen, "/focus off"),
            Some("focus mode automatic \u{2014} activates at 12 rows".to_string())
        );
        assert_eq!(
            apply_focus_command(&mut screen, "/focus sideways"),
            Some("usage: /focus [on|off]".to_string())
        );
        assert_eq!(apply_focus_command(&mut screen, "/focused"), None);
    }

    #[test]
    fn running_non_command_text_still_steers() {
        // Regression guard for #489: ordinary text keeps its steering path and
        // never trips the settings request.
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let mut pending: Option<PendingApproval> = None;
        for ch in "/settle the merge".chars() {
            handle_running_event(&mut screen, key(KeyCode::Char(ch)), &mut pending, &steering);
        }
        assert!(handle_running_event(
            &mut screen,
            key(KeyCode::Enter),
            &mut pending,
            &steering,
        ));
        assert_eq!(steering.take_steering(), vec!["/settle the merge"]);
        assert!(!steering.take_settings(), "no settings request for prose");
    }

    #[test]
    fn is_settings_command_matches_only_the_settings_token() {
        assert!(is_settings_command("/settings"));
        assert!(is_settings_command("  /settings  "));
        // Trailing args are ignored, matching route_command's token split.
        assert!(is_settings_command("/settings foo"));
        assert!(!is_settings_command("/settle"));
        assert!(!is_settings_command("/settingsx"));
        assert!(!is_settings_command("settings"));
        assert!(!is_settings_command("please open /settings"));
    }

    #[test]
    fn page_keys_do_not_consume_a_pending_approval() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let (tx, _rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: true,
            allow_project: false,
        });
        // PageUp is a no-op (native scroll) and must not answer the approval.
        assert!(!handle_running_event(
            &mut screen,
            key(KeyCode::PageUp),
            &mut pending,
            &steering,
        ));
        assert!(pending.is_some(), "a page key must not answer the approval");
    }

    #[test]
    fn input_eof_cancels_turn_and_denies_pending() {
        let mut screen = Screen::new();
        let token = CancellationToken::new();
        let (tx, rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: true,
            allow_project: false,
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

    #[test]
    fn idle_chords_open_picker_and_cycle() {
        let mut screen = Screen::new();
        // `$` opens the skill mention picker and does not enter the editor.
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::Char('$'))),
            IdleKey::OpenSkillPicker
        ));
        assert!(screen.editor_is_empty());
        // Ctrl+L opens the model picker.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL)
            ),
            IdleKey::OpenModelPicker
        ));
        // Ctrl+P cycles forward; Ctrl+Shift+P cycles backward.
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            IdleKey::CycleModel(true)
        ));
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(
                    KeyCode::Char('p'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                )
            ),
            IdleKey::CycleModel(false)
        ));
        // Shift+Tab (BackTab) cycles effort.
        assert!(matches!(
            handle_idle_event(&mut screen, key(KeyCode::BackTab)),
            IdleKey::CycleEffort
        ));
    }

    #[test]
    fn idle_chords_yield_to_an_active_palette() {
        let mut screen = Screen::new();
        // While the slash palette steers, Ctrl+P must not hijack navigation.
        handle_idle_event(&mut screen, key(KeyCode::Char('/')));
        assert!(screen.palette.is_active(&screen.editor_text()));
        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue | IdleKey::Ignore
        ));

        let mut screen = Screen::new();
        for c in "/model".chars() {
            handle_idle_event(&mut screen, key(KeyCode::Char(c)));
        }
        assert!(matches!(
            handle_idle_event(&mut screen, key_mod(KeyCode::Enter, KeyModifiers::SHIFT)),
            IdleKey::Continue
        ));
        assert_eq!(screen.editor_text(), "/model\n");
    }

    #[test]
    fn to_modal_key_maps_navigation_and_chords() {
        assert_eq!(to_modal_key(&key(KeyCode::Up)), Some(ModalKey::Up));
        assert_eq!(to_modal_key(&key(KeyCode::Enter)), Some(ModalKey::Enter));
        // Tab is not consumed by any modal; it maps to None so it falls through.
        assert_eq!(to_modal_key(&key(KeyCode::Tab)), None);
        assert_eq!(
            to_modal_key(&key(KeyCode::Char('j'))),
            Some(ModalKey::Char('j'))
        );
        assert_eq!(
            to_modal_key(&key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(ModalKey::CtrlS)
        );
        assert_eq!(
            to_modal_key(&key_mod(KeyCode::Up, KeyModifiers::ALT)),
            Some(ModalKey::AltUp)
        );
        // Unmapped chords fall through to None (the modal ignores them).
        assert_eq!(
            to_modal_key(&key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn login_input_resize_requests_redraw_without_manual_edit() {
        let (manual_tx, _manual_rx) = std::sync::mpsc::channel::<String>();
        let mut dialog = LoginDialog::new("openai-codex", false);

        assert!(handle_login_input_event(
            &mut dialog,
            &Event::Resize(90, 30),
            &manual_tx,
        ));
    }

    #[test]
    fn is_modal_cancel_matches_esc_and_ctrl_c() {
        assert!(is_modal_cancel(&key(KeyCode::Esc)));
        assert!(is_modal_cancel(&key_mod(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_modal_cancel(&key(KeyCode::Enter)));
    }

    // --- faceplate stash / reopen-before-draw, dialog-guard round trips, and
    // slash routing (spec §7 criteria 7, 12, 17–18, 21). The async loop needs a
    // live TTY, so these drive the production functions the loop composes:
    // `leaves_faceplate_for_guard` (the stash decision), `refresh_settings_panel`
    // (the reopen), `apply_action` (the advisory), and the `open_settings_expanded`
    // / `model_command` slash delegates. ---

    use crate::mimir::selection::{ProviderId, ReasoningEffort};

    fn null_selection() -> ModelSelection {
        ModelSelection {
            provider: ProviderId::OpenAiCodex,
            model: "gpt-5.5".to_string(),
            base_url: "https://example".to_string(),
            reasoning: None,
            cache_retention: crate::mimir::selection::PromptCacheRetention::Short,
            codex_transport: crate::mimir::selection::CodexTransport::Auto,
            context_management: crate::mimir::selection::ContextManagement::default(),
            legacy_context_management: crate::mimir::selection::ContextManagement::default(),
            tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            configured_tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            retry_policy: crate::mimir::retry::RetryPolicy::default(),
            open_ai_compatible: crate::mimir::selection::OpenAiCompatibleConfig::default(),
        }
    }

    /// An in-memory harness carrying `chars/4` estimated context tokens, so the
    /// large-context switch advisory can be tripped deterministically.
    fn harness_with_context(
        chars: usize,
        budget: Option<u64>,
    ) -> (Harness<NullChat>, crate::tools::test_support::TestDir) {
        use crate::nexus::{Agent, Message};
        let dir = crate::tools::test_support::temp_dir();
        let messages = vec![Message::user(&"x".repeat(chars))];
        let agent = Agent::resumed(NullChat, crate::tools::built_in_tools(), messages);
        let harness = crate::wayland::Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            None,
            budget,
        );
        (harness, dir)
    }

    fn model_choice(
        provider: ProviderId,
        model_id: &str,
        is_current: bool,
        is_default: bool,
    ) -> settings_menu::ModelChoice {
        let qualified = format!("{}/{}", provider.as_str(), model_id);
        settings_menu::ModelChoice {
            display: crate::mimir::model_catalog::display_name(&qualified),
            provider_label: provider.display_name().to_string(),
            levels: crate::mimir::model_capabilities::level_options(provider, model_id)
                .iter()
                .map(|option| (option.level, option.label))
                .collect(),
            provider,
            model_id: model_id.to_string(),
            is_current,
            is_default,
            qualified,
        }
    }

    /// A hand-built faceplate snapshot with a real catalog + providers, so the
    /// hatch-content assertions do not depend on the runner's auth store.
    fn faceplate_snapshot() -> settings_menu::Snapshot {
        settings_menu::Snapshot {
            default_model: "openai-codex/gpt-5.5".to_string(),
            reasoning_levels: vec![
                (ReasoningEffort::Low, "low"),
                (ReasoningEffort::Medium, "medium"),
                (ReasoningEffort::High, "high"),
            ],
            reasoning: ReasoningEffort::Medium,
            catalog: vec![
                model_choice(ProviderId::OpenAiCodex, "gpt-5.5", true, true),
                model_choice(ProviderId::Anthropic, "claude-sonnet-4-6", false, false),
            ],
            scope_candidates: vec![
                settings_menu::ScopeChoice {
                    qualified: "openai-codex/gpt-5.5".to_string(),
                    provider_label: "OpenAI Codex".to_string(),
                },
                settings_menu::ScopeChoice {
                    qualified: "anthropic/claude-sonnet-4-6".to_string(),
                    provider_label: "Anthropic".to_string(),
                },
            ],
            scope_enabled: None,
            scope_persisted: None,
            providers: vec![
                settings_menu::ProviderStatus {
                    id: "openai-codex".to_string(),
                    name: "OpenAI Codex".to_string(),
                    badge: "subscription".to_string(),
                    oauth_capable: true,
                    api_key_capable: false,
                    credentialed: true,
                },
                settings_menu::ProviderStatus {
                    id: "anthropic".to_string(),
                    name: "Anthropic".to_string(),
                    badge: "\u{2014}".to_string(),
                    oauth_capable: true,
                    api_key_capable: true,
                    credentialed: false,
                },
            ],
            policy: settings_menu::PolicySnapshot::default(),
            default_approval: "auto".to_string(),
            skip_permissions: false,
            context_token_budget: 232_000,
            compaction_enabled: true,
            compaction_warn_pct: 60,
            compaction_start_pct: 72,
            compaction_hard_pct: 90,
            compaction_keep_recent_tokens: 8_000,
            compaction_hard_wait_ms: 120_000,
            compaction_reactive: true,
            compaction_worker_input: "transcript".to_string(),
            resolved_ladder: None,
            compaction_summarizer: "subagent".to_string(),
            microcompaction: true,
            microcompaction_watermark: 32_000,
            compaction_aggressiveness: "conservative".to_string(),
            compaction_cache_timing: "cacheAware".to_string(),
            semantic_retain_per_path: 1,
            tool_clearing_keep_recent: 8,
            prompt_cache_retention: "short".to_string(),
            web_search_backend: "off".to_string(),
            read_web_page_backend: "off".to_string(),
            verify_command: None,
            verify_max_attempts: 3,
            theme: "terminal".to_string(),
            alt_screen: "auto".to_string(),
            scroll_speed: 3,
            reduced_motion: false,
            mutation_safety: true,
            native_jj_available: true,
            native_jj_enabled: false,
            worktree_root: None,
        }
    }

    fn flatten(lines: &[ratatui::text::Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --- criterion 7: the stash is armed for exactly the guard handoffs ---
    #[test]
    fn guard_handoffs_arm_the_faceplate_stash_but_inline_refreshes_do_not() {
        // The reopen-before-draw stash must be armed for every conditional
        // dialog-guard handoff. Inline refreshes (scope, effort, ordinary
        // settings, policy, logout) never leave the faceplate, so they must not
        // arm it — logout in particular refreshes in place (§2.5).
        let guards = [
            ModalAction::SelectModel {
                id: "anthropic/claude-sonnet-4-6".to_string(),
                effort: ReasoningEffort::Medium,
                save_default: true,
            },
            ModalAction::ConfirmModelSwitch {
                id: "anthropic/claude-sonnet-4-6".to_string(),
                effort: ReasoningEffort::Medium,
                save_default: true,
                compact_first: false,
            },
            ModalAction::CycleModel { forward: true },
            ModalAction::SaveSetting {
                field: crate::ui::settings_menu::Field::MutationSafety,
                value: Some("true".to_string()),
            },
            ModalAction::BeginLogin(ProviderId::Anthropic),
            ModalAction::OpenApiKeyDialog("openai".to_string()),
        ];
        for action in &guards {
            assert!(
                leaves_faceplate_for_guard(action),
                "{action:?} must stash the faceplate"
            );
        }
        let inline = [
            ModalAction::ApplyScoped(None),
            ModalAction::SaveScoped(None),
            ModalAction::AdjustEffort(ReasoningEffort::High),
            ModalAction::SaveSetting {
                field: crate::ui::settings_menu::Field::ReducedMotion,
                value: Some("true".to_string()),
            },
            ModalAction::EditPolicy(crate::wayland::trust::ProjectPolicyEdit::GrantTool(
                "write".to_string(),
            )),
            ModalAction::Logout("anthropic".to_string()),
            ModalAction::ToggleSkipPermissions,
        ];
        for action in &inline {
            assert!(
                !leaves_faceplate_for_guard(action),
                "{action:?} refreshes in place, not via the stash"
            );
        }
    }

    // --- criterion 7: no frame is ever drawn with the dock collapsed ---
    #[test]
    fn a_dialog_guard_round_trip_reopens_the_faceplate_before_any_frame() {
        // Models run_modal_phase's draw ordering (~L2170–2185) over a real Screen
        // and the production `refresh_settings_panel` reopen: the guard's own
        // handler closes the panel (no draw in that window), then the loop reopens
        // the faceplate BEFORE the next draw. Covers OAuth, API-key, large-context,
        // and native-jj consent guards.
        let (mut harness, _dir) = harness_with_context(80_000, Some(40_000));
        let build = |_s: &ModelSelection, _p: &str| Ok(NullChat);
        let mut switch = ModelSwitch::new(null_selection(), "P".to_string(), &build, None);

        // Mint a real large-context switch prompt to use as one guard.
        let switch_prompt = match picker::apply_action(
            ModalAction::SelectModel {
                id: "anthropic/claude-sonnet-4-6".to_string(),
                effort: ReasoningEffort::Medium,
                save_default: false,
            },
            None,
            &mut harness,
            &mut switch,
            &SettingsEventSink::default(),
        ) {
            ActionResult::Replace(modal, _) => *modal,
            _ => panic!("expected the large-context switch prompt"),
        };

        let cases: Vec<(settings_menu::HatchTarget, Modal)> = vec![
            // The OAuth login dialog — the path whose trailing draw was removed so
            // the stash owns the reopen.
            (
                settings_menu::HatchTarget::Login,
                Modal::LoginDialog(LoginDialog::new("Anthropic", true)),
            ),
            (
                settings_menu::HatchTarget::Login,
                login::open_api_key_dialog("openai"),
            ),
            (settings_menu::HatchTarget::Model, switch_prompt),
            (
                settings_menu::HatchTarget::Model,
                crate::ui::modal::jj_setup(),
            ),
        ];

        for (target, guard) in cases {
            let mut screen = Screen::new();
            // The faceplate is docked and drawn on the hatch (frame 0).
            screen.open_modal(picker::open_settings_expanded(&harness, &switch, target));
            let view = match &screen.modal {
                Some(Modal::Settings(panel)) => panel.view(),
                _ => panic!("faceplate front"),
            };
            let mut frames = vec![screen.modal.is_some()];
            // A child-row verb hands off to the guard; the loop draws it.
            screen.open_modal(guard);
            frames.push(screen.modal.is_some());
            // The guard resolves: its handler closes the modal (no draw here),
            // then the loop reopens the faceplate before the next draw.
            screen.close_modal();
            screen.open_modal(picker::refresh_settings_panel(
                view.clone(),
                None,
                &harness,
                &switch,
            ));
            frames.push(screen.modal.is_some());
            // Every drawn frame kept a modal — the dock never collapsed.
            assert!(
                frames.iter().all(|&present| present),
                "{target:?}: a frame was drawn without the faceplate: {frames:?}"
            );
            // The faceplate came back, expanded, cursor held.
            match &screen.modal {
                Some(Modal::Settings(panel)) => {
                    let back = panel.view();
                    assert_eq!(
                        back.expanded(),
                        view.expanded(),
                        "reopened expanded ({target:?})"
                    );
                    assert_eq!(back.cursor(), view.cursor(), "cursor held ({target:?})");
                }
                _ => panic!("faceplate reopened ({target:?})"),
            }
        }
    }

    // --- criterion 12: large-context select prompts, then re-homes the candidate ---
    #[test]
    fn a_large_context_switch_prompts_then_returns_to_the_same_candidate() {
        let (mut harness, _dir) = harness_with_context(80_000, Some(40_000));
        let build = |_s: &ModelSelection, _p: &str| Ok(NullChat);
        let mut switch = ModelSwitch::new(null_selection(), "P".to_string(), &build, None);

        // A real model change carrying a large context overlays the advisory.
        match picker::apply_action(
            ModalAction::SelectModel {
                id: "anthropic/claude-sonnet-4-6".to_string(),
                effort: ReasoningEffort::Medium,
                save_default: false,
            },
            None,
            &mut harness,
            &mut switch,
            &SettingsEventSink::default(),
        ) {
            ActionResult::Replace(modal, _) => {
                assert!(
                    matches!(*modal, Modal::SwitchContext(_)),
                    "advisory overlays the prompt"
                )
            }
            _ => panic!("large context must overlay the switch prompt"),
        }

        // Below the threshold the same switch resolves without the guard.
        let (mut small, _small_dir) = harness_with_context(400, Some(40_000));
        let mut small_switch = ModelSwitch::new(null_selection(), "P".to_string(), &build, None);
        let resolved = picker::apply_action(
            ModalAction::SelectModel {
                id: "anthropic/claude-sonnet-4-6".to_string(),
                effort: ReasoningEffort::Medium,
                save_default: false,
            },
            None,
            &mut small,
            &mut small_switch,
            &SettingsEventSink::default(),
        );
        assert!(
            !matches!(resolved, ActionResult::Replace(modal, _) if matches!(*modal, Modal::SwitchContext(_))),
            "no advisory below the threshold"
        );

        // The stashed view is the single source of truth for the return: neither
        // a confirm nor a cancel touches it, so both re-home the same candidate.
        let mut panel = settings_menu::SettingsPanel::with_expanded(
            faceplate_snapshot(),
            settings_menu::HatchTarget::Model,
        );
        panel.handle_key(ModalKey::Down); // off the active row, onto the next candidate
        let candidate = panel.view().cursor();
        assert!(
            matches!(candidate, settings_menu::PanelRow::ModelChild(_)),
            "cursor on a candidate row"
        );
        let view = panel.view();
        for resolution in ["confirm", "cancel"] {
            let mut back = settings_menu::SettingsPanel::new(faceplate_snapshot());
            back.restore(view.clone());
            assert_eq!(
                back.view().expanded(),
                Some(settings_menu::RowId::Model),
                "{resolution}: returns expanded on the model hatch"
            );
            assert_eq!(
                back.view().cursor(),
                candidate,
                "{resolution}: cursor held on the same candidate"
            );
        }
    }

    // --- decision #4 / coordinator override: the dangerous skip-approvals bypass
    // PERSISTS as the default permission mode (#520) and survives a restart. It is
    // NOT session-only — the faceplate row clicked once must still be dangerous on
    // the next boot. ---
    #[test]
    fn skip_approvals_persists_the_dangerous_default_and_survives_a_restart() {
        use crate::mimir::test_support::ConfigPathGuard;
        use crate::nexus::{ApprovalMode, PermissionMode};

        let dir = crate::tools::test_support::temp_dir();
        let global = dir.path.join("settings.json");
        let _guard = ConfigPathGuard::set(&global);

        let (mut harness, _hdir) = harness_with_context(400, Some(40_000));
        harness.set_approval_mode(ApprovalMode::Auto);
        let build = |_s: &ModelSelection, _p: &str| Ok(NullChat);
        let mut switch = ModelSwitch::new(null_selection(), "P".to_string(), &build, None);
        assert!(!harness.skip_permissions(), "starts with the bypass off");

        // Click the faceplate skip-approvals switch ON: applied live AND persisted
        // through #520's permission-mode default.
        let _ = picker::apply_action(
            ModalAction::ToggleSkipPermissions,
            None,
            &mut harness,
            &mut switch,
            &SettingsEventSink::default(),
        );
        assert!(
            harness.skip_permissions(),
            "the bypass is live this session"
        );

        // Restart-shaped: a FRESH settings load reads the persisted global token,
        // and startup resolution re-enables the bypass — proving it is not
        // session-only but survives across restarts.
        let reloaded = crate::config::Settings::load(&dir.path).unwrap();
        assert_eq!(
            reloaded.default_approval.as_deref(),
            Some(crate::nexus::DANGEROUS_SKIP_PERMISSIONS_TOKEN),
            "the dangerous default persisted to global settings"
        );
        assert!(
            matches!(
                PermissionMode::from_startup_setting(reloaded.default_approval.as_deref()),
                PermissionMode::DangerousSkipPermissions
            ),
            "a fresh boot resolves the persisted token back to the bypass"
        );

        // Toggling it back OFF restores AND persists the parked approval preset,
        // so a later restart is no longer dangerous (the persistence is symmetric).
        let _ = picker::apply_action(
            ModalAction::ToggleSkipPermissions,
            None,
            &mut harness,
            &mut switch,
            &SettingsEventSink::default(),
        );
        assert!(!harness.skip_permissions(), "the bypass is cleared live");
        let reloaded = crate::config::Settings::load(&dir.path).unwrap();
        assert_ne!(
            reloaded.default_approval.as_deref(),
            Some(crate::nexus::DANGEROUS_SKIP_PERMISSIONS_TOKEN),
            "clearing the bypass persists a normal default, not the dangerous token"
        );
        assert!(
            !matches!(
                PermissionMode::from_startup_setting(reloaded.default_approval.as_deref()),
                PermissionMode::DangerousSkipPermissions
            ),
            "a fresh boot no longer resolves to the bypass"
        );
    }

    // --- criterion 17: a resolved login returns expanded, badge/count refreshed ---
    #[test]
    fn a_resolved_login_returns_to_the_provider_row_with_a_refreshed_badge() {
        // The login/api-key dialog is a guard: on any resolution the loop reopens
        // the providers hatch from a fresh snapshot with the cursor held. Modeled
        // as the loop's refresh does it — a fresh panel from the post-login
        // snapshot with the stashed view restored.
        let panel = settings_menu::SettingsPanel::with_expanded(
            faceplate_snapshot(),
            settings_menu::HatchTarget::Login,
        );
        let view = panel.view();
        let cursor = view.cursor();
        assert_eq!(
            cursor,
            settings_menu::PanelRow::ProviderChild("anthropic".to_string()),
            "login lands on the uncredentialed row"
        );

        // Cancel: the same snapshot, view restored — back on the row, expanded.
        let mut cancelled = settings_menu::SettingsPanel::new(faceplate_snapshot());
        cancelled.restore(view.clone());
        assert_eq!(
            cancelled.view().expanded(),
            Some(settings_menu::RowId::Providers)
        );
        assert_eq!(cancelled.view().cursor(), cursor);

        // Success: anthropic authenticates — badge → subscription, count 1 → 2 —
        // and the cursor stays put across the refresh.
        let mut snap = faceplate_snapshot();
        snap.providers[1].credentialed = true;
        snap.providers[1].badge = "subscription".to_string();
        let mut back = settings_menu::SettingsPanel::new(snap);
        back.restore(view);
        assert_eq!(
            back.view().cursor(),
            cursor,
            "cursor held across the refresh"
        );
        let rendered = flatten(&back.render_budgeted(100, 80));
        assert!(
            rendered.contains("2 connected"),
            "header count refreshed:\n{rendered}"
        );
        assert!(
            rendered.contains("subscription"),
            "badge refreshed:\n{rendered}"
        );
    }

    // --- criterion 18: logout drops the row, decrements the count, refreshes catalog ---
    #[test]
    fn logout_drops_the_row_decrements_the_header_and_refreshes_the_catalog() {
        // Logout is inline (not a guard, asserted above): the loop captures the
        // view and rebuilds from the post-logout snapshot. The row drops to ○ · —,
        // the header count decrements, and the ENGINE catalog shrinks (the logged
        // out provider's models leave it). Cursor held.
        let mut before = faceplate_snapshot();
        before.providers[1].credentialed = true;
        before.providers[1].badge = "subscription".to_string();
        before.catalog.push(model_choice(
            ProviderId::Anthropic,
            "claude-opus-4-8",
            false,
            false,
        ));
        let panel =
            settings_menu::SettingsPanel::with_expanded(before, settings_menu::HatchTarget::Logout);
        let view = panel.view();
        let cursor = view.cursor();
        assert_eq!(
            cursor,
            settings_menu::PanelRow::ProviderChild("openai-codex".to_string()),
            "logout lands on the first credentialed row"
        );

        // openai-codex logs out: uncredentialed, its model gone from the catalog.
        let mut after = faceplate_snapshot();
        after.providers[0].credentialed = false;
        after.providers[0].badge = "\u{2014}".to_string();
        after.providers[1].credentialed = true;
        after.providers[1].badge = "subscription".to_string();
        after.catalog = vec![model_choice(
            ProviderId::Anthropic,
            "claude-sonnet-4-6",
            true,
            true,
        )];
        let mut back = settings_menu::SettingsPanel::new(after.clone());
        back.restore(view);
        assert_eq!(
            back.view().cursor(),
            cursor,
            "cursor held across the refresh"
        );
        let rendered = flatten(&back.render_budgeted(100, 80));
        assert!(
            rendered.contains("1 connected"),
            "count decremented:\n{rendered}"
        );
        assert!(
            rendered.contains('\u{2014}'),
            "logged-out row dropped to —:\n{rendered}"
        );

        // The ENGINE catalog refreshed — gpt-5.5 left with its provider.
        let catalog_view = flatten(
            &settings_menu::SettingsPanel::with_expanded(after, settings_menu::HatchTarget::Model)
                .render_budgeted(100, 80),
        );
        assert!(
            !catalog_view.contains("GPT 5.5"),
            "the logged-out provider's model left the catalog:\n{catalog_view}"
        );
    }

    // --- criterion 21: slash entries open the faceplate on the named hatch ---
    #[test]
    fn slash_entries_open_the_faceplate_on_the_named_hatch() {
        // route_command needs a live TuiUi, so it delegates the hatch mapping to
        // `open_settings_expanded` (per HatchTarget) and `model_command` (bare
        // /model / /reasoning). The expansion each target opens is auth-independent
        // (the cursor placement and the typed fast path are pinned in the
        // settings_menu / picker unit tests).
        let (harness, _dir) = harness_with_context(400, None);
        let build = |_s: &ModelSelection, _p: &str| Ok(NullChat);
        let switch = ModelSwitch::new(null_selection(), "P".to_string(), &build, None);

        for (target, want) in [
            (
                settings_menu::HatchTarget::Model,
                settings_menu::RowId::Model,
            ),
            (
                settings_menu::HatchTarget::Scope,
                settings_menu::RowId::Scope,
            ),
            (
                settings_menu::HatchTarget::Permissions,
                settings_menu::RowId::Permissions,
            ),
            (
                settings_menu::HatchTarget::Login,
                settings_menu::RowId::Providers,
            ),
            (
                settings_menu::HatchTarget::Logout,
                settings_menu::RowId::Providers,
            ),
        ] {
            match picker::open_settings_expanded(&harness, &switch, target) {
                Modal::Settings(panel) => assert_eq!(
                    panel.view().expanded(),
                    Some(want),
                    "{target:?} opens its hatch"
                ),
                _ => panic!("expected the faceplate for {target:?}"),
            }
        }
    }
}
