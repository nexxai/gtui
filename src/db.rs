use crate::models;
use crate::sync::SyncStore;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fs2::FileExt;
use inflections::case::to_title_case;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use sqlx::ConnectOptions;
use sqlx::Connection;
use sqlx::migrate::Migrate;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
const V0_SCHEMA: &str = include_str!("../tests/fixtures/schema-v0.sql");
const LEGACY_DATABASE_NAME: &str = "gtui.db";
const APPLICATION_DIRECTORY_NAME: &str = "gtui";
const LEGACY_LEASE_NAME: &str = ".legacy-quarantine.lock";
const QUARANTINE_MARKER_NAME: &str = ".quarantine-in-progress";
const QUARANTINE_MARKER_TEMP_NAME: &str = ".quarantine-in-progress.tmp";
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
#[cfg(test)]
const OPAQUE_QUARANTINE_STAGES_PER_ARTIFACT: usize = 8;
#[cfg(test)]
const OPAQUE_DESTINATION_DURABLE_STAGE: usize = 6;

type SchemaObject = (String, String, String, Option<String>);
type LedgerRow = (i64, i64, Vec<u8>);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileFingerprint {
    length: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct QuarantineArtifact {
    basename: String,
    fingerprint: FileFingerprint,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum QuarantineKind {
    StandaloneSqlite,
    OpaqueFamily,
}

#[derive(Debug, thiserror::Error)]
#[error("SQLite cache changed while its standalone backup was staged")]
struct QuarantineSourceChanged;

#[derive(Debug, Deserialize, Serialize)]
struct QuarantineMarker {
    version: u8,
    kind: QuarantineKind,
    source_database: PathBuf,
    artifacts: Vec<QuarantineArtifact>,
}

#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
    in_memory: bool,
    _account_lease: Option<Arc<File>>,
}

#[derive(Debug)]
pub struct AccountOpen {
    pub database: Database,
    pub legacy_quarantined: bool,
}

impl Database {
    #[cfg(test)]
    pub async fn new(database_url: &str) -> Result<Self> {
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);
        let in_memory = options
            .to_url_lossy()
            .query_pairs()
            .any(|(key, value)| key == "mode" && value == "memory");

        let pool = SqlitePool::connect_with(options).await?;
        Ok(Self {
            pool,
            in_memory,
            _account_lease: None,
        })
    }

    /// The filesystem boundary protects this cache from other OS users and path substitution.
    /// Code already executing as the same OS user is outside that boundary and can alter the
    /// user's files; callers must not treat these checks as a sandbox for same-user code.
    pub async fn open_account(
        data_root: impl AsRef<Path>,
        legacy_root: impl AsRef<Path>,
        account_subject: &str,
    ) -> Result<AccountOpen> {
        validate_account_subject(account_subject)?;
        let data_root = validate_root_directory(data_root.as_ref(), "application data root")?;
        let legacy_root = validate_root_directory(legacy_root.as_ref(), "legacy cache root")?;
        let data_directory = prepare_data_directory(&data_root)?;
        validate_directory_owner(&legacy_root, &data_directory, "legacy cache root")?;
        let _legacy_lease = acquire_legacy_lease(&data_directory)?;
        recover_quarantines(&legacy_root, &data_directory).await?;

        let database_path = account_database_path(&data_root, account_subject);
        let artifacts = existing_database_artifacts(&database_path, &data_directory)?;
        let lease = Arc::new(acquire_account_lease(&database_path, &data_directory)?);

        if artifacts.is_empty() {
            create_account_database(&database_path, account_subject, lease.clone()).await?;
        } else if !artifacts.contains(&database_path) {
            quarantine_database(&database_path, &data_directory, "quarantine")
                .await
                .context("failed to quarantine incomplete account cache")?;
            bail!("account cache is missing its main database; cache quarantined");
        }

        let database = match Self::open_verified_account_database(
            &database_path,
            account_subject,
            lease.clone(),
        )
        .await
        {
            Ok(database) => database,
            Err(error) => {
                quarantine_database(&database_path, &data_directory, "quarantine")
                    .await
                    .context("failed to quarantine invalid account cache")?;
                return Err(error)
                    .context("account cache identity verification failed; cache quarantined");
            }
        };
        let legacy_quarantined = handle_legacy_database(&legacy_root, &data_directory)
            .await
            .context("failed to handle unowned legacy cache")?;

        Ok(AccountOpen {
            database,
            legacy_quarantined,
        })
    }

    async fn open_verified_account_database(
        path: &Path,
        account_subject: &str,
        lease: Arc<File>,
    ) -> Result<Self> {
        let directory = path
            .parent()
            .context("account cache path has no parent directory")?;
        existing_database_artifacts(path, directory)?;
        let before = validate_regular_file_if_present(path, "account cache", directory)?
            .context("account cache disappeared before it was opened")?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false);
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .context("failed to open account cache")?;
        validate_path_identity(path, &before, "account cache", directory)?;

        let migration_result = async {
            migrate_exclusively(&mut connection, Some(account_subject)).await?;
            let journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode = WAL")
                .fetch_one(&mut connection)
                .await
                .context("failed to enable WAL for verified account cache")?;
            if !journal_mode.eq_ignore_ascii_case("wal") {
                bail!("failed to enable WAL for verified account cache");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let close_result = connection
            .close()
            .await
            .context("failed to close verified migration connection");
        migration_result?;
        close_result?;

        Self::connect_file(path, SqliteJournalMode::Wal, lease).await
    }

    async fn connect_file(
        path: &Path,
        journal_mode: SqliteJournalMode,
        lease: Arc<File>,
    ) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .journal_mode(journal_mode);
        let pool = SqlitePool::connect_with(options)
            .await
            .context("failed to open account cache")?;

        Ok(Self {
            pool,
            in_memory: false,
            _account_lease: Some(lease),
        })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        self.run_migrations_for_account(None).await
    }

    async fn run_migrations_for_account(&self, account_subject: Option<&str>) -> Result<()> {
        let _in_memory_anchor = if self.in_memory {
            Some(
                self.pool
                    .acquire()
                    .await
                    .context("failed to retain in-memory database during migration")?,
            )
        } else {
            None
        };
        let mut connection = self
            .pool
            .acquire()
            .await
            .context("failed to acquire database migration connection")?
            .detach();
        let migration_result = migrate_exclusively(&mut connection, account_subject).await;
        let close_result = connection
            .close()
            .await
            .context("failed to close database migration connection");

        migration_result?;
        close_result
    }

    // -- Labels --

    pub async fn upsert_labels(&self, labels: &[models::Label]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for label in labels {
            sqlx::query(include_str!("../sql/upsert_labels.sql"))
                .bind(&label.id)
                .bind(&label.name)
                .bind(&label.label_type)
                .bind(&label.color_foreground)
                .bind(&label.color_background)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_labels(&self) -> Result<Vec<models::Label>> {
        let mut labels: Vec<models::Label> = sqlx::query_as(include_str!("../sql/get_labels.sql"))
            .fetch_all(&self.pool)
            .await?;

        // Derive display_name from the raw name
        for label in &mut labels {
            label.display_name = to_title_case(&label.name);
        }

        // Priority sorting: INBOX first, then alphabetical
        labels.sort_by(|a, b| match (a.id.as_str(), b.id.as_str()) {
            ("INBOX", _) => std::cmp::Ordering::Less,
            (_, "INBOX") => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });

        Ok(labels)
    }

    // -- Messages --

    pub async fn upsert_messages(
        &self,
        messages: &[models::Message],
        label_id: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for msg in messages {
            sqlx::query(include_str!("../sql/upsert_messages.sql"))
                .bind(&msg.id)
                .bind(&msg.thread_id)
                .bind(&msg.snippet)
                .bind(&msg.from_address)
                .bind(&msg.to_address)
                .bind(&msg.subject)
                .bind(msg.internal_date)
                .bind(&msg.body_plain)
                .bind(&msg.body_html)
                .bind(msg.is_read)
                .execute(&mut *tx)
                .await?;

            sqlx::query(include_str!("../sql/link_message_label.sql"))
                .bind(&msg.id)
                .bind(label_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_messages_by_label(
        &self,
        label_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<models::Message>> {
        let messages = sqlx::query_as(include_str!("../sql/get_messages_by_label.sql"))
            .bind(label_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;

        Ok(messages)
    }

    pub async fn get_messages_by_thread(&self, thread_id: &str) -> Result<Vec<models::Message>> {
        let messages = sqlx::query_as(include_str!("../sql/get_messages_by_thread.sql"))
            .bind(thread_id)
            .fetch_all(&self.pool)
            .await?;

        Ok(messages)
    }

    pub async fn get_messages_with_dates_by_label(
        &self,
        label_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query_as(include_str!("../sql/get_messages_with_dates_by_label.sql"))
            .bind(label_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows)
    }

    pub async fn mark_message_as_read(&self, id: &str, is_read: bool) -> Result<()> {
        sqlx::query(include_str!("../sql/mark_message_as_read.sql"))
            .bind(is_read)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn message_exists(&self, id: &str) -> Result<bool> {
        let row = sqlx::query(include_str!("../sql/message_exists.sql"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub async fn get_message_date(&self, id: &str) -> Result<Option<i64>> {
        let row = sqlx::query_scalar(include_str!("../sql/get_message_date.sql"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    pub async fn delete_message(&self, id: &str) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(include_str!("../sql/delete_message_labels.sql"))
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(include_str!("../sql/delete_message.sql"))
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn remove_label_from_message(&self, message_id: &str, label_id: &str) -> Result<()> {
        sqlx::query(include_str!("../sql/remove_label_from_message.sql"))
            .bind(message_id)
            .bind(label_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn add_label_to_message(&self, message_id: &str, label_id: &str) -> Result<()> {
        sqlx::query(include_str!("../sql/add_label_to_message.sql"))
            .bind(message_id)
            .bind(label_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn validate_account_subject(account_subject: &str) -> Result<()> {
    if account_subject.is_empty() || account_subject.len() > 255 || !account_subject.is_ascii() {
        bail!("verified account subject is invalid");
    }
    Ok(())
}

fn validate_root_directory(directory: &Path, description: &str) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {description}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("{description} must be a real directory, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.mode() & 0o022 != 0 {
            bail!("{description} must not be writable by group or other users");
        }
    }

    directory
        .canonicalize()
        .with_context(|| format!("failed to resolve {description}"))
}

fn prepare_data_directory(data_root: &Path) -> Result<PathBuf> {
    let data_directory = data_root.join(APPLICATION_DIRECTORY_NAME);
    match std::fs::symlink_metadata(&data_directory) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&data_directory) {
                Ok(()) => sync_parent(data_root)?,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).context("failed to create private cache data directory");
                }
            }
        }
        Err(error) => {
            return Err(error).context("failed to inspect private cache data directory");
        }
    }
    validate_private_directory(&data_directory, "cache data directory")?;
    validate_directory_owner(data_root, &data_directory, "application data root")?;

    data_directory
        .canonicalize()
        .context("failed to resolve private cache data directory")
}

fn validate_private_directory(directory: &Path, description: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {description}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("{description} must be a real directory, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.mode() & 0o777 != 0o700 {
            bail!("{description} must have owner-only read, write, and execute permissions");
        }
    }
    Ok(())
}

fn validate_directory_owner(
    directory: &Path,
    private_directory: &Path,
    description: &str,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let owner = std::fs::symlink_metadata(directory)
            .with_context(|| format!("failed to inspect {description} owner"))?
            .uid();
        let private_owner = std::fs::symlink_metadata(private_directory)
            .context("failed to inspect cache data directory owner")?
            .uid();
        if owner != private_owner {
            bail!("{description} owner must match the cache data directory owner");
        }
    }
    Ok(())
}

fn account_database_path(data_root: &Path, account_subject: &str) -> PathBuf {
    let account_key = format!("{:x}", Sha256::digest(account_subject.as_bytes()));
    data_root
        .join(APPLICATION_DIRECTORY_NAME)
        .join(format!("gtui-{account_key}.db"))
}

fn acquire_account_lease(database_path: &Path, private_directory: &Path) -> Result<File> {
    let lock_path = database_path.with_extension("lock");
    acquire_lease(
        &lock_path,
        private_directory,
        "account cache lease",
        "account already open; close the other gtui instance and retry",
    )
}

fn acquire_legacy_lease(private_directory: &Path) -> Result<File> {
    acquire_lease(
        &private_directory.join(LEGACY_LEASE_NAME),
        private_directory,
        "legacy cache lease",
        "legacy cache handling is already in progress; retry after the other gtui instance starts",
    )
}

fn acquire_lease(
    lock_path: &Path,
    private_directory: &Path,
    description: &str,
    busy_message: &str,
) -> Result<File> {
    let before = validate_regular_file_if_present(lock_path, description, private_directory)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lease = options
        .open(lock_path)
        .with_context(|| format!("failed to open {description}"))?;
    let after = validate_regular_file_if_present(lock_path, description, private_directory)?
        .with_context(|| format!("{description} disappeared while it was being opened"))?;
    validate_opened_file_identity(lock_path, &lease, before.as_ref(), &after, description)?;
    if before.is_none() {
        sync_parent(private_directory)?;
    }

    if let Err(error) = FileExt::try_lock_exclusive(&lease) {
        if error.kind() == ErrorKind::WouldBlock {
            bail!(busy_message.to_owned());
        }
        return Err(error).with_context(|| format!("failed to acquire {description}"));
    }

    Ok(lease)
}

async fn create_account_database(
    database_path: &Path,
    account_subject: &str,
    lease: Arc<File>,
) -> Result<()> {
    let directory = database_path
        .parent()
        .context("account cache path has no parent directory")?;
    let temporary = tempfile::Builder::new()
        .prefix(".gtui-account-")
        .suffix(".db.tmp")
        .tempfile_in(directory)
        .context("failed to create temporary account cache")?;
    let temporary_metadata = validate_temporary_file(
        temporary.path(),
        temporary.as_file(),
        directory,
        "temporary account cache",
    )?;
    let database =
        Database::connect_file(temporary.path(), SqliteJournalMode::Delete, lease).await?;
    validate_path_identity(
        temporary.path(),
        &temporary_metadata,
        "temporary account cache",
        directory,
    )?;

    let initialization = async {
        database.run_migrations().await?;
        sqlx::query("INSERT INTO account_identity (singleton, account_subject) VALUES (1, ?)")
            .bind(account_subject)
            .execute(&database.pool)
            .await
            .context("failed to bind account cache identity")?;
        verify_account_identity(&database.pool, account_subject).await
    }
    .await;
    database.pool.close().await;
    initialization?;

    temporary
        .as_file()
        .sync_all()
        .context("failed to sync temporary account cache")?;
    match temporary.persist_noclobber(database_path) {
        Ok(file) => {
            let installed = validate_regular_file_if_present(
                database_path,
                "installed account cache",
                directory,
            )?
            .context("installed account cache disappeared")?;
            validate_opened_file_identity(
                database_path,
                &file,
                Some(&temporary_metadata),
                &installed,
                "installed account cache",
            )?;
            file.sync_all()
                .context("failed to sync installed account cache")?;
            sync_parent(directory)?;
        }
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            drop(error.file);
        }
        Err(error) => return Err(error.error).context("failed to install account cache"),
    }

    Ok(())
}

async fn verify_account_identity(pool: &SqlitePool, account_subject: &str) -> Result<()> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT singleton, account_subject FROM account_identity",
    )
    .fetch_all(pool)
    .await
    .context("account cache has no valid identity table")?;

    verify_account_identity_rows(&rows, account_subject)
}

async fn verify_account_identity_connection(
    connection: &mut SqliteConnection,
    account_subject: &str,
) -> Result<()> {
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT singleton, account_subject FROM account_identity",
    )
    .fetch_all(connection)
    .await
    .context("account cache has no valid identity table")?;

    verify_account_identity_rows(&rows, account_subject)
}

fn verify_account_identity_rows(rows: &[(i64, String)], account_subject: &str) -> Result<()> {
    if rows.len() != 1 || rows[0].0 != 1 || rows[0].1 != account_subject {
        bail!("account cache identity does not match the authenticated account");
    }
    validate_account_subject(&rows[0].1)
}

async fn handle_legacy_database(legacy_root: &Path, data_directory: &Path) -> Result<bool> {
    let path = legacy_root.join(LEGACY_DATABASE_NAME);
    let artifacts = existing_database_artifacts(&path, data_directory)?;
    if artifacts.is_empty() {
        return Ok(false);
    }

    let nonempty = artifacts.iter().try_fold(false, |nonempty, artifact| {
        Ok::<_, anyhow::Error>(
            nonempty
                || validate_regular_file_if_present(
                    artifact,
                    "legacy cache artifact",
                    data_directory,
                )?
                .context("legacy cache artifact disappeared during inspection")?
                .len()
                    != 0,
        )
    })?;
    if nonempty {
        quarantine_database(&path, data_directory, "unowned-backup").await?;
        return Ok(true);
    }

    for artifact in artifacts {
        let metadata =
            validate_regular_file_if_present(&artifact, "legacy cache artifact", data_directory)?
                .context("legacy cache artifact disappeared before removal")?;
        if metadata.len() != 0 {
            quarantine_database(&path, data_directory, "unowned-backup").await?;
            return Ok(true);
        }
        std::fs::remove_file(artifact).context("failed to remove empty legacy cache")?;
    }
    sync_parent(legacy_root)?;
    Ok(false)
}

async fn quarantine_database(
    database_path: &Path,
    data_directory: &Path,
    suffix: &str,
) -> Result<Option<PathBuf>> {
    start_quarantine(database_path, data_directory, suffix, None).await
}

async fn start_quarantine(
    database_path: &Path,
    data_directory: &Path,
    suffix: &str,
    stop_after_stage: Option<usize>,
) -> Result<Option<PathBuf>> {
    let sources = existing_database_artifacts(database_path, data_directory)?;
    if sources.is_empty() {
        return Ok(None);
    }

    if sources.contains(&database_path.to_path_buf())
        && sqlite_database_is_valid(database_path, data_directory).await?
    {
        return start_sqlite_quarantine(database_path, data_directory, suffix, stop_after_stage)
            .await
            .map(Some);
    }

    let backup_directory = create_private_backup_directory(data_directory, suffix)?;
    let marker = quarantine_marker(
        database_path,
        QuarantineKind::OpaqueFamily,
        &sources,
        data_directory,
    )?;
    write_quarantine_marker(&backup_directory, &marker, data_directory)?;
    let source_changed =
        resume_opaque_quarantine(&backup_directory, &marker, data_directory, stop_after_stage)?;
    if source_changed && stop_after_stage.is_none() {
        Box::pin(start_quarantine(
            database_path,
            data_directory,
            suffix,
            None,
        ))
        .await?;
    }

    Ok(Some(backup_directory))
}

async fn start_sqlite_quarantine(
    database_path: &Path,
    data_directory: &Path,
    suffix: &str,
    stop_after_stage: Option<usize>,
) -> Result<PathBuf> {
    let mut connection = open_exclusive_sqlite(database_path, data_directory).await?;
    let mut staged_backup = None;
    let operation = async {
        quiesce_sqlite(&mut connection).await?;
        let sources = existing_database_artifacts(database_path, data_directory)?;
        if !sources.contains(&database_path.to_path_buf()) {
            bail!("SQLite cache disappeared while it was being quiesced");
        }
        let backup_directory = create_private_backup_directory(data_directory, suffix)?;
        let marker = quarantine_marker(
            database_path,
            QuarantineKind::StandaloneSqlite,
            &sources,
            data_directory,
        )?;
        write_quarantine_marker(&backup_directory, &marker, data_directory)?;
        staged_backup = Some(backup_directory.clone());
        if stop_after_stage == Some(0) {
            bail!("injected quarantine interruption");
        }
        install_standalone_backup(&backup_directory, &marker, data_directory).await?;
        if stop_after_stage == Some(1) {
            bail!("injected quarantine interruption");
        }
        Ok::<_, anyhow::Error>((backup_directory, marker))
    }
    .await;
    let close = connection
        .close()
        .await
        .context("failed to close quiesced SQLite cache");
    let (backup_directory, marker) = match operation {
        Ok(staged) => staged,
        Err(error) if error.downcast_ref::<QuarantineSourceChanged>().is_some() => {
            close?;
            let backup_directory = staged_backup
                .context("changed SQLite cache has no quarantine staging directory")?;
            abandon_quarantine_marker(&backup_directory, data_directory)?;
            return Box::pin(start_sqlite_quarantine(
                database_path,
                data_directory,
                suffix,
                stop_after_stage,
            ))
            .await;
        }
        Err(error) => return Err(error),
    };
    close?;
    let source_changed =
        finish_sqlite_quarantine(&backup_directory, &marker, data_directory, stop_after_stage)
            .await?;
    if source_changed && stop_after_stage.is_none() {
        Box::pin(start_quarantine(
            database_path,
            data_directory,
            suffix,
            None,
        ))
        .await?;
    }
    Ok(backup_directory)
}

fn existing_database_artifacts(
    database_path: &Path,
    private_directory: &Path,
) -> Result<Vec<PathBuf>> {
    database_artifacts(database_path)
        .into_iter()
        .filter_map(|path| {
            match validate_regular_file_if_present(
                &path,
                "SQLite cache artifact",
                private_directory,
            ) {
                Ok(Some(_)) => Some(Ok(path)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .collect()
}

fn database_artifacts(database_path: &Path) -> [PathBuf; 4] {
    [
        database_path.to_path_buf(),
        sidecar_path(database_path, "-wal"),
        sidecar_path(database_path, "-shm"),
        sidecar_path(database_path, "-journal"),
    ]
}

fn sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn validate_regular_file_if_present(
    path: &Path,
    description: &str,
    private_directory: &Path,
) -> Result<Option<std::fs::Metadata>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {description}"));
        }
    };
    validate_regular_metadata(&metadata, description, private_directory)?;
    Ok(Some(metadata))
}

fn validate_regular_metadata(
    metadata: &std::fs::Metadata,
    description: &str,
    private_directory: &Path,
) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("{description} must be a regular file, not a symlink or special file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let directory_owner = std::fs::symlink_metadata(private_directory)
            .context("failed to inspect cache data directory owner")?
            .uid();
        if metadata.uid() != directory_owner {
            bail!("{description} owner must match the cache data directory owner");
        }
        if metadata.mode() & 0o022 != 0 {
            bail!("{description} must not be writable by group or other users");
        }
        if metadata.nlink() != 1 {
            bail!("{description} must have exactly one filesystem link");
        }
    }
    #[cfg(windows)]
    {
        let _ = private_directory;
        // Rust's std APIs expose no equivalent hard-link/handle identity check on Windows. This
        // boundary trusts the same OS user and the LocalAppData ACL; keep the Unix nlink check.
    }
    Ok(())
}

fn validate_opened_file_identity(
    path: &Path,
    file: &File,
    before: Option<&std::fs::Metadata>,
    after: &std::fs::Metadata,
    description: &str,
) -> Result<()> {
    let opened = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let same = |left: &std::fs::Metadata, right: &std::fs::Metadata| {
            left.dev() == right.dev() && left.ino() == right.ino()
        };
        if before.is_some_and(|before| !same(before, after)) || !same(&opened, after) {
            bail!(
                "{description} changed identity while {} was opened",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_path_identity(
    path: &Path,
    before: &std::fs::Metadata,
    description: &str,
    private_directory: &Path,
) -> Result<()> {
    let after = validate_regular_file_if_present(path, description, private_directory)?
        .with_context(|| format!("{description} disappeared while it was open"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if before.dev() != after.dev() || before.ino() != after.ino() {
            bail!("{description} changed identity while it was open");
        }
    }
    Ok(())
}

fn validate_temporary_file(
    path: &Path,
    file: &File,
    private_directory: &Path,
    description: &str,
) -> Result<std::fs::Metadata> {
    let metadata = validate_regular_file_if_present(path, description, private_directory)?
        .with_context(|| format!("{description} disappeared after creation"))?;
    validate_opened_file_identity(path, file, None, &metadata, description)?;
    Ok(metadata)
}

fn open_regular_file(
    path: &Path,
    description: &str,
    private_directory: &Path,
) -> Result<(File, std::fs::Metadata)> {
    let before = validate_regular_file_if_present(path, description, private_directory)?
        .with_context(|| format!("{description} disappeared before it was opened"))?;
    let file = File::open(path).with_context(|| format!("failed to open {description}"))?;
    let after = validate_regular_file_if_present(path, description, private_directory)?
        .with_context(|| format!("{description} disappeared while it was opened"))?;
    validate_opened_file_identity(path, &file, Some(&before), &after, description)?;
    Ok((file, after))
}

fn fingerprint_file(
    path: &Path,
    description: &str,
    private_directory: &Path,
) -> Result<FileFingerprint> {
    let (mut file, metadata) = open_regular_file(path, description, private_directory)?;
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {description}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        length += read as u64;
    }
    validate_path_identity(path, &metadata, description, private_directory)?;
    if length != metadata.len() {
        bail!("{description} changed length while it was read");
    }
    Ok(FileFingerprint {
        length,
        sha256: format!("{:x}", digest.finalize()),
    })
}

fn create_private_backup_directory(data_directory: &Path, suffix: &str) -> Result<PathBuf> {
    if !matches!(suffix, "quarantine" | "unowned-backup") {
        bail!("invalid cache quarantine kind");
    }

    for attempt in 0_u64.. {
        let backup_directory = data_directory.join(format!("{suffix}-{attempt}"));
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&backup_directory) {
            Ok(()) => {
                validate_private_directory(&backup_directory, "cache backup directory")?;
                validate_directory_owner(
                    &backup_directory,
                    data_directory,
                    "cache backup directory",
                )?;
                sync_parent(data_directory)?;
                return Ok(backup_directory);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).context("failed to create private cache backup directory");
            }
        }
    }

    unreachable!("unbounded cache backup directory search exhausted")
}

fn quarantine_marker(
    database_path: &Path,
    kind: QuarantineKind,
    sources: &[PathBuf],
    data_directory: &Path,
) -> Result<QuarantineMarker> {
    let artifacts = sources
        .iter()
        .map(|source| {
            let basename = source
                .file_name()
                .and_then(|name| name.to_str())
                .context("cache artifact has no valid file name")?
                .to_owned();
            Ok(QuarantineArtifact {
                basename,
                fingerprint: fingerprint_file(source, "cache artifact", data_directory)?,
            })
        })
        .collect::<Result<_>>()?;
    Ok(QuarantineMarker {
        version: 2,
        kind,
        source_database: database_path.to_path_buf(),
        artifacts,
    })
}

fn write_quarantine_marker(
    backup_directory: &Path,
    marker: &QuarantineMarker,
    data_directory: &Path,
) -> Result<()> {
    let temporary_path = backup_directory.join(QUARANTINE_MARKER_TEMP_NAME);
    let marker_path = backup_directory.join(QUARANTINE_MARKER_NAME);
    let bytes = serde_json::to_vec(marker).context("failed to encode cache quarantine marker")?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary_path)
        .context("failed to create cache quarantine marker")?;
    file.write_all(&bytes)
        .context("failed to write cache quarantine marker")?;
    file.sync_all()
        .context("failed to sync cache quarantine marker")?;
    let temporary_metadata = validate_temporary_file(
        &temporary_path,
        &file,
        data_directory,
        "temporary cache quarantine marker",
    )?;
    drop(file);
    std::fs::rename(&temporary_path, &marker_path)
        .context("failed to install cache quarantine marker")?;
    let marker_metadata =
        validate_regular_file_if_present(&marker_path, "cache quarantine marker", data_directory)?
            .context("cache quarantine marker disappeared after installation")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if temporary_metadata.dev() != marker_metadata.dev()
            || temporary_metadata.ino() != marker_metadata.ino()
        {
            bail!("cache quarantine marker changed identity during installation");
        }
    }
    sync_parent(backup_directory)
}

async fn recover_quarantines(legacy_root: &Path, data_directory: &Path) -> Result<()> {
    let mut backup_directories = Vec::new();
    for entry in
        std::fs::read_dir(data_directory).context("failed to scan private cache data directory")?
    {
        let entry = entry.context("failed to inspect private cache data entry")?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .context("failed to inspect private cache data entry")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !metadata.file_type().is_dir() {
            if is_backup_directory_name(&name) {
                bail!("cache backup path must be a real directory, not a symlink or special file");
            }
            continue;
        }

        match std::fs::symlink_metadata(path.join(QUARANTINE_MARKER_NAME)) {
            Ok(_) => backup_directories.push(path),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to inspect cache quarantine marker"),
        }
    }
    backup_directories.sort();

    for backup_directory in backup_directories {
        validate_private_directory(&backup_directory, "cache backup directory")?;
        validate_directory_owner(&backup_directory, data_directory, "cache backup directory")?;
        let marker = read_quarantine_marker(&backup_directory, data_directory)?;
        validate_quarantine_marker(&marker, legacy_root, data_directory)?;

        let _lease = if marker.source_database.parent() == Some(data_directory) {
            Some(acquire_account_lease(
                &marker.source_database,
                data_directory,
            )?)
        } else {
            None
        };
        let source_changed = match marker.kind {
            QuarantineKind::StandaloneSqlite => {
                recover_sqlite_quarantine(&backup_directory, &marker, data_directory).await?
            }
            QuarantineKind::OpaqueFamily => {
                resume_opaque_quarantine(&backup_directory, &marker, data_directory, None)?
            }
        };
        if source_changed {
            let suffix = if marker.source_database.parent() == Some(data_directory) {
                "quarantine"
            } else {
                "unowned-backup"
            };
            start_quarantine(&marker.source_database, data_directory, suffix, None).await?;
        }
    }

    Ok(())
}

fn is_backup_directory_name(name: &str) -> bool {
    ["quarantine-", "unowned-backup-"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|attempt| {
            !attempt.is_empty() && attempt.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn read_quarantine_marker(
    backup_directory: &Path,
    data_directory: &Path,
) -> Result<QuarantineMarker> {
    let marker_path = backup_directory.join(QUARANTINE_MARKER_NAME);
    let (file, _) = open_regular_file(&marker_path, "cache quarantine marker", data_directory)?;
    serde_json::from_reader(file).context("failed to decode cache quarantine marker")
}

fn validate_quarantine_marker(
    marker: &QuarantineMarker,
    legacy_root: &Path,
    data_directory: &Path,
) -> Result<()> {
    if marker.version != 2 {
        bail!("unsupported cache quarantine marker version");
    }
    let source_parent = marker
        .source_database
        .parent()
        .context("cache quarantine marker source has no parent")?;
    let source_name = marker
        .source_database
        .file_name()
        .and_then(|name| name.to_str())
        .context("cache quarantine marker source has no valid name")?;
    let is_legacy = source_parent == legacy_root && source_name == LEGACY_DATABASE_NAME;
    let is_account = source_parent == data_directory && is_account_database_name(source_name);
    if !is_legacy && !is_account {
        bail!("cache quarantine marker names an unexpected source");
    }

    let artifact_names = marker
        .artifacts
        .iter()
        .map(|artifact| artifact.basename.as_str())
        .collect::<Vec<_>>();
    let expected = database_artifacts(&marker.source_database)
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .filter(|name| artifact_names.contains(&name.as_str()))
        .collect::<Vec<_>>();
    if marker.artifacts.is_empty()
        || artifact_names != expected
        || marker.artifacts.iter().any(|artifact| {
            artifact.fingerprint.sha256.len() != 64
                || !artifact
                    .fingerprint
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        })
    {
        bail!("cache quarantine marker has an invalid artifact manifest");
    }
    if marker.kind == QuarantineKind::StandaloneSqlite
        && marker.artifacts[0].basename != source_name
    {
        bail!("standalone SQLite quarantine marker has no main database");
    }

    Ok(())
}

fn is_account_database_name(name: &str) -> bool {
    name.strip_prefix("gtui-")
        .and_then(|name| name.strip_suffix(".db"))
        .is_some_and(|key| {
            key.len() == 64
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn marker_artifact<'a>(
    marker: &'a QuarantineMarker,
    basename: &str,
) -> Result<&'a QuarantineArtifact> {
    marker
        .artifacts
        .iter()
        .find(|artifact| artifact.basename == basename)
        .context("cache quarantine marker has no main database fingerprint")
}

async fn sqlite_database_is_valid(database_path: &Path, data_directory: &Path) -> Result<bool> {
    if !has_sqlite_header(database_path, data_directory)? {
        return Ok(false);
    }
    let temporary_directory = tempfile::Builder::new()
        .prefix(".sqlite-classification-")
        .tempdir_in(data_directory)
        .context("failed to create temporary SQLite classification directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            temporary_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .context("failed to secure temporary SQLite classification directory")?;
    }
    validate_private_directory(
        temporary_directory.path(),
        "temporary SQLite classification directory",
    )?;
    validate_directory_owner(
        temporary_directory.path(),
        data_directory,
        "temporary SQLite classification directory",
    )?;
    let sources = existing_database_artifacts(database_path, data_directory)?;
    let source_fingerprints = sources
        .iter()
        .map(|source| fingerprint_file(source, "SQLite cache artifact", data_directory))
        .collect::<Result<Vec<_>>>()?;
    for (source_path, source_fingerprint) in sources.iter().zip(&source_fingerprints) {
        let (mut source, source_metadata) =
            open_regular_file(source_path, "SQLite cache artifact", data_directory)?;
        let destination = temporary_directory.path().join(
            source_path
                .file_name()
                .context("SQLite cache artifact has no file name")?,
        );
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut copy = options
            .open(&destination)
            .context("failed to create temporary SQLite classification artifact")?;
        std::io::copy(&mut source, &mut copy)
            .context("failed to copy SQLite artifact for classification")?;
        copy.sync_all()
            .context("failed to sync SQLite classification artifact")?;
        validate_path_identity(
            source_path,
            &source_metadata,
            "SQLite cache artifact",
            data_directory,
        )?;
        if fingerprint_file(
            &destination,
            "temporary SQLite classification artifact",
            data_directory,
        )? != *source_fingerprint
        {
            return Err(QuarantineSourceChanged.into());
        }
    }
    for (source, fingerprint) in sources.iter().zip(source_fingerprints) {
        if fingerprint_file(source, "SQLite cache artifact", data_directory)? != fingerprint {
            return Err(QuarantineSourceChanged.into());
        }
    }
    let temporary_database = temporary_directory.path().join(
        database_path
            .file_name()
            .context("SQLite cache has no file name")?,
    );
    let options = SqliteConnectOptions::new()
        .filename(temporary_database)
        .create_if_missing(false);
    let mut connection = match SqliteConnection::connect_with(&options).await {
        Ok(connection) => connection,
        Err(error) if sqlite_error_is_invalid(&error) => return Ok(false),
        Err(error) => return Err(error).context("failed to validate SQLite cache"),
    };
    let validation = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(&mut connection)
        .await;
    let close = connection.close().await;
    match validation {
        Ok(rows) => {
            close.context("failed to close validated SQLite cache")?;
            Ok(rows.as_slice() == ["ok"])
        }
        Err(error) if sqlite_error_is_invalid(&error) => Ok(false),
        Err(error) => Err(error).context("failed to validate SQLite cache contents"),
    }
}

async fn open_exclusive_sqlite(
    database_path: &Path,
    data_directory: &Path,
) -> Result<SqliteConnection> {
    let before =
        validate_regular_file_if_present(database_path, "SQLite cache artifact", data_directory)?
            .context("SQLite cache artifact disappeared before exclusive open")?;
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(false);
    let connection = SqliteConnection::connect_with(&options)
        .await
        .context("failed to open SQLite cache for quarantine")?;
    validate_path_identity(
        database_path,
        &before,
        "SQLite cache artifact",
        data_directory,
    )?;
    Ok(connection)
}

async fn quiesce_sqlite(connection: &mut SqliteConnection) -> Result<()> {
    let result = async {
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&mut *connection)
            .await?;
        acquire_exclusive_lock(connection).await?;
        let (busy, _, _) = sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&mut *connection)
            .await
            .context("failed to checkpoint SQLite cache WAL")?;
        if busy != 0 {
            bail!("cache is busy; close every process using it before retrying");
        }
        let journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode = DELETE")
            .fetch_one(&mut *connection)
            .await
            .context("failed to switch quiesced SQLite cache to DELETE journaling")?;
        if !journal_mode.eq_ignore_ascii_case("delete") {
            bail!("failed to switch quiesced SQLite cache to DELETE journaling");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    match result {
        Err(error) if anyhow_error_is_sqlite_busy(&error) => {
            bail!("cache is busy; close every process using it before retrying")
        }
        result => result.context("failed to establish exclusive SQLite quarantine access"),
    }
}

async fn install_standalone_backup(
    backup_directory: &Path,
    marker: &QuarantineMarker,
    data_directory: &Path,
) -> Result<()> {
    let source_name = marker
        .source_database
        .file_name()
        .and_then(|name| name.to_str())
        .context("SQLite cache has no valid file name")?;
    let source_record = marker_artifact(marker, source_name)?;
    let destination = backup_directory.join(source_name);
    if validate_regular_file_if_present(&destination, "standalone SQLite backup", data_directory)?
        .is_some()
    {
        return validate_installed_backup(&destination, source_record, data_directory).await;
    }

    if fingerprint_file(
        &marker.source_database,
        "SQLite cache source",
        data_directory,
    )? != source_record.fingerprint
    {
        return Err(QuarantineSourceChanged.into());
    }
    let (mut source, source_metadata) = open_regular_file(
        &marker.source_database,
        "SQLite cache source",
        data_directory,
    )?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".standalone-backup-")
        .suffix(".db.tmp")
        .tempfile_in(backup_directory)
        .context("failed to create temporary standalone SQLite backup")?;
    let temporary_metadata = validate_temporary_file(
        temporary.path(),
        temporary.as_file(),
        data_directory,
        "temporary standalone SQLite backup",
    )?;
    std::io::copy(&mut source, temporary.as_file_mut())
        .context("failed to copy standalone SQLite backup")?;
    temporary
        .as_file_mut()
        .flush()
        .context("failed to flush standalone SQLite backup")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync standalone SQLite backup")?;
    validate_path_identity(
        &marker.source_database,
        &source_metadata,
        "SQLite cache source",
        data_directory,
    )?;
    if fingerprint_file(
        temporary.path(),
        "temporary standalone SQLite backup",
        data_directory,
    )? != source_record.fingerprint
    {
        return Err(QuarantineSourceChanged.into());
    }
    validate_sqlite_integrity(temporary.path(), data_directory).await?;
    if fingerprint_file(
        temporary.path(),
        "temporary standalone SQLite backup",
        data_directory,
    )? != source_record.fingerprint
    {
        bail!("standalone SQLite integrity check changed the backup contents");
    }

    match temporary.persist_noclobber(&destination) {
        Ok(file) => {
            let installed = validate_regular_file_if_present(
                &destination,
                "standalone SQLite backup",
                data_directory,
            )?
            .context("standalone SQLite backup disappeared after installation")?;
            validate_opened_file_identity(
                &destination,
                &file,
                Some(&temporary_metadata),
                &installed,
                "standalone SQLite backup",
            )?;
            file.sync_all()
                .context("failed to sync installed standalone SQLite backup")?;
            sync_parent(backup_directory)?;
        }
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            drop(error.file);
        }
        Err(error) => {
            return Err(error.error).context("failed to install standalone SQLite backup");
        }
    }
    validate_installed_backup(&destination, source_record, data_directory).await
}

async fn validate_installed_backup(
    destination: &Path,
    source_record: &QuarantineArtifact,
    data_directory: &Path,
) -> Result<()> {
    if fingerprint_file(destination, "standalone SQLite backup", data_directory)?
        != source_record.fingerprint
    {
        bail!("standalone SQLite backup fingerprint does not match its marker");
    }
    validate_sqlite_integrity(destination, data_directory).await?;
    if fingerprint_file(destination, "standalone SQLite backup", data_directory)?
        != source_record.fingerprint
    {
        bail!("standalone SQLite integrity check changed the installed backup contents");
    }
    let (file, _) = open_regular_file(destination, "standalone SQLite backup", data_directory)?;
    file.sync_all()
        .context("failed to sync standalone SQLite backup")?;
    sync_parent(
        destination
            .parent()
            .context("standalone SQLite backup has no parent")?,
    )
}

async fn validate_sqlite_integrity(path: &Path, data_directory: &Path) -> Result<()> {
    let before = validate_regular_file_if_present(path, "SQLite backup", data_directory)?
        .context("SQLite backup disappeared before integrity check")?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .context("failed to open standalone SQLite backup for integrity check")?;
    validate_path_identity(path, &before, "SQLite backup", data_directory)?;
    let rows = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(&mut connection)
        .await
        .context("failed to run standalone SQLite backup integrity check");
    let close = connection
        .close()
        .await
        .context("failed to close standalone SQLite backup integrity check");
    let rows = rows?;
    close?;
    if rows.as_slice() != ["ok"] {
        bail!(
            "standalone SQLite backup failed integrity check: {}",
            rows.join("; ")
        );
    }
    for sidecar in database_artifacts(path).into_iter().skip(1) {
        if validate_regular_file_if_present(&sidecar, "SQLite backup sidecar", data_directory)?
            .is_some()
        {
            bail!("standalone SQLite backup integrity check left a sidecar");
        }
    }
    Ok(())
}

async fn recover_sqlite_quarantine(
    backup_directory: &Path,
    marker: &QuarantineMarker,
    data_directory: &Path,
) -> Result<bool> {
    let source_name = marker
        .source_database
        .file_name()
        .and_then(|name| name.to_str())
        .context("SQLite cache has no valid file name")?;
    let source_record = marker_artifact(marker, source_name)?;
    let destination = backup_directory.join(source_name);
    let source_exists = validate_regular_file_if_present(
        &marker.source_database,
        "SQLite cache source",
        data_directory,
    )?
    .is_some();
    let destination_exists =
        validate_regular_file_if_present(&destination, "standalone SQLite backup", data_directory)?
            .is_some();

    if !source_exists && !destination_exists {
        bail!("SQLite quarantine recovery found neither source nor backup");
    }
    if destination_exists {
        validate_installed_backup(&destination, source_record, data_directory).await?;
    } else {
        let source_fingerprint = fingerprint_file(
            &marker.source_database,
            "SQLite cache source",
            data_directory,
        )?;
        if source_fingerprint != source_record.fingerprint {
            abandon_quarantine_marker(backup_directory, data_directory)?;
            return Ok(true);
        }
        if !sqlite_database_is_valid(&marker.source_database, data_directory).await? {
            abandon_quarantine_marker(backup_directory, data_directory)?;
            return Ok(true);
        }
        let mut connection = open_exclusive_sqlite(&marker.source_database, data_directory).await?;
        let operation = async {
            quiesce_sqlite(&mut connection).await?;
            if fingerprint_file(
                &marker.source_database,
                "SQLite cache source",
                data_directory,
            )? != source_record.fingerprint
            {
                return Err(QuarantineSourceChanged.into());
            }
            install_standalone_backup(backup_directory, marker, data_directory).await
        }
        .await;
        let close = connection
            .close()
            .await
            .context("failed to close recovered SQLite cache");
        match operation {
            Ok(()) => close?,
            Err(error) if error.downcast_ref::<QuarantineSourceChanged>().is_some() => {
                close?;
                abandon_quarantine_marker(backup_directory, data_directory)?;
                return Ok(true);
            }
            Err(error) => return Err(error),
        }
    }

    if source_exists
        && fingerprint_file(
            &marker.source_database,
            "SQLite cache source",
            data_directory,
        )? == source_record.fingerprint
    {
        let mut connection = open_exclusive_sqlite(&marker.source_database, data_directory).await?;
        let quiesce = quiesce_sqlite(&mut connection).await;
        let close = connection
            .close()
            .await
            .context("failed to close duplicate SQLite cache source");
        quiesce?;
        close?;
    }
    finish_sqlite_quarantine(backup_directory, marker, data_directory, None).await
}

async fn finish_sqlite_quarantine(
    backup_directory: &Path,
    marker: &QuarantineMarker,
    data_directory: &Path,
    stop_after_stage: Option<usize>,
) -> Result<bool> {
    let source_name = marker
        .source_database
        .file_name()
        .and_then(|name| name.to_str())
        .context("SQLite cache has no valid file name")?;
    let source_record = marker_artifact(marker, source_name)?;
    let destination = backup_directory.join(source_name);
    if validate_regular_file_if_present(
        &marker.source_database,
        "SQLite cache source",
        data_directory,
    )?
    .is_none()
        && validate_regular_file_if_present(
            &destination,
            "standalone SQLite backup",
            data_directory,
        )?
        .is_none()
    {
        bail!("SQLite quarantine recovery found neither source nor backup");
    }
    validate_installed_backup(&destination, source_record, data_directory).await?;

    let mut source_changed = false;
    for source in database_artifacts(&marker.source_database) {
        let Some(_) = validate_regular_file_if_present(
            &source,
            "SQLite cache source artifact",
            data_directory,
        )?
        else {
            continue;
        };
        let basename = source
            .file_name()
            .and_then(|name| name.to_str())
            .context("SQLite cache source artifact has no valid name")?;
        let Some(record) = marker
            .artifacts
            .iter()
            .find(|artifact| artifact.basename == basename)
        else {
            source_changed = true;
            continue;
        };
        if fingerprint_file(&source, "SQLite cache source artifact", data_directory)?
            != record.fingerprint
        {
            source_changed = true;
        }
    }

    if !source_changed {
        // Destination durability must precede every source-side removal.
        sync_parent(backup_directory)?;
        for source in database_artifacts(&marker.source_database) {
            if validate_regular_file_if_present(
                &source,
                "SQLite cache source artifact",
                data_directory,
            )?
            .is_some()
            {
                std::fs::remove_file(&source)
                    .context("failed to remove backed-up SQLite cache source artifact")?;
            }
        }
        sync_parent(
            marker
                .source_database
                .parent()
                .context("SQLite cache source has no parent")?,
        )?;
        if stop_after_stage == Some(2) {
            bail!("injected quarantine interruption");
        }
    }

    complete_quarantine_marker(backup_directory, data_directory)?;
    Ok(source_changed)
}

fn resume_opaque_quarantine(
    backup_directory: &Path,
    marker: &QuarantineMarker,
    data_directory: &Path,
    stop_after_stage: Option<usize>,
) -> Result<bool> {
    let mut completed_stage = 0;
    let mut checkpoint = || {
        if stop_after_stage == Some(completed_stage) {
            bail!("injected quarantine interruption");
        }
        completed_stage += 1;
        Ok(())
    };
    checkpoint()?;
    let source_directory = marker
        .source_database
        .parent()
        .context("quarantine marker source has no parent directory")?;
    let mut source_changed = false;

    for source in database_artifacts(&marker.source_database) {
        let basename = source
            .file_name()
            .and_then(|name| name.to_str())
            .context("cache artifact has no valid name")?;
        let destination = backup_directory.join(basename);
        let source_exists =
            validate_regular_file_if_present(&source, "opaque cache artifact", data_directory)?
                .is_some();
        let destination_exists = validate_regular_file_if_present(
            &destination,
            "quarantined opaque cache artifact",
            data_directory,
        )?
        .is_some();
        let Some(record) = marker
            .artifacts
            .iter()
            .find(|artifact| artifact.basename == basename)
        else {
            if destination_exists {
                bail!("cache quarantine recovery found an untracked destination artifact");
            }
            source_changed |= source_exists;
            continue;
        };

        if destination_exists
            && fingerprint_file(
                &destination,
                "quarantined opaque cache artifact",
                data_directory,
            )? != record.fingerprint
        {
            bail!("quarantined opaque cache artifact does not match its marker");
        }
        let source_matches = source_exists
            && fingerprint_file(&source, "opaque cache artifact", data_directory)?
                == record.fingerprint;

        if !source_exists && !destination_exists {
            bail!("cache quarantine recovery found neither source nor destination artifact");
        }
        if source_exists && !source_matches {
            if destination_exists {
                source_changed = true;
                continue;
            }
            abandon_quarantine_marker(backup_directory, data_directory)?;
            return Ok(true);
        }

        if !destination_exists {
            let (mut source_file, source_metadata) =
                open_regular_file(&source, "opaque cache artifact", data_directory)?;
            let mut temporary = tempfile::Builder::new()
                .prefix(".opaque-backup-")
                .suffix(".tmp")
                .tempfile_in(backup_directory)
                .context("failed to create temporary opaque cache backup")?;
            let temporary_metadata = validate_temporary_file(
                temporary.path(),
                temporary.as_file(),
                data_directory,
                "temporary opaque cache backup",
            )?;
            checkpoint()?;
            std::io::copy(&mut source_file, temporary.as_file_mut())
                .context("failed to copy opaque cache backup")?;
            checkpoint()?;
            temporary
                .as_file()
                .sync_all()
                .context("failed to sync temporary opaque cache backup")?;
            checkpoint()?;
            validate_path_identity(
                &source,
                &source_metadata,
                "opaque cache artifact",
                data_directory,
            )?;
            if fingerprint_file(
                temporary.path(),
                "temporary opaque cache backup",
                data_directory,
            )? != record.fingerprint
            {
                drop(source_file);
                drop(temporary);
                abandon_quarantine_marker(backup_directory, data_directory)?;
                return Ok(true);
            }
            checkpoint()?;

            match temporary.persist_noclobber(&destination) {
                Ok(file) => {
                    let installed = validate_regular_file_if_present(
                        &destination,
                        "quarantined opaque cache artifact",
                        data_directory,
                    )?
                    .context("quarantined opaque cache artifact disappeared after installation")?;
                    validate_opened_file_identity(
                        &destination,
                        &file,
                        Some(&temporary_metadata),
                        &installed,
                        "quarantined opaque cache artifact",
                    )?;
                    file.sync_all()
                        .context("failed to sync installed opaque cache backup")?;
                }
                Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
                    drop(error.file);
                }
                Err(error) => {
                    return Err(error.error).context("failed to install opaque cache backup");
                }
            }
            if fingerprint_file(
                &destination,
                "quarantined opaque cache artifact",
                data_directory,
            )? != record.fingerprint
            {
                bail!("installed opaque cache artifact does not match its marker");
            }
            checkpoint()?;
        }

        let (destination_file, _) = open_regular_file(
            &destination,
            "quarantined opaque cache artifact",
            data_directory,
        )?;
        destination_file
            .sync_all()
            .context("failed to sync quarantined opaque cache artifact")?;
        if fingerprint_file(
            &destination,
            "quarantined opaque cache artifact",
            data_directory,
        )? != record.fingerprint
        {
            bail!("quarantined opaque cache artifact changed before becoming durable");
        }
        sync_parent(backup_directory)?;
        if !destination_exists {
            checkpoint()?;
        }

        if !source_exists {
            sync_parent(source_directory)?;
            continue;
        }
        if fingerprint_file(&source, "opaque cache artifact", data_directory)? != record.fingerprint
        {
            source_changed = true;
            continue;
        }
        std::fs::remove_file(&source)
            .context("failed to remove durably backed-up opaque cache artifact")?;
        if !destination_exists {
            checkpoint()?;
        }
        sync_parent(source_directory)?;
        if !destination_exists {
            checkpoint()?;
        }
    }

    complete_quarantine_marker(backup_directory, data_directory)?;
    if source_changed {
        sync_parent(source_directory)?;
    }
    Ok(source_changed)
}

fn complete_quarantine_marker(backup_directory: &Path, data_directory: &Path) -> Result<()> {
    let marker_path = backup_directory.join(QUARANTINE_MARKER_NAME);
    validate_regular_file_if_present(&marker_path, "cache quarantine marker", data_directory)?
        .context("cache quarantine marker disappeared before completion")?;
    std::fs::remove_file(marker_path).context("failed to complete cache quarantine")?;
    sync_parent(backup_directory)
}

fn abandon_quarantine_marker(backup_directory: &Path, data_directory: &Path) -> Result<()> {
    complete_quarantine_marker(backup_directory, data_directory)
        .context("failed to preserve changed cache source for reprocessing")?;
    match std::fs::remove_dir(backup_directory) {
        Ok(()) => sync_parent(data_directory),
        Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error).context("failed to remove empty quarantine staging directory"),
    }
}

fn has_sqlite_header(path: &Path, data_directory: &Path) -> Result<bool> {
    let metadata = validate_regular_file_if_present(path, "SQLite cache artifact", data_directory)?
        .context("SQLite cache artifact disappeared during inspection")?;
    if metadata.len() < SQLITE_HEADER.len() as u64 {
        return Ok(false);
    }
    let (mut file, _) = open_regular_file(path, "SQLite cache artifact", data_directory)?;
    let mut header = [0_u8; SQLITE_HEADER.len()];
    file.read_exact(&mut header)
        .context("failed to inspect SQLite cache artifact header")?;
    Ok(&header == SQLITE_HEADER)
}

fn sqlite_error_is_invalid(error: &sqlx::Error) -> bool {
    sqlite_error_has_code(error, &[11, 26])
}

fn sqlite_error_has_code(error: &sqlx::Error, codes: &[i32]) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| codes.contains(&(code & 0xff)))
}

fn anyhow_error_is_sqlite_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .is_some_and(|error| sqlite_error_has_code(error, &[5, 6]))
    })
}

#[cfg(unix)]
fn sync_parent(directory: &Path) -> Result<()> {
    File::open(directory)?
        .sync_all()
        .context("failed to sync cache directory")
}

#[cfg(not(unix))]
fn sync_parent(_directory: &Path) -> Result<()> {
    Ok(())
}

async fn migrate_exclusively(
    connection: &mut SqliteConnection,
    account_subject: Option<&str>,
) -> Result<()> {
    acquire_exclusive_lock(connection).await?;
    if let Some(account_subject) = account_subject {
        verify_account_identity_connection(connection, account_subject).await?;
    }
    validate_migration_state(connection).await?;
    MIGRATOR
        .run_direct(connection)
        .await
        .context("failed to run database migrations")
}

async fn acquire_exclusive_lock(connection: &mut SqliteConnection) -> Result<()> {
    let locking_mode = sqlx::query_scalar::<_, String>("PRAGMA locking_mode = EXCLUSIVE")
        .fetch_one(&mut *connection)
        .await
        .context("failed to set exclusive database migration locking mode")?;
    if locking_mode != "exclusive" {
        bail!("failed to set exclusive database migration locking mode");
    }

    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut *connection)
        .await
        .context("failed to acquire exclusive database migration lock")?;
    sqlx::query("COMMIT")
        .execute(connection)
        .await
        .context("failed to retain exclusive database migration lock")?;

    Ok(())
}

async fn validate_migration_state(connection: &mut SqliteConnection) -> Result<()> {
    let actual_ledger_schema = ledger_schema_objects(connection).await?;
    let mut expected = SqliteConnection::connect("sqlite::memory:")
        .await
        .context("failed to prepare expected migration schema")?;
    expected
        .ensure_migrations_table()
        .await
        .context("failed to prepare expected migration ledger")?;
    let expected_ledger_schema = ledger_schema_objects(&mut expected).await?;

    if actual_ledger_schema.is_empty() {
        return validate_adoptable_schema(connection, &mut expected).await;
    }
    if actual_ledger_schema != expected_ledger_schema {
        bail!(
            "unsupported migration ledger: restore a valid SQLx migration ledger or remove the cache to re-sync"
        );
    }

    let applied = sqlx::query_as::<_, LedgerRow>(
        "SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await
    .context("failed to inspect database migration ledger")?;
    if applied.is_empty() {
        return validate_adoptable_schema(connection, &mut expected).await;
    }

    let known = MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .collect::<Vec<_>>();
    if applied.len() > known.len()
        || applied
            .iter()
            .zip(&known)
            .any(|((version, success, checksum), migration)| {
                *version != migration.version
                    || *success != 1
                    || checksum.as_slice() != migration.checksum.as_ref()
            })
    {
        bail!(
            "unsupported migration ledger: expected successful, checksum-matching embedded migrations; restore a valid cache or remove it to re-sync"
        );
    }

    for migration in known.into_iter().take(applied.len()) {
        sqlx::query(&migration.sql)
            .execute(&mut expected)
            .await
            .context("failed to derive expected application schema")?;
    }

    if application_schema_objects(connection).await?
        != application_schema_objects(&mut expected).await?
    {
        bail!(
            "unsupported versioned schema: application objects do not match the applied migration ledger; restore a valid cache or remove it to re-sync"
        );
    }

    Ok(())
}

async fn validate_adoptable_schema(
    connection: &mut SqliteConnection,
    expected: &mut SqliteConnection,
) -> Result<()> {
    let actual = application_schema_objects(connection).await?;
    if actual.is_empty() {
        return Ok(());
    }

    sqlx::query(V0_SCHEMA)
        .execute(&mut *expected)
        .await
        .context("failed to prepare known v0 schema")?;
    if actual != application_schema_objects(expected).await? {
        bail!(
            "unsupported unversioned schema: expected an empty database or the exact gtui v0 schema; back up the cache and restore a compatible schema or remove it to re-sync"
        );
    }

    Ok(())
}

async fn application_schema_objects(
    connection: &mut SqliteConnection,
) -> Result<Vec<SchemaObject>> {
    load_schema_objects(
        connection,
        "SELECT type, name, tbl_name, sql
         FROM sqlite_schema
         WHERE name NOT GLOB 'sqlite_*' AND tbl_name <> '_sqlx_migrations'
         ORDER BY type, name",
    )
    .await
}

async fn ledger_schema_objects(connection: &mut SqliteConnection) -> Result<Vec<SchemaObject>> {
    load_schema_objects(
        connection,
        "SELECT type, name, tbl_name, sql
         FROM sqlite_schema
         WHERE name NOT GLOB 'sqlite_*' AND tbl_name = '_sqlx_migrations'
         ORDER BY type, name",
    )
    .await
}

async fn load_schema_objects(
    connection: &mut SqliteConnection,
    query: &str,
) -> Result<Vec<SchemaObject>> {
    let objects = sqlx::query_as::<_, SchemaObject>(query)
        .fetch_all(connection)
        .await
        .context("failed to inspect database schema")?;

    Ok(normalize_schema_objects(objects))
}

fn normalize_schema_objects(objects: Vec<SchemaObject>) -> Vec<SchemaObject> {
    objects
        .into_iter()
        .map(|(object_type, name, table_name, sql)| {
            let sql = sql.map(|sql| sql.split_whitespace().collect::<Vec<_>>().join(" "));
            (object_type, name, table_name, sql)
        })
        .collect()
}

#[cfg(test)]
async fn all_schema_objects(pool: &SqlitePool) -> Result<Vec<SchemaObject>> {
    let objects = sqlx::query_as::<_, SchemaObject>(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_schema
         ORDER BY type, name",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect database schema")?;

    Ok(normalize_schema_objects(objects))
}

#[async_trait]
impl SyncStore for Database {
    async fn upsert_labels(&self, labels: &[models::Label]) -> Result<()> {
        Database::upsert_labels(self, labels).await
    }

    async fn upsert_messages(&self, messages: &[models::Message], label_id: &str) -> Result<()> {
        Database::upsert_messages(self, messages, label_id).await
    }

    async fn message_exists(&self, id: &str) -> Result<bool> {
        Database::message_exists(self, id).await
    }

    async fn get_message_date(&self, id: &str) -> Result<Option<i64>> {
        Database::get_message_date(self, id).await
    }

    async fn get_messages_with_dates_by_label(
        &self,
        label_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        Database::get_messages_with_dates_by_label(self, label_id, limit).await
    }

    async fn remove_label_from_message(&self, message_id: &str, label_id: &str) -> Result<()> {
        Database::remove_label_from_message(self, message_id, label_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::migrate::Migrate;
    use tempfile::TempDir;

    async fn temporary_database() -> Result<(TempDir, Database)> {
        let directory = tempfile::tempdir()?;
        let database_url = temporary_database_url(&directory);
        let database = Database::new(&database_url).await?;

        Ok((directory, database))
    }

    fn temporary_database_url(directory: &TempDir) -> String {
        format!("sqlite://{}", directory.path().join("gtui.db").display())
    }

    async fn initialize_v0(database: &Database) -> Result<()> {
        sqlx::query(V0_SCHEMA).execute(&database.pool).await?;
        Ok(())
    }

    async fn schema_objects(database: &Database) -> Result<Vec<SchemaObject>> {
        all_schema_objects(&database.pool).await
    }

    async fn object_names(database: &Database, object_type: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT name
             FROM sqlite_schema
             WHERE type = ? AND name NOT GLOB 'sqlite_*'
             ORDER BY name",
        )
        .bind(object_type)
        .fetch_all(&database.pool)
        .await?)
    }

    async fn ledger_rows(database: &Database) -> Result<Vec<(i64, bool, Vec<u8>)>> {
        Ok(sqlx::query_as(
            "SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&database.pool)
        .await?)
    }

    async fn assert_unsupported_schema(database: &Database) -> Result<()> {
        let before = schema_objects(database).await?;
        let error = database
            .run_migrations()
            .await
            .expect_err("schema accepted");

        assert!(
            error.to_string().contains("unsupported unversioned schema"),
            "unexpected error: {error:#}"
        );
        assert_eq!(schema_objects(database).await?, before);
        assert!(
            !object_names(database, "table")
                .await?
                .contains(&"_sqlx_migrations".to_string())
        );

        Ok(())
    }

    async fn assert_versioned_schema_rejected_without_writes(database: &Database) -> Result<()> {
        let before_schema = schema_objects(database).await?;
        let before_ledger = ledger_rows(database).await?;

        let error = database
            .run_migrations()
            .await
            .expect_err("applied schema damage accepted");

        assert!(
            error.to_string().contains("unsupported versioned schema"),
            "unexpected error: {error:#}"
        );
        assert_eq!(schema_objects(database).await?, before_schema);
        assert_eq!(ledger_rows(database).await?, before_ledger);

        Ok(())
    }

    async fn close_account(open: AccountOpen) {
        open.database.pool.close().await;
    }

    fn prepare_test_data(directory: &TempDir) -> Result<PathBuf> {
        let data_root = validate_root_directory(directory.path(), "test data root")?;
        prepare_data_directory(&data_root)
    }

    fn backup_directories(data_directory: &Path, kind: &str) -> Result<Vec<PathBuf>> {
        let mut paths = data_directory
            .read_dir()?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&format!("{kind}-")))
        });
        paths.sort();
        Ok(paths)
    }

    fn only_backup_directory(data_directory: &Path, kind: &str) -> Result<PathBuf> {
        let backups = backup_directories(data_directory, kind)?;
        if backups.len() != 1 {
            bail!("expected one {kind} directory, found {}", backups.len());
        }
        Ok(backups[0].clone())
    }

    fn latest_backup_directory(data_directory: &Path, kind: &str) -> Result<PathBuf> {
        backup_directories(data_directory, kind)?
            .pop()
            .with_context(|| format!("expected at least one {kind} directory"))
    }

    async fn create_hot_wal_fixture(path: &Path) -> Result<()> {
        use std::process::Command;

        let status = Command::new(std::env::current_exe()?)
            .arg("db::tests::account_hot_wal_fixture_child")
            .arg("--exact")
            .arg("--nocapture")
            .env("GTUI_TEST_HOT_WAL_PATH", path)
            .status()
            .context("failed to create crash-produced WAL fixture")?;
        if !status.success() {
            bail!("WAL fixture child exited unsuccessfully: {status}");
        }
        if !path.is_file() || !sidecar_path(path, "-wal").is_file() {
            bail!("WAL fixture child did not leave a hot SQLite family");
        }
        Ok(())
    }

    async fn create_integrity_corrupt_sqlite_fixture(path: &Path) -> Result<Vec<u8>> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query(
            "PRAGMA page_size = 512;
             VACUUM;
             CREATE TABLE legacy_rows (value BLOB NOT NULL);
             WITH RECURSIVE rows(value) AS (
                 VALUES(1)
                 UNION ALL
                 SELECT value + 1 FROM rows WHERE value < 100
             )
             INSERT INTO legacy_rows SELECT zeroblob(100) FROM rows;",
        )
        .execute(&mut connection)
        .await?;
        let root_page = sqlx::query_scalar::<_, i64>(
            "SELECT rootpage FROM sqlite_schema WHERE name = 'legacy_rows'",
        )
        .fetch_one(&mut connection)
        .await? as usize;
        connection.close().await?;

        let mut bytes = std::fs::read(path)?;
        let page_size = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
        let root_offset = (root_page - 1) * page_size;
        assert_eq!(
            bytes[root_offset], 5,
            "fixture root is not an interior page"
        );
        let cell_offset =
            u16::from_be_bytes([bytes[root_offset + 12], bytes[root_offset + 13]]) as usize;
        let child_page = u32::from_be_bytes(
            bytes[root_offset + cell_offset..root_offset + cell_offset + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let child_offset = (child_page - 1) * page_size;
        assert_eq!(bytes[child_offset], 13, "fixture child is not a leaf page");
        bytes[child_offset + 3..child_offset + 5].copy_from_slice(&[0, 0]);
        std::fs::write(path, &bytes)?;

        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .immutable(true);
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_schema")
            .fetch_one(&mut connection)
            .await?;
        connection.close().await?;
        Ok(bytes)
    }

    async fn assert_standalone_backup(path: &Path, data_directory: &Path, rows: i64) -> Result<()> {
        validate_sqlite_integrity(path, data_directory).await?;
        for sidecar in database_artifacts(path).into_iter().skip(1) {
            assert!(!sidecar.try_exists()?);
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .immutable(true);
        let mut connection = SqliteConnection::connect_with(&options).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
                .fetch_one(&mut connection)
                .await?,
            "delete"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM legacy_rows")
                .fetch_one(&mut connection)
                .await?,
            rows
        );
        connection.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn account_hot_wal_fixture_child() -> Result<()> {
        let Some(path) = std::env::var_os("GTUI_TEST_HOT_WAL_PATH") else {
            return Ok(());
        };
        let options = SqliteConnectOptions::new()
            .filename(PathBuf::from(path))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query("PRAGMA wal_autocheckpoint = 0")
            .execute(&mut connection)
            .await?;
        sqlx::query("CREATE TABLE legacy_rows (value TEXT NOT NULL)")
            .execute(&mut connection)
            .await?;
        sqlx::query("INSERT INTO legacy_rows VALUES ('preserved from WAL')")
            .execute(&mut connection)
            .await?;
        std::process::exit(0);
    }

    #[tokio::test]
    async fn account_reopen_preserves_cache_and_other_account_is_isolated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject_a = "stable-subject-a";
        let subject_b = "stable-subject-b";
        let first = Database::open_account(directory.path(), directory.path(), subject_a).await?;
        let data_directory = directory.path().join(APPLICATION_DIRECTORY_NAME);
        assert_eq!(
            account_database_path(directory.path(), subject_a).parent(),
            Some(data_directory.as_path())
        );
        sqlx::query("INSERT INTO labels (id, name, type) VALUES ('INBOX', 'Inbox', 'system')")
            .execute(&first.database.pool)
            .await?;
        close_account(first).await;
        let filename = account_database_path(directory.path(), subject_a)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let account_key = filename
            .strip_prefix("gtui-")
            .and_then(|name| name.strip_suffix(".db"))
            .unwrap();
        assert_eq!(account_key.len(), 64);
        assert!(
            account_key
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );

        let reopened =
            Database::open_account(directory.path(), directory.path(), subject_a).await?;
        assert_eq!(reopened.database.get_labels().await?.len(), 1);
        close_account(reopened).await;

        let other = Database::open_account(directory.path(), directory.path(), subject_b).await?;
        assert!(other.database.get_labels().await?.is_empty());
        assert_ne!(
            account_database_path(directory.path(), subject_a),
            account_database_path(directory.path(), subject_b)
        );
        close_account(other).await;

        Ok(())
    }

    #[tokio::test]
    async fn account_data_and_legacy_roots_are_independently_injected() -> Result<()> {
        let data_root = tempfile::tempdir()?;
        let legacy_root = tempfile::tempdir()?;
        let subject = "stable-subject-a";

        let opened = Database::open_account(data_root.path(), legacy_root.path(), subject).await?;

        assert!(account_database_path(data_root.path(), subject).is_file());
        assert!(!account_database_path(legacy_root.path(), subject).try_exists()?);
        assert!(
            !legacy_root
                .path()
                .join(APPLICATION_DIRECTORY_NAME)
                .try_exists()?
        );
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_rejects_non_directory_runtime_and_non_regular_artifacts() -> Result<()> {
        let runtime_parent = tempfile::tempdir()?;
        let runtime_file = runtime_parent.path().join("runtime-file");
        File::create(&runtime_file)?;
        assert!(
            Database::open_account(&runtime_file, runtime_parent.path(), "stable-subject-a")
                .await
                .expect_err("file runtime root was accepted")
                .to_string()
                .contains("real directory")
        );

        for artifact_index in 0..=database_artifacts(Path::new("gtui.db")).len() {
            let directory = tempfile::tempdir()?;
            prepare_test_data(&directory)?;
            let database_path = account_database_path(directory.path(), "stable-subject-a");
            let artifact = if artifact_index == database_artifacts(&database_path).len() {
                database_path.with_extension("lock")
            } else {
                database_artifacts(&database_path)[artifact_index].clone()
            };
            std::fs::create_dir(&artifact)?;

            let error =
                Database::open_account(directory.path(), directory.path(), "stable-subject-a")
                    .await
                    .expect_err("non-regular account artifact was accepted");
            assert!(error.to_string().contains("regular file"));
        }

        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn account_rejects_symlinked_runtime_data_and_artifact_paths() -> Result<()> {
        use std::os::unix::fs::symlink;

        let runtime_target = tempfile::tempdir()?;
        let runtime_parent = tempfile::tempdir()?;
        let runtime_link = runtime_parent.path().join("runtime-link");
        symlink(runtime_target.path(), &runtime_link)?;
        assert!(
            Database::open_account(&runtime_link, runtime_parent.path(), "stable-subject-a")
                .await
                .expect_err("symlinked runtime root was accepted")
                .to_string()
                .contains("real directory")
        );
        assert!(
            !runtime_target
                .path()
                .join(APPLICATION_DIRECTORY_NAME)
                .try_exists()?
        );

        let data_runtime = tempfile::tempdir()?;
        let data_target = tempfile::tempdir()?;
        symlink(
            data_target.path(),
            data_runtime.path().join(APPLICATION_DIRECTORY_NAME),
        )?;
        assert!(
            Database::open_account(data_runtime.path(), data_runtime.path(), "stable-subject-a",)
                .await
                .expect_err("symlinked data directory was accepted")
                .to_string()
                .contains("real directory")
        );

        for artifact_index in 0..=database_artifacts(Path::new("gtui.db")).len() {
            let directory = tempfile::tempdir()?;
            prepare_test_data(&directory)?;
            let database_path = account_database_path(directory.path(), "stable-subject-a");
            let artifact = if artifact_index == database_artifacts(&database_path).len() {
                database_path.with_extension("lock")
            } else {
                database_artifacts(&database_path)[artifact_index].clone()
            };
            let target = directory.path().join(format!("target-{artifact_index}"));
            std::fs::write(&target, b"must remain untouched")?;
            symlink(&target, &artifact)?;

            let error =
                Database::open_account(directory.path(), directory.path(), "stable-subject-a")
                    .await
                    .expect_err("symlinked account artifact was accepted");
            assert!(error.to_string().contains("regular file"));
            assert_eq!(std::fs::read(target)?, b"must remain untouched");
        }

        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn account_unix_rejects_multi_link_database_lease_and_legacy_artifacts() -> Result<()> {
        for artifact_kind in ["database", "lease", "legacy"] {
            let data_root = tempfile::tempdir()?;
            let legacy_root = tempfile::tempdir()?;
            let data_directory = prepare_test_data(&data_root)?;
            let database_path = account_database_path(data_root.path(), "stable-subject-a");
            let artifact = match artifact_kind {
                "database" => database_path,
                "lease" => database_path.with_extension("lock"),
                "legacy" => legacy_root.path().join(LEGACY_DATABASE_NAME),
                _ => unreachable!(),
            };
            std::fs::write(&artifact, b"same-user hard-link fixture")?;
            std::fs::hard_link(
                &artifact,
                data_directory.join(format!("{artifact_kind}-second-link")),
            )?;

            let error =
                Database::open_account(data_root.path(), legacy_root.path(), "stable-subject-a")
                    .await
                    .expect_err("multi-link cache artifact was accepted");
            assert!(
                format!("{error:#}").contains("exactly one filesystem link"),
                "unexpected error: {error:#}"
            );
            assert_eq!(std::fs::read(&artifact)?, b"same-user hard-link fixture");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn account_windows_hard_links_use_same_user_acl_boundary() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        let second_link = directory.path().join("second-link");
        std::fs::write(&source, b"same-user hard-link fixture")?;
        std::fs::hard_link(&source, second_link)?;

        validate_regular_metadata(
            &std::fs::symlink_metadata(source)?,
            "Windows cache artifact",
            directory.path(),
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn account_rejects_unsafe_runtime_and_data_permissions() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let unsafe_runtime = tempfile::tempdir()?;
        std::fs::set_permissions(
            unsafe_runtime.path(),
            std::fs::Permissions::from_mode(0o770),
        )?;
        let error = Database::open_account(
            unsafe_runtime.path(),
            unsafe_runtime.path(),
            "stable-subject-a",
        )
        .await
        .expect_err("group-writable runtime root was accepted");
        assert!(error.to_string().contains("must not be writable"));

        let unsafe_data = tempfile::tempdir()?;
        let data_directory = prepare_test_data(&unsafe_data)?;
        std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o750))?;
        let error =
            Database::open_account(unsafe_data.path(), unsafe_data.path(), "stable-subject-a")
                .await
                .expect_err("non-private data directory was accepted");
        assert!(error.to_string().contains("owner-only"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn account_creates_owner_private_data_directory() -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir()?;
        let opened =
            Database::open_account(directory.path(), directory.path(), "stable-subject-a").await?;
        let data_directory = directory.path().join(APPLICATION_DIRECTORY_NAME);
        let metadata = std::fs::symlink_metadata(&data_directory)?;
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(
            account_database_path(directory.path(), "stable-subject-a").parent(),
            Some(data_directory.as_path())
        );
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_filename_identity_mismatch_is_quarantined() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject_a = "stable-subject-a";
        let subject_b = "stable-subject-b";
        let first = Database::open_account(directory.path(), directory.path(), subject_a).await?;
        close_account(first).await;
        let path_a = account_database_path(directory.path(), subject_a);
        let path_b = account_database_path(directory.path(), subject_b);
        std::fs::rename(path_a, &path_b)?;

        let error = Database::open_account(directory.path(), directory.path(), subject_b)
            .await
            .expect_err("mismatched identity was accepted");

        assert!(
            error.to_string().contains("identity verification failed"),
            "unexpected error: {error:#}"
        );
        assert!(!path_b.try_exists()?);
        let data_directory = directory.path().join(APPLICATION_DIRECTORY_NAME);
        let backup = latest_backup_directory(&data_directory, "quarantine")?;
        assert!(backup.join(path_b.file_name().unwrap()).is_file());
        Ok(())
    }

    #[tokio::test]
    async fn account_ownerless_database_is_quarantined_not_claimed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
        let data_directory = prepare_test_data(&directory)?;
        let path = account_database_path(directory.path(), subject);
        let ownerless = Database::new(&format!("sqlite://{}", path.display())).await?;
        ownerless.run_migrations().await?;
        ownerless.pool.close().await;

        assert!(
            Database::open_account(directory.path(), directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert!(!backup_directories(&data_directory, "quarantine")?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn account_owner_verification_precedes_migration_schema_writes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
        let data_directory = prepare_test_data(&directory)?;
        let path = account_database_path(directory.path(), subject);
        let ownerless = Database::new(&format!("sqlite://{}", path.display())).await?;
        initialize_v0(&ownerless).await?;
        ownerless.pool.close().await;

        let error = Database::open_account(directory.path(), directory.path(), subject)
            .await
            .expect_err("ownerless cache was migrated before identity verification");
        assert!(
            error.to_string().contains("identity verification failed"),
            "unexpected error: {error:#}"
        );

        let backup = latest_backup_directory(&data_directory, "quarantine")?;
        let quarantined = backup.join(path.file_name().unwrap());
        let options = SqliteConnectOptions::new()
            .filename(quarantined)
            .read_only(true);
        let pool = SqlitePool::connect_with(options).await?;
        let schema_writes = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE name IN ('_sqlx_migrations', 'account_identity')",
        )
        .fetch_one(&pool)
        .await?;
        pool.close().await;
        assert_eq!(schema_writes, 0);
        Ok(())
    }

    #[tokio::test]
    async fn account_wrong_owner_open_does_not_mutate_journal_mode_or_sidecars() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject_a = "stable-subject-a";
        let subject_b = "stable-subject-b";
        let opened = Database::open_account(directory.path(), directory.path(), subject_a).await?;
        close_account(opened).await;
        let data_directory = directory.path().join(APPLICATION_DIRECTORY_NAME);
        let path = account_database_path(directory.path(), subject_a);

        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(false),
        )
        .await?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut connection)
            .await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA journal_mode = DELETE")
                .fetch_one(&mut connection)
                .await?,
            "delete"
        );
        connection.close().await?;
        let before = std::fs::read(&path)?;
        let before_sidecars = database_artifacts(&path)
            .into_iter()
            .skip(1)
            .map(|sidecar| sidecar.try_exists())
            .collect::<std::io::Result<Vec<_>>>()?;
        let lease = Arc::new(acquire_account_lease(&path, &data_directory)?);

        let error = Database::open_verified_account_database(&path, subject_b, lease)
            .await
            .expect_err("wrong account owner was accepted");

        assert!(
            format!("{error:#}").contains("identity does not match"),
            "unexpected error: {error:#}"
        );
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(
            database_artifacts(&path)
                .into_iter()
                .skip(1)
                .map(|sidecar| sidecar.try_exists())
                .collect::<std::io::Result<Vec<_>>>()?,
            before_sidecars
        );
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true)
            .immutable(true);
        let mut connection = SqliteConnection::connect_with(&options).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
                .fetch_one(&mut connection)
                .await?,
            "delete"
        );
        connection.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn account_duplicate_identity_database_is_quarantined() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
        let data_directory = prepare_test_data(&directory)?;
        let path = account_database_path(directory.path(), subject);
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await?;
        sqlx::query("CREATE TABLE account_identity (singleton INTEGER, account_subject TEXT)")
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO account_identity VALUES (1, 'stable-subject-a'), (1, 'stable-subject-a')",
        )
        .execute(&pool)
        .await?;
        pool.close().await;

        assert!(
            Database::open_account(directory.path(), directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert!(!backup_directories(&data_directory, "quarantine")?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn account_malformed_database_is_quarantined() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
        let data_directory = prepare_test_data(&directory)?;
        let path = account_database_path(directory.path(), subject);
        std::fs::write(&path, b"fake malformed sqlite fixture")?;

        assert!(
            Database::open_account(directory.path(), directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert_eq!(backup_directories(&data_directory, "quarantine")?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn account_malformed_schema_is_quarantined_after_owner_check() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
        let opened = Database::open_account(directory.path(), directory.path(), subject).await?;
        close_account(opened).await;
        let path = account_database_path(directory.path(), subject);
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path)).await?;
        sqlx::query("DROP TRIGGER messages_ai")
            .execute(&pool)
            .await?;
        pool.close().await;

        assert!(
            Database::open_account(directory.path(), directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert!(
            !backup_directories(
                &directory.path().join(APPLICATION_DIRECTORY_NAME),
                "quarantine",
            )?
            .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_quarantine_preserves_basenames_and_journal() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy = directory.path().join(LEGACY_DATABASE_NAME);
        for (index, artifact) in database_artifacts(&legacy).into_iter().enumerate() {
            std::fs::write(artifact, format!("legacy artifact {index}"))?;
        }

        let opened =
            Database::open_account(directory.path(), directory.path(), "stable-subject-a").await?;

        assert!(opened.legacy_quarantined);
        let data_directory = directory.path().join(APPLICATION_DIRECTORY_NAME);
        assert!(existing_database_artifacts(&legacy, &data_directory)?.is_empty());
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::symlink_metadata(&backup)?.mode() & 0o777, 0o700);
        }
        for (index, artifact) in database_artifacts(&legacy).into_iter().enumerate() {
            let name = artifact.file_name().unwrap();
            assert_eq!(
                std::fs::read_to_string(backup.join(name))?,
                format!("legacy artifact {index}")
            );
        }
        assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_sidecar_only_family_starts_and_is_preserved_opaquely() -> Result<()> {
        let data_root = tempfile::tempdir()?;
        let legacy_root = tempfile::tempdir()?;
        let legacy = legacy_root.path().join(LEGACY_DATABASE_NAME);
        let wal = sidecar_path(&legacy, "-wal");
        std::fs::write(&wal, b"orphaned WAL bytes")?;

        let opened =
            Database::open_account(data_root.path(), legacy_root.path(), "stable-subject-a")
                .await?;

        assert!(opened.legacy_quarantined);
        assert!(!wal.try_exists()?);
        let data_directory = data_root.path().join(APPLICATION_DIRECTORY_NAME);
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert_eq!(
            std::fs::read(backup.join(wal.file_name().unwrap()))?,
            b"orphaned WAL bytes"
        );
        assert!(!backup.join(LEGACY_DATABASE_NAME).try_exists()?);
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_invalid_sqlite_header_is_preserved_opaquely() -> Result<()> {
        let data_root = tempfile::tempdir()?;
        let legacy_root = tempfile::tempdir()?;
        let legacy = legacy_root.path().join(LEGACY_DATABASE_NAME);
        let mut invalid = SQLITE_HEADER.to_vec();
        invalid.extend_from_slice(b"not a complete SQLite database");
        std::fs::write(&legacy, &invalid)?;
        let journal = sidecar_path(&legacy, "-journal");
        std::fs::write(&journal, b"opaque journal")?;

        let opened =
            Database::open_account(data_root.path(), legacy_root.path(), "stable-subject-a")
                .await?;

        assert!(opened.legacy_quarantined);
        let data_directory = data_root.path().join(APPLICATION_DIRECTORY_NAME);
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert_eq!(std::fs::read(backup.join(LEGACY_DATABASE_NAME))?, invalid);
        assert_eq!(
            std::fs::read(backup.join(journal.file_name().unwrap()))?,
            b"opaque journal"
        );
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_integrity_corruption_is_preserved_opaquely() -> Result<()> {
        let data_root = tempfile::tempdir()?;
        let legacy_root = tempfile::tempdir()?;
        let legacy = legacy_root.path().join(LEGACY_DATABASE_NAME);
        let corrupt = create_integrity_corrupt_sqlite_fixture(&legacy).await?;

        let opened =
            Database::open_account(data_root.path(), legacy_root.path(), "stable-subject-a")
                .await?;

        assert!(opened.legacy_quarantined);
        assert!(!legacy.try_exists()?);
        let data_directory = data_root.path().join(APPLICATION_DIRECTORY_NAME);
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert_eq!(std::fs::read(backup.join(LEGACY_DATABASE_NAME))?, corrupt);
        assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_corrupt_standalone_recovery_falls_back_to_opaque() -> Result<()> {
        let data_root = tempfile::tempdir()?;
        let legacy_root_directory = tempfile::tempdir()?;
        let legacy_root =
            validate_root_directory(legacy_root_directory.path(), "legacy cache root")?;
        let data_directory = prepare_test_data(&data_root)?;
        let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
        let corrupt = create_integrity_corrupt_sqlite_fixture(&legacy).await?;
        let sources = existing_database_artifacts(&legacy, &data_directory)?;
        let stale_backup = create_private_backup_directory(&data_directory, "unowned-backup")?;
        let marker = quarantine_marker(
            &legacy,
            QuarantineKind::StandaloneSqlite,
            &sources,
            &data_directory,
        )?;
        write_quarantine_marker(&stale_backup, &marker, &data_directory)?;

        recover_quarantines(&legacy_root, &data_directory).await?;

        assert!(!legacy.try_exists()?);
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert_eq!(std::fs::read(backup.join(LEGACY_DATABASE_NAME))?, corrupt);
        assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_hot_wal_becomes_standalone_backup() -> Result<()> {
        let data_root = tempfile::tempdir()?;
        let legacy_root = tempfile::tempdir()?;
        let legacy = legacy_root.path().join(LEGACY_DATABASE_NAME);
        create_hot_wal_fixture(&legacy).await?;

        let opened =
            Database::open_account(data_root.path(), legacy_root.path(), "stable-subject-a")
                .await?;

        assert!(opened.legacy_quarantined);
        let data_directory = data_root.path().join(APPLICATION_DIRECTORY_NAME);
        assert!(existing_database_artifacts(&legacy, &data_directory)?.is_empty());
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert_standalone_backup(&backup.join(LEGACY_DATABASE_NAME), &data_directory, 1).await?;
        assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_hot_wal_backup_recovers_every_crash_stage() -> Result<()> {
        for stop_after_stage in 0..=2 {
            let directory = tempfile::tempdir()?;
            let legacy_root = validate_root_directory(directory.path(), "legacy cache root")?;
            let data_directory = prepare_data_directory(&legacy_root)?;
            let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
            create_hot_wal_fixture(&legacy).await?;

            let error = start_quarantine(
                &legacy,
                &data_directory,
                "unowned-backup",
                Some(stop_after_stage),
            )
            .await
            .expect_err("SQLite quarantine crash was not injected");
            assert!(
                error
                    .to_string()
                    .contains("injected quarantine interruption"),
                "unexpected error: {error:#}"
            );
            let backup = only_backup_directory(&data_directory, "unowned-backup")?;
            let destination = backup.join(LEGACY_DATABASE_NAME);
            if existing_database_artifacts(&legacy, &data_directory)?.is_empty() {
                assert!(
                    destination.is_file(),
                    "source was removed before backup existed"
                );
            }

            recover_quarantines(&legacy_root, &data_directory).await?;

            assert!(existing_database_artifacts(&legacy, &data_directory)?.is_empty());
            assert_standalone_backup(&destination, &data_directory, 1).await?;
            assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        }
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_sqlite_changed_after_backup_is_reprocessed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy_root = validate_root_directory(directory.path(), "legacy cache root")?;
        let data_directory = prepare_data_directory(&legacy_root)?;
        let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
        create_hot_wal_fixture(&legacy).await?;
        start_quarantine(&legacy, &data_directory, "unowned-backup", Some(1))
            .await
            .expect_err("SQLite quarantine crash was not injected");

        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&legacy)
                .create_if_missing(false),
        )
        .await?;
        sqlx::query("INSERT INTO legacy_rows VALUES ('changed after backup')")
            .execute(&mut connection)
            .await?;
        connection.close().await?;

        recover_quarantines(&legacy_root, &data_directory).await?;
        assert!(!legacy.try_exists()?);
        let backups = backup_directories(&data_directory, "unowned-backup")?;
        assert_eq!(backups.len(), 2);
        let first_backup = &backups[0];
        assert_standalone_backup(&first_backup.join(LEGACY_DATABASE_NAME), &data_directory, 1)
            .await?;
        assert!(!first_backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        assert_standalone_backup(&backups[1].join(LEGACY_DATABASE_NAME), &data_directory, 2)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn account_opaque_recovery_removes_only_identical_duplicate_source() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy_root = validate_root_directory(directory.path(), "legacy cache root")?;
        let data_directory = prepare_data_directory(&legacy_root)?;
        let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
        std::fs::write(&legacy, b"opaque duplicate")?;
        start_quarantine(
            &legacy,
            &data_directory,
            "unowned-backup",
            Some(OPAQUE_DESTINATION_DURABLE_STAGE),
        )
        .await
        .expect_err("opaque quarantine crash was not injected");
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert!(legacy.is_file());
        assert!(backup.join(LEGACY_DATABASE_NAME).is_file());

        recover_quarantines(&legacy_root, &data_directory).await?;

        assert!(!legacy.try_exists()?);
        assert_eq!(
            std::fs::read(backup.join(LEGACY_DATABASE_NAME))?,
            b"opaque duplicate"
        );
        assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        Ok(())
    }

    #[tokio::test]
    async fn account_sqlite_recovery_rejects_missing_source_and_destination() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy_root = validate_root_directory(directory.path(), "legacy cache root")?;
        let data_directory = prepare_data_directory(&legacy_root)?;
        let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
        create_hot_wal_fixture(&legacy).await?;
        start_quarantine(&legacy, &data_directory, "unowned-backup", Some(0))
            .await
            .expect_err("SQLite quarantine crash was not injected");
        for artifact in database_artifacts(&legacy) {
            match std::fs::remove_file(artifact) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        let error = recover_quarantines(&legacy_root, &data_directory)
            .await
            .expect_err("missing SQLite source and backup were accepted");

        assert!(format!("{error:#}").contains("neither source nor backup"));
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert!(backup.join(QUARANTINE_MARKER_NAME).is_file());
        Ok(())
    }

    #[tokio::test]
    async fn account_opaque_quarantine_recovers_every_copy_boundary() -> Result<()> {
        let artifact_count = database_artifacts(Path::new("gtui.db")).len();
        for stop_after_stage in 0..=artifact_count * OPAQUE_QUARANTINE_STAGES_PER_ARTIFACT {
            let data_root = tempfile::tempdir()?;
            let legacy_root_directory = tempfile::tempdir()?;
            let legacy_root =
                validate_root_directory(legacy_root_directory.path(), "legacy cache root")?;
            let data_directory = prepare_test_data(&data_root)?;
            let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
            for (index, artifact) in database_artifacts(&legacy).into_iter().enumerate() {
                std::fs::write(artifact, format!("artifact {index}"))?;
            }

            let error = start_quarantine(
                &legacy,
                &data_directory,
                "unowned-backup",
                Some(stop_after_stage),
            )
            .await
            .expect_err("quarantine crash was not injected");
            assert!(
                error
                    .to_string()
                    .contains("injected quarantine interruption")
            );

            let backup = only_backup_directory(&data_directory, "unowned-backup")?;
            for (index, source) in database_artifacts(&legacy).into_iter().enumerate() {
                let destination = backup.join(source.file_name().unwrap());
                let install_stage = index * OPAQUE_QUARANTINE_STAGES_PER_ARTIFACT + 5;
                let removal_stage = index * OPAQUE_QUARANTINE_STAGES_PER_ARTIFACT + 7;
                assert_eq!(
                    source.try_exists()?,
                    stop_after_stage < removal_stage,
                    "source state at stage {stop_after_stage} for artifact {index}"
                );
                assert_eq!(
                    destination.try_exists()?,
                    stop_after_stage >= install_stage,
                    "destination state at stage {stop_after_stage} for artifact {index}"
                );
                if source.try_exists()? {
                    assert_eq!(
                        std::fs::read_to_string(&source)?,
                        format!("artifact {index}")
                    );
                }
                if destination.try_exists()? {
                    assert_eq!(
                        std::fs::read_to_string(&destination)?,
                        format!("artifact {index}")
                    );
                }
            }

            recover_quarantines(&legacy_root, &data_directory).await?;
            assert!(existing_database_artifacts(&legacy, &data_directory)?.is_empty());
            for (index, artifact) in database_artifacts(&legacy).into_iter().enumerate() {
                assert_eq!(
                    std::fs::read_to_string(backup.join(artifact.file_name().unwrap()))?,
                    format!("artifact {index}")
                );
            }
            assert!(!backup.join(QUARANTINE_MARKER_NAME).try_exists()?);
        }
        Ok(())
    }

    #[tokio::test]
    async fn account_quarantine_recovery_preserves_changed_source() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy_root = validate_root_directory(directory.path(), "legacy cache root")?;
        let data_directory = prepare_data_directory(&legacy_root)?;
        let legacy = legacy_root.join(LEGACY_DATABASE_NAME);
        std::fs::write(&legacy, b"original")?;

        start_quarantine(
            &legacy,
            &data_directory,
            "unowned-backup",
            Some(OPAQUE_DESTINATION_DURABLE_STAGE),
        )
        .await
        .expect_err("quarantine crash was not injected");
        std::fs::write(&legacy, b"replacement")?;

        recover_quarantines(&legacy_root, &data_directory).await?;
        let backups = backup_directories(&data_directory, "unowned-backup")?;
        assert_eq!(backups.len(), 2);
        assert_eq!(
            std::fs::read(backups[0].join(LEGACY_DATABASE_NAME))?,
            b"original"
        );
        assert!(!legacy.try_exists()?);
        assert_eq!(
            std::fs::read(backups[1].join(LEGACY_DATABASE_NAME))?,
            b"replacement"
        );
        assert!(
            backups
                .iter()
                .all(|backup| !backup.join(QUARANTINE_MARKER_NAME).exists())
        );
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_busy_sqlite_is_not_quarantined() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy_url = temporary_database_url(&directory);
        let legacy = Database::new(&legacy_url).await?;
        legacy.run_migrations().await?;
        let mut writer = legacy.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *writer).await?;

        let error = Database::open_account(directory.path(), directory.path(), "stable-subject-a")
            .await
            .expect_err("busy legacy cache was moved");
        assert!(
            format!("{error:#}").contains("cache is busy"),
            "unexpected error: {error:#}"
        );
        assert!(directory.path().join(LEGACY_DATABASE_NAME).is_file());
        assert!(
            backup_directories(
                &directory.path().join(APPLICATION_DIRECTORY_NAME),
                "unowned-backup"
            )?
            .is_empty()
        );

        sqlx::query("ROLLBACK").execute(&mut *writer).await?;
        drop(writer);
        legacy.pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_empty_database_is_removed_after_account_creation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy = directory.path().join(LEGACY_DATABASE_NAME);
        File::create(&legacy)?;

        let opened =
            Database::open_account(directory.path(), directory.path(), "stable-subject-a").await?;

        assert!(!opened.legacy_quarantined);
        assert!(!legacy.try_exists()?);
        assert!(account_database_path(directory.path(), "stable-subject-a").try_exists()?);
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_lease_rejects_second_open() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first =
            Database::open_account(directory.path(), directory.path(), "stable-subject-a").await?;

        let error = Database::open_account(directory.path(), directory.path(), "stable-subject-a")
            .await
            .expect_err("second account lease was acquired");

        assert!(error.to_string().contains("account already open"));
        close_account(first).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_lease_child_process() -> Result<()> {
        let Some(directory) = std::env::var_os("GTUI_TEST_LEASE_DIRECTORY") else {
            return Ok(());
        };
        let ready = std::env::var_os("GTUI_TEST_LEASE_READY")
            .context("child lease test has no readiness path")?;
        let stop = std::env::var_os("GTUI_TEST_LEASE_STOP")
            .context("child lease test has no stop path")?;
        let directory = PathBuf::from(directory);
        let opened = Database::open_account(&directory, &directory, "stable-subject-a").await?;
        File::create(ready)?.sync_all()?;

        for _ in 0..1_000 {
            if std::fs::symlink_metadata(&stop).is_ok() {
                close_account(opened).await;
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        bail!("child lease test timed out waiting for its stop signal")
    }

    #[tokio::test]
    async fn account_legacy_lease_child_process() -> Result<()> {
        let Some(data_root) = std::env::var_os("GTUI_TEST_LEGACY_LEASE_DATA_ROOT") else {
            return Ok(());
        };
        let ready = std::env::var_os("GTUI_TEST_LEGACY_LEASE_READY")
            .context("child legacy lease test has no readiness path")?;
        let stop = std::env::var_os("GTUI_TEST_LEGACY_LEASE_STOP")
            .context("child legacy lease test has no stop path")?;
        let data_root = validate_root_directory(Path::new(&data_root), "test data root")?;
        let data_directory = prepare_data_directory(&data_root)?;
        let _lease = acquire_legacy_lease(&data_directory)?;
        File::create(ready)?.sync_all()?;

        for _ in 0..1_000 {
            if std::fs::symlink_metadata(&stop).is_ok() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        bail!("child legacy lease test timed out waiting for its stop signal")
    }

    #[tokio::test]
    async fn account_lease_rejects_independent_process() -> Result<()> {
        use std::process::Command;

        let directory = tempfile::tempdir()?;
        let ready = directory.path().join("child-ready");
        let stop = directory.path().join("child-stop");
        let mut child = Command::new(std::env::current_exe()?)
            .arg("db::tests::account_lease_child_process")
            .arg("--exact")
            .arg("--nocapture")
            .env("GTUI_TEST_LEASE_DIRECTORY", directory.path())
            .env("GTUI_TEST_LEASE_READY", &ready)
            .env("GTUI_TEST_LEASE_STOP", &stop)
            .spawn()
            .context("failed to spawn child lease holder")?;

        let mut child_ready = false;
        for _ in 0..1_000 {
            if std::fs::symlink_metadata(&ready).is_ok() {
                child_ready = true;
                break;
            }
            if child.try_wait()?.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let parent_check = async {
            if !child_ready {
                bail!("child lease holder did not signal readiness");
            }
            let error = match Database::open_account(
                directory.path(),
                directory.path(),
                "stable-subject-a",
            )
            .await
            {
                Ok(opened) => {
                    close_account(opened).await;
                    bail!("parent acquired a lease held by a child process");
                }
                Err(error) => error,
            };
            if !error.to_string().contains("account already open") {
                bail!("unexpected second-process lease error: {error:#}");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;

        File::create(&stop)?.sync_all()?;
        let status = child.wait().context("failed to join child lease holder")?;
        if !status.success() {
            bail!("child lease holder exited unsuccessfully: {status}");
        }
        parent_check
    }

    #[tokio::test]
    async fn account_legacy_lease_rejects_independent_process() -> Result<()> {
        use std::process::Command;

        let data_root = tempfile::tempdir()?;
        let legacy_root = tempfile::tempdir()?;
        let ready = data_root.path().join("legacy-child-ready");
        let stop = data_root.path().join("legacy-child-stop");
        let mut child = Command::new(std::env::current_exe()?)
            .arg("db::tests::account_legacy_lease_child_process")
            .arg("--exact")
            .arg("--nocapture")
            .env("GTUI_TEST_LEGACY_LEASE_DATA_ROOT", data_root.path())
            .env("GTUI_TEST_LEGACY_LEASE_READY", &ready)
            .env("GTUI_TEST_LEGACY_LEASE_STOP", &stop)
            .spawn()
            .context("failed to spawn child legacy lease holder")?;

        let mut child_ready = false;
        for _ in 0..1_000 {
            if std::fs::symlink_metadata(&ready).is_ok() {
                child_ready = true;
                break;
            }
            if child.try_wait()?.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let parent_check = async {
            if !child_ready {
                bail!("child legacy lease holder did not signal readiness");
            }
            let error = match Database::open_account(
                data_root.path(),
                legacy_root.path(),
                "stable-subject-b",
            )
            .await
            {
                Ok(opened) => {
                    close_account(opened).await;
                    bail!("parent acquired a legacy lease held by a child process");
                }
                Err(error) => error,
            };
            if !error
                .to_string()
                .contains("legacy cache handling is already in progress")
            {
                bail!("unexpected second-process legacy lease error: {error:#}");
            }
            if account_database_path(data_root.path(), "stable-subject-b").try_exists()? {
                bail!("account cache was created before acquiring the legacy lease");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;

        File::create(&stop)?.sync_all()?;
        let status = child
            .wait()
            .context("failed to join child legacy lease holder")?;
        if !status.success() {
            bail!("child legacy lease holder exited unsuccessfully: {status}");
        }
        parent_check
    }

    #[tokio::test]
    async fn migration_fresh_database_has_complete_schema_and_record() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        let (_fixture_directory, fixture_database) = temporary_database().await?;
        initialize_v0(&fixture_database).await?;

        database.run_migrations().await?;

        let migrated_v0_objects = schema_objects(&database)
            .await?
            .into_iter()
            .filter(|(_, _, table_name, _)| {
                table_name != "_sqlx_migrations" && table_name != "account_identity"
            })
            .collect::<Vec<_>>();
        assert_eq!(
            migrated_v0_objects,
            schema_objects(&fixture_database).await?
        );
        assert_eq!(
            object_names(&database, "table").await?,
            [
                "_sqlx_migrations",
                "account_identity",
                "labels",
                "message_labels",
                "messages",
                "messages_fts",
                "messages_fts_config",
                "messages_fts_data",
                "messages_fts_docsize",
                "messages_fts_idx",
            ]
        );
        assert_eq!(
            object_names(&database, "index").await?,
            [
                "idx_message_labels_label_id",
                "idx_messages_internal_date",
                "idx_messages_thread_id",
            ]
        );
        assert_eq!(
            object_names(&database, "trigger").await?,
            ["messages_ad", "messages_ai", "messages_au"]
        );
        assert_eq!(
            sqlx::query_as::<_, (i64, bool)>(
                "SELECT version, success FROM _sqlx_migrations ORDER BY version"
            )
            .fetch_all(&database.pool)
            .await?,
            [(1, true), (2, true)]
        );

        Ok(())
    }

    #[tokio::test]
    async fn migration_in_memory_database_remains_available() -> Result<()> {
        let database = Database::new("sqlite::memory:").await?;

        database.run_migrations().await?;

        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'messages')"
            )
            .fetch_one(&database.pool)
            .await?
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
                .fetch_one(&database.pool)
                .await?,
            2
        );

        Ok(())
    }

    #[tokio::test]
    async fn migration_second_run_is_idempotent() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        database.run_migrations().await?;
        let first_schema = schema_objects(&database).await?;

        database.run_migrations().await?;

        assert_eq!(schema_objects(&database).await?, first_schema);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
                .fetch_one(&database.pool)
                .await?,
            2
        );

        Ok(())
    }

    #[tokio::test]
    async fn migration_preserves_v0_data_and_fts_behavior() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        sqlx::query("INSERT INTO labels (id, name, type) VALUES ('INBOX', 'Inbox', 'system')")
            .execute(&database.pool)
            .await?;
        sqlx::query(
            "INSERT INTO messages
             (id, thread_id, subject, internal_date, body_plain)
             VALUES ('message-1', 'thread-1', 'MigrationToken', 1, 'kept body')",
        )
        .execute(&database.pool)
        .await?;
        sqlx::query(
            "INSERT INTO message_labels (message_id, label_id) VALUES ('message-1', 'INBOX')",
        )
        .execute(&database.pool)
        .await?;

        database.run_migrations().await?;

        assert_eq!(
            sqlx::query_as::<_, (String, String)>(
                "SELECT labels.name, messages.body_plain
                 FROM message_labels
                 JOIN labels ON labels.id = message_labels.label_id
                 JOIN messages ON messages.id = message_labels.message_id"
            )
            .fetch_one(&database.pool)
            .await?,
            ("Inbox".to_string(), "kept body".to_string())
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'MigrationToken'"
            )
            .fetch_one(&database.pool)
            .await?,
            1
        );

        sqlx::query("UPDATE messages SET subject = 'UpdatedToken' WHERE id = 'message-1'")
            .execute(&database.pool)
            .await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'UpdatedToken'"
            )
            .fetch_one(&database.pool)
            .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'MigrationToken'"
            )
            .fetch_one(&database.pool)
            .await?,
            0
        );

        Ok(())
    }

    #[tokio::test]
    async fn migration_accepts_whitespace_normalized_v0_schema() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        let normalized_v0 = V0_SCHEMA
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n");
        sqlx::query(&normalized_v0).execute(&database.pool).await?;

        database.run_migrations().await?;

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
                .fetch_one(&database.pool)
                .await?,
            2
        );

        Ok(())
    }

    #[tokio::test]
    async fn migration_accepts_analyzed_v0_schema() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        sqlx::query("ANALYZE").execute(&database.pool).await?;
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'sqlite_stat1')"
            )
            .fetch_one(&database.pool)
            .await?
        );

        database.run_migrations().await?;

        assert_eq!(ledger_rows(&database).await?.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn migration_v0_adoption_rebuilds_stale_fts() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        sqlx::query(
            "INSERT INTO messages (id, thread_id, subject, internal_date)
             VALUES ('message-1', 'thread-1', 'PreviouslyMissing', 1)",
        )
        .execute(&database.pool)
        .await?;
        sqlx::query("INSERT INTO messages_fts(messages_fts) VALUES('delete-all')")
            .execute(&database.pool)
            .await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'PreviouslyMissing'"
            )
            .fetch_one(&database.pool)
            .await?,
            0
        );

        database.run_migrations().await?;

        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'PreviouslyMissing'"
            )
            .fetch_one(&database.pool)
            .await?,
            1
        );

        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_empty_ledger_with_incompatible_schema_without_writes() -> Result<()>
    {
        let (_directory, database) = temporary_database().await?;
        let mut connection = database.pool.acquire().await?;
        connection.ensure_migrations_table().await?;
        drop(connection);
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY)")
            .execute(&database.pool)
            .await?;
        let before = schema_objects(&database).await?;

        let error = database
            .run_migrations()
            .await
            .expect_err("empty ledger bypassed adoption validation");

        assert!(
            error.to_string().contains("unsupported unversioned schema"),
            "unexpected error: {error:#}"
        );
        assert_eq!(schema_objects(&database).await?, before);
        assert!(ledger_rows(&database).await?.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn migration_empty_ledger_adopts_exact_v0_schema() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        let mut connection = database.pool.acquire().await?;
        connection.ensure_migrations_table().await?;
        drop(connection);

        database.run_migrations().await?;

        assert_eq!(ledger_rows(&database).await?.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_malformed_ledger_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        sqlx::query("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)")
            .execute(&database.pool)
            .await?;
        let before = schema_objects(&database).await?;

        let error = database
            .run_migrations()
            .await
            .expect_err("malformed ledger accepted");

        assert!(
            error.to_string().contains("unsupported migration ledger"),
            "unexpected error: {error:#}"
        );
        assert_eq!(schema_objects(&database).await?, before);

        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_failed_ledger_row_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        database.run_migrations().await?;
        sqlx::query("UPDATE _sqlx_migrations SET success = FALSE WHERE version = 1")
            .execute(&database.pool)
            .await?;
        let before_schema = schema_objects(&database).await?;
        let before_ledger = ledger_rows(&database).await?;

        let error = database
            .run_migrations()
            .await
            .expect_err("failed ledger row accepted");

        assert!(
            error.to_string().contains("unsupported migration ledger"),
            "unexpected error: {error:#}"
        );
        assert_eq!(schema_objects(&database).await?, before_schema);
        assert_eq!(ledger_rows(&database).await?, before_ledger);

        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_checksum_corruption_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        database.run_migrations().await?;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1")
            .execute(&database.pool)
            .await?;
        let before_schema = schema_objects(&database).await?;
        let before_ledger = ledger_rows(&database).await?;

        let error = database
            .run_migrations()
            .await
            .expect_err("checksum corruption accepted");

        assert!(
            error.to_string().contains("unsupported migration ledger"),
            "unexpected error: {error:#}"
        );
        assert_eq!(schema_objects(&database).await?, before_schema);
        assert_eq!(ledger_rows(&database).await?, before_ledger);

        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_applied_schema_damage_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        database.run_migrations().await?;
        sqlx::query("DROP TRIGGER messages_ai")
            .execute(&database.pool)
            .await?;

        assert_versioned_schema_rejected_without_writes(&database).await
    }

    #[tokio::test]
    async fn migration_rejects_applied_schema_change_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        database.run_migrations().await?;
        sqlx::query(
            "DROP INDEX idx_messages_thread_id;
             CREATE INDEX idx_messages_thread_id ON messages(subject);",
        )
        .execute(&database.pool)
        .await?;

        assert_versioned_schema_rejected_without_writes(&database).await
    }

    #[tokio::test]
    async fn migration_exclusive_boundary_blocks_competing_schema_writer() -> Result<()> {
        let (directory, database) = temporary_database().await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA journal_mode = WAL")
                .fetch_one(&database.pool)
                .await?,
            "wal"
        );
        let mut competing = SqliteConnection::connect(&temporary_database_url(&directory)).await?;
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&mut competing)
            .await?;
        let mut migration_connection = database.pool.acquire().await?.detach();

        let migration_result = async {
            acquire_exclusive_lock(&mut migration_connection).await?;
            validate_migration_state(&mut migration_connection).await?;

            let writer_error = match sqlx::query("CREATE TABLE competing_writer (id INTEGER)")
                .execute(&mut competing)
                .await
            {
                Ok(_) => bail!("competing schema writer bypassed exclusive migration lock"),
                Err(error) => error,
            };
            let error_code = writer_error
                .as_database_error()
                .and_then(|error| error.code());
            if error_code.as_deref() != Some("5") {
                bail!("competing schema writer failed for an unexpected reason: {writer_error}");
            }

            MIGRATOR
                .run_direct(&mut migration_connection)
                .await
                .context("failed to run database migrations")
        }
        .await;
        let close_result = migration_connection
            .close()
            .await
            .context("failed to close test migration connection");

        migration_result?;
        close_result?;
        sqlx::query("CREATE TABLE competing_writer (id INTEGER)")
            .execute(&mut competing)
            .await?;
        competing.close().await?;

        Ok(())
    }

    #[tokio::test]
    async fn migration_rejects_incompatible_messages_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        sqlx::query("CREATE TABLE messages (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL)")
            .execute(&database.pool)
            .await?;

        assert_unsupported_schema(&database).await
    }

    #[tokio::test]
    async fn migration_rejects_incompatible_trigger_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        sqlx::query(
            "DROP TRIGGER messages_ai;
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN SELECT 1; END;",
        )
        .execute(&database.pool)
        .await?;

        assert_unsupported_schema(&database).await
    }

    #[tokio::test]
    async fn migration_rejects_incompatible_index_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        sqlx::query(
            "DROP INDEX idx_messages_thread_id;
             CREATE INDEX idx_messages_thread_id ON messages(subject);",
        )
        .execute(&database.pool)
        .await?;

        assert_unsupported_schema(&database).await
    }

    #[tokio::test]
    async fn migration_rejects_incompatible_fts_without_writes() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        initialize_v0(&database).await?;
        sqlx::query(
            "DROP TABLE messages_fts;
             CREATE VIRTUAL TABLE messages_fts USING fts5(
                 subject,
                 content='messages',
                 content_rowid='rowid'
             );",
        )
        .execute(&database.pool)
        .await?;

        assert_unsupported_schema(&database).await
    }
}
