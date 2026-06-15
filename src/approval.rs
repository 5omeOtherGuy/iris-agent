//! Approval decision values shared between Nexus enforcement and UI front-ends.

/// Outcome of an approval review for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    /// Allow this one call.
    Allow,
    /// Allow this call and auto-approve later calls of the same tool for the
    /// rest of the session. Nexus owns and enforces that session policy.
    AllowAlways,
    /// Refuse this call. Default for empty/invalid/EOF input (safe-by-default).
    Deny,
}

/// Parse a terminal decision line. `y`/`yes` allow once; `a`/`always` allow for
/// the session; any other input (including empty/EOF) denies (safe-by-default).
/// Case-insensitive.
pub(crate) fn parse_decision(line: &str) -> ApprovalDecision {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::Allow,
        "a" | "always" => ApprovalDecision::AllowAlways,
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
    fn parses_always_input_as_allow_always() {
        for line in ["a\n", "A\n", "always\n", "ALWAYS\n", "  always  \n"] {
            assert_eq!(
                parse_decision(line),
                ApprovalDecision::AllowAlways,
                "{line:?}"
            );
        }
    }

    #[test]
    fn parses_negative_and_invalid_input_as_deny() {
        for line in ["n\n", "N\n", "no\n", "\n", "maybe\n", "/exit\n"] {
            assert_eq!(parse_decision(line), ApprovalDecision::Deny, "{line:?}");
        }
    }
}
