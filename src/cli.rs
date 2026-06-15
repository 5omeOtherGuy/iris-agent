use anyhow::Result;

use crate::nexus::{Agent, ChatProvider};
use crate::ui::{Ui, UiEvent, is_exit_command};

pub(crate) fn run_session<P: ChatProvider>(agent: &mut Agent<P>, ui: &mut dyn Ui) -> Result<()> {
    if let Some(mut log) = crate::transcript::TranscriptLog::open_if_enabled()? {
        let mut ui = crate::transcript::TranscriptUi::new(ui, &mut log);
        run_session_inner(agent, &mut ui)
    } else {
        run_session_inner(agent, ui)
    }
}

fn run_session_inner<P: ChatProvider>(agent: &mut Agent<P>, ui: &mut dyn Ui) -> Result<()> {
    ui.emit(UiEvent::SessionStarted)?;

    while let Some(prompt) = ui.next_prompt()? {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            continue;
        }
        if is_exit_command(prompt) {
            break;
        }

        if let Err(error) = agent.submit_turn(prompt, ui) {
            ui.emit(UiEvent::from_turn_error(&error))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use std::cell::RefCell;

    use crate::nexus::{AssistantTurn, Message, TurnSink};
    use crate::ui::text::TextUi;

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
        let mut agent = Agent::new(provider, dir.path.clone());
        let mut ui = TextUi::new("bad\nagain\n/exit\n".as_bytes(), Vec::new(), Vec::new());

        run_session(&mut agent, &mut ui)?;

        let (_, out, err) = ui.into_parts();
        assert!(String::from_utf8(out)?.contains("assistant> ok"));
        assert!(String::from_utf8(err)?.contains("provider error: boom"));
        Ok(())
    }
}
