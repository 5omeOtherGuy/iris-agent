use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const DESCRIPTION: &str = "Ask the user 1-4 structured questions when you need preferences, clarification, or an implementation decision. The user can always choose Other and enter free text. Set multiSelect to true when choices are not mutually exclusive. To recommend an option, put it first and append (Recommended) to its label. Add markdown preview text when comparing concrete alternatives. Do not use this tool to ask for permission to run a tool or perform an action; Iris handles tool approvals separately.";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AskUserQuestionInput {
    pub(crate) questions: Vec<Question>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) answers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) annotations: BTreeMap<String, Annotation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<Metadata>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Question {
    pub(crate) question: String,
    pub(crate) header: String,
    pub(crate) options: Vec<QuestionOption>,
    #[serde(default)]
    pub(crate) multi_select: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuestionOption {
    pub(crate) label: String,
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preview: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Annotation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) notes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
}

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["questions"],
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["question", "header", "options"],
                    "properties": {
                        "question": { "type": "string", "minLength": 1 },
                        "header": { "type": "string", "minLength": 1, "maxLength": 12 },
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["label", "description"],
                                "properties": {
                                    "label": { "type": "string", "minLength": 1 },
                                    "description": { "type": "string", "minLength": 1 },
                                    "preview": { "type": "string" }
                                }
                            }
                        },
                        "multiSelect": { "type": "boolean", "default": false }
                    }
                }
            },
            "answers": {
                "type": "object",
                "additionalProperties": { "type": "string" }
            },
            "annotations": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "preview": { "type": "string" },
                        "notes": { "type": "string" }
                    }
                }
            },
            "metadata": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "source": { "type": "string" } }
            }
        }
    })
}

pub(crate) fn parse_input(value: &Value) -> Result<AskUserQuestionInput> {
    let input: AskUserQuestionInput = serde_json::from_value(value.clone())?;
    if !(1..=4).contains(&input.questions.len()) {
        bail!("questions must contain between 1 and 4 items");
    }

    let mut questions = HashSet::new();
    for question in &input.questions {
        if question.question.trim().is_empty() {
            bail!("question text must not be empty");
        }
        if question.header.trim().is_empty() || question.header.chars().count() > 12 {
            bail!("question header must contain 1 to 12 characters");
        }
        if !(2..=4).contains(&question.options.len()) {
            bail!("each question must contain between 2 and 4 options");
        }
        if !questions.insert(question.question.as_str()) {
            bail!("question texts must be unique");
        }

        let mut labels = HashSet::new();
        for option in &question.options {
            if option.label.trim().is_empty() || option.description.trim().is_empty() {
                bail!("option labels and descriptions must not be empty");
            }
            if option.label == "Other" {
                bail!("Other is added automatically and must not be supplied as an option");
            }
            if !labels.insert(option.label.as_str()) {
                bail!("option labels must be unique within each question");
            }
            if question.multi_select && option.preview.is_some() {
                bail!("preview is not supported for multi-select questions");
            }
        }
    }

    for key in input.answers.keys().chain(input.annotations.keys()) {
        if !questions.contains(key.as_str()) {
            bail!("answers and annotations must reference a supplied question");
        }
    }
    Ok(input)
}

pub(crate) fn format_result(input: &AskUserQuestionInput) -> String {
    let answers = input
        .questions
        .iter()
        .filter_map(|question| {
            input.answers.get(&question.question).map(|answer| {
                let mut rendered = format!("\"{}\"=\"{}\"", question.question, answer);
                if let Some(annotation) = input.annotations.get(&question.question) {
                    if let Some(preview) = &annotation.preview {
                        rendered.push_str(&format!(" (preview: {preview})"));
                    }
                    if let Some(notes) = &annotation.notes {
                        rendered.push_str(&format!(" (notes: {notes})"));
                    }
                }
                rendered
            })
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "User has answered your questions: {answers}. You can now continue with the user's answers in mind."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> Value {
        json!({
            "question": "Which formatter?",
            "header": "Formatter",
            "options": [
                {"label": "Rustfmt", "description": "Use rustfmt", "preview": "```rust\nfn main() {}\n```"},
                {"label": "None", "description": "Do not format"}
            ],
            "multiSelect": false
        })
    }

    #[test]
    fn schema_has_strict_reference_bounds() {
        let schema = parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["questions"]["minItems"], 1);
        assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
        let item = &schema["properties"]["questions"]["items"];
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(item["properties"]["header"]["maxLength"], 12);
        assert_eq!(item["properties"]["options"]["minItems"], 2);
        assert_eq!(item["properties"]["options"]["maxItems"], 4);
        assert_eq!(
            item["properties"]["options"]["items"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn validates_header_and_uniqueness() {
        let mut too_long = question();
        too_long["header"] = json!("thirteen chars");
        assert!(parse_input(&json!({"questions": [too_long]})).is_err());

        let duplicate_question = question();
        assert!(
            parse_input(&json!({"questions": [duplicate_question.clone(), duplicate_question]}))
                .is_err()
        );

        let mut duplicate_option = question();
        duplicate_option["options"][1]["label"] = json!("Rustfmt");
        assert!(parse_input(&json!({"questions": [duplicate_option]})).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_multiselect_previews() {
        let mut unknown = question();
        unknown["surprise"] = json!(true);
        assert!(parse_input(&json!({"questions": [unknown]})).is_err());

        let mut multi_preview = question();
        multi_preview["multiSelect"] = json!(true);
        assert!(parse_input(&json!({"questions": [multi_preview]})).is_err());
    }

    #[test]
    fn formats_answers_and_annotations_for_the_model() {
        let input = parse_input(&json!({
            "questions": [question()],
            "answers": {"Which formatter?": "Rustfmt"},
            "annotations": {"Which formatter?": {"preview": "rustfmt preview", "notes": "standard"}}
        }))
        .unwrap();
        let result = format_result(&input);
        assert!(result.contains("\"Which formatter?\"=\"Rustfmt\""));
        assert!(result.contains("preview: rustfmt preview"));
        assert!(result.contains("notes: standard"));
    }

    #[test]
    fn guidance_covers_reference_usage_rules() {
        assert!(DESCRIPTION.contains("Other"));
        assert!(DESCRIPTION.contains("multiSelect"));
        assert!(DESCRIPTION.contains("(Recommended)"));
        assert!(DESCRIPTION.contains("preview"));
        assert!(DESCRIPTION.contains("Do not use this tool to ask for permission"));
    }
}
