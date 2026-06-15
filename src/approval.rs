//! Approval decision values shared between Nexus enforcement and UI front-ends.

/// Outcome of an approval review for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Allow,
    Deny,
}

/// Parse a terminal decision line. `y`/`yes` (case-insensitive) allow; any other
/// input denies.
pub(crate) fn parse_decision(line: &str) -> ApprovalDecision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        _ => ApprovalDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
