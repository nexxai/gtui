# Undo Feature Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add ability to undo delete and archive actions by pressing 'u' when Messages or Details panel is focused.

**Architecture:** A global undo stack stores `UndoableAction` variants (Delete/Archive) with captured message state. On 'u' keypress, pop the stack and reverse the action locally (UI + DB) and remotely (Gmail API). Display a status message to confirm the undo.

**Tech Stack:** Rust, ratatui, tokio, sqlx (SQLite), google-gmail1 API

---

## Task 1: Create undo.rs Module

**Files:**
- Create: `src/undo.rs`

**Step 1: Create the undo module with UndoableAction enum**

```rust
use crate::models::Message;

/// Represents an action that can be undone
#[derive(Debug, Clone)]
pub enum UndoableAction {
    /// Message was deleted (moved to trash)
    Delete {
        message: Message,
        label_id: String,
    },
    /// Message was archived (INBOX label removed)
    Archive {
        message: Message,
    },
}

impl UndoableAction {
    /// Returns a human-readable description for status messages
    pub fn description(&self) -> &'static str {
        match self {
            UndoableAction::Delete { .. } => "delete",
            UndoableAction::Archive { .. } => "archive",
        }
    }
}
```

**Step 2: Verify the module compiles**

Run: `cargo check`
Expected: Compiles with no errors (warnings OK)

**Step 3: Commit**

```bash
git add src/undo.rs
git commit -m "feat(undo): add UndoableAction enum"
```

---

## Task 2: Add undo Keybinding to Config

**Files:**
- Modify: `src/config.rs:17-30` (Keybindings struct)
- Modify: `src/config.rs:35-47` (Default impl)

**Step 1: Add undo field to Keybindings struct**

In `src/config.rs`, add to the `Keybindings` struct after line 29 (`quit`):

```rust
pub struct Keybindings {
    pub next_panel: Vec<String>,
    pub prev_panel: Vec<String>,
    pub move_up: Vec<String>,
    pub move_down: Vec<String>,
    pub mark_read: Vec<String>,
    pub new_message: Vec<String>,
    pub reply: Vec<String>,
    pub delete: Vec<String>,
    pub archive: Vec<String>,
    pub send_message: Vec<String>,
    pub quit: Vec<String>,
    pub undo: Vec<String>,  // ADD THIS LINE
}
```

**Step 2: Add default for undo keybinding**

In the `Default` impl for `Config`, add after line 46 (`quit`):

```rust
impl Default for Config {
    fn default() -> Self {
        Self {
            keybindings: Keybindings {
                next_panel: vec!["l".to_string(), "Right".to_string(), "Tab".to_string()],
                prev_panel: vec!["h".to_string(), "Left".to_string(), "BackTab".to_string()],
                move_up: vec!["k".to_string(), "Up".to_string()],
                move_down: vec!["j".to_string(), "Down".to_string()],
                mark_read: vec![" ".to_string()],
                new_message: vec!["n".to_string()],
                reply: vec!["r".to_string()],
                delete: vec!["Backspace".to_string(), "d".to_string()],
                archive: vec!["a".to_string()],
                send_message: vec!["ctrl-s".to_string()],
                quit: vec!["q".to_string()],
                undo: vec!["u".to_string()],  // ADD THIS LINE
            },
            signatures: Signatures::default(),
        }
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 4: Commit**

```bash
git add src/config.rs
git commit -m "feat(undo): add undo keybinding config (default: u)"
```

---

## Task 3: Add Gmail API Methods for Undo

**Files:**
- Modify: `src/gmail.rs:159-177` (after archive_message)

**Step 1: Add untrash_message method**

Add after `archive_message` method (around line 177):

```rust
    pub async fn untrash_message(&self, id: &str) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Untrashing message: {}", id));
        }
        self.hub
            .users()
            .messages_untrash("me", id)
            .doit()
            .await
            .context("Failed to untrash message")?;
        Ok(())
    }

    pub async fn unarchive_message(&self, id: &str) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Unarchiving message: {}", id));
        }
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            add_label_ids: Some(vec!["INBOX".to_string()]),
            remove_label_ids: None,
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to unarchive message")?;
        Ok(())
    }
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/gmail.rs
git commit -m "feat(undo): add untrash_message and unarchive_message API methods"
```

---

## Task 4: Add Database Method for Undo

**Files:**
- Modify: `src/db.rs:248-255` (after remove_label_from_message)

**Step 1: Add add_label_to_message method**

Add after `remove_label_from_message` method (around line 255):

```rust
    pub async fn add_label_to_message(&self, message_id: &str, label_id: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO message_labels (message_id, label_id) VALUES (?, ?)")
            .bind(message_id)
            .bind(label_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/db.rs
git commit -m "feat(undo): add add_label_to_message database method"
```

---

## Task 5: Add Undo Stack and Status Message to UIState

**Files:**
- Modify: `src/ui.rs:1-10` (imports)
- Modify: `src/ui.rs:47-61` (UIState struct)
- Modify: `src/ui.rs:63-81` (Default impl)

**Step 1: Add import for undo module**

At line 2, add the import:

```rust
use crate::models;
use crate::sync::SyncState;
use crate::undo::UndoableAction;  // ADD THIS LINE
```

**Step 2: Add fields to UIState struct**

Add after `sync_state` field (line 60):

```rust
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
    pub undo_stack: Vec<UndoableAction>,      // ADD THIS LINE
    pub status_message: Option<String>,        // ADD THIS LINE
}
```

**Step 3: Initialize new fields in Default impl**

Add after `sync_state` initialization (line 78):

```rust
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
            undo_stack: Vec::new(),           // ADD THIS LINE
            status_message: None,              // ADD THIS LINE
        }
    }
}
```

**Step 4: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 5: Commit**

```bash
git add src/ui.rs
git commit -m "feat(undo): add undo_stack and status_message to UIState"
```

---

## Task 6: Render Status Message in UI

**Files:**
- Modify: `src/ui.rs:184-194` (messages panel block)

**Step 1: Update messages panel title to include status**

Find the messages_block creation (around line 184) and modify to include status:

```rust
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
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/ui.rs
git commit -m "feat(undo): render status message in Conversations panel title"
```

---

## Task 7: Wire Up Undo Module in main.rs

**Files:**
- Modify: `src/main.rs:1-7` (module declarations)

**Step 1: Add mod undo declaration**

Add after line 6 (`mod sync;`):

```rust
mod auth;
mod config;
mod db;
mod gmail;
mod models;
mod sync;
mod undo;  // ADD THIS LINE
mod ui;
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(undo): add undo module to main.rs"
```

---

## Task 8: Capture Delete Actions to Undo Stack

**Files:**
- Modify: `src/main.rs:552-584` (delete handler)

**Step 1: Add undo import**

At line 9, add the import:

```rust
use crate::config::{Config, matches_key};
use crate::gmail::GmailClient;
use crate::ui::FocusedPanel;
use crate::undo::UndoableAction;  // ADD THIS LINE
```

**Step 2: Capture message before delete**

Find the delete handler (around line 552) and add capture before the message is removed. Replace the existing delete block:

```rust
                    } else if matches_key(key, &config.keybindings.delete) {
                        // Delete
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            // Capture for undo BEFORE removing
                            let current_label_id = ui_state
                                .labels
                                .get(ui_state.selected_label_index)
                                .map(|l| l.id.clone())
                                .unwrap_or_else(|| "INBOX".to_string());
                            ui_state.undo_stack.push(UndoableAction::Delete {
                                message: m.clone(),
                                label_id: current_label_id,
                            });

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

                            // Clear any previous status message
                            ui_state.status_message = None;
                        }
                    }
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(undo): capture delete actions to undo stack"
```

---

## Task 9: Capture Archive Actions to Undo Stack

**Files:**
- Modify: `src/main.rs:585-619` (archive handler)

**Step 1: Add capture before archive**

Find the archive handler (around line 585) and replace with:

```rust
                    } else if matches_key(key, &config.keybindings.archive) {
                        // Archive
                        if let Some(m) = ui_state.messages.get(ui_state.selected_message_index) {
                            // Capture for undo BEFORE removing
                            ui_state.undo_stack.push(UndoableAction::Archive {
                                message: m.clone(),
                            });

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

                            // Clear any previous status message
                            ui_state.status_message = None;
                        }
                    }
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(undo): capture archive actions to undo stack"
```

---

## Task 10: Implement Undo Handler

**Files:**
- Modify: `src/main.rs:619` (after archive handler, before closing brace of Browsing mode)

**Step 1: Add undo handler**

Add after the archive handler block, before the closing brace of `UIMode::Browsing`:

```rust
                    } else if matches_key(key, &config.keybindings.undo) {
                        // Undo - only in Messages or Details panel
                        if matches!(
                            ui_state.focused_panel,
                            FocusedPanel::Messages | FocusedPanel::Details
                        ) {
                            if let Some(action) = ui_state.undo_stack.pop() {
                                let description = action.description();
                                match action {
                                    UndoableAction::Delete { message, label_id } => {
                                        // Re-insert into UI at top
                                        ui_state.messages.insert(0, message.clone());
                                        ui_state.selected_message_index = 0;

                                        // Re-insert into database
                                        let _ = db.upsert_messages(&[message.clone()], &label_id).await;

                                        // Untrash via Gmail API
                                        if let Some(gmail) = &gmail_client {
                                            let gmail = gmail.clone();
                                            let id = message.id.clone();
                                            tokio::spawn(async move {
                                                let _ = gmail.untrash_message(&id).await;
                                            });
                                        }

                                        // Refresh detail view
                                        ui_state.threaded_messages =
                                            db.get_messages_by_thread(&message.thread_id).await?;
                                    }
                                    UndoableAction::Archive { message } => {
                                        // Re-insert into UI at top (only if viewing INBOX)
                                        let current_label = ui_state
                                            .labels
                                            .get(ui_state.selected_label_index)
                                            .map(|l| l.id.as_str());
                                        if current_label == Some("INBOX") {
                                            ui_state.messages.insert(0, message.clone());
                                            ui_state.selected_message_index = 0;
                                        }

                                        // Re-add INBOX label in database
                                        let _ = db.add_label_to_message(&message.id, "INBOX").await;

                                        // Unarchive via Gmail API
                                        if let Some(gmail) = &gmail_client {
                                            let gmail = gmail.clone();
                                            let id = message.id.clone();
                                            tokio::spawn(async move {
                                                let _ = gmail.unarchive_message(&id).await;
                                            });
                                        }

                                        // Refresh detail view if message was re-added
                                        if current_label == Some("INBOX") {
                                            ui_state.threaded_messages =
                                                db.get_messages_by_thread(&message.thread_id).await?;
                                        }
                                    }
                                }
                                ui_state.status_message = Some(format!("Undone: {}", description));
                            }
                        }
                    }
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(undo): implement undo handler for delete and archive"
```

---

## Task 11: Clear Status Message on Keypress

**Files:**
- Modify: `src/main.rs:331` (start of Browsing key handling)

**Step 1: Clear status on non-undo keypress**

At the start of the `UIMode::Browsing` block (around line 331), add status clearing. Find where key handling begins for Browsing mode and add at the top:

```rust
                ui::UIMode::Browsing => {
                    // Clear status message on any keypress (will be set again if undo is pressed)
                    if !matches_key(key, &config.keybindings.undo) {
                        ui_state.status_message = None;
                    }

                    if matches_key(key, &config.keybindings.quit) {
                        break;
                    }
                    // ... rest of handlers
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(undo): clear status message on non-undo keypress"
```

---

## Task 12: Final Build and Manual Test

**Step 1: Run full build**

Run: `cargo build`
Expected: Compiles successfully

**Step 2: Manual testing checklist**

Test the following scenarios:
1. Delete a message, press 'u' - message should reappear in list
2. Archive a message (from INBOX), press 'u' - message should reappear in INBOX
3. Delete multiple messages, press 'u' repeatedly - all should restore in reverse order
4. Navigate to a different label, press 'u' - should still undo last action
5. Focus Labels panel, press 'u' - should do nothing (no status message)
6. Verify status message shows "Undone: delete" or "Undone: archive" in panel title

**Step 3: Final commit**

```bash
git add -A
git commit -m "feat(undo): complete undo feature implementation"
```

---

## Summary of Changes

| File | Changes |
|------|---------|
| `src/undo.rs` | NEW - UndoableAction enum with Delete/Archive variants |
| `src/config.rs` | Add `undo` field to Keybindings, default to `["u"]` |
| `src/gmail.rs` | Add `untrash_message()` and `unarchive_message()` methods |
| `src/db.rs` | Add `add_label_to_message()` method |
| `src/ui.rs` | Add `undo_stack` and `status_message` to UIState, render status in title |
| `src/main.rs` | Add mod undo, capture actions, implement undo handler, clear status |
