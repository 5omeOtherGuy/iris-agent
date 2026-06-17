use super::*;
use crate::cli::run_session;
use crate::tools::ToolState;
use crate::ui::text::TextUi;
use crate::wayland::Harness;
use anyhow::anyhow;
use std::cell::{Cell, RefCell};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
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
    run_session(harness, &mut ui)?;
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
}

impl RecordingFrontend {
    fn new(decision: ApprovalDecision) -> Self {
        Self {
            events: RefCell::new(Vec::new()),
            decision: Cell::new(decision),
            events_at_review: RefCell::new(None),
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
    fn review<'a>(&'a self, _call: &'a ToolCall, _allow_always: bool) -> ApprovalFuture<'a> {
        let mut snapshot = self.events_at_review.borrow_mut();
        if snapshot.is_none() {
            *snapshot = Some(self.events.borrow().clone());
        }
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
    assert!(matches!(events[0], AgentEvent::ToolProposed(_)));
    assert!(matches!(events[1], AgentEvent::ToolResult { .. }));
    assert!(matches!(events[2], AgentEvent::AssistantText(_)));
    assert!(matches!(events[3], AgentEvent::TurnComplete));
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
    assert!(matches!(
        at_review.last(),
        Some(AgentEvent::DiffPreview { .. })
    ));

    let events = frontend.events.borrow();
    assert!(matches!(events[0], AgentEvent::DiffPreview { .. }));
    assert!(matches!(events[1], AgentEvent::ToolResult { .. }));
    assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "new\n");
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
    assert!(matches!(events[0], AgentEvent::ToolDenied(_)));
    // Malformed args must not preflight: the gate saw no events before deciding.
    assert!(
        frontend
            .events_at_review
            .borrow()
            .as_ref()
            .is_some_and(Vec::is_empty)
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
    assert_eq!(harness.agent.messages[1], Message::assistant("Hello"));
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
    assert_eq!(harness.agent.messages.len(), 3);
    assert_eq!(harness.agent.messages[0].content, "fail");
    assert_eq!(harness.agent.messages[1].content, "again");
    assert_eq!(harness.agent.messages[2].content, "recovered");
    Ok(())
}

#[test]
fn tool_loop_reads_workspace_file_and_returns_result_to_model() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({ "path": "note.txt" }),
            }],
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
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({ "path": "note.txt" }),
            }],
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
    // Result row carries the tool summary and a line count, then the body.
    assert!(rendered.contains("\u{2713} read note.txt"));
    assert!(rendered.contains("hello from file"));
    assert!(errors.is_empty());
    Ok(())
}

#[test]
fn tool_error_is_displayed_and_loop_continues() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "unknown".to_string(),
                arguments: json!({}),
            }],
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
    assert!(rendered.contains("\u{2717} unknown"));
    assert!(rendered.contains("error: unknown tool: unknown"));
    assert!(rendered.contains("assistant> recovered"));
    assert!(errors.is_empty());
    Ok(())
}

#[test]
fn tool_loop_stops_gracefully_at_roundtrip_limit() -> Result<()> {
    let workspace = test_workspace()?;
    fs::write(workspace.path.join("note.txt"), "hello from file")?;
    let repeated_call = || {
        Ok(AssistantTurn {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({ "path": "note.txt" }),
            }],
        })
    };
    let provider = FakeProvider::new((0..MAX_TOOL_ROUNDTRIPS).map(|_| repeated_call()).collect());
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    run_text_session(
        &mut harness,
        "read forever\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    // Hitting the guard ends the turn gracefully: a user-visible notice,
    // no provider error, and the REPL keeps running (it consumes /exit).
    let rendered = String::from_utf8(output)?;
    assert!(rendered.contains("stopped after"));
    assert!(errors.is_empty());
    // The provider is consulted exactly the capped number of times, then
    // the loop stops without one extra round-trip.
    assert_eq!(
        harness.agent.provider.seen.borrow().len(),
        MAX_TOOL_ROUNDTRIPS
    );
    Ok(())
}

#[test]
fn unknown_tool_call_returns_tool_error_to_model() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "unknown".to_string(),
                arguments: json!({}),
            }],
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
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({ "not_path": "note.txt" }),
            }],
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

    let result = tool_result_json(
        &read_file(&workspace.path, "missing.txt").map(crate::tools::ToolOutput::text),
    );

    assert!(result.contains("\"ok\":false"));
    assert!(result.contains("failed to resolve path"));
    Ok(())
}

fn single_call_turn(name: &str, arguments: Value) -> AssistantTurn {
    AssistantTurn {
        text: None,
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments,
        }],
    }
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
    assert!(rendered.contains("\u{2713} write out.txt"));
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
    assert!(rendered.contains("\u{2717} denied \u{b7} write out.txt"));
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
    assert!(result.content.contains("has not been read this session"));
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
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    arguments: json!({ "path": "a.txt", "content": "a" }),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "write".to_string(),
                    arguments: json!({ "path": "b.txt", "content": "b" }),
                },
            ],
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
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    arguments: json!({ "path": "a.txt", "content": "a" }),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "write".to_string(),
                    arguments: json!({ "path": "b.txt", "content": "b" }),
                },
            ],
        }),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    // A single "always" decision must satisfy both write calls. Only one
    // decision line is supplied; the second call must not read input. If the
    // policy were not enforced in Nexus, the second write would consume
    // "/exit" as its decision and b.txt would never be written.
    run_text_session(
        &mut harness,
        "write both\na\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(workspace.path.join("a.txt").exists());
    assert!(workspace.path.join("b.txt").exists());
    let rendered = String::from_utf8(output)?;
    assert!(rendered.contains("auto-approved \u{b7} write b.txt \u{b7} session"));
    Ok(())
}

#[test]
fn always_allow_does_not_cross_tool_boundaries() -> Result<()> {
    // "always" on write must not silently auto-approve a later bash call.
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(single_call_turn(
            "write",
            json!({ "path": "a.txt", "content": "a" }),
        )),
        Ok(single_call_turn("bash", json!({ "command": "echo hi" }))),
        Ok(AssistantTurn::text("done")),
    ]);
    let mut harness = test_harness(provider, &workspace.path, crate::tools::built_in_tools());
    let mut output = Vec::new();
    let mut errors = Vec::new();

    // write -> always (a); bash -> denied (n). bash must still prompt.
    run_text_session(
        &mut harness,
        "go\na\nn\n/exit\n".as_bytes(),
        &mut output,
        &mut errors,
    )?;

    assert!(workspace.path.join("a.txt").exists());
    let seen = harness.agent.provider.seen.borrow();
    let bash_result = seen[2].last().unwrap();
    assert!(bash_result.content.contains("\"denied\":true"));
    Ok(())
}

#[test]
fn always_allow_does_not_auto_approve_bash() -> Result<()> {
    let workspace = test_workspace()?;
    let provider = FakeProvider::new(vec![
        Ok(AssistantTurn {
            text: None,
            tool_calls: vec![
                ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "echo first" }),
                },
                ToolCall {
                    id: "call_2".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "echo second" }),
                },
            ],
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
    assert!(!rendered.contains("auto-approved · bash"));
    let seen = harness.agent.provider.seen.borrow();
    assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
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
    let mut harness = Harness::new(agent, workspace.path.clone(), ToolState::new(), Some(log));

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
    fn review<'a>(&'a self, _call: &'a ToolCall, _allow_always: bool) -> ApprovalFuture<'a> {
        Box::pin(async move {
            futures::future::pending::<()>().await;
            Ok(ApprovalDecision::Allow)
        })
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
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
    assert_eq!(events[0], AgentEvent::AssistantTextDelta("Hel".to_string()));
    assert_eq!(events[1], AgentEvent::AssistantTextDelta("lo".to_string()));
    assert_eq!(events[2], AgentEvent::AssistantTextEnd("Hello".to_string()));
    assert_eq!(events[3], AgentEvent::TurnComplete);
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
    assert_eq!(messages[1], Message::assistant("partial"));
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
            tool_calls: vec![
                call("c1", "probe", json!({ "tag": "a" })),
                call("c2", "probe", json!({ "tag": "b" })),
            ],
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
            tool_calls: vec![
                call("c1", "probe", json!({ "tag": "a" })),
                call("c2", "probe", json!({ "tag": "b" })),
            ],
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
fn safe_tool_parallelism_is_bounded() -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let workspace = test_workspace()?;
    let tool_calls = (0..MAX_PARALLEL_TOOL_CALLS + 2)
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
            tool_calls,
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
    assert!(peak > 1, "safe calls should still overlap");
    assert!(
        peak <= MAX_PARALLEL_TOOL_CALLS,
        "parallel batch exceeded cap: {peak}"
    );
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
            tool_calls: vec![
                call("c1", "safe", json!({ "tag": "a" })),
                call("c2", "safe", json!({ "tag": "b" })),
                call("c3", "danger", json!({ "tag": "c" })),
            ],
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
        name: "read".to_string(),
        arguments: serde_json::json!({ "path": "a.txt" }),
    };
    log.append(&Message::assistant_tool_call(&call))?; // dangling: no Tool result
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
    Ok(())
}
