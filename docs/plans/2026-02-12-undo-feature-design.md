# Undo Feature Design

## Overview

Add the ability to undo delete and archive actions by pressing 'u' when the Messages or Details panel is focused. Users can repeatedly press 'u' to undo multiple actions. The undo history is session-only (not persisted across restarts).

## Requirements

- **Trigger**: Press 'u' key (configurable) when Messages or Details panel is focused
- **Actions supported**: Delete and Archive
- **Stack size**: Unlimited (session only)
- **Scope**: Global undo stack (works regardless of which label is currently viewed)
- **API behavior**: Immediate Gmail API reversal on undo
- **Feedback**: Status message displayed (e.g., "Undone: archive")
- **Restore behavior**: Messages restored to their original label

## Data Structures

### UndoableAction Enum (new file: src/undo.rs)

```rust
#[derive(Debug, Clone)]
pub enum UndoableAction {
    Delete {
        message: Message,
        label_id: String,  // Label it was deleted from
    },
    Archive {
        message: Message,
    },
}

impl UndoableAction {
    pub fn description(&self) -> &'static str {
        match self {
            UndoableAction::Delete { .. } => "delete",
            UndoableAction::Archive { .. } => "archive",
        }
    }
}
```

### UIState Additions (ui.rs)

```rust
pub struct UIState {
    // ... existing fields ...
    pub undo_stack: Vec<UndoableAction>,
    pub status_message: Option<String>,
}
```

## Implementation Changes

### 1. New File: src/undo.rs

- Define `UndoableAction` enum
- Add `description()` method for status messages

### 2. Gmail API (gmail.rs)

Add two new methods:

```rust
pub async fn untrash_message(&self, id: &str) -> Result<()>
pub async fn unarchive_message(&self, id: &str) -> Result<()>
```

### 3. Database (db.rs)

Add one new method:

```rust
pub async fn add_label_to_message(&self, message_id: &str, label_id: &str) -> Result<()>
```

### 4. Configuration (config.rs)

Add to `Keybindings` struct:

```rust
pub undo: Vec<String>,  // Default: ["u"]
```

### 5. UI State (ui.rs)

- Add `undo_stack: Vec<UndoableAction>` field
- Add `status_message: Option<String>` field
- Initialize both in `UIState::new()`
- Render status message in UI (bottom of screen)

### 6. Main Event Loop (main.rs)

**Capture actions before execution:**

In delete handler, before removing message:
```rust
let current_label = ui_state.labels[ui_state.selected_label_index].id.clone();
ui_state.undo_stack.push(UndoableAction::Delete {
    message: m.clone(),
    label_id: current_label,
});
```

In archive handler, before removing message:
```rust
ui_state.undo_stack.push(UndoableAction::Archive {
    message: m.clone(),
});
```

**Handle undo key:**

```rust
else if matches_key(key, &config.keybindings.undo) {
    if matches!(ui_state.focused_panel, FocusedPanel::Messages | FocusedPanel::Details) {
        if let Some(action) = ui_state.undo_stack.pop() {
            // Reverse the action (restore UI, DB, call Gmail API)
            // Set status message
        }
    }
}
```

**Clear status message:**

Clear `status_message` on any keypress or after rendering.

## File Change Summary

| File | Changes |
|------|---------|
| `src/undo.rs` | NEW - UndoableAction enum |
| `src/main.rs` | Add mod undo, capture actions, handle 'u' key |
| `src/ui.rs` | Add undo_stack and status_message to UIState, render status |
| `src/gmail.rs` | Add untrash_message() and unarchive_message() |
| `src/db.rs` | Add add_label_to_message() |
| `src/config.rs` | Add undo keybinding |

## Testing

1. Delete a message, press 'u' - message should reappear
2. Archive a message, press 'u' - message should reappear in INBOX
3. Delete multiple messages, press 'u' repeatedly - all should restore in reverse order
4. Navigate to different label, press 'u' - should still undo last action
5. Focus Labels panel, press 'u' - should do nothing
6. Verify Gmail web UI reflects the undone actions
