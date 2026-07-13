#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{DESCRIPTION, format_result, parameters, parse_input};

    fn valid_input() -> Value {
        json!({
            "questions": [{
                "question": "Which storage should we use?",
                "header": "Storage",
                "options": [
                    {"label": "SQLite", "description": "Local and simple", "preview": "```sql\nselect 1;\n```"},
                    {"label": "Postgres", "description": "Shared and scalable"}
                ],
                "multiSelect": false
            }]
        })
    }

    #[test]
    fn schema_is_strict_and_matches_reference_bounds() {
        let schema = parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["questions"]["minItems"], 1);
        assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
        assert_eq!(
            schema["properties"]["questions"]["items"]["properties"]["header"]["maxLength"],
            12
        );
        assert_eq!(
            schema["properties"]["questions"]["items"]["properties"]["options"]["minItems"],
            2
        );
        assert_eq!(
            schema["properties"]["questions"]["items"]["properties"]["options"]["maxItems"],
            4
        );
    }

    #[test]
    fn validation_rejects_duplicate_questions_and_option_labels() {
        let mut duplicate_question = valid_input();
        let question = duplicate_question["questions"][0].clone();
        duplicate_question["questions"] = json!([question.clone(), question]);
        assert!(parse_input(&duplicate_question).is_err());

        let mut duplicate_option = valid_input();
        duplicate_option["questions"][0]["options"][1]["label"] = json!("SQLite");
        assert!(parse_input(&duplicate_option).is_err());
    }

    #[test]
    fn validation_rejects_bounds_unknown_fields_and_preview_on_multi_select() {
        let mut long_header = valid_input();
        long_header["questions"][0]["header"] = json!("thirteen chars");
        assert!(parse_input(&long_header).is_err());

        let mut unknown = valid_input();
        unknown["unexpected"] = json!(true);
        assert!(parse_input(&unknown).is_err());

        let mut multi_preview = valid_input();
        multi_preview["questions"][0]["multiSelect"] = json!(true);
        assert!(parse_input(&multi_preview).is_err());
    }

    #[test]
    fn result_relay_preserves_question_order_answers_and_annotations() {
        let mut value = valid_input();
        value["answers"] = json!({"Which storage should we use?": "SQLite"});
        value["annotations"] = json!({
            "Which storage should we use?": {
                "preview": "```sql\nselect 1;\n```",
                "notes": "Keep migrations small"
            }
        });
        let input = parse_input(&value).unwrap();
        assert_eq!(
            format_result(&input),
            "User has answered your questions: \"Which storage should we use?\"=\"SQLite\" selected preview:\n```sql\nselect 1;\n``` user notes: Keep migrations small. You can now continue with the user's answers in mind."
        );
    }

    #[test]
    fn model_guidance_covers_practical_use_and_non_permission_boundary() {
        for phrase in [
            "Other",
            "multiSelect",
            "(Recommended)",
            "preview",
            "Do not use this tool to ask for permission",
        ] {
            assert!(DESCRIPTION.contains(phrase), "missing guidance: {phrase}");
        }
    }
}
