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
        let rows = sqlx::query(include_str!("../sql/get_messages_by_thread.sql"))
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
                has_sent_reply: false, // Not applicable for individual thread messages
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
            sqlx::query(include_str!("../sql/upsert_labels.sql"))
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
            sqlx::query(include_str!("../sql/upsert_messages.sql"))
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

            sqlx::query(include_str!("../sql/link_message_label.sql"))
                .bind(&msg.id)
                .bind(label_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn get_labels(&self) -> Result<Vec<models::Label>> {
        let rows = sqlx::query(include_str!("../sql/get_labels.sql"))
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
        let rows = sqlx::query(include_str!("../sql/get_messages_by_label.sql"))
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
                has_sent_reply: row.get(10),
            })
            .collect();

        Ok(messages)
    }

    pub async fn get_messages_with_dates_by_label(
        &self,
        label_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(include_str!("../sql/get_messages_with_dates_by_label.sql"))
            .bind(label_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
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
        let row = sqlx::query(include_str!("../sql/get_message_date.sql"))
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
        sqlx::query(include_str!("../sql/delete_message_labels.sql"))
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query(include_str!("../sql/delete_message.sql"))
            .bind(id)
            .execute(&self.pool)
            .await?;
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
