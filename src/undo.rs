use std::fmt;

use crate::models::Message;

/// Represents an action that can be undone.
#[derive(Debug, Clone)]
pub enum UndoableAction {
    /// Messages were deleted (moved to trash) - stores all messages in the thread.
    Delete {
        messages: Vec<Message>,
        label_id: String,
        original_index: usize,
    },
    /// Messages were archived (label removed) - stores all messages in the thread.
    Archive {
        messages: Vec<Message>,
        label_ids: Vec<String>,
        original_index: usize,
    },
}

impl fmt::Display for UndoableAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delete { .. } => write!(f, "delete"),
            Self::Archive { .. } => write!(f, "archive"),
        }
    }
}
