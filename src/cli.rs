use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use crate::nexus::ChatProvider;
use crate::ui::tui::TuiUi;
use crate::ui::{Ui, UiBridge, UiEvent, slash};
use crate::wayland::Harness;

/// Entry point for the interactive session. Selects the front-end exactly as the
/// former `ui::tui::stdio()` did -- the persistent full-screen TUI when both
/// stdin and stdout are terminals, otherwise the blocking text UI for pipes/CI
/// -- and runs the matching driver.
pub(crate) fn run_interactive<P: ChatProvider>(harness: &mut Harness<P>) -> Result<()> {
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        match TuiUi::new() {
            Ok(tui) => return run_tui(harness, tui),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "TUI unavailable; using text UI");
            }
        }
    }
    let mut ui = crate::ui::text::TextUi::stdio();
    run_session(harness, &mut ui)
}

/// Drive the full-screen TUI: owns a Tier-3 current-thread runtime, hands it to
/// the async event loop, and bounds shutdown like the text driver does.
fn run_tui<P: ChatProvider>(harness: &mut Harness<P>, tui: TuiUi) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = crate::ui::tui_loop::run(harness, &runtime, tui);
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Drive the interactive REPL. Owns the Tier-3 runtime: a current-thread tokio
/// runtime that `block_on`s each turn, plus a per-turn cancellation token that a
/// background watcher thread trips when the user presses Ctrl-C. The blocking
/// stdin reads and rendering stay synchronous; only the turn itself runs on the
/// runtime so provider streams and tools are raced against cancellation.
pub(crate) fn run_session<P: ChatProvider>(
    harness: &mut Harness<P>,
    ui: &mut dyn Ui,
) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = run_session_inner(harness, ui, &runtime);
    // Bound shutdown so an orphaned blocking provider request (the loop dropped
    // its stream on cancel) cannot hang process exit, including early error exits.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

fn run_session_inner<P: ChatProvider>(
    harness: &mut Harness<P>,
    ui: &mut dyn Ui,
    runtime: &Runtime,
) -> Result<()> {
    ui.emit(UiEvent::SessionStarted)?;

    while let Some(prompt) = ui.next_prompt()? {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            continue;
        }
        if slash::is_exit(prompt) {
            break;
        }

        // Clear any stale interrupt BEFORE arming the watcher, so a Ctrl-C left
        // over from the idle prompt cannot cancel this fresh turn immediately.
        crate::signals::reset();
        let token = CancellationToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let watcher = std::thread::spawn({
            let token = token.clone();
            let done = Arc::clone(&done);
            move || watch_for_interrupt(&token, &done)
        });

        // One bridge per turn backs both Nexus seams (observer + approval gate)
        // from two shared borrows; it drops here so `ui` is free for the
        // session-driver events below.
        let result = {
            let bridge = UiBridge::new(ui);
            runtime.block_on(harness.submit_turn(prompt, &bridge, &bridge, &token))
        };
        // Stop and join the watcher so it cannot leak past the turn or trip the
        // next one.
        done.store(true, Ordering::Relaxed);
        let _ = watcher.join();

        if let Err(error) = result {
            ui.emit(UiEvent::from_turn_error(&error))?;
        }
    }

    ui.shutdown()?;
    Ok(())
}

/// Bridge the async-signal-safe SIGINT handler (which only flips a global
/// atomic) onto a turn's [`CancellationToken`], running on its own OS thread so
/// it can trip the token even while a synchronous blocking tool occupies the
/// runtime's executor thread.
///
/// ponytail: 20ms poll. A self-pipe/eventfd would remove the poll, but for
/// human-scale Ctrl-C latency the poll is enough; it stops as soon as the turn
/// finishes (`done`).
fn watch_for_interrupt(token: &CancellationToken, done: &AtomicBool) {
    while !done.load(Ordering::Relaxed) {
        if crate::signals::interrupted() {
            token.cancel();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use std::cell::RefCell;

    use crate::nexus::{Agent, AssistantTurn, Message, ProviderEvent, ProviderStream, Tools};
    use crate::ui::text::TextUi;
    use crate::wayland::Harness;

    struct FakeProvider {
        responses: RefCell<Vec<Result<AssistantTurn, String>>>,
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
            }
        }
    }

    impl ChatProvider for FakeProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let item = match self.responses.borrow_mut().pop() {
                Some(Ok(turn)) => Ok(ProviderEvent::Completed(turn)),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            };
            Ok(Box::pin(futures::stream::once(async move { item })))
        }
    }

    #[test]
    fn provider_error_is_rendered_and_session_continues() -> Result<()> {
        let provider = FakeProvider::new(vec![Err("boom"), Ok(AssistantTurn::text("ok"))]);
        let dir = crate::tools::test_support::temp_dir();
        let agent = Agent::new(provider, crate::tools::built_in_tools());
        let mut harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            None,
        );
        let mut ui = TextUi::new("bad\nagain\n/exit\n".as_bytes(), Vec::new(), Vec::new());

        run_session(&mut harness, &mut ui)?;

        let (_, out, err) = ui.into_parts();
        assert!(String::from_utf8(out)?.contains("assistant> ok"));
        assert!(String::from_utf8(err)?.contains("provider error: boom"));
        Ok(())
    }
}
