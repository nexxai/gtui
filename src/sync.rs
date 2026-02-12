use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct SyncState {
    pub synced_labels: HashSet<String>,
    pub currently_syncing: Option<String>,
    /// Tracks messages that were recently modified locally (archived/deleted)
    /// Maps message_id -> timestamp when it was modified
    /// Sync should skip updating these messages for a grace period
    pub recently_modified: HashMap<String, Instant>,
}

impl SyncState {
    /// Mark multiple messages as recently modified
    pub fn mark_modified_many(&mut self, message_ids: Vec<String>) {
        let now = Instant::now();
        for id in message_ids {
            self.recently_modified.insert(id, now);
        }
    }

    /// Check if a message was recently modified (within the grace period)
    pub fn is_recently_modified(&self, message_id: &str) -> bool {
        // Extended grace period to handle Gmail's eventual consistency
        // It can take several minutes for label changes to propagate
        const GRACE_PERIOD: Duration = Duration::from_secs(300); // 5 minutes

        if let Some(&timestamp) = self.recently_modified.get(message_id) {
            Instant::now().duration_since(timestamp) < GRACE_PERIOD
        } else {
            false
        }
    }

    /// Clean up expired entries from the recently_modified map
    pub fn cleanup_expired(&mut self) {
        const GRACE_PERIOD: Duration = Duration::from_secs(300); // 5 minutes
        let now = Instant::now();

        self.recently_modified
            .retain(|_, &mut timestamp| now.duration_since(timestamp) < GRACE_PERIOD);
    }
}
