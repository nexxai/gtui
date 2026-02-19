use crate::config::Config;
use crate::db;
use crate::gmail::GmailClient;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::debug;

/// Grace period for recently-modified messages. Gmail's eventual consistency
/// means label changes can take several minutes to propagate.
const GRACE_PERIOD: Duration = Duration::from_secs(300);

#[derive(Debug, Default)]
pub struct SyncState {
    pub synced_labels: HashSet<String>,
    pub currently_syncing: Option<String>,
    /// Messages recently modified locally (archived/deleted).
    /// Sync skips these during the grace period to avoid race conditions.
    pub recently_modified: HashMap<String, Instant>,
}

impl SyncState {
    /// Mark multiple messages as recently modified.
    pub fn mark_modified_many(&mut self, message_ids: Vec<String>) {
        let now = Instant::now();
        for id in message_ids {
            self.recently_modified.insert(id, now);
        }
    }

    /// Check if a message was recently modified (within the grace period).
    pub fn is_recently_modified(&self, message_id: &str) -> bool {
        self.recently_modified
            .get(message_id)
            .is_some_and(|ts| ts.elapsed() < GRACE_PERIOD)
    }

    /// Clean up expired entries from the recently_modified map.
    pub fn cleanup_expired(&mut self) {
        self.recently_modified
            .retain(|_, ts| ts.elapsed() < GRACE_PERIOD);
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
        let Ok(sync_db) = db::Database::new(&db_url).await else {
            return;
        };

        let sync_interval_seconds = Config::load().sync_interval_seconds;

        loop {
            let mut has_new_data = false;

            if let Ok(labels) = sync_client.list_labels().await {
                let _ = sync_db.upsert_labels(&labels).await;
                has_new_data = true;

                let mut label_ids: Vec<String> =
                    labels.iter().map(|label| label.id.clone()).collect();

                // Move priority label to front if present
                if let Some(ref priority) = drain_priority_label(&mut priority_rx)
                    && let Some(pos) = label_ids.iter().position(|id| id == priority)
                {
                    let p = label_ids.remove(pos);
                    label_ids.insert(0, p);
                }

                for label_id in &label_ids {
                    // Update sync state and clean up in a single lock acquisition
                    if let Ok(mut state) = sync_state.lock() {
                        state.currently_syncing = Some(label_id.clone());
                        state.cleanup_expired();
                    }

                    if let Ok((ids, next_page_token)) = sync_client
                        .list_messages(vec![label_id.to_string()], 100, None)
                        .await
                    {
                        let mut messages = Vec::new();
                        let mut remote_ids = HashSet::new();
                        let mut oldest_date = i64::MAX;

                        // Collect recently-modified IDs in a single lock
                        let recently_modified: HashSet<String> = sync_state
                            .lock()
                            .map(|s| {
                                ids.iter()
                                    .filter(|id| s.is_recently_modified(id))
                                    .cloned()
                                    .collect()
                            })
                            .unwrap_or_default();

                        for id in &ids {
                            if recently_modified.contains(id) {
                                debug!(id, "skipping recently modified message");
                                continue;
                            }

                            remote_ids.insert(id.clone());

                            if let Ok(exists) = sync_db.message_exists(id).await {
                                if !exists {
                                    if let Ok(msg) = sync_client.get_message(id).await {
                                        oldest_date = oldest_date.min(msg.internal_date);
                                        messages.push(msg);
                                    }
                                } else if let Ok(Some(date)) = sync_db.get_message_date(id).await {
                                    oldest_date = oldest_date.min(date);
                                }
                            }
                        }

                        // Only remove if we have the COMPLETE picture from Gmail.
                        // A next_page_token means partial view -- must not remove anything.
                        let should_remove = next_page_token.is_none() && !ids.is_empty();

                        debug!(
                            label_id,
                            remote_count = ids.len(),
                            has_next_page = next_page_token.is_some(),
                            oldest_date,
                            should_remove,
                            "sync label summary"
                        );

                        let _ = sync_db.upsert_messages(&messages, label_id).await;
                        if !messages.is_empty() {
                            has_new_data = true;
                        }

                        // Detect removals (archived/deleted from other clients)
                        if should_remove
                            && let Ok(local_info) = sync_db
                                .get_messages_with_dates_by_label(label_id, 200)
                                .await
                        {
                            // Single lock to filter out recently-modified messages
                            let recently_modified_local: HashSet<String> = sync_state
                                .lock()
                                .map(|s| {
                                    local_info
                                        .iter()
                                        .filter(|(id, _)| s.is_recently_modified(id))
                                        .map(|(id, _)| id.clone())
                                        .collect()
                                })
                                .unwrap_or_default();

                            for (local_id, local_date) in local_info {
                                if recently_modified_local.contains(&local_id) {
                                    continue;
                                }

                                if local_date >= oldest_date
                                    && !remote_ids.contains(&local_id)
                                    && sync_db
                                        .remove_label_from_message(&local_id, label_id)
                                        .await
                                        .is_ok()
                                {
                                    has_new_data = true;
                                    debug!(
                                        local_id,
                                        label_id, oldest_date, "confirmed removal from label"
                                    );
                                }
                            }
                        }
                    }

                    // Mark label as synced and notify UI
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

            tokio::time::sleep(Duration::from_secs(sync_interval_seconds)).await;
        }
    });
}
