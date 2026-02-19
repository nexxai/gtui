use crate::config::Config;
use crate::db;
use crate::gmail::GmailClient;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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

fn drain_priority_label(rx: &mut mpsc::Receiver<String>) -> Option<String> {
    let mut priority_label = None;
    while let Ok(p) = rx.try_recv() {
        priority_label = Some(p);
    }
    priority_label
}

pub fn spawn_sync_task(
    sync_client: GmailClient,
    db_url: String,
    refresh_tx: mpsc::Sender<()>,
    sync_state: Arc<Mutex<SyncState>>,
    mut priority_rx: mpsc::Receiver<String>,
) {
    tokio::spawn(async move {
        if let Ok(sync_db) = db::Database::new(&db_url).await {
            let sync_interval_seconds = Config::load().sync_interval_seconds;
            loop {
                let mut has_new_data = false;
                if let Ok(l) = sync_client.list_labels().await {
                    let _ = sync_db.upsert_labels(&l).await;
                    has_new_data = true;

                    // Build label list, with priority label first
                    let mut label_ids: Vec<String> =
                        l.iter().map(|label| label.id.clone()).collect();

                    // Drain priority channel and move priority label to front
                    let priority_label = drain_priority_label(&mut priority_rx);
                    if let Some(ref priority) = priority_label
                        && let Some(pos) = label_ids.iter().position(|id| id == priority)
                    {
                        let p = label_ids.remove(pos);
                        label_ids.insert(0, p);
                    }

                    for label_id in &label_ids {
                        // Update currently_syncing state
                        if let Ok(mut state) = sync_state.lock() {
                            state.currently_syncing = Some(label_id.clone());
                        }

                        // Clean up expired entries from recently_modified
                        if let Ok(mut state) = sync_state.lock() {
                            state.cleanup_expired();
                        }

                        if let Ok((ids, next_page_token)) = sync_client
                            .list_messages(vec![label_id.to_string()], 100, None)
                            .await
                        {
                            let mut messages = Vec::new();
                            let mut remote_ids = std::collections::HashSet::new();
                            let mut oldest_date = i64::MAX;

                            for id in &ids {
                                // Skip messages that were recently modified locally
                                // to avoid race conditions with archive/delete
                                let is_recently_modified = if let Ok(state) = sync_state.lock() {
                                    state.is_recently_modified(id)
                                } else {
                                    false
                                };

                                if is_recently_modified {
                                    sync_client.debug_log(&format!(
                                        "SYNC SKIP: {} was recently modified, skipping",
                                        id
                                    ));
                                    // Don't add to remote_ids so removal detection works
                                    continue;
                                }

                                remote_ids.insert(id.clone());
                                if let Ok(exists) = sync_db.message_exists(id).await {
                                    if !exists {
                                        if let Ok(msg) = sync_client.get_message(id).await {
                                            oldest_date = oldest_date.min(msg.internal_date);
                                            messages.push(msg);
                                        }
                                    } else if let Ok(Some(date)) =
                                        sync_db.get_message_date(id).await
                                    {
                                        oldest_date = oldest_date.min(date);
                                    }
                                }
                            }

                            // Only perform removal if we have the COMPLETE picture from Gmail
                            // (no next page token means we got all results) AND we actually got results.
                            // If there's a next_page_token, we only have a partial view
                            // and MUST NOT remove anything — doing so would incorrectly
                            // strip labels from messages outside the partial window.
                            let should_remove = next_page_token.is_none() && !ids.is_empty();

                            sync_client.debug_log(&format!(
                                "SYNC {}: {} remote IDs, next_page={}, oldest_date={}, should_remove={}",
                                label_id,
                                ids.len(),
                                next_page_token.is_some(),
                                oldest_date,
                                should_remove
                            ));

                            let _ = sync_db.upsert_messages(&messages, label_id).await;
                            if !messages.is_empty() {
                                has_new_data = true;
                            }

                            // Detection of removals (archived/deleted from other clients)
                            // Only do this if we have the complete remote picture
                            if should_remove
                                && let Ok(local_info) = sync_db
                                    .get_messages_with_dates_by_label(label_id, 200)
                                    .await
                            {
                                for (local_id, local_date) in local_info {
                                    // Skip messages that were recently modified locally
                                    let is_recently_modified = if let Ok(state) = sync_state.lock()
                                    {
                                        state.is_recently_modified(&local_id)
                                    } else {
                                        false
                                    };

                                    if is_recently_modified {
                                        continue;
                                    }

                                    // Only remove if the message is within the date range
                                    // of what the remote returned (i.e. it SHOULD have been
                                    // in the remote set if it still had this label)
                                    if local_date >= oldest_date
                                        && !remote_ids.contains(&local_id)
                                        && let Ok(_) = sync_db
                                            .remove_label_from_message(&local_id, label_id)
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

                        // Mark this label as synced and send refresh
                        if let Ok(mut state) = sync_state.lock() {
                            state.synced_labels.insert(label_id.clone());
                            state.currently_syncing = None;
                        }
                        if has_new_data {
                            let _ = refresh_tx.send(()).await;
                            has_new_data = false;
                        }
                    }
                }

                if has_new_data {
                    let _ = refresh_tx.send(()).await;
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(sync_interval_seconds)).await;
            }
        }
    });
}
