
ALTER TABLE similarity_edges ADD COLUMN clip_start_ms   INTEGER; -- clip-side first aligned scene (ms)
ALTER TABLE similarity_edges ADD COLUMN clip_end_ms     INTEGER; -- clip-side last aligned scene (ms)
ALTER TABLE similarity_edges ADD COLUMN source_start_ms INTEGER; -- source-side first aligned scene (ms)
ALTER TABLE similarity_edges ADD COLUMN source_end_ms   INTEGER; -- source-side last aligned scene (ms)
ALTER TABLE similarity_edges ADD COLUMN matched_scenes  INTEGER; -- clip scenes that aligned (coverage numerator)
ALTER TABLE similarity_edges ADD COLUMN clip_scenes     INTEGER; -- total scenes in the clip (coverage denominator)
