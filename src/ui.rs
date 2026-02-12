use crate::models;
use crate::sync::SyncState;
use crate::undo::UndoableAction;
use chrono::{DateTime, Local};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};
use std::sync::{Arc, Mutex};
use tui_textarea::TextArea;

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

pub struct ComposeState<'a> {
    pub to: TextArea<'a>,
    pub cc: TextArea<'a>,
    pub bcc: TextArea<'a>,
    pub subject: TextArea<'a>,
    pub body: TextArea<'a>,
    pub focused_field: ComposeField,
    pub show_cc_bcc: bool,
}

impl<'a> ComposeState<'a> {
    pub fn new(to: &str, cc: &str, bcc: &str, subject: &str, body: &str) -> Self {
        let mut to_textarea = TextArea::from(to.lines());
        let mut cc_textarea = TextArea::from(cc.lines());
        let mut bcc_textarea = TextArea::from(bcc.lines());
        let mut subject_textarea = TextArea::from(subject.lines());
        let mut body_textarea = TextArea::from(body.lines());

        // Disable cursor line highlighting for cleaner look
        let no_highlight = Style::default();
        to_textarea.set_cursor_line_style(no_highlight);
        cc_textarea.set_cursor_line_style(no_highlight);
        bcc_textarea.set_cursor_line_style(no_highlight);
        subject_textarea.set_cursor_line_style(no_highlight);
        body_textarea.set_cursor_line_style(no_highlight);

        Self {
            to: to_textarea,
            cc: cc_textarea,
            bcc: bcc_textarea,
            subject: subject_textarea,
            body: body_textarea,
            focused_field: ComposeField::To,
            show_cc_bcc: false,
        }
    }

    /// Get the current text content of a field
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

    /// Get mutable reference to the currently focused textarea
    pub fn focused_textarea(&mut self) -> &mut TextArea<'a> {
        match self.focused_field {
            ComposeField::To => &mut self.to,
            ComposeField::Cc => &mut self.cc,
            ComposeField::Bcc => &mut self.bcc,
            ComposeField::Subject => &mut self.subject,
            ComposeField::Body => &mut self.body,
        }
    }
}

pub struct UIState<'a> {
    pub labels: Vec<models::Label>,
    pub messages: Vec<models::Message>,
    pub threaded_messages: Vec<models::Message>,
    pub selected_label_index: usize,
    pub selected_message_index: usize,
    pub messages_list_state: ListState,
    pub detail_scroll: u16,
    pub focused_panel: FocusedPanel,
    pub mode: UIMode,
    pub compose_state: Option<ComposeState<'a>>,
    pub auth_url: Option<String>,
    pub remote_signature: Option<String>,
    pub sync_state: Arc<Mutex<SyncState>>,
    pub undo_stack: Vec<UndoableAction>,
    pub status_message: Option<String>,
}

impl<'a> Default for UIState<'a> {
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
            status_message: None,
        }
    }
}

pub fn render(f: &mut Frame, state: &mut UIState<'_>) {
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

    // Panel 1: Labels
    let items: Vec<ListItem> = state
        .labels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let style = if i == state.selected_label_index {
                Style::default()
                    .fg(Color::Yellow)
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
        .border_style(if state.focused_panel == FocusedPanel::Labels {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        });

    let labels_list = List::new(items)
        .block(labels_block)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(labels_list, chunks[0]);

    // Panel 2: Message List
    let list_width = chunks[1].width.saturating_sub(2) as usize; // Inset from sides

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
                Style::default().fg(Color::Yellow)
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

            let pad = |s: String, len: usize| {
                let char_count = s.chars().count();
                if char_count > len {
                    let truncated: String = s.chars().take(len.saturating_sub(3)).collect();
                    format!("{}...", truncated)
                } else {
                    format!("{:width$}", s, width = len)
                }
            };

            let inner_len = list_width.saturating_sub(2);
            let line1 = format!("{}", pad(s_label, inner_len));
            let line2 = format!("{}", pad(t_label, inner_len));
            let line3 = format!("{}", pad(sub_label, inner_len));

            let is_selected = i == state.selected_message_index;
            let indicator = if is_selected { "█" } else { " " };
            let item_text = format!(
                "{}{}\n{}{}\n{}{}",
                indicator, line1, indicator, line2, indicator, line3
            );
            ListItem::new(item_text).style(style)
        })
        .collect();

    let messages_title = if let Some(ref status) = state.status_message {
        format!("Conversations - {}", status)
    } else {
        "Conversations".to_string()
    };

    let messages_block = Block::default()
        .borders(Borders::ALL)
        .title(messages_title)
        .border_style(if state.focused_panel == FocusedPanel::Messages {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        });

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
            Style::default().fg(Color::Yellow)
        };

        let status_paragraph = Paragraph::new(status_text)
            .block(messages_block)
            .style(status_style)
            .wrap(ratatui::widgets::Wrap { trim: true });
        f.render_widget(status_paragraph, chunks[1]);
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
        f.render_stateful_widget(list_widget, chunks[1], &mut state.messages_list_state);
    }

    // Panel 3: Thread Details
    let details_block = Block::default()
        .borders(Borders::ALL)
        .title("Message Details")
        .border_style(if state.focused_panel == FocusedPanel::Details {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        });

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
                &clean_body(
                    msg.body_plain
                        .as_deref()
                        .unwrap_or_else(|| msg.snippet.as_deref().unwrap_or(""))
                )
            ));
            detail_content
                .push_str("\n------------------------------------------------------------\n\n");
        }
    }

    let detail_paragraph = Paragraph::new(detail_content)
        .block(details_block)
        .wrap(ratatui::widgets::Wrap { trim: true })
        .scroll((state.detail_scroll, 0));
    f.render_widget(detail_paragraph, chunks[2]);

    // Popup for composing
    if let UIMode::Composing = state.mode {
        if let Some(cs) = &mut state.compose_state {
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

            let mut current_chunk = 0;

            // To field
            let to_style = if cs.focused_field == ComposeField::To {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            cs.to.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" To ")
                    .border_style(to_style),
            );
            f.render_widget(&cs.to, chunks[current_chunk]);
            current_chunk += 1;

            // Cc/Bcc fields (optional)
            if cs.show_cc_bcc {
                let cc_style = if cs.focused_field == ComposeField::Cc {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                cs.cc.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Cc ")
                        .border_style(cc_style),
                );
                f.render_widget(&cs.cc, chunks[current_chunk]);
                current_chunk += 1;

                let bcc_style = if cs.focused_field == ComposeField::Bcc {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                cs.bcc.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Bcc ")
                        .border_style(bcc_style),
                );
                f.render_widget(&cs.bcc, chunks[current_chunk]);
                current_chunk += 1;
            }

            // Subject field
            let sub_style = if cs.focused_field == ComposeField::Subject {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            cs.subject.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Subject ")
                    .border_style(sub_style),
            );
            f.render_widget(&cs.subject, chunks[current_chunk]);
            let sub_chunk_idx = current_chunk;
            current_chunk += 1;

            // Body field
            let body_title = if cs.show_cc_bcc {
                " Body [Esc to Cancel, Ctrl-S to Send, Tab to Switch, Ctrl-B to Hide CC/BCC] "
            } else {
                " Body [Esc to Cancel, Ctrl-S to Send, Tab to Switch, Ctrl-B to Show CC/BCC] "
            };
            let body_style = if cs.focused_field == ComposeField::Body {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            cs.body.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(body_title)
                    .border_style(body_style),
            );
            f.render_widget(&cs.body, chunks[current_chunk]);
            let body_chunk_idx = current_chunk;

            // Set cursor position - TextArea handles this internally but we need to tell the frame
            let (cursor_row, cursor_col) = match cs.focused_field {
                ComposeField::To => {
                    let (row, col) = cs.to.cursor();
                    (chunks[0].y + 1 + row as u16, chunks[0].x + 1 + col as u16)
                }
                ComposeField::Cc => {
                    let (row, col) = cs.cc.cursor();
                    (chunks[1].y + 1 + row as u16, chunks[1].x + 1 + col as u16)
                }
                ComposeField::Bcc => {
                    let (row, col) = cs.bcc.cursor();
                    (chunks[2].y + 1 + row as u16, chunks[2].x + 1 + col as u16)
                }
                ComposeField::Subject => {
                    let (row, col) = cs.subject.cursor();
                    (
                        chunks[sub_chunk_idx].y + 1 + row as u16,
                        chunks[sub_chunk_idx].x + 1 + col as u16,
                    )
                }
                ComposeField::Body => {
                    let (row, col) = cs.body.cursor();
                    (
                        chunks[body_chunk_idx].y + 1 + row as u16,
                        chunks[body_chunk_idx].x + 1 + col as u16,
                    )
                }
            };
            f.set_cursor_position((cursor_col, cursor_row));
        }
    }
}

fn render_authentication(f: &mut Frame, state: &mut UIState<'_>) {
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
                    .fg(Color::Yellow)
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

    // Split by newline. This gives us lines, but also empty strings for consecutive newlines.
    // We want to treat whitespace-only lines as empty lines.
    let lines: Vec<&str> = normalized.split('\n').collect();

    let mut consecutive_empty_lines = 0;
    let mut first_content = true;

    for line in lines {
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
}
