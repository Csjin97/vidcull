mod common;

use std::path::{Path, PathBuf};

use common::binaries_or_skip;
use vidcull_core::types::Codec;
use vidcull_parser::fallback::{
    DecodePath, FallbackMetrics, decode_frame_at, decode_path_for, decode_sparse, probe_fallback,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn probe_fallback_reads_av1_metadata() {
    let Some(bins) = binaries_or_skip("probe_fallback_reads_av1_metadata") else {
        return;
    };
    let meta = probe_fallback(&bins, &fixture("av1_320x180_1s.mp4")).expect("probe av1");
    assert_eq!(meta.codec, Codec::Av1);
    assert_eq!(meta.resolution.width, 320);
    assert_eq!(meta.resolution.height, 180);
    assert_eq!(meta.fps_x1000, Some(30_000));
    assert_eq!(meta.duration.expect("duration").as_millis(), 1000);
    assert!(meta.bitrate_bps.is_some());
}

#[test]
fn probe_fallback_reads_vp9_metadata() {
    let Some(bins) = binaries_or_skip("probe_fallback_reads_vp9_metadata") else {
        return;
    };
    let meta = probe_fallback(&bins, &fixture("vp9_320x180_1s.webm")).expect("probe vp9");
    assert_eq!(meta.codec, Codec::Vp9);
    assert_eq!(meta.resolution.width, 320);
    assert_eq!(meta.resolution.height, 180);
}

#[test]
fn probe_fallback_reads_mpeg2_metadata() {
    let Some(bins) = binaries_or_skip("probe_fallback_reads_mpeg2_metadata") else {
        return;
    };
    let meta = probe_fallback(&bins, &fixture("mpeg2_320x180_1s.mpg")).expect("probe mpeg2");
    assert_eq!(meta.codec, Codec::Mpeg2);
    assert_eq!(meta.resolution.width, 320);
}

#[test]
fn decode_fallback_yields_one_grayscale_frame() {
    let Some(bins) = binaries_or_skip("decode_fallback_yields_one_grayscale_frame") else {
        return;
    };
    let path = fixture("av1_320x180_1s.mp4");
    let meta = probe_fallback(&bins, &path).expect("probe");
    let (w, h) = (meta.resolution.width, meta.resolution.height);
    let frame = decode_frame_at(&bins, &path, 0, w, h).expect("decode frame");
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 180);
    assert_eq!(frame.timestamp_ms, 0);
    assert_eq!(frame.pixels.len(), 320 * 180);
}

#[test]
fn decode_sparse_respects_budget() {
    let Some(bins) = binaries_or_skip("decode_sparse_respects_budget") else {
        return;
    };
    let path = fixture("vp9_320x180_1s.webm");
    let meta = probe_fallback(&bins, &path).expect("probe");
    let dur = meta.duration.expect("duration").as_millis();
    let frames = decode_sparse(&bins, &path, dur, 320, 180, 3).expect("sparse decode");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp_ms, 0);
    assert_eq!(frames[0].pixels.len(), 320 * 180);
}

#[test]
fn corrupt_file_fails_gracefully_without_panic() {
    let Some(bins) = binaries_or_skip("corrupt_file_fails_gracefully_without_panic") else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let bogus = tmp.path().join("garbage.mp4");
    std::fs::write(&bogus, b"this is definitely not a video container").expect("write");
    assert!(probe_fallback(&bins, &bogus).is_err());
    assert!(decode_frame_at(&bins, &bogus, 0, 320, 180).is_err());
}

#[test]
fn fast_path_codecs_never_enter_fallback() {
    assert_eq!(decode_path_for(&Codec::H264), DecodePath::Native);
    assert_eq!(decode_path_for(&Codec::H265), DecodePath::Native);
    assert_eq!(decode_path_for(&Codec::Av1), DecodePath::Fallback);

    let metrics = FallbackMetrics::default();
    metrics.record(DecodePath::Native);
    metrics.record(DecodePath::Native);
    assert_eq!(
        metrics.fallback_count(),
        0,
        "fast-path must not enter fallback"
    );
    metrics.record(DecodePath::Fallback);
    assert_eq!(metrics.fallback_count(), 1);
    assert_eq!(metrics.native_count(), 2);
}
