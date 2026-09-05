use crate::models;
use crate::sync::SyncStore;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fs2::FileExt;
use inflections::case::to_title_case;
use sha2::{Digest, Sha256};
#[cfg(test)]
use sqlx::ConnectOptions;
use sqlx::Connection;
use sqlx::migrate::Migrate;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
const V0_SCHEMA: &str = include_str!("../tests/fixtures/schema-v0.sql");
const LEGACY_DATABASE_NAME: &str = "gtui.db";

type SchemaObject = (String, String, String, Option<String>);
type LedgerRow = (i64, i64, Vec<u8>);

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
        let directory = directory.as_ref();
        let database_path = account_database_path(directory, account_subject);
        let lease = Arc::new(acquire_account_lease(&database_path)?);

        if !database_path
            .try_exists()
            .context("failed to inspect account cache path")?
        {
            create_account_database(&database_path, account_subject, lease.clone()).await?;
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
                quarantine_database(&database_path, "quarantine")
                    .context("failed to quarantine invalid account cache")?;
                return Err(error)
                    .context("account cache identity verification failed; cache quarantined");
            }
        };
        let legacy_quarantined =
            handle_legacy_database(directory).context("failed to handle unowned legacy cache")?;

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
        inspect_account_identity(path, account_subject).await?;
        let database = Self::connect_file(path, SqliteJournalMode::Wal, lease).await?;

        if let Err(error) = database.run_migrations().await {
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
        let migration_result = migrate_exclusively(&mut connection).await;
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

fn account_database_path(directory: &Path, account_subject: &str) -> PathBuf {
    let account_key = format!("{:x}", Sha256::digest(account_subject.as_bytes()));
    directory.join(format!("gtui-{account_key}.db"))
}

fn acquire_account_lease(database_path: &Path) -> Result<File> {
    let lock_path = database_path.with_extension("lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lease = options
        .open(lock_path)
        .context("failed to open account cache lease")?;

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

async fn inspect_account_identity(path: &Path, account_subject: &str) -> Result<()> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .context("failed to inspect account cache")?;
    let verification = verify_account_identity_connection(&mut connection, account_subject).await;
    let close = connection
        .close()
        .await
        .context("failed to close account cache identity inspection");

    verification?;
    close
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

fn handle_legacy_database(directory: &Path) -> Result<bool> {
    let path = directory.join(LEGACY_DATABASE_NAME);
    let artifacts = existing_database_artifacts(&path)?;
    if artifacts.is_empty() {
        return Ok(false);
    }

    let nonempty = artifacts.iter().try_fold(false, |nonempty, artifact| {
        Ok::<_, std::io::Error>(nonempty || artifact.metadata()?.len() != 0)
    })?;
    if nonempty {
        quarantine_database(&path, "unowned-backup")?;
        return Ok(true);
    }

    for artifact in artifacts {
        std::fs::remove_file(artifact).context("failed to remove empty legacy cache")?;
    }
    sync_parent(directory)?;
    Ok(false)
}

fn quarantine_database(database_path: &Path, suffix: &str) -> Result<bool> {
    let sources = existing_database_artifacts(database_path)?;
    if sources.is_empty() {
        return Ok(false);
    }
    let directory = database_path
        .parent()
        .context("cache path has no parent directory")?;

    for attempt in 0_u64.. {
        let destinations = sources
            .iter()
            .map(|source| quarantine_path(directory, source, attempt, suffix))
            .collect::<Result<Vec<_>>>()?;
        let mut linked = Vec::new();
        let mut collision = false;

        for (source, destination) in sources.iter().zip(&destinations) {
            match std::fs::hard_link(source, destination) {
                Ok(()) => linked.push(destination.clone()),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    collision = true;
                    break;
                }
                Err(error) => {
                    remove_links(&linked)?;
                    return Err(error).context("failed to quarantine cache artifact");
                }
            }
        }
        if collision {
            remove_links(&linked)?;
            continue;
        }

        for destination in &destinations {
            File::open(destination)?
                .sync_all()
                .context("failed to sync quarantined cache artifact")?;
        }
        for source in &sources {
            std::fs::remove_file(source).context("failed to remove quarantined cache artifact")?;
        }
        sync_parent(directory)?;
        return Ok(true);
    }

    unreachable!("unbounded quarantine suffix search exhausted")
}

fn existing_database_artifacts(database_path: &Path) -> Result<Vec<PathBuf>> {
    database_artifacts(database_path)
        .into_iter()
        .filter_map(|path| match path.try_exists() {
            Ok(true) => Some(Ok(path)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::io::Result<Vec<_>>>()
        .context("failed to inspect cache artifacts")
}

fn database_artifacts(database_path: &Path) -> [PathBuf; 3] {
    [
        database_path.to_path_buf(),
        sidecar_path(database_path, "-wal"),
        sidecar_path(database_path, "-shm"),
    ]
}

fn sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn quarantine_path(directory: &Path, source: &Path, attempt: u64, suffix: &str) -> Result<PathBuf> {
    let mut name = OsString::from(
        source
            .file_name()
            .context("cache artifact path has no file name")?,
    );
    name.push(format!(".{attempt}.{suffix}"));
    Ok(directory.join(name))
}

fn remove_links(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        std::fs::remove_file(path).context("failed to clean partial cache quarantine")?;
    }
    Ok(())
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

async fn migrate_exclusively(connection: &mut SqliteConnection) -> Result<()> {
    acquire_exclusive_lock(connection).await?;
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

    fn count_files_with_suffix(directory: &Path, suffix: &str) -> Result<usize> {
        Ok(directory
            .read_dir()?
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(suffix))
            .count())
    }

    #[tokio::test]
    async fn account_reopen_preserves_cache_and_other_account_is_isolated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject_a = "stable-subject-a";
        let subject_b = "stable-subject-b";
        let first = Database::open_account(directory.path(), subject_a).await?;
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
        assert_eq!(count_files_with_suffix(directory.path(), ".quarantine")?, 3);
        Ok(())
    }

    #[tokio::test]
    async fn account_ownerless_database_is_quarantined_not_claimed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
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
        assert_eq!(count_files_with_suffix(directory.path(), ".quarantine")?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn account_duplicate_identity_database_is_quarantined() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
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
        assert_eq!(count_files_with_suffix(directory.path(), ".quarantine")?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn account_malformed_database_is_quarantined() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let subject = "stable-subject-a";
        let path = account_database_path(directory.path(), subject);
        std::fs::write(&path, b"fake malformed sqlite fixture")?;

        assert!(
            Database::open_account(directory.path(), subject)
                .await
                .is_err()
        );
        assert!(!path.try_exists()?);
        assert_eq!(count_files_with_suffix(directory.path(), ".quarantine")?, 1);
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
        assert!(count_files_with_suffix(directory.path(), ".quarantine")? >= 1);
        Ok(())
    }

    #[tokio::test]
    async fn account_legacy_nonempty_database_and_sidecars_are_backed_up() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let legacy = directory.path().join(LEGACY_DATABASE_NAME);
        for artifact in database_artifacts(&legacy) {
            std::fs::write(artifact, b"fake legacy cache fixture")?;
        }
        let existing_backup = directory.path().join("gtui.db.0.unowned-backup");
        std::fs::write(&existing_backup, b"fake existing backup fixture")?;

        let opened = Database::open_account(directory.path(), "stable-subject-a").await?;

        assert!(opened.legacy_quarantined);
        assert!(existing_database_artifacts(&legacy)?.is_empty());
        assert_eq!(
            std::fs::read(existing_backup)?,
            b"fake existing backup fixture"
        );
        assert_eq!(
            count_files_with_suffix(directory.path(), ".unowned-backup")?,
            4
        );
        close_account(opened).await;
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
