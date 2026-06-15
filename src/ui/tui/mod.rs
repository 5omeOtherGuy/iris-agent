use std::io::{self, IsTerminal};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::approval::ApprovalDecision;
use crate::nexus::{Agent, ChatProvider, ToolCall};
use crate::ui::{TurnErrorKind, Ui, UiEvent};

pub(crate) enum TuiRequest {
    Event(UiEvent),
    Approval {
        call: ToolCall,
        reply: Sender<ApprovalDecision>,
    },
    Prompt {
        reply: Sender<Option<String>>,
    },
}

#[derive(Clone)]
pub(crate) struct TuiUiHandle {
    tx: Sender<TuiRequest>,
}

impl TuiUiHandle {
    pub(crate) fn new(tx: Sender<TuiRequest>) -> Self {
        Self { tx }
    }
}

impl Ui for TuiUiHandle {
    fn next_prompt(&mut self) -> Result<Option<String>> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(TuiRequest::Prompt { reply })
            .context("failed to request TUI prompt")?;
        rx.recv().context("TUI prompt channel closed")
    }

    fn emit(&mut self, event: UiEvent) -> Result<()> {
        self.tx
            .send(TuiRequest::Event(event))
            .context("failed to send TUI event")
    }

    fn request_approval(&mut self, call: &ToolCall) -> Result<ApprovalDecision> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(TuiRequest::Approval {
                call: call.clone(),
                reply,
            })
            .context("failed to request TUI approval")?;
        rx.recv().context("TUI approval channel closed")
    }
}

pub(crate) fn should_use_tui() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var_os("CI").is_none()
        && std::env::var_os("IRIS_TEXT").is_none()
}

pub(crate) fn run_tui_session<P>(agent: Agent<P>) -> Result<()>
where
    P: ChatProvider + Send + 'static,
{
    let mut stdout = io::stdout();
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let _guard = TerminalModeGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;
    terminal.clear()?;

    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut agent = agent;
        let mut ui = TuiUiHandle::new(tx);
        crate::cli::run_session(&mut agent, &mut ui)
    });

    let worker_result = run_event_loop(&mut terminal, rx, &worker);
    let join_result = worker
        .join()
        .map_err(|_| anyhow!("TUI worker thread panicked"))?;
    worker_result?;
    join_result
}

struct TerminalModeGuard;

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rx: Receiver<TuiRequest>,
    worker: &thread::JoinHandle<Result<()>>,
) -> Result<()> {
    let mut state = TuiState::default();

    loop {
        drain_requests(&rx, &mut state);
        terminal.draw(|frame| render_frame(frame, &state))?;

        if worker.is_finished() && state.prompt_reply.is_none() && state.approval.is_none() {
            break;
        }

        if state.approval.is_some() || state.prompt_reply.is_some() {
            if event::poll(Duration::from_millis(50))? {
                handle_key(event::read()?, &mut state)?;
            }
            continue;
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(request) => state.apply_request(request),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if worker.is_finished() {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn drain_requests(rx: &Receiver<TuiRequest>, state: &mut TuiState) {
    while let Ok(request) = rx.try_recv() {
        state.apply_request(request);
    }
}

fn handle_key(event: Event, state: &mut TuiState) -> Result<()> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }

    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(pending) = state.approval.take() {
            let _ = pending.reply.send(ApprovalDecision::Deny);
        }
        if let Some(reply) = state.prompt_reply.take() {
            let _ = reply.send(None);
        }
        state.status = "cancelled".to_string();
        return Ok(());
    }

    if let Some(pending) = state.approval.take() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let _ = pending.reply.send(ApprovalDecision::Allow);
                state.status =
                    format!("approved {}", crate::tool_display::summarize(&pending.call));
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                let _ = pending.reply.send(ApprovalDecision::Deny);
                state.status = format!("denied {}", crate::tool_display::summarize(&pending.call));
            }
            _ => {
                state.approval = Some(pending);
            }
        }
        return Ok(());
    }

    if state.prompt_reply.is_some() {
        match key.code {
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(reply) = state.prompt_reply.take() {
                    let _ = reply.send(None);
                }
            }
            KeyCode::Char(c) => state.input.push(c),
            KeyCode::Backspace => {
                state.input.pop();
            }
            KeyCode::Enter => {
                let prompt = std::mem::take(&mut state.input);
                state.transcript.push(format!("iris> {prompt}"));
                if let Some(reply) = state.prompt_reply.take() {
                    let _ = reply.send(Some(prompt));
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[derive(Default)]
pub(crate) struct TuiState {
    transcript: Vec<String>,
    streaming_line: Option<String>,
    input: String,
    status: String,
    approval: Option<PendingApproval>,
    prompt_reply: Option<Sender<Option<String>>>,
}

struct PendingApproval {
    call: ToolCall,
    reply: Sender<ApprovalDecision>,
}

impl TuiState {
    fn apply_request(&mut self, request: TuiRequest) {
        match request {
            TuiRequest::Event(event) => self.apply_event(event),
            TuiRequest::Approval { call, reply } => {
                self.status = format!(
                    "approve {}? press y or n",
                    crate::tool_display::summarize(&call)
                );
                self.approval = Some(PendingApproval { call, reply });
            }
            TuiRequest::Prompt { reply } => {
                self.prompt_reply = Some(reply);
                self.status = "enter prompt".to_string();
            }
        }
    }

    pub(crate) fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::SessionStarted => self
                .transcript
                .push("Iris MVP. Type /exit to quit.".to_string()),
            UiEvent::AssistantText(text) => self.transcript.push(format!("assistant> {text}")),
            UiEvent::AssistantTextDelta(delta) => {
                let line = self
                    .streaming_line
                    .get_or_insert_with(|| "assistant> ".to_string());
                line.push_str(&delta);
            }
            UiEvent::AssistantTextEnd(text) => {
                if let Some(line) = self.streaming_line.take() {
                    self.transcript.push(line);
                } else if !text.is_empty() {
                    self.transcript.push(format!("assistant> {text}"));
                }
            }
            UiEvent::ToolProposed(call) => self
                .transcript
                .push(crate::tool_display::proposed_line(&call)),
            UiEvent::DiffPreview { call: _, diff } => self.transcript.push(format!("diff> {diff}")),
            UiEvent::ToolDenied(call) => self
                .transcript
                .push(crate::tool_display::denied_line(&call)),
            UiEvent::ToolResult { call, content } => self
                .transcript
                .push(crate::tool_display::result_line(&call, &content)),
            UiEvent::ToolError { call, message } => self
                .transcript
                .push(crate::tool_display::error_line(&call, &message)),
            UiEvent::Notice(message) => self.transcript.push(format!("note: {message}")),
            UiEvent::TurnError { kind, message } => {
                if let Some(line) = self.streaming_line.take() {
                    self.transcript.push(line);
                }
                let prefix = match kind {
                    TurnErrorKind::Provider => "provider error",
                    TurnErrorKind::Auth => "auth error",
                };
                self.transcript.push(format!("{prefix}: {message}"));
            }
            UiEvent::TurnComplete => self.status = "turn complete".to_string(),
        }
    }
}

pub(crate) fn render_frame(frame: &mut Frame<'_>, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    let mut transcript = state.transcript.join("\n");
    if let Some(line) = &state.streaming_line {
        if !transcript.is_empty() {
            transcript.push('\n');
        }
        transcript.push_str(line);
    }
    let visible_transcript_lines = chunks[0].height.saturating_sub(2) as usize;
    let transcript_width = chunks[0].width.saturating_sub(2).max(1) as usize;
    let transcript_lines = wrapped_line_count(&transcript, transcript_width);
    let scroll = transcript_lines.saturating_sub(visible_transcript_lines) as u16;
    let body = Paragraph::new(transcript)
        .block(Block::default().title("Iris").borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(body, chunks[0]);

    let input_title = if state.approval.is_some() {
        "Approval"
    } else if state.status.is_empty() {
        "Prompt"
    } else {
        state.status.as_str()
    };
    let input_text = if let Some(pending) = &state.approval {
        format!(
            "approve {}? [y/N]",
            crate::tool_display::summarize(&pending.call)
        )
    } else {
        state.input.clone()
    };
    let input = Paragraph::new(input_text)
        .block(Block::default().title(input_title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);
}

fn wrapped_line_count(text: &str, width: usize) -> usize {
    text.lines()
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: json!({ "path": "note.txt", "content": "hi" }),
        }
    }

    #[test]
    fn handle_sends_events_and_blocks_for_replies() -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let mut handle = TuiUiHandle::new(tx);

        handle.emit(UiEvent::AssistantText("hi".to_string()))?;
        match rx.recv()? {
            TuiRequest::Event(UiEvent::AssistantText(text)) => assert_eq!(text, "hi"),
            _ => panic!("expected assistant event"),
        }

        let mut approval_handle = handle.clone();
        let approval_thread =
            thread::spawn(move || approval_handle.request_approval(&call("write")));
        match rx.recv()? {
            TuiRequest::Approval { reply, .. } => reply.send(ApprovalDecision::Allow)?,
            _ => panic!("expected approval request"),
        }
        assert_eq!(approval_thread.join().unwrap()?, ApprovalDecision::Allow);

        let prompt_thread = thread::spawn(move || handle.next_prompt());
        match rx.recv()? {
            TuiRequest::Prompt { reply } => reply.send(Some("hello".to_string()))?,
            _ => panic!("expected prompt request"),
        }
        assert_eq!(prompt_thread.join().unwrap()?, Some("hello".to_string()));
        Ok(())
    }

    #[test]
    fn turn_error_flushes_partial_streaming_line() {
        let mut state = TuiState::default();

        state.apply_event(UiEvent::AssistantTextDelta("partial".to_string()));
        state.apply_event(UiEvent::TurnError {
            kind: TurnErrorKind::Provider,
            message: "boom".to_string(),
        });

        assert_eq!(state.streaming_line, None);
        assert_eq!(state.transcript[0], "assistant> partial");
        assert_eq!(state.transcript[1], "provider error: boom");
    }

    #[test]
    fn renders_scripted_event_stream() -> Result<()> {
        let mut state = TuiState::default();
        state.apply_event(UiEvent::SessionStarted);
        state.apply_event(UiEvent::AssistantText("hello".to_string()));
        state.apply_event(UiEvent::ToolProposed(call("read")));
        state.apply_event(UiEvent::ToolResult {
            call: call("read"),
            content: "file body".to_string(),
        });
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| render_frame(frame, &state))?;

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("assistant"));
        assert!(rendered.contains("tool&gt;") || rendered.contains("tool>"));
        assert!(rendered.contains("file body"));
        Ok(())
    }
}
