CREATE TABLE account_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    account_subject TEXT NOT NULL CHECK (
        length(account_subject) BETWEEN 1 AND 255
    )
) WITHOUT ROWID;
