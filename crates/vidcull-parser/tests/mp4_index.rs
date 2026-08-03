use std::fs;
use std::io::Write;
use std::path::PathBuf;

use vidcull_core::Error;
use vidcull_parser::mp4_index::{Keyframe, index_mp4};

const MP4_TIMESCALE: u32 = 15_360;
const MP4_SAMPLE_COUNT: u32 = 30;
const MP4_DURATION_MS: u64 = 1000;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn index_reports_track_timescale_and_sample_count() {
    let idx = index_mp4(fixture("black_320x180_30fps_1s.mp4")).expect("index mp4");
    assert_eq!(idx.timescale, MP4_TIMESCALE);
    assert_eq!(idx.sample_count, MP4_SAMPLE_COUNT);
}

#[test]
fn index_returns_a_single_keyframe_at_origin() {
    let idx = index_mp4(fixture("black_320x180_30fps_1s.mp4")).expect("index mp4");
    assert_eq!(
        idx.keyframes,
        vec![Keyframe {
            sample_number: 1,
            timestamp_ms: 0,
        }],
    );
}

#[test]
fn index_collapses_into_a_single_gop_spanning_the_clip() {
    let idx = index_mp4(fixture("black_320x180_30fps_1s.mp4")).expect("index mp4");
    assert_eq!(idx.gops.len(), 1);
    let gop = &idx.gops[0];
    assert_eq!(gop.start_sample, 1);
    assert_eq!(gop.size, MP4_SAMPLE_COUNT);
    assert_eq!(gop.start_timestamp_ms, 0);
    assert_eq!(gop.duration_ms, MP4_DURATION_MS);
}

#[test]
fn index_keyframe_count_never_exceeds_sample_count() {
    let idx = index_mp4(fixture("black_320x180_30fps_1s.mp4")).expect("index mp4");
    assert!(u32::try_from(idx.keyframes.len()).unwrap() <= idx.sample_count);
}

#[test]
fn truncated_mp4_fails_with_parse_error_not_panic() {
    let bytes = fs::read(fixture("black_320x180_30fps_1s.mp4")).expect("read fixture");
    let truncated_len = 64usize.min(bytes.len() / 4);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("truncated.mp4");
    let mut file = fs::File::create(&path).expect("create truncated");
    file.write_all(&bytes[..truncated_len])
        .expect("write truncated");
    drop(file);

    let err = index_mp4(path).expect_err("truncated mp4 must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "truncated mp4 should surface as Parse, got {err:?}"
    );
}

#[test]
fn missing_file_surfaces_as_io_error() {
    let err = index_mp4(PathBuf::from("/nonexistent/dir/missing.mp4")).expect_err("missing file");
    assert!(matches!(err, Error::Io(_)), "expected Io, got {err:?}");
}

#[test]
fn garbage_bytes_fail_without_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("garbage.mp4");
    fs::write(&path, vec![0xFFu8; 4096]).expect("write garbage");
    let err = index_mp4(path).expect_err("garbage must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "garbage mp4 should surface as Parse, got {err:?}"
    );
}
