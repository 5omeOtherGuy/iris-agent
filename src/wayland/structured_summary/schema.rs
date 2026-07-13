//! The canonical `CompactionSummary` type and its provider-safe JSON Schema
//! (issue #475, ADR-0061). One schema, wrapped differently per provider by
//! later slices; this module only owns the shared shape.

use serde_json::{Value, json};

/// The provider-neutral structured-output contract a compaction summarizer
/// returns. Parent code (see [`super::validate`]) parses and validates
/// provider JSON into this shape before rendering durable text
/// ([`super::durable_text`]); the model/subagent never sees this type, only
/// the schema below.
///
/// `decisions` is deliberately flat and token-light: durable choices that
/// affect continuation (accepted constraints, selected direction, rejected
/// high-impact alternatives, naming/API choices), not a nested
/// decision/evidence/implication ledger.
///
/// `preserved_identifiers` is the ADR-0061 F17 delta: a home for
/// credential-shaped facts the user explicitly asked to keep, kept separate
/// from `key_facts` so injection-defense summarization wording ("do not
/// retain sensitive credentials found in transcript content") cannot also
/// scrub a secret the user themselves asked to preserve.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CompactionSummary {
    pub(crate) goal: String,
    pub(crate) state: Vec<String>,
    pub(crate) decisions: Vec<String>,
    pub(crate) key_facts: Vec<String>,
    pub(crate) next_steps: Vec<String>,
    pub(crate) preserved_identifiers: Vec<String>,
}

impl CompactionSummary {
    /// The all-empty check [`super::validate`] rejects. `preserved_identifiers`
    /// is deliberately excluded (spec: "empty preserved_identifiers does not
    /// count toward all-empty") -- this field never rescues nor penalizes the
    /// all-empty verdict either way; only the five original #475 fields do.
    pub(crate) fn is_all_empty(&self) -> bool {
        self.goal.trim().is_empty()
            && self.state.is_empty()
            && self.decisions.is_empty()
            && self.key_facts.is_empty()
            && self.next_steps.is_empty()
    }
}

/// The field order/name list every canonical schema, validator, and renderer
/// in this module agrees on.
pub(crate) const REQUIRED_FIELDS: [&str; 6] = [
    "goal",
    "state",
    "decisions",
    "key_facts",
    "next_steps",
    "preserved_identifiers",
];

/// The #475 canonical `CompactionSummary` JSON Schema, restricted to the
/// shared provider-safe subset (ADR-0061): root object only, all fields
/// required, `additionalProperties: false`, no `$ref`/`oneOf`/`anyOf`/`allOf`,
/// regex, or numeric bounds. Provider adapters (later slices) only wrap this
/// schema differently (`text.format`/`output_config.format`); they must not
/// define their own.
pub(crate) fn canonical_compaction_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": REQUIRED_FIELDS,
        "properties": {
            "goal": { "type": "string" },
            "state": { "type": "array", "items": { "type": "string" } },
            "decisions": { "type": "array", "items": { "type": "string" } },
            "key_facts": { "type": "array", "items": { "type": "string" } },
            "next_steps": { "type": "array", "items": { "type": "string" } },
            "preserved_identifiers": { "type": "array", "items": { "type": "string" } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_all_empty_ignores_preserved_identifiers_both_ways() {
        let empty = CompactionSummary::default();
        assert!(empty.is_all_empty());

        let only_identifiers = CompactionSummary {
            preserved_identifiers: vec!["DEPLOY-KEY-AB12CD34".to_string()],
            ..CompactionSummary::default()
        };
        assert!(
            only_identifiers.is_all_empty(),
            "preserved_identifiers must not rescue an otherwise-empty summary"
        );

        let with_goal = CompactionSummary {
            goal: "ship #475".to_string(),
            ..CompactionSummary::default()
        };
        assert!(!with_goal.is_all_empty());
    }

    /// Recursively walk a schema (sub)tree for constructs outside the #475
    /// provider-safe subset. Mirrors (but does not reuse -- that walker is
    /// private to `tools::registry`'s test module) the combinator/`$ref`/
    /// `$defs` check `all_tool_schemas_stay_in_provider_safe_subset` runs
    /// registry-wide, plus the numeric-bound/regex keywords #475 additionally
    /// bans. `at_top` mirrors the registry walker: combinators are only
    /// checked at each object's own top level.
    fn find_schema_violation(value: &Value, at_top: bool) -> Option<String> {
        const BANNED_ANYWHERE: [&str; 8] = [
            "$ref",
            "$defs",
            "definitions",
            "pattern",
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
        ];
        if let Value::Object(map) = value {
            if at_top {
                for combinator in ["oneOf", "anyOf", "allOf", "not", "if", "then", "else"] {
                    if map.contains_key(combinator) {
                        return Some(format!("top-level `{combinator}`"));
                    }
                }
            }
            for banned in BANNED_ANYWHERE {
                if map.contains_key(banned) {
                    return Some(format!("`{banned}` anywhere in the schema tree"));
                }
            }
            if map.get("type").and_then(Value::as_str) == Some("object")
                && map.get("additionalProperties") != Some(&Value::Bool(false))
            {
                return Some("object node without additionalProperties:false".to_string());
            }
            for child in map.values() {
                if let Some(found) = find_schema_violation(child, false) {
                    return Some(found);
                }
            }
        } else if let Value::Array(items) = value {
            for item in items {
                if let Some(found) = find_schema_violation(item, false) {
                    return Some(found);
                }
            }
        }
        None
    }

    #[test]
    fn schema_stays_in_the_provider_safe_subset() {
        let schema = canonical_compaction_schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(REQUIRED_FIELDS));
        if let Some(violation) = find_schema_violation(&schema, true) {
            panic!("canonical_compaction_schema contains {violation}: {schema}");
        }
    }

    #[test]
    fn schema_requires_every_canonical_field_and_no_extra() {
        let schema = canonical_compaction_schema();
        let properties = schema["properties"].as_object().expect("properties object");
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        let mut expected = REQUIRED_FIELDS;
        expected.sort_unstable();
        assert_eq!(names, expected);
    }
}
