SELECT m.id, m.internal_date
FROM messages m
JOIN message_labels ml ON m.id = ml.message_id
WHERE ml.label_id = ?
ORDER BY m.internal_date DESC
LIMIT ?
