use crate::models;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use google_gmail1::Gmail;
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;
use inflections::case::to_title_case;

#[derive(Clone)]
pub struct GmailClient {
    hub: Gmail<HttpsConnector<HttpConnector>>,
    debug_logging: bool,
}

impl GmailClient {
    pub fn new(hub: Gmail<HttpsConnector<HttpConnector>>, debug_logging: bool) -> Self {
        Self { hub, debug_logging }
    }

    pub async fn get_signature(&self) -> Result<Option<String>> {
        let (_, aliases) = self
            .hub
            .users()
            .settings_send_as_list("me")
            .doit()
            .await
            .context("Failed to list send-as aliases")?;

        if let Some(alias_list) = aliases.send_as {
            // Find the primary alias
            if let Some(primary) = alias_list
                .into_iter()
                .find(|a| a.is_primary.unwrap_or(false))
            {
                return Ok(primary.signature.map(|s| convert_html_to_plain_text(&s)));
            }
        }
        Ok(None)
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
        label_ids: Vec<String>,
        max_results: u32,
        page_token: Option<String>,
    ) -> Result<(Vec<String>, Option<String>)> {
        let mut req = self
            .hub
            .users()
            .messages_list("me")
            .max_results(max_results);

        for label_id in &label_ids {
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

        let mut body_plain = None;
        if let Some(payload) = &msg.payload {
            body_plain = extract_text_body(payload, "text/plain");
        }

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
            is_read: !msg
                .label_ids
                .as_ref()
                .unwrap_or(&vec![])
                .contains(&"UNREAD".to_string()),
            has_sent_reply: msg
                .label_ids
                .unwrap_or_default()
                .contains(&"SENT".to_string()),
        })
    }

    #[allow(dead_code)]
    pub async fn trash_message(&self, id: &str) -> Result<()> {
        self.trash_messages(&[id.to_string()]).await
    }

    pub async fn trash_messages(&self, ids: &[String]) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Trashing messages: {:?}", ids));
        }
        let req = google_gmail1::api::BatchDeleteMessagesRequest {
            ids: Some(ids.to_vec()),
        };
        self.hub
            .users()
            .messages_batch_delete(req, "me")
            .doit()
            .await
            .context("Failed to trash messages")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn archive_message(&self, id: &str) -> Result<()> {
        self.archive_messages(&[id.to_string()]).await
    }

    pub async fn archive_messages(&self, ids: &[String]) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Archiving messages: {:?}", ids));
        }
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            remove_label_ids: Some(vec!["INBOX".to_string()]),
            add_label_ids: None,
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to archive messages")?;
        Ok(())
    }

    pub async fn remove_label_from_messages(&self, ids: &[String], label_id: &str) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Removing label {} from messages: {:?}", label_id, ids));
        }
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            remove_label_ids: Some(vec![label_id.to_string()]),
            add_label_ids: None,
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to remove label from messages")?;
        Ok(())
    }

    pub async fn add_label_to_message(&self, id: &str, label_id: &str) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Adding label {} to message: {}", label_id, id));
        }
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
        if self.debug_logging {
            self.debug_log(&format!("Untrashing message: {}", id));
        }
        self.hub
            .users()
            .messages_untrash("me", id)
            .doit()
            .await
            .context("Failed to untrash message")?;
        Ok(())
    }

    pub async fn unarchive_message(&self, id: &str) -> Result<()> {
        if self.debug_logging {
            self.debug_log(&format!("Unarchiving message: {}", id));
        }
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            add_label_ids: Some(vec!["INBOX".to_string()]),
            remove_label_ids: None,
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to unarchive message")?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
    ) -> Result<Option<String>> {
        let mut headers = vec![
            format!("From: me"),
            format!("To: {}", to),
            format!("Subject: {}", encode_header_value(subject)),
        ];

        if !cc.is_empty() {
            headers.push(format!("Cc: {}", cc));
        }
        if !bcc.is_empty() {
            headers.push(format!("Bcc: {}", bcc));
        }

        headers.push(format!("Content-Type: text/plain; charset=\"UTF-8\""));

        let raw_message = format!("{}\r\n\r\n{}", headers.join("\r\n"), body);

        // Logging for troubleshooting
        if self.debug_logging {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("gtui_debug.log")
            {
                use std::io::Write;
                let _ = writeln!(file, "--- SEND ATTEMPT ---");
                let _ = writeln!(file, "To: {}", to);
                let _ = writeln!(file, "Subject: {}", subject);
                let _ = writeln!(file, "Raw Message Body Length: {}", body.len());
            }
        }

        use std::io::Cursor;
        let cursor = Cursor::new(raw_message.into_bytes());

        let result = self
            .hub
            .users()
            .messages_send(google_gmail1::api::Message::default(), "me")
            .upload(cursor, "message/rfc822".parse().unwrap())
            .await;

        if self.debug_logging {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .append(true)
                .open("gtui_debug.log")
            {
                use std::io::Write;
                match &result {
                    Ok(_) => {
                        let _ = writeln!(file, "Result: SUCCESS");
                    }
                    Err(e) => {
                        let _ = writeln!(file, "Result: ERROR: {:?}", e);
                    }
                }
            }
        }

        let response = result.context("Failed to send message")?;
        
        // Return the sent message ID so it can be fetched and stored
        Ok(response.1.id)
    }

    pub async fn mark_as_read(&self, id: &str) -> Result<()> {
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            remove_label_ids: Some(vec!["UNREAD".to_string()]),
            add_label_ids: None,
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to mark message as read")?;
        Ok(())
    }

    pub async fn mark_as_unread(&self, id: &str) -> Result<()> {
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(vec![id.to_string()]),
            remove_label_ids: None,
            add_label_ids: Some(vec!["UNREAD".to_string()]),
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to mark message as unread")?;
        Ok(())
    }

    pub fn debug_log(&self, msg: &str) {
        if self.debug_logging {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("gtui_debug.log")
            {
                use std::io::Write;
                let _ = writeln!(file, "{}", msg);
            }
        }
    }
}

/// Encode a header value using RFC 2047 MIME encoded-word syntax if it contains non-ASCII characters.
/// This ensures proper handling of special characters like curly quotes in email subjects.
fn encode_header_value(value: &str) -> String {
    // Check if the string contains any non-ASCII characters
    if value.is_ascii() {
        return value.to_string();
    }
    
    // Use Base64 encoding for the header (RFC 2047)
    // Format: =?charset?encoding?encoded_text?=
    let encoded = general_purpose::STANDARD.encode(value.as_bytes());
    format!("=?UTF-8?B?{}?=", encoded)
}

fn convert_html_to_plain_text(html: &str) -> String {
    let mut text = html.to_string();

    // Replace line-breaking tags with newlines
    text = text.replace("<br>", "\n");
    text = text.replace("<br/>", "\n");
    text = text.replace("<br />", "\n");
    text = text.replace("</div>", "\n");
    text = text.replace("</p>", "\n\n");
    text = text.replace("</li>", "\n");

    // Strip all other tags
    let mut stripped = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            stripped.push(c);
        }
    }

    // Decode common HTML entities
    let decoded = stripped
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");

    // Clean up whitespace: collapse multiple newlines and trim
    let mut final_text = String::new();
    let mut last_was_newline = false;

    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_was_newline && !final_text.is_empty() {
                final_text.push('\n');
                last_was_newline = true;
            }
        } else {
            if last_was_newline && !final_text.is_empty() {
                // final_text.push('\n'); // already pushed one above
            }
            final_text.push_str(trimmed);
            final_text.push('\n');
            last_was_newline = false;
        }
    }

    final_text.trim().to_string()
}

fn extract_text_body(part: &google_gmail1::api::MessagePart, mime_type: &str) -> Option<String> {
    if let Some(mime) = &part.mime_type {
        if mime == mime_type {
            if let Some(body) = &part.body {
                if let Some(data) = &body.data {
                    use base64::{Engine as _, engine::general_purpose};
                    let data_str = String::from_utf8_lossy(data);

                    // Try decoding as base64url (Gmail's default)
                    let decoded = general_purpose::URL_SAFE_NO_PAD
                        .decode(data_str.trim().replace('-', "+").replace('_', "/"))
                        .or_else(|_| {
                            general_purpose::URL_SAFE
                                .decode(data_str.trim().replace('-', "+").replace('_', "/"))
                        })
                        .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(data_str.trim()))
                        .or_else(|_| general_purpose::STANDARD.decode(data_str.trim()));

                    match decoded {
                        Ok(bytes) => return String::from_utf8(bytes).ok(),
                        Err(_) => {
                            // If base64 decoding fails, it might already be raw content
                            return String::from_utf8(data.clone()).ok();
                        }
                    }
                }
            }
        }
    }

    if let Some(parts) = &part.parts {
        let mut full_body = String::new();
        for p in parts {
            if let Some(body) = extract_text_body(p, mime_type) {
                full_body.push_str(&body);
            }
        }
        if !full_body.is_empty() {
            return Some(full_body);
        }
    }

    None
}
