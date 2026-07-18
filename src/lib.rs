use std::cell::RefCell;
use std::env;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use nexus::{Agent, ChatProvider};
use reqwest::blocking::Client;
use tokio_util::sync::CancellationToken;

mod approval;
mod cli;
mod config;
mod display_path;
mod errors;
mod git;
mod goal;
mod handles;
mod metrics;
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

#[cfg(test)]
mod goal_tests;
#[cfg(test)]
mod structured_summary_probe;

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
    // `--dangerously-skip-permissions` (ADR-0049) is positional-agnostic: strip
    // it here so every session entry point can opt into the dangerous permission
    // mode. Session startup persists that operator choice as the global default;
    // project config, trust stores, and env vars still cannot enable it.
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
        [command] if command == "version" || command == "--version" || command == "-V" => {
            println!("{}", version_line());
            Ok(())
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PermissionDefaults {
    approval_mode: nexus::ApprovalMode,
    skip_permissions: bool,
}

impl Default for PermissionDefaults {
    fn default() -> Self {
        Self {
            approval_mode: nexus::ApprovalMode::Strict,
            skip_permissions: false,
        }
    }
}

fn permission_defaults_from_mode(mode: nexus::PermissionMode) -> PermissionDefaults {
    match mode {
        nexus::PermissionMode::Approval(mode) => PermissionDefaults {
            approval_mode: mode,
            skip_permissions: false,
        },
        nexus::PermissionMode::DangerousSkipPermissions => PermissionDefaults {
            approval_mode: nexus::ApprovalMode::Strict,
            skip_permissions: true,
        },
    }
}

fn permission_defaults_from_setting(setting: Option<&str>) -> PermissionDefaults {
    permission_defaults_from_mode(nexus::PermissionMode::from_startup_setting(setting))
}

fn startup_permission_defaults(
    setting: Option<&str>,
    cli_skip_permissions: bool,
) -> PermissionDefaults {
    if cli_skip_permissions {
        permission_defaults_from_mode(nexus::PermissionMode::DangerousSkipPermissions)
    } else {
        permission_defaults_from_setting(setting)
    }
}

fn permission_defaults_for_cwd(cwd: &Path) -> PermissionDefaults {
    config::Settings::load(cwd)
        .map(|settings| permission_defaults_from_setting(settings.default_approval.as_deref()))
        .unwrap_or_default()
}

fn permission_token(skip_permissions: bool, approval_mode: nexus::ApprovalMode) -> &'static str {
    if skip_permissions {
        nexus::DANGEROUS_SKIP_PERMISSIONS_TOKEN
    } else {
        approval_mode.as_token()
    }
}

fn persist_default_permission(token: &str) {
    if let Err(error) = config::save_default_approval(token) {
        tracing::warn!(error = %format!("{error:#}"), "failed to save default approval mode");
    }
}

fn persist_cli_skip_permissions(cli_skip_permissions: bool) {
    if cli_skip_permissions
        && let Err(error) = config::save_default_approval(nexus::DANGEROUS_SKIP_PERMISSIONS_TOKEN)
    {
        eprintln!("warning: could not save default approval mode: {error:#}");
    }
}

/// Resolve the full tool-surface configuration from settings: bash-tool-mode,
/// the model-compaction tool, and the web-tool backends + keys. An unknown
/// web-backend value fails loudly here (matching `default_provider`).
fn resolve_tools_config(settings: &config::Settings) -> Result<tools::ToolsConfig> {
    Ok(tools::ToolsConfig {
        bash_tool_mode: settings.bash_tool_mode(),
        model_compaction_tool: settings.compaction_model_tool(),
        web: resolve_web_tools_config(settings)?,
        subagents: None,
    })
}

fn child_selection_for_request(
    parent: &mimir::selection::ModelSelection,
    request: &iris_subagent_runtime::WorkerRequest,
) -> Result<mimir::selection::ModelSelection> {
    match wayland::subagents::route_from_request(request)? {
        Some(route) => mimir::selection::selection_from_effective_route(
            parent,
            &route.provider,
            &route.model,
            &route.base_url,
            route.effort.as_deref(),
        ),
        None => Ok(parent.clone()),
    }
}

fn child_provider_factory(
    selection: Arc<Mutex<mimir::selection::ModelSelection>>,
    active_session_id: Arc<Mutex<String>>,
) -> wayland::subagents::ChildProviderFactory {
    Arc::new(move |request| {
        let parent = selection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let effective = child_selection_for_request(&parent, request)?;
        let session_id = active_session_id
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        build_provider(
            &effective,
            "You are an isolated delegated Iris worker. Follow the supplied instruction, use only the advertised tools, and report exact results without claiming parent-workspace mutations.",
            &session_id,
        )
    })
}

fn subagent_tools_config(
    cwd: &Path,
    session_id: &str,
    selection: Arc<Mutex<mimir::selection::ModelSelection>>,
    active_session_id: Arc<Mutex<String>>,
) -> Result<(
    tools::SubagentToolsConfig,
    Arc<wayland::subagents::SubagentBackend>,
)> {
    let backend = Arc::new(wayland::subagents::SubagentBackend::open(
        cwd.to_path_buf(),
        &wayland::subagents::resolve_worker_state_dir(session_id)?,
        wayland::subagents::resolve_worktree_root()?,
    )?);
    let provider_factory = child_provider_factory(selection.clone(), active_session_id);
    // Snapshot the authenticated catalog once so the spawn_subagent schema enum
    // and its pre-spawn resolution share one source of truth. A missing auth
    // store degrades to an empty catalog (model override then errors loudly).
    let catalog = {
        let settings = config::Settings::load(cwd).unwrap_or_default();
        match mimir::auth::storage::AuthStore::from_env() {
            Ok(auth) => mimir::model_catalog::available_models(&auth, &settings),
            Err(_) => Vec::new(),
        }
    };
    Ok((
        tools::SubagentToolsConfig {
            backend: backend.clone(),
            provider_factory,
            selection,
            catalog,
            capability_ceiling: iris_subagent_runtime::CapabilityMode::All,
            session_id: session_id.to_string(),
            nesting_depth: 0,
            max_nesting_depth: 2,
            approval: None,
        },
        backend,
    ))
}

/// Build the [`tools::web::WebToolsConfig`] from settings + the auth store.
/// Keys are resolved once here (store wins over env); the auth store is only
/// consulted when a backend is actually enabled, so a fully-off config never
/// touches the auth file. A missing HOME degrades to env-only key resolution
/// rather than failing startup.
fn resolve_web_tools_config(settings: &config::Settings) -> Result<tools::web::WebToolsConfig> {
    use mimir::auth::storage::{
        AuthStore, BRAVE_ENV_VAR, BRAVE_SERVICE_ID, JINA_ENV_VAR, JINA_SERVICE_ID,
    };

    let web_search = settings.web_search_backend()?;
    let read_web_page = settings.read_web_page_backend()?;
    if web_search.is_none() && read_web_page.is_none() {
        return Ok(tools::web::WebToolsConfig::default());
    }

    // Resolve the GLOBAL-ONLY bounds + endpoint (validated at their boundary).
    let bounds = settings.web_bounds()?;
    // Ignore an unused SearXNG endpoint. A stale typo must not disable another
    // selected backend; validate it only when query text would be sent there.
    let searxng_url = if web_search == Some(tools::web::SearchBackend::Searxng) {
        settings.searxng_url()?
    } else {
        None
    };
    // A SearXNG search backend has no default endpoint, so it needs a trusted
    // `searxngUrl`; fail loudly rather than register a tool that cannot run.
    if web_search == Some(tools::web::SearchBackend::Searxng) && searxng_url.is_none() {
        anyhow::bail!(
            "webSearchBackend is \"searxng\" but searxngUrl is not set; add a trusted searxngUrl to global settings"
        );
    }

    let auth = AuthStore::from_env().ok();
    let key = |service_id: &str, env_var: &str| -> Option<String> {
        match &auth {
            Some(store) => store.service_api_key(service_id, env_var),
            None => std::env::var(env_var)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        }
    };

    Ok(tools::web::WebToolsConfig {
        web_search,
        read_web_page,
        brave_key: key(BRAVE_SERVICE_ID, BRAVE_ENV_VAR),
        jina_key: key(JINA_SERVICE_ID, JINA_ENV_VAR),
        searxng_url,
        search_timeout: bounds.search_timeout,
        read_timeout: bounds.read_timeout,
        max_search_results: bounds.max_search_results,
        max_search_response_bytes: bounds.max_search_response_bytes,
        max_read_response_bytes: bounds.max_read_response_bytes,
        max_read_output_bytes: bounds.max_read_output_bytes,
    })
}

fn persist_session_permission_override(
    persisted_skip_permissions: Option<bool>,
    effective_skip_permissions: bool,
    approval_mode: nexus::ApprovalMode,
) {
    if persisted_skip_permissions.is_some() {
        persist_default_permission(permission_token(effective_skip_permissions, approval_mode));
    }
}

fn run_agent_inner(
    force_plain: bool,
    startup_modal: Option<ui::modal::Modal>,
    cli_skip_permissions: bool,
) -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    let permission_defaults =
        startup_permission_defaults(settings.default_approval.as_deref(), cli_skip_permissions);
    persist_cli_skip_permissions(cli_skip_permissions);
    // First-run onboarding: if no ~/.iris/AGENTS.md exists, discover peer-tool
    // instruction files and offer the user a choice. Must run before assemble()
    // so the newly written file is picked up by the project-doc discovery walk.
    wayland::system_prompt::onboarding::maybe_onboard();
    // One resolution point owns provider/model/reasoning precedence; capability
    // validation then rejects a configured reasoning level the model cannot do.
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = session::new_session_id();
    let background_selection = Arc::new(Mutex::new(selection.clone()));
    let background_session_id = Arc::new(Mutex::new(session_id.clone()));
    let (subagent_config, subagent_backend) = subagent_tools_config(
        &cwd,
        &session_id,
        background_selection.clone(),
        background_session_id.clone(),
    )?;
    let mut tools_config = resolve_tools_config(&settings)?;
    tools_config.subagents = Some(subagent_config);
    let tools = tools::built_in_tools_with(&tools_config);
    let prompt_assembly = wayland::system_prompt::assemble_with_notices(&cwd, &tools);
    let system_prompt = prompt_assembly.prompt;
    let mut startup_notices = prompt_assembly.notices;
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    let agent = Agent::new(provider, tools)
        .with_max_tool_roundtrips(settings.max_tool_roundtrips())
        .with_project_policy(project_policy(&cwd), project_policy_sink(&cwd))
        .with_skip_permissions(permission_defaults.skip_permissions);
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
    announce_skip_permissions(permission_defaults.skip_permissions, session.as_mut());
    // Resume foundation: surface prior persisted sessions for this workspace.
    // The /resume UI is a later milestone; this only proves the store reads
    // back and signals that persistence is durable and resumable.
    log_resumable_sessions(&cwd);
    // The Tier-2 harness owns the execution surface (workspace + tool state),
    // persistence, and the auto-compaction policy, wrapping the bare in-memory
    // agent. When the context token total exceeds the budget at a turn
    // boundary, the harness compacts before the provider request.
    let (context_budget, compaction_trigger) = resolved_compaction_trigger(&settings, &selection)?;
    let budget = Some(context_budget.hard_compaction_threshold);
    let native_jj = wayland::trust::native_jj(&cwd).unwrap_or(false);
    let mut harness = wayland::Harness::new_configured(
        agent,
        cwd.clone(),
        tools::ToolState::new(),
        session,
        budget,
        wayland::HarnessRuntimeConfig::new(wayland::MutationSafetyConfig {
            enabled: settings.mutation_safety(),
            native_jj,
        })
        .with_worker_runtime(subagent_backend.runtime().clone()),
    );
    harness.set_subagent_backend(subagent_backend.clone());
    harness.set_compaction_trigger(context_budget, compaction_trigger);
    // Post-change verification (issue #265): engaged only when a `verify` block
    // is present; the command runs under the unchanged approval gate.
    harness.set_verification(settings.verification());
    harness.set_summarizer(settings.compaction_summarizer());
    harness.set_compaction_worker(settings.compaction_worker_config()?);
    let provider_native_enabled =
        settings.compaction_provider_native()? == config::ProviderNativeMode::Auto;
    harness.set_provider_native(provider_native_enabled);
    install_compaction_summarizer_factory(
        &mut harness,
        background_selection.clone(),
        settings
            .compaction_worker_model()
            .map(|model| {
                mimir::selection::ModelSelection::resolve_compaction_worker(&settings, model)
            })
            .transpose()?,
        system_prompt.clone(),
        background_session_id.clone(),
    );
    harness.set_tool_result_compaction(selection.tool_result_compaction.clone());
    let _ = harness.set_task_workflow_enabled(settings.tasks());
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
    // Startup permission mode: apply the GLOBAL-ONLY `defaultApproval`
    // preference. A dangerous default enables skip-permissions; normal modes use
    // Nexus's approval preset and clear skip.
    harness.set_approval_mode(permission_defaults.approval_mode);
    // Tier-3 mode-switch state: `/model` `/reasoning` rebuild a provider from the
    // same system prompt via `build_provider` and install it at a turn boundary.
    // The session id lives in a shared cell so an in-session `/resume` `/new`
    // swap can point the provider builder at the swapped session's id.
    let session_cell = Rc::new(RefCell::new(session_id.clone()));
    let build_cell = session_cell.clone();
    let build = move |selection: &mimir::selection::ModelSelection, prompt: &str| {
        build_provider(selection, prompt, &build_cell.borrow())
    };
    let mut switch_state = cli::ModelSwitch::new(
        selection,
        system_prompt,
        &build,
        settings.enabled_models.clone(),
    );
    switch_state.set_background_selection_cell(background_selection);
    switch_state.set_compaction_settings(settings.clone());
    let mut switch = Some(switch_state);
    let swap_cwd = cwd.clone();
    let swap = move |source: &cli::SessionSource| {
        load_session_source(
            &swap_cwd,
            &session_cell,
            &background_session_id,
            permission_defaults_for_cwd(&swap_cwd),
            source,
        )
    };
    // The start page (IrisMark + launcher) shows only when Iris launches
    // interactively with no task and no resume target; a bare `iris resume`
    // opens the resume picker instead.
    let jj_modal = native_jj_discovery_modal(force_plain, &cwd, &settings, &harness);
    let (startup_modal, followup_modal) = match (startup_modal, jj_modal) {
        (Some(first), followup) => (Some(first), followup),
        (None, first) => (first, None),
    };
    let start_page = startup_modal.is_none();
    startup_notices.extend(cli::provider_native_compaction_notices(
        provider_native_enabled,
    ));
    cli::run_interactive(
        &mut harness,
        &mut switch,
        force_plain,
        settings.tui_settings(),
        &swap,
        cli::StartupUi {
            notices: startup_notices,
            modal: startup_modal,
            followup_modal,
            start_page,
            resumed_session: None,
        },
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
    background_cell: &Arc<Mutex<String>>,
    permission_defaults: PermissionDefaults,
    source: &cli::SessionSource,
) -> Result<cli::LoadedSource> {
    match source {
        cli::SessionSource::Fresh => {
            let id = session::new_session_id();
            let mut session_log = match session::SessionLog::create_with_id(cwd, &id) {
                Ok(log) => Some(log),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "new-session persistence disabled");
                    None
                }
            };
            record_skip_permissions(permission_defaults.skip_permissions, session_log.as_mut());
            Ok(cli::LoadedSource {
                session_id: cli::SessionIdGuard::swap_with_background(
                    cell.clone(),
                    background_cell.clone(),
                    id,
                ),
                session_log,
                messages: Vec::new(),
                entry_ids: Vec::new(),
                resumed: 0,
                approval_mode: permission_defaults.approval_mode,
                skip_permissions: permission_defaults.skip_permissions,
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
            let skip_permissions = session_skip_permissions(
                false,
                permission_defaults.skip_permissions,
                stored.dangerous_skip_permissions,
            );
            persist_session_permission_override(
                stored.dangerous_skip_permissions,
                skip_permissions,
                permission_defaults.approval_mode,
            );
            let mut session_log = match session::SessionLog::resume(&meta.path) {
                Ok(log) => Some(log),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "resume persistence disabled");
                    None
                }
            };
            record_skip_permissions(skip_permissions, session_log.as_mut());
            Ok(cli::LoadedSource {
                session_id: cli::SessionIdGuard::swap_with_background(
                    cell.clone(),
                    background_cell.clone(),
                    meta.id,
                ),
                session_log,
                messages: stored.messages,
                entry_ids,
                resumed,
                approval_mode: permission_defaults.approval_mode,
                skip_permissions,
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
    let permission_defaults =
        startup_permission_defaults(settings.default_approval.as_deref(), skip_permissions);
    persist_cli_skip_permissions(skip_permissions);
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = session::new_session_id();
    let background_selection = Arc::new(Mutex::new(selection.clone()));
    let background_session_id = Arc::new(Mutex::new(session_id.clone()));
    let (subagent_config, subagent_backend) = subagent_tools_config(
        &cwd,
        &session_id,
        background_selection.clone(),
        background_session_id.clone(),
    )?;
    let mut tools_config = resolve_tools_config(&settings)?;
    tools_config.subagents = Some(subagent_config);
    let tools = tools::built_in_tools_with(&tools_config);
    let system_prompt = wayland::system_prompt::assemble(&cwd, &tools);
    let usage_base = print::UsageBase::estimate(&system_prompt, &tools);
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    // The persisted project policy applies headless too (a granted tool/command
    // auto-approves), but the print gate cannot mint new grants, so no sink.
    let agent = Agent::new(provider, tools)
        .with_max_tool_roundtrips(settings.max_tool_roundtrips())
        .with_project_policy(project_policy(&cwd), None)
        .with_skip_permissions(permission_defaults.skip_permissions);
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
    announce_skip_permissions(permission_defaults.skip_permissions, session.as_mut());
    let (context_budget, compaction_trigger) = resolved_compaction_trigger(&settings, &selection)?;
    let budget = Some(context_budget.hard_compaction_threshold);
    let native_jj = wayland::trust::native_jj(&cwd).unwrap_or(false);
    let mut harness = wayland::Harness::new_configured(
        agent,
        cwd,
        tools::ToolState::new(),
        session,
        budget,
        wayland::HarnessRuntimeConfig::new(wayland::MutationSafetyConfig {
            enabled: settings.mutation_safety(),
            native_jj,
        })
        .with_worker_runtime(subagent_backend.runtime().clone()),
    );
    harness.set_subagent_backend(subagent_backend.clone());
    harness.set_compaction_trigger(context_budget, compaction_trigger);
    harness.set_verification(settings.verification());
    harness.set_summarizer(settings.compaction_summarizer());
    harness.set_compaction_worker(settings.compaction_worker_config()?);
    let provider_native_enabled =
        settings.compaction_provider_native()? == config::ProviderNativeMode::Auto;
    harness.set_provider_native(provider_native_enabled);
    if provider_native_enabled {
        eprintln!("warning: {}", cli::PROVIDER_NATIVE_COMPACTION_WARNING);
    }
    install_compaction_summarizer_factory(
        &mut harness,
        background_selection,
        settings
            .compaction_worker_model()
            .map(|model| {
                mimir::selection::ModelSelection::resolve_compaction_worker(&settings, model)
            })
            .transpose()?,
        system_prompt.clone(),
        background_session_id,
    );
    harness.set_tool_result_compaction(selection.tool_result_compaction.clone());
    let _ = harness.set_task_workflow_enabled(settings.tasks());
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
    harness.set_approval_mode(permission_defaults.approval_mode);

    // Merge piped stdin (when not a TTY) into the prompt before the turn.
    let piped = print::read_piped_stdin()?;
    let prompt = print::merge_prompt(prompt_arg, piped.as_deref());

    // Opt-in diagnostics sink (benchmarking): when `IRIS_USAGE_JSON` names a
    // path, the observer writes the run's token/cache/tool accounting there
    // after every provider turn, so even an errored or killed turn leaves the
    // latest totals. `None` disables the sink; stdout stays answer-only.
    let usage_path = env::var("IRIS_USAGE_JSON")
        .ok()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from);
    let observer = print::PrintObserver::with_base(usage_path, usage_base);
    let gate = print::PrintApprovalGate::new(approve);
    let turn_result = cli::run_print_turn(&mut harness, &prompt, &observer, &gate);

    // Final flush regardless of turn outcome (best effort, never propagated),
    // then surface any turn error after the accounting is safely on disk.
    observer.flush_usage();
    turn_result?;

    // Only the final assistant answer reaches stdout; everything else is
    // suppressed by the observer.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{}", observer.final_text())?;
    stdout.flush()?;
    Ok(())
}

/// Whether the current session should run with dangerous skip-permissions.
/// Explicit CLI input wins for the initial entry point; otherwise a transcript
/// marker overrides the global default, and absence inherits the default.
fn session_skip_permissions(
    cli_skip_permissions: bool,
    default_skip_permissions: bool,
    persisted_skip_permissions: Option<bool>,
) -> bool {
    cli_skip_permissions || persisted_skip_permissions.unwrap_or(default_skip_permissions)
}

/// Best-effort transcript audit for skip-permissions mode. Used both at process
/// start and for in-session `/new`/`/resume` swaps so dangerous mode survives the
/// next restart of the target session too.
fn record_skip_permissions(skip_permissions: bool, session: Option<&mut session::SessionLog>) {
    if !skip_permissions {
        return;
    }
    if let Some(log) = session
        && let Err(error) = log.append_dangerous_mode()
    {
        tracing::warn!(error = %format!("{error:#}"), "failed to record skip-permissions mode");
    }
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
    record_skip_permissions(skip_permissions, session);
}

fn native_jj_discovery_modal<P: ChatProvider>(
    force_plain: bool,
    cwd: &Path,
    settings: &config::Settings,
    harness: &wayland::Harness<P>,
) -> Option<ui::modal::Modal> {
    (!cli::prefers_text_ui(force_plain)
        && settings.mutation_safety()
        && harness.native_jj_available()
        && wayland::trust::native_jj(cwd).is_none())
    .then(ui::modal::jj_setup)
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

fn install_compaction_summarizer_factory(
    harness: &mut wayland::Harness<Box<dyn ChatProvider>>,
    selection: Arc<Mutex<mimir::selection::ModelSelection>>,
    dedicated_selection: Option<mimir::selection::ModelSelection>,
    system_prompt: String,
    session_id: Arc<Mutex<String>>,
) {
    let native_selection = selection.clone();
    let native_system_prompt = system_prompt.clone();
    let native_session_id = session_id.clone();
    harness.set_provider_compaction_factory(Arc::new(move || {
        let selection = native_selection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let session_id = native_session_id
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        build_provider(&selection, &native_system_prompt, &session_id)
    }));
    harness.set_compaction_summarizer_factory(Arc::new(move || {
        let selection = dedicated_selection.clone().unwrap_or_else(|| {
            selection
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        });
        let session_id = session_id
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        build_provider(&selection, wayland::SUMMARY_SYSTEM_PROMPT, &session_id)
    }));
}

fn resolved_compaction_trigger(
    settings: &config::Settings,
    selection: &mimir::selection::ModelSelection,
) -> Result<(
    metrics::ResolvedContextBudget,
    config::CompactionTriggerConfig,
)> {
    let trigger = settings.compaction_trigger()?;
    // Resolve one shared policy for enforcement, `/context`, diagnostics, and
    // the session meter. Display capacity, preparation, and hard application
    // remain distinct so each surface uses the value its label describes.
    let mut budget = metrics::ResolvedContextBudget::resolve(
        mimir::model_catalog::effective_context_window(selection, config::DEFAULT_SUMMARY_RESERVE)
            .map(Into::into),
        settings.context_token_budget,
        settings.context_token_budget(),
    );
    if settings.compaction_hard_threshold_is_explicit() {
        budget = budget.with_hard_threshold_fraction(trigger.hard);
    }
    Ok((budget, trigger))
}

/// Resume an existing session by id: load its transcript from the store,
/// reconstruct the provider-visible messages, seed the agent with them, and
/// continue appending future turns to the same log. Errors clearly when the id
/// is unknown or the session cannot be read.
fn resume_agent(session_id: &str, force_plain: bool, cli_skip_permissions: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    let permission_defaults =
        permission_defaults_from_setting(settings.default_approval.as_deref());
    persist_cli_skip_permissions(cli_skip_permissions);
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
    let skip_permissions = session_skip_permissions(
        cli_skip_permissions,
        permission_defaults.skip_permissions,
        stored.dangerous_skip_permissions,
    );
    persist_session_permission_override(
        stored.dangerous_skip_permissions,
        skip_permissions,
        permission_defaults.approval_mode,
    );

    // Resume assembles instructions through the same harness-owned baukasten as
    // a fresh session, so a resumed turn gets identical fragment/context output.
    // Onboarding must run first so a newly written ~/.iris/AGENTS.md is picked up.
    wayland::system_prompt::onboarding::maybe_onboard();
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = meta.id.clone();
    let background_selection = Arc::new(Mutex::new(selection.clone()));
    let background_session_id = Arc::new(Mutex::new(session_id.clone()));
    let (subagent_config, subagent_backend) = subagent_tools_config(
        &cwd,
        &session_id,
        background_selection.clone(),
        background_session_id.clone(),
    )?;
    let mut tools_config = resolve_tools_config(&settings)?;
    tools_config.subagents = Some(subagent_config);
    let tools = tools::built_in_tools_with(&tools_config);
    let prompt_assembly = wayland::system_prompt::assemble_with_notices(&cwd, &tools);
    let system_prompt = prompt_assembly.prompt;
    let mut startup_notices = prompt_assembly.notices;
    let (context_budget, compaction_trigger) = resolved_compaction_trigger(&settings, &selection)?;
    let budget = Some(context_budget.hard_compaction_threshold);
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

    let native_jj = wayland::trust::native_jj(&cwd).unwrap_or(false);
    let mut harness = wayland::Harness::resumed_configured(
        agent,
        cwd.clone(),
        tools::ToolState::new(),
        session,
        entry_ids,
        budget,
        wayland::HarnessRuntimeConfig::new(wayland::MutationSafetyConfig {
            enabled: settings.mutation_safety(),
            native_jj,
        })
        .with_worker_runtime(subagent_backend.runtime().clone()),
    );
    harness.set_subagent_backend(subagent_backend.clone());
    harness.set_compaction_trigger(context_budget, compaction_trigger);
    harness.set_verification(settings.verification());
    harness.set_summarizer(settings.compaction_summarizer());
    harness.set_compaction_worker(settings.compaction_worker_config()?);
    let provider_native_enabled =
        settings.compaction_provider_native()? == config::ProviderNativeMode::Auto;
    harness.set_provider_native(provider_native_enabled);
    install_compaction_summarizer_factory(
        &mut harness,
        background_selection.clone(),
        settings
            .compaction_worker_model()
            .map(|model| {
                mimir::selection::ModelSelection::resolve_compaction_worker(&settings, model)
            })
            .transpose()?,
        system_prompt.clone(),
        background_session_id.clone(),
    );
    harness.set_tool_result_compaction(selection.tool_result_compaction.clone());
    let _ = harness.set_task_workflow_enabled(settings.tasks());
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
    // Startup permission mode: apply the GLOBAL-ONLY `defaultApproval` normal
    // preset; dangerous state is carried by `skip_permissions` above.
    harness.set_approval_mode(permission_defaults.approval_mode);
    let session_cell = Rc::new(RefCell::new(session_id.clone()));
    let build_cell = session_cell.clone();
    let build = move |selection: &mimir::selection::ModelSelection, prompt: &str| {
        build_provider(selection, prompt, &build_cell.borrow())
    };
    let mut switch_state = cli::ModelSwitch::new(
        selection,
        system_prompt,
        &build,
        settings.enabled_models.clone(),
    );
    switch_state.set_background_selection_cell(background_selection);
    switch_state.set_compaction_settings(settings.clone());
    let mut switch = Some(switch_state);
    let swap_cwd = cwd.clone();
    let swap = move |source: &cli::SessionSource| {
        load_session_source(
            &swap_cwd,
            &session_cell,
            &background_session_id,
            permission_defaults_for_cwd(&swap_cwd),
            source,
        )
    };
    let jj_modal = native_jj_discovery_modal(force_plain, &cwd, &settings, &harness);
    startup_notices.extend(cli::provider_native_compaction_notices(
        provider_native_enabled,
    ));
    cli::run_interactive(
        &mut harness,
        &mut switch,
        force_plain,
        settings.tui_settings(),
        &swap,
        cli::StartupUi {
            notices: startup_notices,
            modal: jj_modal,
            followup_modal: None,
            start_page: false,
            resumed_session: Some(session_id.clone()),
        },
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
                selection.codex_transport,
                selection.codex_stream_idle_timeout,
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
                        supports_reasoning:
                            mimir::model_capabilities::openai_api_supports_reasoning(model),
                        api_key_required: true,
                        prompt_cache_key: Some(session_id),
                        cache_retention: selection.cache_retention,
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
                        prompt_cache_key: None,
                        cache_retention: mimir::selection::PromptCacheRetention::None,
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
const CARGO_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";
fn command_name() -> &'static str {
    COMMAND_NAME
}

/// Cargo args to install a specific release `tag` from the git repo. The tag is
/// pinned with `--tag` so the source-build fallback installs the latest
/// *release*, never bleeding-edge `main` — `iris update` must not ship testing
/// commits to users (see `update_via_cargo`).
fn update_args(tag: &str) -> [String; 8] {
    [
        "install".to_owned(),
        "--git".to_owned(),
        UPDATE_REPO.to_owned(),
        UPDATE_PACKAGE.to_owned(),
        "--tag".to_owned(),
        tag.to_owned(),
        "--locked".to_owned(),
        "--force".to_owned(),
    ]
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
    // Source builds have no prebuilt to self-replace, but `iris update` must
    // still track the latest *stable release*, not `main`. Resolve the latest
    // release and gate on semver so we never downgrade or install a
    // prerelease/testing build, then `cargo install --tag <release>`.
    let current = env!("CARGO_PKG_VERSION");
    selfupdate::voice::masthead("update");
    selfupdate::voice::step("check", &format!("running v{current} · source build"));
    let release = selfupdate::latest_release()?;
    match selfupdate::decide_update(
        current,
        &release.tag_name,
        release.prerelease,
        release.draft,
    ) {
        selfupdate::UpdateAction::UpToDate => {
            selfupdate::voice::done(
                "current",
                &format!("already on the latest release (v{current})"),
            );
            Ok(())
        }
        selfupdate::UpdateAction::Ahead => {
            selfupdate::voice::done(
                "ahead",
                &format!(
                    "running v{current}, newer than the latest release ({}) · nothing to do",
                    release.tag_name
                ),
            );
            Ok(())
        }
        selfupdate::UpdateAction::Skip => {
            selfupdate::voice::skip(
                "skipped",
                &format!(
                    "latest release ({}) is not stable · iris update installs stable releases only",
                    release.tag_name
                ),
            );
            Ok(())
        }
        selfupdate::UpdateAction::Update => {
            selfupdate::voice::step(
                "cargo",
                &format!(
                    "install {UPDATE_PACKAGE} {} from {UPDATE_REPO}",
                    release.tag_name
                ),
            );
            let status = update_command(&release.tag_name).status().context(
                "failed to run cargo; install Rust/Cargo or update with cargo install manually",
            )?;
            if !status.success() {
                bail!("cargo install failed with {status}");
            }
            selfupdate::voice::done("updated", &format!("v{current} → {}", release.tag_name));
            Ok(())
        }
    }
}

fn update_command(tag: &str) -> Command {
    let mut command = Command::new("cargo");
    command.args(update_args(tag));
    // `cargo install --git` can incorrectly reuse a stale binary when pointed at
    // a shared `CARGO_TARGET_DIR` and the git checkout changes without a version
    // bump. `iris update` must install the fetched revision, so ignore inherited
    // target-dir settings and let Cargo use its install-local build directory.
    command.env_remove(CARGO_TARGET_DIR_ENV);
    command
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

/// The `iris --version` line: crate version plus the build's target triple, so
/// a bug report or an update check names the exact artifact.
fn version_line() -> String {
    format!(
        "iris {} ({})",
        env!("CARGO_PKG_VERSION"),
        selfupdate::TARGET
    )
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
    eprintln!("  iris update                       Update Iris to the latest stable release");
    eprintln!("  iris -V, --version                Print the version and build target");
    eprintln!();
    eprintln!("Danger:");
    eprintln!("  --dangerously-skip-permissions   DANGER: auto-approve EVERY tool call with no");
    eprintln!(
        "                                   approval prompt, INCLUDING destructive commands."
    );
    eprintln!("                                   Bypasses the safety floors and saves this");
    eprintln!("                                   permission mode as the global default.");
    eprintln!("                                   Use only in a sandbox you trust.");
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct SessionDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl SessionDirGuard {
        fn set(path: &Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os("IRIS_SESSION_DIR");
            // SAFETY: serialized by ENV_LOCK and restored on drop.
            unsafe { std::env::set_var("IRIS_SESSION_DIR", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for SessionDirGuard {
        fn drop(&mut self) {
            match &self.previous {
                // SAFETY: serialized by ENV_LOCK and restored before release.
                Some(value) => unsafe { std::env::set_var("IRIS_SESSION_DIR", value) },
                // SAFETY: serialized by ENV_LOCK and restored before release.
                None => unsafe { std::env::remove_var("IRIS_SESSION_DIR") },
            }
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "iris-lib-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn session_file(root: &Path) -> PathBuf {
        let slug_dir = fs::read_dir(root).unwrap().next().unwrap().unwrap().path();
        fs::read_dir(slug_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    #[test]
    fn codex_context_policy_uses_cli_defaults_and_preserves_explicit_legacy_clamps() {
        let mut settings = config::Settings {
            default_provider: Some("openai-codex".to_string()),
            default_model: Some("gpt-5.6-sol".to_string()),
            ..config::Settings::default()
        };
        let selection = mimir::selection::ModelSelection::resolve(&settings).unwrap();
        let (policy, _) = resolved_compaction_trigger(&settings, &selection).unwrap();
        assert_eq!(policy.displayed_context_window, 353_400);
        assert_eq!(policy.preparation_threshold, 254_448);
        assert_eq!(policy.hard_compaction_threshold, 334_800);

        settings.compaction = Some(config::CompactionSettings {
            thresholds: Some(config::CompactionThresholdSettings {
                hard: Some(0.95),
                ..config::CompactionThresholdSettings::default()
            }),
            ..config::CompactionSettings::default()
        });
        let (policy, _) = resolved_compaction_trigger(&settings, &selection).unwrap();
        assert_eq!(policy.hard_compaction_threshold, 335_730);

        settings.compaction = None;
        settings.context_token_budget = Some(235_808);
        let (policy, _) = resolved_compaction_trigger(&settings, &selection).unwrap();
        assert_eq!(policy.displayed_context_window, 235_808);
        assert_eq!(policy.hard_compaction_threshold, 235_808);
    }

    #[test]
    fn accepted_child_route_reconstructs_selection_and_legacy_inherits() {
        let parent =
            mimir::selection::ModelSelection::resolve(&config::Settings::default()).unwrap();
        let legacy = iris_subagent_runtime::WorkerRequest::read_only("legacy");
        assert_eq!(
            child_selection_for_request(&parent, &legacy).unwrap(),
            parent
        );

        let route = wayland::subagents::ChildRoute::new(
            "anthropic",
            "claude-opus-4-6",
            "https://api.anthropic.com",
            Some("xhigh"),
        );
        let mut request = iris_subagent_runtime::WorkerRequest::read_only("routed");
        wayland::subagents::attach_route(&mut request, &route).unwrap();
        let effective = child_selection_for_request(&parent, &request).unwrap();
        assert_eq!(effective.provider, mimir::selection::ProviderId::Anthropic);
        assert_eq!(effective.model, "claude-opus-4-6");
        assert_eq!(
            effective.reasoning,
            Some(mimir::selection::ReasoningEffort::XHigh)
        );
    }

    #[test]
    fn command_name_is_iris() {
        assert_eq!(command_name(), "iris");
    }

    #[test]
    fn session_skip_permissions_honors_cli_marker_then_default() {
        assert!(!session_skip_permissions(false, false, None));
        assert!(session_skip_permissions(true, false, Some(false)));
        assert!(session_skip_permissions(false, false, Some(true)));
        assert!(!session_skip_permissions(false, true, Some(false)));
        assert!(session_skip_permissions(false, true, None));
    }

    #[test]
    fn permission_defaults_accept_dangerous_global_mode() {
        let defaults = permission_defaults_from_setting(Some("dangerously-skip-permissions"));
        assert!(defaults.skip_permissions);
        assert_eq!(defaults.approval_mode, nexus::ApprovalMode::Strict);

        let defaults = permission_defaults_from_setting(Some("auto"));
        assert!(!defaults.skip_permissions);
        assert_eq!(defaults.approval_mode, nexus::ApprovalMode::Auto);
    }

    #[test]
    fn record_skip_permissions_appends_dangerous_mode_entry() {
        let dir = TempDir::new();
        let mut log = session::SessionLog::create_in(&dir.path, Path::new("/w")).unwrap();
        record_skip_permissions(true, Some(&mut log));
        drop(log);

        let file = fs::read_to_string(session_file(&dir.path)).unwrap();
        assert!(file.contains("\"type\":\"dangerousMode\""), "{file}");
    }

    #[test]
    fn load_session_source_fresh_inherits_dangerous_default() {
        let dir = TempDir::new();
        let _guard = SessionDirGuard::set(&dir.path);
        let config_path = dir.path.join("settings.json");
        fs::write(
            &config_path,
            r#"{ "defaultApproval": "dangerously-skip-permissions" }"#,
        )
        .unwrap();
        let settings: config::Settings =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let defaults = permission_defaults_from_setting(settings.default_approval.as_deref());
        let cell = Rc::new(RefCell::new("current".to_string()));
        let background = Arc::new(Mutex::new("current".to_string()));

        let loaded = load_session_source(
            Path::new("/w"),
            &cell,
            &background,
            defaults,
            &cli::SessionSource::Fresh,
        )
        .unwrap();

        assert!(loaded.skip_permissions);
        assert_eq!(loaded.approval_mode, nexus::ApprovalMode::Strict);
        assert!(
            fs::read_to_string(session_file(&dir.path))
                .unwrap()
                .contains("\"type\":\"dangerousMode\""),
        );
    }

    #[test]
    fn load_session_source_resume_inherits_default_when_no_marker() {
        let dir = TempDir::new();
        let _guard = SessionDirGuard::set(&dir.path);
        let cwd = Path::new("/w");
        let id = session::new_session_id();
        let mut log = session::SessionLog::create_with_id(cwd, &id).unwrap();
        log.append(&nexus::Message::user("hi")).unwrap();
        drop(log);

        let cell = Rc::new(RefCell::new("current".to_string()));
        let background = Arc::new(Mutex::new("current".to_string()));
        let loaded = load_session_source(
            cwd,
            &cell,
            &background,
            PermissionDefaults {
                approval_mode: nexus::ApprovalMode::Auto,
                skip_permissions: true,
            },
            &cli::SessionSource::Resume(id),
        )
        .unwrap();

        assert!(
            loaded.skip_permissions,
            "unmarked sessions inherit the default"
        );
        assert_eq!(loaded.approval_mode, nexus::ApprovalMode::Auto);
    }

    #[test]
    fn version_line_names_the_exact_artifact() {
        let line = version_line();
        assert_eq!(
            line,
            format!(
                "iris {} ({})",
                env!("CARGO_PKG_VERSION"),
                selfupdate::TARGET
            ),
            "version line must carry the crate version and the build target"
        );
        assert!(
            line.starts_with("iris "),
            "version line must be greppable by tooling: {line}"
        );
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
    fn update_command_installs_pinned_release_tag_locked_with_force() {
        assert_eq!(
            UPDATE_REPO,
            "https://github.com/5omeOtherGuy/iris-agent.git"
        );
        assert_eq!(UPDATE_PACKAGE, "iris-agent");
        // The cargo fallback pins the release tag with `--tag` so it installs the
        // latest release, never `main`.
        assert_eq!(
            update_args("v1.2.3"),
            [
                "install",
                "--git",
                UPDATE_REPO,
                UPDATE_PACKAGE,
                "--tag",
                "v1.2.3",
                "--locked",
                "--force"
            ]
        );
    }

    #[test]
    fn update_command_removes_inherited_cargo_target_dir() {
        let command = update_command("v1.2.3");
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == CARGO_TARGET_DIR_ENV && value.is_none())
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
    fn unused_invalid_searxng_url_does_not_break_native_search() {
        let settings = config::Settings {
            web_search_backend: Some("native".into()),
            searxng_url: Some("searx.example".into()),
            ..config::Settings::default()
        };

        let web = resolve_web_tools_config(&settings).unwrap();
        assert_eq!(web.web_search, Some(tools::web::SearchBackend::Native));
        assert_eq!(web.searxng_url, None);
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
