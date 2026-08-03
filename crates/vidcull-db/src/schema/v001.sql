
CREATE TABLE files (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT    NOT NULL UNIQUE,
    size_bytes    INTEGER NOT NULL,
    mtime_ns      INTEGER NOT NULL,
    inode         INTEGER,
    content_hash  BLOB,   -- 32-byte BLAKE3, NULL until Phase 3 hashes it
    codec         TEXT,
    container     TEXT,
    duration_ms   INTEGER,
    fps_x1000     INTEGER, -- fps * 1000 to keep integer-only storage
    bitrate_bps   INTEGER,
    width_px      INTEGER,
    height_px     INTEGER,
    first_seen_at INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL,
    deleted_at    INTEGER
) STRICT;

CREATE UNIQUE INDEX idx_files_path ON files(path);
CREATE INDEX idx_files_content_hash ON files(content_hash) WHERE content_hash IS NOT NULL;
CREATE INDEX idx_files_last_seen ON files(last_seen_at);

CREATE TABLE fingerprints (
    file_id        INTEGER PRIMARY KEY,
    tier1_global   BLOB    NOT NULL,
    tier2_temporal BLOB,
    format_version INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_fingerprints_file ON fingerprints(file_id);

CREATE TABLE scene_hashes (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id      INTEGER NOT NULL,
    ts_ms        INTEGER NOT NULL,
    phash        BLOB    NOT NULL,
    band_index   INTEGER NOT NULL, -- LSH band bucket for Phase 6 lookups
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_scene_hashes_file ON scene_hashes(file_id);
CREATE INDEX idx_scene_hashes_band ON scene_hashes(band_index);

CREATE TABLE duplicate_groups (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    trust_level  TEXT    NOT NULL CHECK (trust_level IN ('EXACT','VERY_LIKELY','POSSIBLE')),
    best_file_id INTEGER,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    FOREIGN KEY (best_file_id) REFERENCES files(id) ON DELETE SET NULL
) STRICT;

CREATE TABLE duplicate_group_members (
    group_id INTEGER NOT NULL,
    file_id  INTEGER NOT NULL,
    PRIMARY KEY (group_id, file_id),
    FOREIGN KEY (group_id) REFERENCES duplicate_groups(id) ON DELETE CASCADE,
    FOREIGN KEY (file_id)  REFERENCES files(id)            ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_duplicate_group_members_file ON duplicate_group_members(file_id);

CREATE TABLE similarity_edges (
    group_id    INTEGER NOT NULL,
    file_a      INTEGER NOT NULL,
    file_b      INTEGER NOT NULL,
    score_x1000 INTEGER NOT NULL, -- similarity score * 1000, integer-only
    PRIMARY KEY (group_id, file_a, file_b),
    FOREIGN KEY (group_id) REFERENCES duplicate_groups(id) ON DELETE CASCADE,
    FOREIGN KEY (file_a)   REFERENCES files(id)            ON DELETE CASCADE,
    FOREIGN KEY (file_b)   REFERENCES files(id)            ON DELETE CASCADE,
    CHECK (file_a < file_b) -- canonicalize ordering to dedupe edges
) STRICT;

CREATE INDEX idx_similarity_edges_group ON similarity_edges(group_id);

CREATE TABLE scan_state (
    root_path     TEXT PRIMARY KEY,
    last_scan_at  INTEGER NOT NULL,
    cursor        BLOB,             -- opaque resume cursor (postcard)
    files_seen    INTEGER NOT NULL DEFAULT 0,
    bytes_seen    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE task_queue (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT    NOT NULL,
    state         TEXT    NOT NULL CHECK (state IN ('PENDING','RUNNING','DONE','FAILED')),
    priority      INTEGER NOT NULL DEFAULT 0,
    payload       BLOB,
    attempts      INTEGER NOT NULL DEFAULT 0,
    enqueued_at   INTEGER NOT NULL,
    started_at    INTEGER,
    finished_at   INTEGER,
    last_error    TEXT
) STRICT;

CREATE INDEX idx_task_queue_state ON task_queue(state, priority DESC, enqueued_at);
