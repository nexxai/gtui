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
}

impl SyncReport {
    fn success() -> Self {
        Self {
            completion: SyncCompletion::Success,
            changed: false,
            processed_label_count: 0,
            processed_message_count: 0,
            error: None,
        }
    }

    fn fail(&mut self, error: &str) {
        self.completion = SyncCompletion::Failed;
        if self.error.is_none() {
            self.error = Some(error.to_string());
        }
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

trait SyncObserver {
    fn take_priority_label(&mut self) -> Option<String>;
    fn label_started(&mut self, label_id: &str);
    fn recently_modified_ids(&mut self, message_ids: &[String]) -> HashSet<String>;
    fn label_finished(&mut self, label_id: &str, completion: SyncCompletion, changed: bool);
}

struct SchedulerObserver<'a> {
    refresh_tx: &'a mpsc::Sender<()>,
    sync_state: &'a Arc<Mutex<SyncState>>,
    priority_rx: &'a mut mpsc::Receiver<String>,
}

impl SyncObserver for SchedulerObserver<'_> {
    fn take_priority_label(&mut self) -> Option<String> {
        drain_priority_label(self.priority_rx)
    }

    fn label_started(&mut self, label_id: &str) {
        if let Ok(mut state) = self.sync_state.lock() {
            state.currently_syncing = Some(label_id.to_string());
        }
    }

    fn recently_modified_ids(&mut self, message_ids: &[String]) -> HashSet<String> {
        self.sync_state
            .lock()
            .map(|mut state| {
                state.cleanup_expired();
                message_ids
                    .iter()
                    .filter(|id| state.is_recently_modified(id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn label_finished(&mut self, label_id: &str, completion: SyncCompletion, changed: bool) {
        if let Ok(mut state) = self.sync_state.lock() {
            if completion == SyncCompletion::Success {
                state.synced_labels.insert(label_id.to_string());
            }
            state.currently_syncing = None;
        }

        if changed {
            let _ = self.refresh_tx.try_send(());
        }
    }
}

async fn run_sync_cycle<M, S, O>(sync_client: &M, sync_db: &S, observer: &mut O) -> SyncReport
where
    M: MailSource + ?Sized,
    S: SyncStore + ?Sized,
    O: SyncObserver + ?Sized,
{
    let mut report = SyncReport::success();
    let labels = match sync_client.list_labels().await {
        Ok(labels) => labels,
        Err(_) => {
            report.fail("failed to list Gmail labels");
            return report;
        }
    };

    if sync_db.upsert_labels(&labels).await.is_err() {
        report.fail("failed to store Gmail labels");
        return report;
    }
    report.changed = !labels.is_empty();

    let mut label_ids: Vec<String> = labels.into_iter().map(|label| label.id).collect();

    if let Some(priority) = observer.take_priority_label()
        && let Some(pos) = label_ids.iter().position(|id| id == &priority)
    {
        let priority = label_ids.remove(pos);
        label_ids.insert(0, priority);
    }

    let mut catalog_changed = report.changed;

    for label_id in label_ids {
        observer.label_started(&label_id);
        let mut label_completion = SyncCompletion::Success;
        let mut label_changed = std::mem::take(&mut catalog_changed);

        let (ids, next_page_token) = match sync_client
            .list_messages(std::slice::from_ref(&label_id), 100, None)
            .await
        {
            Ok(messages) => messages,
            Err(_) => {
                report.fail("failed to list Gmail messages");
                observer.label_finished(&label_id, SyncCompletion::Failed, label_changed);
                continue;
            }
        };

        let mut messages = Vec::new();
        let mut remote_ids = HashSet::new();
        let mut oldest_date = i64::MAX;
        let recently_modified = observer.recently_modified_ids(&ids);

        for id in &ids {
            if recently_modified.contains(id) {
                debug!(id, "skipping recently modified message");
                continue;
            }

            remote_ids.insert(id.clone());

            let exists = match sync_db.message_exists(id).await {
                Ok(exists) => exists,
                Err(_) => {
                    report.fail("failed to inspect stored message");
                    label_completion = SyncCompletion::Failed;
                    continue;
                }
            };

            if !exists {
                let message = match sync_client.get_message(id).await {
                    Ok(message) => message,
                    Err(_) => {
                        report.fail("failed to get Gmail message");
                        label_completion = SyncCompletion::Failed;
                        continue;
                    }
                };
                oldest_date = oldest_date.min(message.internal_date);
                messages.push(message);
            } else {
                match sync_db.get_message_date(id).await {
                    Ok(Some(date)) => oldest_date = oldest_date.min(date),
                    Ok(None) => {}
                    Err(_) => {
                        report.fail("failed to inspect stored message date");
                        label_completion = SyncCompletion::Failed;
                        continue;
                    }
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
            report.fail("failed to store Gmail messages");
            label_completion = SyncCompletion::Failed;
        } else if !messages.is_empty() {
            label_changed = true;
        }

        // Detect removals (archived/deleted from other clients)
        if should_remove {
            match sync_db
                .get_messages_with_dates_by_label(&label_id, 200)
                .await
            {
                Ok(local_info) => {
                    let local_ids = local_info
                        .iter()
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    let recently_modified_local = observer.recently_modified_ids(&local_ids);

                    for (local_id, local_date) in local_info {
                        if recently_modified_local.contains(&local_id) {
                            continue;
                        }

                        if local_date >= oldest_date && !remote_ids.contains(&local_id) {
                            if sync_db
                                .remove_label_from_message(&local_id, &label_id)
                                .await
                                .is_err()
                            {
                                report.fail("failed to update stored message labels");
                                label_completion = SyncCompletion::Failed;
                                continue;
                            }
                            label_changed = true;
                            debug!(
                                local_id,
                                label_id, oldest_date, "confirmed removal from label"
                            );
                        }
                    }
                }
                Err(_) => {
                    report.fail("failed to list stored messages");
                    label_completion = SyncCompletion::Failed;
                }
            }
        }

        if label_completion == SyncCompletion::Success {
            report.processed_label_count += 1;
        }
        report.changed |= label_changed;
        observer.label_finished(&label_id, label_completion, label_changed);
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
        let mut observer = SchedulerObserver {
            refresh_tx: &refresh_tx,
            sync_state: &sync_state,
            priority_rx: &mut priority_rx,
        };

        loop {
            run_sync_cycle(&sync_client, &sync_db, &mut observer).await;

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

    #[derive(Debug, PartialEq, Eq)]
    struct LabelPublication {
        label_id: String,
        completion: SyncCompletion,
        changed: bool,
    }

    #[derive(Default)]
    struct ScriptedObserver {
        priority_labels: VecDeque<String>,
        priority_checks: usize,
        recently_modified: HashSet<String>,
        mark_after_recent_checks: VecDeque<Vec<String>>,
        recent_checks: Vec<Vec<String>>,
        started_labels: Vec<String>,
        publications: Vec<LabelPublication>,
    }

    impl SyncObserver for ScriptedObserver {
        fn take_priority_label(&mut self) -> Option<String> {
            self.priority_checks += 1;
            self.priority_labels.pop_front()
        }

        fn label_started(&mut self, label_id: &str) {
            self.started_labels.push(label_id.to_string());
        }

        fn recently_modified_ids(&mut self, message_ids: &[String]) -> HashSet<String> {
            self.recent_checks.push(message_ids.to_vec());
            let recently_modified = message_ids
                .iter()
                .filter(|id| self.recently_modified.contains(*id))
                .cloned()
                .collect();

            if let Some(ids) = self.mark_after_recent_checks.pop_front() {
                self.recently_modified.extend(ids);
            }

            recently_modified
        }

        fn label_finished(&mut self, label_id: &str, completion: SyncCompletion, changed: bool) {
            self.publications.push(LabelPublication {
                label_id: label_id.to_string(),
                completion,
                changed,
            });
        }
    }

    fn publication(label_id: &str, completion: SyncCompletion, changed: bool) -> LabelPublication {
        LabelPublication {
            label_id: label_id.to_string(),
            completion,
            changed,
        }
    }

    #[derive(Default)]
    struct ScriptedMailSource {
        label_results: Mutex<VecDeque<Result<Vec<Label>>>>,
        message_list_results: Mutex<VecDeque<MessageListResult>>,
        message_results: Mutex<VecDeque<Result<Message>>>,
        listed_label_ids: Mutex<Vec<Vec<String>>>,
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
            label_ids: &[String],
            _max_results: u32,
            _page_token: Option<String>,
        ) -> Result<(Vec<String>, Option<String>)> {
            self.calls.lock().unwrap().push("list_messages");
            self.listed_label_ids
                .lock()
                .unwrap()
                .push(label_ids.to_vec());
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

        let mut observer = ScriptedObserver::default();
        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Success);
        assert!(report.changed);
        assert_eq!(report.processed_label_count, 1);
        assert_eq!(report.processed_message_count, 2);
        assert_eq!(report.error, None);
        assert_eq!(
            observer.publications,
            [publication("INBOX", SyncCompletion::Success, true)]
        );
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

        let mut observer = ScriptedObserver::default();
        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(report.processed_label_count, 0);
        assert_eq!(
            observer.publications,
            [publication("INBOX", SyncCompletion::Failed, true)]
        );
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

        let mut observer = ScriptedObserver::default();
        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(report.processed_label_count, 0);
        assert_eq!(
            observer.publications,
            [publication("INBOX", SyncCompletion::Failed, true)]
        );
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

        let mut observer = ScriptedObserver::default();
        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Success);
        assert!(!report.changed);
        assert_eq!(report.processed_label_count, 0);
        assert_eq!(report.processed_message_count, 0);
        assert!(observer.publications.is_empty());
        assert_eq!(*source.calls.lock().unwrap(), ["list_labels"]);
        assert_eq!(*store.calls.lock().unwrap(), ["upsert_labels"]);
    }

    #[tokio::test]
    async fn cycle_continues_after_first_label_failure() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("FIRST"), label("SECOND")])])),
            message_list_results: Mutex::new(VecDeque::from([
                Err(anyhow!("first label detail")),
                Ok((Vec::new(), None)),
            ])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Ok(())])),
            ..ScriptedSyncStore::default()
        };

        let mut observer = ScriptedObserver::default();
        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(report.processed_label_count, 1);
        assert_eq!(
            observer.publications,
            [
                publication("FIRST", SyncCompletion::Failed, true),
                publication("SECOND", SyncCompletion::Success, false),
            ]
        );
        assert_eq!(
            *source.calls.lock().unwrap(),
            ["list_labels", "list_messages", "list_messages"]
        );
    }

    #[tokio::test]
    async fn cycle_continues_after_message_failure_and_keeps_first_error() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("INBOX")])])),
            message_list_results: Mutex::new(VecDeque::from([Ok((
                vec!["message-1".to_string(), "message-2".to_string()],
                None,
            ))])),
            message_results: Mutex::new(VecDeque::from([
                Err(anyhow!("first message detail")),
                Ok(message("message-2", 20)),
            ])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_exists_results: Mutex::new(VecDeque::from([Ok(false), Ok(false)])),
            local_message_results: Mutex::new(VecDeque::from([Err(anyhow!("later store detail"))])),
            ..ScriptedSyncStore::default()
        };
        let mut observer = ScriptedObserver::default();

        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(report.processed_message_count, 1);
        assert_eq!(report.error.as_deref(), Some("failed to get Gmail message"));
        assert_eq!(
            *source.calls.lock().unwrap(),
            ["list_labels", "list_messages", "get_message", "get_message"]
        );
        assert_eq!(
            observer.publications,
            [publication("INBOX", SyncCompletion::Failed, true)]
        );
    }

    #[tokio::test]
    async fn cycle_scheduler_observer_publishes_completed_labels_and_refreshes() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("FIRST"), label("SECOND")])])),
            message_list_results: Mutex::new(VecDeque::from([
                Err(anyhow!("first label detail")),
                Ok((vec!["message-2".to_string()], None)),
            ])),
            message_results: Mutex::new(VecDeque::from([Ok(message("message-2", 20))])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_exists_results: Mutex::new(VecDeque::from([Ok(false)])),
            local_message_results: Mutex::new(VecDeque::from([Ok(Vec::new())])),
            ..ScriptedSyncStore::default()
        };
        let sync_state = Arc::new(Mutex::new(SyncState::default()));
        let (refresh_tx, mut refresh_rx) = mpsc::channel(4);
        let (_priority_tx, mut priority_rx) = mpsc::channel(1);
        let mut observer = SchedulerObserver {
            refresh_tx: &refresh_tx,
            sync_state: &sync_state,
            priority_rx: &mut priority_rx,
        };

        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        let state = sync_state.lock().unwrap();
        assert_eq!(state.currently_syncing, None);
        assert_eq!(state.synced_labels, HashSet::from(["SECOND".to_string()]));
        drop(state);
        assert_eq!(refresh_rx.try_recv(), Ok(()));
        assert_eq!(refresh_rx.try_recv(), Ok(()));
        assert!(refresh_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cycle_rechecks_recent_modifications_during_the_cycle() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("FIRST"), label("SECOND")])])),
            message_list_results: Mutex::new(VecDeque::from([
                Ok((vec!["message-1".to_string()], None)),
                Ok((vec!["message-2".to_string()], None)),
            ])),
            message_results: Mutex::new(VecDeque::from([Ok(message("message-1", 10))])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            message_exists_results: Mutex::new(VecDeque::from([Ok(false)])),
            local_message_results: Mutex::new(VecDeque::from([Ok(Vec::new()), Ok(Vec::new())])),
            ..ScriptedSyncStore::default()
        };
        let mut observer = ScriptedObserver {
            mark_after_recent_checks: VecDeque::from([vec!["message-2".to_string()]]),
            ..ScriptedObserver::default()
        };

        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Success);
        assert_eq!(report.processed_label_count, 2);
        assert_eq!(report.processed_message_count, 1);
        assert_eq!(
            *source.calls.lock().unwrap(),
            [
                "list_labels",
                "list_messages",
                "get_message",
                "list_messages"
            ]
        );
        assert_eq!(
            observer.recent_checks,
            [
                vec!["message-1".to_string()],
                Vec::new(),
                vec!["message-2".to_string()],
                Vec::new(),
            ]
        );
    }

    #[tokio::test]
    async fn cycle_label_list_failure_preserves_priority() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Err(anyhow!("source detail"))])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore::default();
        let mut observer = ScriptedObserver {
            priority_labels: VecDeque::from(["INBOX".to_string()]),
            ..ScriptedObserver::default()
        };

        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Failed);
        assert_eq!(observer.priority_checks, 0);
        assert_eq!(observer.priority_labels, ["INBOX"]);
        assert!(observer.started_labels.is_empty());
    }

    #[tokio::test]
    async fn cycle_applies_priority_after_label_discovery() {
        let source = ScriptedMailSource {
            label_results: Mutex::new(VecDeque::from([Ok(vec![label("FIRST"), label("SECOND")])])),
            message_list_results: Mutex::new(VecDeque::from([
                Ok((Vec::new(), None)),
                Ok((Vec::new(), None)),
            ])),
            ..ScriptedMailSource::default()
        };
        let store = ScriptedSyncStore {
            label_write_results: Mutex::new(VecDeque::from([Ok(())])),
            message_write_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            ..ScriptedSyncStore::default()
        };
        let mut observer = ScriptedObserver {
            priority_labels: VecDeque::from(["SECOND".to_string()]),
            ..ScriptedObserver::default()
        };

        let report = run_sync_cycle(&source, &store, &mut observer).await;

        assert_eq!(report.completion, SyncCompletion::Success);
        assert_eq!(observer.started_labels, ["SECOND", "FIRST"]);
        assert_eq!(
            *source.listed_label_ids.lock().unwrap(),
            [vec!["SECOND".to_string()], vec!["FIRST".to_string()]]
        );
    }
}
