ALTER TABLE fingerprints ADD COLUMN partial_skip_reason TEXT;
ALTER TABLE fingerprints ADD COLUMN partial_skip_size_bytes INTEGER;
ALTER TABLE fingerprints ADD COLUMN partial_skip_mtime_ns INTEGER;
