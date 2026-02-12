mod auth;
mod config;
mod db;
mod gmail;
mod models;
mod sync;
mod ui;

use crate::config::{Config, matches_key};
use crate::gmail::GmailClient;
use crate::ui::FocusedPanel;
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
                let mut priority_rx = priority_rx.take().unwrap();
                tokio::spawn(async move {
                    if let Ok(sync_db) = db::Database::new(&sync_db_url).await {
                        loop {
                            let mut has_new_data = false;
                            if let Ok(l) = sync_client.list_labels().await {
                                let _ = sync_db.upsert_labels(&l).await;
                                has_new_data = true;

                                // Build label list, with priority label first
                                let mut label_ids: Vec<String> =
                                    l.iter().map(|label| label.id.clone()).collect();

                                // Drain priority channel and move priority label to front
                                let mut priority_label = None;
                                while let Ok(p) = priority_rx.try_recv() {
                                    priority_label = Some(p);
                                }
                                if let Some(ref priority) = priority_label {
                                    if let Some(pos) =
                                        label_ids.iter().position(|id| id == priority)
                                    {
                                        let p = label_ids.remove(pos);
                                        label_ids.insert(0, p);
                                    }
                                }

                                for label_id in &label_ids {
                                    // Update currently_syncing state
                                    if let Ok(mut state) = sync_state_clone.lock() {
                                        state.currently_syncing = Some(label_id.clone());
                                    }

                                    if let Ok((ids, next_page_token)) = sync_client
                                        .list_messages(vec![label_id.to_string()], 100, None)
                                        .await
                                    {
                                        let mut messages = Vec::new();
                                        let mut remote_ids = std::collections::HashSet::new();
                                        let mut oldest_date = i64::MAX;

                                        for id in &ids {
                                            remote_ids.insert(id.clone());
                                            if let Ok(exists) = sync_db.message_exists(id).await {
                                                if !exists {
                                                    if let Ok(msg) =
                                                        sync_client.get_message(id).await
                                                    {
                                                        oldest_date =
                                                            oldest_date.min(msg.internal_date);
                                                        messages.push(msg);
                                                    }
                                                } else {
                                                    if let Ok(Some(date)) =
                                                        sync_db.get_message_date(id).await
                                                    {
                                                        oldest_date = oldest_date.min(date);
                                                    }
                                                }
                                            }
                                        }

                                        // Only perform removal if we have the COMPLETE picture from Gmail
                                        // (no next page token means we got all results) AND we actually got results.
                                        // If there's a next_page_token, we only have a partial view
                                        // and MUST NOT remove anything — doing so would incorrectly
                                        // strip labels from messages outside the partial window.
                                        let should_remove =
                                            next_page_token.is_none() && !ids.is_empty();

                                        sync_client.debug_log(&format!(
                                        "SYNC {}: {} remote IDs, next_page={}, oldest_date={}, should_remove={}",
                                        label_id, ids.len(), next_page_token.is_some(), oldest_date, should_remove
                                    ));

                                        let _ = sync_db.upsert_messages(&messages, label_id).await;
                                        if !messages.is_empty() {
                                            has_new_data = true;
                                        }

                                        // Detection of removals (archived/deleted from other clients)
                                        // Only do this if we have the complete remote picture
                                        if should_remove {
                                            if let Ok(local_info) = sync_db
                                                .get_messages_with_dates_by_label(label_id, 200)
                                                .await
                                            {
                                                for (local_id, local_date) in local_info {
                                                    // Only remove if the message is within the date range
                                                    // of what the remote returned (i.e. it SHOULD have been
                                                    // in the remote set if it still had this label)
                                                    if local_date >= oldest_date
                                                        && !remote_ids.contains(&local_id)
                                                    {
                                                        if let Ok(_) = sync_db
                                                            .remove_label_from_message(
                                                                &local_id, label_id,
                                                            )
                                                            .await
                                                        {
                                                            has_new_data = true;
                                                            sync_client.debug_log(&format!(
                                                            "REMOVAL: Confirmed {} missing from {} (oldest_date: {})", 
                                                            local_id, label_id, oldest_date
                                                        ));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Mark this label as synced and send refresh
                                    if let Ok(mut state) = sync_state_clone.lock() {
                                        state.synced_labels.insert(label_id.clone());
                                        state.currently_syncing = None;
                                    }
                                    if has_new_data {
                                        let _ = sync_refresh_tx.send(()).await;
                                        has_new_data = false;
                                    }
                                }
                            }

                            if has_new_data {
                                let _ = sync_refresh_tx.send(()).await;
                            }

                            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                        }
                    }
                });

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
                        ui_state.threaded_messages =
                            db.get_messages_by_thread(&msg.thread_id).await?;
                    }
                } else {
                    ui_state.selected_message_index = 0;
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
                                    ui_state.selected_message_index += 1;
                                    ui_state.detail_scroll = 0;
                                    if let Some(msg) =
                                        ui_state.messages.get(ui_state.selected_message_index)
                                    {
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&msg.thread_id).await?;
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
                            ui_state.compose_state = Some(ui::ComposeState {
                                to: m.from_address.clone().unwrap_or_default(),
                                cc: "".to_string(),
                                bcc: "".to_string(),
                                subject: new_subject,
                                body: final_body,
                                focused_field: ui::ComposeField::Body,
                                cursor_index: 0,
                                show_cc_bcc: false,
                            });
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

                        ui_state.compose_state = Some(ui::ComposeState {
                            to: "".to_string(),
                            cc: "".to_string(),
                            bcc: "".to_string(),
                            subject: "".to_string(),
                            body,
                            focused_field: ui::ComposeField::To,
                            cursor_index: 0,
                            show_cc_bcc: false,
                        });
                    } else if matches_key(key, &config.keybindings.delete) {
                        // Delete
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            let id = m.id.clone();
                            if let Some(gmail) = &gmail_client {
                                let gmail = gmail.clone();
                                let db_url_str = db_url.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = gmail.trash_message(&id).await {
                                        eprintln!("Error trashing message: {}", e);
                                    }
                                    if let Ok(db_clone) = db::Database::new(&db_url_str).await {
                                        let _ = db_clone.delete_message(&id).await;
                                    }
                                });
                            }
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
                    } else if matches_key(key, &config.keybindings.archive) {
                        // Archive
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            let id = m.id.clone();
                            if let Some(gmail) = &gmail_client {
                                let gmail = gmail.clone();
                                let db_url_str = db_url.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = gmail.archive_message(&id).await {
                                        eprintln!("Error archiving message: {}", e);
                                    }
                                    if let Ok(db_clone) = db::Database::new(&db_url_str).await {
                                        let _ =
                                            db_clone.remove_label_from_message(&id, "INBOX").await;
                                    }
                                });
                            }
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
                                    cs.to.clone(),
                                    cs.cc.clone(),
                                    cs.bcc.clone(),
                                    cs.subject.clone(),
                                    cs.body.clone(),
                                );
                                let gmail = gmail.clone();
                                tokio::spawn(async move {
                                    let _ = gmail.send_message(&to, &cc, &bcc, &sub, &body).await;
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
                            cs.cursor_index = 0;
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
                            cs.cursor_index = 0;
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            let field = match cs.focused_field {
                                ui::ComposeField::To => &mut cs.to,
                                ui::ComposeField::Cc => &mut cs.cc,
                                ui::ComposeField::Bcc => &mut cs.bcc,
                                ui::ComposeField::Subject => &mut cs.subject,
                                ui::ComposeField::Body => &mut cs.body,
                            };
                            if cs.cursor_index <= field.len() {
                                field.insert(cs.cursor_index, c);
                                cs.cursor_index += 1;
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            let field = match cs.focused_field {
                                ui::ComposeField::To => &mut cs.to,
                                ui::ComposeField::Cc => &mut cs.cc,
                                ui::ComposeField::Bcc => &mut cs.bcc,
                                ui::ComposeField::Subject => &mut cs.subject,
                                ui::ComposeField::Body => &mut cs.body,
                            };
                            if cs.cursor_index > 0 {
                                field.remove(cs.cursor_index - 1);
                                cs.cursor_index -= 1;
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            match cs.focused_field {
                                ui::ComposeField::Body => {
                                    cs.body.insert(cs.cursor_index, '\n');
                                    cs.cursor_index += 1;
                                }
                                _ => {
                                    // Tab logic
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
                                    cs.cursor_index = 0;
                                }
                            }
                        }
                    }
                    KeyCode::Left => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            if cs.cursor_index > 0 {
                                cs.cursor_index -= 1;
                            }
                        }
                    }
                    KeyCode::Right => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            let field_len = match cs.focused_field {
                                ui::ComposeField::To => cs.to.len(),
                                ui::ComposeField::Cc => cs.cc.len(),
                                ui::ComposeField::Bcc => cs.bcc.len(),
                                ui::ComposeField::Subject => cs.subject.len(),
                                ui::ComposeField::Body => cs.body.len(),
                            };
                            if cs.cursor_index < field_len {
                                cs.cursor_index += 1;
                            }
                        }
                    }
                    _ => {}
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
