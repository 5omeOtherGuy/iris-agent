//! Local validation of provider structured-output JSON (issue #475,
//! ADR-0061). Pure parse-and-check: no provider code, no network, no
//! session-log mutation. Parent code calls this after a provider/forced-tool
//! response and before rendering durable text ([`super::durable_text`]).

use super::schema::{CompactionSummary, REQUIRED_FIELDS};
use serde_json::Value;

/// Why provider JSON output failed to validate as a canonical
/// `CompactionSummary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SummaryValidationError {
    /// The raw text is not valid JSON at all.
    MalformedJson(String),
    /// The JSON value parsed, but its root is not an object.
    NotAnObject,
    /// A required field is absent.
    MissingField(&'static str),
    /// A field outside the canonical six is present
    /// (`additionalProperties: false`).
    UnknownField(String),
    /// A field is present but has the wrong JSON type.
    WrongType(String),
    /// Every one of `goal`/`state`/`decisions`/`key_facts`/`next_steps` is
    /// empty. `preserved_identifiers` never counts either way (issue #475:
    /// "empty preserved_identifiers does not count toward all-empty").
    AllEmpty,
}

impl std::fmt::Display for SummaryValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedJson(error) => write!(f, "malformed JSON: {error}"),
            Self::NotAnObject => write!(f, "summary is not a JSON object"),
            Self::MissingField(field) => write!(f, "missing required field: {field}"),
            Self::UnknownField(field) => write!(f, "unknown field: {field}"),
            Self::WrongType(message) => write!(f, "wrong type: {message}"),
            Self::AllEmpty => write!(
                f,
                "summary is all-empty (goal/state/decisions/key_facts/next_steps)"
            ),
        }
    }
}

impl std::error::Error for SummaryValidationError {}

/// Parse raw provider JSON text into a validated [`CompactionSummary`].
/// Rejects malformed JSON, a non-object root, missing/unknown fields, wrong
/// types, and an all-empty summary.
pub(crate) fn parse_compaction_summary(
    raw: &str,
) -> Result<CompactionSummary, SummaryValidationError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| SummaryValidationError::MalformedJson(error.to_string()))?;
    parse_compaction_summary_value(&value)
}

/// As [`parse_compaction_summary`], starting from an already-parsed
/// [`Value`] (e.g. the JSON a forced-tool call's arguments decoded into).
pub(crate) fn parse_compaction_summary_value(
    value: &Value,
) -> Result<CompactionSummary, SummaryValidationError> {
    let object = value
        .as_object()
        .ok_or(SummaryValidationError::NotAnObject)?;
    for key in object.keys() {
        if !REQUIRED_FIELDS.contains(&key.as_str()) {
            return Err(SummaryValidationError::UnknownField(key.clone()));
        }
    }
    for key in REQUIRED_FIELDS {
        if !object.contains_key(key) {
            return Err(SummaryValidationError::MissingField(key));
        }
    }
    let goal = object["goal"]
        .as_str()
        .ok_or_else(|| SummaryValidationError::WrongType("goal must be a string".to_string()))?
        .to_string();
    let state = string_array(object, "state")?;
    let decisions = string_array(object, "decisions")?;
    let key_facts = string_array(object, "key_facts")?;
    let next_steps = string_array(object, "next_steps")?;
    let preserved_identifiers = string_array(object, "preserved_identifiers")?;

    let summary = CompactionSummary {
        goal,
        state,
        decisions,
        key_facts,
        next_steps,
        preserved_identifiers,
    };
    if summary.is_all_empty() {
        return Err(SummaryValidationError::AllEmpty);
    }
    Ok(summary)
}

fn string_array(
    object: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<Vec<String>, SummaryValidationError> {
    let array = object[key]
        .as_array()
        .ok_or_else(|| SummaryValidationError::WrongType(format!("{key} must be an array")))?;
    array
        .iter()
        .map(|item| {
            item.as_str().map(str::to_string).ok_or_else(|| {
                SummaryValidationError::WrongType(format!("{key} must contain only strings"))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({
            "goal": "Ship #475 structured summaries",
            "state": ["renderer written"],
            "decisions": ["native first, forced-tool fallback second"],
            "key_facts": ["src/wayland/structured_summary/ holds the new modules"],
            "next_steps": ["wire provider request plumbing"],
            "preserved_identifiers": ["DEPLOY-KEY-AB12CD34"]
        })
    }

    #[test]
    fn accepts_the_canonical_shape() {
        let value = good();
        let summary = parse_compaction_summary_value(&value).expect("valid summary");
        assert_eq!(summary.goal, "Ship #475 structured summaries");
        assert_eq!(summary.preserved_identifiers, vec!["DEPLOY-KEY-AB12CD34"]);
    }

    #[test]
    fn accepts_empty_arrays_including_decisions() {
        let mut value = good();
        value["decisions"] = json!([]);
        value["preserved_identifiers"] = json!([]);
        assert!(parse_compaction_summary_value(&value).is_ok());
    }

    #[test]
    fn rejects_malformed_json() {
        let error = parse_compaction_summary("{not valid json").unwrap_err();
        assert!(matches!(error, SummaryValidationError::MalformedJson(_)));
    }

    #[test]
    fn rejects_non_object_root() {
        let error = parse_compaction_summary_value(&json!(["not", "an", "object"])).unwrap_err();
        assert_eq!(error, SummaryValidationError::NotAnObject);
    }

    #[test]
    fn rejects_each_missing_required_field_individually() {
        for key in REQUIRED_FIELDS {
            let mut value = good();
            value.as_object_mut().unwrap().remove(key);
            let error = parse_compaction_summary_value(&value).unwrap_err();
            assert_eq!(
                error,
                SummaryValidationError::MissingField(key),
                "field: {key}"
            );
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let mut value = good();
        value["unexpected"] = json!(true);
        let error = parse_compaction_summary_value(&value).unwrap_err();
        assert_eq!(
            error,
            SummaryValidationError::UnknownField("unexpected".to_string())
        );
    }

    #[test]
    fn rejects_wrong_type_goal() {
        let mut value = good();
        value["goal"] = json!(123);
        assert!(matches!(
            parse_compaction_summary_value(&value).unwrap_err(),
            SummaryValidationError::WrongType(_)
        ));
    }

    #[test]
    fn rejects_wrong_type_array_field() {
        let mut value = good();
        value["state"] = json!("not-an-array");
        assert!(matches!(
            parse_compaction_summary_value(&value).unwrap_err(),
            SummaryValidationError::WrongType(_)
        ));
    }

    #[test]
    fn rejects_non_string_array_items() {
        let mut value = good();
        value["decisions"] = json!([1, 2]);
        assert!(matches!(
            parse_compaction_summary_value(&value).unwrap_err(),
            SummaryValidationError::WrongType(_)
        ));
    }

    #[test]
    fn rejects_all_empty_summary() {
        let value = json!({
            "goal": "",
            "state": [],
            "decisions": [],
            "key_facts": [],
            "next_steps": [],
            "preserved_identifiers": []
        });
        assert_eq!(
            parse_compaction_summary_value(&value).unwrap_err(),
            SummaryValidationError::AllEmpty
        );
    }

    #[test]
    fn preserved_identifiers_alone_does_not_rescue_an_all_empty_summary() {
        let value = json!({
            "goal": "",
            "state": [],
            "decisions": [],
            "key_facts": [],
            "next_steps": [],
            "preserved_identifiers": ["DEPLOY-KEY-AB12CD34"]
        });
        assert_eq!(
            parse_compaction_summary_value(&value).unwrap_err(),
            SummaryValidationError::AllEmpty,
            "preserved_identifiers must not count toward all-empty either way"
        );
    }

    #[test]
    fn empty_preserved_identifiers_does_not_fail_an_otherwise_populated_summary() {
        let mut value = good();
        value["preserved_identifiers"] = json!([]);
        assert!(parse_compaction_summary_value(&value).is_ok());
    }
}
