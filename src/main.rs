mod auth;
mod config;
mod db;
mod gmail;
mod logging;
mod models;
mod sync;
mod ui;
mod undo;

use crate::config::{Config, matches_key};
use crate::gmail::GmailClient;
use crate::ui::FocusedPanel;
use crate::undo::UndoableAction;
use chrono::{DateTime, Local};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use google_gmail1::Gmail;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::sync::{Arc, Mutex};

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
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut ui_state = ui::UIState::default();
    ui_state.debug_logging = debug_logging;

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
        if let Ok(_) = auth_clone.token(auth::SCOPES).await {
            let _ = done_tx.send(true).await;
        }
    });

    let mut authenticated = false;
    let mut current_offset = 0;
    let limit = 50;

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

        if !authenticated {
            if let Ok(true) = done_rx.try_recv() {
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
                        ui_state.threaded_messages =
                            db.get_messages_by_thread(&msg.thread_id).await?;
                    }
                }
            }
        }

        // Check for sync refresh — drain all pending signals, then reload once
        let mut needs_refresh = false;
        while let Ok(()) = refresh_rx.try_recv() {
            needs_refresh = true;
        }
        if needs_refresh {
            // Re-load labels
            ui_state.labels = db.get_labels().await?;
            if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                // Re-load messages for current label
                let mut new_messages = db
                    .get_messages_by_label(&label.id, limit, current_offset)
                    .await?;

                // If we got no messages but have an offset, we might be scrolled past the end.
                // Reset to 0 and try again.
                if new_messages.is_empty() && current_offset > 0 {
                    current_offset = 0;
                    new_messages = db.get_messages_by_label(&label.id, limit, 0).await?;
                }

                // If the message list changed, we need to be careful with the selection index
                ui_state.messages = new_messages;

                // Clamp selection index
                if !ui_state.messages.is_empty() {
                    if ui_state.selected_message_index >= ui_state.messages.len() {
                        ui_state.selected_message_index = ui_state.messages.len().saturating_sub(1);
                    }

                    // Re-load threaded messages for selected message
                    if let Some(msg) = ui_state.messages.get(ui_state.selected_message_index) {
                        logging::debug(
                            debug_logging,
                            &format!("[Main] Sync refresh loading thread_id: {:?}", msg.thread_id),
                        );
                        ui_state.threaded_messages =
                            db.get_messages_by_thread(&msg.thread_id).await?;
                        logging::debug(
                            debug_logging,
                            &format!(
                                "[Main] Sync refresh loaded {} messages",
                                ui_state.threaded_messages.len()
                            ),
                        );
                    }
                } else {
                    ui_state.selected_message_index = 0;
                    logging::debug(
                        debug_logging,
                        "[Main] Clearing threaded_messages (no messages in label)",
                    );
                    ui_state.threaded_messages.clear();
                }
            }
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
                    // Clear status message on any keypress (will be set again if undo is pressed)
                    if !matches_key(key, &config.keybindings.undo) {
                        ui_state.status_message = None;
                    }
                    if matches_key(key, &config.keybindings.quit) {
                        break;
                    }

                    // Panel switching
                    if matches_key(key, &config.keybindings.prev_panel) {
                        ui_state.focused_panel = match ui_state.focused_panel {
                            FocusedPanel::Details => FocusedPanel::Messages,
                            FocusedPanel::Messages => FocusedPanel::Labels,
                            FocusedPanel::Labels => FocusedPanel::Labels,
                        };
                    } else if matches_key(key, &config.keybindings.next_panel) {
                        ui_state.focused_panel = match ui_state.focused_panel {
                            FocusedPanel::Labels => FocusedPanel::Messages,
                            FocusedPanel::Messages => FocusedPanel::Details,
                            FocusedPanel::Details => FocusedPanel::Details,
                        };
                    }
                    // Navigation within panels
                    else if matches_key(key, &config.keybindings.move_down) {
                        match ui_state.focused_panel {
                            FocusedPanel::Labels => {
                                if ui_state.selected_label_index
                                    < ui_state.labels.len().saturating_sub(1)
                                {
                                    ui_state.selected_label_index += 1;
                                    let label = &ui_state.labels[ui_state.selected_label_index];
                                    current_offset = 0;
                                    ui_state.messages = db
                                        .get_messages_by_label(&label.id, limit, current_offset)
                                        .await?;
                                    ui_state.selected_message_index = 0;
                                    ui_state.detail_scroll = 0;
                                    if let Some(msg) = ui_state.messages.get(0) {
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&msg.thread_id).await?;
                                    } else {
                                        ui_state.threaded_messages.clear();
                                    }
                                    let _ = priority_tx.try_send(label.id.clone());
                                }
                            }
                            FocusedPanel::Messages => {
                                if ui_state.selected_message_index
                                    < ui_state.messages.len().saturating_sub(1)
                                {
                                    let old_idx = ui_state.selected_message_index;
                                    ui_state.selected_message_index += 1;
                                    ui_state.detail_scroll = 0;
                                    if let Some(msg) =
                                        ui_state.messages.get(ui_state.selected_message_index)
                                    {
                                        logging::debug(
                                            debug_logging,
                                            &format!(
                                                "[Main] Navigating: idx {} -> {}, thread_id: {:?}",
                                                old_idx, ui_state.selected_message_index, msg.thread_id
                                            ),
                                        );
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&msg.thread_id).await?;
                                        logging::debug(
                                            debug_logging,
                                            &format!(
                                                "[Main] Loaded {} messages for thread",
                                                ui_state.threaded_messages.len()
                                            ),
                                        );
                                    }

                                    if ui_state.selected_message_index
                                        >= ui_state.messages.len().saturating_sub(5)
                                    {
                                        current_offset += limit;
                                        if let Some(label) =
                                            ui_state.labels.get(ui_state.selected_label_index)
                                        {
                                            let mut additional = db
                                                .get_messages_by_label(
                                                    &label.id,
                                                    limit,
                                                    current_offset,
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
                    } else if matches_key(key, &config.keybindings.move_up) {
                        match ui_state.focused_panel {
                            FocusedPanel::Labels => {
                                if ui_state.selected_label_index > 0 {
                                    ui_state.selected_label_index -= 1;
                                    let label = &ui_state.labels[ui_state.selected_label_index];
                                    current_offset = 0;
                                    ui_state.messages = db
                                        .get_messages_by_label(&label.id, limit, current_offset)
                                        .await?;
                                    ui_state.selected_message_index = 0;
                                    ui_state.detail_scroll = 0;
                                    if let Some(msg) = ui_state.messages.get(0) {
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&msg.thread_id).await?;
                                    } else {
                                        ui_state.threaded_messages.clear();
                                    }
                                    let _ = priority_tx.try_send(label.id.clone());
                                }
                            }
                            FocusedPanel::Messages => {
                                if ui_state.selected_message_index > 0 {
                                    ui_state.selected_message_index -= 1;
                                    ui_state.detail_scroll = 0;
                                    if let Some(msg) =
                                        ui_state.messages.get(ui_state.selected_message_index)
                                    {
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&msg.thread_id).await?;
                                    }
                                }
                            }
                            FocusedPanel::Details => {
                                ui_state.detail_scroll = ui_state.detail_scroll.saturating_sub(1);
                            }
                        }
                    }
                    // Email Actions
                    else if matches_key(key, &config.keybindings.mark_read) {
                        // Toggle Read/Unread
                        if let Some(m) = ui_state.messages.get_mut(ui_state.selected_message_index)
                        {
                            let is_currently_read = m.is_read;
                            m.is_read = !is_currently_read;
                            let id = m.id.clone();
                            if let Some(gmail) = &gmail_client {
                                let gmail = gmail.clone();
                                let db_url_str = db_url.clone();
                                let new_status = !is_currently_read;
                                tokio::spawn(async move {
                                    if let Ok(db_clone) = db::Database::new(&db_url_str).await {
                                        if new_status {
                                            let _ = gmail.mark_as_read(&id).await;
                                        } else {
                                            let _ = gmail.mark_as_unread(&id).await;
                                        }
                                        let _ =
                                            db_clone.mark_message_as_read(&id, new_status).await;
                                    }
                                });
                            }
                        }
                    } else if matches_key(key, &config.keybindings.reply) {
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
                            let _ = execute!(io::stdout(), crossterm::cursor::Show);
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
                    } else if matches_key(key, &config.keybindings.forward) {
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
                            let _ = execute!(io::stdout(), crossterm::cursor::Show);
                            let compose = ui::ComposeState::new(
                                "",  // Empty To field
                                "",
                                "",
                                &new_subject,
                                &forward_body,
                            );
                            // Cursor starts in To field (default)
                            ui_state.compose_state = Some(compose);
                        }
                    } else if matches_key(key, &config.keybindings.new_message) {
                        // New message
                        ui_state.mode = ui::UIMode::Composing;
                        let _ = execute!(io::stdout(), crossterm::cursor::Show);

                        let mut body = String::new();
                        let sig_to_use = ui_state
                            .remote_signature
                            .as_ref()
                            .or(config.signatures.new_message.as_ref());
                        if let Some(sig) = sig_to_use {
                            body.push_str("\n\n--\n");
                            body.push_str(sig);
                        }

                        ui_state.compose_state = Some(ui::ComposeState::new(
                            "",
                            "",
                            "",
                            "",
                            &body,
                        ));
                    } else if matches_key(key, &config.keybindings.delete) {
                        // Do nothing if labels panel is active
                        if ui_state.focused_panel == FocusedPanel::Labels {
                            continue;
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
                                    eprintln!("Error deleting message from DB: {}", e);
                                }
                            }

                            // Call Gmail API and await result to ensure it succeeds
                            let mut api_succeeded = true;
                            if let Some(gmail) = &gmail_client {
                                match gmail.trash_messages(&message_ids).await {
                                    Ok(_) => {
                                        ui_state.status_message = Some("Deleted successfully".to_string());
                                    }
                                    Err(e) => {
                                        eprintln!("Error trashing messages: {}", e);
                                        ui_state.status_message = Some(format!("Delete failed: {}", e));
                                        api_succeeded = false;
                                        // Restore messages to database since API failed
                                        if let Err(e) = db.upsert_messages(&thread_messages, &current_label_id).await {
                                            eprintln!("Error restoring messages to DB: {}", e);
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
                                if let Some(msg) =
                                    ui_state.messages.get(ui_state.selected_message_index)
                                {
                                    ui_state.threaded_messages =
                                        db.get_messages_by_thread(&msg.thread_id).await?;
                                } else {
                                    ui_state.threaded_messages.clear();
                                }
                            }
                        }
                    } else if matches_key(key, &config.keybindings.archive) {
                        // Do nothing if labels panel is active
                        if ui_state.focused_panel == FocusedPanel::Labels {
                            continue;
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
                            
                            // Determine which label to remove: INBOX normally, or Category label if viewing a Category
                            let current_label_id = ui_state
                                .labels
                                .get(ui_state.selected_label_index)
                                .map(|l| l.id.clone())
                                .unwrap_or_else(|| "INBOX".to_string());
                            
                            // If viewing a Category label (CATEGORY_*), remove that label instead of INBOX
                            // Otherwise, remove INBOX (standard archive behavior)
                            let label_to_remove = if current_label_id.starts_with("CATEGORY_") {
                                current_label_id.clone()
                            } else {
                                "INBOX".to_string()
                            };
                            
                            // Remove the label from database SYNCHRONOUSLY to ensure consistency
                            for id in &message_ids {
                                if let Err(e) = db.remove_label_from_message(id, &label_to_remove).await {
                                    eprintln!("Error removing {} label from DB: {}", label_to_remove, e);
                                }
                            }

                            // Call Gmail API and await result to ensure it succeeds
                            let mut api_succeeded = true;
                            if let Some(gmail) = &gmail_client {
                                // If archiving from a Category, remove the category label from Gmail
                                // Otherwise use standard archive (remove INBOX)
                                let result = if current_label_id.starts_with("CATEGORY_") {
                                    gmail.remove_label_from_messages(&message_ids, &current_label_id).await
                                } else {
                                    gmail.archive_messages(&message_ids).await
                                };
                                
                                match result {
                                    Ok(_) => {
                                        ui_state.status_message = Some("Archived successfully".to_string());
                                    }
                                    Err(e) => {
                                        eprintln!("Error archiving messages: {}", e);
                                        ui_state.status_message = Some(format!("Archive failed: {}", e));
                                        api_succeeded = false;
                                        // Restore label since API failed
                                        for id in &message_ids {
                                            if let Err(e) = db.add_label_to_message(id, &label_to_remove).await {
                                                eprintln!("Error restoring {} label: {}", label_to_remove, e);
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
                                    label_id: label_to_remove.clone(),
                                    original_index,
                                });

                                ui_state.messages.remove(ui_state.selected_message_index);
                                if ui_state.selected_message_index >= ui_state.messages.len()
                                    && !ui_state.messages.is_empty()
                                {
                                    ui_state.selected_message_index = ui_state.messages.len() - 1;
                                }

                                // Refresh detail view
                                if let Some(msg) =
                                    ui_state.messages.get(ui_state.selected_message_index)
                                {
                                    ui_state.threaded_messages =
                                        db.get_messages_by_thread(&msg.thread_id).await?;
                                } else {
                                    ui_state.threaded_messages.clear();
                                }
                            }
                        }
                    } else if matches_key(key, &config.keybindings.undo) {
                        // Undo - only in Messages or Details panel
                        if matches!(
                            ui_state.focused_panel,
                            FocusedPanel::Messages | FocusedPanel::Details
                        ) {
                            if let Some(action) = ui_state.undo_stack.pop() {
                                let description = action.description();
                                match action {
                                    UndoableAction::Delete { messages, label_id, original_index } => {
                                        // Get the representative message (first one) for UI insertion
                                        let representative = messages.first().cloned().unwrap_or_default();
                                        
                                        // Re-insert into UI at original position (clamped to list size)
                                        let insert_index = original_index.min(ui_state.messages.len());
                                        ui_state.messages.insert(insert_index, representative.clone());
                                        ui_state.selected_message_index = insert_index;

                                        // Re-insert all messages into database
                                        let _ = db.upsert_messages(&messages, &label_id).await;

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
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&representative.thread_id).await?;
                                    }
                                    UndoableAction::Archive { messages, label_id, original_index } => {
                                        // Get the representative message (first one) for UI insertion
                                        let representative = messages.first().cloned().unwrap_or_default();

                                        // Re-insert into UI at original position (only if viewing the same label)
                                        let current_label = ui_state
                                            .labels
                                            .get(ui_state.selected_label_index)
                                            .map(|l| l.id.as_str());
                                        if current_label == Some(&label_id) {
                                            let insert_index = original_index.min(ui_state.messages.len());
                                            ui_state.messages.insert(insert_index, representative.clone());
                                            ui_state.selected_message_index = insert_index;
                                        }

                                        // Re-add the removed label in database for all messages
                                        for message in &messages {
                                            let _ = db.add_label_to_message(&message.id, &label_id).await;
                                        }

                                        // Restore label via Gmail API
                                        if let Some(gmail) = &gmail_client {
                                            let gmail = gmail.clone();
                                            let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                                            let label_to_restore = label_id.clone();
                                            tokio::spawn(async move {
                                                if label_to_restore == "INBOX" {
                                                    // Use unarchive for INBOX
                                                    for id in ids {
                                                        let _ = gmail.unarchive_message(&id).await;
                                                    }
                                                } else {
                                                    // Use add_label_to_message for other labels (like categories)
                                                    for id in ids {
                                                        let _ = gmail.add_label_to_message(&id, &label_to_restore).await;
                                                    }
                                                }
                                            });
                                        }

                                        // Refresh detail view if message was re-added
                                        if current_label == Some(&label_id) {
                                            ui_state.threaded_messages =
                                                db.get_messages_by_thread(&representative.thread_id).await?;
                                        }
                                    }
                                }
                                ui_state.status_message = Some(format!("Undone: {}", description));
                            }
                        }
                    }
                }
                ui::UIMode::Composing => match key.code {
                    KeyCode::Esc => {
                        ui_state.mode = ui::UIMode::Browsing;
                        let _ = execute!(io::stdout(), crossterm::cursor::Hide);
                        ui_state.compose_state = None;
                    }
                    _ if matches_key(key, &config.keybindings.send_message) => {
                        if let Some(cs) = &ui_state.compose_state {
                            if let Some(gmail) = &gmail_client {
                                let (to, cc, bcc, sub, body) = (
                                    cs.get_to(),
                                    cs.get_cc(),
                                    cs.get_bcc(),
                                    cs.get_subject(),
                                    cs.get_body(),
                                );
                                let gmail = gmail.clone();
                                let db_url_str = db_url.clone();
                                let refresh_tx_clone = refresh_tx.clone();
                                tokio::spawn(async move {
                                    // Send the message and get its ID
                                    if let Ok(Some(msg_id)) = gmail.send_message(&to, &cc, &bcc, &sub, &body).await {
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
                        }
                        ui_state.mode = ui::UIMode::Browsing;
                        let _ = execute!(io::stdout(), crossterm::cursor::Hide);
                        ui_state.compose_state = None;
                    }
                    KeyCode::Char('b')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
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
                        if let Some(cs) = &mut ui_state.compose_state {
                            match cs.focused_field {
                                ui::ComposeField::Body => {
                                    // Let TextArea handle Enter in body
                                    cs.focused_textarea().input(key);
                                }
                                _ => {
                                    // Move to next field on Enter in other fields
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
                                        _ => ui::ComposeField::Body,
                                    };
                                }
                            }
                        }
                    }
                    _ => {
                        // Let TextArea handle all other input (chars, backspace, arrows, Ctrl+arrows, etc.)
                        if let Some(cs) = &mut ui_state.compose_state {
                            cs.focused_textarea().input(key);
                        }
                    }
                },
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
