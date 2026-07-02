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

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant as StdInstant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui_textarea::CursorMove;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::cli::{LoadedSource, ModelSwitch, SessionLoader, SessionSource};
use crate::mimir::auth::storage::AuthStore;
use crate::mimir::model_catalog;
use crate::nexus::{
    AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ChatProvider, ToolCall,
};
use crate::ui::UiEvent;
use crate::ui::login::{self, LoginBackend, LoginOutcome, LoginUpdate, OAuthLoginBackend};
use crate::ui::modal::{LoginDialog, Modal, ModalAction, ModalKey, ModalOutcome};
use crate::ui::picker::{self, ActionResult, ModelCommand};
use crate::ui::slash::{self, SlashAction, SlashCommand};
use crate::ui::steering::SteeringQueue;
use crate::ui::tui::{FocusTarget, Screen, TuiUi};
use crate::wayland::Harness;

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

/// Footer git branch cache lifetime. The footer is presentation-only, so a
/// branch can be a few seconds stale instead of shelling out on every loop.
const FOOTER_BRANCH_TTL: Duration = Duration::from_secs(5);

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

/// The active turn's cancellation token, shared with the input thread so a raw
/// Ctrl-C cancels even while a synchronous tool blocks the executor thread.
type CurrentTurn = Arc<Mutex<Option<CancellationToken>>>;

/// Run the interactive terminal-surface session to completion on `runtime`, then
/// restore the terminal. `tui` already owns raw mode and paste/key flags.
pub(crate) fn run<P: ChatProvider>(
    harness: &mut Harness<P>,
    runtime: &Runtime,
    mut tui: TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    swap: &SessionLoader<'_>,
    startup_modal: Option<Modal>,
) -> Result<()> {
    let result = runtime.block_on(session_loop(harness, &mut tui, switch, swap, startup_modal));
    tui.shutdown();
    result
}

/// What routing a submitted `/` command decided. `Consumed` = handled (a modal
/// may now be open); `Fall` = not a command, run it as a normal turn;
/// `Swap` = perform an in-session session swap at the boundary.
enum RouteOutcome {
    Consumed,
    Fall,
    Swap(SessionSource),
}

/// Outcome of the idle (between-turns) input phase.
enum IdleOutcome {
    Submit(String),
    Exit,
    /// Ctrl+L: open the model picker.
    OpenModelPicker,
    /// Ctrl+P (forward) / Shift+Ctrl+P (backward): cycle the model.
    CycleModel(bool),
    /// Shift+Tab: cycle the thinking/effort level.
    CycleEffort,
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
    CycleModel(bool),
    CycleEffort,
}

/// A gated tool waiting for the user's decision: the reply channel back into the
/// turn future plus whether "always" is on offer.
struct PendingApproval {
    call: ToolCall,
    reply: oneshot::Sender<ApprovalDecision>,
    allow_always: bool,
}

/// A review request crossing from the turn future into the loop.
struct ApprovalRequest {
    call: ToolCall,
    allow_always: bool,
    reply: oneshot::Sender<ApprovalDecision>,
}

async fn session_loop<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    swap: &SessionLoader<'_>,
    startup_modal: Option<Modal>,
) -> Result<()> {
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
    refresh_footer(tui, switch);
    // `iris resume` (no id) on a rich TTY opens the resume picker on start by
    // handing a pre-built modal here. Open it before the first draw and before
    // the blocking input reader starts, so the first key acts on a visible
    // picker.
    if let Some(modal) = startup_modal {
        tui.screen.open_modal(modal);
    }
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
            )
            .await?;
            if let Some(source) = requested {
                perform_swap(&source, swap, harness, tui, switch)?;
            }
            refresh_footer(tui, switch);
            tui.draw()?;
            continue;
        }
        match idle_phase(tui, &mut input_rx, &mut tick).await? {
            IdleOutcome::Exit => break,
            IdleOutcome::OpenModelPicker => {
                if let Some(sw) = switch.as_mut() {
                    match picker::model_command("", harness, sw) {
                        ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                        ModelCommand::Lines(lines) => apply_notices(tui, lines),
                    }
                }
            }
            IdleOutcome::CycleModel(forward) => {
                if let Some(sw) = switch.as_mut() {
                    let lines = picker::cycle_model(forward, harness, sw);
                    apply_notices(tui, lines);
                }
            }
            IdleOutcome::CycleEffort => {
                if let Some(sw) = switch.as_mut() {
                    let lines = picker::cycle_effort(harness, sw);
                    apply_notices(tui, lines);
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
                // Picker/model/reasoning/session commands are handled at this
                // safe inter-turn boundary and never start a turn.
                match route_command(&prompt, harness, tui, switch)? {
                    RouteOutcome::Swap(source) => {
                        perform_swap(&source, swap, harness, tui, switch)?;
                    }
                    // Consumed: a modal may now be open; the top-of-loop modal
                    // phase runs it on the next iteration.
                    RouteOutcome::Consumed => {}
                    RouteOutcome::Fall => {
                        tui.screen.commit_user(&prompt);
                        tui.screen.start_turn();
                        tui.draw()?;
                        run_turn(
                            harness,
                            tui,
                            &mut input_rx,
                            &mut tick,
                            &current_turn,
                            &prompt,
                            steering.as_ref(),
                        )
                        .await?;
                        tui.screen.end_turn();
                        tui.draw()?;
                    }
                }
            }
        }
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
    harness.swap_session(loaded.session_log, loaded.messages, resumed);
    tui.reset_screen();
    tui.screen.apply(UiEvent::SessionStarted);
    let notice = match source {
        SessionSource::Fresh => "Started a new session.".to_string(),
        SessionSource::Resume(_) => {
            format!("Resumed session ({resumed} message(s) restored).")
        }
    };
    apply_notices(tui, vec![notice]);
    Ok(())
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

/// Refresh the idle status footer from the live model selection. A no-op when
/// no model switch is wired (the footer then stays unset and the keybind hint
/// shows instead).
fn refresh_footer<P: ChatProvider>(tui: &mut TuiUi, switch: &Option<ModelSwitch<'_, P>>) {
    let Some(sw) = switch.as_ref() else {
        return;
    };
    let selection = sw.selection();
    let effort = selection
        .reasoning
        .map(|effort| effort.as_str().to_string());
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
    let display = if let Some(home) = std::env::var_os("HOME")
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
    };

    match footer_branch(&cwd) {
        Some(branch) => format!("{display} ({branch})"),
        None => display,
    }
}

#[derive(Default)]
struct FooterBranchCache {
    entries: HashMap<PathBuf, FooterBranchEntry>,
}

struct FooterBranchEntry {
    branch: Option<String>,
    observed_at: StdInstant,
}

fn footer_branch(cwd: &std::path::Path) -> Option<String> {
    static CACHE: OnceLock<Mutex<FooterBranchCache>> = OnceLock::new();
    let now = StdInstant::now();
    let key = cwd.to_path_buf();
    let cache = CACHE.get_or_init(|| Mutex::new(FooterBranchCache::default()));
    if let Ok(guard) = cache.lock()
        && let Some(entry) = guard.entries.get(&key)
        && now.duration_since(entry.observed_at) < FOOTER_BRANCH_TTL
    {
        return entry.branch.clone();
    }

    let branch = read_footer_branch(cwd);
    if let Ok(mut guard) = cache.lock() {
        guard.entries.insert(
            key,
            FooterBranchEntry {
                branch: branch.clone(),
                observed_at: now,
            },
        );
    }
    branch
}

fn read_footer_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("branch")
        .arg("--show-current")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// Route a submitted `/` command to its picker/handler. Returns a
/// [`RouteOutcome`]: `Consumed` (handled, a modal may be open), `Fall` (not a
/// command; run it as a turn), or `Swap` (perform a session swap at the
/// boundary). `/login`/`/logout` with arguments are intentionally not recognized
/// (pi-mono parity) and fall through to a normal turn.
fn route_command<P: ChatProvider>(
    prompt: &str,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
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
            match picker::model_command(rest, harness, sw) {
                ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                ModelCommand::Lines(lines) => apply_notices(tui, lines),
            }
            Ok(RouteOutcome::Consumed)
        }
        "/scoped-models" => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            match picker::open_scoped(sw) {
                ModelCommand::Open(modal) => tui.screen.open_modal(modal),
                ModelCommand::Lines(lines) => apply_notices(tui, lines),
            }
            Ok(RouteOutcome::Consumed)
        }
        "/settings" => {
            let Some(sw) = switch.as_mut() else {
                return Ok(RouteOutcome::Fall);
            };
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_settings(sw));
            Ok(RouteOutcome::Consumed)
        }
        "/trust" if rest.is_empty() => {
            // Needs a switch to rebuild the provider with the re-assembled prompt.
            if switch.as_ref().is_none() {
                return Ok(RouteOutcome::Fall);
            }
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(picker::open_trust());
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
        "/copy" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            apply_notices(tui, crate::cli::copy_command_lines(harness));
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
            tui.screen.commit_user(prompt);
            tui.screen.open_modal(login::open_login());
            Ok(RouteOutcome::Consumed)
        }
        "/logout" if rest.is_empty() => {
            tui.screen.commit_user(prompt);
            match AuthStore::from_env() {
                Ok(auth) => match login::open_logout(&auth) {
                    login::LoginStep::Open(modal) => tui.screen.open_modal(modal),
                    login::LoginStep::Lines(lines) => apply_notices(tui, lines),
                },
                Err(error) => apply_notices(tui, vec![format!("auth unavailable: {error:#}")]),
            }
            Ok(RouteOutcome::Consumed)
        }
        "/reasoning" => {
            // Legacy text effort path is preserved as a compatible alias. It
            // takes the whole `Option<ModelSwitch>` like the text driver does.
            tui.screen.commit_user(prompt);
            if let Some(lines) = crate::cli::handle_model_command(prompt, harness, switch) {
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
    let contents = debug_snapshot_contents(size.width, size.height, &rendered, harness.messages());
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
    messages: &[crate::nexus::Message],
) -> String {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(rendered.len() + messages.len() + 8);
    out.push(format!(
        "Iris {} debug snapshot at unix-ms {unix_ms}",
        env!("CARGO_PKG_VERSION")
    ));
    out.push(format!("Terminal: {width}x{height}"));
    out.push(format!("Total lines: {}", rendered.len()));
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
) -> Result<IdleOutcome> {
    let mut last_resize_width = ratatui::crossterm::terminal::size()
        .ok()
        .map(|(width, _)| width);
    let mut pending_width_resize: Option<Instant> = None;
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
                    match handle_idle_event(&mut tui.screen, event) {
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
                        IdleKey::CycleModel(forward) => return Ok(IdleOutcome::CycleModel(forward)),
                        IdleKey::CycleEffort => return Ok(IdleOutcome::CycleEffort),
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
    steering: &SteeringQueue,
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

    let result = {
        let mut turn = std::pin::pin!(harness.submit_turn(prompt, &bridge, &bridge, &token));
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
                    tui.screen.apply(event);
                    // A drained (injected) steering/follow-up message lowers the
                    // queued count; refresh it from the live queue before redraw.
                    tui.screen.set_queued(steering.len());
                    request_render(&mut sched, tui)?;
                }
                Some(request) = appr_rx.recv() => {
                    tui.screen.show_approval(&request.call, request.allow_always);
                    pending = Some(PendingApproval {
                        call: request.call.clone(),
                        reply: request.reply,
                        allow_always: request.allow_always,
                    });
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
                            if handle_running_event(&mut tui.screen, event, &mut pending, steering)
                            {
                                // Reflect any just-enqueued (or cleared) steering
                                // input on the working indicator.
                                tui.screen.set_queued(steering.len());
                                request_render(&mut sched, tui)?;
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
    // that path.
    if token.is_cancelled() {
        steering.clear();
        tui.screen.set_queued(0);
    }
    // Any approval still pending here means the turn ended without resolving it
    // (cancellation); its receiver is already gone, so just drop it.
    drop(pending);

    if let Err(error) = result {
        tui.screen.apply(UiEvent::from_turn_error(&error));
    }
    tui.screen.clear_approval();
    Ok(())
}

/// Drive an open picker/dialog to completion: route keys to the modal, apply the
/// outcomes (model/effort switches, scoped edits, login/logout) at this safe
/// inter-turn boundary, and return when the modal closes (or input ends).
/// Returns the session to swap to when the `/resume` picker selected one, so the
/// caller performs the swap with harness + switch + loader in scope.
async fn run_modal_phase<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    switch: &mut Option<ModelSwitch<'_, P>>,
    login_backend: &Arc<dyn LoginBackend>,
) -> Result<Option<SessionSource>> {
    while tui.screen.focus() == FocusTarget::Modal {
        tokio::select! {
            maybe = input_rx.recv() => {
                let Some(event) = maybe else {
                    // Terminal input ended: close the picker and return.
                    tui.screen.close_modal();
                    break;
                };
                // Track focus even while a modal is open so a turn started
                // later in an unfocused pane begins with the animation paused.
                match &event {
                    Event::FocusGained => {
                        tui.screen.set_terminal_focused(true);
                    }
                    Event::FocusLost => {
                        tui.screen.set_terminal_focused(false);
                    }
                    _ => {}
                }
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
                let requested = apply_modal_outcome(
                    outcome, harness, tui, input_rx, tick, switch, login_backend,
                )
                .await?;
                if requested.is_some() {
                    return Ok(requested);
                }
                // The picker may have switched model/effort; refresh the
                // footer before drawing so it never shows a stale model.
                refresh_footer(tui, switch);
                tui.draw()?;
            }
            _ = tick.tick() => {}
        }
    }
    Ok(None)
}

/// Interpret one [`ModalOutcome`]. Returns a requested session swap (from the
/// `/resume` picker) for the caller to perform.
async fn apply_modal_outcome<P: ChatProvider>(
    outcome: ModalOutcome,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    switch: &mut Option<ModelSwitch<'_, P>>,
    login_backend: &Arc<dyn LoginBackend>,
) -> Result<Option<SessionSource>> {
    match outcome {
        ModalOutcome::Ignore | ModalOutcome::Redraw => Ok(None),
        ModalOutcome::Close => {
            tui.screen.close_modal();
            Ok(None)
        }
        ModalOutcome::Emit(action) => {
            dispatch_action(action, harness, tui, input_rx, tick, switch, login_backend).await
        }
    }
}

/// Apply a [`ModalAction`]: model/scoped/effort actions go through the picker;
/// login/logout actions are handled here (they need the auth store / backend);
/// a `/resume` selection is returned up as the session to swap to.
async fn dispatch_action<P: ChatProvider>(
    action: ModalAction,
    harness: &mut Harness<P>,
    tui: &mut TuiUi,
    input_rx: &mut UnboundedReceiver<Event>,
    tick: &mut tokio::time::Interval,
    switch: &mut Option<ModelSwitch<'_, P>>,
    login_backend: &Arc<dyn LoginBackend>,
) -> Result<Option<SessionSource>> {
    match action {
        ModalAction::ResumeSession(id) => {
            // Close the picker and hand the chosen session up to the loop, which
            // performs the swap at the safe inter-turn boundary.
            tui.screen.close_modal();
            return Ok(Some(SessionSource::Resume(id)));
        }
        ModalAction::ChooseLoginMethod(method) => match AuthStore::from_env() {
            Ok(auth) => match login::provider_select(method, &auth) {
                login::LoginStep::Open(modal) => tui.screen.open_modal(modal),
                login::LoginStep::Lines(lines) => {
                    apply_notices(tui, lines);
                    tui.screen.close_modal();
                }
            },
            Err(error) => {
                apply_notices(tui, vec![format!("auth unavailable: {error:#}")]);
                tui.screen.close_modal();
            }
        },
        ModalAction::BackToLoginMethod => tui.screen.open_modal(login::open_login()),
        ModalAction::BeginLogin(provider) => {
            run_login(provider, tui, input_rx, tick, login_backend).await?;
        }
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
            tui.screen.close_modal();
        }
        ModalAction::Logout(id) => {
            let lines = match AuthStore::from_env() {
                Ok(auth) => login::apply_logout(&id, &auth),
                Err(error) => vec![format!("auth unavailable: {error:#}")],
            };
            apply_notices(tui, lines);
            tui.screen.close_modal();
        }
        // Model / scoped / effort / settings actions.
        other => {
            let Some(sw) = switch.as_mut() else {
                tui.screen.close_modal();
                return Ok(None);
            };
            match picker::apply_action(other, harness, sw) {
                ActionResult::Close(lines) => {
                    apply_notices(tui, lines);
                    tui.screen.close_modal();
                }
                ActionResult::Keep(lines) => apply_notices(tui, lines),
                ActionResult::Replace(modal, lines) => {
                    apply_notices(tui, lines);
                    tui.screen.open_modal(*modal);
                }
            }
        }
    }
    Ok(None)
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
    tui.screen.close_modal();
    tui.draw()?;
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
        KeyCode::Tab => ModalKey::Tab,
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
        // Mouse capture is disabled so the terminal owns scroll/select/copy of
        // the native scrollback; no Mouse events arrive here.
        Event::Resize(..) => return IdleKey::Continue,
        // Focus reports gate the spinner's tick redraws; a regain redraws once
        // so a pane switched back to is visually current.
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

    // Explicit focus routing (Editor < Palette < Modal). Modals run in their own
    // phase, so idle focus is only ever Editor or Palette here. Reuse the input
    // snapshot already computed above instead of re-joining the editor buffer.
    let focus = screen.focus_for(&input);

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
            KeyCode::Char('o') | KeyCode::Char('O') if ctrl => {
                screen.toggle_latest_panel();
                return IdleKey::Continue;
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

    match key.code {
        // --- control flow (idle-only: exit / submit a prompt) ---
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
        KeyCode::Enter if shift || ctrl => screen.editor.insert_newline(),
        KeyCode::Enter => {
            let text = screen.submit();
            if text.trim().is_empty() {
                return IdleKey::Continue;
            }
            return IdleKey::Submit(text);
        }
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
        screen.clear_approval();
    }
}

/// Handle one terminal event while a turn runs. The composer stays live so the
/// user can queue a steering message (Enter) or a follow-up (Alt+Enter) without
/// interrupting the turn; Ctrl-C aborts and Ctrl-O toggles the latest panel.
/// While a tool is awaiting approval the composer is frozen and only the
/// approval keys (plus Ctrl-C/-O) act, so a `y`/`n` can't be both an answer and
/// typed text. Returns whether a redraw is needed.
fn handle_running_event(
    screen: &mut Screen,
    event: Event,
    pending: &mut Option<PendingApproval>,
    steering: &SteeringQueue,
) -> bool {
    match event {
        // Paste composes into the live editor (but not while an approval is
        // pending, when the composer is frozen).
        Event::Paste(text) if pending.is_none() => {
            insert_paste(screen, &text);
            true
        }
        // Mouse capture is off; the terminal scrolls its own scrollback. Resize
        // still triggers a redraw of the terminal surface.
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
                screen.toggle_latest_panel();
                return true;
            }
            if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
                // The input thread already cancelled the token. Aborting also
                // discards anything the user queued, and unblocks a pending
                // approval as Deny so Nexus observes the cancellation and aborts.
                steering.clear();
                if let Some(p) = pending.take() {
                    let _ = p.reply.send(ApprovalDecision::Deny);
                    screen.clear_approval();
                }
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
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                        Some(ApprovalDecision::Deny)
                    }
                    _ => None,
                };
                if let Some(decision) = decision {
                    let p = pending.take().expect("pending approval present");
                    screen.record_approval(&p.call, decision);
                    let _ = p.reply.send(decision);
                    screen.clear_approval();
                    return true;
                }
                return false;
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
                    screen.editor.insert_newline();
                    true
                }
                KeyCode::Enter => {
                    let text = screen.submit();
                    if !text.trim().is_empty() {
                        steering.enqueue_steering(text);
                    }
                    true
                }
                code => apply_editor_key(screen, code, ctrl, alt),
            }
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
    use crate::nexus::SteeringSource;
    use crate::ui::tui::Screen;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn debug_snapshot_contents_carry_size_rendered_lines_and_messages() {
        let rendered = vec!["[0] (w=2) \"hi\"".to_string(), "[1] (w=0) \"\"".to_string()];
        let messages = vec![
            crate::nexus::Message::user("question"),
            crate::nexus::Message::assistant("answer"),
        ];
        let contents = debug_snapshot_contents(80, 24, &rendered, &messages);
        assert!(contents.contains("Iris "), "{contents}");
        assert!(contents.contains("Terminal: 80x24"), "{contents}");
        assert!(contents.contains("Total lines: 2"), "{contents}");
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
    fn ctrl_o_toggles_latest_panel_when_idle() {
        let mut screen = Screen::new();
        // Long output caps to a preview, so the panel is foldable and ctrl+o
        // reveals it.
        let content = (0..20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.apply(UiEvent::ToolResult {
            call: call(),
            content,
            exit_code: None,
            duration: None,
        });
        // Capped output starts collapsed (preview).
        assert!(screen.latest_panel_collapsed());

        assert!(matches!(
            handle_idle_event(
                &mut screen,
                key_mod(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            IdleKey::Continue
        ));
        // ctrl+o reveals the full output.
        assert!(!screen.latest_panel_collapsed());
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
        // Running phase: losing focus needs no redraw, regaining focus redraws
        // once (the frozen animation catches up), repeats are no-ops.
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
        assert!(!screen.tick(), "unfocused pane stops animating");
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
        assert!(screen.tick(), "refocused pane animates again");

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
    fn page_keys_do_not_consume_a_pending_approval() {
        let mut screen = Screen::new();
        let steering = SteeringQueue::default();
        let (tx, _rx) = oneshot::channel();
        let mut pending = Some(PendingApproval {
            call: call(),
            reply: tx,
            allow_always: true,
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
        assert_eq!(to_modal_key(&key(KeyCode::Tab)), Some(ModalKey::Tab));
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
}
