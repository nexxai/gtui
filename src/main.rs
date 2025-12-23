mod auth;
mod config;
mod db;
mod gmail;
mod models;
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

    // Initial Auth setup
    let secret = auth::Authenticator::load_secret("credentials.json").await?;

    use tokio::sync::mpsc;
    let (tx, mut rx) = mpsc::channel::<String>(1);
    let (done_tx, mut done_rx) = mpsc::channel::<bool>(1);
    let (refresh_tx, mut refresh_rx) = mpsc::channel::<()>(1);

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

                // Kick off sync
                let sync_client = client.clone();
                let sync_db_url = db_url.clone();
                let sync_refresh_tx = refresh_tx.clone();
                tokio::spawn(async move {
                    if let Ok(sync_db) = db::Database::new(&sync_db_url).await {
                        loop {
                            let mut has_new_data = false;
                            if let Ok(l) = sync_client.list_labels().await {
                                let _ = sync_db.upsert_labels(&l).await;
                                has_new_data = true;
                            }

                            for label_id in &["INBOX", "SENT"] {
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
                                                if let Ok(msg) = sync_client.get_message(id).await {
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

                                    // CRITICAL FIX: Only set oldest_date to 0 if we know we have the ENTIRE folder.
                                    // If there's a next page, we only know about messages newer than the oldest one we fetched.
                                    if next_page_token.is_none() && !ids.is_empty() {
                                        oldest_date = 0;
                                    } else if ids.is_empty() {
                                        // If the folder is empty on remote, it should be empty locally too.
                                        oldest_date = 0;
                                    }

                                    let _ = sync_db.upsert_messages(&messages, label_id).await;
                                    if !messages.is_empty() {
                                        has_new_data = true;
                                    }

                                    // Detection of removals (archived/deleted from other clients)
                                    if let Ok(local_info) = sync_db
                                        .get_messages_with_dates_by_label(label_id, 200)
                                        .await
                                    {
                                        for (local_id, local_date) in local_info {
                                            // Only remove if we are sure it SHOULD be in our results window
                                            if local_date >= oldest_date
                                                && !remote_ids.contains(&local_id)
                                            {
                                                if let Ok(_) = sync_db
                                                    .remove_label_from_message(&local_id, label_id)
                                                    .await
                                                {
                                                    has_new_data = true;
                                                    sync_client.debug_log(&format!(
                                                        "REMOVAL: Confirmed {} missing from {} (older_than: {})", 
                                                        local_id, label_id, oldest_date
                                                    ));
                                                }
                                            }
                                        }
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

        // Check for sync refresh
        while let Ok(()) = refresh_rx.try_recv() {
            // Re-load labels
            ui_state.labels = db.get_labels().await?;
            if let Some(label) = ui_state.labels.get(ui_state.selected_label_index) {
                // Re-load messages for current label
                let new_messages = db
                    .get_messages_by_label(&label.id, limit, current_offset)
                    .await?;

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

        terminal.draw(|f| ui::render(f, &ui_state))?;

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
                    } else if matches_key(key.code, &config.keybindings.move_up) {
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
                    else if matches_key(key.code, &config.keybindings.mark_read) {
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
                    } else if matches_key(key.code, &config.keybindings.reply) {
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
                            if let Some(sig) = &config.signatures.reply {
                                signature_part.push_str("--\n");
                                signature_part.push_str(sig);
                                signature_part.push_str("\n\n");
                            }

                            let final_body = format!("\n\n{}{}", signature_part, quoted_body);

                            ui_state.mode = ui::UIMode::Composing;
                            let _ = execute!(io::stdout(), crossterm::cursor::Show);
                            ui_state.compose_state = Some(ui::ComposeState {
                                to: m.from_address.clone().unwrap_or_default(),
                                subject: new_subject,
                                body: final_body,
                                focused_field: ui::ComposeField::Body,
                                cursor_index: 0,
                            });
                        }
                    } else if matches_key(key.code, &config.keybindings.new_message) {
                        // New message
                        ui_state.mode = ui::UIMode::Composing;
                        let _ = execute!(io::stdout(), crossterm::cursor::Show);

                        let mut body = String::new();
                        if let Some(sig) = &config.signatures.new_message {
                            body.push_str("\n\n--\n");
                            body.push_str(sig);
                        }

                        ui_state.compose_state = Some(ui::ComposeState {
                            to: "".to_string(),
                            subject: "".to_string(),
                            body,
                            focused_field: ui::ComposeField::To,
                            cursor_index: 0,
                        });
                    } else if matches_key(key.code, &config.keybindings.delete) {
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
                        }
                    } else if matches_key(key.code, &config.keybindings.archive) {
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
                        }
                    }
                }
                ui::UIMode::Composing => match key.code {
                    KeyCode::Esc => {
                        ui_state.mode = ui::UIMode::Browsing;
                        let _ = execute!(io::stdout(), crossterm::cursor::Hide);
                        ui_state.compose_state = None;
                    }
                    KeyCode::Char('s')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        if let Some(cs) = &ui_state.compose_state {
                            if let Some(gmail) = &gmail_client {
                                let (to, sub, body) =
                                    (cs.to.clone(), cs.subject.clone(), cs.body.clone());
                                let gmail = gmail.clone();
                                tokio::spawn(async move {
                                    let _ = gmail.send_message(&to, &sub, &body).await;
                                });
                            }
                        }
                        ui_state.mode = ui::UIMode::Browsing;
                        let _ = execute!(io::stdout(), crossterm::cursor::Hide);
                        ui_state.compose_state = None;
                    }
                    KeyCode::Tab => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            cs.focused_field = match cs.focused_field {
                                ui::ComposeField::To => ui::ComposeField::Subject,
                                ui::ComposeField::Subject => ui::ComposeField::Body,
                                ui::ComposeField::Body => ui::ComposeField::To,
                            };
                            cs.cursor_index = 0; // Or keep track of separate cursors
                        }
                    }
                    KeyCode::BackTab => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            cs.focused_field = match cs.focused_field {
                                ui::ComposeField::To => ui::ComposeField::Body,
                                ui::ComposeField::Subject => ui::ComposeField::To,
                                ui::ComposeField::Body => ui::ComposeField::Subject,
                            };
                            cs.cursor_index = 0;
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(cs) = &mut ui_state.compose_state {
                            let field = match cs.focused_field {
                                ui::ComposeField::To => &mut cs.to,
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
                                    // Tab to next field on Enter for non-body
                                    cs.focused_field = match cs.focused_field {
                                        ui::ComposeField::To => ui::ComposeField::Subject,
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
