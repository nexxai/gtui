use crate::models::Message;

/// Represents an action that can be undone
#[derive(Debug, Clone)]
pub enum UndoableAction {
    /// Messages were deleted (moved to trash) - stores all messages in the thread
    Delete {
        messages: Vec<Message>,
        label_id: String,
        original_index: usize,
    },
    /// Messages were archived (label removed) - stores all messages in the thread
    Archive {
        messages: Vec<Message>,
        label_id: String,
        original_index: usize,
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
