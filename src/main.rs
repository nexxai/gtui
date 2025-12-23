mod auth;
mod db;
mod models;
mod ui;
mod gmail;
mod config;

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use google_gmail1::Gmail;
use crate::gmail::GmailClient;
use crate::ui::FocusedPanel;
use crate::config::{Config, matches_key};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load();
    let db_url = "sqlite:gtui.db?mode=rwc";
    let db = db::Database::new(db_url).await?;
    db.run_migrations().await?;

    // Initial Auth
    let secret = auth::Authenticator::load_secret("credentials.json").await?;
    let auth = auth::Authenticator::authenticate(secret).await?;
    
    let hub = Gmail::new(
        hyper::Client::builder().build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .expect("Failed to load native roots")
                .https_only()
                .enable_http1()
                .build(),
        ),
        auth,
    );

    let gmail_client = GmailClient::new(hub);
    let sync_client = gmail_client.clone();
    let sync_db = db::Database::new(db_url).await?; 

    let mut ui_state = ui::UIState::default();
    let mut current_offset = 0;
    let limit = 50;

    // Background Sync Task
    tokio::spawn(async move {
        loop {
            if let Ok(l) = sync_client.list_labels().await {
                let _ = sync_db.upsert_labels(&l).await;
            }

            if let Ok((ids, _)) = sync_client.list_messages(vec!["INBOX".to_string()], 100, None).await {
                let mut messages = Vec::new();
                for id in ids {
                    if let Ok(msg) = sync_client.get_message(&id).await {
                        messages.push(msg);
                    }
                }
                let _ = sync_db.upsert_messages(&messages, "INBOX").await;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    // Initial Sync
    let labels = gmail_client.list_labels().await?;
    db.upsert_labels(&labels).await?;
    let (message_ids, _) = gmail_client.list_messages(vec!["INBOX".to_string()], 20, None).await?;
    let mut messages = Vec::new();
    for id in message_ids {
        if let Ok(msg) = gmail_client.get_message(&id).await {
            messages.push(msg);
        }
    }
    db.upsert_messages(&messages, "INBOX").await?;

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, crossterm::terminal::EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Load initial data for UI
    ui_state.labels = db.get_labels().await?;
    
    // Select INBOX by default
    if let Some(index) = ui_state.labels.iter().position(|l| l.id == "INBOX") {
        ui_state.selected_label_index = index;
    }

    if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
        ui_state.messages = db.get_messages_by_label(&label.id, limit, current_offset).await?;
    }

    loop {
        terminal.draw(|f| ui::render(f, &ui_state))?;

        if let Event::Key(key) = event::read()? {
            match ui_state.mode {
                ui::UIMode::Browsing => {
                    if matches_key(key.code, &config.keybindings.quit) {
                        break;
                    }
                    
                    // Panel switching
                    if matches_key(key.code, &config.keybindings.prev_panel) {
                        ui_state.focused_panel = match ui_state.focused_panel {
                            FocusedPanel::Details => FocusedPanel::Messages,
                            FocusedPanel::Messages => FocusedPanel::Labels,
                            FocusedPanel::Labels => FocusedPanel::Labels,
                        };
                    } else if matches_key(key.code, &config.keybindings.next_panel) {
                        ui_state.focused_panel = match ui_state.focused_panel {
                            FocusedPanel::Labels => FocusedPanel::Messages,
                            FocusedPanel::Messages => FocusedPanel::Details,
                            FocusedPanel::Details => FocusedPanel::Details,
                        };
                    }
                    // Navigation within panels
                    else if matches_key(key.code, &config.keybindings.move_down) {
                        match ui_state.focused_panel {
                            FocusedPanel::Labels => {
                                if ui_state.selected_label_index < ui_state.labels.len().saturating_sub(1) {
                                    ui_state.selected_label_index += 1;
                                    let label = &ui_state.labels[ui_state.selected_label_index];
                                    current_offset = 0;
                                    ui_state.messages = db.get_messages_by_label(&label.id, limit, current_offset).await?;
                                    ui_state.selected_message_index = 0;
                                }
                            }
                            FocusedPanel::Messages => {
                                if ui_state.selected_message_index < ui_state.messages.len().saturating_sub(1) {
                                    ui_state.selected_message_index += 1;
                                    if ui_state.selected_message_index >= ui_state.messages.len().saturating_sub(5) {
                                        current_offset += limit;
                                        if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                                            let mut additional = db.get_messages_by_label(&label.id, limit, current_offset).await?;
                                            ui_state.messages.append(&mut additional);
                                        }
                                    }
                                }
                            }
                            FocusedPanel::Details => {}
                        }
                    }
                    else if matches_key(key.code, &config.keybindings.move_up) {
                        match ui_state.focused_panel {
                            FocusedPanel::Labels => {
                                if ui_state.selected_label_index > 0 {
                                    ui_state.selected_label_index -= 1;
                                    let label = &ui_state.labels[ui_state.selected_label_index];
                                    current_offset = 0;
                                    ui_state.messages = db.get_messages_by_label(&label.id, limit, current_offset).await?;
                                    ui_state.selected_message_index = 0;
                                }
                            }
                            FocusedPanel::Messages => {
                                if ui_state.selected_message_index > 0 {
                                    ui_state.selected_message_index -= 1;
                                }
                            }
                            FocusedPanel::Details => {}
                        }
                    }
                    // Email Actions
                    else if matches_key(key.code, &config.keybindings.mark_read) {
                        // Mark as read
                        if let Some(m) = ui_state.messages.get_mut(ui_state.selected_message_index) {
                            if !m.is_read {
                                m.is_read = true;
                                let id = m.id.clone();
                                let gmail = gmail_client.clone();
                                let db_clone = db::Database::new(db_url).await?;
                                tokio::spawn(async move {
                                    let _ = gmail.mark_as_read(&id).await;
                                    let _ = db_clone.mark_message_as_read(&id, true).await;
                                });
                            }
                        }
                    }
                    else if matches_key(key.code, &config.keybindings.reply) {
                        // Reply
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            let subject = m.subject.as_deref().unwrap_or("");
                            let new_subject = if subject.to_lowercase().starts_with("re:") {
                                subject.to_string()
                            } else {
                                format!("Re: {}", subject)
                            };
                            ui_state.mode = ui::UIMode::Composing;
                            ui_state.compose_state = Some(ui::ComposeState {
                                to: m.from_address.clone().unwrap_or_default(),
                                subject: new_subject,
                                body: "".to_string(),
                            });
                        }
                    }
                    else if matches_key(key.code, &config.keybindings.new_message) {
                        // New message
                        ui_state.mode = ui::UIMode::Composing;
                        ui_state.compose_state = Some(ui::ComposeState {
                            to: "".to_string(),
                            subject: "".to_string(),
                            body: "".to_string(),
                        });
                    }
                    else if matches_key(key.code, &config.keybindings.delete) {
                        // Delete
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            let id = m.id.clone();
                            let gmail = gmail_client.clone();
                            tokio::spawn(async move { let _ = gmail.trash_message(&id).await; });
                            ui_state.messages.remove(ui_state.selected_message_index);
                            if ui_state.selected_message_index >= ui_state.messages.len() && !ui_state.messages.is_empty() {
                                ui_state.selected_message_index = ui_state.messages.len() - 1;
                            }
                        }
                    }
                    else if matches_key(key.code, &config.keybindings.archive) {
                        // Archive
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            let id = m.id.clone();
                            let gmail = gmail_client.clone();
                            tokio::spawn(async move { let _ = gmail.archive_message(&id).await; });
                            ui_state.messages.remove(ui_state.selected_message_index);
                            if ui_state.selected_message_index >= ui_state.messages.len() && !ui_state.messages.is_empty() {
                                ui_state.selected_message_index = ui_state.messages.len() - 1;
                            }
                        }
                    }
                }
                ui::UIMode::Composing => {
                    match key.code {
                        KeyCode::Esc => {
                            ui_state.mode = ui::UIMode::Browsing;
                            ui_state.compose_state = None;
                        }
                        KeyCode::Char('s') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                            if let Some(cs) = &ui_state.compose_state {
                                let gmail = gmail_client.clone();
                                let (to, sub, body) = (cs.to.clone(), cs.subject.clone(), cs.body.clone());
                                tokio::spawn(async move { let _ = gmail.send_message(&to, &sub, &body).await; });
                            }
                            ui_state.mode = ui::UIMode::Browsing;
                            ui_state.compose_state = None;
                        }
                        KeyCode::Char(c) => { if let Some(cs) = &mut ui_state.compose_state { cs.body.push(c); } }
                        KeyCode::Backspace => { if let Some(cs) = &mut ui_state.compose_state { cs.body.pop(); } }
                        KeyCode::Enter => { if let Some(cs) = &mut ui_state.compose_state { cs.body.push('\n'); } }
                        _ => {}
                    }
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
