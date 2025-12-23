use anyhow::{Result, Context};
use google_gmail1::Gmail;
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;
use crate::models;

#[derive(Clone)]
pub struct GmailClient {
    hub: Gmail<HttpsConnector<HttpConnector>>,
}

impl GmailClient {
    pub fn new(hub: Gmail<HttpsConnector<HttpConnector>>) -> Self {
        Self { hub }
    }

    pub async fn list_labels(&self) -> Result<Vec<models::Label>> {
        let (_, label_list) = self.hub.users()
            .labels_list("me")
            .doit()
            .await
            .context("Failed to list labels")?;

        let labels = label_list.labels.unwrap_or_default()
            .into_iter()
            .map(|l| models::Label {
                id: l.id.unwrap_or_default(),
                name: l.name.unwrap_or_default(),
                label_type: l.type_.unwrap_or_default(),
                color_foreground: l.color.as_ref().and_then(|c| c.text_color.clone()),
                color_background: l.color.as_ref().and_then(|c| c.background_color.clone()),
            })
            .collect();

        Ok(labels)
    }

    pub async fn list_messages(&self, label_ids: Vec<String>, max_results: u32, page_token: Option<String>) -> Result<(Vec<String>, Option<String>)> {
        let mut req = self.hub.users().messages_list("me").max_results(max_results);
        
        for label_id in &label_ids {
            req = req.add_label_ids(label_id);
        }
        
        if let Some(token) = &page_token {
            req = req.page_token(token);
        }

        let (_, message_list) = req.doit().await.context("Failed to list messages")?;
        
        let ids = message_list.messages.unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.id)
            .collect();

        Ok((ids, message_list.next_page_token))
    }

    pub async fn get_message(&self, id: &str) -> Result<models::Message> {
        let (_, msg) = self.hub.users().messages_get("me", id)
            .format("full")
            .doit()
            .await
            .context(format!("Failed to get message {}", id))?;

        let mut from = None;
        let mut to = None;
        let mut subject = None;
        let internal_date = msg.internal_date.unwrap_or(0);

        if let Some(payload) = &msg.payload {
            if let Some(headers) = &payload.headers {
                for header in headers {
                    match header.name.as_deref() {
                        Some("From") => from = header.value.clone(),
                        Some("To") => to = header.value.clone(),
                        Some("Subject") => subject = header.value.clone(),
                        _ => {}
                    }
                }
            }
        }

        Ok(models::Message {
            id: msg.id.unwrap_or_default(),
            thread_id: msg.thread_id.unwrap_or_default(),
            snippet: msg.snippet,
            from_address: from,
            to_address: to,
            subject,
            internal_date,
            body_plain: None,
            body_html: None,
            is_read: !msg.label_ids.unwrap_or_default().contains(&"UNREAD".to_string()),
        })
    }

    pub async fn trash_message(&self, id: &str) -> Result<()> {
        self.hub.users().messages_trash("me", id)
            .doit()
            .await
            .context("Failed to trash message")?;
        Ok(())
    }

    pub async fn archive_message(&self, id: &str) -> Result<()> {
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            remove_label_ids: Some(vec!["INBOX".to_string()]),
            add_label_ids: None,
        };
        self.hub.users().messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to archive message")?;
        Ok(())
    }

    pub async fn send_message(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        let raw_message = format!(
            "To: {}\r\nSubject: {}\r\n\r\n{}",
            to, subject, body
        );
        use base64::{Engine as _, engine::general_purpose};
        let encoded = general_purpose::URL_SAFE_NO_PAD.encode(raw_message);
        
        let msg = google_gmail1::api::Message {
            ..Default::default()
        };

        use std::io::Cursor;
        let cursor = Cursor::new(encoded);

        let _ = self.hub.users().messages_send(msg, "me")
            .upload(cursor, "message/rfc822".parse().unwrap())
            .await
            .context("Failed to send message")?;
        
        Ok(())
    }

    pub async fn mark_as_read(&self, id: &str) -> Result<()> {
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            remove_label_ids: Some(vec!["UNREAD".to_string()]),
            add_label_ids: None,
        };
        self.hub.users().messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to mark message as read")?;
        Ok(())
    }
}
