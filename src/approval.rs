//! Approval decision seam between Nexus enforcement and the terminal UI.
//!
//! Boundary (AGENTS.md): Nexus enforces the approval policy and only depends on
//! the [`Approver`] trait and [`ApprovalDecision`]. The terminal adapter
//! ([`TerminalApprover`]) is the only piece that prompts on `output` and reads a
//! decision line from the REPL `input`.
//!
//! Critical constraint: `Agent::run` holds `io::stdin().lock()` for the whole
//! session and passes it into the loop as `input`. An approver that opened its
//! own `io::stdin()` would deadlock on the second lock, so the approver borrows
//! the live `input`/`output` streams instead of owning handles.

use std::io::{BufRead, Write};

use anyhow::Result;

use crate::nexus::ToolCall;

/// Outcome of an approval review for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Allow,
    Deny,
}

/// Decision seam consulted by Nexus before executing a mutating tool.
///
/// Generic over the live REPL streams; never used as a trait object. A future
/// object-safe variant could take `&mut dyn BufRead` / `&mut dyn Write`, but the
/// MVP does not need dynamic dispatch.
pub(crate) trait Approver {
    fn review<R: BufRead, W: Write>(
        &mut self,
        call: &ToolCall,
        input: &mut R,
        output: &mut W,
    ) -> Result<ApprovalDecision>;
}

/// Terminal adapter: prompts for `y/n` and reads one line from the REPL input.
///
/// EOF / non-interactive input denies (the safe default, matching pi-mono's
/// `!ctx.hasUI` block). Parsing trims and lowercases the line; `y`/`yes` allow,
/// everything else (including empty input and `/exit`) denies without retrying.
pub(crate) struct TerminalApprover;

impl Approver for TerminalApprover {
    fn review<R: BufRead, W: Write>(
        &mut self,
        call: &ToolCall,
        input: &mut R,
        output: &mut W,
    ) -> Result<ApprovalDecision> {
        write!(output, "approve {}? [y/N] ", call.name)?;
        output.flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            // EOF / piped / non-interactive: deny.
            writeln!(output)?;
            return Ok(ApprovalDecision::Deny);
        }

        Ok(parse_decision(&line))
    }
}

/// Parse a terminal decision line. `y`/`yes` (case-insensitive) allow; any other
/// input denies.
fn parse_decision(line: &str) -> ApprovalDecision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call() -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: "write".to_string(),
            arguments: json!({ "path": "note.txt", "content": "hi" }),
        }
    }

    #[test]
    fn parses_affirmative_input_as_allow() {
        for line in ["y\n", "Y\n", "yes\n", "YES\n", "  yes  \n"] {
            assert_eq!(parse_decision(line), ApprovalDecision::Allow, "{line:?}");
        }
    }

    #[test]
    fn parses_negative_and_invalid_input_as_deny() {
        for line in ["n\n", "N\n", "no\n", "\n", "maybe\n", "/exit\n"] {
            assert_eq!(parse_decision(line), ApprovalDecision::Deny, "{line:?}");
        }
    }

    #[test]
    fn terminal_approver_allows_on_yes() -> Result<()> {
        let mut approver = TerminalApprover;
        let mut input = "y\n".as_bytes();
        let mut output = Vec::new();
        let decision = approver.review(&call(), &mut input, &mut output)?;
        assert_eq!(decision, ApprovalDecision::Allow);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("approve write?")
        );
        Ok(())
    }

    #[test]
    fn terminal_approver_denies_on_no() -> Result<()> {
        let mut approver = TerminalApprover;
        let mut input = "n\n".as_bytes();
        let mut output = Vec::new();
        let decision = approver.review(&call(), &mut input, &mut output)?;
        assert_eq!(decision, ApprovalDecision::Deny);
        Ok(())
    }

    #[test]
    fn terminal_approver_denies_on_eof() -> Result<()> {
        let mut approver = TerminalApprover;
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let decision = approver.review(&call(), &mut input, &mut output)?;
        assert_eq!(decision, ApprovalDecision::Deny);
        Ok(())
    }

    #[test]
    fn terminal_approver_denies_on_invalid_input() -> Result<()> {
        let mut approver = TerminalApprover;
        let mut input = "maybe\n".as_bytes();
        let mut output = Vec::new();
        let decision = approver.review(&call(), &mut input, &mut output)?;
        assert_eq!(decision, ApprovalDecision::Deny);
        Ok(())
    }
}
