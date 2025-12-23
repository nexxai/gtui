use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};
use crate::models;

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
}

pub struct UIState {
    pub labels: Vec<models::Label>,
    pub messages: Vec<models::Message>,
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
            Constraint::Percentage(20), // List of mails
            Constraint::Percentage(60), // Selected email details
        ])
        .split(f.size());

    // Panel 1: Labels
    let items: Vec<ListItem> = state.labels.iter()
        .enumerate()
        .map(|(i, l)| {
            let style = if i == state.selected_label_index {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
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
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        });

    let labels_list = List::new(items)
        .block(labels_block)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(labels_list, chunks[0]);

    // Panel 2: Message List
    let msg_items: Vec<ListItem> = state.messages.iter()
        .enumerate()
        .map(|(i, m)| {
            let mut style = if i == state.selected_message_index {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            
            if !m.is_read {
                style = style.add_modifier(Modifier::BOLD);
            }

            let subject = m.subject.as_deref().unwrap_or("(No Subject)");
            ListItem::new(subject).style(style)
        })
        .collect();

    let messages_block = Block::default()
        .borders(Borders::ALL)
        .title("Messages")
        .border_style(if state.focused_panel == FocusedPanel::Messages {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        });

    let list_widget = List::new(msg_items).block(messages_block);
    f.render_widget(list_widget, chunks[1]);

    // Panel 3: Message Detail or Compose
    match state.mode {
        UIMode::Browsing => {
            let detail_text = if let Some(m) = state.messages.get(state.selected_message_index) {
                format!(
                    "From: {}\nTo: {}\nSubject: {}\n\n{}",
                    m.from_address.as_deref().unwrap_or(""),
                    m.to_address.as_deref().unwrap_or(""),
                    m.subject.as_deref().unwrap_or(""),
                    m.snippet.as_deref().unwrap_or("")
                )
            } else {
                "No email selected".to_string()
            };

            let details_block = Block::default()
                .borders(Borders::ALL)
                .title("Email Details")
                .border_style(if state.focused_panel == FocusedPanel::Details {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                });

            let detail_paragraph = Paragraph::new(detail_text)
                .block(details_block)
                .wrap(ratatui::widgets::Wrap { trim: true });
            f.render_widget(detail_paragraph, chunks[2]);
        }
        UIMode::Composing => {
            if let Some(cs) = &state.compose_state {
                let compose_text = format!(
                    "To: {}\nSubject: {}\n\n{}",
                    cs.to, cs.subject, cs.body
                );
                let compose_paragraph = Paragraph::new(compose_text)
                    .block(Block::default().borders(Borders::ALL).title("Compose Message [Esc to Cancel, Ctrl-S to Send]"))
                    .wrap(ratatui::widgets::Wrap { trim: true });
                f.render_widget(compose_paragraph, chunks[2]);
            }
        }
    }
}
