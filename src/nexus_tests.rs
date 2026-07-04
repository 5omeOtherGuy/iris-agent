use super::*;
use crate::cli::run_session;
use crate::tools::ToolState;
use crate::ui::text::TextUi;
use crate::wayland::Harness;
use anyhow::anyhow;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Drive a single async future to completion on a current-thread runtime. The
/// loop/harness/agent APIs are async; the direct-call tests use this instead of
/// the full `run_session` REPL driver.
fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// Build a single-event provider stream that yields one terminal turn (or a
/// provider error). No text deltas: tests that need deltas use [`DeltaProvider`].
fn turn_stream(item: Result<AssistantTurn, String>) -> ProviderStream<'static> {
    let event = match item {
        Ok(turn) => Ok(ProviderEvent::Completed(turn)),
        Err(error) => Err(anyhow!(error)),
    };
    Box::pin(futures::stream::once(async move { event }))
}

struct FakeProvider {
    responses: RefCell<Vec<Result<AssistantTurn, String>>>,
    seen: RefCell<Vec<Vec<Message>>>,
}

impl FakeProvider {
    fn new(responses: Vec<Result<AssistantTurn, &str>>) -> Self {
        Self {
            responses: RefCell::new(
                responses
                    .into_iter()
                    .map(|result| result.map_err(str::to_string))
                    .rev()
                    .collect(),
            ),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl ChatProvider for FakeProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.seen.borrow_mut().push(messages.to_vec());
        let item = match self.responses.borrow_mut().pop() {
            Some(Ok(turn)) => Ok(turn),
            Some(Err(error)) => Err(error),
            None => Err("unexpected call".to_string()),
        };
        Ok(turn_stream(item))
    }
}

fn run_text_session<P: ChatProvider>(
    harness: &mut Harness<P>,
    input: &[u8],
    output: &mut Vec<u8>,
    errors: &mut Vec<u8>,
) -> Result<()> {
    let mut ui = TextUi::new(input, Vec::new(), Vec::new());
    let mut switch = None;
    run_session(harness, &mut ui, &mut switch)?;
    let (_, out, err) = ui.into_parts();
    *output = out;
    *errors = err;
    Ok(())
}

/// Wrap a bare agent in a Tier-2 harness over `workspace` with no transcript
/// log -- the in-memory setup the loop/approval/tool tests run against.
fn test_harness<P: ChatProvider>(provider: P, workspace: &Path, tools: Tools) -> Harness<P> {
    Harness::new(
        Agent::new(provider, tools),
        workspace.to_path_buf(),
        ToolState::new(),
        None,
        // Auto-compaction disabled: these tests exercise the loop, not the
        // budget policy.
        None,
    )
}

/// Front-end stub backing both Nexus seams: records every `AgentEvent` and
/// answers each approval review with a canned decision (`&self` + interior
/// mutability, like the real `UiBridge`). `review` snapshots the events seen so
/// far the first time it is called, so a test can assert emit/approval ordering
/// -- the checks the old in-`request_approval` asserts used to make.
struct RecordingFrontend {
    events: RefCell<Vec<AgentEvent>>,
    decision: Cell<ApprovalDecision>,
    events_at_review: RefCell<Option<Vec<AgentEvent>>>,
    /// The structured review facts the last `review` call received, so a test
    /// can assert Nexus threads `destructive`/`dirty_paths` to the gate.
    last_ctx: RefCell<Option<ReviewContext>>,
}

impl RecordingFrontend {
    fn new(decision: ApprovalDecision) -> Self {
        Self {
            events: RefCell::new(Vec::new()),
            decision: Cell::new(decision),
            events_at_review: RefCell::new(None),
            last_ctx: RefCell::new(None),
        }
    }
}

impl AgentObserver for RecordingFrontend {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events.borrow_mut().push(event);
        Ok(())
    }
}

impl ApprovalGate for RecordingFrontend {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        let mut snapshot = self.events_at_review.borrow_mut();
        if snapshot.is_none() {
            *snapshot = Some(self.events.borrow().clone());
        }
        *self.last_ctx.borrow_mut() = Some(ctx);
        let decision = self.decision.get();
        Box::pin(async move { Ok(decision) })
    }
}

#[test]
fn submit_turn_emits_non_gated_tool_sequence() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("read note", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    let display_events: Vec<_> = events
        .iter()
        .filter(|event| {
            !matches!(
                event,
                AgentEvent::ToolLifecycle { .. } | AgentEvent::ProviderTurnCompleted { .. }
            )
        })
        .collect();
    assert!(matches!(
        display_events[0],
        AgentEvent::ProviderTurnStarted { .. }
    ));
    assert!(matches!(display_events[1], AgentEvent::ToolProposed(_)));
    assert!(matches!(display_events[2], AgentEvent::ToolStarted(_)));
    assert!(matches!(display_events[3], AgentEvent::ToolResult { .. }));
    assert!(matches!(
        display_events[4],
        AgentEvent::ProviderTurnStarted { .. }
    ));
    assert!(matches!(display_events[5], AgentEvent::AssistantText(_)));
    assert!(matches!(display_events[6], AgentEvent::TurnComplete));
    // read is never gated: the approval gate must not be consulted.
    assert!(frontend.events_at_review.borrow().is_none());
    Ok(())
}

#[test]
fn gated_write_emits_diff_preview_before_approval() -> Result<()> {
    // out.txt does not pre-exist: a blind create still emits a diff preview
    // (old is empty) and is not subject to the stale-file guard, so this
    // test stays focused on preview-before-approval ordering.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "new\n" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("write it", &frontend, &frontend, &CancellationToken::new()))?;

    // The diff preview is emitted before the gate is consulted.
    let at_review = frontend.events_at_review.borrow();
    let at_review = at_review
        .as_ref()
        .expect("write is gated; the gate must be consulted");
    assert!(
        at_review
            .iter()
            .position(|event| matches!(event, AgentEvent::DiffPreview { .. }))
            .is_some_and(
                |diff_at| at_review[diff_at + 1..].iter().any(|event| matches!(
                    event,
                    AgentEvent::ToolLifecycle {
                        state: ToolEventState::ApprovalRequested,
                        ..
                    }
                ))
            )
    );

    let events = frontend.events.borrow();
    let lifecycle_states: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolLifecycle { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
    assert!(lifecycle_states.starts_with(&[
        ToolEventState::Proposed,
        ToolEventState::ApprovalRequested,
        ToolEventState::Approved,
    ]));
    assert!(matches!(events[0], AgentEvent::ProviderTurnStarted { .. }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::DiffPreview { .. }))
    );
    // ToolStarted is emitted after approval resolves, before execution.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolStarted(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolResult { .. }))
    );
    assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "new\n");
    Ok(())
}

#[test]
fn ungated_tool_can_emit_diff_preview_before_execution() -> Result<()> {
    struct PreviewTool;
    impl Tool for PreviewTool {
        fn name(&self) -> &str {
            "preview"
        }
        fn description(&self) -> &str {
            "ungated previewing tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::text("ran")) })
        }
        fn diff_preview(&self, _workspace: &Path, _args: &Value) -> Option<String> {
            Some("--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n".to_string())
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("preview", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(PreviewTool)]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("preview", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    let diff_at = events
        .iter()
        .position(|event| matches!(event, AgentEvent::DiffPreview { .. }))
        .expect("ungated preview event");
    let started_at = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolStarted(_)))
        .expect("tool started event");
    assert!(diff_at < started_at, "{events:#?}");
    assert!(
        frontend.events_at_review.borrow().is_none(),
        "ungated preview must not consult the approval gate"
    );
    Ok(())
}

#[test]
fn malformed_denial_skips_diff_preview() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("write", json!({ "path": "out.txt" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("write it", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentEvent::DiffPreview { .. }))
    );
    assert!(matches!(events[0], AgentEvent::ProviderTurnStarted { .. }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolDenied(_)))
    );
    // Malformed args must not preflight: the gate saw only the provider-turn
    // correlation event plus approval-request metadata before deciding.
    assert!(
        frontend
            .events_at_review
            .borrow()
            .as_ref()
            .is_some_and(|events| {
                events
                    .iter()
                    .all(|event| !matches!(event, AgentEvent::DiffPreview { .. }))
                    && events.iter().any(|event| {
                        matches!(
                            event,
                            AgentEvent::ToolLifecycle {
                                state: ToolEventState::ApprovalRequested,
                                ..
                            }
                        )
                    })
            })
    );
    assert!(!workspace.path.join("out.txt").exists());
    Ok(())
}

#[test]
fn repl_keeps_conversation_across_turns() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn::text("hello")),
        Ok(AssistantTurn::text("goodbye")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "hi\nbye\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(String::from_utf8(output)?.contains("assistant> hello"));
    assert!(errors.is_empty());
    assert_eq!(harness.agent.provider.seen.borrow().len(), 2);
    assert_eq!(harness.agent.provider.seen.borrow()[1][0].content, "hi");
    assert_eq!(harness.agent.provider.seen.borrow()[1][1].content, "hello");
    assert_eq!(harness.agent.provider.seen.borrow()[1][2].content, "bye");
    Ok(())
}

struct AuthFailProvider;
impl ChatProvider for AuthFailProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let event = Err(crate::errors::AuthError::new("token expired").into());
        Ok(Box::pin(futures::stream::once(async move { event })))
    }
}

struct DeltaProvider;
impl ChatProvider for DeltaProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let events = vec![
            Ok(ProviderEvent::TextDelta("Hel".to_string())),
            Ok(ProviderEvent::TextDelta("lo".to_string())),
            Ok(ProviderEvent::Completed(AssistantTurn::text("Hello"))),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

#[test]
fn streamed_deltas_render_in_order_and_commit_once() -> Result<()> {
    let workspace = test_workspace()?;
    let mut harness = test_harness(
        DeltaProvider,
        &workspace.path,
        crate::tools::built_in_tools(),
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "hello\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    // The startup banner prefixes the session; this test only cares that the
    // streamed deltas commit once, in order, after the prompt.
    assert!(
        String::from_utf8(output)?.ends_with("Type /exit to quit.\niris> assistant> Hello\niris> ")
    );
    assert!(errors.is_empty());
    assert_eq!(harness.agent.messages.len(), 2);
    assert_eq!(
        harness.agent.messages[1],
        Message::assistant("Hello").with_provider_turn_id("turn_00000000")
    );
    Ok(())
}

#[test]
fn repl_reports_auth_errors_with_login_hint() -> Result<()> {
    let workspace = test_workspace()?;
    let mut harness = test_harness(
        AuthFailProvider,
        &workspace.path,
        crate::tools::built_in_tools(),
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "hello\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(errors)?;
    assert!(rendered.contains("auth error:"));
    assert!(rendered.contains("re-run the login command"));
    assert!(!rendered.contains("provider error:"));
    Ok(())
}

#[test]
fn repl_reports_provider_errors_and_continues() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Err("boom"), Ok(AssistantTurn::text("recovered"))]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "fail\nagain\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(String::from_utf8(errors)?.contains("provider error: boom"));
    assert!(String::from_utf8(output)?.contains("assistant> recovered"));
    assert_eq!(harness.agent.messages.len(), 2);
    assert_eq!(harness.agent.messages[0].content, "again");
    assert_eq!(harness.agent.messages[1].content, "recovered");
    Ok(())
}

#[test]
fn tool_loop_reads_workspace_file_and_returns_result_to_model() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                thought_signature: None,
                name: "read".to_string(),
                arguments: json!({ "path": "note.txt" }),
            }],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("The file says hello from file.")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read note\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert!(String::from_utf8(output)?.contains("assistant> The file says hello from file."));
    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 2);
    let tool_result = seen[1].last().unwrap();
    assert_eq!(tool_result.role, Role::Tool);
    assert_eq!(tool_result.tool_call_id.as_deref(), Some("call_1"));
    assert!(tool_result.content.contains("hello from file"));
    // #15 contract: structured metadata rides alongside the text on the wire.
    assert!(tool_result.content.contains("\"metadata\""));
    assert!(tool_result.content.contains("\"total_lines\":1"));
    Ok(())
}

#[test]
fn tool_result_is_displayed_to_user() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                thought_signature: None,
                name: "read".to_string(),
                arguments: json!({ "path": "note.txt" }),
            }],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read note\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    // Read-only tools render as Codex-style exploration summaries; the full
    // content still goes to the model, not the terminal transcript.
    assert!(rendered.contains("• Explored"));
    assert!(rendered.contains("  └ Read note.txt"));
    assert!(!rendered.contains("hello from file"));
    assert!(errors.is_empty());
    Ok(())
}

#[test]
fn tool_error_is_displayed_and_loop_continues() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                thought_signature: None,
                name: "unknown".to_string(),
                arguments: json!({}),
            }],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("recovered")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "use bad tool\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    assert!(rendered.contains("✗ Ran unknown"));
    assert!(rendered.contains("error: unknown tool: unknown"));
    assert!(rendered.contains("assistant> recovered"));
    assert!(errors.is_empty());
    Ok(())
}

fn repeated_read_call() -> Result<AssistantTurn, &'static str> {
    Ok(AssistantTurn {
        text: None,
        reasoning: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "read".to_string(),
            arguments: json!({ "path": "note.txt" }),
        }],

        response_id: None,
        usage: None,
        completion_reason: None,
    })
}

#[test]
fn tool_loop_stops_gracefully_at_configured_soft_cap() -> Result<()> {
    // With a configured soft cap, the loop ends the turn gracefully after that
    // many round-trips: a user-visible notice, no provider error, and the REPL
    // keeps running. There is no built-in default cap (see
    // `tool_loop_is_unbounded_by_default`).
    const CAP: usize = 5;
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    // Script more tool calls than the cap to prove the loop stops at the cap,
    // not because the provider ran out of scripted turns.
    let provider = FakeProvider::new((0..CAP + 3).map(|_| repeated_read_call()).collect());
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()).with_max_tool_roundtrips(Some(CAP)),
        workspace.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read forever\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    assert!(rendered.contains("stopped after"));
    assert!(errors.is_empty());
    // The provider is consulted exactly the capped number of times, then the
    // loop stops without one extra round-trip.
    assert_eq!(harness.agent.provider.seen.borrow().len(), CAP);
    Ok(())
}

#[test]
fn tool_loop_is_unbounded_by_default() -> Result<()> {
    // No configured cap: the loop runs while the model emits tool calls and
    // ends only when the model stops, with no built-in fixed turn cap. Script
    // well past the old hardcoded 50-roundtrip ceiling to prove it is gone.
    const ROUNDS: usize = 60;
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    let mut turns: Vec<Result<AssistantTurn, &'static str>> =
        (0..ROUNDS).map(|_| repeated_read_call()).collect();
    // A final turn with no tool calls ends the turn naturally.
    turns.push(Ok(AssistantTurn {
        text: Some("done reading".to_string()),
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: None,
    }));
    let provider = FakeProvider::new(turns);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read forever\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    // The model's natural completion is reached; no soft-cap notice fires.
    assert!(rendered.contains("done reading"), "out: {rendered}");
    assert!(!rendered.contains("stopped after"), "out: {rendered}");
    assert!(errors.is_empty());
    // Every scripted turn (ROUNDS tool calls + the final text turn) is consumed.
    assert_eq!(harness.agent.provider.seen.borrow().len(), ROUNDS + 1);
    Ok(())
}

#[test]
fn unknown_tool_call_returns_tool_error_to_model() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                thought_signature: None,
                name: "unknown".to_string(),
                arguments: json!({}),
            }],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("I could not use that tool.")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "use bad tool\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert_tool_error_contains(
        &harness.agent.provider.seen.borrow()[1],
        "unknown tool: unknown",
    );
    Ok(())
}

#[test]
fn unknown_tool_resolution_yields_unknown_tool_error() -> Result<()> {
    // After Step B the loop resolves calls by name over the injected set (pi's
    // `tools.find(t => t.name === name)`); an unresolved name must still surface
    // `unknown tool: <name>` (the analogue of pi's `Tool <name> not found`) as
    // the tool result fed back to the model.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("ghost", json!({}))),
        Ok(AssistantTurn::text("ok")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "use ghost\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert_tool_error_contains(
        &harness.agent.provider.seen.borrow()[1],
        "unknown tool: ghost",
    );
    Ok(())
}

#[test]
fn unknown_tool_does_not_emit_a_phantom_tool_started() -> Result<()> {
    // An unresolved tool must NOT open a live exec cell: `ToolStarted` is
    // emitted only when a real tool begins executing, so the front-end never
    // shows a `Running` cell for a call that immediately fails.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("ghost", json!({}))),
        Ok(AssistantTurn::text("ok")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("use ghost", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolStarted(_))),
        "a phantom ToolStarted was emitted for an unknown tool"
    );
    // The call still produces a ToolError, keeping the transcript pairing valid.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolError { .. })),
        "unknown tool should still surface a ToolError"
    );
    Ok(())
}

#[test]
fn injected_custom_tool_is_resolved_and_executed() -> Result<()> {
    // Tools are injected and resolved by name over the provided set: a tool that
    // is NOT a built-in still runs, proving the loop cannot have regressed to a
    // hardcoded built-in dispatch.
    struct MarkerTool;
    impl Tool for MarkerTool {
        fn name(&self) -> &str {
            "marker"
        }
        fn description(&self) -> &str {
            "test marker tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::text("marker-tool-ran")) })
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("marker", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(MarkerTool)]),
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "use marker\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert_eq!(tool_result.role, Role::Tool);
    assert!(tool_result.content.contains("marker-tool-ran"));
    Ok(())
}

#[test]
fn malformed_read_arguments_return_tool_error_to_model() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                thought_signature: None,
                name: "read".to_string(),
                arguments: json!({ "not_path": "note.txt" }),
            }],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("The read call was malformed.")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read malformed\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert_tool_error_contains(
        &harness.agent.provider.seen.borrow()[1],
        "read tool arguments must include path",
    );
    Ok(())
}

#[test]
fn read_tool_rejects_paths_outside_workspace() -> Result<()> {
    let workspace = test_workspace()?;
    let outside = workspace.path.parent().unwrap().join("outside.txt");
    fs::write(&outside, "secret")?;

    let result = read_file(&workspace.path, "../outside.txt");

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("escapes workspace")
    );
    fs::remove_file(outside)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn read_tool_rejects_symlink_escape_from_workspace() -> Result<()> {
    let workspace = test_workspace()?;
    let outside_dir = test_workspace()?;
    let outside = outside_dir.path.join("outside.txt");
    fs::write(&outside, "secret")?;
    std::os::unix::fs::symlink(&outside, workspace.path.join("link.txt"))?;

    let result = read_file(&workspace.path, "link.txt");

    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("escapes workspace")
    );
    Ok(())
}

#[test]
fn read_tool_returns_missing_file_error() -> Result<()> {
    let workspace = test_workspace()?;

    let error = read_file(&workspace.path, "missing.txt").unwrap_err();
    let result = ToolResultContract::tool_error(error).into_wire_json();

    assert!(result.contains("\"ok\":false"));
    assert!(result.contains("failed to resolve path"));
    Ok(())
}

#[test]
fn unclassified_tool_error_wire_shape_is_byte_identical() {
    // An unclassified error carries no `metadata` key: the wire bytes stay
    // exactly `{ "ok": false, "error": ... }` (ADR-0040 opt-in guarantee).
    let error = anyhow!("plain failure text");
    let wire = ToolResultContract::tool_error(error).into_wire_json();

    assert_eq!(wire, r#"{"error":"plain failure text","ok":false}"#);
    assert!(!wire.contains("metadata"));
}

#[test]
fn classified_tool_error_emits_metadata_beside_error() {
    // A classified error keeps the `error` string and adds a compact
    // `metadata` object carrying `class` plus any fields.
    let error = ClassifiedError::new("not-found", "could not find the text")
        .with("occurrences", json!(0))
        .into();
    let wire = ToolResultContract::tool_error(error).into_wire_value();

    assert_eq!(wire["ok"], json!(false));
    assert_eq!(wire["error"], json!("could not find the text"));
    assert_eq!(wire["metadata"]["class"], json!("not-found"));
    assert_eq!(wire["metadata"]["occurrences"], json!(0));
}

#[test]
fn denied_and_cancelled_wire_shapes_are_unchanged() {
    // ADR-0040 must not perturb the pre-existing denial/cancel envelopes.
    assert_eq!(
        ToolResultContract::denied().into_wire_json(),
        r#"{"denied":true,"error":"tool call denied by user","ok":false}"#
    );
    assert_eq!(
        ToolResultContract::cancelled().into_wire_json(),
        r#"{"cancelled":true,"error":"tool call cancelled by user","ok":false}"#
    );
}

fn single_call_turn(name: &str, arguments: Value) -> AssistantTurn {
    AssistantTurn {
        text: None,
        reasoning: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments,
        }],
        response_id: None,
        usage: None,
        completion_reason: None,
    }
}

#[test]
fn provider_turn_started_events_identify_each_model_round_trip() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("read note", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    let turn_ids: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderTurnStarted { turn_id } => Some(turn_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(turn_ids, ["turn_00000000", "turn_00000001"]);
    let completed_turn_ids: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderTurnCompleted { turn_id, .. } => Some(turn_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(completed_turn_ids, ["turn_00000000", "turn_00000001"]);
    let tool_states: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolLifecycle {
                provider_turn_id,
                call_id,
                state,
                ..
            } => Some((provider_turn_id.as_str(), call_id.as_str(), *state)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_states,
        [
            ("turn_00000000", "call_1", ToolEventState::Proposed),
            ("turn_00000000", "call_1", ToolEventState::Started),
            ("turn_00000000", "call_1", ToolEventState::Succeeded),
        ]
    );
    assert_eq!(
        harness.agent.messages()[1].provider_turn_id.as_deref(),
        Some("turn_00000000")
    );
    assert_eq!(
        harness.agent.messages()[2].provider_turn_id.as_deref(),
        Some("turn_00000000")
    );
    assert_eq!(
        harness.agent.messages()[3].provider_turn_id.as_deref(),
        Some("turn_00000001")
    );
    Ok(())
}

#[test]
fn provider_completion_event_carries_response_id_and_usage() -> Result<()> {
    let workspace = test_workspace()?;
    let usage = ProviderUsage {
        provider: "anthropic".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        input_tokens: 10,
        output_tokens: 3,
        cache_read_input_tokens: 4,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 0,
        total_tokens: 13,
        cache_creation: Some(CacheCreation {
            ephemeral_5m_input_tokens: 2,
            ephemeral_1h_input_tokens: 0,
        }),
    };
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: Some("done".to_string()),
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        response_id: Some("msg_1".to_string()),
        usage: Some(usage.clone()),
        completion_reason: Some(CompletionReason::MaxOutputTokens),
    })]);
    let mut harness = test_harness(provider, &workspace.path, Tools::new(Vec::new()));
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    let completed = frontend
        .events
        .borrow()
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderTurnCompleted {
                turn_id,
                response_id,
                usage,
                completion_reason,
            } => Some((
                turn_id.clone(),
                response_id.clone(),
                usage.clone(),
                *completion_reason,
            )),
            _ => None,
        })
        .expect("completion event");
    assert_eq!(completed.0, "turn_00000000");
    assert_eq!(completed.3, Some(CompletionReason::MaxOutputTokens));
    assert_eq!(completed.1.as_deref(), Some("msg_1"));
    assert_eq!(completed.2, Some(usage));
    // A truncation completion reason surfaces a provider-neutral user notice.
    assert!(
        frontend.events.borrow().iter().any(|e| matches!(
            e,
            AgentEvent::Notice(m) if m.contains("maximum output-token limit")
        )),
        "max-output-token truncation should emit a user notice"
    );
    Ok(())
}

#[test]
fn context_window_exceeded_completion_emits_user_notice() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: Some("partial".to_string()),
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: Some(CompletionReason::ContextWindowExceeded),
    })]);
    let mut harness = test_harness(provider, &workspace.path, Tools::new(Vec::new()));
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events.borrow().iter().any(|e| matches!(
            e,
            AgentEvent::Notice(m) if m.contains("context-window limit")
        )),
        "context-window truncation should emit a user notice"
    );
    Ok(())
}

#[test]
fn routine_completion_reason_emits_no_notice() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: Some("done".to_string()),
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: Some(CompletionReason::EndTurn),
    })]);
    let mut harness = test_harness(provider, &workspace.path, Tools::new(Vec::new()));
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        !frontend
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(_))),
        "a routine end_turn completion must not emit a notice"
    );
    Ok(())
}

#[test]
fn content_less_refusal_emits_user_notice() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: None,
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: Some(CompletionReason::Refusal),
    })]);
    let mut harness = test_harness(provider, &workspace.path, Tools::new(Vec::new()));
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events.borrow().iter().any(|e| matches!(
            e,
            AgentEvent::Notice(m) if m.contains("declined to respond")
        )),
        "a content-less refusal should emit a user notice"
    );
    Ok(())
}

#[test]
fn refusal_with_text_emits_no_notice() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: Some("I can't help with that.".to_string()),
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: Some(CompletionReason::Refusal),
    })]);
    let mut harness = test_harness(provider, &workspace.path, Tools::new(Vec::new()));
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        !frontend
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(_))),
        "a refusal that carried explanatory text must not add a notice"
    );
    Ok(())
}

#[test]
fn completed_turn_records_reasoning_and_all_tool_calls_before_results() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("a.txt"), "A")?;
    fs::write(workspace.path.join("b.txt"), "B")?;
    let origin = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: Some("working".to_string()),
            reasoning: vec![ReasoningBlock::new("thinking", Some("sig"), false, origin)],
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    thought_signature: None,
                    name: "read".to_string(),
                    arguments: json!({ "path": "a.txt" }),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    thought_signature: None,
                    name: "read".to_string(),
                    arguments: json!({ "path": "b.txt" }),
                },
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let roles: Vec<Role> = harness.agent.messages().iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::User,
            Role::AssistantReasoning,
            Role::Assistant,
            Role::AssistantToolCall,
            Role::AssistantToolCall,
            Role::Tool,
            Role::Tool,
            Role::Assistant,
        ],
        "one model turn must keep reasoning/text/all tool calls contiguous before tool results"
    );
    Ok(())
}

#[test]
fn completed_turn_emits_reasoning_event_before_text_without_changing_storage() -> Result<()> {
    let workspace = test_workspace()?;
    let origin = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: Some("the answer".to_string()),
        reasoning: vec![ReasoningBlock::new(
            "let me think",
            Some("sig"),
            false,
            origin,
        )],
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: None,
    })]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    let reasoning_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::AssistantReasoning { .. }))
        .expect("reasoning event emitted");
    let text_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::AssistantText(_)))
        .expect("assistant text event emitted");
    assert!(
        reasoning_at < text_at,
        "reasoning event must precede assistant text"
    );
    match &events[reasoning_at] {
        AgentEvent::AssistantReasoning { text, redacted } => {
            assert_eq!(text, "let me think");
            assert!(!redacted);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    drop(events);

    // Storage is unchanged: the reasoning row is still persisted (ADR-0016).
    let reasoning_rows: Vec<&Message> = harness
        .agent
        .messages()
        .iter()
        .filter(|m| m.role == Role::AssistantReasoning)
        .collect();
    assert_eq!(reasoning_rows.len(), 1);
    assert_eq!(reasoning_rows[0].content, "let me think");
    assert_eq!(reasoning_rows[0].continuity.as_deref(), Some("sig"));
    Ok(())
}

#[test]
fn redacted_reasoning_emits_event_without_leaking_text() -> Result<()> {
    let workspace = test_workspace()?;
    let origin = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: Some("done".to_string()),
        // A redacted block still stores its opaque content, but the emitted
        // display event must never carry that text downstream.
        reasoning: vec![ReasoningBlock::new(
            "opaque-secret",
            Some("data"),
            true,
            origin,
        )],
        tool_calls: Vec::new(),
        response_id: None,
        usage: None,
        completion_reason: None,
    })]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    let reasoning = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::AssistantReasoning { text, redacted } => Some((text.clone(), *redacted)),
            _ => None,
        })
        .expect("redacted reasoning event emitted");
    assert!(reasoning.1, "event must be marked redacted");
    assert!(
        reasoning.0.is_empty(),
        "redacted reasoning text must never leak into the event: {:?}",
        reasoning.0
    );
    drop(events);

    // Storage is unchanged: the redacted row keeps its opaque content + flag.
    let row = harness
        .agent
        .messages()
        .iter()
        .find(|m| m.role == Role::AssistantReasoning)
        .expect("reasoning row stored");
    assert!(row.redacted);
    assert_eq!(row.content, "opaque-secret");
    Ok(())
}

#[test]
fn observer_error_on_tool_result_still_records_paired_transcript() -> Result<()> {
    // A front-end that fails while rendering a ToolResult must not leave a
    // dangling assistant-tool-call in the transcript. `record_call` appends both
    // the assistant call and its paired tool-result BEFORE emitting the observer
    // event, so even when the observer errors the persisted transcript stays a
    // valid call/result pair the next provider request can accept.
    struct FailOnToolResult;
    impl AgentObserver for FailOnToolResult {
        fn on_event(&self, event: AgentEvent) -> Result<()> {
            match event {
                AgentEvent::ToolResult { .. } => Err(anyhow!("render failed")),
                _ => Ok(()),
            }
        }
    }
    impl ApprovalGate for FailOnToolResult {
        fn review<'a>(
            &'a self,
            _call: &'a ToolCall,
            _allow_always: bool,
            _allow_project: bool,
            _ctx: ReviewContext,
        ) -> ApprovalFuture<'a> {
            Box::pin(async move { Ok(ApprovalDecision::Allow) })
        }
    }

    struct MarkerTool;
    impl Tool for MarkerTool {
        fn name(&self) -> &str {
            "marker"
        }
        fn description(&self) -> &str {
            "test marker tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::text("marker-ran")) })
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(single_call_turn("marker", json!({})))]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(MarkerTool)]),
    );
    let frontend = FailOnToolResult;

    let result = block_on(harness.submit_turn(
        "use marker",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ));
    assert!(
        result.is_err(),
        "observer error should surface as a turn error"
    );

    // Transcript: user, assistant-tool-call, tool-result. The pair is complete
    // despite the observer failing on the result event (the pre-fix bug skipped
    // the tool-result push, leaving only 2 messages and a dangling call).
    let messages = &harness.agent.messages;
    assert_eq!(messages.len(), 3, "expected user + tool-call + tool-result");
    assert_eq!(messages[1].role, Role::AssistantToolCall);
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert!(messages[2].content.contains("marker-ran"));
    Ok(())
}

#[test]
fn approved_write_executes_and_creates_file() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "hi" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "hi");
    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[1].last().unwrap().content.contains("\"ok\":true"));
    Ok(())
}

#[test]
fn approved_write_renders_prompt_and_result_without_raw_json() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "hi" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    // The approval prompt carries the summary; the result row follows it.
    assert!(rendered.contains("approve write out.txt?"));
    assert!(rendered.contains("• Ran write out.txt"));
    // No separate proposed line and no raw `name({json})` argument dump.
    assert!(!rendered.contains("tool> write({"));
    Ok(())
}

#[test]
fn denied_write_skips_execution_and_records_denial() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "hi" }),
        )),
        Ok(AssistantTurn::text("understood")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert!(!workspace.path.join("out.txt").exists());
    let rendered = String::from_utf8(output)?;
    assert!(rendered.contains("✗ Denied write out.txt"));
    // Gated calls no longer double-print a raw `tool> write({...})` line.
    assert!(!rendered.contains("tool> write({"));

    let seen = harness.agent.provider.seen.borrow();
    let denial = seen[1].last().unwrap();
    assert_eq!(denial.role, Role::Tool);
    assert!(denial.content.contains("\"denied\":true"));
    assert!(denial.content.contains("denied by user"));
    assert_eq!(denial.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(denial.tool_name.as_deref(), Some("write"));

    // Assistant-tool-call -> tool-result pairing preserved on deny.
    let tool_call = seen[1]
        .iter()
        .find(|m| m.role == Role::AssistantToolCall)
        .unwrap();
    assert_eq!(tool_call.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(tool_call.tool_name.as_deref(), Some("write"));
    Ok(())
}

#[test]
fn read_is_never_gated_even_under_auto_deny() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read note\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert_eq!(tool_result.role, Role::Tool);
    assert!(tool_result.content.contains("hello from file"));
    Ok(())
}

#[test]
fn denied_bash_does_not_run_command() -> Result<()> {
    let workspace = test_workspace()?;
    let marker = workspace.path.join("marker");
    let command = format!("touch {}", marker.display());
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("bash", json!({ "command": command }))),
        Ok(AssistantTurn::text("ok")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "run it\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(!marker.exists());
    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
    Ok(())
}

#[test]
fn denied_edit_leaves_file_unchanged() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "original")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "edit",
            json!({ "path": "note.txt", "old": "original", "new": "changed" }),
        )),
        Ok(AssistantTurn::text("ok")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "edit it\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert_eq!(
        fs::read_to_string(workspace.path.join("note.txt"))?,
        "original"
    );
    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
    Ok(())
}

#[test]
fn read_then_edit_succeeds_end_to_end() -> Result<()> {
    // Proves the session-scoped observation store persists across tool
    // calls: the read in roundtrip 0 satisfies the edit's freshness guard
    // in roundtrip 1.
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "original")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
        Ok(single_call_turn(
            "edit",
            json!({
                "file_path": "note.txt",
                "old_string": "original",
                "new_string": "changed"
            }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "go\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert_eq!(
        fs::read_to_string(workspace.path.join("note.txt"))?,
        "changed"
    );
    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[2].last().unwrap().content.contains("\"ok\":true"));
    Ok(())
}

#[test]
fn edit_without_prior_read_is_rejected_end_to_end() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "original")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "edit",
            json!({
                "file_path": "note.txt",
                "old_string": "original",
                "new_string": "changed"
            }),
        )),
        Ok(AssistantTurn::text("understood")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "go\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    // Approved by the user, but the freshness guard refuses the blind edit
    // and the file is left unchanged.
    assert_eq!(
        fs::read_to_string(workspace.path.join("note.txt"))?,
        "original"
    );
    let seen = harness.agent.provider.seen.borrow();
    let result = seen[1].last().unwrap();
    assert!(result.content.contains("\"ok\":false"));
    // Prose stays exactly as informative; the `stale-file` class is additive
    // (ADR-0040) and travels beside the unchanged `error` string.
    assert!(result.content.contains("has not been read this session"));
    assert!(result.content.contains("\"class\":\"stale-file\""));
    assert!(result.content.contains("\"reason\":\"unread\""));
    Ok(())
}

#[test]
fn edit_not_found_failure_carries_class_metadata_end_to_end() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "original")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
        Ok(single_call_turn(
            "edit",
            json!({
                "file_path": "note.txt",
                "old_string": "does-not-exist",
                "new_string": "changed"
            }),
        )),
        Ok(AssistantTurn::text("understood")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "go\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let seen = harness.agent.provider.seen.borrow();
    let result = seen[2].last().unwrap();
    assert!(result.content.contains("\"ok\":false"));
    assert!(result.content.contains("could not find the text"));
    assert!(result.content.contains("\"class\":\"not-found\""));
    Ok(())
}

#[test]
fn edit_not_unique_failure_carries_class_metadata_end_to_end() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("dup.txt"), "dup\ndup\n")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "dup.txt" }))),
        Ok(single_call_turn(
            "edit",
            json!({
                "file_path": "dup.txt",
                "old_string": "dup",
                "new_string": "x"
            }),
        )),
        Ok(AssistantTurn::text("understood")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "go\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let seen = harness.agent.provider.seen.borrow();
    let result = seen[2].last().unwrap();
    assert!(result.content.contains("\"ok\":false"));
    assert!(result.content.contains("found 2 occurrences"));
    assert!(result.content.contains("\"class\":\"not-unique\""));
    // Compact machine-readable field beside the class (ADR-0036/0040).
    assert!(result.content.contains("\"occurrences\":2"));
    Ok(())
}

#[test]
fn classified_edit_failure_metadata_is_persisted_to_transcript() -> Result<()> {
    // ADR-0040 metadata rides the existing persistence path: the JSONL
    // tool-result row for a classified failure carries the `class`.
    let workspace = test_workspace()?;
    let root = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "original")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
        Ok(single_call_turn(
            "edit",
            json!({
                "file_path": "note.txt",
                "old_string": "does-not-exist",
                "new_string": "changed"
            }),
        )),
        Ok(AssistantTurn::text("understood")),
    ]);
    let agent = Agent::new(provider, crate::tools::built_in_tools());
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let log_path = log.path().to_path_buf();
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );

    let mut out = Vec::new();
    let mut err = Vec::new();
    // `y` approves the edit; the guarded edit then fails with not-found.
    run_text_session(&mut harness, b"go\ny\n/exit\n", &mut out, &mut err)?;

    let persisted = fs::read_to_string(&log_path)?;
    let tool_row = persisted
        .lines()
        .find(|line| line.contains("\"role\":\"tool\"") && line.contains("could not find the text"))
        .expect("classified edit failure persisted as a tool row");
    // The tool-result content is a JSON string inside the JSONL row, so the
    // metadata object's quotes are escaped (`\"class\":\"not-found\"`).
    assert!(tool_row.contains("class") && tool_row.contains("not-found"));
    Ok(())
}

#[test]
fn terminal_approver_allows_write_end_to_end() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "hi" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(errors.is_empty());
    assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "hi");
    Ok(())
}

#[test]
fn terminal_approver_denies_write_end_to_end() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "hi" }),
        )),
        Ok(AssistantTurn::text("understood")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\nn\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(!workspace.path.join("out.txt").exists());
    Ok(())
}

#[test]
fn allowed_malformed_args_reach_tool_validation() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("write", json!({ "path": "out.txt" }))),
        Ok(AssistantTurn::text("ok")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\ny\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert!(tool_result.content.contains("\"ok\":false"));
    assert!(!tool_result.content.contains("\"denied\":true"));
    Ok(())
}

#[test]
fn denied_malformed_args_return_denial_without_validation() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("write", json!({ "path": "out.txt" }))),
        Ok(AssistantTurn::text("ok")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write it\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
    Ok(())
}

#[test]
fn multiple_gated_calls_consume_one_decision_each() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    thought_signature: None,
                    name: "write".to_string(),
                    arguments: json!({ "path": "a.txt", "content": "a" }),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    thought_signature: None,
                    name: "write".to_string(),
                    arguments: json!({ "path": "b.txt", "content": "b" }),
                },
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "write both\ny\nn\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(workspace.path.join("a.txt").exists());
    assert!(!workspace.path.join("b.txt").exists());
    Ok(())
}

#[test]
fn always_allow_auto_approves_later_same_tool_calls_in_session() -> Result<()> {
    // The Nexus session allow-policy: one "always" decision auto-approves later
    // calls to the SAME tool. Exercised with a custom approval-requiring tool
    // that opts into allow-always; the built-in mutating tools (write/edit/bash)
    // deliberately opt OUT (see registry.rs), so the policy mechanism is tested
    // through a tool that participates in it. Only one decision line is
    // supplied; if the policy were not enforced in Nexus, the second call would
    // consume "/exit" as its decision.
    struct ApprovableTool;
    impl Tool for ApprovableTool {
        fn name(&self) -> &str {
            "approvable"
        }
        fn description(&self) -> &str {
            "approval-requiring tool that supports allow-always"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::text("ran")) })
        }
        fn requires_approval(&self) -> bool {
            true
        }
        // supports_allow_always defaults to true: this tool participates.
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    thought_signature: None,
                    name: "approvable".to_string(),
                    arguments: json!({}),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    thought_signature: None,
                    name: "approvable".to_string(),
                    arguments: json!({}),
                },
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(ApprovableTool)]),
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "do both\na\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    assert!(rendered.contains("You approved iris to run approvable"));
    // Both calls ran: the next provider request carries two ok tool results.
    let seen = harness.agent.provider.seen.borrow();
    let results: Vec<_> = seen[1].iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|m| m.content.contains("\"ok\":true")));
    Ok(())
}

#[test]
fn mutating_builtins_opt_out_of_allow_always() {
    // Fix: the mutating built-ins gate on approval but opt OUT of allow-always,
    // so a single "always" can never authorize arbitrary later effects. The UI
    // reads this classification to omit the "always" choice (tested in
    // ui::text); here we pin the registry classification itself.
    let tools = crate::tools::built_in_tools();
    for name in ["write", "edit", "bash"] {
        let tool = tools
            .by_name(name)
            .unwrap_or_else(|| panic!("{name} should be a built-in tool"));
        assert!(tool.requires_approval(), "{name} should require approval");
        assert!(
            !tool.supports_allow_always(),
            "{name} must opt out of allow-always so a session grant cannot authorize later effects"
        );
    }
}

#[test]
fn always_allow_does_not_cross_tool_boundaries() -> Result<()> {
    // "always" on one tool must not silently auto-approve a different tool. The
    // built-in mutating tools now opt out of allow-always (so none of them can
    // be the always-allowed example), so this uses two custom approval-requiring
    // tools: `alpha` participates in allow-always, `beta` must still prompt.
    struct AllowAlwaysTool;
    impl Tool for AllowAlwaysTool {
        fn name(&self) -> &str {
            "alpha"
        }
        fn description(&self) -> &str {
            "allow-always-capable tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::text("alpha-ran")) })
        }
        fn requires_approval(&self) -> bool {
            true
        }
    }
    struct GatedTool;
    impl Tool for GatedTool {
        fn name(&self) -> &str {
            "beta"
        }
        fn description(&self) -> &str {
            "approval-requiring tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move { Ok(ToolOutput::text("beta-ran")) })
        }
        fn requires_approval(&self) -> bool {
            true
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("alpha", json!({}))),
        Ok(single_call_turn("beta", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(AllowAlwaysTool), Box::new(GatedTool)]),
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    // alpha -> always (a); beta -> denied (n). beta must still prompt.
    run_text_session(
        &mut harness,
        "go\na\nn\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let seen = harness.agent.provider.seen.borrow();
    let beta_result = seen[2].last().unwrap();
    assert!(beta_result.content.contains("\"denied\":true"));
    Ok(())
}

#[test]
fn always_allow_does_not_auto_approve_bash() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    thought_signature: None,
                    name: "bash".to_string(),
                    arguments: json!({ "command": "echo first" }),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    thought_signature: None,
                    name: "bash".to_string(),
                    arguments: json!({ "command": "echo second" }),
                },
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "run both\na\nn\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    assert!(!rendered.contains("You approved iris to run echo second this session"));
    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
    Ok(())
}

// ---- ADR-0027: per-project permission policy -------------------------------

/// Like [`test_harness`], with a persistent project policy and grant sink
/// installed on the agent (the ADR-0027 "project" precedence layer).
fn test_harness_with_policy<P: ChatProvider>(
    provider: P,
    workspace: &Path,
    tools: Tools,
    policy: ProjectPolicy,
    sink: Option<Box<dyn ProjectPolicySink>>,
) -> Harness<P> {
    Harness::new(
        Agent::new(provider, tools).with_project_policy(policy, sink),
        workspace.to_path_buf(),
        ToolState::new(),
        None,
        None,
    )
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    trust_path: Option<OsString>,
}

impl EnvGuard {
    fn with_trust_path(path: &Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let guard = Self {
            _lock: lock,
            trust_path: std::env::var_os("IRIS_TRUST_PATH"),
        };
        // SAFETY: nexus env-sensitive tests run under ENV_LOCK and restore the
        // process-global var before releasing it.
        unsafe { std::env::set_var("IRIS_TRUST_PATH", path) };
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialized under ENV_LOCK by EnvGuard and restored on drop.
        unsafe {
            match &self.trust_path {
                Some(value) => std::env::set_var("IRIS_TRUST_PATH", value),
                None => std::env::remove_var("IRIS_TRUST_PATH"),
            }
        }
    }
}

/// Records every grant Nexus asks to persist, standing in for the Tier-2 store.
struct RecordingSink {
    grants: std::rc::Rc<RefCell<Vec<PolicyGrant>>>,
}

impl ProjectPolicySink for RecordingSink {
    fn persist(&self, grant: &PolicyGrant) -> Result<()> {
        self.grants.borrow_mut().push(grant.clone());
        Ok(())
    }
}

fn policy(tools: &[&str], bash_exact: &[&str], bash_prefix: &[&str]) -> ProjectPolicy {
    ProjectPolicy {
        tools: tools.iter().map(|t| t.to_string()).collect(),
        bash_exact: bash_exact.iter().map(|c| c.to_string()).collect(),
        bash_prefix: bash_prefix.iter().map(|p| p.to_string()).collect(),
    }
}

#[test]
fn invariant_1_only_the_injected_policy_grants_never_repo_files() -> Result<()> {
    // A cloned repo ships a policy file pre-approving `write`. Nexus consults
    // only the injected `ProjectPolicy` (loaded by the host from the HOME-owned
    // store; see wayland::trust::invariant_1_a_repo_shipped_policy_file_grants_nothing
    // for the store side). With an empty injected policy, the repo-shipped file
    // changes nothing: the gate is consulted and a Deny denies.
    let workspace = test_workspace()?;
    fs::create_dir_all(workspace.path.join(".iris"))?;
    fs::write(
        workspace.path.join(".iris/trust.json"),
        r#"{ "*": { "allow_tools": ["write", "edit"] } }"#,
    )?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "pwned.txt", "content": "x" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        ProjectPolicy::default(),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("write it", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events_at_review.borrow().is_some(),
        "the gate must be consulted: a repo-shipped policy file grants nothing"
    );
    assert!(
        !workspace.path.join("pwned.txt").exists(),
        "the denied write must not run"
    );
    Ok(())
}

#[test]
fn invariant_2_destructive_bash_reprompts_despite_project_grants() -> Result<()> {
    // Both an exact grant for the destructive command AND a covering prefix
    // grant are stored; the destructive floor still forces the prompt.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "bash",
            json!({ "command": "/bin/rm -rf sub" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        policy(&[], &["/bin/rm -rf sub"], &["/bin/rm"]),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("clean up", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events_at_review.borrow().is_some(),
        "a destructive command must re-prompt even when granted"
    );
    // The gate receives the structured facts, not UI copy: the destructive
    // floor is threaded through `ReviewContext` (issue #262/ADR-0010).
    let ctx = frontend
        .last_ctx
        .borrow()
        .clone()
        .expect("the gate received a review context");
    assert!(ctx.destructive, "destructive fact is threaded to the gate");
    assert!(
        ctx.dirty_paths.is_empty(),
        "no dirty-tree gate fired for this call"
    );
    let events = frontend.events.borrow();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentEvent::ToolAutoApproved(_))),
        "a destructive command must never auto-approve"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Notice(message) if message.contains("destructive command")
        )),
        "the forced re-prompt is explained"
    );
    Ok(())
}

#[test]
fn invariant_2_prefix_grant_does_not_auto_approve_destructive_compound_suffix() -> Result<()> {
    // A safe-looking granted prefix must not auto-approve a compound command
    // whose suffix is path-qualified destructive bash (`/bin/rm`).
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "bash",
            json!({ "command": "git status; /bin/rm -rf sub" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        policy(&[], &[], &["git"]),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("status", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events_at_review.borrow().is_some(),
        "a destructive suffix under a granted prefix must still prompt"
    );
    assert!(
        frontend
            .events
            .borrow()
            .iter()
            .all(|event| !matches!(event, AgentEvent::ToolAutoApproved(_))),
        "a granted prefix cannot bypass the destructive floor"
    );
    Ok(())
}

#[test]
fn invariant_2_allow_project_on_a_destructive_call_is_never_persisted() -> Result<()> {
    // Defense in depth: even if a front-end answers AllowProject for a
    // destructive call (the option is not offered), the call runs once and no
    // grant is applied or persisted.
    let workspace = test_workspace()?;
    fs::create_dir_all(workspace.path.join("sub"))?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "bash",
            json!({ "command": "mkfs.ext4 /dev/sdz" }),
        )),
        Ok(single_call_turn(
            "bash",
            json!({ "command": "mkfs.ext4 /dev/sdz" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let grants = std::rc::Rc::new(RefCell::new(Vec::new()));
    let sink = RecordingSink {
        grants: grants.clone(),
    };
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        ProjectPolicy::default(),
        Some(Box::new(sink)),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::AllowProject);

    block_on(harness.submit_turn("clean up", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        grants.borrow().is_empty(),
        "a destructive allow must never be persisted: {:?}",
        grants.borrow()
    );
    let events = frontend.events.borrow();
    // The second identical call must not auto-approve: nothing stuck in-memory
    // either.
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentEvent::ToolAutoApproved(_))),
        "no auto-approval may follow a refused destructive grant"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Notice(message) if message.contains("cannot be granted")
        )),
        "the refused grant is explained"
    );
    Ok(())
}

#[test]
fn project_grant_for_write_auto_approves_without_prompting() -> Result<()> {
    // The #209 core: a persisted per-project `write` grant auto-approves
    // without consulting the gate -- even though `write` opts out of the
    // session allow-always layer. (Cross-session persistence of the grant
    // itself is pinned in wayland::trust::grants_round_trip_and_persist_across_reads.)
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "note.txt", "content": "hello" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        policy(&["write"], &[], &[]),
        None,
    );
    // If the gate were consulted it would deny; the grant must bypass it.
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("write it", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events_at_review.borrow().is_none(),
        "a granted tool must not prompt"
    );
    let events = frontend.events.borrow();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolAutoApproved(_))),
        "the auto-approval is surfaced as an event"
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("note.txt"))?,
        "hello",
        "the granted write ran"
    );
    Ok(())
}

#[test]
fn project_bash_grants_match_exact_and_token_boundary_prefix() -> Result<()> {
    // An exact command grant and a prefix grant both auto-approve; the prefix
    // matches only at a token boundary.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("bash", json!({ "command": "echo hi" }))),
        Ok(single_call_turn(
            "bash",
            json!({ "command": "printf 'granted by prefix'" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        policy(&[], &["echo hi"], &["printf"]),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("run", &frontend, &frontend, &CancellationToken::new()))?;

    assert!(
        frontend.events_at_review.borrow().is_none(),
        "granted commands must not prompt"
    );
    let auto = frontend
        .events
        .borrow()
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolAutoApproved(_)))
        .count();
    assert_eq!(auto, 2, "both granted commands auto-approve");
    Ok(())
}

#[test]
fn invariants_3_and_4_grants_do_not_loosen_anything_ungranted() -> Result<()> {
    // A project policy loosens exactly what the user granted, nothing else
    // (ADR-0014: nothing self-waives). With `write` granted and `echo hi`
    // granted: `edit` still prompts, a lexically-adjacent bash command
    // (`echo hijack` does NOT match the `echo hi` exact grant, `printfx` does
    // NOT match a `printf` prefix grant) still prompts.
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello")?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "edit",
            json!({ "path": "note.txt", "oldText": "hello", "newText": "bye" }),
        )),
        Ok(single_call_turn(
            "bash",
            json!({ "command": "echo hijack" }),
        )),
        Ok(single_call_turn("bash", json!({ "command": "printfx" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        policy(&["write"], &["echo hi"], &["printf"]),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AgentEvent::ToolAutoApproved(_))),
        "nothing ungranted may auto-approve"
    );
    let denied = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolDenied(_)))
        .count();
    assert_eq!(denied, 3, "every ungranted call prompts and is denied");
    assert_eq!(
        fs::read_to_string(workspace.path.join("note.txt"))?,
        "hello",
        "the denied edit must not run"
    );
    Ok(())
}

#[test]
fn allow_project_persists_the_grant_and_covers_later_calls() -> Result<()> {
    // `[p]` at the prompt: the grant is persisted through the sink AND applied
    // in-memory, so the next identical call auto-approves without prompting.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "a.txt", "content": "1" }),
        )),
        Ok(single_call_turn(
            "write",
            json!({ "path": "b.txt", "content": "2" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let grants = std::rc::Rc::new(RefCell::new(Vec::new()));
    let sink = RecordingSink {
        grants: grants.clone(),
    };
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        ProjectPolicy::default(),
        Some(Box::new(sink)),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::AllowProject);

    block_on(harness.submit_turn(
        "write twice",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;

    assert_eq!(
        grants.borrow().as_slice(),
        &[PolicyGrant::Tool("write".to_string())],
        "exactly one grant is persisted"
    );
    let events = frontend.events.borrow();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolAutoApproved(_))),
        "the second write auto-approves from the fresh grant"
    );
    assert!(workspace.path.join("a.txt").exists());
    assert!(workspace.path.join("b.txt").exists());
    Ok(())
}

#[test]
fn allow_project_persists_to_disk_and_fresh_agent_auto_approves() -> Result<()> {
    // End-to-end ADR-0027 #209 path: a text approval prompt's `p` answer writes
    // the HOME-owned policy store through PolicyStoreSink; a fresh Agent loads
    // policy_for(cwd) and auto-approves a later write without consulting its
    // denial gate.
    let workspace = test_workspace()?;
    let store = test_workspace()?;
    let store_file = store.path.join("trust.json");
    let _env = EnvGuard::with_trust_path(&store_file);

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "a.txt", "content": "one" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        ProjectPolicy::default(),
        Some(Box::new(crate::wayland::trust::PolicyStoreSink::new(
            workspace.path.clone(),
        ))),
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();
    run_text_session(
        &mut harness,
        "go\np\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;
    assert!(workspace.path.join("a.txt").exists());
    assert!(
        String::from_utf8(output)?.contains("for this project"),
        "text prompt used the project-grant path"
    );

    let fresh_policy = crate::wayland::trust::policy_for(&workspace.path).to_policy();
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "b.txt", "content": "two" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        crate::tools::built_in_tools(),
        fresh_policy,
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    block_on(harness.submit_turn(
        "fresh write",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;

    assert!(
        frontend.events_at_review.borrow().is_none(),
        "fresh agent should auto-approve from persisted project policy"
    );
    assert_eq!(fs::read_to_string(workspace.path.join("b.txt"))?, "two");
    Ok(())
}

#[test]
fn precedence_session_project_then_global_prompt() -> Result<()> {
    // The three layers resolve independently and most-specific-first: an
    // allow-always-capable tool rides the session layer, a project-granted
    // tool rides the persistent project layer, and an ungranted gated tool
    // falls through to the global default (prompt).
    struct GatedNamed(&'static str);
    impl Tool for GatedNamed {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "gated test tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            let name = self.0;
            Box::pin(async move { Ok(ToolOutput::text(format!("{name}-ran"))) })
        }
        fn requires_approval(&self) -> bool {
            true
        }
        // alpha supports allow-always; the others use the default (true) too --
        // the layer split under test comes from the session/project state, not
        // the capability flag.
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("alpha", json!({}))),
        Ok(single_call_turn("alpha", json!({}))),
        Ok(single_call_turn("granted", json!({}))),
        Ok(single_call_turn("beta", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness_with_policy(
        provider,
        &workspace.path,
        Tools::new(vec![
            Box::new(GatedNamed("alpha")),
            Box::new(GatedNamed("granted")),
            Box::new(GatedNamed("beta")),
        ]),
        policy(&["granted"], &[], &[]),
        None,
    );
    let mut output = Vec::new();
    let mut errors = Vec::new();

    // alpha #1 -> always (session layer sticks); alpha #2 auto-approves;
    // `granted` auto-approves (project layer, consumes no prompt answer);
    // beta prompts (global default) -> denied.
    run_text_session(
        &mut harness,
        "go\na\nn\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    let rendered = String::from_utf8(output)?;
    assert!(
        rendered.contains("You approved iris to run alpha"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("approve granted"),
        "the project-granted tool must not prompt: {rendered}"
    );
    let seen = harness.agent.provider.seen.borrow();
    // The project-granted tool ran (its result reached provider context)...
    assert!(
        seen[3].iter().any(|m| m.content.contains("granted-ran")),
        "project-granted tool ran"
    );
    // ...and beta's tool result is a denial (global default prompted, user n).
    assert!(
        seen.last()
            .unwrap()
            .last()
            .unwrap()
            .content
            .contains("\"denied\":true"),
        "the ungranted tool falls through to the global default and is denied"
    );
    Ok(())
}

struct TestWorkspace {
    path: PathBuf,
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn assert_tool_error_contains(messages: &[Message], expected: &str) {
    let tool_result = messages.last().unwrap();
    assert_eq!(tool_result.role, Role::Tool);
    assert!(tool_result.content.contains("\"ok\":false"));
    assert!(tool_result.content.contains(expected));
}

fn read_file(workspace: &Path, path: &str) -> Result<String> {
    crate::tools::read_file(workspace, path)
}

fn test_workspace() -> Result<TestWorkspace> {
    // nanos alone collide across parallel tests; a process-unique counter
    // guarantees a distinct directory per call.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("iris-agent-test-{nanos}-{seq}"));
    fs::create_dir(&path)?;
    Ok(TestWorkspace { path })
}

#[test]
fn turn_persists_transcript_when_log_attached() -> Result<()> {
    let workspace = test_workspace()?;
    let root = test_workspace()?; // separate temp dir as the session root
    let provider = FakeProvider::new(vec![Ok(AssistantTurn::text("done"))]);
    let agent = Agent::new(provider, crate::tools::built_in_tools());
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let log_path = log.path().to_path_buf();
    // Persistence is a harness concern: construct it with the log attached.
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );

    let mut out = Vec::new();
    let mut err = Vec::new();
    run_text_session(&mut harness, b"hello\n/exit\n", &mut out, &mut err)?;

    let lines: Vec<String> = fs::read_to_string(&log_path)?
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 3, "header + user + assistant"); // {lines:?}
    assert!(lines[0].contains("\"type\":\"session\""));
    assert!(lines[1].contains("\"role\":\"user\"") && lines[1].contains("hello"));
    assert!(lines[2].contains("\"role\":\"assistant\"") && lines[2].contains("done"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Large tool output handles (issue #61): an oversized successful tool result is
// stored out of provider context behind a stable handle, with a compact preview
// in the transcript; small results stay inline; resume keeps the handle stable
// and never re-inlines the full payload.
// ---------------------------------------------------------------------------

/// Test tool that returns a caller-supplied body, so a test can drive a result
/// of any size through the real record/offload path.
struct BigTool {
    body: String,
}

impl Tool for BigTool {
    fn name(&self) -> &str {
        "big"
    }
    fn description(&self) -> &str {
        "emits a large output"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn execute<'a>(
        &'a self,
        _args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        let body = self.body.clone();
        Box::pin(async move { Ok(ToolOutput::text(body)) })
    }
}

/// An output comfortably over the inline threshold: a head marker, then filler,
/// a unique middle marker (which the head+tail preview must omit), more filler,
/// then a tail marker.
fn oversized_body() -> String {
    let filler = "lorem ipsum dolor sit amet filler line\n".repeat(800);
    let body = format!("HEAD-MARKER\n{filler}MIDDLE-SECRET-MARKER\n{filler}TAIL-END-MARKER");
    assert!(
        body.len() > MAX_INLINE_TOOL_OUTPUT_BYTES,
        "body must exceed the inline threshold"
    );
    body
}

/// Pull the offloaded handle id out of a tool-result JSON payload.
fn output_handle_id(tool_result_content: &str) -> String {
    let value: Value = serde_json::from_str(tool_result_content).unwrap();
    value["metadata"]["outputHandle"]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn oversized_tool_output_is_stored_behind_a_handle_and_compacted_in_context() -> Result<()> {
    let workspace = test_workspace()?;
    let root = test_workspace()?; // separate session root
    let body = oversized_body();

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("big", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![Box::new(BigTool { body: body.clone() })]),
    );
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let log_path = log.path().to_path_buf();
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    // The provider's follow-up request carries the compact result, not the full
    // payload: the middle of the output is omitted, the handle is referenced,
    // and the payload is far smaller than the original body.
    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert_eq!(tool_result.role, Role::Tool);
    assert!(
        !tool_result.content.contains("MIDDLE-SECRET-MARKER"),
        "the omitted middle must not reach provider context"
    );
    assert!(
        tool_result.content.contains("HEAD-MARKER"),
        "head preview kept"
    );
    assert!(
        tool_result.content.contains("TAIL-END-MARKER"),
        "tail preview kept"
    );
    let handle_events: Vec<_> = frontend
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::OutputHandleStored {
                provider_turn_id,
                call_id,
                handle_id,
                bytes,
                lines,
            } => Some((
                provider_turn_id.clone(),
                call_id.clone(),
                handle_id.clone(),
                *bytes,
                *lines,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(handle_events.len(), 1, "one handle event");
    assert_eq!(handle_events[0].0, "turn_00000000");
    assert_eq!(handle_events[0].1, "call_1");
    assert_eq!(handle_events[0].3, body.len());
    assert_eq!(handle_events[0].4, body.lines().count());
    assert!(
        !format!("{handle_events:?}").contains("MIDDLE-SECRET-MARKER"),
        "handle event must carry metadata only, not the full body"
    );
    assert!(tool_result.content.contains("outputHandle"));
    assert!(
        tool_result.content.len() < body.len(),
        "compacted result must be smaller than the full output"
    );

    // The handle metadata records the true size, and the full output round-trips
    // from the store by handle -- nothing is truncated and discarded.
    let parsed: Value = serde_json::from_str(&tool_result.content)?;
    assert_eq!(parsed["metadata"]["outputHandle"]["bytes"], body.len());
    assert_eq!(
        parsed["metadata"]["outputHandle"]["lines"],
        body.lines().count()
    );
    let id = output_handle_id(&tool_result.content);
    let store = crate::handles::HandleStore::for_session(&log_path);
    assert_eq!(store.get(&id)?.as_deref(), Some(body.as_str()));
    Ok(())
}

#[test]
fn small_tool_output_stays_inline_unchanged() -> Result<()> {
    let workspace = test_workspace()?;
    let root = test_workspace()?;
    // A clearly sub-threshold body keeps the original inline encoding even with a
    // store attached: full content present, no handle.
    let body = "a small result\nwith two lines".to_string();

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("big", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![Box::new(BigTool { body: body.clone() })]),
    );
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert!(
        tool_result
            .content
            .contains("a small result\\nwith two lines")
    );
    assert!(!tool_result.content.contains("outputHandle"));
    Ok(())
}

#[test]
fn oversized_output_without_a_store_is_kept_inline_not_discarded() -> Result<()> {
    let workspace = test_workspace()?;
    let body = oversized_body();

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("big", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![Box::new(BigTool { body: body.clone() })]),
    );
    // No session log -> no handle store. The full output must stay inline rather
    // than be truncated and lost.
    let mut harness = Harness::new(agent, workspace.path.clone(), ToolState::new(), None, None);
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert!(
        tool_result.content.contains("MIDDLE-SECRET-MARKER"),
        "without a store the full output stays inline"
    );
    assert!(!tool_result.content.contains("outputHandle"));
    Ok(())
}

#[test]
fn offloaded_preview_is_safe_on_multibyte_boundaries() -> Result<()> {
    let workspace = test_workspace()?;
    let root = test_workspace()?;
    // A leading ASCII byte shifts every multibyte char off the preview byte caps
    // (4 KiB / 2 KiB), so clamp_head/clamp_tail must back off to a char boundary
    // rather than panic on a mid-char slice.
    let body = format!("x{}", "\u{1F600}".repeat(20_000));
    assert!(body.len() > MAX_INLINE_TOOL_OUTPUT_BYTES);

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("big", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![Box::new(BigTool { body: body.clone() })]),
    );
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let log_path = log.path().to_path_buf();
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    let tool_result = seen[1].last().unwrap();
    assert!(tool_result.content.contains("outputHandle"));
    // The stored bytes are exactly the original output, intact across the slice.
    let id = output_handle_id(&tool_result.content);
    let store = crate::handles::HandleStore::for_session(&log_path);
    assert_eq!(store.get(&id)?.as_deref(), Some(body.as_str()));
    Ok(())
}

// ---------------------------------------------------------------------------
// Handle dereference (issue #205): the model reads an offloaded output back into
// context via the `read_output` tool, paging through the same line-window as
// `read`, and a dereference result that itself exceeds the inline threshold is
// re-offloaded behind a fresh handle (no re-inlining loop).
// ---------------------------------------------------------------------------

/// A body of uniquely numbered lines, comfortably over the inline threshold, so
/// it is offloaded and `read_output` must page it. Each line is distinct so
/// paging assertions can pin exact line ranges.
fn numbered_oversized_body() -> String {
    let mut body = String::new();
    for i in 1..=6000 {
        body.push_str(&format!("L{i:05}-marker\n"));
    }
    assert!(
        body.len() > MAX_INLINE_TOOL_OUTPUT_BYTES,
        "body must exceed the inline threshold"
    );
    body
}

#[test]
fn read_output_pages_an_offloaded_handle_and_reoffloads_a_large_dereference() -> Result<()> {
    let workspace = test_workspace()?;
    let root = test_workspace()?;
    let body = numbered_oversized_body();

    // The handle id is content-addressed (SHA-256 of the body), independent of
    // the session path, so we can compute it up front and script the model's
    // `read_output` calls against the id the harness will mint for `big`.
    let precompute = test_workspace()?;
    let precompute_store = crate::handles::HandleStore::with_dir(precompute.path.join("outputs"));
    let expected_id = precompute_store.put(&body)?;

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("big", json!({}))),
        Ok(single_call_turn(
            "read_output",
            json!({ "handle_id": expected_id, "limit": 3 }),
        )),
        Ok(single_call_turn(
            "read_output",
            json!({ "handle_id": expected_id, "offset": 4, "limit": 3 }),
        )),
        Ok(single_call_turn(
            "read_output",
            json!({ "handle_id": expected_id, "limit": 6000 }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![
            Box::new(BigTool { body: body.clone() }),
            crate::tools::read_output_tool(),
        ]),
    );
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let log_path = log.path().to_path_buf();
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();

    // Request 2 carries the offloaded `big` result: the handle the model then
    // dereferences is exactly the content-addressed id we precomputed.
    let big_result = &seen[1].last().unwrap().content;
    assert_eq!(output_handle_id(big_result), expected_id);

    // Request 3 carries read_output#1 (limit 3): the first three lines, paged
    // with a `read`-style continuation notice, and small enough to stay inline.
    let page1 = &seen[2].last().unwrap().content;
    assert!(page1.contains("L00001-marker"), "page1={page1}");
    assert!(page1.contains("L00003-marker"));
    assert!(!page1.contains("L00004-marker"), "limit must window");
    assert!(page1.contains("Use offset=4 to continue"));
    assert!(
        !page1.contains("outputHandle"),
        "a small paged result stays inline, not re-offloaded"
    );

    // Request 4 carries read_output#2 (offset 4, limit 3): the continuation picks
    // up exactly where page 1 stopped.
    let page2 = &seen[3].last().unwrap().content;
    assert!(page2.contains("L00004-marker"), "page2={page2}");
    assert!(page2.contains("L00006-marker"));
    assert!(!page2.contains("L00001-marker"));
    assert!(!page2.contains("L00007-marker"));

    // Request 5 carries read_output#3 (limit 6000): the window byte-caps at ~50KB,
    // so the dereference result itself exceeds the inline threshold and is
    // offloaded again behind a fresh handle -- no re-inlining loop.
    let page3 = &seen[4].last().unwrap().content;
    assert!(
        page3.contains("outputHandle"),
        "a >50KB dereference result must be re-offloaded"
    );
    let reoffload_id = output_handle_id(page3);
    assert_ne!(
        reoffload_id, expected_id,
        "re-offload mints a new content-addressed handle"
    );
    let store = crate::handles::HandleStore::for_session(&log_path);
    assert!(
        store.get(&reoffload_id)?.is_some(),
        "the re-offloaded page is itself retrievable by handle"
    );

    Ok(())
}

#[test]
fn structured_result_contract_serializes_stable_success_error_denied_and_cancelled_shapes() {
    let mut metadata = serde_json::Map::new();
    metadata.insert("entries".to_string(), json!(2));
    metadata.insert("truncated".to_string(), json!(false));

    let success = ToolResultContract::success(ToolOutput {
        content: "listed".to_string(),
        metadata,
    })
    .into_wire_json();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&success).unwrap(),
        json!({ "ok": true, "content": "listed", "metadata": { "entries": 2, "truncated": false } })
    );

    let error = ToolResultContract::tool_error(anyhow!("boom")).into_wire_json();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&error).unwrap(),
        json!({ "ok": false, "error": "boom" })
    );

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ToolResultContract::denied().into_wire_json())
            .unwrap(),
        json!({ "ok": false, "error": "tool call denied by user", "denied": true })
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &ToolResultContract::cancelled().into_wire_json()
        )
        .unwrap(),
        json!({ "ok": false, "error": "tool call cancelled by user", "cancelled": true })
    );
}

#[test]
fn output_handle_metadata_contract_serializes_without_body_or_preview() {
    let handle = OutputHandleMetadata {
        id: "abc123".to_string(),
        bytes: 42,
        lines: 3,
    };

    assert_eq!(
        handle.to_value(),
        json!({ "id": "abc123", "bytes": 42, "lines": 3 })
    );
}

#[test]
fn offload_threshold_is_inclusive_inline_at_limit_offloads_above() -> Result<()> {
    // Direct unit test of the offload decision: at the threshold stays inline,
    // one byte over offloads. Exercises the boundary `success_tool_result_json`
    // branches without a full turn.
    let dir = test_workspace()?;
    let store = crate::handles::HandleStore::with_dir(dir.path.join("outputs"));

    let at_limit = ToolOutput::text("a".repeat(MAX_INLINE_TOOL_OUTPUT_BYTES));
    let (at_json, at_handle) = success_tool_result_json(Some(&store), at_limit);
    assert!(at_handle.is_none());
    assert!(
        !at_json.contains("outputHandle"),
        "a result exactly at the threshold stays inline"
    );

    let over_limit = ToolOutput::text("a".repeat(MAX_INLINE_TOOL_OUTPUT_BYTES + 1));
    let (over_json, over_handle) = success_tool_result_json(Some(&store), over_limit);
    assert!(over_handle.is_some());
    assert!(
        over_json.contains("outputHandle"),
        "one byte over the threshold offloads"
    );
    Ok(())
}

#[test]
fn empty_output_stays_inline() {
    let (out, handle) = success_tool_result_json(None, ToolOutput::text(""));
    assert!(handle.is_none());
    assert!(out.contains("\"ok\":true"));
    assert!(!out.contains("outputHandle"));
}

#[test]
fn offload_falls_back_to_inline_when_the_store_errors() {
    // A store whose `put` fails must not lose the payload: the full output is
    // kept inline rather than truncated and discarded.
    struct FailingStore;
    impl ToolOutputStore for FailingStore {
        fn put(&self, _content: &str) -> Result<String> {
            Err(anyhow!("disk full"))
        }
        fn get(&self, _id: &str) -> Result<Option<String>> {
            Ok(None)
        }
    }

    let body = "Z".repeat(MAX_INLINE_TOOL_OUTPUT_BYTES + 100);
    let (out, handle) =
        success_tool_result_json(Some(&FailingStore), ToolOutput::text(body.clone()));
    assert!(handle.is_none());
    assert!(
        out.contains(&body),
        "full output preserved inline on store failure"
    );
    assert!(!out.contains("outputHandle"));
}

#[test]
fn resume_keeps_the_handle_reference_and_does_not_reinline_large_output() -> Result<()> {
    let workspace = test_workspace()?;
    let root = test_workspace()?;
    let body = oversized_body();

    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("big", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![Box::new(BigTool { body: body.clone() })]),
    );
    let log = crate::session::SessionLog::create_in(&root.path, &workspace.path)?;
    let session_id = log.id().to_string();
    let log_path = log.path().to_path_buf();
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;
    drop(harness); // flush + close the transcript

    // Reopen the session from disk: the rebuilt context carries the compact
    // handle reference, never the re-inlined full payload, and the handle is
    // still retrievable from the store.
    let store = crate::session::SessionStore::with_root(root.path.clone());
    let meta = store.find(&session_id)?.expect("session present");
    let stored = store.open(&meta)?;
    let tool_result = stored
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("a tool result is in the rebuilt context");
    assert!(
        !tool_result.content.contains("MIDDLE-SECRET-MARKER"),
        "resume must not re-inline the offloaded payload"
    );
    assert!(tool_result.content.contains("outputHandle"));

    let id = output_handle_id(&tool_result.content);
    let handles = crate::handles::HandleStore::for_session(&log_path);
    assert_eq!(handles.get(&id)?.as_deref(), Some(body.as_str()));
    Ok(())
}

// ---------------------------------------------------------------------------
// Runtime tests: async streaming, cancellation races, and safe-parallel /
// exclusive tool scheduling. These exercise the Codex-style runtime mechanics
// added on top of pi-mono's loop shape.
// ---------------------------------------------------------------------------

use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

/// Provider that streams one text delta and then never completes, so a turn only
/// ends if cancellation is raced against the pending stream read.
struct BlockingStreamProvider;
impl ChatProvider for BlockingStreamProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let head =
            futures::stream::once(async { Ok(ProviderEvent::TextDelta("partial".to_string())) });
        let tail = futures::stream::pending::<Result<ProviderEvent>>();
        Ok(Box::pin(head.chain(tail)))
    }
}

/// Tool that awaits before returning, proving the loop awaits async tools.
struct SlowTool;
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "awaits then returns"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn execute<'a>(
        &'a self,
        _args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            Ok(ToolOutput::text("slept"))
        })
    }
}

/// Tool that records it received a live cancellation token, waits for it to fire,
/// then returns an error. Proves child-token delivery + prompt tool abort.
struct CancelAwareTool {
    started: Arc<AtomicBool>,
    saw_cancel: Arc<AtomicBool>,
}
impl Tool for CancelAwareTool {
    fn name(&self) -> &str {
        "cancelaware"
    }
    fn description(&self) -> &str {
        "waits for cancellation"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn execute<'a>(
        &'a self,
        _args: &'a Value,
        _env: &'a ToolEnv<'_>,
        cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            self.started.store(true, AtomicOrdering::SeqCst);
            cancel.cancelled().await;
            self.saw_cancel.store(true, AtomicOrdering::SeqCst);
            Err(anyhow!("tool observed cancellation"))
        })
    }
}

/// Tool that records peak concurrency. `active`/`peak` are shared so a test can
/// observe whether two calls overlapped. Echoes its `tag` argument so result
/// ordering is checkable. `safe` controls concurrency-safety.
struct ProbeTool {
    tool_name: String,
    safe: bool,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        &self.tool_name
    }
    fn description(&self) -> &str {
        "concurrency probe"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "tag": { "type": "string" } } })
    }
    fn is_concurrency_safe(&self) -> bool {
        self.safe
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let current = self.active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.peak.fetch_max(current, AtomicOrdering::SeqCst);
            // Yield so a concurrent peer can also enter before we leave.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            self.active.fetch_sub(1, AtomicOrdering::SeqCst);
            let tag = args.get("tag").and_then(Value::as_str).unwrap_or("");
            Ok(ToolOutput::text(format!("{}:{tag}", self.tool_name)))
        })
    }
}

/// Approval gate whose `review` future never resolves, standing in for a
/// *cancellable* pending approval (one the executor can poll). It lets a test
/// prove the loop races a pending approval against cancellation; it is NOT the
/// real terminal gate, whose stdin read is blocking and cannot be preempted.
struct BlockingApprovalGate;
impl AgentObserver for BlockingApprovalGate {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
    }
}
impl ApprovalGate for BlockingApprovalGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async move {
            futures::future::pending::<()>().await;
            Ok(ApprovalDecision::Allow)
        })
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        thought_signature: None,
        name: name.to_string(),
        arguments,
    }
}

#[test]
fn streamed_events_reach_observer_in_order() -> Result<()> {
    // A provider that streams two deltas then completes: the observer must see
    // the deltas in order, a single committed end event, and a correct turn.
    let workspace = test_workspace()?;
    let mut harness = test_harness(
        DeltaProvider,
        &workspace.path,
        crate::tools::built_in_tools(),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    assert_eq!(
        events[0],
        AgentEvent::ProviderTurnStarted {
            turn_id: "turn_00000000".to_string()
        }
    );
    assert_eq!(events[1], AgentEvent::AssistantTextDelta("Hel".to_string()));
    assert_eq!(events[2], AgentEvent::AssistantTextDelta("lo".to_string()));
    assert_eq!(events[3], AgentEvent::AssistantTextEnd("Hello".to_string()));
    assert_eq!(
        events[4],
        AgentEvent::ProviderTurnCompleted {
            turn_id: "turn_00000000".to_string(),
            response_id: None,
            usage: None,
            completion_reason: None,
        }
    );
    assert_eq!(events[5], AgentEvent::TurnComplete);
    assert_eq!(harness.agent.messages().last().unwrap().content, "Hello");
    Ok(())
}

#[test]
fn cancellation_during_provider_stream_exits_promptly_with_valid_state() -> Result<()> {
    let workspace = test_workspace()?;
    let mut harness = test_harness(
        BlockingStreamProvider,
        &workspace.path,
        crate::tools::built_in_tools(),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let token = CancellationToken::new();

    block_on(async {
        let turn = harness.submit_turn("go", &frontend, &frontend, &token);
        let canceller = async {
            // Cancel only once the first delta has actually streamed in.
            loop {
                let saw_delta = frontend
                    .events
                    .borrow()
                    .iter()
                    .any(|e| matches!(e, AgentEvent::AssistantTextDelta(_)));
                if saw_delta {
                    break;
                }
                tokio::task::yield_now().await;
            }
            token.cancel();
        };
        let (result, ()) = tokio::join!(turn, canceller);
        result
    })?;

    // Partial text is committed (transcript stays valid: user + assistant), and
    // an interrupt notice is emitted. No hang: reaching here is the proof.
    let messages = harness.agent.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "go");
    assert_eq!(
        messages[1],
        Message::assistant("partial").with_provider_turn_id("turn_00000000")
    );
    assert!(
        frontend
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(m) if m.contains("interrupted")))
    );
    Ok(())
}

#[test]
fn cancellation_before_tools_proposes_remaining_calls_before_cancelling() -> Result<()> {
    struct CancelParentTool {
        parent: CancellationToken,
    }
    impl Tool for CancelParentTool {
        fn name(&self) -> &str {
            "trip"
        }
        fn description(&self) -> &str {
            "cancels parent turn"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move {
                self.parent.cancel();
                Ok(ToolOutput::text("tripped"))
            })
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(AssistantTurn {
        text: None,
        reasoning: Vec::new(),
        tool_calls: vec![
            call("call_1", "trip", json!({})),
            call("call_2", "read", json!({ "path": "b.txt" })),
        ],
        response_id: None,
        usage: None,
        completion_reason: None,
    })]);
    let token = CancellationToken::new();
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(CancelParentTool {
            parent: token.clone(),
        })]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &token))?;

    let states: Vec<_> = frontend
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolLifecycle { call_id, state, .. } => Some((call_id.clone(), *state)),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        [
            ("call_1".to_string(), ToolEventState::Proposed),
            ("call_1".to_string(), ToolEventState::Started),
            ("call_1".to_string(), ToolEventState::Cancelled),
            ("call_2".to_string(), ToolEventState::Proposed),
            ("call_2".to_string(), ToolEventState::Cancelled),
        ]
    );
    Ok(())
}

#[test]
fn cancelled_tool_outcome_emits_typed_cancelled_event_not_tool_error() -> Result<()> {
    struct AlreadyCancelledTool;
    impl Tool for AlreadyCancelledTool {
        fn name(&self) -> &str {
            "cancelme"
        }
        fn description(&self) -> &str {
            "cancelled test tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async move {
                cancel.cancel();
                Ok(ToolOutput::text("should not display"))
            })
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("cancelme", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(AlreadyCancelledTool)]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    assert!(
        events.iter().any(
            |event| matches!(event, AgentEvent::ToolCancelled(call) if call.name == "cancelme")
        ),
        "cancelled outcome should emit a typed display event: {events:#?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            AgentEvent::ToolError { message, .. } if message == "cancelled"
        )),
        "cancelled outcome must not be displayed as ToolError: {events:#?}"
    );
    Ok(())
}

#[test]
fn async_tool_result_feeds_follow_up_turn() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("slow", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(SlowTool)]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 2, "tool result must drive a follow-up turn");
    let tool_result = seen[1].last().unwrap();
    assert_eq!(tool_result.role, Role::Tool);
    assert!(tool_result.content.contains("slept"));
    assert_eq!(harness.agent.messages().last().unwrap().content, "done");
    Ok(())
}

#[test]
fn cancellation_during_tool_aborts_and_records_valid_result() -> Result<()> {
    let started = Arc::new(AtomicBool::new(false));
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("cancelaware", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(CancelAwareTool {
            started: Arc::clone(&started),
            saw_cancel: Arc::clone(&saw_cancel),
        })]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    let token = CancellationToken::new();

    block_on(async {
        let turn = harness.submit_turn("go", &frontend, &frontend, &token);
        let canceller = async {
            while !started.load(AtomicOrdering::SeqCst) {
                tokio::task::yield_now().await;
            }
            token.cancel();
        };
        let (result, ()) = tokio::join!(turn, canceller);
        result
    })?;

    assert!(
        saw_cancel.load(AtomicOrdering::SeqCst),
        "tool must receive the child cancellation token"
    );
    // Every emitted call gets a result: the tool's cooperative error is recorded
    // and the transcript ends valid.
    let messages = harness.agent.messages();
    let tool_result = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(tool_result.content.contains("\"ok\":false"));
    Ok(())
}

#[test]
fn cancelled_bash_records_cancelled_result_not_success() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![Ok(single_call_turn(
        "bash",
        json!({ "command": "sleep 30", "timeout": 30 }),
    ))]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    let token = CancellationToken::new();
    let trip = token.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        trip.cancel();
    });

    block_on(harness.submit_turn("run", &frontend, &frontend, &token))?;
    canceller.join().unwrap();

    let tool_result = harness
        .agent
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(
        tool_result.content.contains("\"cancelled\":true"),
        "expected cancelled payload, got: {}",
        tool_result.content
    );
    assert!(
        !tool_result.content.contains("\"ok\":true"),
        "cancelled bash must not be recorded as success: {}",
        tool_result.content
    );
    Ok(())
}

#[test]
fn unsafe_tools_run_sequentially() -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![
                call("c1", "probe", json!({ "tag": "a" })),
                call("c2", "probe", json!({ "tag": "b" })),
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(ProbeTool {
            tool_name: "probe".to_string(),
            safe: false,
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        })]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        peak.load(AtomicOrdering::SeqCst),
        1,
        "exclusive tools overlapped"
    );
    Ok(())
}

#[test]
fn safe_tools_run_in_parallel_with_ordered_results() -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![
                call("c1", "probe", json!({ "tag": "a" })),
                call("c2", "probe", json!({ "tag": "b" })),
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(ProbeTool {
            tool_name: "probe".to_string(),
            safe: true,
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        })]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        peak.load(AtomicOrdering::SeqCst),
        2,
        "two concurrency-safe tools should overlap"
    );
    // Results are recorded in the model's call order, not completion order.
    let seen = harness.agent.provider.seen.borrow();
    let results: Vec<&str> = seen[1]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        results[0].contains("probe:a"),
        "first result out of order: {results:?}"
    );
    assert!(
        results[1].contains("probe:b"),
        "second result out of order: {results:?}"
    );
    Ok(())
}

#[test]
fn auto_compaction_does_not_split_reasoning_from_retained_tool_calls() -> Result<()> {
    use crate::session::SessionLog;

    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let workspace = test_workspace()?;
    let root = test_workspace()?;
    let reasoning = "R".repeat(400);
    let origin = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: vec![ReasoningBlock::new(&reasoning, Some("sig"), false, origin)],
            tool_calls: vec![call("c1", "probe", json!({ "tag": "a" }))],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("after tool")),
        Ok(AssistantTurn::text("second done")),
    ]);
    let agent = Agent::new(
        provider,
        Tools::new(vec![Box::new(ProbeTool {
            tool_name: "probe".to_string(),
            safe: false,
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        })]),
    );
    let log = SessionLog::create_in(&root.path, &workspace.path)?;
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        Some(80),
    );

    run_text_session(
        &mut harness,
        b"first\nsecond\n/exit\n",
        &mut Vec::new(),
        &mut Vec::new(),
    )?;

    let seen = harness.agent.provider.seen.borrow();
    let second_turn_context = &seen[2];
    let call_idx = second_turn_context
        .iter()
        .position(|m| m.role == Role::AssistantToolCall && m.tool_call_id.as_deref() == Some("c1"))
        .expect("the first turn's tool call should be retained in the tail");
    assert!(
        second_turn_context[..call_idx]
            .iter()
            .any(|m| m.role == Role::AssistantReasoning && m.content == reasoning),
        "compaction retained a tool call without its preceding reasoning row"
    );
    Ok(())
}

#[test]
fn safe_tool_parallelism_is_uncapped() -> Result<()> {
    // There is no fixed parallelism cap: every parallelizable call in the batch
    // runs concurrently (pi-mono parity). Use a batch well past the old
    // hardcoded cap of 8 and prove the whole batch overlaps.
    const BATCH: usize = 12;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let workspace = test_workspace()?;
    let tool_calls = (0..BATCH)
        .map(|idx| {
            call(
                &format!("c{idx}"),
                "probe",
                json!({ "tag": idx.to_string() }),
            )
        })
        .collect();
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls,

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(ProbeTool {
            tool_name: "probe".to_string(),
            safe: true,
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        })]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let peak = peak.load(AtomicOrdering::SeqCst);
    assert!(peak > 8, "batch should exceed the old cap of 8: {peak}");
    assert_eq!(peak, BATCH, "the whole parallelizable batch should overlap");
    Ok(())
}

#[test]
fn safe_tools_do_not_cross_an_unsafe_tool() -> Result<()> {
    // [safe, safe, unsafe]: the safe pair overlaps (peak_safe == 2), but the
    // exclusive tool runs alone (peak_unsafe == 1), and results stay in order.
    let active = Arc::new(AtomicUsize::new(0));
    let peak_safe = Arc::new(AtomicUsize::new(0));
    let peak_unsafe = Arc::new(AtomicUsize::new(0));
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![
                call("c1", "safe", json!({ "tag": "a" })),
                call("c2", "safe", json!({ "tag": "b" })),
                call("c3", "danger", json!({ "tag": "c" })),
            ],

            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![
            Box::new(ProbeTool {
                tool_name: "safe".to_string(),
                safe: true,
                active: Arc::clone(&active),
                peak: Arc::clone(&peak_safe),
            }),
            Box::new(ProbeTool {
                tool_name: "danger".to_string(),
                safe: false,
                active: Arc::clone(&active),
                peak: Arc::clone(&peak_unsafe),
            }),
        ]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        peak_safe.load(AtomicOrdering::SeqCst),
        2,
        "safe pair did not overlap"
    );
    assert_eq!(
        peak_unsafe.load(AtomicOrdering::SeqCst),
        1,
        "exclusive tool ran alongside a peer"
    );
    let seen = harness.agent.provider.seen.borrow();
    let results: Vec<&str> = seen[1]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect();
    assert!(results[0].contains("safe:a"));
    assert!(results[1].contains("safe:b"));
    assert!(results[2].contains("danger:c"));
    Ok(())
}

// NOTE: this exercises the Nexus loop's approval/cancellation race with a
// *cancellable* gate (a future the executor can poll), not the real terminal
// prompt. The terminal `UiBridge::review` does a blocking stdin read that the
// single-threaded executor cannot preempt, so the first Ctrl-C does not
// interrupt a pending terminal prompt (the second force-quits at the process
// level). Real-terminal approval cancellation needs a non-blocking input layer
// (deferred; see ROADMAP).
#[test]
fn loop_cancels_a_pending_approval_with_a_cancellable_gate() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "hi" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let gate = BlockingApprovalGate;
    let token = CancellationToken::new();

    block_on(async {
        let turn = harness.submit_turn("write it", &gate, &gate, &token);
        let canceller = async {
            // Give the turn a chance to reach the pending approval, then cancel.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            token.cancel();
        };
        let (result, ()) = tokio::join!(turn, canceller);
        result
    })?;

    // The gated write never ran...
    assert!(!workspace.path.join("out.txt").exists());
    // ...and the call is recorded as cancelled (not denied), keeping the
    // transcript valid: the emitted tool call still has exactly one result.
    let messages = harness.agent.messages();
    let tool_result = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(
        tool_result.content.contains("cancelled"),
        "expected a cancelled tool result, got: {}",
        tool_result.content
    );
    Ok(())
}

/// A provider that echoes the first message it is given, so a test can prove
/// which conversation context reached the model. On a fresh session the first
/// message is the new prompt; on a resumed session it is the loaded history.
struct EchoFirstMessageProvider;

impl ChatProvider for EchoFirstMessageProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let first = messages
            .first()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(turn_stream(Ok(AssistantTurn::text(&first))))
    }
}

/// Resume end-to-end: a prior session is loaded from the store, its messages
/// seed the agent, and the next turn must see that prior context. The echo
/// provider returns the first message it received, so the resumed user fact
/// must come back out -- this fails if the loaded history was dropped. The
/// continued turns are also appended to the same log without duplicating the
/// loaded entries.
#[test]
fn resumed_session_feeds_prior_context_into_next_turn() -> Result<()> {
    use crate::session::{SessionLog, SessionStore};

    let dir = crate::tools::test_support::temp_dir();

    // A prior session with a memorable fact, then drop the live handle.
    let mut log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let id = log.id().to_string();
    log.append(&Message::user("the codeword is ostrich"))?;
    log.append(&Message::assistant("understood"))?;
    let path = log.path().to_path_buf();
    drop(log);

    // Resume: load the transcript and rebuild provider-visible context.
    let store = SessionStore::with_root(dir.path.clone());
    let meta = store.find(&id)?.expect("session id present in store");
    let stored = store.open(&meta)?;
    let resumed = stored.messages.len();
    assert_eq!(resumed, 2, "history reconstructed from the store");

    let agent = Agent::resumed(
        EchoFirstMessageProvider,
        crate::tools::built_in_tools(),
        stored.messages,
    );
    let session = SessionLog::resume(&path)?;
    let mut harness = Harness::resumed(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(session),
        resumed,
        None,
    );

    let mut out = Vec::new();
    let mut err = Vec::new();
    run_text_session(
        &mut harness,
        b"what is the codeword?\n/exit\n",
        &mut out,
        &mut err,
    )?;

    // The echoed first message is the resumed fact, not the new prompt -- proof
    // the prior context reached the next model turn.
    let rendered = String::from_utf8(out)?;
    assert!(
        rendered.contains("the codeword is ostrich"),
        "resumed context did not reach the provider; got: {rendered}"
    );

    // The continued turn was appended to the same log, not a new file, and the
    // loaded history was not rewritten: 2 loaded + new user + new assistant.
    let reopened = store.open(&meta)?;
    assert_eq!(reopened.messages.len(), 4);
    assert_eq!(reopened.messages[0].content, "the codeword is ostrich");
    assert_eq!(reopened.messages[2].content, "what is the codeword?");
    assert_eq!(reopened.messages[2].role, Role::User);
    Ok(())
}

/// A session whose last persisted entry is an unanswered tool call (a prior
/// crash between the call and its result) must resume into a provider-valid
/// sequence: the dangling call is paired with a synthetic result before the new
/// user prompt, and that repair is persisted to the same log.
#[test]
fn resume_repairs_a_dangling_tool_call_before_the_next_turn() -> Result<()> {
    use crate::session::{SessionLog, SessionStore};

    let dir = crate::tools::test_support::temp_dir();
    let mut log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let id = log.id().to_string();
    log.append(&Message::user("run the tool"))?;
    let call = ToolCall {
        id: "call_1".to_string(),
        thought_signature: None,
        name: "read".to_string(),
        arguments: serde_json::json!({ "path": "a.txt" }),
    };
    log.append(&Message::assistant_tool_call(&call).with_provider_turn_id("turn_00000005"))?; // dangling: no Tool result
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(dir.path.clone());
    let meta = store.find(&id)?.expect("session present");
    let stored = store.open(&meta)?;
    let on_disk = stored.messages.len();
    assert_eq!(on_disk, 2, "user + dangling tool call");

    // Capture exactly what the provider receives on the next turn.
    let provider = FakeProvider::new(vec![Ok(AssistantTurn::text("done"))]);
    let agent = Agent::resumed(provider, crate::tools::built_in_tools(), stored.messages);
    let session = SessionLog::resume(&path)?;
    let mut harness = Harness::resumed(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(session),
        on_disk,
        None,
    );

    let mut out = Vec::new();
    let mut err = Vec::new();
    run_text_session(&mut harness, b"continue\n/exit\n", &mut out, &mut err)?;

    // The reconstructed history pairs the dangling call with a Tool result, so
    // the provider never sees an unanswered tool call followed by a user turn.
    let seen = harness.agent.provider.seen.borrow();
    let first = seen.first().expect("provider was called");
    let call_idx = first
        .iter()
        .position(|m| m.role == Role::AssistantToolCall)
        .expect("tool call present");
    assert_eq!(
        first[call_idx + 1].role,
        Role::Tool,
        "dangling tool call must be answered before the next message"
    );

    // The synthetic result was persisted to the same log: reading it back also
    // yields a valid call/result pairing.
    let reopened = store.open(&meta)?;
    let idx = reopened
        .messages
        .iter()
        .position(|m| m.role == Role::AssistantToolCall)
        .unwrap();
    assert_eq!(reopened.messages[idx + 1].role, Role::Tool);
    assert_eq!(
        reopened.messages[idx + 1].provider_turn_id.as_deref(),
        Some("turn_00000005"),
        "synthetic repair result must keep the dangling call's provider turn id"
    );
    Ok(())
}

#[test]
fn resume_repairs_all_dangling_tool_calls_before_the_next_turn() -> Result<()> {
    use crate::session::{SessionLog, SessionStore};

    let dir = crate::tools::test_support::temp_dir();
    let mut log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let id = log.id().to_string();
    log.append(&Message::user("run both tools"))?;
    for call in [
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "a.txt" }),
        },
        ToolCall {
            id: "call_2".to_string(),
            thought_signature: None,
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "b.txt" }),
        },
    ] {
        log.append(&Message::assistant_tool_call(&call))?;
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(dir.path.clone());
    let meta = store.find(&id)?.expect("session present");
    let stored = store.open(&meta)?;
    let on_disk = stored.messages.len();
    let provider = FakeProvider::new(vec![Ok(AssistantTurn::text("done"))]);
    let agent = Agent::resumed(provider, crate::tools::built_in_tools(), stored.messages);
    let session = SessionLog::resume(&path)?;
    let mut harness = Harness::resumed(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(session),
        on_disk,
        None,
    );

    run_text_session(
        &mut harness,
        b"continue\n/exit\n",
        &mut Vec::new(),
        &mut Vec::new(),
    )?;

    let seen = harness.agent.provider.seen.borrow();
    let first = seen.first().expect("provider was called");
    let new_user_idx = first
        .iter()
        .position(|m| m.role == Role::User && m.content == "continue")
        .expect("new user prompt present");
    for id in ["call_1", "call_2"] {
        let result_idx = first
            .iter()
            .position(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("missing synthetic result for {id}"));
        assert!(
            result_idx < new_user_idx,
            "{id} must be answered before the next user prompt"
        );
    }
    Ok(())
}

/// Under budget: the harness must not create a compaction entry, and the
/// second turn still sees the prior context (no loss).
#[test]
fn under_budget_session_does_not_auto_compact() -> Result<()> {
    use crate::session::SessionLog;

    let dir = crate::tools::test_support::temp_dir();
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn::text("a")),
        Ok(AssistantTurn::text("b")),
    ]);
    let agent = Agent::new(provider, crate::tools::built_in_tools());
    let log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let log_path = log.path().to_path_buf();
    // A large budget two short turns stay well under: the policy runs each turn
    // but never fires.
    let mut harness = Harness::new(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(log),
        Some(1_000_000),
    );

    run_text_session(
        &mut harness,
        b"hi\nthere\n/exit\n",
        &mut Vec::new(),
        &mut Vec::new(),
    )?;

    let on_disk = fs::read_to_string(&log_path)?;
    assert!(
        !on_disk
            .lines()
            .any(|line| line.contains("\"type\":\"compaction\"")),
        "an under-budget session must not create a compaction entry"
    );
    // The second turn still received the first turn's context.
    assert_eq!(harness.agent.provider.seen.borrow().len(), 2);
    Ok(())
}

/// Over budget: at the second turn boundary the accumulated context exceeds the
/// budget, so the harness compacts before the provider request -- persisting a
/// compaction entry and opening the request with the summary instead of the
/// covered turns.
#[test]
fn over_budget_session_auto_compacts_at_turn_boundary() -> Result<()> {
    use crate::session::SessionLog;

    let dir = crate::tools::test_support::temp_dir();
    // ~100-token assistant replies and ~100-token user prompts, against a tiny
    // 50-token budget, so the second turn's boundary is over budget.
    let long = "R".repeat(400);
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn::text(&long)),
        Ok(AssistantTurn::text(&long)),
    ]);
    let agent = Agent::new(provider, crate::tools::built_in_tools());
    let log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let log_path = log.path().to_path_buf();
    let mut harness = Harness::new(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(log),
        Some(50),
    );

    let prompt_a = "P".repeat(400);
    let prompt_b = "Q".repeat(400);
    let input = format!("{prompt_a}\n{prompt_b}\n/exit\n");
    run_text_session(
        &mut harness,
        input.as_bytes(),
        &mut Vec::new(),
        &mut Vec::new(),
    )?;

    // The compaction entry was written automatically at a safe turn boundary.
    let on_disk = fs::read_to_string(&log_path)?;
    assert!(
        on_disk
            .lines()
            .any(|line| line.contains("\"type\":\"compaction\"")),
        "an over-budget session must persist a compaction entry"
    );

    // The second provider request opened with the summary, not the covered
    // turns, and never replays a covered message verbatim.
    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 2, "two provider requests");
    assert!(
        seen[1][0].content.starts_with("[auto-compacted summary"),
        "second request must open with the compaction summary, got: {}",
        seen[1][0].content
    );
    assert!(
        !seen[1].iter().any(|m| m.content == prompt_a),
        "covered turns must not be replayed as standalone messages"
    );
    Ok(())
}

#[test]
fn auto_compaction_emits_typed_event_with_ids_and_token_estimates() -> Result<()> {
    use crate::session::SessionLog;

    let dir = crate::tools::test_support::temp_dir();
    let long = "R".repeat(400);
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn::text(&long)),
        Ok(AssistantTurn::text(&long)),
    ]);
    let agent = Agent::new(provider, crate::tools::built_in_tools());
    let log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let mut harness = Harness::new(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(log),
        Some(50),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn(
        &"P".repeat(400),
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    block_on(harness.submit_turn(
        &"Q".repeat(400),
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;

    let events = frontend.events.borrow();
    let compaction = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::CompactionApplied {
                compaction_id,
                covered_from,
                covered_to,
                covered_messages,
                original_tokens_estimate,
                summary_tokens_estimate,
                budget,
            } => Some((
                compaction_id,
                covered_from,
                covered_to,
                *covered_messages,
                *original_tokens_estimate,
                *summary_tokens_estimate,
                *budget,
            )),
            _ => None,
        })
        .expect("compaction event");
    assert!(!compaction.0.is_empty());
    assert!(!compaction.1.is_empty());
    assert!(!compaction.2.is_empty());
    assert_eq!(compaction.3, 2);
    assert!(compaction.4 > compaction.5);
    assert_eq!(compaction.6, 50);
    Ok(())
}

/// Resume after auto-compaction: reopening a session that was auto-compacted
/// live rebuilds context through the summary, without duplicating the covered
/// turns -- the durable read-time view matches the live compacted context.
#[test]
fn resume_after_auto_compaction_rebuilds_through_the_summary() -> Result<()> {
    use crate::session::{SessionLog, SessionStore};

    let dir = crate::tools::test_support::temp_dir();
    let long = "R".repeat(400);
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn::text(&long)),
        Ok(AssistantTurn::text(&long)),
    ]);
    let agent = Agent::new(provider, crate::tools::built_in_tools());
    let log = SessionLog::create_in(&dir.path, Path::new("/w"))?;
    let id = log.id().to_string();
    let mut harness = Harness::new(
        agent,
        dir.path.clone(),
        ToolState::new(),
        Some(log),
        Some(50),
    );

    let prompt_a = "P".repeat(400);
    let prompt_b = "Q".repeat(400);
    let input = format!("{prompt_a}\n{prompt_b}\n/exit\n");
    run_text_session(
        &mut harness,
        input.as_bytes(),
        &mut Vec::new(),
        &mut Vec::new(),
    )?;
    drop(harness);

    // Reopen from disk: the read-time rebuild applies the auto-compaction entry.
    let store = SessionStore::with_root(dir.path.clone());
    let meta = store.find(&id)?.expect("session present in store");
    let stored = store.open(&meta)?;
    assert!(
        stored
            .messages
            .iter()
            .any(|m| m.content.starts_with("[auto-compacted summary")),
        "the rebuilt context must carry the auto-compaction summary"
    );
    assert!(
        !stored.messages.iter().any(|m| m.content == prompt_a),
        "covered turns must not be duplicated in the rebuilt context"
    );
    Ok(())
}

// --- Provider-specific tool surface planner (issue #60) ---------------------

/// Provider that reports configurable [`ProviderCapabilities`] and records the
/// model-visible tool names it is asked to advertise each turn, then returns
/// scripted turns. Proves `Agent::new` applies the surface plan (so providers
/// advertise the planned set) and that hidden tools stay executable.
struct SurfaceProbe {
    caps: ProviderCapabilities,
    advertised: RefCell<Vec<Vec<String>>>,
    responses: RefCell<Vec<Result<AssistantTurn, String>>>,
}

impl SurfaceProbe {
    fn new(caps: ProviderCapabilities, responses: Vec<Result<AssistantTurn, &str>>) -> Self {
        Self {
            caps,
            advertised: RefCell::new(Vec::new()),
            responses: RefCell::new(
                responses
                    .into_iter()
                    .map(|result| result.map_err(str::to_string))
                    .rev()
                    .collect(),
            ),
        }
    }
}

impl ChatProvider for SurfaceProbe {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.advertised
            .borrow_mut()
            .push(tools.iter().map(|tool| tool.name().to_string()).collect());
        let item = match self.responses.borrow_mut().pop() {
            Some(Ok(turn)) => Ok(turn),
            Some(Err(error)) => Err(error),
            None => Err("unexpected call".to_string()),
        };
        Ok(turn_stream(item))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.caps
    }
}

const FULL_SURFACE: [&str; 8] = [
    "read",
    "bash",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
    "read_output",
];

#[test]
fn surface_plan_defaults_to_full_built_in_surface() {
    // Default capabilities leave the model-visible surface identical to the
    // built-in declaration order -- the parity every existing provider relies on.
    let mut tools = crate::tools::built_in_tools();
    tools.plan_surface(&ProviderCapabilities::default());
    let visible: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(visible, FULL_SURFACE);
}

#[test]
fn native_edit_capability_hides_only_edit_but_keeps_it_executable() {
    let mut tools = crate::tools::built_in_tools();
    tools.plan_surface(&ProviderCapabilities { native_edit: true });

    let visible: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(
        visible,
        ["read", "bash", "write", "grep", "find", "ls", "read_output"]
    );
    assert!(
        !visible.contains(&"edit"),
        "edit must be hidden from the model"
    );
    // Safety invariant: hidden from declarations, still resolvable for execution.
    assert!(
        tools.by_name("edit").is_some(),
        "hidden tool must stay in the execution registry"
    );
}

#[test]
fn replace_provider_replans_the_tool_surface() {
    // A bare agent over the default (full) surface; swapping in a native-edit
    // provider must re-plan so `edit` is dropped from the model-visible surface,
    // while other tools stay visible and `edit` stays executable.
    let mut agent = Agent::new(
        SurfaceProbe::new(ProviderCapabilities::default(), Vec::new()),
        crate::tools::built_in_tools(),
    );
    assert!(
        agent.tools.iter().any(|tool| tool.name() == "edit"),
        "edit is visible under default capabilities"
    );

    agent.replace_provider(SurfaceProbe::new(
        ProviderCapabilities { native_edit: true },
        Vec::new(),
    ));
    assert!(
        !agent.tools.iter().any(|tool| tool.name() == "edit"),
        "replace_provider re-plans the surface and hides edit"
    );
    assert!(
        agent.tools.iter().any(|tool| tool.name() == "read"),
        "other tools stay visible after the swap"
    );
    assert!(
        agent.tools.by_name("edit").is_some(),
        "hidden edit is still executable"
    );
}

#[test]
fn default_provider_is_advertised_the_full_surface() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = SurfaceProbe::new(
        ProviderCapabilities::default(),
        vec![Ok(AssistantTurn::text("done"))],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    let advertised = harness.agent.provider.advertised.borrow();
    assert_eq!(advertised[0], FULL_SURFACE);
    Ok(())
}

#[test]
fn native_edit_provider_is_advertised_a_surface_without_edit() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = SurfaceProbe::new(
        ProviderCapabilities { native_edit: true },
        vec![Ok(AssistantTurn::text("done"))],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    let advertised = harness.agent.provider.advertised.borrow();
    assert_eq!(
        advertised[0],
        ["read", "bash", "write", "grep", "find", "ls", "read_output"]
    );
    assert!(!advertised[0].iter().any(|name| name == "edit"));
    Ok(())
}

#[test]
fn hidden_edit_tool_still_executes_when_the_model_calls_it() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "old\n")?;
    // The provider hides `edit` from its advertised surface, but the model calls
    // it anyway (e.g. a resumed transcript). Execution resolves by name over the
    // full registry, so the call runs and is gated normally rather than failing
    // as an unknown tool -- approval/execution stay decoupled from visibility.
    let provider = SurfaceProbe::new(
        ProviderCapabilities { native_edit: true },
        vec![
            Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
            Ok(single_call_turn(
                "edit",
                json!({ "file_path": "note.txt", "old_string": "old", "new_string": "new" }),
            )),
            Ok(AssistantTurn::text("done")),
        ],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("fix it", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolError { message, .. } if message.contains("unknown tool")
        )),
        "hidden edit must not be reported as an unknown tool"
    );
    // The edit ran and mutated the file, and the gate was consulted (gated path).
    assert_eq!(
        fs::read_to_string(workspace.path.join("note.txt"))?,
        "new\n"
    );
    assert!(frontend.events_at_review.borrow().is_some());
    // Every advertised surface this turn still omitted edit.
    assert!(
        harness
            .agent
            .provider
            .advertised
            .borrow()
            .iter()
            .all(|surface| !surface.iter().any(|name| name == "edit"))
    );
    Ok(())
}

#[test]
fn streaming_tool_deltas_stay_out_of_messages_and_exit_metadata_threads_through() -> Result<()> {
    // A tool that streams a chunk through the injected sink and reports exit
    // code + duration via metadata. Exercises the exclusive-path emitter, the
    // delta event, and record_call lifting the metadata onto ToolResult.
    struct StreamingTool;
    impl Tool for StreamingTool {
        fn name(&self) -> &str {
            "streamer"
        }
        fn description(&self) -> &str {
            "test streaming tool"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            let sink = env.output_sink;
            Box::pin(async move {
                if let Some(sink) = sink {
                    sink.emit_chunk("STREAMED_CHUNK");
                }
                Ok(ToolOutput::text("final output")
                    .with("exitCode", json!(7))
                    .with("durationMs", json!(123)))
            })
        }
    }

    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("streamer", json!({}))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(
        provider,
        &workspace.path,
        Tools::new(vec![Box::new(StreamingTool)]),
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("stream", &frontend, &frontend, &CancellationToken::new()))?;

    let events = frontend.events.borrow();
    // A display-only delta was emitted carrying the chunk.
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolOutputDelta { chunk, .. } if chunk == "STREAMED_CHUNK"
        )),
        "no ToolOutputDelta emitted"
    );
    // The final ToolResult carries exit code + duration lifted from metadata.
    let (exit_code, duration) = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolResult {
                exit_code,
                duration,
                ..
            } => Some((*exit_code, *duration)),
            _ => None,
        })
        .expect("a ToolResult event");
    assert_eq!(exit_code, Some(7));
    assert_eq!(duration, Some(Duration::from_millis(123)));

    // The streamed delta NEVER enters provider context, and the display-only
    // exec metadata is stripped from the wire (no exitCode/durationMs leak).
    for message in harness.agent.messages() {
        assert!(
            !message.content.contains("STREAMED_CHUNK"),
            "streamed delta leaked into provider context: {}",
            message.content
        );
        assert!(
            !message.content.contains("exitCode") && !message.content.contains("durationMs"),
            "display-only exec metadata leaked to the wire: {}",
            message.content
        );
    }
    Ok(())
}

// --- steering / follow-up mid-run message injection (pi-mono parity) ---

use std::collections::VecDeque;
use std::rc::Rc;

/// In-memory steering/follow-up queue test double. Implements the Tier-1
/// [`SteeringSource`] contract with the same FIFO drain-all policy as the real
/// Tier-3 `SteeringQueue`, kept here so the loop tests stay self-contained.
#[derive(Default)]
struct TestSteering {
    steering: RefCell<VecDeque<String>>,
    follow_up: RefCell<VecDeque<String>>,
}

impl TestSteering {
    fn push_steer(&self, text: &str) {
        self.steering.borrow_mut().push_back(text.to_string());
    }
    fn push_follow_up(&self, text: &str) {
        self.follow_up.borrow_mut().push_back(text.to_string());
    }
}

impl SteeringSource for TestSteering {
    fn take_steering(&self) -> Vec<String> {
        self.steering.borrow_mut().drain(..).collect()
    }
    fn take_follow_up(&self) -> Vec<String> {
        self.follow_up.borrow_mut().drain(..).collect()
    }
}

/// One enqueue a provider performs while a turn is in flight.
#[derive(Clone)]
enum Enqueue {
    Steer(String),
    Follow(String),
}

/// Provider that records `seen` like [`FakeProvider`] and, immediately before
/// answering a chosen call index, enqueues into the shared steering queue --
/// simulating the user typing while that provider turn streamed.
struct EnqueueingProvider {
    responses: RefCell<Vec<Result<AssistantTurn, String>>>,
    seen: RefCell<Vec<Vec<Message>>>,
    queue: Rc<TestSteering>,
    on_call: RefCell<Vec<Vec<Enqueue>>>,
    call: Cell<usize>,
}

impl EnqueueingProvider {
    fn new(
        responses: Vec<Result<AssistantTurn, &str>>,
        queue: Rc<TestSteering>,
        on_call: Vec<Vec<Enqueue>>,
    ) -> Self {
        Self {
            responses: RefCell::new(
                responses
                    .into_iter()
                    .map(|result| result.map_err(str::to_string))
                    .rev()
                    .collect(),
            ),
            seen: RefCell::new(Vec::new()),
            queue,
            on_call: RefCell::new(on_call),
            call: Cell::new(0),
        }
    }
}

impl ChatProvider for EnqueueingProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.seen.borrow_mut().push(messages.to_vec());
        let idx = self.call.get();
        self.call.set(idx + 1);
        let actions = self.on_call.borrow().get(idx).cloned().unwrap_or_default();
        for action in actions {
            match action {
                Enqueue::Steer(text) => self.queue.push_steer(&text),
                Enqueue::Follow(text) => self.queue.push_follow_up(&text),
            }
        }
        let item = match self.responses.borrow_mut().pop() {
            Some(Ok(turn)) => Ok(turn),
            Some(Err(error)) => Err(error),
            None => Err("unexpected call".to_string()),
        };
        Ok(turn_stream(item))
    }
}

#[test]
fn steering_injected_after_tool_round_reaches_next_provider_context() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello")?;
    let queue = Rc::new(TestSteering::default());
    let provider = EnqueueingProvider::new(
        vec![
            Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
            Ok(AssistantTurn::text("done")),
        ],
        queue.clone(),
        // The user types a steering message while the first (tool) turn runs.
        vec![vec![Enqueue::Steer("also check config".to_string())]],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("read note", &frontend, &frontend, &CancellationToken::new()))?;

    // The second provider call sees the steering message injected after the tool
    // result, before the model's next response.
    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1]
            .iter()
            .any(|m| m.role == Role::User && m.content == "also check config"),
        "steering must reach the next provider context: {:?}",
        seen[1]
    );
    // It is announced so the UI can render the row in transcript order.
    assert!(
        frontend
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::UserMessage(t) if t == "also check config"))
    );
    Ok(())
}

#[test]
fn follow_up_injected_when_agent_would_stop() -> Result<()> {
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    let provider = EnqueueingProvider::new(
        vec![
            Ok(AssistantTurn::text("working")),
            Ok(AssistantTurn::text("done")),
        ],
        queue.clone(),
        // The user queues a follow-up while the first (tool-less) response runs.
        vec![vec![Enqueue::Follow("now write tests".to_string())]],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 2, "a follow-up triggers a second provider turn");
    assert!(
        seen[1]
            .iter()
            .any(|m| m.role == Role::User && m.content == "now write tests"),
        "follow-up must reach the continued turn: {:?}",
        seen[1]
    );
    Ok(())
}

#[test]
fn no_queued_messages_ends_the_turn_without_a_second_call() -> Result<()> {
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    // Nothing is ever queued: the tool-less response ends the turn.
    let provider = EnqueueingProvider::new(
        vec![Ok(AssistantTurn::text("all done"))],
        queue.clone(),
        Vec::new(),
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        harness.agent.provider.seen.borrow().len(),
        1,
        "no queued messages means exactly one provider round trip"
    );
    assert!(
        frontend
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnComplete))
    );
    Ok(())
}

#[test]
fn would_stop_injects_steering_before_follow_up() -> Result<()> {
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    let provider = EnqueueingProvider::new(
        vec![
            Ok(AssistantTurn::text("a")),
            Ok(AssistantTurn::text("b")),
            Ok(AssistantTurn::text("c")),
        ],
        queue.clone(),
        // Both queued during the first (tool-less) response: steering injects
        // first (continuing the loop), the follow-up only at the next stop.
        vec![vec![
            Enqueue::Steer("steer first".to_string()),
            Enqueue::Follow("follow second".to_string()),
        ]],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[1].iter().any(|m| m.content == "steer first"),
        "steering injected before the second response: {:?}",
        seen[1]
    );
    assert!(
        !seen[1].iter().any(|m| m.content == "follow second"),
        "follow-up must NOT inject while steering is pending: {:?}",
        seen[1]
    );
    assert!(
        seen[2].iter().any(|m| m.content == "follow second"),
        "follow-up injected only at the next stop point: {:?}",
        seen[2]
    );
    Ok(())
}

#[test]
fn batched_steering_merges_into_one_user_message() -> Result<()> {
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    let provider = EnqueueingProvider::new(
        vec![
            Ok(AssistantTurn::text("a")),
            Ok(AssistantTurn::text("done")),
        ],
        queue.clone(),
        // Two steering messages queued during the same response: they drain
        // together and must merge into one user message, never two consecutive
        // user messages (which some providers reject).
        vec![vec![
            Enqueue::Steer("first".to_string()),
            Enqueue::Steer("second".to_string()),
        ]],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    // Exactly one merged user message was injected (no consecutive user rows).
    let injected: Vec<&Message> = harness
        .agent
        .messages()
        .iter()
        .filter(|m| m.role == Role::User && m.content != "go")
        .collect();
    assert_eq!(
        injected.len(),
        1,
        "batched steering merges into one message"
    );
    assert_eq!(injected[0].content, "first\n\nsecond");
    // No two consecutive user messages anywhere in the transcript.
    let messages = harness.agent.messages();
    assert!(
        !messages
            .windows(2)
            .any(|w| w[0].role == Role::User && w[1].role == Role::User),
        "transcript must not contain consecutive user messages: {messages:?}"
    );
    Ok(())
}

#[test]
fn cancellation_after_injection_drops_unanswered_user_message() -> Result<()> {
    // First response is tool-less, so the loop injects the queued follow-up and
    // continues. The next provider turn cancels before answering; the injected,
    // still-unanswered user message must be truncated so the transcript ends on
    // the assistant reply (no dangling trailing user message).
    struct CancelAfterInjection {
        token: CancellationToken,
        queue: Rc<TestSteering>,
        call: Cell<usize>,
    }
    impl ChatProvider for CancelAfterInjection {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let idx = self.call.get();
            self.call.set(idx + 1);
            if idx == 0 {
                // The user queues a follow-up during the (final) first response.
                self.queue.push_follow_up("late instruction");
                Ok(turn_stream(Ok(AssistantTurn::text("working"))))
            } else {
                // The continued turn is cancelled before it can answer.
                self.token.cancel();
                Ok(Box::pin(futures::stream::pending::<Result<ProviderEvent>>()))
            }
        }
    }

    let workspace = test_workspace()?;
    let token = CancellationToken::new();
    let queue = Rc::new(TestSteering::default());
    let provider = CancelAfterInjection {
        token: token.clone(),
        queue: queue.clone(),
        call: Cell::new(0),
    };
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &token))?;

    // The injected-but-unanswered follow-up is dropped: transcript ends on the
    // assistant reply, with no trailing user message and no duplicate prompt.
    let messages = harness.agent.messages();
    assert_eq!(messages.len(), 2, "messages: {messages:?}");
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].content, "hi");
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].content, "working");
    assert!(
        !messages.iter().any(|m| m.content == "late instruction"),
        "the unanswered injected message must be truncated on cancel"
    );
    Ok(())
}

#[test]
fn steering_queued_before_first_request_coalesces_into_prompt() -> Result<()> {
    // A steering message already queued when the turn starts (e.g. typed in the
    // submit/arm gap, or left by a cancellation race) is drained at the top of
    // the first loop iteration, where the trailing message is the prompt. It
    // must coalesce into that prompt, never push a second consecutive user
    // message (which some providers reject).
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    queue.push_steer("and prefer ripgrep");
    let provider = EnqueueingProvider::new(
        vec![Ok(AssistantTurn::text("ok"))],
        queue.clone(),
        Vec::new(),
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn(
        "search the code",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;

    let messages = harness.agent.messages();
    let users: Vec<&Message> = messages.iter().filter(|m| m.role == Role::User).collect();
    assert_eq!(users.len(), 1, "prompt + steering must be one user message");
    assert_eq!(users[0].content, "search the code\n\nand prefer ripgrep");
    assert!(
        !messages
            .windows(2)
            .any(|w| w[0].role == Role::User && w[1].role == Role::User),
        "no consecutive user messages: {messages:?}"
    );
    // The steering text is still announced as its own row for the transcript.
    assert!(
        frontend
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::UserMessage(t) if t == "and prefer ripgrep"))
    );
    Ok(())
}

#[test]
fn empty_completion_then_follow_up_does_not_make_consecutive_user_messages() -> Result<()> {
    // A content-less completion (no text, tools, or reasoning -- allowed by some
    // providers) pushes no assistant message. A follow-up injected at that
    // would-stop point must coalesce into the still-unanswered prompt rather
    // than appearing as a second consecutive user message.
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    let provider = EnqueueingProvider::new(
        vec![Ok(AssistantTurn::text("")), Ok(AssistantTurn::text("done"))],
        queue.clone(),
        vec![vec![Enqueue::Follow("please continue".to_string())]],
    );
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("hi", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(seen.len(), 2, "the follow-up drives a second provider turn");
    assert!(
        seen[1]
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("please continue")),
        "follow-up must reach the continued turn: {:?}",
        seen[1]
    );
    let messages = harness.agent.messages();
    assert!(
        !messages
            .windows(2)
            .any(|w| w[0].role == Role::User && w[1].role == Role::User),
        "no consecutive user messages after an empty completion: {messages:?}"
    );
    Ok(())
}

#[test]
fn soft_cap_does_not_strand_an_injected_follow_up() -> Result<()> {
    // The tool-roundtrip soft cap must not strand a would-stop follow-up: a
    // tool-less continuation does not count toward the cap, so the injected
    // message always gets a provider response and never dangles unanswered.
    const CAP: usize = 1;
    let workspace = test_workspace()?;
    let queue = Rc::new(TestSteering::default());
    let provider = EnqueueingProvider::new(
        vec![
            Ok(AssistantTurn::text("first")),
            Ok(AssistantTurn::text("answered")),
        ],
        queue.clone(),
        // Queue the follow-up during the first response; with CAP == 1 a path
        // that counted the continuation would return before answering it.
        vec![vec![Enqueue::Follow("keep going".to_string())]],
    );
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()).with_max_tool_roundtrips(Some(CAP)),
        workspace.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    harness.set_steering_source(queue.clone());
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    let seen = harness.agent.provider.seen.borrow();
    assert_eq!(
        seen.len(),
        2,
        "the follow-up still gets a provider response"
    );
    let messages = harness.agent.messages();
    assert_eq!(
        messages.last().map(|m| m.role),
        Some(Role::Assistant),
        "transcript must not end on an unanswered injected user message: {messages:?}"
    );
    Ok(())
}

#[test]
fn swap_session_switches_log_resets_context_and_cursor() -> Result<()> {
    use crate::session::SessionLog;
    let workspace = test_workspace()?;
    let root = workspace.path.join("sessions");
    // Two turns of provider responses, one before and one after the swap.
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn::text("first answer")),
        Ok(AssistantTurn::text("second answer")),
    ]);
    let log_a = SessionLog::create_in(&root, &workspace.path)?;
    let path_a = log_a.path().to_path_buf();
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        workspace.path.clone(),
        ToolState::new(),
        Some(log_a),
        None,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    block_on(harness.submit_turn(
        "first prompt",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    assert!(fs::read_to_string(&path_a)?.contains("first prompt"));

    // Swap to a resumed session: a new log plus two preloaded messages already
    // "on disk" (resumed = 2).
    let log_b = SessionLog::create_in(&root, &workspace.path)?;
    let path_b = log_b.path().to_path_buf();
    let preload = vec![
        Message::user("resumed prompt"),
        Message::assistant("resumed answer"),
    ];
    harness.swap_session(Some(log_b), preload, 2);

    // The agent context is now the resumed messages, not the pre-swap turn.
    let after_swap = harness.agent.messages();
    assert_eq!(
        after_swap.len(),
        2,
        "context replaced with resumed messages"
    );
    assert_eq!(after_swap[0].content, "resumed prompt");
    assert!(
        !after_swap.iter().any(|m| m.content == "first prompt"),
        "pre-swap context is gone: {after_swap:?}"
    );

    // The next turn appends only new messages to the new log; the resumed
    // prefix (persisted cursor = 2) is not re-written, and the old log is
    // untouched.
    block_on(harness.submit_turn(
        "second prompt",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    let b_contents = fs::read_to_string(&path_b)?;
    assert!(
        b_contents.contains("second prompt"),
        "new turn lands in the swapped log"
    );
    assert!(
        !b_contents.contains("resumed prompt"),
        "the resumed prefix is not re-persisted (cursor honored): {b_contents}"
    );
    assert!(
        !fs::read_to_string(&path_a)?.contains("second prompt"),
        "the old session log receives no further turns"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Post-change verification loop (issue #265): after a turn that changed files,
// run a configured command as a NORMAL gated shell execution and iterate on
// failure. Fake-provider loop tests over a real Harness + scratch git repo.
// ---------------------------------------------------------------------------

use crate::config::VerificationConfig;

/// Init a scratch git repo (so the dirty-tree guard runs in git mode and a
/// failed verification leaves rollback points), returning the workspace.
fn verify_git_workspace() -> Result<TestWorkspace> {
    let workspace = test_workspace()?;
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&workspace.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    fs::write(workspace.path.join("committed.txt"), "base\n")?;
    git(&["add", "committed.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    Ok(workspace)
}

fn verify_harness(
    provider: FakeProvider,
    workspace: &Path,
    command: Option<&str>,
    max_attempts: u32,
) -> Harness<FakeProvider> {
    let mut harness = test_harness(provider, workspace, crate::tools::built_in_tools());
    harness.set_verification(Some(VerificationConfig {
        command: command.map(str::to_string),
        max_attempts,
    }));
    harness
}

/// The verification outcomes emitted, in order.
fn verification_outcomes(frontend: &RecordingFrontend) -> Vec<VerificationOutcome> {
    frontend
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Verification(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

/// A write turn (mutates) followed by a text turn (ends the model turn). Two
/// provider responses = one `submit_turn`.
fn write_then_done(path: &str, content: &str) -> Vec<Result<AssistantTurn, &'static str>> {
    vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": path, "content": content }),
        )),
        Ok(AssistantTurn::text("done")),
    ]
}

#[test]
fn verification_passes_on_first_try_without_retry() -> Result<()> {
    let workspace = verify_git_workspace()?;
    let provider = FakeProvider::new(write_then_done("start.txt", "hi\n"));
    // `true` exits 0; the counter file records that it ran exactly once.
    let mut harness = verify_harness(
        provider,
        &workspace.path,
        Some("printf x >> runs.log; true"),
        3,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("do it", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        verification_outcomes(&frontend),
        vec![VerificationOutcome::Passed { attempts: 1 }]
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("runs.log")).unwrap_or_default(),
        "x",
        "the verification command ran exactly once"
    );
    Ok(())
}

#[test]
fn verification_fails_then_model_fixes_and_passes() -> Result<()> {
    let workspace = verify_git_workspace()?;
    // Turn 1 writes start.txt (mutates). The retry writes ok.flag, which the
    // verify command tests for. Four responses = two `submit_turn`s.
    let mut responses = write_then_done("start.txt", "hi\n");
    responses.extend(write_then_done("ok.flag", "ok\n"));
    let provider = FakeProvider::new(responses);
    let mut harness = verify_harness(
        provider,
        &workspace.path,
        Some("printf x >> runs.log; test -f ok.flag"),
        3,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("do it", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        verification_outcomes(&frontend),
        vec![VerificationOutcome::Passed { attempts: 2 }],
        "failure fed back, model edited, re-run passed; attempts reported"
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("runs.log")).unwrap_or_default(),
        "xx",
        "verification ran twice (initial fail + post-fix pass)"
    );
    Ok(())
}

#[test]
fn verification_fails_after_exhausting_attempts_and_stays_rollbackable() -> Result<()> {
    let workspace = verify_git_workspace()?;
    let mut responses = write_then_done("a.txt", "a\n");
    responses.extend(write_then_done("b.txt", "b\n"));
    let provider = FakeProvider::new(responses);
    // Always fails, emitting a marker so the last output is preserved.
    let mut harness = verify_harness(
        provider,
        &workspace.path,
        Some("printf x >> runs.log; echo FAILMARKER; false"),
        2,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("do it", &frontend, &frontend, &CancellationToken::new()))?;

    let outcomes = verification_outcomes(&frontend);
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        VerificationOutcome::Failed {
            attempts,
            exit_code,
            last_output,
        } => {
            assert_eq!(*attempts, 2, "reported fail-after-N with N == cap");
            assert_eq!(*exit_code, Some(1));
            assert!(
                last_output.contains("FAILMARKER"),
                "last output preserved: {last_output:?}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(workspace.path.join("runs.log")).unwrap_or_default(),
        "xx",
        "verification ran exactly twice (the cap)"
    );
    // Verification did not settle the task: the model's edits remain
    // rollbackable (ADR-0028).
    assert!(
        !harness.checkpoint_restore_points().is_empty(),
        "a failed loop leaves the task unsettled and rollbackable"
    );
    Ok(())
}

/// Gate that denies one tool name and allows everything else, so the model's
/// mutation is approved but the verification shell command is denied.
struct DenyToolGate {
    deny: &'static str,
    events: RefCell<Vec<AgentEvent>>,
}

impl AgentObserver for DenyToolGate {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events.borrow_mut().push(event);
        Ok(())
    }
}

impl ApprovalGate for DenyToolGate {
    fn review<'a>(
        &'a self,
        call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        let decision = if call.name == self.deny {
            ApprovalDecision::Deny
        } else {
            ApprovalDecision::Allow
        };
        Box::pin(async move { Ok(decision) })
    }
}

#[test]
fn verification_command_denial_reports_skipped_by_denial() -> Result<()> {
    let workspace = verify_git_workspace()?;
    let provider = FakeProvider::new(write_then_done("start.txt", "hi\n"));
    let mut harness = verify_harness(
        provider,
        &workspace.path,
        Some("printf x >> runs.log; true"),
        3,
    );
    // Allow the model's `write`, deny the verification `bash`: same approval
    // gate as any shell command, no exemption.
    let gate = DenyToolGate {
        deny: "bash",
        events: RefCell::new(Vec::new()),
    };

    block_on(harness.submit_turn("do it", &gate, &gate, &CancellationToken::new()))?;

    let outcomes: Vec<VerificationOutcome> = gate
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Verification(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![VerificationOutcome::SkippedApprovalDenied],
        "denied verification is reported as skipped-by-denial, not dropped or passed"
    );
    assert!(
        !workspace.path.join("runs.log").exists(),
        "the denied command never executed"
    );
    Ok(())
}

#[test]
fn verification_unconfigured_reports_skipped_but_off_is_silent() -> Result<()> {
    // Engaged with no command -> skipped-unconfigured after a mutating turn.
    let workspace = verify_git_workspace()?;
    let provider = FakeProvider::new(write_then_done("start.txt", "hi\n"));
    let mut harness = verify_harness(provider, &workspace.path, None, 3);
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    block_on(harness.submit_turn("do it", &frontend, &frontend, &CancellationToken::new()))?;
    assert_eq!(
        verification_outcomes(&frontend),
        vec![VerificationOutcome::SkippedUnconfigured]
    );

    // Feature off (no verify block wired) -> no verification event at all,
    // preserving pre-#265 behavior.
    let workspace2 = verify_git_workspace()?;
    let provider2 = FakeProvider::new(write_then_done("start.txt", "hi\n"));
    let mut off = test_harness(provider2, &workspace2.path, crate::tools::built_in_tools());
    let frontend2 = RecordingFrontend::new(ApprovalDecision::Allow);
    block_on(off.submit_turn("do it", &frontend2, &frontend2, &CancellationToken::new()))?;
    assert!(
        verification_outcomes(&frontend2).is_empty(),
        "an unwired feature emits nothing"
    );
    Ok(())
}

#[test]
fn verification_never_executes_beyond_max_attempts() -> Result<()> {
    let workspace = verify_git_workspace()?;
    // The model keeps changing files each retry, but verification always fails;
    // the loop must still stop exactly at the cap.
    let mut responses = write_then_done("a.txt", "a\n");
    responses.extend(write_then_done("b.txt", "b\n"));
    responses.extend(write_then_done("c.txt", "c\n"));
    // A 4th change is scripted but must never be reached (cap == 3).
    responses.extend(write_then_done("d.txt", "d\n"));
    let provider = FakeProvider::new(responses);
    let mut harness = verify_harness(
        provider,
        &workspace.path,
        Some("printf x >> runs.log; false"),
        3,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("do it", &frontend, &frontend, &CancellationToken::new()))?;

    match verification_outcomes(&frontend).as_slice() {
        [
            VerificationOutcome::Failed {
                attempts: 3,
                exit_code: Some(1),
                ..
            },
        ] => {}
        other => panic!("expected a single fail after 3 attempts, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(workspace.path.join("runs.log")).unwrap_or_default(),
        "xxx",
        "verification ran exactly max_attempts (3) times, never more"
    );
    Ok(())
}

#[test]
fn verification_stops_when_model_makes_no_further_changes() -> Result<()> {
    let workspace = verify_git_workspace()?;
    // Turn 1 mutates; the retry turn produces only text (no further changes), so
    // the loop must stop instead of re-running verification (no retry storm).
    let mut responses = write_then_done("start.txt", "hi\n");
    responses.push(Ok(AssistantTurn::text("I cannot fix this")));
    let provider = FakeProvider::new(responses);
    let mut harness = verify_harness(
        provider,
        &workspace.path,
        Some("printf x >> runs.log; false"),
        5,
    );
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("do it", &frontend, &frontend, &CancellationToken::new()))?;

    match verification_outcomes(&frontend).as_slice() {
        [VerificationOutcome::Failed { attempts: 1, .. }] => {}
        other => panic!("expected a single fail after 1 attempt, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(workspace.path.join("runs.log")).unwrap_or_default(),
        "x",
        "verification ran once; no retry without a further change"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Epic #261 acceptance (Milestone 5, git-centered workflow): the whole
// dirty-tree safety + net-diff + rollback contract proven end to end over a
// real Harness driven by the fake provider. One task edits a clean tracked file
// and creates a new file while a user's dirty tracked file and untracked file
// sit untouched. Asserts, in one flow: (a) user files byte-identical through the
// task; (b) the net diff is scoped to exactly Iris's authored paths; (c)
// rollback restores Iris's paths byte-identically while user files stay intact;
// (d) settlement leaves the refs/iris/* namespace empty.
// ---------------------------------------------------------------------------

/// Init a scratch git repo with two committed tracked files, so the acceptance
/// test has real committed history (2+ files) plus room for a dirty and a clean
/// tracked file.
fn acceptance_git_workspace() -> Result<TestWorkspace> {
    let workspace = test_workspace()?;
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&workspace.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    // Neutralize any global ignore patterns for the harness's own git calls
    // (which do not scrub the env like this helper): the untracked user file
    // must actually be untracked-and-visible, not globally ignored.
    git(&["config", "core.excludesFile", "/dev/null"]);
    fs::write(workspace.path.join("alpha.txt"), "alpha base\n")?;
    fs::write(workspace.path.join("beta.txt"), "beta base\n")?;
    git(&["add", "alpha.txt", "beta.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    Ok(workspace)
}

/// Read `refs/iris/` in `root` and return its `git for-each-ref` output.
fn iris_refs(root: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["for-each-ref", "refs/iris/"])
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn epic_261_acceptance_end_to_end() -> Result<()> {
    let workspace = acceptance_git_workspace()?;
    let root = &workspace.path;

    // The user's uncommitted work before the task: a DIRTY tracked file (an
    // uncommitted modification of committed history) and an UNTRACKED file. Iris
    // must never touch either.
    let dirty = root.join("alpha.txt");
    let untracked = root.join("user_notes.txt");
    let dirty_pre = b"alpha base\nuser uncommitted edit\n".to_vec();
    let untracked_pre = b"user scratch notes\n".to_vec();
    fs::write(&dirty, &dirty_pre)?;
    fs::write(&untracked, &untracked_pre)?;

    // Iris's own targets: a CLEAN committed file it edits, and a new file it
    // creates. `clean_pre` is the committed (clean) content rollback must
    // restore.
    let clean = root.join("beta.txt");
    let created = root.join("gamma.txt");
    let clean_pre = b"beta base\n".to_vec();

    // The scripted task: read the clean tracked file (satisfies read-before-write),
    // edit it, then create a new file; a final text turn ends the model turn.
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("read", json!({ "path": "beta.txt" }))),
        Ok(single_call_turn(
            "write",
            json!({ "path": "beta.txt", "content": "beta base\niris edit\n" }),
        )),
        Ok(single_call_turn(
            "write",
            json!({ "path": "gamma.txt", "content": "iris created this\n" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, root, crate::tools::built_in_tools());
    // Allow whatever the gate asks on Iris's own targets. The user's dirty file
    // is never an Iris target, so it is never prompted for or approved.
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn(
        "edit beta and add gamma",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;

    // Iris's edits landed as scripted.
    assert_eq!(fs::read(&clean)?, b"beta base\niris edit\n");
    assert_eq!(fs::read(&created)?, b"iris created this\n");

    // (a) The user's dirty and untracked files are byte-identical to pre-task.
    assert_eq!(
        fs::read(&dirty)?,
        dirty_pre,
        "the dirty user file is untouched during the task"
    );
    assert_eq!(
        fs::read(&untracked)?,
        untracked_pre,
        "the untracked user file is untouched during the task"
    );

    // (b) The task net diff covers exactly Iris's authored paths -- the user's
    // dirty and untracked paths are absent, and their bytes never leak.
    let diff = harness.task_diff()?;
    let mut paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["beta.txt", "gamma.txt"],
        "the net diff is scoped to Iris's authored paths only: {paths:?}"
    );
    let unified = diff.unified();
    assert!(
        !unified.contains("user uncommitted edit"),
        "the user's dirty edit never leaks into the diff"
    );
    assert!(
        !unified.contains("user scratch notes"),
        "the untracked user file never leaks into the diff"
    );
    let gamma = diff
        .files
        .iter()
        .find(|f| f.path == "gamma.txt")
        .expect("gamma.txt in the diff");
    assert!(
        gamma.unified.contains("--- /dev/null"),
        "gamma.txt renders as a create"
    );
    let beta = diff
        .files
        .iter()
        .find(|f| f.path == "beta.txt")
        .expect("beta.txt in the diff");
    assert!(
        beta.unified.contains("+iris edit"),
        "beta.txt renders the edit"
    );

    // The git-backed checkpoint chain must actually have been exercised (not
    // the non-git fallback): refs exist under refs/iris/ before settlement.
    assert!(
        iris_refs(root).contains("refs/iris/checkpoints/"),
        "checkpoint chain refs exist pre-settlement: {:?}",
        iris_refs(root)
    );

    // (c) Rollback restores Iris's paths to their pre-task state byte-identically
    // -- the created file is gone, the edited file is back to its committed
    // content -- while the user's dirty/untracked work stays byte-identical.
    let outcome = harness.rollback_checkpoint(0)?;
    assert!(
        outcome.summary.contains("rolled back"),
        "rollback summary: {}",
        outcome.summary
    );
    assert!(
        !created.exists(),
        "the Iris-created file is removed on rollback"
    );
    assert_eq!(
        fs::read(&clean)?,
        clean_pre,
        "the Iris-edited file is restored to its pre-task content"
    );
    assert_eq!(
        fs::read(&dirty)?,
        dirty_pre,
        "the dirty user file remains byte-identical after rollback"
    );
    assert_eq!(
        fs::read(&untracked)?,
        untracked_pre,
        "the untracked user file remains byte-identical after rollback"
    );

    // (d) Settlement (the rollback) leaves the refs/iris/* namespace empty: the
    // checkpoint chain is torn down, no refs accumulate.
    assert!(
        iris_refs(root).is_empty(),
        "refs/iris/ is empty after settlement: {:?}",
        iris_refs(root)
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// ADR-0032 approval presets (strict / auto / never-ask) and safety floors.
//
// These drive the bare `Agent` directly with a custom `ToolEnv` so the auto and
// never-ask policy is exercised at the Nexus/tool-policy layer -- the enforcement
// point (ADR-0005) -- rather than through a front-end. A `FakeGuard` stands in
// for the dirty-tree `MutationGuard` so a "dirty" target is deterministic
// without a real git repo.
// ---------------------------------------------------------------------------

/// A `MutationGuard` test double: the paths passed to `protecting` are reported
/// as unapproved-protected until approved (mirroring a pre-existing dirty file).
struct FakeGuard {
    protected: Vec<PathBuf>,
    approved: RefCell<Vec<PathBuf>>,
    all_dirty: Cell<bool>,
}

impl FakeGuard {
    fn none() -> Self {
        Self {
            protected: Vec::new(),
            approved: RefCell::new(Vec::new()),
            all_dirty: Cell::new(false),
        }
    }
    fn protecting(paths: Vec<PathBuf>) -> Self {
        Self {
            protected: paths,
            approved: RefCell::new(Vec::new()),
            all_dirty: Cell::new(false),
        }
    }
}

impl MutationGuard for FakeGuard {
    fn note_mutation(&self) -> Option<String> {
        None
    }
    fn unapproved_protected(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        if self.all_dirty.get() {
            return Vec::new();
        }
        let approved = self.approved.borrow();
        paths
            .iter()
            .filter(|p| self.protected.contains(p) && !approved.contains(p))
            .cloned()
            .collect()
    }
    fn approve(&self, paths: &[PathBuf], all_dirty: bool) {
        if all_dirty {
            self.all_dirty.set(true);
        }
        self.approved.borrow_mut().extend_from_slice(paths);
    }
    fn before_exec(&self, _paths: &[PathBuf]) {}
    fn after_exec(&self, _approved: &[PathBuf], _expected: Option<&str>) -> Vec<PathBuf> {
        Vec::new()
    }
    fn restore(&self, _paths: &[PathBuf]) -> Result<()> {
        Ok(())
    }
}

/// Drive one turn against the bare agent with a custom `ToolEnv` (optionally a
/// dirty-tree guard). Uses the same current-thread `block_on` as the other
/// direct-call tests.
fn run_preset_turn(
    agent: &mut Agent<FakeProvider>,
    prompt: &str,
    workspace: &Path,
    guard: Option<&dyn MutationGuard>,
    frontend: &RecordingFrontend,
) -> Result<()> {
    let state = RefCell::new(ToolState::new());
    let env = ToolEnv {
        workspace,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: guard,
    };
    block_on(agent.submit_turn(
        prompt,
        frontend,
        frontend,
        &env,
        &CancellationToken::new(),
        None,
    ))
}

fn write_turn(path: &str, content: &str) -> Vec<Result<AssistantTurn, &'static str>> {
    // Leak the args into 'static values so the fixture matches FakeProvider's
    // signature; the test process is short-lived so this is fine.
    let args = json!({ "path": path, "content": content });
    vec![
        Ok(AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                thought_signature: None,
                name: "write".to_string(),
                arguments: args,
            }],
            response_id: None,
            usage: None,
            completion_reason: None,
        }),
        Ok(AssistantTurn::text("done")),
    ]
}

fn auto_approved_count(frontend: &RecordingFrontend) -> usize {
    frontend
        .events
        .borrow()
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolAutoApproved(_)))
        .count()
}

fn denied_count(frontend: &RecordingFrontend) -> usize {
    frontend
        .events
        .borrow()
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolDenied(_)))
        .count()
}

#[test]
fn strict_mode_still_prompts_for_write() -> Result<()> {
    // The default (strict) preset preserves the current behavior: a gated write
    // consults the approval gate rather than auto-running.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("out.txt", "new\n"));
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    assert_eq!(agent.approval_mode(), ApprovalMode::Strict);
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(
        frontend.events_at_review.borrow().is_some(),
        "strict mode must consult the approval gate for a write"
    );
    assert_eq!(auto_approved_count(&frontend), 0);
    Ok(())
}

#[test]
fn auto_mode_runs_clean_workspace_write_without_prompting() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("out.txt", "new\n"));
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);
    // Deny would refuse if the gate were consulted -- it must not be.
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(
        frontend.events_at_review.borrow().is_none(),
        "auto mode must NOT consult the gate for a clean in-workspace write"
    );
    assert_eq!(
        auto_approved_count(&frontend),
        1,
        "the write is auto-approved"
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("out.txt"))?,
        "new\n",
        "the auto-approved write actually ran"
    );
    Ok(())
}

#[test]
fn auto_mode_runs_clean_workspace_edit_without_prompting() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("code.txt"), "old line\n")?;
    let args =
        json!({ "file_path": "code.txt", "old_string": "old line", "new_string": "new line" });
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("edit", args)),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "edit it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    // The point of this test is the auto-approval ROUTING for a clean
    // in-workspace edit: the gate is not consulted and the call is auto-approved.
    // (Edit mechanics -- read-before-mutate, matching -- are covered elsewhere.)
    assert!(
        frontend.events_at_review.borrow().is_none(),
        "auto mode must NOT consult the gate for a clean in-workspace edit"
    );
    assert_eq!(auto_approved_count(&frontend), 1);
    Ok(())
}

#[test]
fn auto_mode_prompts_for_dirty_write_target() -> Result<()> {
    // The dirty-file floor overrides auto: a pre-existing dirty target still
    // routes through the approval gate even in auto mode.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("out.txt", "new\n"));
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::protecting(vec![PathBuf::from("out.txt")]);

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    let ctx = frontend.last_ctx.borrow();
    let ctx = ctx
        .as_ref()
        .expect("auto must consult the gate for a dirty target");
    assert!(
        !ctx.dirty_paths.is_empty(),
        "the dirty-tree gate fired and threaded the protected paths"
    );
    assert_eq!(auto_approved_count(&frontend), 0);
    Ok(())
}

#[test]
fn auto_mode_prompts_for_outside_workspace_write() -> Result<()> {
    // The auto target-in-workspace check keeps an escaping path on the prompt
    // path, independent of the runtime path-confinement opt-in.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("../escape.txt", "x\n"));
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(
        frontend.events_at_review.borrow().is_some(),
        "an outside-workspace write must NOT auto-run"
    );
    assert_eq!(auto_approved_count(&frontend), 0);
    Ok(())
}

#[test]
fn auto_mode_prompts_for_destructive_bash() -> Result<()> {
    // The destructive floor overrides auto: `rm -rf` still prompts in auto mode.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "bash",
            json!({ "command": "rm -rf build" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "clean",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    let ctx = frontend.last_ctx.borrow();
    let ctx = ctx
        .as_ref()
        .expect("destructive bash must consult the gate in auto");
    assert!(ctx.destructive, "the destructive floor fired");
    assert_eq!(auto_approved_count(&frontend), 0);
    Ok(())
}

#[test]
fn auto_mode_does_not_auto_run_plain_bash() -> Result<()> {
    // v1 floor: bash is never auto-approved (the sandbox preflight is deferred),
    // so even a clean, non-destructive shell command still prompts in auto.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn("bash", json!({ "command": "echo hi" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::none();

    run_preset_turn(&mut agent, "run", &workspace.path, Some(&guard), &frontend)?;

    assert!(
        frontend.events_at_review.borrow().is_some(),
        "unproven sandboxed bash must NOT auto-run in auto mode"
    );
    assert_eq!(auto_approved_count(&frontend), 0);
    Ok(())
}

#[test]
fn never_ask_denies_unresolved_write_prompt() -> Result<()> {
    // never-ask: a call that would prompt is denied without consulting the gate.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("out.txt", "new\n"));
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::NeverAsk);
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(
        frontend.events_at_review.borrow().is_none(),
        "never-ask must not consult (prompt) the gate"
    );
    assert_eq!(denied_count(&frontend), 1, "the unresolved call is denied");
    assert!(
        !workspace.path.join("out.txt").exists(),
        "the denied write never ran"
    );
    Ok(())
}

#[test]
fn never_ask_denies_destructive_bash() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "bash",
            json!({ "command": "rm -rf build" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut agent = Agent::new(provider, crate::tools::built_in_tools());
    agent.set_approval_mode(ApprovalMode::NeverAsk);
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "clean",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(frontend.events_at_review.borrow().is_none());
    assert_eq!(
        denied_count(&frontend),
        1,
        "destructive bash denied in never-ask"
    );
    Ok(())
}

#[test]
fn never_ask_honors_explicit_project_grant() -> Result<()> {
    // never-ask denies PROMPTS, not explicit grants: a project-granted, clean,
    // non-floor write still runs without prompting.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("out.txt", "new\n"));
    let mut policy = ProjectPolicy::default();
    policy.tools.insert("write".to_string());
    let mut agent =
        Agent::new(provider, crate::tools::built_in_tools()).with_project_policy(policy, None);
    agent.set_approval_mode(ApprovalMode::NeverAsk);
    let frontend = RecordingFrontend::new(ApprovalDecision::Deny);
    let guard = FakeGuard::none();

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(
        frontend.events_at_review.borrow().is_none(),
        "an explicit grant is not a prompt"
    );
    assert_eq!(
        auto_approved_count(&frontend),
        1,
        "the granted write auto-approves"
    );
    assert_eq!(denied_count(&frontend), 0);
    assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "new\n");
    Ok(())
}

#[test]
fn never_ask_floor_overrides_explicit_grant_for_dirty_target() -> Result<()> {
    // A floor still denies in never-ask even with an explicit grant: a project
    // grant cannot pre-approve a pre-existing dirty file (ADR-0032 dirty floor).
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(write_turn("out.txt", "new\n"));
    let mut policy = ProjectPolicy::default();
    policy.tools.insert("write".to_string());
    let mut agent =
        Agent::new(provider, crate::tools::built_in_tools()).with_project_policy(policy, None);
    agent.set_approval_mode(ApprovalMode::NeverAsk);
    let frontend = RecordingFrontend::new(ApprovalDecision::Allow);
    let guard = FakeGuard::protecting(vec![PathBuf::from("out.txt")]);

    run_preset_turn(
        &mut agent,
        "write it",
        &workspace.path,
        Some(&guard),
        &frontend,
    )?;

    assert!(frontend.events_at_review.borrow().is_none());
    assert_eq!(
        denied_count(&frontend),
        1,
        "the dirty floor denies in never-ask despite the project grant"
    );
    Ok(())
}

#[test]
fn approval_mode_parses_and_round_trips_tokens() {
    assert_eq!(ApprovalMode::parse("strict"), Some(ApprovalMode::Strict));
    assert_eq!(
        ApprovalMode::parse("on-request"),
        Some(ApprovalMode::Strict)
    );
    assert_eq!(ApprovalMode::parse("AUTO"), Some(ApprovalMode::Auto));
    assert_eq!(ApprovalMode::parse("never"), Some(ApprovalMode::NeverAsk));
    assert_eq!(
        ApprovalMode::parse("never-ask"),
        Some(ApprovalMode::NeverAsk)
    );
    assert_eq!(ApprovalMode::parse("bogus"), None);
    assert_eq!(ApprovalMode::Auto.as_token(), "auto");
    assert_eq!(ApprovalMode::default(), ApprovalMode::Strict);
}

#[test]
fn approval_mode_from_startup_setting_resolves_or_defaults() {
    // Absent -> today's default (posture unchanged).
    assert_eq!(
        ApprovalMode::from_startup_setting(None),
        ApprovalMode::Strict
    );
    // A valid token is applied.
    assert_eq!(
        ApprovalMode::from_startup_setting(Some("auto")),
        ApprovalMode::Auto
    );
    assert_eq!(
        ApprovalMode::from_startup_setting(Some("never")),
        ApprovalMode::NeverAsk
    );
    // An invalid token falls back to the default rather than changing posture.
    assert_eq!(
        ApprovalMode::from_startup_setting(Some("bogus")),
        ApprovalMode::Strict
    );
}

#[test]
fn approval_command_switches_session_mode_in_text_path() -> Result<()> {
    // End-to-end through the text session driver: `/approval auto` flips the
    // session preset at the inter-turn boundary, so the following clean
    // in-workspace write auto-runs without an approval prompt on stdin.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "out.txt", "content": "auto\n" }),
        )),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());

    let mut out = Vec::new();
    let mut err = Vec::new();
    // No approval answer is supplied on stdin: if auto did not engage, the write
    // would block on a prompt and the scripted provider turn would not complete.
    run_text_session(
        &mut harness,
        b"/approval auto\nwrite it\n/exit\n",
        &mut out,
        &mut err,
    )?;

    let rendered = String::from_utf8_lossy(&out);
    assert!(
        rendered.contains("approval mode set to auto"),
        "the command notice is shown: {rendered}"
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("out.txt"))?,
        "auto\n",
        "the auto-approved write ran without a prompt"
    );
    Ok(())
}
