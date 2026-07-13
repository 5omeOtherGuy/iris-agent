use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::nexus::InteractionOutcome;
use crate::tools::ask_user_question::{Annotation, AskUserQuestionInput, parse_input};
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
            return self.render_review();
        }
        let question = &self.input.questions[self.current];
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("{}  ", question.header),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!(
                "Question {} of {}",
                self.current + 1,
                self.input.questions.len()
            )),
        ])];
        lines.push(Line::from(question.question.clone()));
        lines.push(Line::from(""));

        for (index, option) in question.options.iter().enumerate() {
            let focused = self.focused[self.current] == index;
            let checked = self.selected[self.current].contains(&index);
            let marker = option_marker(question.multi_select, checked, focused);
            lines.push(Line::from(format!("{marker} {}", option.label)));
            lines.push(Line::styled(
                format!("    {}", option.description),
                Style::default().fg(Color::DarkGray),
            ));
        }

        let other_index = question.options.len();
        let other_marker = option_marker(
            false,
            self.free_text[self.current].is_some(),
            self.focused[self.current] == other_index,
        );
        let other = self.free_text[self.current]
            .as_deref()
            .unwrap_or("free-text answer");
        lines.push(Line::from(format!("{other_marker} Other: {other}")));
        let chat_focused = self.focused[self.current] == other_index + 1;
        lines.push(Line::from(format!(
            "{} Chat about this",
            if chat_focused { ">" } else { " " }
        )));

        if self.editing_other {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Type your answer and press Enter",
                Style::default().fg(Color::Yellow),
            ));
        } else if let Some(preview) = self.focused_preview() {
            lines.push(Line::from(""));
            lines.push(Line::styled("Preview", Style::default().fg(Color::Cyan)));
            lines.extend(preview.lines().map(|line| Line::from(line.to_string())));
        }

        lines.push(Line::from(""));
        let hint = if question.multi_select {
            "↑/↓ move  Space select  Tab next  Esc cancel"
        } else {
            "↑/↓ move  Enter select  Tab next  Esc cancel"
        };
        lines.push(Line::styled(
            truncate_hint(hint, width),
            Style::default().fg(Color::DarkGray),
        ));
        lines
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
                    .or_insert_with(Annotation::default);
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

    fn render_review(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::styled(
            "Review your answers",
            Style::default().fg(Color::Cyan),
        )];
        for (index, question) in self.input.questions.iter().enumerate() {
            let answer = if let Some(free_text) = &self.free_text[index] {
                free_text.clone()
            } else {
                self.selected[index]
                    .iter()
                    .map(|selected| question.options[*selected].label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            lines.push(Line::from(format!("{}: {answer}", question.header)));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "Enter submit  Shift-Tab edit  Esc cancel",
            Style::default().fg(Color::DarkGray),
        ));
        lines
    }
}

fn option_marker(multi_select: bool, checked: bool, focused: bool) -> String {
    let cursor = if focused { ">" } else { " " };
    let mark = if multi_select {
        if checked { "[x]" } else { "[ ]" }
    } else if checked {
        "(*)"
    } else {
        "( )"
    };
    format!("{cursor} {mark}")
}

fn truncate_hint(hint: &str, width: u16) -> String {
    hint.chars()
        .take(width.saturating_sub(2) as usize)
        .collect()
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
        assert!(review.contains("Review your answers"));
        let Some(AskUserDialogOutcome::Submitted(arguments)) = dialog.handle_key(ModalKey::Enter)
        else {
            panic!("review did not submit")
        };
        assert_eq!(arguments["answers"]["Which checks?"], "Fmt, Clippy");
        assert_eq!(arguments["answers"]["Which database?"], "SQLite");
    }

    #[test]
    fn focused_option_renders_markdown_preview() {
        let dialog = AskUserDialog::from_arguments(&json!({
            "questions": [question("Which formatter?", "Formatter", false)]
        }))
        .unwrap();
        let rendered = dialog
            .render(80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Preview"));
        assert!(rendered.contains("fn main() {}"));
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
