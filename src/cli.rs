use anyhow::Result;

use crate::nexus::ChatProvider;
use crate::ui::{Ui, UiBridge, UiEvent, is_exit_command};
use crate::wayland::Harness;

pub(crate) fn run_session<P: ChatProvider>(
    harness: &mut Harness<P>,
    ui: &mut dyn Ui,
) -> Result<()> {
    ui.emit(UiEvent::SessionStarted)?;

    while let Some(prompt) = ui.next_prompt()? {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            continue;
        }
        if is_exit_command(prompt) {
            break;
        }

        // One bridge per turn backs both Nexus seams (observer + approval gate)
        // from two shared borrows; it drops here so `ui` is free for the
        // session-driver events below.
        let result = {
            let bridge = UiBridge::new(ui);
            harness.submit_turn(prompt, &bridge, &bridge)
        };
        if let Err(error) = result {
            ui.emit(UiEvent::from_turn_error(&error))?;
        }
    }

    ui.shutdown()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use std::cell::RefCell;

    use crate::nexus::{Agent, AssistantTurn, Message, Tools, TurnSink};
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
        fn respond(
            &self,
            _messages: &[Message],
            _tools: &Tools,
            _sink: &mut dyn TurnSink,
        ) -> Result<AssistantTurn> {
            match self.responses.borrow_mut().pop() {
                Some(Ok(turn)) => Ok(turn),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            }
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
