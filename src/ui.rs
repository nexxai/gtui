use crate::models;
use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

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
}

pub struct ComposeState {
    pub to: String,
    pub subject: String,
    pub body: String,
    pub cursor_index: usize,
}

pub struct UIState {
    pub labels: Vec<models::Label>,
    pub messages: Vec<models::Message>,
    pub threaded_messages: Vec<models::Message>,
    pub selected_label_index: usize,
    pub selected_message_index: usize,
    pub focused_panel: FocusedPanel,
    pub mode: UIMode,
    pub compose_state: Option<ComposeState>,
}

impl Default for UIState {
    fn default() -> Self {
        Self {
            labels: Vec::new(),
            messages: Vec::new(),
            threaded_messages: Vec::new(),
            selected_label_index: 0,
            selected_message_index: 0,
            focused_panel: FocusedPanel::Labels,
            mode: UIMode::Browsing,
            compose_state: None,
        }
    }
}

pub fn render(f: &mut Frame, state: &UIState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20), // Folder structure
            Constraint::Percentage(30), // List of mails
            Constraint::Percentage(50), // Selected email details
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
            ListItem::new(l.name.clone()).style(style)
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
    let list_width = chunks[1].width.saturating_sub(2) as usize;
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
            let time_str = date.format("%b %d %H:%M").to_string();

            let mut style = if i == state.selected_message_index {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };

            if !m.is_read {
                style = style.add_modifier(Modifier::BOLD);
            }

            // Truncate to fit if necessary (crude)
            let s_label = format!(" Sender: {}", sender);
            let t_label = format!(" Time:   {}", time_str);
            let sub_label = format!(" Sub:    {}", subject);

            let pad = |s: String, len: usize| {
                if s.len() > len {
                    format!("{}...", &s[..len.saturating_sub(4)])
                } else {
                    format!("{:width$}", s, width = len)
                }
            };

            let inner_len = list_width.saturating_sub(2);
            let line1 = format!("│{}│", pad(s_label, inner_len));
            let line2 = format!("│{}│", pad(t_label, inner_len));
            let line3 = format!("│{}│", pad(sub_label, inner_len));

            let item_text = format!(
                "┌{}┐\n{}\n{}\n{}\n└{}┘",
                border_line, line1, line2, line3, border_line
            );
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

    let list_widget = List::new(msg_items).block(messages_block);
    f.render_widget(list_widget, chunks[1]);

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
                msg.snippet.as_deref().unwrap_or("")
            ));
            detail_content
                .push_str("\n------------------------------------------------------------\n\n");
        }
    }

    let detail_paragraph = Paragraph::new(detail_content)
        .block(details_block)
        .wrap(ratatui::widgets::Wrap { trim: true });
    f.render_widget(detail_paragraph, chunks[2]);

    // Popup for composing
    if let UIMode::Composing = state.mode {
        if let Some(cs) = &state.compose_state {
            let area = centered_rect(80, 80, f.size());
            f.render_widget(Clear, area); // This clears the background for the popup

            let compose_text = format!("To: {}\nSubject: {}\n\n{}", cs.to, cs.subject, cs.body);
            let compose_paragraph = Paragraph::new(compose_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Compose Message [Esc to Cancel, Ctrl-S to Send] ")
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .wrap(ratatui::widgets::Wrap { trim: true });
            f.render_widget(compose_paragraph, area);

            // Set cursor position. This is a simple estimation.
            // For a better implementation, we'd need to account for wrapping.
            // But for a centered popup, we'll just place it at the start of the body area for now.
            // Subject/To are fixed in this demo's UI text block.
            // "To: ...\nSubject: ...\n\n" is 3 lines.
            let cursor_x = area.x + 1;
            let cursor_y = area.y + 1 + 3; // After To and Subject lines
            f.set_cursor(cursor_x, cursor_y);
        }
    }
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
