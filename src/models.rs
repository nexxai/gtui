use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Label {
    pub id: String,
    pub name: String,
    pub label_type: String, // 'system' or 'user'
    pub color_foreground: Option<String>,
    pub color_background: Option<String>,
    #[sqlx(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub snippet: Option<String>,
    pub from_address: Option<String>,
    pub to_address: Option<String>,
    pub subject: Option<String>,
    pub internal_date: i64,
    pub body_plain: Option<String>,
    pub body_html: Option<String>,
    pub is_read: bool,
}
