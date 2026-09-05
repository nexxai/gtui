use crate::models;
use crate::sync::SyncStore;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use inflections::case::to_title_case;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
const V0_SCHEMA: &str = include_str!("../tests/fixtures/schema-v0.sql");

type SchemaObject = (String, String, String, Option<String>);

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self> {
        use sqlx::sqlite::SqliteConnectOptions;
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);

        let pool = SqlitePool::connect_with(options).await?;
        Ok(Self { pool })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        let is_versioned = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type = 'table' AND name = '_sqlx_migrations'
            )",
        )
        .fetch_one(&self.pool)
        .await
        .context("failed to inspect database migration state")?;

        if !is_versioned {
            self.verify_unversioned_schema().await?;
        }

        MIGRATOR
            .run(&self.pool)
            .await
            .context("failed to run database migrations")?;
        Ok(())
    }

    async fn verify_unversioned_schema(&self) -> Result<()> {
        let actual = schema_objects(&self.pool).await?;
        if actual.is_empty() {
            return Ok(());
        }

        let expected_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .context("failed to prepare known v0 schema")?;
        sqlx::query(V0_SCHEMA)
            .execute(&expected_pool)
            .await
            .context("failed to prepare known v0 schema")?;

        if actual != schema_objects(&expected_pool).await? {
            bail!(
                "unsupported unversioned schema: expected an empty database or the exact gtui v0 schema; back up the cache and restore a compatible schema or remove it to re-sync"
            );
        }

        Ok(())
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

async fn schema_objects(pool: &SqlitePool) -> Result<Vec<SchemaObject>> {
    let objects = sqlx::query_as::<_, SchemaObject>(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_schema
         ORDER BY type, name",
    )
    .fetch_all(pool)
    .await
    .context("failed to inspect unversioned database schema")?;

    Ok(objects
        .into_iter()
        .map(|(object_type, name, table_name, sql)| {
            let sql = sql.map(|sql| sql.split_whitespace().collect::<Vec<_>>().join(" "));
            (object_type, name, table_name, sql)
        })
        .collect())
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
    use tempfile::TempDir;

    async fn temporary_database() -> Result<(TempDir, Database)> {
        let directory = tempfile::tempdir()?;
        let database_path = directory.path().join("gtui.db");
        let database_url = format!("sqlite://{}", database_path.display());
        let database = Database::new(&database_url).await?;

        Ok((directory, database))
    }

    async fn initialize_v0(database: &Database) -> Result<()> {
        sqlx::query(V0_SCHEMA).execute(&database.pool).await?;
        Ok(())
    }

    async fn schema_objects(database: &Database) -> Result<Vec<SchemaObject>> {
        super::schema_objects(&database.pool).await
    }

    async fn object_names(database: &Database, object_type: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT name
             FROM sqlite_schema
             WHERE type = ? AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .bind(object_type)
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
