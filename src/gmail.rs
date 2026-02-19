use crate::models;
use crate::text::convert_html_to_plain_text;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use google_gmail1::Gmail;
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;
use inflections::case::to_title_case;
use tracing::debug;

#[derive(Clone)]
pub struct GmailClient {
    hub: Gmail<HttpsConnector<HttpConnector>>,
}

impl GmailClient {
    pub fn new(hub: Gmail<HttpsConnector<HttpConnector>>) -> Self {
        Self { hub }
    }

    pub async fn get_signature(&self) -> Result<Option<String>> {
        let (_, aliases) = self
            .hub
            .users()
            .settings_send_as_list("me")
            .doit()
            .await
            .context("Failed to list send-as aliases")?;

        let primary = aliases
            .send_as
            .into_iter()
            .flatten()
            .find(|a| a.is_primary.unwrap_or(false));

        Ok(primary.and_then(|a| a.signature.map(|s| convert_html_to_plain_text(&s))))
    }

    pub async fn list_labels(&self) -> Result<Vec<models::Label>> {
        let (_, label_list) = self
            .hub
            .users()
            .labels_list("me")
            .doit()
            .await
            .context("Failed to list labels")?;

        let labels = label_list
            .labels
            .unwrap_or_default()
            .into_iter()
            .map(|l| models::Label {
                id: l.id.unwrap_or_default(),
                name: l.name.clone().unwrap_or_default(),
                label_type: l.type_.unwrap_or_default(),
                color_foreground: l.color.as_ref().and_then(|c| c.text_color.clone()),
                color_background: l.color.as_ref().and_then(|c| c.background_color.clone()),
                display_name: to_title_case(l.name.as_deref().unwrap_or_default()),
            })
            .collect();

        Ok(labels)
    }

    pub async fn list_messages(
        &self,
        label_ids: &[String],
        max_results: u32,
        page_token: Option<String>,
    ) -> Result<(Vec<String>, Option<String>)> {
        let mut req = self
            .hub
            .users()
            .messages_list("me")
            .max_results(max_results);

        for label_id in label_ids {
            req = req.add_label_ids(label_id);
        }

        if let Some(token) = &page_token {
            req = req.page_token(token);
        }

        let (_, message_list) = req.doit().await.context("Failed to list messages")?;

        let ids = message_list
            .messages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.id)
            .collect();

        Ok((ids, message_list.next_page_token))
    }

    pub async fn get_message(&self, id: &str) -> Result<models::Message> {
        let (_, msg) = self
            .hub
            .users()
            .messages_get("me", id)
            .format("full")
            .doit()
            .await
            .context(format!("Failed to get message {}", id))?;

        let (from, to, subject) = msg.payload.as_ref().map(parse_headers).unwrap_or_default();
        let internal_date = msg.internal_date.unwrap_or(0);

        let body_plain = msg
            .payload
            .as_ref()
            .and_then(|payload| decode_body(payload, "text/plain"));

        Ok(models::Message {
            id: msg.id.unwrap_or_default(),
            thread_id: msg.thread_id.unwrap_or_default(),
            snippet: msg.snippet,
            from_address: from,
            to_address: to,
            subject,
            internal_date,
            body_plain,
            body_html: None,
            is_read: !has_label(msg.label_ids.as_deref(), "UNREAD"),
            has_sent_reply: has_label(msg.label_ids.as_deref(), "SENT"),
        })
    }

    #[allow(dead_code)]
    pub async fn trash_message(&self, id: &str) -> Result<()> {
        self.trash_messages(&[id.to_string()]).await
    }

    pub async fn trash_messages(&self, ids: &[String]) -> Result<()> {
        debug!(?ids, count = ids.len(), "trashing messages");

        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            add_label_ids: Some(vec!["TRASH".to_string()]),
            remove_label_ids: None,
        };

        self.batch_modify(req)
            .await
            .context("Failed to trash messages")
    }

    #[allow(dead_code)]
    pub async fn archive_message(&self, id: &str) -> Result<()> {
        self.archive_messages(&[id.to_string()]).await
    }

    pub async fn archive_messages(&self, ids: &[String]) -> Result<()> {
        debug!(?ids, "archiving messages");

        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            remove_label_ids: Some(vec!["INBOX".to_string()]),
            add_label_ids: None,
        };

        self.batch_modify(req)
            .await
            .context("Failed to archive messages")
    }

    pub async fn remove_labels_from_messages(
        &self,
        ids: &[String],
        label_ids: &[String],
    ) -> Result<()> {
        debug!(?label_ids, ?ids, "removing labels from messages");

        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            remove_label_ids: Some(label_ids.to_vec()),
            add_label_ids: None,
        };

        self.batch_modify(req)
            .await
            .context("Failed to remove labels from messages")
    }

    pub async fn add_label_to_message(&self, id: &str, label_id: &str) -> Result<()> {
        debug!(id, label_id, "adding label to message");

        let req = google_gmail1::api::ModifyMessageRequest {
            add_label_ids: Some(vec![label_id.to_string()]),
            remove_label_ids: None,
        };

        self.hub
            .users()
            .messages_modify(req, "me", id)
            .doit()
            .await
            .context("Failed to add label to message")?;

        Ok(())
    }

    pub async fn untrash_message(&self, id: &str) -> Result<()> {
        debug!(id, "untrashing message");

        self.hub
            .users()
            .messages_untrash("me", id)
            .doit()
            .await
            .context("Failed to untrash message")?;

        Ok(())
    }

    pub async fn unarchive_message(&self, id: &str) -> Result<()> {
        debug!(id, "unarchiving message");

        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            add_label_ids: Some(vec!["INBOX".to_string()]),
            remove_label_ids: None,
        };

        self.batch_modify(req)
            .await
            .context("Failed to unarchive message")
    }

    pub async fn send_message(
        &self,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
    ) -> Result<Option<String>> {
        debug!(to, subject, body_len = body.len(), "sending message");

        let raw_message = format!("{}\r\n\r\n{}", build_headers(to, cc, bcc, subject), body);

        use std::io::Cursor;
        let cursor = Cursor::new(raw_message.into_bytes());

        let result = self
            .hub
            .users()
            .messages_send(google_gmail1::api::Message::default(), "me")
            .upload(cursor, "message/rfc822".parse().unwrap())
            .await;

        match &result {
            Ok(_) => debug!("send succeeded"),
            Err(e) => debug!(?e, "send failed"),
        }

        let response = result.context("Failed to send message")?;
        Ok(response.1.id)
    }

    pub async fn mark_as_read(&self, id: &str) -> Result<()> {
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            remove_label_ids: Some(vec!["UNREAD".to_string()]),
            add_label_ids: None,
        };

        self.batch_modify(req)
            .await
            .context("Failed to mark message as read")
    }

    pub async fn mark_as_unread(&self, id: &str) -> Result<()> {
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            remove_label_ids: None,
            add_label_ids: Some(vec!["UNREAD".to_string()]),
        };

        self.batch_modify(req)
            .await
            .context("Failed to mark message as unread")
    }

    // -- private helpers --

    async fn batch_modify(
        &self,
        req: google_gmail1::api::BatchModifyMessagesRequest,
    ) -> Result<()> {
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await?;

        Ok(())
    }
}

/// Encode a header value using RFC 2047 MIME encoded-word syntax if it contains non-ASCII characters.
fn encode_header_value(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }

    let encoded = general_purpose::STANDARD.encode(value.as_bytes());
    format!("=?UTF-8?B?{}?=", encoded)
}

fn build_headers(to: &str, cc: &str, bcc: &str, subject: &str) -> String {
    let mut headers = vec![
        "From: me".to_string(),
        format!("To: {}", to),
        format!("Subject: {}", encode_header_value(subject)),
    ];

    if !cc.is_empty() {
        headers.push(format!("Cc: {}", cc));
    }
    if !bcc.is_empty() {
        headers.push(format!("Bcc: {}", bcc));
    }

    headers.push("Content-Type: text/plain; charset=\"UTF-8\"".to_string());
    headers.join("\r\n")
}

fn parse_headers(
    payload: &google_gmail1::api::MessagePart,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut from = None;
    let mut to = None;
    let mut subject = None;

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

    (from, to, subject)
}

fn decode_body(part: &google_gmail1::api::MessagePart, mime_type: &str) -> Option<String> {
    // Check if this part matches the desired MIME type
    if part.mime_type.as_deref() == Some(mime_type)
        && let Some(data) = part.body.as_ref().and_then(|b| b.data.as_ref())
    {
        let data_str = String::from_utf8_lossy(data);

        // Gmail uses URL-safe base64 encoding
        let decoded = general_purpose::URL_SAFE_NO_PAD
            .decode(data_str.trim())
            .or_else(|_| general_purpose::URL_SAFE.decode(data_str.trim()))
            .or_else(|_| general_purpose::STANDARD.decode(data_str.trim()));

        return match decoded {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            Err(_) => String::from_utf8(data.clone()).ok(),
        };
    }

    // Recurse into sub-parts
    let full_body: String = part
        .parts
        .iter()
        .flatten()
        .filter_map(|p| decode_body(p, mime_type))
        .collect();

    (!full_body.is_empty()).then_some(full_body)
}

fn has_label(label_ids: Option<&[String]>, label: &str) -> bool {
    label_ids.is_some_and(|ids| ids.iter().any(|id| id == label))
}
