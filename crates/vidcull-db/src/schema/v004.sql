CREATE TABLE daemon_settings (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    payload BLOB    NOT NULL
) STRICT;
