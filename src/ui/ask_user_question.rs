use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::nexus::InteractionOutcome;
use crate::tools::ask_user_question::{AskUserQuestionInput, parse_input};
use crate::ui::modal::ModalKey;

const CHAT_FEEDBACK: &str = "The user wants to discuss the questions before answering them.";
const MAX_FEEDBACK_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AskUserDialogOutcome {
    Submitted(Value),
    Rejected { feedback: Option<String> },
}

impl From<AskUserDialogOutcome> for InteractionOutcome {
    fn from(value: AskUserDialogOutcome) -> Self {
        match value {
            AskUserDialogOutcome::Submitted(arguments) => Self::Submitted(arguments),
            AskUserDialogOutcome::Rejected { feedback } => Self::Rejected { feedback },
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AskUserDialog {
    input: AskUserQuestionInput,
    current: usize,
    focused: Vec<usize>,
    selected: Vec<BTreeSet<usize>>,
    free_text: Vec<Option<String>>,
    editing_other: bool,
    review: bool,
}

impl AskUserDialog {
    pub(crate) fn from_arguments(arguments: &Value) -> Result<Self> {
        let input = parse_input(arguments)?;
        let count = input.questions.len();
        Ok(Self {
            input,
            current: 0,
            focused: vec![0; count],
            selected: vec![BTreeSet::new(); count],
            free_text: vec![None; count],
            editing_other: false,
            review: false,
        })
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> Option<AskUserDialogOutcome> {
        if self.editing_other {
            return self.handle_other_key(key);
        }
        if self.review {
            return match key {
                ModalKey::Enter => self.submit(),
                ModalKey::Esc => Some(Self::cancel()),
                ModalKey::BackTab | ModalKey::Left => {
                    self.review = false;
                    self.current = self.input.questions.len() - 1;
                    None
                }
                _ => None,
            };
        }

        let item_count = self.input.questions[self.current].options.len() + 2;
        match key {
            ModalKey::Up => {
                self.focused[self.current] = self.focused[self.current]
                    .checked_sub(1)
                    .unwrap_or(item_count - 1);
                None
            }
            ModalKey::Down => {
                self.focused[self.current] = (self.focused[self.current] + 1) % item_count;
                None
            }
            ModalKey::Left | ModalKey::BackTab => {
                if self.current > 0 {
                    self.current -= 1;
                }
                None
            }
            ModalKey::Right | ModalKey::Tab => {
                self.advance_question();
                None
            }
            ModalKey::Enter | ModalKey::Char(' ') => self.activate_focused(),
            ModalKey::Esc => Some(Self::cancel()),
            _ => None,
        }
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.editing_other {
            let answer = self.free_text[self.current].get_or_insert_with(String::new);
            answer.push_str(text);
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        if self.review {
            return self.render_review(width);
        }
        let question = &self.input.questions[self.current];
        let mut rows = Vec::new();
        let body_width = usize::from(width).max(1);
        for line in crate::ui::tui::wrap_to_width(&question.question, body_width) {
            rows.push((Line::from(line), false));
        }
        rows.push((Line::default(), false));

        for (index, option) in question.options.iter().enumerate() {
            let focused = self.focused[self.current] == index;
            let checked = self.selected[self.current].contains(&index);
            rows.push((
                choice_line(checked, &option.label, Some(&option.description), focused),
                focused,
            ));
        }

        let other_index = question.options.len();
        let other_focused = self.focused[self.current] == other_index;
        let other_answer = self.free_text[self.current].as_deref();
        let other_detail = other_answer
            .filter(|answer| !answer.is_empty())
            .unwrap_or("Write a different answer");
        let mut other = choice_line(
            other_answer.is_some(),
            "Other",
            Some(other_detail),
            other_focused,
        );
        if self.editing_other {
            other.spans.push(Span::styled(
                "▋",
                Style::default().fg(crate::ui::palette::orange()),
            ));
        }
        rows.push((other, other_focused));

        let discuss_focused = self.focused[self.current] == other_index + 1;
        rows.push((
            Line::from(vec![
                Span::styled(
                    "Discuss instead",
                    if discuss_focused {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw("  "),
                Span::styled("Return to chat before answering", crate::ui::modal::dim()),
            ]),
            discuss_focused,
        ));

        if let Some(preview) = self.focused_preview() {
            rows.push((Line::default(), false));
            rows.push((
                Line::from(Span::styled(
                    "PREVIEW",
                    crate::ui::modal::dim().add_modifier(Modifier::BOLD),
                )),
                false,
            ));
            for line in preview.lines() {
                rows.push((Line::from(line.to_string()), false));
            }
        }

        let title = format!(
            "Ask · {} · {}/{}",
            question.header,
            self.current + 1,
            self.input.questions.len()
        );
        let footer = if self.editing_other {
            "type answer · ↵ save · esc discard"
        } else if question.multi_select {
            if self.current + 1 == self.input.questions.len() {
                "↑↓ move · space toggle · tab review · esc cancel"
            } else {
                "↑↓ move · space toggle · tab next · esc cancel"
            }
        } else if self.input.questions.len() == 1 {
            "↑↓ move · ↵ select · esc cancel"
        } else {
            "↑↓ move · ↵ select · tab next · esc cancel"
        };
        crate::ui::tui::overlay_menu(Some(&title), rows, Some(footer), usize::from(width))
    }

    fn handle_other_key(&mut self, key: ModalKey) -> Option<AskUserDialogOutcome> {
        match key {
            ModalKey::Char(ch) => {
                self.free_text[self.current]
                    .get_or_insert_with(String::new)
                    .push(ch);
                None
            }
            ModalKey::Backspace => {
                if let Some(answer) = &mut self.free_text[self.current] {
                    answer.pop();
                }
                None
            }
            ModalKey::Enter => {
                if self.free_text[self.current]
                    .as_deref()
                    .is_some_and(|answer| !answer.trim().is_empty())
                {
                    self.editing_other = false;
                    self.selected[self.current].clear();
                    self.finish_answer()
                } else {
                    None
                }
            }
            ModalKey::Esc => {
                self.editing_other = false;
                self.free_text[self.current] = None;
                None
            }
            _ => None,
        }
    }

    fn activate_focused(&mut self) -> Option<AskUserDialogOutcome> {
        let question = &self.input.questions[self.current];
        let focused = self.focused[self.current];
        if focused < question.options.len() {
            self.free_text[self.current] = None;
            if question.multi_select {
                if !self.selected[self.current].insert(focused) {
                    self.selected[self.current].remove(&focused);
                }
                None
            } else {
                self.selected[self.current].clear();
                self.selected[self.current].insert(focused);
                self.finish_answer()
            }
        } else if focused == question.options.len() {
            self.selected[self.current].clear();
            self.free_text[self.current] = Some(String::new());
            self.editing_other = true;
            None
        } else {
            let feedback = CHAT_FEEDBACK[..CHAT_FEEDBACK.len().min(MAX_FEEDBACK_BYTES)].to_string();
            Some(AskUserDialogOutcome::Rejected {
                feedback: Some(feedback),
            })
        }
    }

    fn finish_answer(&mut self) -> Option<AskUserDialogOutcome> {
        if self.input.questions.len() == 1 {
            self.submit()
        } else if self.current + 1 < self.input.questions.len() {
            self.current += 1;
            None
        } else if self.all_answered() {
            self.review = true;
            None
        } else {
            None
        }
    }

    fn advance_question(&mut self) {
        if self.current + 1 < self.input.questions.len() {
            self.current += 1;
        } else if self.all_answered() {
            self.review = true;
        }
    }

    fn all_answered(&self) -> bool {
        self.input.questions.iter().enumerate().all(|(index, _)| {
            self.free_text[index]
                .as_deref()
                .is_some_and(|answer| !answer.trim().is_empty())
                || !self.selected[index].is_empty()
        })
    }

    fn submit(&self) -> Option<AskUserDialogOutcome> {
        if !self.all_answered() {
            return None;
        }
        let mut input = self.input.clone();
        input.answers = BTreeMap::new();
        for (index, question) in input.questions.iter().enumerate() {
            let answer = if let Some(free_text) = &self.free_text[index] {
                free_text.trim().to_string()
            } else {
                self.selected[index]
                    .iter()
                    .map(|selected| question.options[*selected].label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            input.answers.insert(question.question.clone(), answer);
            if !question.multi_select
                && let Some(selected) = self.selected[index].iter().next()
                && let Some(preview) = &question.options[*selected].preview
            {
                let annotation = input
                    .annotations
                    .entry(question.question.clone())
                    .or_default();
                annotation.preview = Some(preview.clone());
            }
        }
        Some(AskUserDialogOutcome::Submitted(
            serde_json::to_value(input).expect("validated AskUserQuestion input serializes"),
        ))
    }

    fn cancel() -> AskUserDialogOutcome {
        AskUserDialogOutcome::Rejected { feedback: None }
    }

    fn focused_preview(&self) -> Option<&str> {
        self.input.questions[self.current]
            .options
            .get(self.focused[self.current])
            .and_then(|option| option.preview.as_deref())
    }

    fn render_review(&self, width: u16) -> Vec<Line<'static>> {
        let rows = self
            .input
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let answer = if let Some(free_text) = &self.free_text[index] {
                    free_text.clone()
                } else {
                    self.selected[index]
                        .iter()
                        .map(|selected| question.options[*selected].label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                (
                    Line::from(vec![
                        Span::styled(
                            question.header.to_uppercase(),
                            crate::ui::modal::dim().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::raw(answer),
                    ]),
                    false,
                )
            })
            .collect();
        let title = format!("Review answers · {}", self.input.questions.len());
        crate::ui::tui::overlay_menu(
            Some(&title),
            rows,
            Some("↵ submit · shift-tab edit · esc cancel"),
            usize::from(width),
        )
    }
}

fn choice_line(checked: bool, label: &str, detail: Option<&str>, focused: bool) -> Line<'static> {
    let mark = if checked { "◉" } else { "○" };
    let mark_style = if checked {
        Style::default().fg(crate::ui::palette::cyan())
    } else {
        crate::ui::modal::dim()
    };
    let label_style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::styled(mark, mark_style),
        Span::raw(" "),
        Span::styled(label.to_string(), label_style),
    ];
    if let Some(detail) = detail {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(detail.to_string(), crate::ui::modal::dim()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn question(question: &str, header: &str, multi_select: bool) -> Value {
        json!({
            "question": question,
            "header": header,
            "multiSelect": multi_select,
            "options": [
                {"label": "Fmt", "description": "Use rustfmt", "preview": if multi_select { Value::Null } else { json!("```rust\nfn main() {}\n```") }},
                {"label": "Clippy", "description": "Use clippy"}
            ]
        })
    }

    #[test]
    fn single_select_submits_immediately_with_preview_annotation() {
        let mut dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which formatter?", "Formatter", false)]
        }))
        .unwrap();
        let Some(AskUserDialogOutcome::Submitted(arguments)) = dialog.handle_key(ModalKey::Enter)
        else {
            panic!("single select did not submit")
        };
        assert_eq!(arguments["answers"]["Which formatter?"], "Fmt");
        assert_eq!(
            arguments["annotations"]["Which formatter?"]["preview"],
            "```rust\nfn main() {}\n```"
        );
    }

    #[test]
    fn other_accepts_free_text() {
        let mut dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which formatter?", "Formatter", false)]
        }))
        .unwrap();
        dialog.handle_key(ModalKey::Down);
        dialog.handle_key(ModalKey::Down);
        dialog.handle_key(ModalKey::Enter);
        for ch in "Custom formatter".chars() {
            dialog.handle_key(ModalKey::Char(ch));
        }
        let Some(AskUserDialogOutcome::Submitted(arguments)) = dialog.handle_key(ModalKey::Enter)
        else {
            panic!("free text did not submit")
        };
        assert_eq!(arguments["answers"]["Which formatter?"], "Custom formatter");
    }

    #[test]
    fn multiselect_and_multiple_questions_use_review() {
        let mut second = question("Which database?", "Database", false);
        second["options"][0]["label"] = json!("SQLite");
        second["options"][1]["label"] = json!("Postgres");
        let mut dialog = AskUserDialog::from_arguments(&json!({
            "questions": [
                question("Which checks?", "Checks", true),
                second
            ]
        }))
        .unwrap();
        dialog.handle_key(ModalKey::Enter);
        dialog.handle_key(ModalKey::Down);
        dialog.handle_key(ModalKey::Enter);
        dialog.handle_key(ModalKey::Tab);
        assert!(dialog.handle_key(ModalKey::Enter).is_none());
        let review = dialog
            .render(80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(review.contains("REVIEW ANSWERS · 2"));
        let Some(AskUserDialogOutcome::Submitted(arguments)) = dialog.handle_key(ModalKey::Enter)
        else {
            panic!("review did not submit")
        };
        assert_eq!(arguments["answers"]["Which checks?"], "Fmt, Clippy");
        assert_eq!(arguments["answers"]["Which database?"], "SQLite");
    }

    fn rendered_text(dialog: &AskUserDialog, width: u16) -> String {
        dialog
            .render(width)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn question_uses_the_shared_frameless_overlay_grammar() {
        let dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which formatter?", "Formatter", false)]
        }))
        .unwrap();
        let lines = dialog.render(80);
        let rendered = rendered_text(&dialog, 80);

        assert!(rendered.starts_with("ASK · FORMATTER · 1/1\nWhich formatter?"));
        assert!(rendered.contains("○ Fmt  Use rustfmt"));
        assert!(rendered.contains("○ Other  Write a different answer"));
        assert!(rendered.contains("Discuss instead  Return to chat before answering"));
        assert!(rendered.contains("PREVIEW\n```rust\nfn main() {}\n```"));
        assert!(rendered.ends_with("↑↓ move · ↵ select · esc cancel"));
        assert!(
            lines
                .iter()
                .find(|line| line.to_string().contains("○ Fmt"))
                .expect("focused option row")
                .spans
                .iter()
                .all(|span| span.style.bg == Some(crate::ui::palette::surface())),
            "focused row should use the house surface fill"
        );
    }

    #[test]
    fn multiselect_uses_house_choice_marks_and_keymap() {
        let mut dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which checks?", "Checks", true)]
        }))
        .unwrap();
        dialog.handle_key(ModalKey::Enter);
        let rendered = rendered_text(&dialog, 80);

        assert!(rendered.contains("◉ Fmt  Use rustfmt"));
        assert!(rendered.contains("○ Clippy  Use clippy"));
        assert!(rendered.ends_with("↑↓ move · space toggle · tab review · esc cancel"));
    }

    #[test]
    fn chat_about_this_rejects_with_bounded_feedback() {
        let mut dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which formatter?", "Formatter", false)]
        }))
        .unwrap();
        for _ in 0..3 {
            dialog.handle_key(ModalKey::Down);
        }
        let Some(AskUserDialogOutcome::Rejected {
            feedback: Some(feedback),
        }) = dialog.handle_key(ModalKey::Enter)
        else {
            panic!("chat did not reject with feedback")
        };
        assert!(!feedback.is_empty());
        assert!(feedback.len() <= MAX_FEEDBACK_BYTES);
    }

    #[test]
    fn escape_rejects_without_feedback() {
        let mut dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which formatter?", "Formatter", false)]
        }))
        .unwrap();
        assert_eq!(
            dialog.handle_key(ModalKey::Esc),
            Some(AskUserDialogOutcome::Rejected { feedback: None })
        );
    }
}
