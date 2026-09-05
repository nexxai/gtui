use crate::config::Config;
use crate::db;
use crate::gmail::GmailClient;
use crate::models::{Label, Message};
use anyhow::Result;
use async_trait::async_trait;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCompletion {
    Success,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub completion: SyncCompletion,
    pub changed: bool,
    pub processed_label_count: usize,
    pub processed_message_count: usize,
    pub error: Option<String>,
    synced_label_ids: Vec<String>,
}

impl SyncReport {
    fn success() -> Self {
        Self {
            completion: SyncCompletion::Success,
            changed: false,
            processed_label_count: 0,
            processed_message_count: 0,
            error: None,
            synced_label_ids: Vec::new(),
        }
    }

    fn failed(mut self, error: &str) -> Self {
        self.completion = SyncCompletion::Failed;
        self.error = Some(error.to_string());
        self
    }
}

#[async_trait]
pub(crate) trait MailSource: Sync {
    async fn list_labels(&self) -> Result<Vec<Label>>;
    async fn list_messages(
        &self,
        label_ids: &[String],
        max_results: u32,
        page_token: Option<String>,
    ) -> Result<(Vec<String>, Option<String>)>;
    async fn get_message(&self, id: &str) -> Result<Message>;
}

#[async_trait]
pub(crate) trait SyncStore: Sync {
    async fn upsert_labels(&self, labels: &[Label]) -> Result<()>;
    async fn upsert_messages(&self, messages: &[Message], label_id: &str) -> Result<()>;
    async fn message_exists(&self, id: &str) -> Result<bool>;
    async fn get_message_date(&self, id: &str) -> Result<Option<i64>>;
    async fn get_messages_with_dates_by_label(
        &self,
        label_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>>;
    async fn remove_label_from_message(&self, message_id: &str, label_id: &str) -> Result<()>;
}

async fn run_sync_cycle<M, S, F>(
    sync_client: &M,
    sync_db: &S,
    priority_label: Option<&str>,
    recently_modified: &HashSet<String>,
    mut label_started: F,
) -> SyncReport
where
    M: MailSource + ?Sized,
    S: SyncStore + ?Sized,
    F: FnMut(&str),
{
    let mut report = SyncReport::success();
    let labels = match sync_client.list_labels().await {
        Ok(labels) => labels,
        Err(_) => return report.failed("failed to list Gmail labels"),
    };

    if sync_db.upsert_labels(&labels).await.is_err() {
        return report.failed("failed to store Gmail labels");
    }
    report.changed = !labels.is_empty();

    let mut label_ids: Vec<String> = labels.into_iter().map(|label| label.id).collect();

    if let Some(priority) = priority_label
        && let Some(pos) = label_ids.iter().position(|id| id == priority)
    {
        let priority = label_ids.remove(pos);
        label_ids.insert(0, priority);
    }

    for label_id in label_ids {
        label_started(&label_id);

        let (ids, next_page_token) = match sync_client
            .list_messages(std::slice::from_ref(&label_id), 100, None)
            .await
        {
            Ok(messages) => messages,
            Err(_) => return report.failed("failed to list Gmail messages"),
        };

        let mut messages = Vec::new();
        let mut remote_ids = HashSet::new();
        let mut oldest_date = i64::MAX;

        for id in &ids {
            if recently_modified.contains(id) {
                debug!(id, "skipping recently modified message");
                continue;
            }

            remote_ids.insert(id.clone());

            let exists = match sync_db.message_exists(id).await {
                Ok(exists) => exists,
                Err(_) => return report.failed("failed to inspect stored message"),
            };

            if !exists {
                let message = match sync_client.get_message(id).await {
                    Ok(message) => message,
                    Err(_) => return report.failed("failed to get Gmail message"),
                };
                oldest_date = oldest_date.min(message.internal_date);
                messages.push(message);
            } else {
                match sync_db.get_message_date(id).await {
                    Ok(Some(date)) => oldest_date = oldest_date.min(date),
                    Ok(None) => {}
                    Err(_) => return report.failed("failed to inspect stored message date"),
                }
            }

            report.processed_message_count += 1;
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

        if sync_db.upsert_messages(&messages, &label_id).await.is_err() {
            return report.failed("failed to store Gmail messages");
        }
        if !messages.is_empty() {
            report.changed = true;
        }

        // Detect removals (archived/deleted from other clients)
        if should_remove {
            let local_info = match sync_db
                .get_messages_with_dates_by_label(&label_id, 200)
                .await
            {
                Ok(local_info) => local_info,
                Err(_) => return report.failed("failed to list stored messages"),
            };

            for (local_id, local_date) in local_info {
                if recently_modified.contains(&local_id) {
                    continue;
                }

                if local_date >= oldest_date && !remote_ids.contains(&local_id) {
                    if sync_db
                        .remove_label_from_message(&local_id, &label_id)
                        .await
                        .is_err()
                    {
                        return report.failed("failed to update stored message labels");
                    }
                    report.changed = true;
                    debug!(
                        local_id,
                        label_id, oldest_date, "confirmed removal from label"
                    );
                }
            }
        }

        report.processed_label_count += 1;
        report.synced_label_ids.push(label_id);
    }

    report
}

pub fn spawn_sync_task(
    sync_client: GmailClient,
    sync_db: db::Database,
    refresh_tx: mpsc::Sender<()>,
    sync_state: Arc<Mutex<SyncState>>,
    mut priority_rx: mpsc::Receiver<String>,
) {
    tokio::spawn(async move {
        let sync_interval_seconds = Config::load().sync_interval_seconds;

        loop {
            let priority_label = drain_priority_label(&mut priority_rx);
            let recently_modified = sync_state
                .lock()
                .map(|mut state| {
                    state.cleanup_expired();
                    state
                        .recently_modified
                        .keys()
                        .filter(|id| state.is_recently_modified(id))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            let report = run_sync_cycle(
                &sync_client,
                &sync_db,
                priority_label.as_deref(),
                &recently_modified,
                |label_id| {
                    if let Ok(mut state) = sync_state.lock() {
                        state.currently_syncing = Some(label_id.to_string());
                    }
                },
            )
            .await;

            if let Ok(mut state) = sync_state.lock() {
                state.synced_labels.extend(report.synced_label_ids.clone());
                state.currently_syncing = None;
            }

            if report.changed {
                let _ = refresh_tx.send(()).await;
            }

            tokio::time::sleep(Duration::from_secs(sync_interval_seconds)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::collections::VecDeque;

    type MessageListResult = Result<(Vec<String>, Option<String>)>;
    type LocalMessagesResult = Result<Vec<(String, i64)>>;

    #[derive(Default)]
    struct ScriptedMailSource {
        label_results: Mutex<VecDeque<Result<Vec<Label>>>>,
        message_list_results: Mutex<VecDeque<MessageListResult>>,
        message_results: Mutex<VecDeque<Result<Message>>>,
        calls: Mutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl MailSource for ScriptedMailSource {
        async fn list_labels(&self) -> Result<Vec<Label>> {
            self.calls.lock().unwrap().push("list_labels");
            next_result(&self.label_results, "list_labels")
        }

        async fn list_messages(
            &self,
            _label_ids: &[String],
            _max_results: u32,
            _page_token: Option<String>,
        ) -> Result<(Vec<String>, Option<String>)> {
            self.calls.lock().unwrap().push("list_messages");
            next_result(&self.message_list_results, "list_messages")
        }

        async fn get_message(&self, _id: &str) -> Result<Message> {
            self.calls.lock().unwrap().push("get_message");
            next_result(&self.message_results, "get_message")
        }
    }

    #[derive(Default)]
    struct ScriptedSyncStore {
        label_write_results: Mutex<VecDeque<Result<()>>>,
        message_write_results: Mutex<VecDeque<Result<()>>>,
        message_exists_results: Mutex<VecDeque<Result<bool>>>,
        message_date_results: Mutex<VecDeque<Result<Option<i64>>>>,
        local_message_results: Mutex<VecDeque<LocalMessagesResult>>,
        remove_label_results: Mutex<VecDeque<Result<()>>>,
        calls: Mutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl SyncStore for ScriptedSyncStore {
        async fn upsert_labels(&self, _labels: &[Label]) -> Result<()> {
            self.calls.lock().unwrap().push("upsert_labels");
            next_result(&self.label_write_results, "upsert_labels")
        }

        async fn upsert_messages(&self, _messages: &[Message], _label_id: &str) -> Result<()> {
            self.calls.lock().unwrap().push("upsert_messages");
            next_result(&self.message_write_results, "upsert_messages")
        }

        async fn message_exists(&self, _id: &str) -> Result<bool> {
            self.calls.lock().unwrap().push("message_exists");
            next_result(&self.message_exists_results, "message_exists")
        }

        async fn get_message_date(&self, _id: &str) -> Result<Option<i64>> {
            self.calls.lock().unwrap().push("get_message_date");
            next_result(&self.message_date_results, "get_message_date")
        }

        async fn get_messages_with_dates_by_label(
            &self,
            _label_id: &str,
            _limit: i64,
        ) -> Result<Vec<(String, i64)>> {
            self.calls
                .lock()
                .unwrap()
                .push("get_messages_with_dates_by_label");
            next_result(
                &self.local_message_results,
                "get_messages_with_dates_by_label",
            )
        }

        async fn remove_label_from_message(
            &self,
            _message_id: &str,
            _label_id: &str,
        ) -> Result<()> {
            self.calls.lock().unwrap().push("remove_label_from_message");
            next_result(&self.remove_label_results, "remove_label_from_message")
        }
    }

    fn next_result<T>(queue: &Mutex<VecDeque<Result<T>>>, operation: &str) -> Result<T> {
        queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("missing scripted result for {operation}"))
    }

    fn label(id: &str) -> Label {
        Label {
            id: id.to_string(),
            name: id.to_string(),
            label_type: "system".to_string(),
            color_foreground: None,
            color_background: None,
            display_name: id.to_string(),
        }
    }

    fn message(id: &str, internal_date: i64) -> Message {
        Message {
            id: id.to_string(),
            thread_id: format!("thread-{id}"),
            internal_date,
            ..Message::default()
        }
    }

    #[tokio::test]
    async fn cycle_success_reports_changes_and_processed_counts() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("INBOX")])])),
            message_list_results: Mutex::new(VecDeque::from([Ok((
                vec!["message-1".to_string(), "message-2".to_string()],
                None,
            ))])),
            message_results: Mutex::new(VecDeque::from([
                Ok(message("message-1", 10)),
                Ok(message("message-2", 20)),
            ])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_exists_results: Mutex::new(VecDeque::from([Ok(false), Ok(false)])),
            local_message_results: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            ..ScriptedSyncStore::default()
        };

        let report = run_sync_cycle(&source, &store, None, &HashSet::new(), |_| {}).await;

        assert_eq!(report.completion, SyncCompletion::Success);
        assert!(report.changed);
        assert_eq!(report.processed_label_count, 1);
        assert_eq!(report.processed_message_count, 2);
        assert_eq!(report.error, None);
        assert_eq!(report.synced_label_ids, ["INBOX"]);
        assert_eq!(
            *source.calls.lock().unwrap(),
            ["list_labels", "list_messages", "get_message", "get_message"]
        );
    }

    #[tokio::test]
    async fn cycle_message_list_error_fails_without_syncing_label() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("INBOX")])])),
            message_list_results: Mutex::new(VecDeque::from([Err(anyhow!("source detail"))])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            ..ScriptedSyncStore::default()
        };

        let report = run_sync_cycle(&source, &store, None, &HashSet::new(), |_| {}).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(report.processed_label_count, 0);
        assert!(report.synced_label_ids.is_empty());
        assert_eq!(
            report.error.as_deref(),
            Some("failed to list Gmail messages")
        );
        assert_eq!(*store.calls.lock().unwrap(), ["upsert_labels"]);
    }

    #[tokio::test]
    async fn cycle_store_write_error_returns_failed_report() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("INBOX")])])),
            message_list_results: Mutex::new(VecDeque::from([Ok((Vec::new(), None))])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Err(anyhow!("store detail"))])),
            ..ScriptedSyncStore::default()
        };

        let report = run_sync_cycle(&source, &store, None, &HashSet::new(), |_| {}).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(report.processed_label_count, 0);
        assert!(report.synced_label_ids.is_empty());
        assert_eq!(
            report.error.as_deref(),
            Some("failed to store Gmail messages")
        );
    }

    #[tokio::test]
    async fn cycle_empty_source_reports_unchanged_success() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            ..ScriptedSyncStore::default()
        };

        let report = run_sync_cycle(&source, &store, None, &HashSet::new(), |_| {}).await;

        assert_eq!(report.completion, SyncCompletion::Success);
        assert!(!report.changed);
        assert_eq!(report.processed_label_count, 0);
        assert_eq!(report.processed_message_count, 0);
        assert_eq!(*source.calls.lock().unwrap(), ["list_labels"]);
        assert_eq!(*store.calls.lock().unwrap(), ["upsert_labels"]);
    }
}
