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
const DATA_DIRECTORY_NAME: &str = ".gtui-data";
const QUARANTINE_MARKER_NAME: &str = ".quarantine-in-progress";
const QUARANTINE_MARKER_TEMP_NAME: &str = ".quarantine-in-progress.tmp";
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

type SchemaObject = (String, String, String, Option<String>);
type LedgerRow = (i64, i64, Vec<u8>);

#[derive(Debug, Deserialize, Serialize)]
struct QuarantineMarker {
    version: u8,
    source_database: PathBuf,
    artifacts: Vec<String>,
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

    pub async fn open_account(
        directory: impl AsRef<Path>,
        account_subject: &str,
    ) -> Result<AccountOpen> {
        validate_account_subject(account_subject)?;
        let directory = validate_runtime_directory(directory.as_ref())?;
        let data_directory = prepare_data_directory(&directory)?;
        recover_quarantines(&directory, &data_directory).await?;

        let database_path = account_database_path(&directory, account_subject);
        let artifacts = existing_database_artifacts(&database_path)?;
        let lease = Arc::new(acquire_account_lease(&database_path)?);

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
        let legacy_quarantined = handle_legacy_database(&directory, &data_directory)
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
        existing_database_artifacts(path)?;
        let database = Self::connect_file(path, SqliteJournalMode::Wal, lease).await?;

        if let Err(error) = database.run_account_migrations(account_subject).await {
            database.pool.close().await;
            return Err(error);
        }
        if let Err(error) = verify_account_identity(&database.pool, account_subject).await {
            database.pool.close().await;
            return Err(error);
        }

        Ok(database)
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

    async fn run_account_migrations(&self, account_subject: &str) -> Result<()> {
        self.run_migrations_for_account(Some(account_subject)).await
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

fn validate_runtime_directory(directory: &Path) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(directory)
        .context("failed to inspect the cache runtime directory")?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("cache runtime root must be a real directory, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.mode() & 0o022 != 0 {
            bail!("cache runtime root must not be writable by group or other users");
        }
    }

    directory
        .canonicalize()
        .context("failed to resolve the cache runtime directory")
}

fn prepare_data_directory(runtime_directory: &Path) -> Result<PathBuf> {
    let data_directory = runtime_directory.join(DATA_DIRECTORY_NAME);
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
                Ok(()) => sync_parent(runtime_directory)?,
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

fn account_database_path(directory: &Path, account_subject: &str) -> PathBuf {
    let account_key = format!("{:x}", Sha256::digest(account_subject.as_bytes()));
    directory
        .join(DATA_DIRECTORY_NAME)
        .join(format!("gtui-{account_key}.db"))
}

fn acquire_account_lease(database_path: &Path) -> Result<File> {
    let lock_path = database_path.with_extension("lock");
    validate_regular_file_if_present(&lock_path, "account cache lease")?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lease = options
        .open(&lock_path)
        .context("failed to open account cache lease")?;
    validate_regular_file_if_present(&lock_path, "account cache lease")?
        .context("account cache lease disappeared while it was being opened")?;

    if let Err(error) = FileExt::try_lock_exclusive(&lease) {
        if error.kind() == ErrorKind::WouldBlock {
            bail!("account already open; close the other gtui instance and retry");
        }
        return Err(error).context("failed to acquire account cache lease");
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
    let database =
        Database::connect_file(temporary.path(), SqliteJournalMode::Delete, lease).await?;

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

async fn handle_legacy_database(directory: &Path, data_directory: &Path) -> Result<bool> {
    let path = directory.join(LEGACY_DATABASE_NAME);
    let artifacts = existing_database_artifacts(&path)?;
    if artifacts.is_empty() {
        return Ok(false);
    }

    let nonempty = artifacts.iter().try_fold(false, |nonempty, artifact| {
        Ok::<_, anyhow::Error>(
            nonempty
                || validate_regular_file_if_present(artifact, "legacy cache artifact")?
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
        validate_regular_file_if_present(&artifact, "legacy cache artifact")?
            .context("legacy cache artifact disappeared before removal")?;
        std::fs::remove_file(artifact).context("failed to remove empty legacy cache")?;
    }
    sync_parent(directory)?;
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
    stop_after_moves: Option<usize>,
) -> Result<Option<PathBuf>> {
    let sources = existing_database_artifacts(database_path)?;
    if sources.is_empty() {
        return Ok(None);
    }
    require_quiescent_sqlite(database_path).await?;

    let backup_directory = create_private_backup_directory(data_directory, suffix)?;
    let marker = QuarantineMarker {
        version: 1,
        source_database: database_path.to_path_buf(),
        artifacts: sources
            .iter()
            .map(|source| {
                source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("cache artifact has no valid file name")
                    .map(str::to_owned)
            })
            .collect::<Result<_>>()?,
    };
    write_quarantine_marker(&backup_directory, &marker)?;
    resume_quarantine(&backup_directory, marker, stop_after_moves)?;

    Ok(Some(backup_directory))
}

fn existing_database_artifacts(database_path: &Path) -> Result<Vec<PathBuf>> {
    database_artifacts(database_path)
        .into_iter()
        .filter_map(
            |path| match validate_regular_file_if_present(&path, "SQLite cache artifact") {
                Ok(Some(_)) => Some(Ok(path)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            },
        )
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
) -> Result<Option<std::fs::Metadata>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {description}"));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("{description} must be a regular file, not a symlink or special file");
    }
    Ok(Some(metadata))
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

fn write_quarantine_marker(backup_directory: &Path, marker: &QuarantineMarker) -> Result<()> {
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
    drop(file);
    std::fs::rename(&temporary_path, &marker_path)
        .context("failed to install cache quarantine marker")?;
    sync_parent(backup_directory)?;
    sync_parent(
        backup_directory
            .parent()
            .context("cache backup directory has no parent")?,
    )
}

async fn recover_quarantines(runtime_directory: &Path, data_directory: &Path) -> Result<()> {
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
        let marker = read_quarantine_marker(&backup_directory)?;
        validate_quarantine_marker(&marker, runtime_directory, data_directory)?;

        let _lease = if marker.source_database.parent() == Some(data_directory) {
            Some(acquire_account_lease(&marker.source_database)?)
        } else {
            None
        };
        if validate_regular_file_if_present(&marker.source_database, "SQLite cache artifact")?
            .is_some()
        {
            require_quiescent_sqlite(&marker.source_database).await?;
        }
        resume_quarantine(&backup_directory, marker, None)?;
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

fn read_quarantine_marker(backup_directory: &Path) -> Result<QuarantineMarker> {
    let marker_path = backup_directory.join(QUARANTINE_MARKER_NAME);
    validate_regular_file_if_present(&marker_path, "cache quarantine marker")?
        .context("cache quarantine marker disappeared during recovery")?;
    let file = File::open(marker_path).context("failed to open cache quarantine marker")?;
    serde_json::from_reader(file).context("failed to decode cache quarantine marker")
}

fn validate_quarantine_marker(
    marker: &QuarantineMarker,
    runtime_directory: &Path,
    data_directory: &Path,
) -> Result<()> {
    if marker.version != 1 {
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
    let is_legacy = source_parent == runtime_directory && source_name == LEGACY_DATABASE_NAME;
    let is_account = source_parent == data_directory && is_account_database_name(source_name);
    if !is_legacy && !is_account {
        bail!("cache quarantine marker names an unexpected source");
    }

    let expected = database_artifacts(&marker.source_database)
        .into_iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .filter(|name| marker.artifacts.contains(name))
        .collect::<Vec<_>>();
    if marker.artifacts.is_empty() || marker.artifacts != expected {
        bail!("cache quarantine marker has an invalid artifact manifest");
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

fn resume_quarantine(
    backup_directory: &Path,
    marker: QuarantineMarker,
    stop_after_moves: Option<usize>,
) -> Result<()> {
    if stop_after_moves == Some(0) {
        bail!("injected quarantine interruption");
    }

    let source_directory = marker
        .source_database
        .parent()
        .context("quarantine marker source has no parent directory")?;
    let mut moved = 0;
    // The manifest stays immutable; each source/destination pair is the durable stage record.
    for source in database_artifacts(&marker.source_database) {
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .context("cache artifact has no valid name")?;
        let destination = backup_directory.join(name);
        let source_exists =
            validate_regular_file_if_present(&source, "SQLite cache artifact")?.is_some();
        let destination_exists =
            validate_regular_file_if_present(&destination, "quarantined cache artifact")?.is_some();

        if !marker.artifacts.iter().any(|artifact| artifact == name) {
            if source_exists || destination_exists {
                bail!("cache quarantine recovery found an untracked artifact");
            }
            continue;
        }

        match (source_exists, destination_exists) {
            (true, false) => {
                File::open(&source)
                    .context("failed to open cache artifact before quarantine")?
                    .sync_all()
                    .context("failed to sync cache artifact before quarantine")?;
                std::fs::rename(&source, &destination)
                    .context("failed to move cache artifact into quarantine")?;
                validate_regular_file_if_present(&destination, "quarantined cache artifact")?
                    .context("quarantined cache artifact disappeared after move")?;
                File::open(&destination)
                    .context("failed to open quarantined cache artifact")?
                    .sync_all()
                    .context("failed to sync quarantined cache artifact")?;
                sync_parent(source_directory)?;
                sync_parent(backup_directory)?;
                moved += 1;
                if stop_after_moves == Some(moved) {
                    bail!("injected quarantine interruption");
                }
            }
            (false, true) => {
                File::open(&destination)
                    .context("failed to reopen quarantined cache artifact")?
                    .sync_all()
                    .context("failed to resync quarantined cache artifact")?;
                sync_parent(source_directory)?;
                sync_parent(backup_directory)?;
            }
            (true, true) => {
                bail!("cache quarantine recovery refused to overwrite an existing artifact");
            }
            (false, false) => bail!("cache quarantine recovery found a missing artifact"),
        }
    }

    let marker_path = backup_directory.join(QUARANTINE_MARKER_NAME);
    validate_regular_file_if_present(&marker_path, "cache quarantine marker")?
        .context("cache quarantine marker disappeared before completion")?;
    std::fs::remove_file(marker_path).context("failed to complete cache quarantine")?;
    sync_parent(backup_directory)
}

async fn require_quiescent_sqlite(database_path: &Path) -> Result<()> {
    if !has_sqlite_header(database_path)? {
        return Ok(());
    }

    validate_regular_file_if_present(database_path, "SQLite cache artifact")?
        .context("SQLite cache artifact disappeared before exclusive check")?;
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(false);
    let mut connection = match SqliteConnection::connect_with(&options).await {
        Ok(connection) => connection,
        Err(error) if sqlite_error_is_invalid(&error) => return Ok(()),
        Err(error) if sqlite_error_is_busy(&error) => {
            bail!("cache is busy; close every process using it before retrying")
        }
        Err(error) => return Err(error).context("failed to open cache for exclusive check"),
    };
    let check = async {
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&mut connection)
            .await?;
        acquire_exclusive_lock(&mut connection).await?;
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_schema")
            .fetch_one(&mut connection)
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let close = connection.close().await;

    match check {
        Ok(()) => close.context("failed to close cache after exclusive check"),
        Err(error) if anyhow_error_is_sqlite_invalid(&error) => Ok(()),
        Err(error) if anyhow_error_is_sqlite_busy(&error) => {
            bail!("cache is busy; close every process using it before retrying")
        }
        Err(error) => Err(error).context("failed to establish exclusive cache access"),
    }
}

fn has_sqlite_header(path: &Path) -> Result<bool> {
    let metadata = validate_regular_file_if_present(path, "SQLite cache artifact")?
        .context("SQLite cache artifact disappeared during inspection")?;
    if metadata.len() < SQLITE_HEADER.len() as u64 {
        return Ok(false);
    }
    let mut header = [0_u8; SQLITE_HEADER.len()];
    File::open(path)
        .context("failed to open SQLite cache artifact for inspection")?
        .read_exact(&mut header)
        .context("failed to inspect SQLite cache artifact header")?;
    Ok(&header == SQLITE_HEADER)
}

fn sqlite_error_is_busy(error: &sqlx::Error) -> bool {
    sqlite_error_has_code(error, &[5, 6])
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
    anyhow_error_has_sqlite_code(error, &[5, 6])
}

fn anyhow_error_is_sqlite_invalid(error: &anyhow::Error) -> bool {
    anyhow_error_has_sqlite_code(error, &[11, 26])
}

fn anyhow_error_has_sqlite_code(error: &anyhow::Error, codes: &[i32]) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .is_some_and(|error| sqlite_error_has_code(error, codes))
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
        let runtime_directory = validate_runtime_directory(directory.path())?;
        prepare_data_directory(&runtime_directory)
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

    #[tokio::test]
    async fn account_reopen_preserves_cache_and_other_account_is_isolated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject_a = "stable-subject-a";
        let subject_b = "stable-subject-b";
        let first = Database::open_account(directory.path(), subject_a).await?;
        let data_directory = directory.path().join(DATA_DIRECTORY_NAME);
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

        let reopened = Database::open_account(directory.path(), subject_a).await?;
        assert_eq!(reopened.database.get_labels().await?.len(), 1);
        close_account(reopened).await;

        let other = Database::open_account(directory.path(), subject_b).await?;
        assert!(other.database.get_labels().await?.is_empty());
        assert_ne!(
            account_database_path(directory.path(), subject_a),
            account_database_path(directory.path(), subject_b)
        );
        close_account(other).await;

        Ok(())
    }

    #[tokio::test]
    async fn account_rejects_non_directory_runtime_and_non_regular_artifacts() -> Result<()> {
        let runtime_parent = tempfile::tempdir()?;
        let runtime_file = runtime_parent.path().join("runtime-file");
        File::create(&runtime_file)?;
        assert!(
            Database::open_account(&runtime_file, "stable-subject-a")
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

            let error = Database::open_account(directory.path(), "stable-subject-a")
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
            Database::open_account(&runtime_link, "stable-subject-a")
                .await
                .expect_err("symlinked runtime root was accepted")
                .to_string()
                .contains("real directory")
        );
        assert!(
            !runtime_target
                .path()
                .join(DATA_DIRECTORY_NAME)
                .try_exists()?
        );

        let data_runtime = tempfile::tempdir()?;
        let data_target = tempfile::tempdir()?;
        symlink(
            data_target.path(),
            data_runtime.path().join(DATA_DIRECTORY_NAME),
        )?;
        assert!(
            Database::open_account(data_runtime.path(), "stable-subject-a")
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

            let error = Database::open_account(directory.path(), "stable-subject-a")
                .await
                .expect_err("symlinked account artifact was accepted");
            assert!(error.to_string().contains("regular file"));
            assert_eq!(std::fs::read(target)?, b"must remain untouched");
        }

        Ok(())
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
        let error = Database::open_account(unsafe_runtime.path(), "stable-subject-a")
            .await
            .expect_err("group-writable runtime root was accepted");
        assert!(error.to_string().contains("must not be writable"));

        let unsafe_data = tempfile::tempdir()?;
        let data_directory = prepare_test_data(&unsafe_data)?;
        std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o750))?;
        let error = Database::open_account(unsafe_data.path(), "stable-subject-a")
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
        let opened = Database::open_account(directory.path(), "stable-subject-a").await?;
        let data_directory = directory.path().join(DATA_DIRECTORY_NAME);
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
        let first = Database::open_account(directory.path(), subject_a).await?;
        close_account(first).await;
        let path_a = account_database_path(directory.path(), subject_a);
        let path_b = account_database_path(directory.path(), subject_b);
        std::fs::rename(path_a, &path_b)?;

        let error = Database::open_account(directory.path(), subject_b)
            .await
            .expect_err("mismatched identity was accepted");

        assert!(error.to_string().contains("identity verification failed"));
        assert!(!path_b.try_exists()?);
        let data_directory = directory.path().join(DATA_DIRECTORY_NAME);
        let backup = only_backup_directory(&data_directory, "quarantine")?;
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
            Database::open_account(directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert_eq!(backup_directories(&data_directory, "quarantine")?.len(), 1);
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

        let error = Database::open_account(directory.path(), subject)
            .await
            .expect_err("ownerless cache was migrated before identity verification");
        assert!(error.to_string().contains("identity verification failed"));

        let backup = only_backup_directory(&data_directory, "quarantine")?;
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
            Database::open_account(directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert_eq!(backup_directories(&data_directory, "quarantine")?.len(), 1);
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
            Database::open_account(directory.path(), subject)
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
        let opened = Database::open_account(directory.path(), subject).await?;
        close_account(opened).await;
        let path = account_database_path(directory.path(), subject);
        let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path)).await?;
        sqlx::query("DROP TRIGGER messages_ai")
            .execute(&pool)
            .await?;
        pool.close().await;

        assert!(
            Database::open_account(directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert_eq!(
            backup_directories(&directory.path().join(DATA_DIRECTORY_NAME), "quarantine")?.len(),
            1
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

        let opened = Database::open_account(directory.path(), "stable-subject-a").await?;

        assert!(opened.legacy_quarantined);
        assert!(existing_database_artifacts(&legacy)?.is_empty());
        let data_directory = directory.path().join(DATA_DIRECTORY_NAME);
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
    async fn account_quarantine_recovers_every_rename_crash_point() -> Result<()> {
        for stop_after_moves in 0..=database_artifacts(Path::new("gtui.db")).len() {
            let directory = tempfile::tempdir()?;
            let runtime_directory = validate_runtime_directory(directory.path())?;
            let data_directory = prepare_data_directory(&runtime_directory)?;
            let legacy = runtime_directory.join(LEGACY_DATABASE_NAME);
            for (index, artifact) in database_artifacts(&legacy).into_iter().enumerate() {
                std::fs::write(artifact, format!("artifact {index}"))?;
            }

            let error = start_quarantine(
                &legacy,
                &data_directory,
                "unowned-backup",
                Some(stop_after_moves),
            )
            .await
            .expect_err("quarantine crash was not injected");
            assert!(
                error
                    .to_string()
                    .contains("injected quarantine interruption")
            );

            recover_quarantines(&runtime_directory, &data_directory).await?;
            assert!(existing_database_artifacts(&legacy)?.is_empty());
            let backup = only_backup_directory(&data_directory, "unowned-backup")?;
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
    async fn account_quarantine_recovery_never_overwrites() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime_directory = validate_runtime_directory(directory.path())?;
        let data_directory = prepare_data_directory(&runtime_directory)?;
        let legacy = runtime_directory.join(LEGACY_DATABASE_NAME);
        std::fs::write(&legacy, b"original")?;

        start_quarantine(&legacy, &data_directory, "unowned-backup", Some(1))
            .await
            .expect_err("quarantine crash was not injected");
        std::fs::write(&legacy, b"replacement")?;

        let error = recover_quarantines(&runtime_directory, &data_directory)
            .await
            .expect_err("quarantine recovery overwrote an artifact");
        assert!(error.to_string().contains("refused to overwrite"));
        let backup = only_backup_directory(&data_directory, "unowned-backup")?;
        assert_eq!(
            std::fs::read(backup.join(LEGACY_DATABASE_NAME))?,
            b"original"
        );
        assert_eq!(std::fs::read(legacy)?, b"replacement");
        assert!(backup.join(QUARANTINE_MARKER_NAME).is_file());
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

        let error = Database::open_account(directory.path(), "stable-subject-a")
            .await
            .expect_err("busy legacy cache was moved");
        assert!(format!("{error:#}").contains("cache is busy"));
        assert!(directory.path().join(LEGACY_DATABASE_NAME).is_file());
        assert!(
            backup_directories(
                &directory.path().join(DATA_DIRECTORY_NAME),
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

        let opened = Database::open_account(directory.path(), "stable-subject-a").await?;

        assert!(!opened.legacy_quarantined);
        assert!(!legacy.try_exists()?);
        assert!(account_database_path(directory.path(), "stable-subject-a").try_exists()?);
        close_account(opened).await;
        Ok(())
    }

    #[tokio::test]
    async fn account_lease_rejects_second_open() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first = Database::open_account(directory.path(), "stable-subject-a").await?;

        let error = Database::open_account(directory.path(), "stable-subject-a")
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
        let opened = Database::open_account(PathBuf::from(directory), "stable-subject-a").await?;
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
            let error = match Database::open_account(directory.path(), "stable-subject-a").await {
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
