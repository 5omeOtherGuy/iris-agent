//! Read-only jj status dropdown: current change, working-copy counts, and a
//! bounded recent log. It intentionally has no mutating actions.

use ratatui::text::{Line, Span};

use crate::git::status::{JjLogEntry, JjStatus};
use crate::ui::symbols;

use super::super::wrap::truncate_line;
use super::super::{dim_style, err_style, prompt_style};
use super::{
    MenuKey, MenuOutcome, cap_block, footer_hints, group_label, internal_rule, readonly_footer,
    step_wrapped,
};

pub(crate) struct JjMenu {
    status: JjStatus,
    selected: usize,
}

impl JjMenu {
    pub(crate) fn new(status: JjStatus) -> Self {
        Self {
            status,
            selected: 0,
        }
    }

    pub(crate) fn set_status(&mut self, status: JjStatus) {
        self.status = status;
        let len = self.status.log.len().max(1);
        if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub(crate) fn render_lines(
        &self,
        width: usize,
        max_rows: usize,
        readonly: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(self.status_line(width));
        lines.push(group_label("RECENT"));
        if self.status.log.is_empty() {
            lines.push(Line::from(Span::styled("no jj log rows", dim_style())));
        } else {
            for (index, entry) in self.status.log.iter().enumerate() {
                lines.push(self.log_row(entry, index == self.selected, width));
            }
        }
        lines.push(internal_rule(width));
        lines.push(if readonly {
            readonly_footer(width)
        } else {
            footer_hints(
                &[("↑↓", "select"), ("esc", "close"), ("", "read-only")],
                width,
            )
        });
        cap_block(lines, max_rows)
    }

    pub(crate) fn handle_key(&mut self, key: MenuKey, _readonly: bool) -> MenuOutcome {
        match key {
            MenuKey::Esc => MenuOutcome::Close,
            MenuKey::Up => self.move_selection(-1),
            MenuKey::Down => self.move_selection(1),
            _ => MenuOutcome::Ignore,
        }
    }

    pub(crate) fn click_line(&mut self, line: usize, _readonly: bool) -> MenuOutcome {
        // line 0 is status, 1 is RECENT; log rows start at 2.
        let Some(index) = line.checked_sub(2) else {
            return MenuOutcome::Ignore;
        };
        if index < self.status.log.len() {
            self.selected = index;
            return MenuOutcome::Redraw;
        }
        MenuOutcome::Ignore
    }

    fn move_selection(&mut self, delta: isize) -> MenuOutcome {
        if self.status.log.is_empty() {
            return MenuOutcome::Ignore;
        }
        self.selected = step_wrapped(self.selected, self.status.log.len(), delta);
        MenuOutcome::Redraw
    }

    fn status_line(&self, width: usize) -> Line<'static> {
        let mut spans = vec![
            Span::styled("jj ".to_string(), dim_style()),
            Span::raw(self.status.change_id.clone()),
        ];
        if !self.status.description.is_empty() {
            spans.push(Span::styled(" \"".to_string(), dim_style()));
            spans.push(Span::raw(self.status.description.clone()));
            spans.push(Span::styled("\"".to_string(), dim_style()));
        }
        if self.status.conflicted > 0 {
            spans.push(Span::styled(
                format!(" {}{}", symbols::REVIEW, self.status.conflicted),
                err_style(),
            ));
        } else if self.status.is_dirty() {
            spans.push(Span::styled(
                format!(" {}{}", symbols::DIRTY, self.status.total_changed),
                prompt_style(),
            ));
        }
        let mut line = Line::from(spans);
        truncate_line(&mut line, width.max(1));
        line
    }

    fn log_row(&self, entry: &JjLogEntry, selected: bool, width: usize) -> Line<'static> {
        let marker = if selected {
            Span::styled(format!("{} ", symbols::ACTIVE), prompt_style())
        } else {
            Span::raw("  ".to_string())
        };
        let desc = if entry.description.is_empty() {
            "(no description)".to_string()
        } else {
            entry.description.clone()
        };
        let mut line = Line::from(vec![
            marker,
            Span::styled(entry.change_id.clone(), dim_style()),
            Span::raw(" ".to_string()),
            Span::raw(desc),
        ]);
        truncate_line(&mut line, width.max(1));
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tui::session_menu::lines_text;

    #[test]
    fn renders_status_and_log_without_actions() {
        let menu = JjMenu::new(JjStatus {
            change_id: "abc12345".to_string(),
            description: "draft change".to_string(),
            total_changed: 2,
            log: vec![JjLogEntry {
                change_id: "abc12345".to_string(),
                description: "draft change".to_string(),
            }],
            ..Default::default()
        });
        let text = lines_text(&menu.render_lines(80, 16, false));
        assert!(text.contains("jj abc12345 \"draft change\" ±2"), "{text}");
        assert!(text.contains("RECENT"), "{text}");
        assert!(text.contains("read-only"), "{text}");
    }
}
