INSERT INTO messages (id, thread_id, snippet, from_address, to_address, subject, internal_date, body_plain, body_html, is_read)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET snippet=excluded.snippet, is_read=excluded.is_read,
body_plain=excluded.body_plain, body_html=excluded.body_html
