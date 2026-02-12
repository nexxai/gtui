use crate::models;
use anyhow::Result;
use inflections::case::to_title_case;
use sqlx::{Row, sqlite::SqlitePool};

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

    pub async fn get_messages_by_thread(&self, thread_id: &str) -> Result<Vec<models::Message>> {
        let rows = sqlx::query(
            "SELECT id, thread_id, snippet, from_address, to_address, subject, internal_date, body_plain, body_html, is_read 
             FROM messages 
             WHERE thread_id = ?
             ORDER BY internal_date DESC"
        )
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;

        let messages = rows
            .into_iter()
            .map(|row| models::Message {
                id: row.get(0),
                thread_id: row.get(1),
                snippet: row.get(2),
                from_address: row.get(3),
                to_address: row.get(4),
                subject: row.get(5),
                internal_date: row.get(6),
                body_plain: row.get(7),
                body_html: row.get(8),
                is_read: row.get(9),
            })
            .collect();

        Ok(messages)
    }

    pub async fn run_migrations(&self) -> Result<()> {
        let schema = include_str!("../schema.sql");
        sqlx::query(schema).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn upsert_labels(&self, labels: &[models::Label]) -> Result<()> {
        for label in labels {
            sqlx::query(
                "INSERT INTO labels (id, name, type, color_foreground, color_background) 
                 VALUES (?, ?, ?, ?, ?) 
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name, type=excluded.type, 
                 color_foreground=excluded.color_foreground, color_background=excluded.color_background"
            )
            .bind(&label.id)
            .bind(&label.name)
            .bind(&label.label_type)
            .bind(&label.color_foreground)
            .bind(&label.color_background)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn upsert_messages(
        &self,
        messages: &[models::Message],
        label_id: &str,
    ) -> Result<()> {
        for msg in messages {
            sqlx::query(
                "INSERT INTO messages (id, thread_id, snippet, from_address, to_address, subject, internal_date, body_plain, body_html, is_read) 
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) 
                 ON CONFLICT(id) DO UPDATE SET snippet=excluded.snippet, is_read=excluded.is_read, 
                 body_plain=excluded.body_plain, body_html=excluded.body_html"
            )
            .bind(&msg.id)
            .bind(&msg.thread_id)
            .bind(&msg.snippet)
            .bind(&msg.from_address)
            .bind(&msg.to_address)
            .bind(&msg.subject)
            .bind(&msg.internal_date)
            .bind(&msg.body_plain)
            .bind(&msg.body_html)
            .bind(msg.is_read)
            .execute(&self.pool)
            .await?;

            sqlx::query(
                "INSERT OR IGNORE INTO message_labels (message_id, label_id) VALUES (?, ?)",
            )
            .bind(&msg.id)
            .bind(label_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn get_labels(&self) -> Result<Vec<models::Label>> {
        let rows = sqlx::query(
            "SELECT id, name, type as label_type, color_foreground, color_background FROM labels ORDER BY name ASC"
        )
        .fetch_all(&self.pool)
        .await?;

        let mut labels: Vec<models::Label> = rows
            .into_iter()
            .map(|row| models::Label {
                id: row.get(0),
                name: row.get(1),
                label_type: row.get(2),
                color_foreground: row.get(3),
                color_background: row.get(4),
                display_name: to_title_case(&row.get::<'_, String, _>(1)),
            })
            .collect();

        // Priority sorting: Put INBOX at the top
        labels.sort_by(|a, b| {
            if a.id == "INBOX" {
                std::cmp::Ordering::Less
            } else if b.id == "INBOX" {
                std::cmp::Ordering::Greater
            } else {
                a.name.cmp(&b.name)
            }
        });

        Ok(labels)
    }

    pub async fn get_messages_by_label(
        &self,
        label_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<models::Message>> {
        let rows = sqlx::query(
            "SELECT m.id, m.thread_id, m.snippet, m.from_address, m.to_address, m.subject, MAX(m.internal_date) as latest_date, m.body_plain, m.body_html, m.is_read 
             FROM messages m
             JOIN message_labels ml ON m.id = ml.message_id
             WHERE ml.label_id = ?
             GROUP BY m.thread_id
             ORDER BY latest_date DESC
             LIMIT ? OFFSET ?"
        )
        .bind(label_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let messages = rows
            .into_iter()
            .map(|row| models::Message {
                id: row.get(0),
                thread_id: row.get(1),
                snippet: row.get(2),
                from_address: row.get(3),
                to_address: row.get(4),
                subject: row.get(5),
                internal_date: row.get(6),
                body_plain: row.get(7),
                body_html: row.get(8),
                is_read: row.get(9),
            })
            .collect();

        Ok(messages)
    }

    pub async fn get_messages_with_dates_by_label(
        &self,
        label_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT m.id, m.internal_date 
             FROM messages m
             JOIN message_labels ml ON m.id = ml.message_id
             WHERE ml.label_id = ?
             ORDER BY m.internal_date DESC
             LIMIT ?",
        )
        .bind(label_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn mark_message_as_read(&self, id: &str, is_read: bool) -> Result<()> {
        sqlx::query("UPDATE messages SET is_read = ? WHERE id = ?")
            .bind(is_read)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn message_exists(&self, id: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM messages WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub async fn get_message_date(&self, id: &str) -> Result<Option<i64>> {
        let row = sqlx::query("SELECT internal_date FROM messages WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        if let Some(r) = row {
            Ok(Some(r.get(0)))
        } else {
            Ok(None)
        }
    }

    pub async fn delete_message(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM message_labels WHERE message_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_label_from_message(&self, message_id: &str, label_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM message_labels WHERE message_id = ? AND label_id = ?")
            .bind(message_id)
            .bind(label_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
