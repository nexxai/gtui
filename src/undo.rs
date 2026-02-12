use crate::models::Message;

/// Represents an action that can be undone
#[derive(Debug, Clone)]
pub enum UndoableAction {
    /// Message was deleted (moved to trash)
    Delete {
        message: Message,
        label_id: String,
        original_index: usize,
    },
    /// Message was archived (INBOX label removed)
    Archive {
        message: Message,
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
