
CREATE TABLE delete_batches (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id      INTEGER NOT NULL,  -- no FK: the batch may have dropped the group row itself
    trust_level   TEXT    NOT NULL CHECK (trust_level IN ('EXACT','VERY_LIKELY','POSSIBLE')),
    best_file_id  INTEGER,           -- the group's best pointer at delete time
    group_dropped INTEGER NOT NULL CHECK (group_dropped IN (0,1)),
    mode          TEXT    NOT NULL CHECK (mode IN ('TRASH','PERMANENT')),
    created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE delete_batch_files (
    batch_id INTEGER NOT NULL,
    file_id  INTEGER NOT NULL,
    path     TEXT    NOT NULL,
    role     TEXT    NOT NULL CHECK (role IN ('DELETED','SURVIVOR')),
    PRIMARY KEY (batch_id, file_id),
    FOREIGN KEY (batch_id) REFERENCES delete_batches(id) ON DELETE CASCADE,
    FOREIGN KEY (file_id)  REFERENCES files(id)          ON DELETE CASCADE
) STRICT;
