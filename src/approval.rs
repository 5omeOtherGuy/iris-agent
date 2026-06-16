//! Terminal-input parsing for approval decisions (Tier 3). The decision enum
//! itself lives in Nexus (`crate::nexus::ApprovalDecision`), the enforcement
//! point; this module only translates a typed line into that decision.

use crate::nexus::ApprovalDecision;

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
