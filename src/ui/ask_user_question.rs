#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{AskUserDialog, AskUserDialogOutcome};
    use crate::nexus::InteractionOutcome;
    use crate::ui::modal::ModalKey;

    fn single(preview: bool) -> Value {
        json!({
            "questions": [{
                "question": "Which database?",
                "header": "Database",
                "options": [
                    {
                        "label": "SQLite",
                        "description": "Local and simple",
                        "preview": preview.then_some("```sql\nselect 1;\n```")
                    },
                    {"label": "Postgres", "description": "Shared and scalable"}
                ],
                "multiSelect": false
            }]
        })
    }

    fn resolved(outcome: AskUserDialogOutcome) -> InteractionOutcome {
        match outcome {
            AskUserDialogOutcome::Resolve(outcome) => outcome,
            other => panic!("expected resolution, got {other:?}"),
        }
    }

    #[test]
    fn single_select_auto_submits_with_preview_annotation() {
        let mut dialog = AskUserDialog::new(single(true)).unwrap();
        let InteractionOutcome::Submitted(arguments) = resolved(dialog.handle_key(ModalKey::Enter))
        else {
            panic!("expected submitted answers");
        };
        assert_eq!(arguments["answers"]["Which database?"], "SQLite");
        assert_eq!(
            arguments["annotations"]["Which database?"]["preview"],
            "```sql\nselect 1;\n```"
        );
    }

    #[test]
    fn other_collects_free_text_and_auto_submits() {
        let mut dialog = AskUserDialog::new(single(false)).unwrap();
        dialog.handle_key(ModalKey::Down);
        dialog.handle_key(ModalKey::Down);
        assert_eq!(dialog.handle_key(ModalKey::Enter), AskUserDialogOutcome::Redraw);
        for ch in "DuckDB".chars() {
            dialog.handle_key(ModalKey::Char(ch));
        }
        let InteractionOutcome::Submitted(arguments) = resolved(dialog.handle_key(ModalKey::Enter))
        else {
            panic!("expected submitted answers");
        };
        assert_eq!(arguments["answers"]["Which database?"], "DuckDB");
    }

    #[test]
    fn multi_select_and_multiple_questions_reach_review_before_submit() {
        let input = json!({
            "questions": [
                {
                    "question": "Which checks?",
                    "header": "Checks",
                    "options": [
                        {"label": "Fmt", "description": "Formatting"},
                        {"label": "Clippy", "description": "Linting"}
                    ],
                    "multiSelect": true
                },
                {
                    "question": "Which database?",
                    "header": "Database",
                    "options": [
                        {"label": "SQLite", "description": "Local"},
                        {"label": "Postgres", "description": "Shared"}
                    ],
                    "multiSelect": false
                }
            ]
        });
        let mut dialog = AskUserDialog::new(input).unwrap();
        dialog.handle_key(ModalKey::Enter);
        dialog.handle_key(ModalKey::Down);
        dialog.handle_key(ModalKey::Enter);
        assert_eq!(dialog.handle_key(ModalKey::Tab), AskUserDialogOutcome::Redraw);
        assert_eq!(dialog.handle_key(ModalKey::Enter), AskUserDialogOutcome::Redraw);
        let rendered = dialog.render(100);
        assert!(
            rendered.iter().any(|line| line.to_string().contains("Review your answers")),
            "{rendered:?}"
        );
        let InteractionOutcome::Submitted(arguments) = resolved(dialog.handle_key(ModalKey::Enter))
        else {
            panic!("expected submitted answers");
        };
        assert_eq!(arguments["answers"]["Which checks?"], "Fmt, Clippy");
        assert_eq!(arguments["answers"]["Which database?"], "SQLite");
    }

    #[test]
    fn focused_preview_is_rendered_with_options() {
        let dialog = AskUserDialog::new(single(true)).unwrap();
        let text = dialog
            .render(100)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("SQLite"));
        assert!(text.contains("select 1;"));
        assert!(text.contains("Preview"));
    }

    #[test]
    fn chat_about_this_rejects_with_bounded_feedback_and_current_answers() {
        let mut dialog = AskUserDialog::new(single(false)).unwrap();
        for _ in 0..3 {
            dialog.handle_key(ModalKey::Down);
        }
        let InteractionOutcome::Rejected { feedback: Some(feedback) } =
            resolved(dialog.handle_key(ModalKey::Enter))
        else {
            panic!("expected reject with feedback");
        };
        assert!(feedback.contains("Start by asking them what they would like to clarify"));
        assert!(feedback.contains("Which database?"));
        assert!(feedback.contains("(No answer provided)"));
        assert!(feedback.len() <= 8_192);
    }

    #[test]
    fn escape_rejects_without_feedback() {
        let mut dialog = AskUserDialog::new(single(false)).unwrap();
        assert_eq!(
            resolved(dialog.handle_key(ModalKey::Esc)),
            InteractionOutcome::Rejected { feedback: None }
        );
    }
}
