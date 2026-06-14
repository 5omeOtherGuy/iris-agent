use std::io::{self, BufRead, Write};

use anyhow::Result;

pub(crate) trait ChatProvider {
    fn respond(&self, messages: &[Message]) -> Result<String>;
}

pub(crate) struct Agent<P> {
    pub(crate) provider: P,
    pub(crate) messages: Vec<Message>,
}

impl<P: ChatProvider> Agent<P> {
    pub(crate) fn new(provider: P) -> Self {
        Self {
            provider,
            messages: Vec::new(),
        }
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        self.run_with(stdin.lock(), &mut stdout, &mut stderr)
    }

    pub(crate) fn run_with<R: BufRead, W: Write, E: Write>(
        &mut self,
        mut input: R,
        output: &mut W,
        errors: &mut E,
    ) -> Result<()> {
        writeln!(output, "Iris MVP. Type /exit to quit.")?;

        loop {
            write!(output, "iris> ")?;
            output.flush()?;

            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                writeln!(output)?;
                return Ok(());
            }

            let prompt = line.trim();
            if prompt.is_empty() {
                continue;
            }
            if matches!(prompt, "/exit" | "/quit") {
                return Ok(());
            }

            self.messages.push(Message::user(prompt));
            match self.provider.respond(&self.messages) {
                Ok(text) => {
                    writeln!(output, "assistant> {text}")?;
                    self.messages.push(Message::assistant(&text));
                }
                Err(error) => writeln!(errors, "provider error: {error:#}")?,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
}

impl Message {
    pub(crate) fn user(content: &str) -> Self {
        Self {
            role: Role::User,
            content: content.to_string(),
        }
    }

    fn assistant(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: content.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::cell::RefCell;

    struct FakeProvider {
        responses: RefCell<Vec<Result<String, String>>>,
        seen: RefCell<Vec<Vec<Message>>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<Result<&str, &str>>) -> Self {
            Self {
                responses: RefCell::new(
                    responses
                        .into_iter()
                        .map(|result| result.map(str::to_string).map_err(str::to_string))
                        .rev()
                        .collect(),
                ),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl ChatProvider for FakeProvider {
        fn respond(&self, messages: &[Message]) -> Result<String> {
            self.seen.borrow_mut().push(messages.to_vec());
            match self.responses.borrow_mut().pop() {
                Some(Ok(text)) => Ok(text),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            }
        }
    }

    #[test]
    fn repl_keeps_conversation_across_turns() -> Result<()> {
        let provider = FakeProvider::new(vec![Ok("hello"), Ok("goodbye")]);
        let mut agent = Agent::new(provider);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("hi\nbye\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(String::from_utf8(output)?.contains("assistant> hello"));
        assert!(errors.is_empty());
        assert_eq!(agent.provider.seen.borrow().len(), 2);
        assert_eq!(agent.provider.seen.borrow()[1][0].content, "hi");
        assert_eq!(agent.provider.seen.borrow()[1][1].content, "hello");
        assert_eq!(agent.provider.seen.borrow()[1][2].content, "bye");
        Ok(())
    }

    #[test]
    fn repl_reports_provider_errors_and_continues() -> Result<()> {
        let provider = FakeProvider::new(vec![Err("boom"), Ok("recovered")]);
        let mut agent = Agent::new(provider);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("fail\nagain\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(String::from_utf8(errors)?.contains("provider error: boom"));
        assert!(String::from_utf8(output)?.contains("assistant> recovered"));
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages[0].content, "fail");
        assert_eq!(agent.messages[1].content, "again");
        assert_eq!(agent.messages[2].content, "recovered");
        Ok(())
    }
}
