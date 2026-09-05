use crate::db;
use crate::models;
use crate::sync::SyncState;
use crate::toast::Toast;
use crate::undo::UndoableAction;
use anyhow::Result;
use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::sync::{Arc, Mutex};
use tui_textarea::{TextArea, WrapMode};

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum FocusedPanel {
    #[default]
    Labels,
    Messages,
    Details,
}

pub enum UIMode {
    Browsing,
    Composing,
    Authentication,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum ComposeField {
    #[default]
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

pub struct ComposeState {
    pub to: TextArea<'static>,
    pub cc: TextArea<'static>,
    pub bcc: TextArea<'static>,
    pub subject: TextArea<'static>,
    pub body: TextArea<'static>,
    pub focused_field: ComposeField,
    pub show_cc_bcc: bool,
}

/// Create a single-line TextArea with no cursor-line highlight.
fn single_line_textarea(text: &str) -> TextArea<'static> {
    let sanitized = text.replace(['\n', '\r'], " ");
    let lines: Vec<String> = sanitized.lines().map(String::from).collect();
    let mut ta = TextArea::new(lines);
    ta.set_cursor_line_style(Style::default());
    ta
}

impl ComposeState {
    pub fn new(to: &str, cc: &str, bcc: &str, subject: &str, body: &str) -> Self {
        let body_lines: Vec<String> = body.lines().map(String::from).collect();
        let mut body_textarea = TextArea::new(body_lines);
        body_textarea.set_cursor_line_style(Style::default());
        body_textarea.set_wrap_mode(WrapMode::WordOrGlyph);

        Self {
            to: single_line_textarea(to),
            cc: single_line_textarea(cc),
            bcc: single_line_textarea(bcc),
            subject: single_line_textarea(subject),
            body: body_textarea,
            focused_field: ComposeField::To,
            show_cc_bcc: false,
        }
    }

    pub fn get_to(&self) -> String {
        self.to.lines().join("\n")
    }

    pub fn get_cc(&self) -> String {
        self.cc.lines().join("\n")
    }

    pub fn get_bcc(&self) -> String {
        self.bcc.lines().join("\n")
    }

    pub fn get_subject(&self) -> String {
        self.subject.lines().join("\n")
    }

    pub fn get_body(&self) -> String {
        self.body.lines().join("\n")
    }

    pub fn focused_textarea(&mut self) -> &mut TextArea<'static> {
        match self.focused_field {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject => &mut self.subject,
            ComposeField::Body => &mut self.body,
        }
    }
}

pub struct UIState {
    pub labels: Vec<models::Label>,
    pub messages: Vec<models::Message>,
    pub threaded_messages: Vec<models::Message>,
    pub selected_label_index: usize,
    pub selected_message_index: usize,
    pub messages_list_state: ListState,
    pub detail_scroll: u16,
    pub focused_panel: FocusedPanel,
    pub mode: UIMode,
    pub compose_state: Option<ComposeState>,
    pub auth_url: Option<String>,
    pub remote_signature: Option<String>,
    pub sync_state: Arc<Mutex<SyncState>>,
    pub undo_stack: Vec<UndoableAction>,
    pub toast: Option<Toast>,
}

impl Default for UIState {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            messages: Vec::new(),
            threaded_messages: Vec::new(),
            selected_label_index: 0,
            selected_message_index: 0,
            messages_list_state: ListState::default(),
            detail_scroll: 0,
            focused_panel: FocusedPanel::Messages,
            mode: UIMode::Browsing,
            compose_state: None,
            auth_url: None,
            remote_signature: None,
            sync_state: Arc::new(Mutex::new(SyncState::default())),
            undo_stack: Vec::new(),
            toast: None,
        }
    }
}

impl UIState {
    pub async fn refresh_labels_and_messages(
        &mut self,
        db: &db::Database,
        limit: i64,
        current_offset: &mut i64,
    ) -> Result<()> {
        let selected_label_id = self
            .labels
            .get(self.selected_label_index)
            .map(|label| label.id.clone());
        self.labels = db.get_labels().await?;
        self.selected_label_index = selected_label_id
            .as_deref()
            .and_then(|id| self.labels.iter().position(|label| label.id == id))
            .unwrap_or(0);
        if let Some(label) = self.labels.get(self.selected_label_index) {
            let mut new_messages = db
                .get_messages_by_label(&label.id, limit, *current_offset)
                .await?;

            if new_messages.is_empty() && *current_offset > 0 {
                *current_offset = 0;
                new_messages = db.get_messages_by_label(&label.id, limit, 0).await?;
            }

            self.messages = new_messages;

            if !self.messages.is_empty() {
                self.clamp_selected_message();

                if let Some(msg) = self.messages.get(self.selected_message_index) {
                    tracing::debug!(
                        thread_id = ?msg.thread_id,
                        "sync refresh loading thread"
                    );
                    self.threaded_messages = db.get_messages_by_thread(&msg.thread_id).await?;
                    tracing::debug!(
                        count = self.threaded_messages.len(),
                        "sync refresh loaded thread messages"
                    );
                }
            } else {
                self.selected_message_index = 0;
                tracing::debug!("clearing threaded_messages (no messages in label)");
                self.threaded_messages.clear();
            }
        } else {
            self.messages.clear();
            self.threaded_messages.clear();
            self.selected_message_index = 0;
        }

        Ok(())
    }

    pub async fn load_thread_for_selected(&mut self, db: &db::Database) -> Result<()> {
        if let Some(msg) = self.messages.get(self.selected_message_index) {
            self.threaded_messages = db.get_messages_by_thread(&msg.thread_id).await?;
        } else {
            self.threaded_messages.clear();
        }

        Ok(())
    }

    pub fn clamp_selected_message(&mut self) {
        if !self.messages.is_empty() && self.selected_message_index >= self.messages.len() {
            self.selected_message_index = self.messages.len().saturating_sub(1);
        }
    }
}

pub fn render(f: &mut Frame, state: &mut UIState) {
    if let UIMode::Authentication = state.mode {
        render_authentication(f, state);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(10), // Folder structure
            Constraint::Percentage(30), // List of mails
            Constraint::Percentage(60), // Selected email details
        ])
        .split(f.area());

    render_labels_panel(f, state, chunks[0]);
    render_messages_panel(f, state, chunks[1]);
    render_details_panel(f, state, chunks[2]);
    render_compose_popup(f, state);

    // Render toast if present
    if let Some(ref toast) = state.toast {
        f.render_widget(toast, f.area());
    }
}

const DETAIL_SEPARATOR: &str = "------------------------------------------------------------";

fn focused_border_style(is_focused: bool) -> Style {
    if is_focused {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn render_labels_panel(f: &mut Frame, state: &UIState, area: Rect) {
    let items: Vec<ListItem> = state
        .labels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let style = if i == state.selected_label_index {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(l.display_name.clone()).style(style)
        })
        .collect();

    let labels_block = Block::default()
        .borders(Borders::ALL)
        .title("Labels")
        .border_style(focused_border_style(
            state.focused_panel == FocusedPanel::Labels,
        ));

    let labels_list = List::new(items)
        .block(labels_block)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(labels_list, area);
}

fn render_messages_panel(f: &mut Frame, state: &mut UIState, area: Rect) {
    let list_width = area.width.saturating_sub(2) as usize; // Inset from sides

    let msg_items: Vec<ListItem> = state
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let sender = m.from_address.as_deref().unwrap_or("Unknown");
            let subject = m.subject.as_deref().unwrap_or("(No Subject)");

            let date = DateTime::from_timestamp_millis(m.internal_date)
                .unwrap_or_default()
                .with_timezone(&Local);
            let time_str = date.format("%b %d %Y @ %-I:%M%p").to_string();

            let mut style = if i == state.selected_message_index {
                Style::default().fg(Color::Blue)
            } else {
                Style::default()
            };

            if !m.is_read {
                style = style.add_modifier(Modifier::BOLD);
            }

            // Reply indicator emoji if the thread contains a sent message
            let reply_indicator = if m.has_sent_reply { "↩ " } else { "" };

            // Truncate to fit if necessary (crude)
            let s_label = format!(" From: {}", sender);
            let t_label = format!(" Time: {}", time_str);
            let sub_label = format!(" {}Subj: {}", reply_indicator, subject);

            let pad = |text: &str, len: usize| {
                let char_count = text.chars().count();
                if char_count > len {
                    let truncated: String = text.chars().take(len.saturating_sub(3)).collect();
                    format!("{}...", truncated)
                } else {
                    format!("{text:width$}", width = len)
                }
            };

            let inner_len = list_width.saturating_sub(2);
            let line1 = pad(&s_label, inner_len);
            let line2 = pad(&t_label, inner_len);
            let line3 = pad(&sub_label, inner_len);

            let is_selected = i == state.selected_message_index;
            let indicator = if is_selected { "█" } else { " " };
            let item_text = format!(
                "{}{}\n{}{}\n{}{}",
                indicator, line1, indicator, line2, indicator, line3
            );
            ListItem::new(item_text).style(style)
        })
        .collect();

    let messages_title = "Conversations".to_string();

    let messages_block = Block::default()
        .borders(Borders::ALL)
        .title(messages_title)
        .border_style(focused_border_style(
            state.focused_panel == FocusedPanel::Messages,
        ));

    if state.messages.is_empty() {
        // Show sync status or "no conversations" message
        let current_label_id = state
            .labels
            .get(state.selected_label_index)
            .map(|l| l.id.clone());
        let current_label_name = state
            .labels
            .get(state.selected_label_index)
            .map(|l| l.display_name.clone())
            .unwrap_or_default();

        let is_synced = if let Some(ref label_id) = current_label_id {
            if let Ok(sync) = state.sync_state.lock() {
                sync.synced_labels.contains(label_id)
            } else {
                false
            }
        } else {
            false
        };

        let status_text = if is_synced {
            "No conversations".to_string()
        } else {
            format!("⏳ Syncing \"{}\"…\n\n  Please wait.", current_label_name)
        };

        let status_style = if is_synced {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Blue)
        };

        let status_paragraph = Paragraph::new(status_text)
            .block(messages_block)
            .style(status_style)
            .wrap(ratatui::widgets::Wrap { trim: true });
        f.render_widget(status_paragraph, area);
    } else {
        // Insert separator items between conversations
        let separator_width = list_width.saturating_sub(2);
        let separator = "─".repeat(separator_width);
        let mut items_with_separators: Vec<ListItem> = Vec::new();
        for (i, item) in msg_items.into_iter().enumerate() {
            items_with_separators.push(item);
            // Add separator after each item except the last one
            if i < state.messages.len().saturating_sub(1) {
                items_with_separators.push(
                    ListItem::new(separator.clone()).style(Style::default().fg(Color::DarkGray)),
                );
            }
        }

        let list_widget = List::new(items_with_separators).block(messages_block);
        // Adjust index to account for separators (each message is followed by a separator)
        let display_index = state.selected_message_index * 2;
        state.messages_list_state.select(Some(display_index));
        f.render_stateful_widget(list_widget, area, &mut state.messages_list_state);
    }
}

fn render_details_panel(f: &mut Frame, state: &UIState, area: Rect) {
    tracing::debug!(
        threaded_msgs = state.threaded_messages.len(),
        selected_idx = state.selected_message_index,
        panel = ?state.focused_panel,
        "rendering details panel"
    );
    if let Some(first_msg) = state.threaded_messages.first() {
        let preview: String = first_msg
            .body_plain
            .as_ref()
            .map(|s| s.chars().take(50).collect())
            .unwrap_or_else(|| "(no body)".to_string());
        tracing::debug!(
            from = ?first_msg.from_address,
            body_preview = ?preview,
            "first thread message"
        );
    }

    let details_block = Block::default()
        .borders(Borders::ALL)
        .title("Message Details")
        .border_style(focused_border_style(
            state.focused_panel == FocusedPanel::Details,
        ));

    let mut detail_content = String::new();
    if state.threaded_messages.is_empty() {
        detail_content = "No conversation selected".to_string();
    } else {
        for msg in &state.threaded_messages {
            let sender = msg.from_address.as_deref().unwrap_or("Unknown");
            let date = DateTime::from_timestamp_millis(msg.internal_date)
                .unwrap_or_default()
                .with_timezone(&Local);
            let time_str = date.format("%Y-%m-%d %H:%M").to_string();

            detail_content.push_str(&format!(
                "From: {}\nDate: {}\n\n{}\n",
                sender,
                time_str,
                clean_body(
                    msg.body_plain
                        .as_deref()
                        .unwrap_or_else(|| msg.snippet.as_deref().unwrap_or(""))
                )
            ));
            detail_content.push('\n');
            detail_content.push_str(DETAIL_SEPARATOR);
            detail_content.push_str("\n\n");
        }
    }

    // Clear the details area first to prevent rendering artifacts when scrolling fast
    f.render_widget(Clear, area);

    tracing::debug!(
        x = area.x,
        y = area.y,
        w = area.width,
        h = area.height,
        content_len = detail_content.len(),
        "details panel dimensions"
    );

    let vertical_bar_count = detail_content
        .chars()
        .filter(|c| *c == '│' || *c == '|')
        .count();
    if vertical_bar_count > 0 {
        tracing::debug!(
            count = vertical_bar_count,
            "found vertical bar chars in content"
        );
    }

    let detail_paragraph = Paragraph::new(detail_content)
        .block(details_block)
        .wrap(ratatui::widgets::Wrap { trim: true })
        .scroll((state.detail_scroll, 0));
    f.render_widget(detail_paragraph, area);
}

/// Return a focused (cyan + bold) or unfocused (gray) border style for compose fields.
fn compose_border_style(is_focused: bool) -> Style {
    if is_focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

/// Apply a titled border to a TextArea and render it into `area`.
fn render_compose_field(
    f: &mut Frame,
    textarea: &mut TextArea<'static>,
    title: &'static str,
    is_focused: bool,
    area: Rect,
) {
    textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(compose_border_style(is_focused)),
    );
    f.render_widget(&*textarea, area);
}

fn render_compose_popup(f: &mut Frame, state: &mut UIState) {
    // Popup for composing
    if let UIMode::Composing = state.mode
        && let Some(cs) = &mut state.compose_state
    {
        let area = centered_rect(80, 80, f.area());
        f.render_widget(Clear, area);

        let mut constraints = vec![
            Constraint::Length(3), // To
        ];
        if cs.show_cc_bcc {
            constraints.push(Constraint::Length(3)); // Cc
            constraints.push(Constraint::Length(3)); // Bcc
        }
        constraints.push(Constraint::Length(3)); // Subject
        constraints.push(Constraint::Min(10)); // Body

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut chunk = 0;

        render_compose_field(
            f,
            &mut cs.to,
            " To ",
            cs.focused_field == ComposeField::To,
            chunks[chunk],
        );
        chunk += 1;

        if cs.show_cc_bcc {
            render_compose_field(
                f,
                &mut cs.cc,
                " Cc ",
                cs.focused_field == ComposeField::Cc,
                chunks[chunk],
            );
            chunk += 1;
            render_compose_field(
                f,
                &mut cs.bcc,
                " Bcc ",
                cs.focused_field == ComposeField::Bcc,
                chunks[chunk],
            );
            chunk += 1;
        }

        render_compose_field(
            f,
            &mut cs.subject,
            " Subject ",
            cs.focused_field == ComposeField::Subject,
            chunks[chunk],
        );
        chunk += 1;

        let body_title = if cs.show_cc_bcc {
            " Body [Esc to Cancel, Ctrl-S to Send, Tab to Switch, Ctrl-B to Hide CC/BCC] "
        } else {
            " Body [Esc to Cancel, Ctrl-S to Send, Tab to Switch, Ctrl-B to Show CC/BCC] "
        };
        render_compose_field(
            f,
            &mut cs.body,
            body_title,
            cs.focused_field == ComposeField::Body,
            chunks[chunk],
        );
    }
}

fn render_authentication(f: &mut Frame, state: &mut UIState) {
    let area = centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" Authentication Required ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6), // Increased for potential wrapping
            Constraint::Length(4),
            Constraint::Min(0),
        ])
        .split(inner);

    let msg = Paragraph::new("To access your Gmail account, please visit the following URL in your browser and authorize the application:")
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(msg, chunks[0]);

    if let Some(url) = &state.auth_url {
        let url_p = Paragraph::new(url.as_str())
            .style(
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            )
            .block(Block::default().borders(Borders::ALL).title(" URL "))
            .wrap(ratatui::widgets::Wrap { trim: false }); // Wrap the URL!
        f.render_widget(url_p, chunks[1]);
    }

    let footer = Paragraph::new("Your default browser should have opened automatically. If not, please copy the URL above (Tip: Hold Shift to select in most terminals).\n\nThe application will proceed automatically once complete.")
        .style(Style::default().fg(Color::Gray))
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(footer, chunks[2]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn clean_body(body: &str) -> String {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut result = String::with_capacity(normalized.len());

    let mut consecutive_empty_lines = 0;
    let mut first_content = true;

    for line in normalized.split('\n') {
        let trimmed = line.trim_end();

        if trimmed.is_empty() {
            consecutive_empty_lines += 1;
        } else {
            // Found content line
            if !first_content {
                // Determine how many newlines to insert before this content.
                // At least 1 (to separate from previous line), at most 2 (to allow one blank line).

                // If we saw 0 empty lines between content, it means: Content\nContent
                // We want 1 newline.
                // If we saw 1 empty line between content, it means: Content\n\nContent
                // We want 2 newlines.
                // If we saw >1 empty lines/whitespace lines, we want max 2 newlines.

                let newlines_to_add = std::cmp::min(consecutive_empty_lines + 1, 2);
                for _ in 0..newlines_to_add {
                    result.push('\n');
                }
            }

            result.push_str(trimmed);
            consecutive_empty_lines = 0;
            first_content = false;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_body_removes_extra_newlines() {
        let input = "Line 1\n\n\nLine 2\n\n\n\nLine 3";
        let expected = "Line 1\n\nLine 2\n\nLine 3";
        assert_eq!(clean_body(input), expected);
    }

    #[test]
    fn test_clean_body_normalizes_crlf() {
        let input = "Line 1\r\n\r\n\r\nLine 2";
        let expected = "Line 1\n\nLine 2";
        assert_eq!(clean_body(input), expected);
    }

    #[test]
    fn test_clean_body_handles_whitespace_lines() {
        let input = "Line 1\n   \n\t\nLine 2";
        let expected = "Line 1\n\nLine 2";
        assert_eq!(clean_body(input), expected);
    }

    #[test]
    fn test_clean_body_complex_mixed() {
        let input = "Line 1\r\n        \r\n\r\n          \r\nLine 2";
        let expected = "Line 1\n\nLine 2";
        assert_eq!(clean_body(input), expected);
    }

    #[test]
    fn test_clean_body_trims_lines() {
        let input = "Line 1   \nLine 2\t";
        let expected = "Line 1\nLine 2";
        assert_eq!(clean_body(input), expected);
    }

    #[test]
    fn test_clean_body_drops_leading_trailing_blanks() {
        let input = "\n\n Line 1\n\nLine 2\n\n";
        let expected = " Line 1\n\nLine 2";
        assert_eq!(clean_body(input), expected);
    }

    #[test]
    fn test_clean_body_all_whitespace_is_empty() {
        let input = " \n\t\n  ";
        let expected = "";
        assert_eq!(clean_body(input), expected);
    }
}
