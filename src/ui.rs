use crate::models;
use crate::sync::SyncState;
use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::sync::{Arc, Mutex};

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
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub focused_field: ComposeField,
    pub cursor_index: usize,
    pub show_cc_bcc: bool,
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
        .split(f.size());

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
    let border_line = "─".repeat(list_width.saturating_sub(2));

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

            // Truncate to fit if necessary (crude)
            let s_label = format!(" From: {}", sender);
            let t_label = format!(" Time: {}", time_str);
            let sub_label = format!(" Subj: {}", subject);

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

            let item_text = format!("{}\n{}\n{}\n{}\n", line1, line2, line3, border_line);
            ListItem::new(item_text).style(style)
        })
        .collect();

    let messages_block = Block::default()
        .borders(Borders::ALL)
        .title("Conversations")
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
        let list_widget = List::new(msg_items).block(messages_block);
        state
            .messages_list_state
            .select(Some(state.selected_message_index));
        f.render_stateful_widget(list_widget, chunks[1], &mut state.messages_list_state);
    }

    // Panel 3: Thread Details
    let details_block = Block::default()
        .borders(Borders::ALL)
        .title("Conversation Context")
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
                msg.body_plain
                    .as_deref()
                    .unwrap_or_else(|| msg.snippet.as_deref().unwrap_or(""))
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
        if let Some(cs) = &state.compose_state {
            let area = centered_rect(80, 80, f.size());
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

            let to_block = Block::default()
                .borders(Borders::ALL)
                .title(" To ")
                .border_style(if cs.focused_field == ComposeField::To {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                });
            let to_paragraph = Paragraph::new(cs.to.as_str()).block(to_block);
            f.render_widget(to_paragraph, chunks[current_chunk]);
            current_chunk += 1;

            if cs.show_cc_bcc {
                let cc_block = Block::default()
                    .borders(Borders::ALL)
                    .title(" Cc ")
                    .border_style(if cs.focused_field == ComposeField::Cc {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    });
                let cc_paragraph = Paragraph::new(cs.cc.as_str()).block(cc_block);
                f.render_widget(cc_paragraph, chunks[current_chunk]);
                current_chunk += 1;

                let bcc_block = Block::default()
                    .borders(Borders::ALL)
                    .title(" Bcc ")
                    .border_style(if cs.focused_field == ComposeField::Bcc {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    });
                let bcc_paragraph = Paragraph::new(cs.bcc.as_str()).block(bcc_block);
                f.render_widget(bcc_paragraph, chunks[current_chunk]);
                current_chunk += 1;
            }

            let sub_block = Block::default()
                .borders(Borders::ALL)
                .title(" Subject ")
                .border_style(if cs.focused_field == ComposeField::Subject {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                });
            let sub_paragraph = Paragraph::new(cs.subject.as_str()).block(sub_block);
            f.render_widget(sub_paragraph, chunks[current_chunk]);
            let sub_chunk_idx = current_chunk;
            current_chunk += 1;

            let body_title = if cs.show_cc_bcc {
                " Body [Esc to Cancel, Ctrl-S to Send, Tab to Switch, Ctrl-B to Hide CC/BCC] "
            } else {
                " Body [Esc to Cancel, Ctrl-S to Send, Tab to Switch, Ctrl-B to Show CC/BCC] "
            };

            let body_block = Block::default()
                .borders(Borders::ALL)
                .title(body_title)
                .border_style(if cs.focused_field == ComposeField::Body {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                });
            let body_paragraph = Paragraph::new(cs.body.as_str())
                .block(body_block)
                .wrap(ratatui::widgets::Wrap { trim: true });
            f.render_widget(body_paragraph, chunks[current_chunk]);
            let body_chunk_idx = current_chunk;

            // Set cursor position based on focused field
            let (cursor_x, cursor_y) = match cs.focused_field {
                ComposeField::To => (chunks[0].x + 1 + cs.cursor_index as u16, chunks[0].y + 1),
                ComposeField::Cc => (chunks[1].x + 1 + cs.cursor_index as u16, chunks[1].y + 1),
                ComposeField::Bcc => (chunks[2].x + 1 + cs.cursor_index as u16, chunks[2].y + 1),
                ComposeField::Subject => (
                    chunks[sub_chunk_idx].x + 1 + cs.cursor_index as u16,
                    chunks[sub_chunk_idx].y + 1,
                ),
                ComposeField::Body => {
                    let mut x = 0;
                    let mut y = 0;
                    for (i, c) in cs.body.chars().enumerate() {
                        if i >= cs.cursor_index {
                            break;
                        }
                        if c == '\n' {
                            x = 0;
                            y += 1;
                        } else {
                            x += 1;
                        }
                    }
                    (
                        chunks[body_chunk_idx].x + 1 + x as u16,
                        chunks[body_chunk_idx].y + 1 + y as u16,
                    )
                }
            };
            f.set_cursor(cursor_x, cursor_y);
        }
    }
}

fn render_authentication(f: &mut Frame, state: &mut UIState) {
    let area = centered_rect(60, 40, f.size());
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
