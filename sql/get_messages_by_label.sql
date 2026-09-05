WITH ranked_messages AS (
    SELECT
        m.*,
        ROW_NUMBER() OVER (
            PARTITION BY m.thread_id
            ORDER BY m.internal_date DESC, m.id DESC
        ) AS thread_rank
    FROM messages m
    JOIN message_labels ml ON m.id = ml.message_id
    WHERE ml.label_id = ?
)
SELECT m.id, m.thread_id, m.snippet, m.from_address, m.to_address, m.subject, m.internal_date, m.body_plain, m.body_html, m.is_read,
EXISTS (
    SELECT 1 FROM messages m2
    JOIN message_labels ml2 ON m2.id = ml2.message_id
    WHERE m2.thread_id = m.thread_id AND ml2.label_id = 'SENT'
) as has_sent_reply
FROM ranked_messages m
WHERE m.thread_rank = 1
ORDER BY m.internal_date DESC, m.id DESC
LIMIT ? OFFSET ?
