CREATE TABLE partial_mih_postings (
    chunk       INTEGER NOT NULL,
    slice_value INTEGER NOT NULL,
    file_id     INTEGER NOT NULL,
    scene_index INTEGER NOT NULL,
    PRIMARY KEY (chunk, slice_value, file_id, scene_index),
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_partial_mih_postings_file ON partial_mih_postings(file_id);

CREATE TABLE partial_index_files (
    file_id     INTEGER PRIMARY KEY,
    scene_count INTEGER NOT NULL,
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
) STRICT;
