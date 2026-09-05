-- Labels table
CREATE TABLE IF NOT EXISTS labels (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    color_foreground TEXT,
    color_background TEXT
);

-- Messages table
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    snippet TEXT,
    from_address TEXT,
    to_address TEXT,
    subject TEXT,
    internal_date INTEGER NOT NULL,
    body_plain TEXT,
    body_html TEXT,
    is_read INTEGER DEFAULT 0
);

-- Junction table for messages and labels
CREATE TABLE IF NOT EXISTS message_labels (
    message_id TEXT NOT NULL,
    label_id TEXT NOT NULL,
    PRIMARY KEY (message_id, label_id),
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY (label_id) REFERENCES labels(id) ON DELETE CASCADE
);

-- Performance Indexes
CREATE INDEX IF NOT EXISTS idx_messages_internal_date ON messages(internal_date DESC);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE INDEX IF NOT EXISTS idx_message_labels_label_id ON message_labels(label_id);

-- FTS5 table for search (External Content Table)
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    subject,
    from_address,
    snippet,
    body_plain,
    content='messages',
    content_rowid='rowid'
);

-- Triggers to keep FTS in sync
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, subject, from_address, snippet, body_plain)
  VALUES (new.rowid, new.subject, new.from_address, new.snippet, new.body_plain);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, subject, from_address, snippet, body_plain)
  VALUES('delete', old.rowid, old.subject, old.from_address, old.snippet, old.body_plain);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, subject, from_address, snippet, body_plain)
  VALUES('delete', old.rowid, old.subject, old.from_address, old.snippet, old.body_plain);
  INSERT INTO messages_fts(rowid, subject, from_address, snippet, body_plain)
  VALUES (new.rowid, new.subject, new.from_address, new.snippet, new.body_plain);
END;
