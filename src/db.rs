use crate::models;
use crate::sync::SyncStore;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use inflections::case::to_title_case;
use sqlx::migrate::Migrate;
use sqlx::sqlite::{SqliteConnection, SqlitePool};
use sqlx::{ConnectOptions, Connection};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
const V0_SCHEMA: &str = include_str!("../tests/fixtures/schema-v0.sql");

type SchemaObject = (String, String, String, Option<String>);
type LedgerRow = (i64, i64, Vec<u8>);

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
    in_memory: bool,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self> {
        use sqlx::sqlite::SqliteConnectOptions;
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);
        let in_memory = options
            .to_url_lossy()
            .query_pairs()
            .any(|(key, value)| key == "mode" && value == "memory");

        let pool = SqlitePool::connect_with(options).await?;
        Ok(Self { pool, in_memory })
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

    #[tokio::test]
    async fn migration_fresh_database_has_complete_schema_and_record() -> Result<()> {
        let (_directory, database) = temporary_database().await?;
        let (_fixture_directory, fixture_database) = temporary_database().await?;
        initialize_v0(&fixture_database).await?;

        database.run_migrations().await?;

        let migrated_v0_objects = schema_objects(&database)
            .await?
            .into_iter()
            .filter(|(_, _, table_name, _)| table_name != "_sqlx_migrations")
            .collect::<Vec<_>>();
        assert_eq!(
            migrated_v0_objects,
            schema_objects(&fixture_database).await?
        );
        assert_eq!(
            object_names(&database, "table").await?,
            [
                "_sqlx_migrations",
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
            [(1, true)]
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
            1
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
            1
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
            1
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

        assert_eq!(ledger_rows(&database).await?.len(), 1);

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

        assert_eq!(ledger_rows(&database).await?.len(), 1);

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
