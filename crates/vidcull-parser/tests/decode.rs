mod common;

use std::path::{Path, PathBuf};

use common::binaries_or_skip;
use vidcull_parser::probe_and_decode_sparse;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn decodes_h264_fixture_to_grayscale_frames() {
    let Some(bins) = binaries_or_skip("decodes_h264_fixture_to_grayscale_frames") else {
        return;
    };
    let decoded = probe_and_decode_sparse(&bins, &fixture("black_320x180_30fps_1s.mp4"), 4)
        .expect("decode H.264 fixture");

    assert_eq!(decoded.metadata.resolution.width, 320);
    assert_eq!(decoded.metadata.resolution.height, 180);
    assert!(
        !decoded.frames.is_empty() && decoded.frames.len() <= 4,
        "expected 1..=4 frames, got {}",
        decoded.frames.len()
    );
    for frame in &decoded.frames {
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 180);
        assert_eq!(
            frame.pixels.len(),
            320 * 180,
            "grayscale buffer must be width*height"
        );
    }
    let timestamps: Vec<u64> = decoded.frames.iter().map(|f| f.timestamp_ms).collect();
    assert!(
        timestamps.windows(2).all(|w| w[0] <= w[1]),
        "timestamps must be non-decreasing: {timestamps:?}"
    );
}

#[test]
fn decodes_av1_fixture_via_probe_escalation() {
    let Some(bins) = binaries_or_skip("decodes_av1_fixture_via_probe_escalation") else {
        return;
    };
    let decoded = probe_and_decode_sparse(&bins, &fixture("av1_320x180_1s.mp4"), 3)
        .expect("decode AV1 fixture");

    assert_eq!(
        decoded.decode_path,
        vidcull_parser::fallback::DecodePath::Fallback,
        "AV1 must take the ffmpeg fallback path"
    );
    assert_eq!(decoded.metadata.resolution.width, 320);
    assert_eq!(decoded.metadata.resolution.height, 180);
    assert!(!decoded.frames.is_empty());
    for frame in &decoded.frames {
        assert_eq!(frame.pixels.len(), 320 * 180);
    }
}

#[test]
fn corrupt_input_errors_without_panic() {
    let Some(bins) = binaries_or_skip("corrupt_input_errors_without_panic") else {
        return;
    };
    let tmp = std::env::temp_dir().join("vidcull_decode_corrupt.mp4");
    std::fs::write(&tmp, b"not a real mp4 file at all").expect("write corrupt fixture");
    let result = probe_and_decode_sparse(&bins, &tmp, 4);
    let _ = std::fs::remove_file(&tmp);
    assert!(result.is_err(), "corrupt input must error, got {result:?}");
}
