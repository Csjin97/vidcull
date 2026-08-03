use std::fs;
use std::io::Write;
use std::path::PathBuf;

use vidcull_core::Error;
use vidcull_parser::mkv_index::{Keyframe, index_mkv};

const MKV_TIMESTAMP_SCALE_NS: u64 = 1_000_000;
const MKV_VIDEO_TRACK: u64 = 1;
const MKV_DURATION_MS: u64 = 1000;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn index_reports_timestamp_scale_and_video_track_number() {
    let idx = index_mkv(fixture("black_320x180_30fps_1s.mkv")).expect("index mkv");
    assert_eq!(idx.timestamp_scale_ns, MKV_TIMESTAMP_SCALE_NS);
    assert_eq!(idx.video_track_number, MKV_VIDEO_TRACK);
}

#[test]
fn index_returns_a_single_keyframe_at_origin() {
    let idx = index_mkv(fixture("black_320x180_30fps_1s.mkv")).expect("index mkv");
    assert_eq!(
        idx.keyframes,
        vec![Keyframe {
            cue_index: 0,
            timestamp_ms: 0,
        }],
    );
    assert_eq!(idx.keyframe_count, 1);
}

#[test]
fn index_collapses_into_a_single_gop_spanning_the_clip() {
    let idx = index_mkv(fixture("black_320x180_30fps_1s.mkv")).expect("index mkv");
    assert_eq!(idx.gops.len(), 1);
    let gop = &idx.gops[0];
    assert_eq!(gop.start_cue_index, 0);
    assert_eq!(gop.start_timestamp_ms, 0);
    assert_eq!(gop.duration_ms, MKV_DURATION_MS);
}

#[test]
fn index_surfaces_segment_duration_for_last_gop_anchoring() {
    let idx = index_mkv(fixture("black_320x180_30fps_1s.mkv")).expect("index mkv");
    assert_eq!(idx.segment_duration_ms, Some(MKV_DURATION_MS));
}

#[test]
fn index_keyframe_count_field_matches_keyframes_len() {
    let idx = index_mkv(fixture("black_320x180_30fps_1s.mkv")).expect("index mkv");
    assert_eq!(
        u32::try_from(idx.keyframes.len()).unwrap(),
        idx.keyframe_count
    );
}

#[test]
fn truncated_mkv_fails_with_parse_error_not_panic() {
    let bytes = fs::read(fixture("black_320x180_30fps_1s.mkv")).expect("read fixture");
    let truncated_len = 64usize.min(bytes.len() / 4);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("truncated.mkv");
    let mut file = fs::File::create(&path).expect("create truncated");
    file.write_all(&bytes[..truncated_len])
        .expect("write truncated");
    drop(file);

    let err = index_mkv(path).expect_err("truncated mkv must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "truncated mkv should surface as Parse, got {err:?}"
    );
}

#[test]
fn missing_file_surfaces_as_io_error() {
    let err = index_mkv(PathBuf::from("/nonexistent/dir/missing.mkv")).expect_err("missing file");
    assert!(matches!(err, Error::Io(_)), "expected Io, got {err:?}");
}

#[test]
fn garbage_bytes_fail_without_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("garbage.mkv");
    fs::write(&path, vec![0xFFu8; 4096]).expect("write garbage");
    let err = index_mkv(path).expect_err("garbage must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "garbage mkv should surface as Parse, got {err:?}"
    );
}

#[test]
fn truncated_inside_cues_element_surfaces_as_parse_error() {
    let bytes = fs::read(fixture("black_320x180_30fps_1s.mkv")).expect("read fixture");
    assert!(bytes.len() > 2620, "fixture smaller than expected");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cues_truncated.mkv");
    fs::write(&path, &bytes[..2620]).expect("write truncated");

    let err = index_mkv(path).expect_err("cues-truncated mkv must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "cues-truncated should surface as Parse, got {err:?}"
    );
}
