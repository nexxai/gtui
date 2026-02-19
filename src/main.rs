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

#[allow(clippy::too_many_arguments)]
async fn handle_navigation_keys(
    key: &KeyEvent,
    config: &Config,
    ui_state: &mut ui::UIState<'_>,
    db: &db::Database,
    current_offset: &mut i64,
    limit: i64,
    priority_tx: &tokio::sync::mpsc::Sender<String>,
    debug_logging: bool,
) -> anyhow::Result<bool> {
    // Panel switching
    if matches_key(*key, &config.keybindings.prev_panel) {
        ui_state.focused_panel = match ui_state.focused_panel {
            FocusedPanel::Details => FocusedPanel::Messages,
            FocusedPanel::Messages => FocusedPanel::Labels,
            FocusedPanel::Labels => FocusedPanel::Labels,
        };
        return Ok(true);
    }
    if matches_key(*key, &config.keybindings.next_panel) {
        ui_state.focused_panel = match ui_state.focused_panel {
            FocusedPanel::Labels => FocusedPanel::Messages,
            FocusedPanel::Messages => FocusedPanel::Details,
            FocusedPanel::Details => FocusedPanel::Details,
        };
        return Ok(true);
    }

    // Navigation within panels
    if matches_key(*key, &config.keybindings.move_down) {
        match ui_state.focused_panel {
            FocusedPanel::Labels => {
                if ui_state.selected_label_index < ui_state.labels.len().saturating_sub(1) {
                    ui_state.selected_label_index += 1;
                    let label_id = ui_state.labels[ui_state.selected_label_index].id.clone();
                    *current_offset = 0;
                    ui_state.messages = db
                        .get_messages_by_label(&label_id, limit, *current_offset)
                        .await?;
                    ui_state.selected_message_index = 0;
                    ui_state.detail_scroll = 0;
                    ui_state.load_thread_for_selected(db).await?;
                    let _ = priority_tx.try_send(label_id);
                }
            }
            FocusedPanel::Messages => {
                if ui_state.selected_message_index < ui_state.messages.len().saturating_sub(1) {
                    let old_idx = ui_state.selected_message_index;
                    ui_state.selected_message_index += 1;
                    ui_state.detail_scroll = 0;
                    if let Some(msg) = ui_state.messages.get(ui_state.selected_message_index) {
                        logging::debug(
                            debug_logging,
                            &format!(
                                "[Main] Navigating: idx {} -> {}, thread_id: {:?}",
                                old_idx, ui_state.selected_message_index, msg.thread_id
                            ),
                        );
                        ui_state.load_thread_for_selected(db).await?;
                        logging::debug(
                            debug_logging,
                            &format!(
                                "[Main] Loaded {} messages for thread",
                                ui_state.threaded_messages.len()
                            ),
                        );
                    }

                    if ui_state.selected_message_index >= ui_state.messages.len().saturating_sub(5)
                    {
                        *current_offset += limit;
                        if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                            let mut additional = db
                                .get_messages_by_label(&label.id, limit, *current_offset)
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

    if matches_key(*key, &config.keybindings.move_up) {
        match ui_state.focused_panel {
            FocusedPanel::Labels => {
                if ui_state.selected_label_index > 0 {
                    ui_state.selected_label_index -= 1;
                    let label_id = ui_state.labels[ui_state.selected_label_index].id.clone();
                    *current_offset = 0;
                    ui_state.messages = db
                        .get_messages_by_label(&label_id, limit, *current_offset)
                        .await?;
                    ui_state.selected_message_index = 0;
                    ui_state.detail_scroll = 0;
                    ui_state.load_thread_for_selected(db).await?;
                    let _ = priority_tx.try_send(label_id);
                }
            }
            FocusedPanel::Messages => {
                if ui_state.selected_message_index > 0 {
                    ui_state.selected_message_index -= 1;
                    ui_state.detail_scroll = 0;
                    ui_state.load_thread_for_selected(db).await?;
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
    config: &Config,
    ui_state: &mut ui::UIState<'_>,
    gmail_client: &Option<GmailClient>,
    db_url: &str,
) -> bool {
    if matches_key(*key, &config.keybindings.mark_read) {
        // Toggle Read/Unread
        if let Some(m) = ui_state.messages.get_mut(ui_state.selected_message_index) {
            let is_currently_read = m.is_read;
            m.is_read = !is_currently_read;
            let id = m.id.clone();
            if let Some(gmail) = &gmail_client {
                let gmail = gmail.clone();
                let db_url_str = db_url.to_owned();
                let new_status = !is_currently_read;
                tokio::spawn(async move {
                    if let Ok(db_clone) = db::Database::new(&db_url_str).await {
                        if new_status {
                            let _ = gmail.mark_as_read(&id).await;
                        } else {
                            let _ = gmail.mark_as_unread(&id).await;
                        }
                        let _ = db_clone.mark_message_as_read(&id, new_status).await;
                    }
                });
            }
        }
        return true;
    }
    if matches_key(*key, &config.keybindings.reply) {
        // Reply
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
                .or(config.signatures.reply.as_ref());
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
    if matches_key(*key, &config.keybindings.forward) {
        // Forward
        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
            let subject = m.subject.as_deref().unwrap_or("");
            let new_subject = if subject.to_lowercase().starts_with("fwd:")
                || subject.to_lowercase().starts_with("fw:")
            {
                subject.to_string()
            } else {
                format!("Fwd: {}", subject)
            };

            // Build forwarded body
            let mut forward_body = String::new();

            // Two blank lines at top for user's context
            forward_body.push_str("\n\n");

            // Add signature (new_message signature for forwards)
            let sig_to_use = ui_state
                .remote_signature
                .as_ref()
                .or(config.signatures.new_message.as_ref());
            if let Some(sig) = sig_to_use {
                forward_body.push_str("--\n");
                forward_body.push_str(sig);
                forward_body.push('\n');
            }

            // Forwarding header block
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

            // Original message body
            let body_to_forward = m.body_plain.as_ref().or(m.snippet.as_ref());
            if let Some(body) = body_to_forward {
                forward_body.push_str(&format!("\n{}", body));
            }

            ui_state.mode = ui::UIMode::Composing;
            let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Show);
            let compose = ui::ComposeState::new(
                "", // Empty To field
                "",
                "",
                &new_subject,
                &forward_body,
            );
            // Cursor starts in To field (default)
            ui_state.compose_state = Some(compose);
        }
        return true;
    }
    if matches_key(*key, &config.keybindings.new_message) {
        // New message
        ui_state.mode = ui::UIMode::Composing;
        let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Show);

        let mut body = String::new();
        let sig_to_use = ui_state
            .remote_signature
            .as_ref()
            .or(config.signatures.new_message.as_ref());
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
    config: &Config,
    ui_state: &mut ui::UIState<'_>,
    db: &db::Database,
    gmail_client: &Option<GmailClient>,
    sync_state_loop: &Arc<Mutex<sync::SyncState>>,
) -> anyhow::Result<bool> {
    if !matches_key(*key, &config.keybindings.delete) {
        return Ok(false);
    }

    // Do nothing if labels panel is active
    if ui_state.focused_panel == FocusedPanel::Labels {
        return Ok(true);
    }
    // Ensure conversations list is the active panel
    ui_state.focused_panel = FocusedPanel::Messages;
    // Delete all messages in the thread
    if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
        let thread_id = m.thread_id.clone();

        // Get all messages in the thread from the database
        let thread_messages = db.get_messages_by_thread(&thread_id).await?;
        let message_ids: Vec<String> = thread_messages.iter().map(|m| m.id.clone()).collect();

        // Mark messages as recently modified to prevent sync from re-adding them
        if let Ok(mut state) = sync_state_loop.lock() {
            state.mark_modified_many(message_ids.clone());
        }

        // Capture for undo BEFORE removing
        let current_label_id = ui_state
            .labels
            .get(ui_state.selected_label_index)
            .map(|l| l.id.clone())
            .unwrap_or_else(|| "INBOX".to_string());
        let original_index = ui_state.selected_message_index;
        ui_state.undo_stack.push(UndoableAction::Delete {
            messages: thread_messages.clone(),
            label_id: current_label_id.clone(),
            original_index,
        });

        // Delete from database SYNCHRONOUSLY to ensure consistency
        for id in &message_ids {
            if let Err(e) = db.delete_message(id).await {
                ui_state.toast = Some(Toast::new(
                    format!("Failed to delete message from DB: {}", e),
                    ToastPosition::BottomRight,
                ));
            }
        }

        // Call Gmail API and await result to ensure it succeeds
        let mut api_succeeded = true;
        if let Some(gmail) = &gmail_client {
            logging::debug(
                ui_state.debug_logging,
                &format!(
                    "About to call trash_messages for {} messages",
                    message_ids.len()
                ),
            );
            match gmail.trash_messages(&message_ids).await {
                Ok(_) => {
                    ui_state.toast = Some(Toast::new(
                        "Deleted successfully",
                        ToastPosition::BottomRight,
                    ));
                }
                Err(e) => {
                    logging::debug(
                        ui_state.debug_logging,
                        &format!("trash_messages returned error: {}", e),
                    );
                    ui_state.toast = Some(Toast::new(
                        format!("Delete failed: {}", e),
                        ToastPosition::BottomRight,
                    ));
                    api_succeeded = false;
                    // Restore messages to database since API failed
                    if let Err(e) = db
                        .upsert_messages(&thread_messages, &current_label_id)
                        .await
                    {
                        ui_state.toast = Some(Toast::new(
                            format!("Failed to restore messages after delete failure: {}", e),
                            ToastPosition::BottomRight,
                        ));
                    }
                    // Remove from recently_modified since operation failed
                    if let Ok(mut state) = sync_state_loop.lock() {
                        for id in &message_ids {
                            state.recently_modified.remove(id);
                        }
                    }
                }
            }
        }

        // Only update UI if API succeeded (or if no gmail client)
        if api_succeeded {
            ui_state.messages.remove(ui_state.selected_message_index);
            if ui_state.selected_message_index >= ui_state.messages.len()
                && !ui_state.messages.is_empty()
            {
                ui_state.selected_message_index = ui_state.messages.len() - 1;
            }

            // Refresh detail view
            ui_state.load_thread_for_selected(db).await?;
        }
    }

    Ok(true)
}

async fn handle_archive_action(
    key: &KeyEvent,
    config: &Config,
    ui_state: &mut ui::UIState<'_>,
    db: &db::Database,
    gmail_client: &Option<GmailClient>,
    sync_state_loop: &Arc<Mutex<sync::SyncState>>,
) -> anyhow::Result<bool> {
    if !matches_key(*key, &config.keybindings.archive) {
        return Ok(false);
    }

    // Do nothing if labels panel is active
    if ui_state.focused_panel == FocusedPanel::Labels {
        return Ok(true);
    }
    // Ensure conversations list is the active panel
    ui_state.focused_panel = FocusedPanel::Messages;
    // Archive all messages in the thread
    if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
        let thread_id = m.thread_id.clone();

        // Get all messages in the thread from the database
        let thread_messages = db.get_messages_by_thread(&thread_id).await?;
        let message_ids: Vec<String> = thread_messages.iter().map(|m| m.id.clone()).collect();

        // Mark messages as recently modified to prevent sync from re-adding them
        if let Ok(mut state) = sync_state_loop.lock() {
            state.mark_modified_many(message_ids.clone());
        }

        // Determine which labels to remove: INBOX plus the current label (if any)
        let current_label_id = ui_state
            .labels
            .get(ui_state.selected_label_index)
            .map(|l| l.id.clone())
            .unwrap_or_else(|| "INBOX".to_string());

        let mut label_ids_to_remove = vec!["INBOX".to_string()];
        if !label_ids_to_remove.contains(&current_label_id) {
            label_ids_to_remove.push(current_label_id.clone());
        }

        let protected_labels = ["DRAFT", "SENT"];
        let mut removable_labels = Vec::new();
        let mut skipped_labels = Vec::new();
        for label_id in label_ids_to_remove {
            if protected_labels.contains(&label_id.as_str()) {
                skipped_labels.push(label_id);
            } else {
                removable_labels.push(label_id);
            }
        }

        // Remove the label from database SYNCHRONOUSLY to ensure consistency
        for label_id in &removable_labels {
            for id in &message_ids {
                if let Err(e) = db.remove_label_from_message(id, label_id).await {
                    ui_state.toast = Some(Toast::new(
                        format!("Failed to remove label from DB: {}", e),
                        ToastPosition::BottomRight,
                    ));
                }
            }
        }

        // Call Gmail API and await result to ensure it succeeds
        let mut api_succeeded = true;
        if let Some(gmail) = &gmail_client {
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
                    // Restore label since API failed
                    for label_id in &removable_labels {
                        for id in &message_ids {
                            if let Err(e) = db.add_label_to_message(id, label_id).await {
                                ui_state.toast = Some(Toast::new(
                                    format!("Error restoring {} label: {}", label_id, e),
                                    ToastPosition::BottomRight,
                                ));
                            }
                        }
                    }
                    // Remove from recently_modified since operation failed
                    if let Ok(mut state) = sync_state_loop.lock() {
                        for id in &message_ids {
                            state.recently_modified.remove(id);
                        }
                    }
                }
            }
        }

        // Only update UI if API succeeded (or if no gmail client)
        if api_succeeded {
            // Capture for undo BEFORE removing
            let original_index = ui_state.selected_message_index;
            ui_state.undo_stack.push(UndoableAction::Archive {
                messages: thread_messages,
                label_ids: removable_labels.clone(),
                original_index,
            });

            ui_state.messages.remove(ui_state.selected_message_index);
            if ui_state.selected_message_index >= ui_state.messages.len()
                && !ui_state.messages.is_empty()
            {
                ui_state.selected_message_index = ui_state.messages.len() - 1;
            }

            // Refresh detail view
            ui_state.load_thread_for_selected(db).await?;
        }
    }

    Ok(true)
}

async fn handle_undo_action(
    key: &KeyEvent,
    config: &Config,
    ui_state: &mut ui::UIState<'_>,
    db: &db::Database,
    gmail_client: &Option<GmailClient>,
) -> anyhow::Result<bool> {
    if !matches_key(*key, &config.keybindings.undo) {
        return Ok(false);
    }

    // Undo - only in Messages or Details panel
    if matches!(
        ui_state.focused_panel,
        FocusedPanel::Messages | FocusedPanel::Details
    ) && let Some(action) = ui_state.undo_stack.pop()
    {
        let description = action.description();
        match &action {
            UndoableAction::Delete {
                messages,
                label_id,
                original_index,
            } => {
                // Get the representative message (first one) for UI insertion
                let representative = messages.first().cloned().unwrap_or_default();

                // Re-insert into UI at original position (clamped to list size)
                let insert_index = (*original_index).min(ui_state.messages.len());
                ui_state
                    .messages
                    .insert(insert_index, representative.clone());
                ui_state.selected_message_index = insert_index;

                // Re-insert all messages into database
                let _ = db.upsert_messages(messages, label_id).await;

                // Untrash all messages via Gmail API
                if let Some(gmail) = &gmail_client {
                    let gmail = gmail.clone();
                    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                    tokio::spawn(async move {
                        for id in ids {
                            let _ = gmail.untrash_message(&id).await;
                        }
                    });
                }

                // Refresh detail view
                ui_state.load_thread_for_selected(db).await?;
            }
            UndoableAction::Archive {
                messages,
                label_ids,
                original_index,
            } => {
                // Get the representative message (first one) for UI insertion
                let representative = messages.first().cloned().unwrap_or_default();

                // Re-insert into UI at original position (only if viewing the same label)
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

                // Re-add the removed label in database for all messages
                for label_id in label_ids {
                    for message in messages {
                        let _ = db.add_label_to_message(&message.id, label_id).await;
                    }
                }

                // Restore label via Gmail API
                if let Some(gmail) = &gmail_client {
                    let gmail = gmail.clone();
                    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                    let labels_to_restore = label_ids.clone();
                    tokio::spawn(async move {
                        for label_id in labels_to_restore {
                            if label_id == "INBOX" {
                                // Use unarchive for INBOX
                                for id in &ids {
                                    let _ = gmail.unarchive_message(id).await;
                                }
                            } else {
                                // Use add_label_to_message for other labels
                                for id in &ids {
                                    let _ = gmail.add_label_to_message(id, &label_id).await;
                                }
                            }
                        }
                    });
                }

                // Refresh detail view if message was re-added
                if label_ids
                    .iter()
                    .any(|label_id| current_label == Some(label_id.as_str()))
                {
                    ui_state.load_thread_for_selected(db).await?;
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
    config: &Config,
    ui_state: &mut ui::UIState<'_>,
    gmail_client: &Option<GmailClient>,
    db_url: &str,
    refresh_tx: &tokio::sync::mpsc::Sender<()>,
) -> bool {
    match key.code {
        KeyCode::Esc => {
            ui_state.mode = ui::UIMode::Browsing;
            let _ = execute!(io::stdout(), ratatui::crossterm::cursor::Hide);
            ui_state.compose_state = None;
        }
        _ if matches_key(*key, &config.keybindings.send_message) => {
            if let Some(cs) = &ui_state.compose_state
                && let Some(gmail) = &gmail_client
            {
                let (to, cc, bcc, sub, body) = (
                    cs.get_to(),
                    cs.get_cc(),
                    cs.get_bcc(),
                    cs.get_subject(),
                    cs.get_body(),
                );
                let gmail = gmail.clone();
                let db_url_str = db_url.to_owned();
                let refresh_tx_clone = refresh_tx.clone();
                tokio::spawn(async move {
                    // Send the message and get its ID
                    if let Ok(Some(msg_id)) = gmail.send_message(&to, &cc, &bcc, &sub, &body).await
                    {
                        // Fetch the sent message to get full details including thread_id
                        if let Ok(sent_msg) = gmail.get_message(&msg_id).await {
                            // Store in database with SENT label
                            if let Ok(db_clone) = db::Database::new(&db_url_str).await {
                                let _ = db_clone.upsert_messages(&[sent_msg], "SENT").await;
                                // Trigger a refresh so the UI updates
                                let _ = refresh_tx_clone.send(()).await;
                            }
                        }
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
            // Ignore Enter in single-line fields (to/cc/bcc/subject)
        }
        _ => {
            if let Some(cs) = &mut ui_state.compose_state {
                cs.focused_textarea().input(*key);
            }
        }
    }

    true
}

#[allow(clippy::print_stdout)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load();
    let debug_logging = std::env::args().any(|arg| arg == "--debug");
    let db_url = "sqlite:gtui.db?mode=rwc".to_string();
    let db = db::Database::new(&db_url).await?;
    db.run_migrations().await?;

    // Handle token reset
    if std::env::args().any(|arg| arg == "--reset-token") {
        auth::RingStorage.clear_token().await?;
        println!("Token cleared. Please restart without --reset-token to re-authenticate.");
        return Ok(());
    }

    // Setup terminal early
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        ratatui::crossterm::terminal::EnterAlternateScreen,
        ratatui::crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut ui_state = ui::UIState {
        debug_logging,
        ..Default::default()
    };

    // Shared sync state for UI awareness
    let sync_state = Arc::new(Mutex::new(sync::SyncState::default()));
    ui_state.sync_state = sync_state.clone();

    // Initial Auth setup
    let secret = auth::Authenticator::load_secret("credentials.json").await?;

    use tokio::sync::mpsc;
    let (tx, mut rx) = mpsc::channel::<String>(1);
    let (done_tx, mut done_rx) = mpsc::channel::<bool>(1);
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<()>(1);
    let (priority_tx, priority_rx) = mpsc::channel::<String>(16);
    let mut priority_rx = Some(priority_rx);

    let auth_builder = auth::Authenticator::authenticate(secret, auth::TuiDelegate { tx }).await?;

    let auth_clone = auth_builder.clone();
    tokio::spawn(async move {
        if auth_clone.token(auth::SCOPES).await.is_ok() {
            let _ = done_tx.send(true).await;
        }
    });

    let mut authenticated = false;
    let mut current_offset: i64 = 0;
    let limit: i64 = 50;

    // We'll hold these in Options until authenticated
    let mut gmail_client: Option<GmailClient> = None;

    // Clone sync_state for use in the main loop
    let sync_state_loop = sync_state.clone();

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

            // Now create the hub and client
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

            let client = GmailClient::new(hub, debug_logging);
            gmail_client = Some(client.clone());

            // Fetch remote signature
            if let Ok(Some(sig)) = client.get_signature().await {
                ui_state.remote_signature = Some(sig);
            }

            // Kick off sync
            let sync_client = client.clone();
            let sync_db_url = db_url.clone();
            let sync_refresh_tx = refresh_tx.clone();
            let sync_state_clone = sync_state.clone();
            let priority_rx = priority_rx.take().unwrap();
            sync::spawn_sync_task(
                sync_client,
                sync_db_url,
                sync_refresh_tx,
                sync_state_clone,
                priority_rx,
            );

            // Load initial data for UI
            ui_state.labels = db.get_labels().await?;
            if let Some(index) = ui_state.labels.iter().position(|l| l.id == "INBOX") {
                ui_state.selected_label_index = index;
            }
            if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                ui_state.messages = db
                    .get_messages_by_label(&label.id, limit, current_offset)
                    .await?;
                if let Some(msg) = ui_state.messages.get(ui_state.selected_message_index) {
                    ui_state.threaded_messages = db.get_messages_by_thread(&msg.thread_id).await?;
                }
            }
        }

        // Check for sync refresh — drain all pending signals, then reload once
        let mut needs_refresh = false;
        while let Ok(()) = refresh_rx.try_recv() {
            needs_refresh = true;
        }
        if needs_refresh {
            ui_state
                .refresh_labels_and_messages(&db, limit, &mut current_offset)
                .await?;
        }

        // Check toast timeout
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
            // Only handle keys if authenticated or to quit
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
                    // Clear toast on any keypress (will be set again if action is pressed)
                    if !matches_key(key, &config.keybindings.undo) {
                        ui_state.toast = None;
                    }
                    if matches_key(key, &config.keybindings.quit) {
                        break;
                    }

                    let handled = handle_navigation_keys(
                        &key,
                        &config,
                        &mut ui_state,
                        &db,
                        &mut current_offset,
                        limit,
                        &priority_tx,
                        debug_logging,
                    )
                    .await?;
                    if handled {
                        continue;
                    }
                    if handle_message_actions(&key, &config, &mut ui_state, &gmail_client, &db_url)
                    {
                        continue;
                    }

                    if handle_delete_action(
                        &key,
                        &config,
                        &mut ui_state,
                        &db,
                        &gmail_client,
                        &sync_state_loop,
                    )
                    .await?
                    {
                        continue;
                    }

                    if handle_archive_action(
                        &key,
                        &config,
                        &mut ui_state,
                        &db,
                        &gmail_client,
                        &sync_state_loop,
                    )
                    .await?
                    {
                        continue;
                    }

                    let handled =
                        handle_undo_action(&key, &config, &mut ui_state, &db, &gmail_client)
                            .await?;
                    if handled {
                        continue;
                    }
                }
                ui::UIMode::Composing => {
                    let _ = handle_composing_keys(
                        &key,
                        &config,
                        &mut ui_state,
                        &gmail_client,
                        &db_url,
                        &refresh_tx,
                    );
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
