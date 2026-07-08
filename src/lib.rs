use std::cell::RefCell;
use std::env;
use std::io::{IsTerminal, Write};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::Duration;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use nexus::{Agent, ChatProvider};
use reqwest::blocking::Client;
use tokio_util::sync::CancellationToken;

mod approval;
mod cli;
mod config;
mod errors;
mod git;
mod handles;
mod mimir;
mod nexus;
mod print;
mod process_group;
mod selfupdate;
mod session;
mod signals;
mod telemetry;
mod tool_display;
mod tool_summary;
mod tools;
mod ui;
mod wayland;

pub mod harness;

/// Binary entry point, exposed on the library so the thin `src/main.rs` shim
/// (and other in-workspace crates) can invoke the full CLI without duplicating
/// module wiring. See ADR: iris-agent lib/bin split for the benchmark harness.
pub fn run_cli() -> ExitCode {
    telemetry::init();
    signals::install();
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            if let Some(auth) = error.downcast_ref::<errors::AuthError>() {
                // Prefer the provider that actually failed (carried on the typed
                // error) so the hint never points at the wrong provider; fall
                // back to the configured default when it is unknown.
                let provider = auth
                    .provider()
                    .map(str::to_string)
                    .unwrap_or_else(configured_provider);
                eprintln!(
                    "hint: run `{}` login {provider} to authenticate",
                    command_name(),
                );
            }
            ExitCode::from(errors::exit_code(&error))
        }
    }
}

fn dispatch() -> Result<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    // `--no-alt-screen` (ADR-0029) is positional-agnostic: strip it before the
    // command table so every entry point honors it, and record it for the
    // screen-mode resolver.
    let before = args.len();
    args.retain(|arg| arg != "--no-alt-screen");
    if args.len() != before {
        ui::screen_mode::set_no_alt_screen_cli();
    }
    // `--dangerously-skip-permissions` (ADR-0049) is positional-agnostic and
    // CLI-ONLY: strip it here so every session entry point honors it, and pass
    // the resulting bool explicitly down to Nexus. It is deliberately not read
    // from any config file, project file, trust store, or env var, so a
    // repository can never grant itself approval.
    let before = args.len();
    args.retain(|arg| arg != "--dangerously-skip-permissions");
    let skip_permissions = args.len() != before;
    // Headless `--print` mode is detected before the command table so `-p`/
    // `--print` (with an optional `--approve` in any position) dispatches to the
    // one-shot runner; a malformed print invocation falls through to the usage
    // error below.
    if let Some(invocation) = print::parse_print_args(&args) {
        return run_print(&invocation.prompt, invocation.approve, skip_permissions);
    }
    match args.as_slice() {
        [] => run_agent(false, skip_permissions),
        [flag] if flag == "--plain" => run_agent(true, skip_permissions),
        // `-c`/`--continue` resumes the newest session for the cwd; parsed like
        // the other bare flags, with an optional trailing `--plain`.
        [flag] if is_continue(flag) => continue_agent(false, skip_permissions),
        [flag, plain] if is_continue(flag) && plain == "--plain" => {
            continue_agent(true, skip_permissions)
        }
        // `resume` with a trailing `--plain` (and no id) prints the plain list;
        // this must precede the `resume <id>` arm so `--plain` is not read as an
        // id.
        [command, plain] if command == "resume" && plain == "--plain" => {
            resume_pick(true, skip_permissions)
        }
        // `resume` with no id: pick a session (picker on a rich TTY, plain list
        // otherwise).
        [command] if command == "resume" => resume_pick(false, skip_permissions),
        [command, session_id] if command == "resume" => {
            resume_agent(session_id, false, skip_permissions)
        }
        [command, session_id, flag] if command == "resume" && flag == "--plain" => {
            resume_agent(session_id, true, skip_permissions)
        }
        [command, provider] if command == "login" && provider == "openai-codex" => {
            login_openai_codex(select_openai_codex_method()?)
        }
        [command, provider] if command == "login" && provider == "openai" => {
            login_api_key(mimir::selection::ProviderId::OpenAi)
        }
        [command, provider] if command == "login" && provider == "openai-compatible" => {
            login_api_key(mimir::selection::ProviderId::OpenAiCompatible)
        }
        [command, provider] if command == "login" && provider == "antigravity" => {
            login_antigravity()
        }
        [command, provider] if command == "login" && provider == "anthropic" => login_anthropic(),
        [command, provider, flag]
            if command == "login" && provider == "openai-codex" && flag == "--browser" =>
        {
            login_openai_codex(LoginMethod::Browser)
        }
        [command, provider, flag]
            if command == "login" && provider == "openai-codex" && flag == "--device-code" =>
        {
            login_openai_codex(LoginMethod::DeviceCode)
        }
        [command, provider, flag]
            if command == "login" && provider == "anthropic" && flag == "--api-key" =>
        {
            login_api_key(mimir::selection::ProviderId::Anthropic)
        }
        [command] if command == "update" => update_agent(),
        [command] if command == "help" || command == "--help" || command == "-h" => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
            Err(errors::UsageError::new("unknown command").into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginMethod {
    Browser,
    DeviceCode,
}

/// The `openai-codex` login methods offered by the interactive menu, in display
/// order. The first entry is the non-interactive default.
const OPENAI_CODEX_METHODS: [(LoginMethod, &str, &str); 2] = [
    (
        LoginMethod::Browser,
        "Browser",
        "opens a login page and waits for the callback",
    ),
    (
        LoginMethod::DeviceCode,
        "Device code",
        "authorize on another device with a short code",
    ),
];

/// A key press mapped to a menu action. Kept separate from the event loop so
/// the key mapping is unit-testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    Up,
    Down,
    Confirm,
    Cancel,
    Ignore,
}

/// Map a key press to a [`MenuAction`]. Arrows and `k`/`j` move, Enter confirms,
/// Esc and Ctrl-C cancel; everything else is ignored.
fn menu_action(event: &ratatui::crossterm::event::KeyEvent) -> MenuAction {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    match event.code {
        KeyCode::Up | KeyCode::Char('k') => MenuAction::Up,
        KeyCode::Down | KeyCode::Char('j') => MenuAction::Down,
        KeyCode::Enter => MenuAction::Confirm,
        KeyCode::Esc => MenuAction::Cancel,
        KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => MenuAction::Cancel,
        _ => MenuAction::Ignore,
    }
}

/// Next index with wrap-around; `len` is assumed non-zero.
fn menu_next(index: usize, len: usize) -> usize {
    (index + 1) % len
}

/// Previous index with wrap-around; `len` is assumed non-zero.
fn menu_prev(index: usize, len: usize) -> usize {
    (index + len - 1) % len
}

/// Choose the `openai-codex` login method. On a TTY this shows an interactive
/// menu; otherwise (piped/non-interactive stdin) it falls back to the default
/// first method so scripts keep working.
fn select_openai_codex_method() -> Result<LoginMethod> {
    if !std::io::stdin().is_terminal() {
        return Ok(OPENAI_CODEX_METHODS[0].0);
    }
    prompt_openai_codex_method()
}

/// Render the method menu, moving the cursor back to the top of the list on
/// each redraw so navigation updates in place.
fn draw_method_menu(out: &mut impl Write, index: usize, redraw: bool) -> Result<()> {
    use ratatui::crossterm::{cursor, execute, terminal};
    if redraw {
        execute!(out, cursor::MoveUp(OPENAI_CODEX_METHODS.len() as u16))?;
    }
    for (i, (_, label, hint)) in OPENAI_CODEX_METHODS.iter().enumerate() {
        let marker = if i == index { '>' } else { ' ' };
        execute!(
            out,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::CurrentLine)
        )?;
        write!(out, "{marker} {label}  ({hint})\r\n")?;
    }
    out.flush()?;
    Ok(())
}

/// Interactive `openai-codex` method picker. Uses the same raw-mode approach as
/// [`read_api_key`]; Esc / Ctrl-C cancel with an error.
fn prompt_openai_codex_method() -> Result<LoginMethod> {
    use ratatui::crossterm::event;

    println!("How do you want to log in to OpenAI Codex?");
    println!("  (up/down to move, Enter to confirm, Esc to cancel)");

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }
    ratatui::crossterm::terminal::enable_raw_mode()?;
    let guard = RawModeGuard;

    let mut out = std::io::stdout();
    let mut index = 0usize;
    draw_method_menu(&mut out, index, false)?;

    let result: Result<LoginMethod> = loop {
        match event::read()? {
            event::Event::Key(ev) if ev.kind == event::KeyEventKind::Press => {
                match menu_action(&ev) {
                    MenuAction::Up => index = menu_prev(index, OPENAI_CODEX_METHODS.len()),
                    MenuAction::Down => index = menu_next(index, OPENAI_CODEX_METHODS.len()),
                    MenuAction::Confirm => break Ok(OPENAI_CODEX_METHODS[index].0),
                    MenuAction::Cancel => break Err(anyhow!("login cancelled")),
                    MenuAction::Ignore => continue,
                }
                draw_method_menu(&mut out, index, true)?;
            }
            _ => {}
        }
    };
    drop(guard);
    println!();
    result
}

/// Whether a bare flag is the resume-newest shorthand (`-c` / `--continue`).
fn is_continue(flag: &str) -> bool {
    matches!(flag, "-c" | "--continue")
}

/// Provider used when the settings file selects none. Stays `openai-codex` for
/// backward compatibility; `anthropic` and `antigravity` are opt-in via
/// `defaultProvider` in settings.
const DEFAULT_PROVIDER: &str = "openai-codex";

/// Best-effort provider id for the auth re-login hint, so an Anthropic or
/// Antigravity auth failure does not tell the user to log into OpenAI. Reads
/// `defaultProvider` from settings; falls back to the default when settings
/// cannot be read (the hint is advisory, never fatal).
fn configured_provider() -> String {
    env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())
        .and_then(|settings| settings.default_provider)
        .map(|provider| provider.trim().to_string())
        .filter(|provider| !provider.is_empty())
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string())
}

fn run_agent(force_plain: bool, skip_permissions: bool) -> Result<()> {
    run_agent_inner(force_plain, None, skip_permissions)
}

/// `iris --continue` / `iris -c`: resume the newest session for the current
/// directory. A clear usage error when the directory has no prior session; on
/// success it reuses the standard [`resume_agent`] path.
fn continue_agent(force_plain: bool, skip_permissions: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let store = session::SessionStore::open_default()?;
    let metas = store.list()?;
    let id = session::newest_for_cwd(&metas, &cwd.to_string_lossy())
        .map(|meta| meta.id.clone())
        .ok_or_else(|| {
            errors::UsageError::new(
                "no prior session found for this directory; run `iris` to start one",
            )
        })?;
    resume_agent(&id, force_plain, skip_permissions)
}

/// `iris resume` with no id. On a plain/non-TTY front-end, print the resumable
/// session list (id, age, preview) for this directory and exit 0. On a rich TTY,
/// start a session with the `/resume` picker open so the user selects one
/// (cancelling leaves them in the fresh session).
fn resume_pick(force_plain: bool, skip_permissions: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let store = session::SessionStore::open_default()?;
    let sessions = store.resumable_for_cwd(&cwd.to_string_lossy())?;
    if cli::prefers_text_ui(force_plain) {
        print_session_list(&sessions, session::current_ms());
        return Ok(());
    }
    // `open_resume` returns `None` when there is nothing to resume; the session
    // then simply starts fresh with no picker.
    let startup_modal = ui::picker::open_resume(&cwd);
    run_agent_inner(force_plain, startup_modal, skip_permissions)
}

/// Print the resumable-session list for the plain/non-TTY `iris resume` path.
fn print_session_list(sessions: &[session::ResumableSession], now_ms: u128) {
    if sessions.is_empty() {
        println!("No prior sessions to resume for this directory.");
        return;
    }
    println!("Resumable sessions for this directory (newest first):");
    for session in sessions {
        println!(
            "  {}  {:>8}  {}",
            session.meta.id,
            session::relative_age(now_ms, session.meta.updated_ms),
            session.preview,
        );
    }
    println!();
    println!("Resume one with: iris resume <session-id>");
}

fn run_agent_inner(
    force_plain: bool,
    startup_modal: Option<ui::modal::Modal>,
    skip_permissions: bool,
) -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    // Harness-owned assembly: the fragment/slot baukasten composes the prompt
    // from the in-binary shipped fragments plus dynamic context (project docs,
    // date, cwd) and the live tool registry (ADR-0026). Fresh and resume call
    // the same function.
    let tools = tools::built_in_tools_for(settings.bash_tool_mode());
    let system_prompt = wayland::system_prompt::assemble(&cwd, &tools);
    // One resolution point owns provider/model/reasoning precedence; capability
    // validation then rejects a configured reasoning level the model cannot do.
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = session::new_session_id();
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    let agent = Agent::new(provider, tools)
        .with_max_tool_roundtrips(settings.max_tool_roundtrips())
        .with_project_policy(project_policy(&cwd), project_policy_sink(&cwd))
        .with_skip_permissions(skip_permissions);
    // Transcript persistence is best-effort: if the log cannot be opened (e.g.
    // no writable session dir), warn and continue in-memory rather than fail.
    let mut session = match session::SessionLog::create_with_id(&cwd, &session_id) {
        Ok(log) => {
            tracing::info!(id = %log.id(), path = %log.path().display(), "session transcript");
            Some(log)
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "session persistence disabled");
            None
        }
    };
    // ADR-0049: loud one-time warning + a transcript audit record so a resumed
    // or audited session shows the mode was active.
    announce_skip_permissions(skip_permissions, session.as_mut());
    // Resume foundation: surface prior persisted sessions for this workspace.
    // The /resume UI is a later milestone; this only proves the store reads
    // back and signals that persistence is durable and resumable.
    log_resumable_sessions(&cwd);
    // The Tier-2 harness owns the execution surface (workspace + tool state),
    // persistence, and the auto-compaction policy, wrapping the bare in-memory
    // agent. When the context token total exceeds the budget at a turn
    // boundary, the harness compacts before the provider request.
    let budget = Some(settings.context_token_budget());
    let mut harness =
        wayland::Harness::new(agent, cwd.clone(), tools::ToolState::new(), session, budget);
    // Post-change verification (issue #265): engaged only when a `verify` block
    // is present; the command runs under the unchanged approval gate.
    harness.set_verification(settings.verification());
    harness.set_summarizer(settings.compaction_summarizer());
    // Opt-in microcompaction (ADR-0048, #378): fold spent tool results when on.
    harness.set_microcompaction(settings.microcompaction());
    // Prompt-cache profile + selection identity for the fold scheduler
    // (issue #400): resolved here so wayland consumes only profile fields.
    harness.set_cache_profile(mimir::selection::cache_profile(&selection));
    harness.note_active_selection(
        selection.provider.as_str(),
        &selection.model,
        selection
            .reasoning
            .map(mimir::selection::ReasoningEffort::as_str),
    );
    // Startup approval posture (ADR-0032): apply the GLOBAL-ONLY `defaultApproval`
    // preference; absent/invalid leaves the built-in default (`strict`). The live
    // `/approval` command stays session-only and is unaffected.
    harness.set_approval_mode(nexus::ApprovalMode::from_startup_setting(
        settings.default_approval.as_deref(),
    ));
    // Tier-3 mode-switch state: `/model` `/reasoning` rebuild a provider from the
    // same system prompt via `build_provider` and install it at a turn boundary.
    // The session id lives in a shared cell so an in-session `/resume` `/new`
    // swap can point the provider builder at the swapped session's id.
    let session_cell = Rc::new(RefCell::new(session_id.clone()));
    let build_cell = session_cell.clone();
    let build = move |selection: &mimir::selection::ModelSelection, prompt: &str| {
        build_provider(selection, prompt, &build_cell.borrow())
    };
    let mut switch = Some(cli::ModelSwitch::new(
        selection,
        system_prompt,
        &build,
        settings.enabled_models.clone(),
    ));
    let swap_cwd = cwd.clone();
    let swap =
        move |source: &cli::SessionSource| load_session_source(&swap_cwd, &session_cell, source);
    // The start page (IrisMark + launcher) shows only when Iris launches
    // interactively with no task and no resume target; a bare `iris resume`
    // opens the resume picker instead.
    let start_page = startup_modal.is_none();
    cli::run_interactive(
        &mut harness,
        &mut switch,
        force_plain,
        settings.tui_settings(),
        &swap,
        startup_modal,
        start_page,
    )
}

/// Load the transcript state for an in-session `/resume` `/new` swap and point
/// the shared session-id cell at the swapped session, so the subsequent provider
/// rebuild keys to it. `Fresh` opens a brand-new transcript; `Resume` reopens a
/// persisted session by id (a clear error when the id is unknown). Log open
/// failures degrade to in-memory persistence, like a normal start.
fn load_session_source(
    cwd: &Path,
    cell: &Rc<RefCell<String>>,
    source: &cli::SessionSource,
) -> Result<cli::LoadedSource> {
    match source {
        cli::SessionSource::Fresh => {
            let id = session::new_session_id();
            let session_log = match session::SessionLog::create_with_id(cwd, &id) {
                Ok(log) => Some(log),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "new-session persistence disabled");
                    None
                }
            };
            Ok(cli::LoadedSource {
                session_id: cli::SessionIdGuard::swap(cell.clone(), id),
                session_log,
                messages: Vec::new(),
                entry_ids: Vec::new(),
                resumed: 0,
                skip_permissions: false,
            })
        }
        cli::SessionSource::Resume(id) => {
            let store = session::SessionStore::open_default()?;
            let meta = store.find(id)?.ok_or_else(|| {
                errors::UsageError::new(format!("no session found with id '{id}'"))
            })?;
            let stored = store.open(&meta)?;
            let resumed = stored.messages.len();
            let entry_ids = stored.entry_ids;
            let session_log = match session::SessionLog::resume(&meta.path) {
                Ok(log) => Some(log),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "resume persistence disabled");
                    None
                }
            };
            Ok(cli::LoadedSource {
                session_id: cli::SessionIdGuard::swap(cell.clone(), meta.id.clone()),
                session_log,
                messages: stored.messages,
                entry_ids,
                resumed,
                skip_permissions: stored.dangerous_skip_permissions,
            })
        }
    }
}

/// Run one headless turn-sequence for `iris -p`/`--print`: assemble the prompt
/// (merging any piped stdin), run the model loop with tool roundtrips, print the
/// final assistant answer to stdout, and return an error (nonzero exit) on
/// failure. Non-interactive throughout: its approval gate denies gated tools
/// by default, or auto-approves with `--approve`, so a piped/CI run cannot hang.
/// The session is persisted like a normal run.
fn run_print(prompt_arg: &str, approve: bool, skip_permissions: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    let tools = tools::built_in_tools_for(settings.bash_tool_mode());
    let system_prompt = wayland::system_prompt::assemble(&cwd, &tools);
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = session::new_session_id();
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    // The persisted project policy applies headless too (a granted tool/command
    // auto-approves), but the print gate cannot mint new grants, so no sink.
    let agent = Agent::new(provider, tools)
        .with_max_tool_roundtrips(settings.max_tool_roundtrips())
        .with_project_policy(project_policy(&cwd), None)
        .with_skip_permissions(skip_permissions);
    // Persist the print run's transcript like a normal run; best-effort.
    let mut session = match session::SessionLog::create_with_id(&cwd, &session_id) {
        Ok(log) => {
            tracing::info!(id = %log.id(), path = %log.path().display(), "session transcript");
            Some(log)
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "session persistence disabled");
            None
        }
    };
    announce_skip_permissions(skip_permissions, session.as_mut());
    let budget = Some(settings.context_token_budget());
    let mut harness =
        wayland::Harness::new(agent, cwd.clone(), tools::ToolState::new(), session, budget);
    harness.set_verification(settings.verification());
    harness.set_summarizer(settings.compaction_summarizer());
    // Opt-in microcompaction (ADR-0048, #378): fold spent tool results when on.
    harness.set_microcompaction(settings.microcompaction());
    // Prompt-cache profile + selection identity for the fold scheduler
    // (issue #400): resolved here so wayland consumes only profile fields.
    harness.set_cache_profile(mimir::selection::cache_profile(&selection));
    harness.note_active_selection(
        selection.provider.as_str(),
        &selection.model,
        selection
            .reasoning
            .map(mimir::selection::ReasoningEffort::as_str),
    );

    // Merge piped stdin (when not a TTY) into the prompt before the turn.
    let piped = print::read_piped_stdin()?;
    let prompt = print::merge_prompt(prompt_arg, piped.as_deref());

    let observer = print::PrintObserver::default();
    let gate = print::PrintApprovalGate::new(approve);
    cli::run_print_turn(&mut harness, &prompt, &observer, &gate)?;

    // Only the final assistant answer reaches stdout; everything else is
    // suppressed by the observer.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{}", observer.final_text())?;
    stdout.flush()?;
    Ok(())
}

/// Session-start side effects of `--dangerously-skip-permissions` (ADR-0049):
/// print the loud one-time warning banner to stderr, and record the mode as a
/// transcript metadata entry so a resumed/audited session shows it was active.
/// A no-op when the flag is off, so a normal session is byte-identical to
/// today. Best-effort like all transcript persistence: a failed append is
/// warned, never fatal.
fn announce_skip_permissions(skip_permissions: bool, session: Option<&mut session::SessionLog>) {
    if !skip_permissions {
        return;
    }
    eprintln!("WARNING: {}", nexus::SKIP_PERMISSIONS_BANNER);
    if let Some(log) = session
        && let Err(error) = log.append_dangerous_mode()
    {
        tracing::warn!(error = %format!("{error:#}"), "failed to record skip-permissions mode");
    }
}

/// The persisted per-project permission policy for `cwd` (ADR-0027), loaded
/// from the HOME-owned store into the enforcement-layer shape Nexus consumes.
fn project_policy(cwd: &Path) -> nexus::ProjectPolicy {
    wayland::trust::policy_for(cwd).to_policy()
}

/// The persistence sink new project grants are written through.
fn project_policy_sink(cwd: &Path) -> Option<Box<dyn nexus::ProjectPolicySink>> {
    Some(Box::new(wayland::trust::PolicyStoreSink::new(
        cwd.to_path_buf(),
    )))
}

/// Resume an existing session by id: load its transcript from the store,
/// reconstruct the provider-visible messages, seed the agent with them, and
/// continue appending future turns to the same log. Errors clearly when the id
/// is unknown or the session cannot be read.
fn resume_agent(session_id: &str, force_plain: bool, skip_permissions: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let store = session::SessionStore::open_default()?;
    let meta = store.find(session_id)?.ok_or_else(|| {
        errors::UsageError::new(format!(
            "no session found with id '{session_id}'; run with no arguments to start a new session"
        ))
    })?;
    let stored = store.open(&meta)?;
    let resumed = stored.messages.len();
    // Durable message ids parallel to the loaded messages (#377): thread them
    // into the harness so a near-budget startup-resumed prefix stays compactable
    // by auto-compaction and `/compact`, instead of being seeded id-less.
    let entry_ids = stored.entry_ids;
    // The rebuilt context's token total from the reconstruction path -- the same
    // number the live session reports via `session::context_tokens`, so it is
    // stable across resume. The harness compares it against the budget at the
    // next turn boundary.
    let context_tokens = stored.context_tokens;
    let skip_permissions = skip_permissions || stored.dangerous_skip_permissions;

    let settings = config::Settings::load(&cwd)?;
    let budget = Some(settings.context_token_budget());
    // Resume assembles instructions through the same harness-owned baukasten as
    // a fresh session, so a resumed turn gets identical fragment/context output.
    let tools = tools::built_in_tools_for(settings.bash_tool_mode());
    let system_prompt = wayland::system_prompt::assemble(&cwd, &tools);
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = meta.id.clone();
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    let agent = Agent::resumed(provider, tools, stored.messages)
        .with_max_tool_roundtrips(settings.max_tool_roundtrips())
        .with_project_policy(project_policy(&cwd), project_policy_sink(&cwd))
        .with_skip_permissions(skip_permissions);

    // Reopen the same transcript for append so continued turns extend it rather
    // than starting a new file. Best-effort, like new-session persistence: if
    // the reopen fails, warn and continue in-memory.
    let mut session = match session::SessionLog::resume(&meta.path) {
        Ok(log) => Some(log),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "resume persistence disabled");
            None
        }
    };
    announce_skip_permissions(skip_permissions, session.as_mut());
    tracing::info!(id = %meta.id, messages = resumed, context_tokens, "resumed session");

    let mut harness = wayland::Harness::resumed(
        agent,
        cwd.clone(),
        tools::ToolState::new(),
        session,
        entry_ids,
        budget,
    );
    harness.set_verification(settings.verification());
    harness.set_summarizer(settings.compaction_summarizer());
    // Opt-in microcompaction (ADR-0048, #378): fold spent tool results when on.
    harness.set_microcompaction(settings.microcompaction());
    // Prompt-cache profile + selection identity for the fold scheduler
    // (issue #400): resolved here so wayland consumes only profile fields.
    harness.set_cache_profile(mimir::selection::cache_profile(&selection));
    harness.note_active_selection(
        selection.provider.as_str(),
        &selection.model,
        selection
            .reasoning
            .map(mimir::selection::ReasoningEffort::as_str),
    );
    // Startup approval posture (ADR-0032): apply the GLOBAL-ONLY `defaultApproval`
    // preference; absent/invalid leaves the built-in default (`strict`).
    harness.set_approval_mode(nexus::ApprovalMode::from_startup_setting(
        settings.default_approval.as_deref(),
    ));
    let session_cell = Rc::new(RefCell::new(session_id.clone()));
    let build_cell = session_cell.clone();
    let build = move |selection: &mimir::selection::ModelSelection, prompt: &str| {
        build_provider(selection, prompt, &build_cell.borrow())
    };
    let mut switch = Some(cli::ModelSwitch::new(
        selection,
        system_prompt,
        &build,
        settings.enabled_models.clone(),
    ));
    let swap_cwd = cwd.clone();
    let swap =
        move |source: &cli::SessionSource| load_session_source(&swap_cwd, &session_cell, source);
    cli::run_interactive(
        &mut harness,
        &mut switch,
        force_plain,
        settings.tui_settings(),
        &swap,
        None,
        false,
    )
}

/// Log the most recent prior session for `cwd` (if any) via the read side of
/// the session store. Best-effort and invisible by default: a read failure is
/// debug-logged, never fatal. This is the seam the future `/resume` command
/// will build on.
fn log_resumable_sessions(cwd: &Path) {
    let store = match session::SessionStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "session store unavailable");
            return;
        }
    };
    let metas = match store.list() {
        Ok(metas) => metas,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "could not list prior sessions");
            return;
        }
    };
    let cwd_str = cwd.to_string_lossy();
    // list() is newest-first, so the first match is the latest session here.
    let mut here = metas.into_iter().filter(|meta| meta.cwd == cwd_str);
    let Some(latest) = here.next() else {
        return;
    };
    let also = here.count();
    match store.open(&latest) {
        Ok(prior) => tracing::info!(
            id = %prior.meta.id,
            created_ms = prior.meta.created_ms,
            updated_ms = prior.meta.updated_ms,
            messages = prior.messages.len(),
            also_resumable = also,
            "prior session available for this workspace"
        ),
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "could not read latest prior session")
        }
    }
}

/// Build the selected provider as a boxed trait object so a single
/// `Agent<Box<dyn ChatProvider>>` can back any provider chosen at runtime.
/// Precedence and the unsupported-provider error now live in `mimir::selection`
/// (`ModelSelection::resolve`), so this only maps the resolved [`ProviderId`] to
/// its concrete adapter. Reused at startup and on every `/model` `/reasoning`
/// switch (rebuilds with the new selection + the same system prompt).
fn build_provider(
    selection: &mimir::selection::ModelSelection,
    system_prompt: &str,
    session_id: &str,
) -> Result<Box<dyn ChatProvider>> {
    use mimir::selection::ProviderId;
    let model = selection.model.as_str();
    let base_url = selection.base_url.as_str();
    let reasoning = selection.reasoning;
    let provider: Box<dyn ChatProvider> = match selection.provider {
        ProviderId::OpenAiCodex => Box::new(
            mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
                model,
                base_url,
                reasoning,
                system_prompt,
                session_id,
                selection.cache_retention,
                selection.retry_policy,
            )?,
        ),
        ProviderId::OpenAi => {
            let auth = mimir::auth::storage::AuthStore::from_env()?;
            let api_key = mimir::auth::api_key::api_key_for_provider(ProviderId::OpenAi, &auth)?;
            Box::new(
                mimir::providers::openai_compatible_chat::OpenAiCompatibleChatProvider::new(
                    mimir::providers::openai_compatible_chat::OpenAiCompatibleChatConfig {
                        provider: ProviderId::OpenAi,
                        model,
                        base_url,
                        reasoning,
                        system_prompt,
                        api_key,
                        supports_reasoning: true,
                        api_key_required: true,
                        retry_policy: selection.retry_policy,
                    },
                )?,
            )
        }
        ProviderId::Anthropic => Box::new(
            mimir::providers::anthropic_messages::AnthropicProvider::new(
                model,
                base_url,
                reasoning,
                system_prompt,
                selection.cache_retention,
                selection.context_management.clone(),
                selection.retry_policy,
            )?,
        ),
        ProviderId::Antigravity => {
            Box::new(mimir::providers::antigravity::AntigravityProvider::new(
                model,
                base_url,
                reasoning,
                system_prompt,
            )?)
        }
        ProviderId::OpenAiCompatible => {
            let auth = mimir::auth::storage::AuthStore::from_env()?;
            let api_key =
                mimir::auth::api_key::api_key_for_provider(ProviderId::OpenAiCompatible, &auth)?;
            Box::new(
                mimir::providers::openai_compatible_chat::OpenAiCompatibleChatProvider::new(
                    mimir::providers::openai_compatible_chat::OpenAiCompatibleChatConfig {
                        provider: ProviderId::OpenAiCompatible,
                        model,
                        base_url,
                        reasoning,
                        system_prompt,
                        api_key,
                        supports_reasoning: selection.open_ai_compatible.reasoning,
                        api_key_required: selection.open_ai_compatible.api_key_required,
                        retry_policy: selection.retry_policy,
                    },
                )?,
            )
        }
    };
    Ok(provider)
}

const COMMAND_NAME: &str = "iris";
const UPDATE_REPO: &str = "https://github.com/5omeOtherGuy/iris-agent.git";
const UPDATE_PACKAGE: &str = "iris-agent";
const UPDATE_ARGS: &[&str] = &[
    "install",
    "--git",
    UPDATE_REPO,
    UPDATE_PACKAGE,
    "--locked",
    "--force",
];

fn command_name() -> &'static str {
    COMMAND_NAME
}

fn update_args() -> &'static [&'static str] {
    UPDATE_ARGS
}

fn update_agent() -> Result<()> {
    match selfupdate::update_strategy() {
        selfupdate::UpdateStrategy::SelfReplace => update_self_replace(),
        selfupdate::UpdateStrategy::CargoInstall => update_via_cargo(),
    }
}

/// Download-and-replace path, compiled into prebuilt release binaries.
#[cfg(feature = "self-update")]
fn update_self_replace() -> Result<()> {
    selfupdate::run()
}

/// Unreachable in source builds: `update_strategy()` only returns `SelfReplace`
/// when the `self-update` feature is on, which also compiles `selfupdate::run`.
#[cfg(not(feature = "self-update"))]
fn update_self_replace() -> Result<()> {
    update_via_cargo()
}

fn update_via_cargo() -> Result<()> {
    println!("Updating Iris from {UPDATE_REPO} ...");
    let status = Command::new("cargo")
        .args(update_args())
        .status()
        .context("failed to run cargo; install Rust/Cargo or update with cargo install manually")?;
    if !status.success() {
        bail!("cargo install failed with {status}");
    }
    Ok(())
}

fn login_openai_codex(method: LoginMethod) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let cancel = CancellationToken::new();

    match method {
        LoginMethod::Browser => {
            mimir::auth::openai_codex::login_browser(&client, &cancel, |auth| {
                println!("OpenAI Codex browser login");
                println!("Open: {}", auth.url);
                crate::ui::login::open_in_browser(&auth.url);
                println!("Waiting for callback at {} ...", auth.redirect_uri);
            })?
        }
        LoginMethod::DeviceCode => mimir::auth::openai_codex::login_device_code(&client, |code| {
            println!("OpenAI Codex device-code login");
            println!("Open: {}", code.verification_uri);
            crate::ui::login::open_in_browser(&code.verification_uri);
            println!("Code: {}", code.user_code);
            println!("Waiting for authorization...");
        })?,
    }

    println!("Logged in to openai-codex.");
    Ok(())
}

fn login_antigravity() -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let cancel = CancellationToken::new();
    mimir::auth::antigravity::login_browser(&client, &cancel, |url| {
        println!("Antigravity (Google account) login");
        println!("Open: {url}");
        crate::ui::login::open_in_browser(url);
        println!("Waiting for callback...");
    })?;
    println!("Logged in to antigravity.");
    Ok(())
}

fn login_api_key(provider: mimir::selection::ProviderId) -> Result<()> {
    let key = read_api_key(provider)?;
    if key.is_empty() {
        bail!("API key is blank");
    }
    let auth = mimir::auth::storage::AuthStore::from_env()?;
    auth.set_api_key_credentials(provider.as_str(), &key)?;
    save_default_after_api_key_login(provider);
    println!("Stored API key for {}.", provider.display_name());
    Ok(())
}

fn read_api_key(provider: mimir::selection::ProviderId) -> Result<String> {
    print!("Enter API key for {}: ", provider.display_name());
    std::io::stdout().flush()?;
    if !std::io::stdin().is_terminal() {
        let mut key = String::new();
        std::io::stdin().read_line(&mut key)?;
        return Ok(key.trim().to_string());
    }

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }

    ratatui::crossterm::terminal::enable_raw_mode()?;
    let guard = RawModeGuard;
    let mut key = String::new();
    let result: Result<()> = loop {
        match ratatui::crossterm::event::read()? {
            ratatui::crossterm::event::Event::Key(event)
                if event.kind == ratatui::crossterm::event::KeyEventKind::Press =>
            {
                match event.code {
                    ratatui::crossterm::event::KeyCode::Enter => break Ok(()),
                    ratatui::crossterm::event::KeyCode::Backspace => {
                        key.pop();
                    }
                    ratatui::crossterm::event::KeyCode::Char('c')
                        if event
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        break Err(anyhow!("API key entry cancelled"));
                    }
                    ratatui::crossterm::event::KeyCode::Char(ch) => key.push(ch),
                    _ => {}
                }
            }
            ratatui::crossterm::event::Event::Paste(text) => key.push_str(&text),
            _ => {}
        }
    };
    drop(guard);
    println!();
    result?;
    Ok(key.trim().to_string())
}

fn save_default_after_api_key_login(provider: mimir::selection::ProviderId) {
    if !matches!(
        provider,
        mimir::selection::ProviderId::OpenAi | mimir::selection::ProviderId::Anthropic
    ) {
        return;
    }
    let already_default = env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())
        .and_then(|settings| settings.default_provider)
        .is_some_and(|default| default.trim() == provider.as_str());
    if !already_default {
        let _ = config::save_default_model(provider.as_str(), provider.default_model());
    }
}

fn login_anthropic() -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let cancel = CancellationToken::new();
    // A background stdin reader feeds a pasted authorization code / full redirect
    // URL to the callback wait, so login works even when the browser is on
    // another machine or the loopback callback cannot be received.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() && !line.trim().is_empty() {
            let _ = tx.send(line);
        }
    });
    mimir::auth::anthropic::login_browser(&client, &cancel, Some(&rx), |url| {
        println!("Anthropic browser login");
        println!("Open: {url}");
        crate::ui::login::open_in_browser(url);
        println!(
            "Waiting for the browser callback... or paste the authorization code / full redirect URL and press Enter."
        );
    })?;
    println!("Logged in to anthropic.");
    Ok(())
}

fn print_help() {
    eprintln!("Usage: iris [command] [options]");
    eprintln!();
    eprintln!("Sessions:");
    eprintln!("  iris                              Start the interactive agent");
    eprintln!("  iris -c, --continue               Resume the newest session for this directory");
    eprintln!("  iris resume [session-id]          Pick a session to resume, or resume one by id");
    eprintln!("    (in-session: /resume picks a session, /new starts a fresh one)");
    eprintln!();
    eprintln!("Print mode (run one turn, print the answer, exit):");
    eprintln!("  iris -p, --print \"prompt\"         Add --approve to auto-approve gated tools;");
    eprintln!(
        "                                   piped stdin is merged: cat log | iris -p \"explain\""
    );
    eprintln!();
    eprintln!("Login / update:");
    eprintln!("  iris login <provider>             openai-codex (default), openai,");
    eprintln!("                                   openai-compatible, antigravity, anthropic");
    eprintln!("                                   flags: --device-code (openai-codex),");
    eprintln!("                                          --api-key (anthropic)");
    eprintln!("  iris update                       Update Iris from GitHub");
    eprintln!();
    eprintln!("Danger:");
    eprintln!("  --dangerously-skip-permissions   DANGER: auto-approve EVERY tool call with no");
    eprintln!(
        "                                   approval prompt, INCLUDING destructive commands."
    );
    eprintln!("                                   Bypasses the safety floors; follows resumed");
    eprintln!("                                   sessions. Use only in a sandbox you trust.");
    eprintln!();
    eprintln!("Display (flag / env var):");
    eprintln!("  --plain, IRIS_PLAIN=1, NO_COLOR   Plain, ANSI-free text UI");
    eprintln!(
        "  --no-alt-screen, IRIS_NO_ALT_SCREEN=1  Run inline instead of the alternate screen"
    );
    eprintln!("  IRIS_REDUCED_MOTION=1             Freeze the working-indicator animation");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_name_is_iris() {
        assert_eq!(command_name(), "iris");
    }

    #[test]
    fn continue_flag_recognizes_short_and_long_forms_only() {
        assert!(is_continue("-c"));
        assert!(is_continue("--continue"));
        assert!(!is_continue("--plain"));
        assert!(!is_continue("resume"));
        assert!(!is_continue("-p"));
        assert!(!is_continue("continue"));
    }

    #[test]
    fn update_command_installs_locked_remote_with_force() {
        assert_eq!(
            UPDATE_REPO,
            "https://github.com/5omeOtherGuy/iris-agent.git"
        );
        assert_eq!(UPDATE_PACKAGE, "iris-agent");
        assert_eq!(
            update_args(),
            &[
                "install",
                "--git",
                UPDATE_REPO,
                UPDATE_PACKAGE,
                "--locked",
                "--force"
            ]
        );
    }

    #[test]
    fn openai_codex_menu_defaults_to_browser_first() {
        assert_eq!(OPENAI_CODEX_METHODS[0].0, LoginMethod::Browser);
    }

    #[test]
    fn menu_navigation_wraps_around() {
        let len = OPENAI_CODEX_METHODS.len();
        assert_eq!(menu_next(0, len), 1);
        assert_eq!(menu_next(len - 1, len), 0);
        assert_eq!(menu_prev(0, len), len - 1);
        assert_eq!(menu_prev(1, len), 0);
    }

    #[test]
    fn menu_action_maps_keys() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code| menu_action(&KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Up), MenuAction::Up);
        assert_eq!(key(KeyCode::Char('k')), MenuAction::Up);
        assert_eq!(key(KeyCode::Down), MenuAction::Down);
        assert_eq!(key(KeyCode::Char('j')), MenuAction::Down);
        assert_eq!(key(KeyCode::Enter), MenuAction::Confirm);
        assert_eq!(key(KeyCode::Esc), MenuAction::Cancel);
        assert_eq!(key(KeyCode::Char('x')), MenuAction::Ignore);
        assert_eq!(
            menu_action(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            MenuAction::Cancel
        );
    }
}
