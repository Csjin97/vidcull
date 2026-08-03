use std::path::PathBuf;

use vidcull_core::types::{Codec, Resolution, VideoDuration};
use vidcull_parser::{ContainerKind, probe};

const MP4_DURATION_MS: u64 = 1000;
const MP4_FPS_X1000: u32 = 30_000;
const MP4_FILE_BYTES: u64 = 2802;
const MP4_OVERALL_BITRATE_BPS: u64 = 22_416;
const MP4_FRAME_COUNT: u64 = 30;

const MKV_DURATION_MS: u64 = 1000;
const MKV_FPS_X1000: u32 = 30_000;
const MKV_FILE_BYTES: u64 = 2628;
const MKV_OVERALL_BITRATE_BPS: u64 = 21_024;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn fixtures_exist_at_expected_byte_lengths() {
    let mp4 = std::fs::metadata(fixture("black_320x180_30fps_1s.mp4")).expect("mp4 fixture");
    assert_eq!(mp4.len(), MP4_FILE_BYTES, "MP4 fixture size drifted");
    let mkv = std::fs::metadata(fixture("black_320x180_30fps_1s.mkv")).expect("mkv fixture");
    assert_eq!(mkv.len(), MKV_FILE_BYTES, "MKV fixture size drifted");
}

#[test]
fn mp4_probe_returns_golden_metadata() {
    let md = probe(fixture("black_320x180_30fps_1s.mp4")).expect("probe mp4");
    assert_eq!(md.container, ContainerKind::Mp4);
    assert_eq!(md.codec, Codec::H264);
    assert_eq!(md.resolution, Resolution::new(320, 180));
    assert_eq!(
        md.duration,
        Some(VideoDuration::from_millis(MP4_DURATION_MS))
    );
    assert_eq!(md.fps_x1000, Some(MP4_FPS_X1000));
    assert_eq!(md.bitrate_bps, Some(MP4_OVERALL_BITRATE_BPS));
}

#[test]
fn mp4_probe_frame_count_matches_30_at_30fps() {
    let md = probe(fixture("black_320x180_30fps_1s.mp4")).expect("probe mp4");
    let derived_frames = u64::from(md.fps_x1000.unwrap()) * MP4_DURATION_MS / 1000 / 1000;
    assert_eq!(derived_frames, MP4_FRAME_COUNT);
}

#[test]
fn mkv_probe_returns_golden_metadata() {
    let md = probe(fixture("black_320x180_30fps_1s.mkv")).expect("probe mkv");
    assert_eq!(md.container, ContainerKind::Mkv);
    assert_eq!(md.codec, Codec::H264);
    assert_eq!(md.resolution, Resolution::new(320, 180));
    assert_eq!(
        md.duration,
        Some(VideoDuration::from_millis(MKV_DURATION_MS))
    );
    assert_eq!(md.fps_x1000, Some(MKV_FPS_X1000));
    assert_eq!(md.bitrate_bps, Some(MKV_OVERALL_BITRATE_BPS));
}

#[test]
fn probe_rejects_unsupported_extension_without_io() {
    let err = probe("/does/not/exist.avi").expect_err("avi must not enter fast path");
    assert!(matches!(err, vidcull_core::Error::Unsupported(_)));
}

#[test]
fn probe_propagates_missing_file_as_io_error() {
    let err = probe("/nonexistent/dir/missing.mp4").expect_err("missing file");
    assert!(
        matches!(err, vidcull_core::Error::Io(_)),
        "expected Io, got {err:?}"
    );
}
