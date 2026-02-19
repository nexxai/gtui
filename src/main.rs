mod auth;
mod config;
mod db;
mod gmail;
mod logging;
mod models;
mod sync;
mod text;
mod toast;
mod ui;
mod undo;

use crate::config::{Config, matches_key};
use crate::gmail::GmailClient;
use crate::toast::{Toast, ToastPosition};
use crate::ui::FocusedPanel;
use crate::undo::UndoableAction;
use chrono::{DateTime, Local};
use google_gmail1::Gmail;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io;
use std::sync::{Arc, Mutex};

/// Shared application state passed to all key handlers.
struct App {
    config: Config,
    db: db::Database,
    gmail_client: Option<GmailClient>,
    sync_state: Arc<Mutex<sync::SyncState>>,
    priority_tx: tokio::sync::mpsc::Sender<String>,
    refresh_tx: tokio::sync::mpsc::Sender<()>,
    current_offset: i64,
    limit: i64,
}

impl App {
    /// Get the currently-selected label ID, defaulting to INBOX.
    fn current_label_id(&self, ui_state: &ui::UIState<'_>) -> String {
        ui_state
            .labels
            .get(ui_state.selected_label_index)
            .map(|l| l.id.clone())
            .unwrap_or_else(|| "INBOX".to_string())
    }

    /// Select a new label, reload messages, and notify the sync task.
    async fn select_label(
        &mut self,
        ui_state: &mut ui::UIState<'_>,
        label_index: usize,
    ) -> anyhow::Result<()> {
        ui_state.selected_label_index = label_index;
        let label_id = ui_state.labels[label_index].id.clone();
        self.current_offset = 0;
        ui_state.messages = self
            .db
            .get_messages_by_label(&label_id, self.limit, self.current_offset)
            .await?;
        ui_state.selected_message_index = 0;
        ui_state.detail_scroll = 0;
        ui_state.load_thread_for_selected(&self.db).await?;
        let _ = self.priority_tx.try_send(label_id);
        Ok(())
    }

    /// Remove the currently-selected message from the UI list and clamp the index.
    fn remove_selected_message(&self, ui_state: &mut ui::UIState<'_>) {
        ui_state.messages.remove(ui_state.selected_message_index);
        if ui_state.selected_message_index >= ui_state.messages.len()
            && !ui_state.messages.is_empty()
        {
            ui_state.selected_message_index = ui_state.messages.len() - 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Key handlers
// ---------------------------------------------------------------------------

async fn handle_navigation_keys(
    key: &KeyEvent,
    app: &mut App,
    ui_state: &mut ui::UIState<'_>,
) -> anyhow::Result<bool> {
    // Panel switching
    if matches_key(*key, &app.config.keybindings.prev_panel) {
        ui_state.focused_panel = match ui_state.focused_panel {
            FocusedPanel::Details => FocusedPanel::Messages,
            FocusedPanel::Messages => FocusedPanel::Labels,
            FocusedPanel::Labels => FocusedPanel::Labels,
        };
        return Ok(true);
    }
    if matches_key(*key, &app.config.keybindings.next_panel) {
        ui_state.focused_panel = match ui_state.focused_panel {
            FocusedPanel::Labels => FocusedPanel::Messages,
            FocusedPanel::Messages => FocusedPanel::Details,
            FocusedPanel::Details => FocusedPanel::Details,
        };
        return Ok(true);
    }

    // Navigation within panels
    if matches_key(*key, &app.config.keybindings.move_down) {
        match ui_state.focused_panel {
            FocusedPanel::Labels => {
                let next = ui_state.selected_label_index + 1;
                if next < ui_state.labels.len() {
                    app.select_label(ui_state, next).await?;
                }
            }
            FocusedPanel::Messages => {
                if ui_state.selected_message_index < ui_state.messages.len().saturating_sub(1) {
                    let old_idx = ui_state.selected_message_index;
                    ui_state.selected_message_index += 1;
                    ui_state.detail_scroll = 0;
                    if let Some(msg) = ui_state.messages.get(ui_state.selected_message_index) {
                        tracing::debug!(
                            old_idx,
                            new_idx = ui_state.selected_message_index,
                            thread_id = ?msg.thread_id,
                            "navigating message list"
                        );
                        ui_state.load_thread_for_selected(&app.db).await?;
                        tracing::debug!(
                            count = ui_state.threaded_messages.len(),
                            "loaded thread messages"
                        );
                    }

                    // Lazy-load more messages near the end
                    if ui_state.selected_message_index
                        >= ui_state.messages.len().saturating_sub(5)
                    {
                        app.current_offset += app.limit;
                        if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                            let mut additional = app
                                .db
                                .get_messages_by_label(
                                    &label.id,
                                    app.limit,
                                    app.current_offset,
                                )
                                .await?;
                            ui_state.messages.append(&mut additional);
                        }
                    }
                }
            }
            FocusedPanel::Details => {
                ui_state.detail_scroll = ui_state.detail_scroll.saturating_add(1);
            }
        }
        return Ok(true);
    }

    if matches_key(*key, &app.config.keybindings.move_up) {
        match ui_state.focused_panel {
            FocusedPanel::Labels => {
                if ui_state.selected_label_index > 0 {
                    let prev = ui_state.selected_label_index - 1;
                    app.select_label(ui_state, prev).await?;
                }
            }
            FocusedPanel::Messages => {
                if ui_state.selected_message_index > 0 {
                    ui_state.selected_message_index -= 1;
                    ui_state.detail_scroll = 0;
                    ui_state.load_thread_for_selected(&app.db).await?;
                }
            }
            FocusedPanel::Details => {
                ui_state.detail_scroll = ui_state.detail_scroll.saturating_sub(1);
            }
        }
        return Ok(true);
    }

    Ok(false)
}

fn handle_message_actions(
    key: &KeyEvent,
    app: &App,
    ui_state: &mut ui::UIState<'_>,
) -> bool {
    if matches_key(*key, &app.config.keybindings.mark_read) {
        if let Some(m) = ui_state.messages.get_mut(ui_state.selected_message_index) {
            let new_status = !m.is_read;
            m.is_read = new_status;
            let id = m.id.clone();
            if let Some(gmail) = &app.gmail_client {
                let gmail = gmail.clone();
                let db_clone = app.db.clone();
                tokio::spawn(async move {
                    if new_status {
                        let _ = gmail.mark_as_read(&id).await;
                    } else {
                        let _ = gmail.mark_as_unread(&id).await;
                    }
                    let _ = db_clone.mark_message_as_read(&id, new_status).await;
                });
            }
        }
        return true;
    }
    if matches_key(*key, &app.config.keybindings.reply) {
        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
            let subject = m.subject.as_deref().unwrap_or("");
            let new_subject = if subject.to_lowercase().starts_with("re:") {
                subject.to_string()
            } else {
                format!("Re: {}", subject)
            };

            let mut quoted_body = String::new();
            let date = DateTime::from_timestamp_millis(m.internal_date)
                .unwrap_or_default()
                .with_timezone(&Local);

            quoted_body.push_str(&format!(
                "\nOn {}, {} wrote:\n",
                date.format("%a, %b %d, %Y at %l:%M %p"),
                m.from_address.as_deref().unwrap_or("Unknown")
            ));

            let body_to_quote = m.body_plain.as_ref().or(m.snippet.as_ref());
            if let Some(body) = body_to_quote {
                for line in body.lines() {
                    quoted_body.push_str(&format!("> {}\n", line));
                }
            }

            let mut signature_part = String::new();
            let sig_to_use = ui_state
                .remote_signature
                .as_ref()
                .or(app.config.signatures.reply.as_ref());
            if let Some(sig) = sig_to_use {
                signature_part.push_str("--\n");
                signature_part.push_str(sig);
                signature_part.push_str("\n\n");
            }

            let final_body = format!("\n\n{}{}", signature_part, quoted_body);

            ui_state.mode = ui::UIMode::Composing;
            let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Show);
            let mut compose = ui::ComposeState::new(
                &m.from_address.clone().unwrap_or_default(),
                "",
                "",
                &new_subject,
                &final_body,
            );
            compose.focused_field = ui::ComposeField::Body;
            ui_state.compose_state = Some(compose);
        }
        return true;
    }
    if matches_key(*key, &app.config.keybindings.forward) {
        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
            let subject = m.subject.as_deref().unwrap_or("");
            let new_subject = if subject.to_lowercase().starts_with("fwd:")
                || subject.to_lowercase().starts_with("fw:")
            {
                subject.to_string()
            } else {
                format!("Fwd: {}", subject)
            };

            let mut forward_body = String::new();
            forward_body.push_str("\n\n");

            let sig_to_use = ui_state
                .remote_signature
                .as_ref()
                .or(app.config.signatures.new_message.as_ref());
            if let Some(sig) = sig_to_use {
                forward_body.push_str("--\n");
                forward_body.push_str(sig);
                forward_body.push('\n');
            }

            forward_body.push_str("\n---------- Forwarded message ----------\n");
            forward_body.push_str(&format!(
                "From: {}\n",
                m.from_address.as_deref().unwrap_or("Unknown")
            ));

            let date = DateTime::from_timestamp_millis(m.internal_date)
                .unwrap_or_default()
                .with_timezone(&Local);
            forward_body.push_str(&format!(
                "Date: {}\n",
                date.format("%a, %b %d, %Y at %l:%M %p")
            ));

            forward_body.push_str(&format!("Subject: {}\n", subject));
            forward_body.push_str(&format!(
                "To: {}\n",
                m.to_address.as_deref().unwrap_or("Unknown")
            ));

            let body_to_forward = m.body_plain.as_ref().or(m.snippet.as_ref());
            if let Some(body) = body_to_forward {
                forward_body.push_str(&format!("\n{}", body));
            }

            ui_state.mode = ui::UIMode::Composing;
            let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Show);
            let compose = ui::ComposeState::new("", "", "", &new_subject, &forward_body);
            ui_state.compose_state = Some(compose);
        }
        return true;
    }
    if matches_key(*key, &app.config.keybindings.new_message) {
        ui_state.mode = ui::UIMode::Composing;
        let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Show);

        let mut body = String::new();
        let sig_to_use = ui_state
            .remote_signature
            .as_ref()
            .or(app.config.signatures.new_message.as_ref());
        if let Some(sig) = sig_to_use {
            body.push_str("\n\n--\n");
            body.push_str(sig);
        }

        ui_state.compose_state = Some(ui::ComposeState::new("", "", "", "", &body));
        return true;
    }

    false
}

async fn handle_delete_action(
    key: &KeyEvent,
    app: &App,
    ui_state: &mut ui::UIState<'_>,
) -> anyhow::Result<bool> {
    if !matches_key(*key, &app.config.keybindings.delete) {
        return Ok(false);
    }

    if ui_state.focused_panel == FocusedPanel::Labels {
        return Ok(true);
    }
    ui_state.focused_panel = FocusedPanel::Messages;

    if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
        let thread_id = m.thread_id.clone();
        let thread_messages = app.db.get_messages_by_thread(&thread_id).await?;
        let message_ids: Vec<String> = thread_messages.iter().map(|m| m.id.clone()).collect();

        if let Ok(mut state) = app.sync_state.lock() {
            state.mark_modified_many(message_ids.clone());
        }

        let current_label_id = app.current_label_id(ui_state);
        let original_index = ui_state.selected_message_index;
        ui_state.undo_stack.push(UndoableAction::Delete {
            messages: thread_messages.clone(),
            label_id: current_label_id.clone(),
            original_index,
        });

        for id in &message_ids {
            if let Err(e) = app.db.delete_message(id).await {
                ui_state.toast = Some(Toast::new(
                    format!("Failed to delete message from DB: {}", e),
                    ToastPosition::BottomRight,
                ));
            }
        }

        let mut api_succeeded = true;
        if let Some(gmail) = &app.gmail_client {
            tracing::debug!(count = message_ids.len(), "about to trash messages");
            match gmail.trash_messages(&message_ids).await {
                Ok(_) => {
                    ui_state.toast = Some(Toast::new(
                        "Deleted successfully",
                        ToastPosition::BottomRight,
                    ));
                }
                Err(e) => {
                    tracing::debug!(?e, "trash_messages failed");
                    ui_state.toast = Some(Toast::new(
                        format!("Delete failed: {}", e),
                        ToastPosition::BottomRight,
                    ));
                    api_succeeded = false;
                    if let Err(e) = app
                        .db
                        .upsert_messages(&thread_messages, &current_label_id)
                        .await
                    {
                        ui_state.toast = Some(Toast::new(
                            format!("Failed to restore messages after delete failure: {}", e),
                            ToastPosition::BottomRight,
                        ));
                    }
                    if let Ok(mut state) = app.sync_state.lock() {
                        for id in &message_ids {
                            state.recently_modified.remove(id);
                        }
                    }
                }
            }
        }

        if api_succeeded {
            app.remove_selected_message(ui_state);
            ui_state.load_thread_for_selected(&app.db).await?;
        }
    }

    Ok(true)
}

async fn handle_archive_action(
    key: &KeyEvent,
    app: &App,
    ui_state: &mut ui::UIState<'_>,
) -> anyhow::Result<bool> {
    if !matches_key(*key, &app.config.keybindings.archive) {
        return Ok(false);
    }

    if ui_state.focused_panel == FocusedPanel::Labels {
        return Ok(true);
    }
    ui_state.focused_panel = FocusedPanel::Messages;

    if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
        let thread_id = m.thread_id.clone();
        let thread_messages = app.db.get_messages_by_thread(&thread_id).await?;
        let message_ids: Vec<String> = thread_messages.iter().map(|m| m.id.clone()).collect();

        if let Ok(mut state) = app.sync_state.lock() {
            state.mark_modified_many(message_ids.clone());
        }

        let current_label_id = app.current_label_id(ui_state);

        let mut label_ids_to_remove = vec!["INBOX".to_string()];
        if !label_ids_to_remove.contains(&current_label_id) {
            label_ids_to_remove.push(current_label_id);
        }

        let protected_labels = ["DRAFT", "SENT"];
        let (removable_labels, skipped_labels): (Vec<_>, Vec<_>) = label_ids_to_remove
            .into_iter()
            .partition(|id| !protected_labels.contains(&id.as_str()));

        for label_id in &removable_labels {
            for id in &message_ids {
                if let Err(e) = app.db.remove_label_from_message(id, label_id).await {
                    ui_state.toast = Some(Toast::new(
                        format!("Failed to remove label from DB: {}", e),
                        ToastPosition::BottomRight,
                    ));
                }
            }
        }

        let mut api_succeeded = true;
        if let Some(gmail) = &app.gmail_client {
            let result = if removable_labels.is_empty() {
                Ok(())
            } else if removable_labels.len() == 1 && removable_labels[0] == "INBOX" {
                gmail.archive_messages(&message_ids).await
            } else {
                gmail
                    .remove_labels_from_messages(&message_ids, &removable_labels)
                    .await
            };

            match result {
                Ok(_) => {
                    if skipped_labels.is_empty() {
                        ui_state.toast = Some(Toast::new(
                            "Archived successfully",
                            ToastPosition::BottomRight,
                        ));
                    } else {
                        let skipped_list = skipped_labels.join(", ");
                        ui_state.toast = Some(Toast::new(
                            format!("Archived (cannot remove label: {})", skipped_list),
                            ToastPosition::BottomRight,
                        ));
                    }
                }
                Err(e) => {
                    ui_state.toast = Some(Toast::new(
                        format!("Archive failed: {}", e),
                        ToastPosition::BottomRight,
                    ));
                    api_succeeded = false;
                    for label_id in &removable_labels {
                        for id in &message_ids {
                            if let Err(e) = app.db.add_label_to_message(id, label_id).await {
                                ui_state.toast = Some(Toast::new(
                                    format!("Error restoring {} label: {}", label_id, e),
                                    ToastPosition::BottomRight,
                                ));
                            }
                        }
                    }
                    if let Ok(mut state) = app.sync_state.lock() {
                        for id in &message_ids {
                            state.recently_modified.remove(id);
                        }
                    }
                }
            }
        }

        if api_succeeded {
            let original_index = ui_state.selected_message_index;
            ui_state.undo_stack.push(UndoableAction::Archive {
                messages: thread_messages,
                label_ids: removable_labels.clone(),
                original_index,
            });

            app.remove_selected_message(ui_state);
            ui_state.load_thread_for_selected(&app.db).await?;
        }
    }

    Ok(true)
}

async fn handle_undo_action(
    key: &KeyEvent,
    app: &App,
    ui_state: &mut ui::UIState<'_>,
) -> anyhow::Result<bool> {
    if !matches_key(*key, &app.config.keybindings.undo) {
        return Ok(false);
    }

    if matches!(
        ui_state.focused_panel,
        FocusedPanel::Messages | FocusedPanel::Details
    ) && let Some(action) = ui_state.undo_stack.pop()
    {
        let description = action.to_string();
        match &action {
            UndoableAction::Delete {
                messages,
                label_id,
                original_index,
            } => {
                let representative = messages.first().cloned().unwrap_or_default();
                let insert_index = (*original_index).min(ui_state.messages.len());
                ui_state
                    .messages
                    .insert(insert_index, representative.clone());
                ui_state.selected_message_index = insert_index;

                let _ = app.db.upsert_messages(messages, label_id).await;

                if let Some(gmail) = &app.gmail_client {
                    let gmail = gmail.clone();
                    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                    tokio::spawn(async move {
                        for id in ids {
                            let _ = gmail.untrash_message(&id).await;
                        }
                    });
                }

                ui_state.load_thread_for_selected(&app.db).await?;
            }
            UndoableAction::Archive {
                messages,
                label_ids,
                original_index,
            } => {
                let representative = messages.first().cloned().unwrap_or_default();
                let current_label = ui_state
                    .labels
                    .get(ui_state.selected_label_index)
                    .map(|l| l.id.as_str());

                if label_ids
                    .iter()
                    .any(|label_id| current_label == Some(label_id.as_str()))
                {
                    let insert_index = (*original_index).min(ui_state.messages.len());
                    ui_state
                        .messages
                        .insert(insert_index, representative.clone());
                    ui_state.selected_message_index = insert_index;
                }

                for label_id in label_ids {
                    for message in messages {
                        let _ = app.db.add_label_to_message(&message.id, label_id).await;
                    }
                }

                if let Some(gmail) = &app.gmail_client {
                    let gmail = gmail.clone();
                    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                    let labels_to_restore = label_ids.clone();
                    tokio::spawn(async move {
                        for label_id in labels_to_restore {
                            if label_id == "INBOX" {
                                for id in &ids {
                                    let _ = gmail.unarchive_message(id).await;
                                }
                            } else {
                                for id in &ids {
                                    let _ = gmail.add_label_to_message(id, &label_id).await;
                                }
                            }
                        }
                    });
                }

                if label_ids
                    .iter()
                    .any(|label_id| current_label == Some(label_id.as_str()))
                {
                    ui_state.load_thread_for_selected(&app.db).await?;
                }
            }
        }
        ui_state.toast = Some(Toast::new(
            format!("Undone: {}", description),
            ToastPosition::BottomRight,
        ));
    }

    Ok(true)
}

fn handle_composing_keys(
    key: &KeyEvent,
    app: &App,
    ui_state: &mut ui::UIState<'_>,
) -> bool {
    match key.code {
        KeyCode::Esc => {
            ui_state.mode = ui::UIMode::Browsing;
            let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Hide);
            ui_state.compose_state = None;
        }
        _ if matches_key(*key, &app.config.keybindings.send_message) => {
            if let Some(cs) = &ui_state.compose_state
                && let Some(gmail) = &app.gmail_client
            {
                let (to, cc, bcc, sub, body) = (
                    cs.get_to(),
                    cs.get_cc(),
                    cs.get_bcc(),
                    cs.get_subject(),
                    cs.get_body(),
                );
                let gmail = gmail.clone();
                let db_clone = app.db.clone();
                let refresh_tx = app.refresh_tx.clone();
                tokio::spawn(async move {
                    if let Ok(Some(msg_id)) = gmail.send_message(&to, &cc, &bcc, &sub, &body).await
                        && let Ok(sent_msg) = gmail.get_message(&msg_id).await
                    {
                        let _ = db_clone.upsert_messages(&[sent_msg], "SENT").await;
                        let _ = refresh_tx.send(()).await;
                    }
                });
            }
            ui_state.mode = ui::UIMode::Browsing;
            let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Hide);
            ui_state.compose_state = None;
        }
        KeyCode::Char('b')
            if key
                .modifiers
                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
        {
            if let Some(cs) = &mut ui_state.compose_state {
                cs.show_cc_bcc = !cs.show_cc_bcc;
            }
        }
        KeyCode::Tab => {
            if let Some(cs) = &mut ui_state.compose_state {
                cs.focused_field = match cs.focused_field {
                    ui::ComposeField::To => {
                        if cs.show_cc_bcc {
                            ui::ComposeField::Cc
                        } else {
                            ui::ComposeField::Subject
                        }
                    }
                    ui::ComposeField::Cc => ui::ComposeField::Bcc,
                    ui::ComposeField::Bcc => ui::ComposeField::Subject,
                    ui::ComposeField::Subject => ui::ComposeField::Body,
                    ui::ComposeField::Body => ui::ComposeField::To,
                };
            }
        }
        KeyCode::BackTab => {
            if let Some(cs) = &mut ui_state.compose_state {
                cs.focused_field = match cs.focused_field {
                    ui::ComposeField::To => ui::ComposeField::Body,
                    ui::ComposeField::Cc => ui::ComposeField::To,
                    ui::ComposeField::Bcc => ui::ComposeField::Cc,
                    ui::ComposeField::Subject => {
                        if cs.show_cc_bcc {
                            ui::ComposeField::Bcc
                        } else {
                            ui::ComposeField::To
                        }
                    }
                    ui::ComposeField::Body => ui::ComposeField::Subject,
                };
            }
        }
        KeyCode::Enter => {
            if let Some(cs) = &mut ui_state.compose_state
                && cs.focused_field == ui::ComposeField::Body
            {
                cs.focused_textarea().input(*key);
            }
        }
        _ => {
            if let Some(cs) = &mut ui_state.compose_state {
                cs.focused_textarea().input(*key);
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[allow(clippy::print_stdout)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load();
    let debug_logging = std::env::args().any(|arg| arg == "--debug");
    logging::init(debug_logging);

    let db = db::Database::new("sqlite:gtui.db?mode=rwc").await?;
    db.run_migrations().await?;

    // Handle token reset
    if std::env::args().any(|arg| arg == "--reset-token") {
        auth::RingStorage.clear_token().await?;
        println!("Token cleared. Please restart without --reset-token to re-authenticate.");
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        ratatui::crossterm::terminal::EnterAlternateScreen,
        ratatui::crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut ui_state = ui::UIState::default();

    let sync_state = Arc::new(Mutex::new(sync::SyncState::default()));
    ui_state.sync_state = sync_state.clone();

    // Auth setup
    let secret = auth::load_secret("credentials.json").await?;

    use tokio::sync::mpsc;
    let (tx, mut rx) = mpsc::channel::<String>(1);
    let (done_tx, mut done_rx) = mpsc::channel::<bool>(1);
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<()>(1);
    let (priority_tx, priority_rx) = mpsc::channel::<String>(16);
    let mut priority_rx = Some(priority_rx);

    let auth_builder = auth::authenticate(secret, auth::TuiDelegate { tx }).await?;

    let auth_clone = auth_builder.clone();
    tokio::spawn(async move {
        if auth_clone.token(auth::SCOPES).await.is_ok() {
            let _ = done_tx.send(true).await;
        }
    });

    let mut app = App {
        config,
        db,
        gmail_client: None,
        sync_state: sync_state.clone(),
        priority_tx,
        refresh_tx,
        current_offset: 0,
        limit: 50,
    };

    let mut authenticated = false;

    loop {
        // Check for auth messages
        while let Ok(url) = rx.try_recv() {
            ui_state.auth_url = Some(url);
            ui_state.mode = ui::UIMode::Authentication;
        }

        if !authenticated && let Ok(true) = done_rx.try_recv() {
            authenticated = true;
            ui_state.mode = ui::UIMode::Browsing;
            ui_state.auth_url = None;

            let hub = Gmail::new(
                hyper::Client::builder().build(
                    hyper_rustls::HttpsConnectorBuilder::new()
                        .with_native_roots()
                        .expect("Failed to load native roots")
                        .https_only()
                        .enable_http1()
                        .build(),
                ),
                auth_builder.clone(),
            );

            let client = GmailClient::new(hub);
            app.gmail_client = Some(client.clone());

            if let Ok(Some(sig)) = client.get_signature().await {
                ui_state.remote_signature = Some(sig);
            }

            let sync_client = client.clone();
            let sync_db = app.db.clone();
            let sync_refresh_tx = app.refresh_tx.clone();
            let sync_state_clone = sync_state.clone();
            let priority_rx = priority_rx.take().unwrap();
            sync::spawn_sync_task(
                sync_client,
                sync_db,
                sync_refresh_tx,
                sync_state_clone,
                priority_rx,
            );

            // Load initial data
            ui_state.labels = app.db.get_labels().await?;
            if let Some(index) = ui_state.labels.iter().position(|l| l.id == "INBOX") {
                ui_state.selected_label_index = index;
            }
            if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                ui_state.messages = app
                    .db
                    .get_messages_by_label(&label.id, app.limit, app.current_offset)
                    .await?;
                if let Some(msg) = ui_state.messages.get(ui_state.selected_message_index) {
                    ui_state.threaded_messages =
                        app.db.get_messages_by_thread(&msg.thread_id).await?;
                }
            }
        }

        // Drain sync refresh signals, then reload once
        let mut needs_refresh = false;
        while let Ok(()) = refresh_rx.try_recv() {
            needs_refresh = true;
        }
        if needs_refresh {
            ui_state
                .refresh_labels_and_messages(&app.db, app.limit, &mut app.current_offset)
                .await?;
        }

        if let Some(toast) = &ui_state.toast
            && toast.is_expired()
        {
            ui_state.toast = None;
        }

        terminal.draw(|f| ui::render(f, &mut ui_state))?;

        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if !authenticated && key.code != KeyCode::Char('q') {
                continue;
            }

            match ui_state.mode {
                ui::UIMode::Authentication => {
                    if key.code == KeyCode::Char('q') {
                        break;
                    }
                }
                ui::UIMode::Browsing => {
                    if !matches_key(key, &app.config.keybindings.undo) {
                        ui_state.toast = None;
                    }
                    if matches_key(key, &app.config.keybindings.quit) {
                        break;
                    }

                    if handle_navigation_keys(&key, &mut app, &mut ui_state).await? {
                        continue;
                    }
                    if handle_message_actions(&key, &app, &mut ui_state) {
                        continue;
                    }
                    if handle_delete_action(&key, &app, &mut ui_state).await? {
                        continue;
                    }
                    if handle_archive_action(&key, &app, &mut ui_state).await? {
                        continue;
                    }
                    if handle_undo_action(&key, &app, &mut ui_state).await? {
                        continue;
                    }
                }
                ui::UIMode::Composing => {
                    handle_composing_keys(&key, &app, &mut ui_state);
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        ratatui::crossterm::terminal::LeaveAlternateScreen,
        ratatui::crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
