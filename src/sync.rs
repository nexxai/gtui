use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct SyncState {
    pub synced_labels: HashSet<String>,
    pub currently_syncing: Option<String>,
}
