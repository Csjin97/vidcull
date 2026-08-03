CREATE TABLE regroup_queue (
    file_id      INTEGER PRIMARY KEY,
    enqueued_at  INTEGER NOT NULL,
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
) STRICT;
