use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const MAX_TOOL_ITERATIONS: usize = 8;

pub(crate) trait ChatProvider {
    fn respond(&self, messages: &[Message]) -> Result<AssistantTurn>;
}

pub(crate) struct Agent<P> {
    pub(crate) provider: P,
    pub(crate) messages: Vec<Message>,
    workspace: PathBuf,
}

impl<P: ChatProvider> Agent<P> {
    pub(crate) fn new(provider: P, workspace: PathBuf) -> Self {
        Self {
            provider,
            messages: Vec::new(),
            workspace,
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
            if let Err(error) = self.complete_turn(output) {
                writeln!(errors, "provider error: {error:#}")?;
            }
        }
    }

    fn complete_turn<W: Write>(&mut self, output: &mut W) -> Result<()> {
        for _ in 0..MAX_TOOL_ITERATIONS {
            let turn = self.provider.respond(&self.messages)?;
            if let Some(text) = turn.text.as_deref().filter(|text| !text.is_empty()) {
                writeln!(output, "assistant> {text}")?;
                self.messages.push(Message::assistant(text));
            }

            if turn.tool_calls.is_empty() {
                return Ok(());
            }

            for call in turn.tool_calls {
                writeln!(output, "tool> {}({})", call.name, call.arguments)?;
                self.messages.push(Message::assistant_tool_call(&call));
                let result = self.execute_tool(&call);
                self.messages.push(Message::tool_result(
                    &call.id,
                    &call.name,
                    &tool_result_json(result),
                ));
            }
        }

        bail!("tool loop exceeded {MAX_TOOL_ITERATIONS} iterations")
    }

    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        match call.name.as_str() {
            "read" => {
                let input: ReadInput = serde_json::from_value(call.arguments.clone())
                    .context("read tool arguments must include path")?;
                read_workspace_file(&self.workspace, &input.path)
            }
            name => bail!("unknown tool: {name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssistantTurn {
    pub(crate) text: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
}

impl AssistantTurn {
    #[cfg(test)]
    pub(crate) fn text(text: &str) -> Self {
        Self {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

#[derive(serde::Deserialize)]
struct ReadInput {
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) tool_name: Option<String>,
}

impl Message {
    pub(crate) fn user(content: &str) -> Self {
        Self::new(Role::User, content)
    }

    pub(crate) fn assistant(content: &str) -> Self {
        Self::new(Role::Assistant, content)
    }

    fn assistant_tool_call(call: &ToolCall) -> Self {
        Self {
            role: Role::AssistantToolCall,
            content: call.arguments.to_string(),
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
        }
    }

    fn tool_result(call_id: &str, name: &str, content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some(name.to_string()),
        }
    }

    fn new(role: Role, content: &str) -> Self {
        Self {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_name: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
    AssistantToolCall,
    Tool,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::AssistantToolCall => "assistant_tool_call",
            Self::Tool => "tool",
        }
    }
}

fn read_workspace_file(workspace: &Path, requested_path: &str) -> Result<String> {
    let workspace = workspace
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?;
    let path = workspace.join(requested_path);
    let resolved = path
        .canonicalize()
        .with_context(|| format!("failed to resolve path {requested_path}"))?;

    if !resolved.starts_with(&workspace) {
        bail!("path escapes workspace: {requested_path}");
    }

    fs::read_to_string(&resolved)
        .with_context(|| format!("failed to read text file {}", resolved.display()))
}

fn tool_result_json(result: Result<String>) -> String {
    match result {
        Ok(content) => json!({ "ok": true, "content": content }).to_string(),
        Err(error) => json!({ "ok": false, "error": error.to_string() }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        fn respond(&self, messages: &[Message]) -> Result<AssistantTurn> {
            self.seen.borrow_mut().push(messages.to_vec());
            match self.responses.borrow_mut().pop() {
                Some(Ok(turn)) => Ok(turn),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            }
        }
    }

    #[test]
    fn repl_keeps_conversation_across_turns() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn::text("hello")),
            Ok(AssistantTurn::text("goodbye")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone());
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
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![Err("boom"), Ok(AssistantTurn::text("recovered"))]);
        let mut agent = Agent::new(provider, workspace.path.clone());
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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("read note\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        assert!(String::from_utf8(output)?.contains("assistant> The file says hello from file."));
        let seen = agent.provider.seen.borrow();
        assert_eq!(seen.len(), 2);
        let tool_result = seen[1].last().unwrap();
        assert_eq!(tool_result.role, Role::Tool);
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("call_1"));
        assert!(tool_result.content.contains("hello from file"));
        Ok(())
    }

    #[test]
    fn tool_loop_stops_after_too_many_tool_calls() -> Result<()> {
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
        let provider =
            FakeProvider::new((0..MAX_TOOL_ITERATIONS).map(|_| repeated_call()).collect());
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("read forever\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(String::from_utf8(errors)?.contains("tool loop exceeded"));
        Ok(())
    }

    #[test]
    fn read_tool_rejects_paths_outside_workspace() -> Result<()> {
        let workspace = test_workspace()?;
        let outside = workspace.path.parent().unwrap().join("outside.txt");
        fs::write(&outside, "secret")?;

        let result = read_workspace_file(&workspace.path, "../outside.txt");

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes workspace")
        );
        fs::remove_file(outside)?;
        Ok(())
    }

    #[test]
    fn read_tool_returns_missing_file_error() -> Result<()> {
        let workspace = test_workspace()?;

        let result = tool_result_json(read_workspace_file(&workspace.path, "missing.txt"));

        assert!(result.contains("\"ok\":false"));
        assert!(result.contains("failed to resolve path"));
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

    fn test_workspace() -> Result<TestWorkspace> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("iris-agent-test-{nanos}"));
        fs::create_dir(&path)?;
        Ok(TestWorkspace { path })
    }
}
