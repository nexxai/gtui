use crate::logging;
use crate::models;
use crate::text::convert_html_to_plain_text;
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
            is_read: !has_label(msg.label_ids.as_ref(), "UNREAD"),
            has_sent_reply: has_label(msg.label_ids.as_ref(), "SENT"),
        })
    }

    #[allow(dead_code)]
    pub async fn trash_message(&self, id: &str) -> Result<()> {
        self.trash_messages(&[id.to_string()]).await
    }

    pub async fn trash_messages(&self, ids: &[String]) -> Result<()> {
        logging::debug(self.debug_logging, &format!("Trashing messages: {:?}", ids));
        logging::debug(
            self.debug_logging,
            &format!("Number of messages to trash: {}", ids.len()),
        );

        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            add_label_ids: Some(vec!["TRASH".to_string()]),
            remove_label_ids: None,
        };

        logging::debug(
            self.debug_logging,
            "About to call Gmail API messages_batch_modify to add TRASH label",
        );

        match self
            .hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
        {
            Ok(_response) => {
                logging::debug(self.debug_logging, "Gmail API call succeeded");
                Ok(())
            }
            Err(e) => {
                logging::debug(
                    self.debug_logging,
                    &format!("Gmail API call failed with error: {:?}", e),
                );
                Err(e).context("Failed to trash messages")
            }
        }
    }

    #[allow(dead_code)]
    pub async fn archive_message(&self, id: &str) -> Result<()> {
        self.archive_messages(&[id.to_string()]).await
    }

    pub async fn archive_messages(&self, ids: &[String]) -> Result<()> {
        logging::debug(
            self.debug_logging,
            &format!("Archiving messages: {:?}", ids),
        );
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

    pub async fn remove_labels_from_messages(
        &self,
        ids: &[String],
        label_ids: &[String],
    ) -> Result<()> {
        logging::debug(
            self.debug_logging,
            &format!("Removing labels {:?} from messages: {:?}", label_ids, ids),
        );
        let req = google_gmail1::api::BatchModifyMessagesRequest {
            ids: Some(ids.to_vec()),
            remove_label_ids: Some(label_ids.to_vec()),
            add_label_ids: None,
        };
        self.hub
            .users()
            .messages_batch_modify(req, "me")
            .doit()
            .await
            .context("Failed to remove labels from messages")?;
        Ok(())
    }

    pub async fn add_label_to_message(&self, id: &str, label_id: &str) -> Result<()> {
        logging::debug(
            self.debug_logging,
            &format!("Adding label {} to message: {}", label_id, id),
        );
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
        logging::debug(self.debug_logging, &format!("Untrashing message: {}", id));
        self.hub
            .users()
            .messages_untrash("me", id)
            .doit()
            .await
            .context("Failed to untrash message")?;
        Ok(())
    }

    pub async fn unarchive_message(&self, id: &str) -> Result<()> {
        logging::debug(self.debug_logging, &format!("Unarchiving message: {}", id));
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
        let raw_message = format!("{}\r\n\r\n{}", build_headers(to, cc, bcc, subject), body);

        // Logging for troubleshooting
        if self.debug_logging {
            logging::debug(self.debug_logging, "--- SEND ATTEMPT ---");
            logging::debug(self.debug_logging, &format!("To: {}", to));
            logging::debug(self.debug_logging, &format!("Subject: {}", subject));
            logging::debug(
                self.debug_logging,
                &format!("Raw Message Body Length: {}", body.len()),
            );
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
            let result_line = match &result {
                Ok(_) => "Result: SUCCESS".to_string(),
                Err(e) => format!("Result: ERROR: {:?}", e),
            };
            logging::debug(self.debug_logging, &result_line);
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
        logging::debug(self.debug_logging, msg);
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

                    return match decoded {
                        Ok(bytes) => String::from_utf8(bytes).ok(),
                        Err(_) => {
                            // If base64 decoding fails, it might already be raw content
                            String::from_utf8(data.clone()).ok()
                        }
                    };
                }
            }
        }
    }

    if let Some(parts) = &part.parts {
        let mut full_body = String::new();
        for p in parts {
            if let Some(body) = decode_body(p, mime_type) {
                full_body.push_str(&body);
            }
        }
        if !full_body.is_empty() {
            return Some(full_body);
        }
    }

    None
}

fn has_label(label_ids: Option<&Vec<String>>, label: &str) -> bool {
    label_ids
        .map(|ids| ids.contains(&label.to_string()))
        .unwrap_or(false)
}
