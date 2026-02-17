SELECT id, thread_id, snippet, from_address, to_address, subject, internal_date, body_plain, body_html, is_read
FROM messages
WHERE thread_id = ?
ORDER BY internal_date DESC
