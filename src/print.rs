//! Headless `--print` mode (Tier 3). Runs one agent turn-sequence with no
//! interactive terminal: the final assistant answer goes to stdout, approval is
//! resolved non-interactively (deny by default, allow with `--approve`), and
//! piped stdin is merged into the prompt. The model loop, event stream, and
//! approval contract are Nexus-owned; this module only supplies the CLI-side
//! adapters (an observer that captures the final answer and a gate that never
//! prompts) plus the pure argument/stdin merging helpers.

use std::cell::RefCell;
use std::io::{self, IsTerminal, Read};

use anyhow::Result;

use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ReviewContext,
    ToolCall,
};

/// A parsed `-p`/`--print` invocation: the prompt argument plus whether gated
/// tools are auto-approved (`--approve`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PrintInvocation {
    pub(crate) prompt: String,
    pub(crate) approve: bool,
}

/// Parse a print-mode invocation from the raw arguments (already stripped of
/// `argv[0]`). Returns `None` when this is not a `-p`/`--print` run so the
/// caller falls through to the other command dispatch. A print run needs
/// exactly one `-p`/`--print` flag and exactly one prompt argument; both it and
/// `--approve` may appear in any position. Anything else (extra positionals,
/// missing prompt, repeated flags) makes it invalid (returns `None`, which
/// surfaces as a usage error when a print flag was present).
pub(crate) fn parse_print_args(args: &[String]) -> Option<PrintInvocation> {
    let mut print = false;
    let mut approve = false;
    let mut prompt: Option<String> = None;
    for arg in args {
        if arg == "-p" || arg == "--print" {
            if print {
                return None;
            }
            print = true;
        } else if arg == "--approve" {
            if approve {
                return None;
            }
            approve = true;
        } else if prompt.is_none() {
            prompt = Some(arg.clone());
        } else {
            return None;
        }
    }
    if !print {
        return None;
    }
    prompt.map(|prompt| PrintInvocation { prompt, approve })
}

/// Merge piped stdin into the prompt. When there is nothing piped (a TTY, or an
/// empty/whitespace-only pipe) the prompt is used verbatim; otherwise the piped
/// content follows the prompt after a blank-line delimiter so the model sees the
/// instruction first, then the material. Pure so the merge is unit-tested
/// without touching the real stdin.
pub(crate) fn merge_prompt(prompt: &str, piped: Option<&str>) -> String {
    match piped {
        Some(content) if !content.trim().is_empty() => {
            format!("{prompt}\n\n{}", content.trim_end())
        }
        _ => prompt.to_string(),
    }
}

/// Read piped stdin for print mode, or `None` when stdin is a terminal (nothing
/// to merge) or the pipe is empty. Blocking read to EOF; the caller merges the
/// result into the prompt.
pub(crate) fn read_piped_stdin() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buffer = String::new();
    io::stdin().lock().read_to_string(&mut buffer)?;
    if buffer.is_empty() {
        Ok(None)
    } else {
        Ok(Some(buffer))
    }
}

/// Observer that captures the final assistant answer for stdout and drops all
/// intermediate output. In headless mode nothing but the final answer may reach
/// stdout, so tool activity, reasoning, and provider metadata are suppressed;
/// the last non-empty assistant text of the turn-sequence is the answer.
#[derive(Default)]
pub(crate) struct PrintObserver {
    final_text: RefCell<String>,
}

impl PrintObserver {
    /// The captured final assistant text (empty when the turn produced no text).
    pub(crate) fn final_text(self) -> String {
        self.final_text.into_inner()
    }
}

impl AgentObserver for PrintObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AssistantText(text) | AgentEvent::AssistantTextEnd(text)
                if !text.is_empty() =>
            {
                *self.final_text.borrow_mut() = text;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Non-interactive approval gate. Print mode must never prompt (that would hang
/// a pipe/CI run), so every gated call is resolved without input: deny by
/// default, or allow once when `--approve` was passed. Nexus still enforces the
/// decision and re-checks cancellation after review.
pub(crate) struct PrintApprovalGate {
    approve: bool,
}

impl PrintApprovalGate {
    pub(crate) fn new(approve: bool) -> Self {
        Self { approve }
    }

    /// The fixed decision this gate returns for any gated call.
    fn decision(&self) -> ApprovalDecision {
        if self.approve {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        }
    }
}

impl ApprovalGate for PrintApprovalGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        let decision = self.decision();
        Box::pin(async move { Ok(decision) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_print_args_reads_prompt_for_both_flags() {
        for flag in ["-p", "--print"] {
            let parsed = parse_print_args(&args(&[flag, "hello"])).expect("parsed");
            assert_eq!(
                parsed,
                PrintInvocation {
                    prompt: "hello".to_string(),
                    approve: false,
                }
            );
        }
    }

    #[test]
    fn parse_print_args_reads_approve_before_or_after_prompt() {
        let after = parse_print_args(&args(&["-p", "hello", "--approve"])).expect("parsed");
        let before = parse_print_args(&args(&["-p", "--approve", "hello"])).expect("parsed");
        let expected = PrintInvocation {
            prompt: "hello".to_string(),
            approve: true,
        };
        assert_eq!(after, expected);
        assert_eq!(before, expected);
    }

    #[test]
    fn parse_print_args_accepts_flags_in_any_position() {
        let expected = PrintInvocation {
            prompt: "hello".to_string(),
            approve: true,
        };
        for order in [
            ["--approve", "-p", "hello"],
            ["--approve", "hello", "--print"],
            ["hello", "--print", "--approve"],
        ] {
            assert_eq!(parse_print_args(&args(&order)).expect("parsed"), expected);
        }
        // Repeated flags are invalid.
        assert!(parse_print_args(&args(&["-p", "--print", "hello"])).is_none());
        assert!(parse_print_args(&args(&["-p", "--approve", "--approve", "x"])).is_none());
    }

    #[test]
    fn parse_print_args_ignores_non_print_and_invalid_invocations() {
        // Not a print invocation.
        assert!(parse_print_args(&args(&[])).is_none());
        assert!(parse_print_args(&args(&["--plain"])).is_none());
        assert!(parse_print_args(&args(&["resume", "abc"])).is_none());
        // Print flag but no prompt, or too many positional args -> invalid.
        assert!(parse_print_args(&args(&["-p"])).is_none());
        assert!(parse_print_args(&args(&["-p", "--approve"])).is_none());
        assert!(parse_print_args(&args(&["-p", "one", "two"])).is_none());
    }

    #[test]
    fn merge_prompt_uses_prompt_verbatim_without_piped_content() {
        assert_eq!(merge_prompt("explain", None), "explain");
        assert_eq!(merge_prompt("explain", Some("")), "explain");
        assert_eq!(merge_prompt("explain", Some("   \n  ")), "explain");
    }

    #[test]
    fn merge_prompt_appends_piped_content_after_a_blank_line() {
        assert_eq!(
            merge_prompt("explain this failure", Some("line1\nline2\n")),
            "explain this failure\n\nline1\nline2"
        );
    }

    #[test]
    fn print_observer_captures_last_non_empty_assistant_text() {
        let observer = PrintObserver::default();
        observer
            .on_event(AgentEvent::AssistantText("preamble".to_string()))
            .unwrap();
        observer
            .on_event(AgentEvent::AssistantTextEnd("final answer".to_string()))
            .unwrap();
        // A trailing empty end (no text produced) must not clobber the answer.
        observer
            .on_event(AgentEvent::AssistantTextEnd(String::new()))
            .unwrap();
        assert_eq!(observer.final_text(), "final answer");
    }

    #[test]
    fn print_observer_suppresses_non_assistant_events() {
        let observer = PrintObserver::default();
        observer
            .on_event(AgentEvent::AssistantReasoning {
                text: "thinking".to_string(),
                redacted: false,
            })
            .unwrap();
        observer.on_event(AgentEvent::TurnComplete).unwrap();
        assert_eq!(observer.final_text(), "");
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: json!({ "command": "rm -rf /" }),
        }
    }

    #[test]
    fn approval_gate_denies_by_default() {
        let gate = PrintApprovalGate::new(false);
        let decision = futures::executor::block_on(gate.review(
            &tool_call(),
            false,
            false,
            ReviewContext::default(),
        ))
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Deny);
    }

    #[test]
    fn approval_gate_allows_with_approve_flag() {
        let gate = PrintApprovalGate::new(true);
        let decision = futures::executor::block_on(gate.review(
            &tool_call(),
            true,
            true,
            ReviewContext::default(),
        ))
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
    }
}
