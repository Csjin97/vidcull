mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::binaries_or_skip;
use tempfile::TempDir;
use vidcull_core::types::Codec;
use vidcull_parser::Cancel;
use vidcull_parser::fallback::{
    DecodeConcurrency, FfmpegBinaries, decode_sparse, decode_sparse_with,
    decode_sparse_with_streaming, fallback_spawn_plan, probe_fallback,
};

fn run_ffmpeg_encode(bins: &FfmpegBinaries, args: &[&str], out: PathBuf) -> PathBuf {
    let status = Command::new(bins.ffmpeg())
        .args(args)
        .arg(&out)
        .status()
        .unwrap_or_else(|e| panic!("spawn ffmpeg encode failed: {e}"));
    assert!(
        status.success(),
        "ffmpeg encode failed ({status}) for args {args:?}"
    );
    out
}

fn encode_vp9_normal_into(bins: &FfmpegBinaries, dir: &Path, filename: &str) -> PathBuf {
    run_ffmpeg_encode(
        bins,
        &[
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=30",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "48",
            "-keyint_min",
            "48",
            "-deadline",
            "good",
            "-cpu-used",
            "5",
            "-row-mt",
            "0",
            "-threads",
            "1",
            "-aq-mode",
            "0",
        ],
        dir.join(filename),
    )
}

fn encode_vp9_normal(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    encode_vp9_normal_into(bins, dir, "vp9_normal_30s.mp4")
}

fn encode_vp9_mkv(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    encode_vp9_normal_into(bins, dir, "vp9_normal_30s.mkv")
}

fn encode_vp9_webm(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    encode_vp9_normal_into(bins, dir, "vp9_normal_30s.webm")
}

fn encode_mpeg2_normal(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    run_ffmpeg_encode(
        bins,
        &[
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=30",
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "48",
            "-keyint_min",
            "48",
            "-qscale:v",
            "4",
        ],
        dir.join("mpeg2_normal_30s.mpg"),
    )
}

fn encode_vp9_lowfps(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    run_ffmpeg_encode(
        bins,
        &[
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=24",
            "-vf",
            "fps=1/4",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "2",
            "-keyint_min",
            "2",
            "-deadline",
            "good",
            "-cpu-used",
            "5",
            "-threads",
            "1",
        ],
        dir.join("vp9_lowfps_4s.webm"),
    )
}

fn encode_vp9_half_fps(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    run_ffmpeg_encode(
        bins,
        &[
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=24",
            "-vf",
            "fps=1/2",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "2",
            "-keyint_min",
            "2",
            "-deadline",
            "good",
            "-cpu-used",
            "5",
            "-threads",
            "1",
        ],
        dir.join("vp9_half_fps_2s.mp4"),
    )
}

fn encode_h264_no_bframes(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    run_ffmpeg_encode(
        bins,
        &[
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=30",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "48",
            "-keyint_min",
            "48",
            "-bf",
            "0",
            "-x264-params",
            "bframes=0:scenecut=0",
            "-preset",
            "ultrafast",
            "-crf",
            "23",
        ],
        dir.join("h264_no_bframes_30s.mp4"),
    )
}

fn encode_h264_with_bframes(bins: &FfmpegBinaries, dir: &Path) -> PathBuf {
    run_ffmpeg_encode(
        bins,
        &[
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=24:duration=30",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "48",
            "-keyint_min",
            "48",
            "-bf",
            "2",
            "-x264-params",
            "bframes=2:b-adapt=0:scenecut=0",
            "-preset",
            "ultrafast",
            "-crf",
            "23",
        ],
        dir.join("h264_with_bframes_30s.mp4"),
    )
}

fn assert_three_way_identical(bins: &FfmpegBinaries, path: &Path, label: &str, budgets: &[usize]) {
    let meta = probe_fallback(bins, path).unwrap_or_else(|e| panic!("[{label}] probe: {e}"));
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    let w = meta.resolution.width;
    let h = meta.resolution.height;
    assert!(
        dur > 0 && w > 0 && h > 0,
        "[{label}] unusable probe metadata: dur={dur} {w}x{h}"
    );

    for &budget in budgets {
        let seq = decode_sparse(bins, path, dur, w, h, budget)
            .unwrap_or_else(|e| panic!("[{label}] budget={budget} decode_sparse: {e}"));

        for cap in [1usize, 4, 16] {
            let conc = DecodeConcurrency::new(cap);
            let par = decode_sparse_with(
                bins,
                path,
                dur,
                w,
                h,
                budget,
                &meta.codec,
                meta.fps_x1000,
                meta.has_b_frames,
                &conc,
            )
            .unwrap_or_else(|e| {
                panic!("[{label}] budget={budget} cap={cap} decode_sparse_with: {e}")
            });

            assert_eq!(
                par.len(),
                seq.len(),
                "[{label}] budget={budget} cap={cap}: frame count mismatch (par={} seq={})",
                par.len(),
                seq.len()
            );
            for (i, (s, p)) in seq.iter().zip(par.iter()).enumerate() {
                assert_eq!(
                    p.timestamp_ms, s.timestamp_ms,
                    "[{label}] budget={budget} cap={cap} frame[{i}]: timestamp mismatch"
                );
                assert_eq!(
                    p.pixels, s.pixels,
                    "[{label}] budget={budget} cap={cap} frame[{i}] @ {}ms: pixel mismatch",
                    s.timestamp_ms
                );
            }
        }
    }
}

#[test]
fn vp9_normal_fps_batch_matches_sequential_per_frame() {
    let Some(bins) = binaries_or_skip("vp9_normal_fps_batch_matches_sequential_per_frame") else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_normal(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 normal");
    assert_eq!(meta.fps_x1000, Some(24_000), "fixture should read 24 fps");
    assert_three_way_identical(&bins, &path, "vp9_normal", &[1, 6, 10_000]);
}

#[test]
fn vp9_mkv_container_routes_per_frame_and_matches_sequential() {
    let Some(bins) = binaries_or_skip("vp9_mkv_container_routes_per_frame_and_matches_sequential")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_mkv(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 mkv");
    assert_eq!(meta.codec, Codec::Vp9, "fixture should probe as VP9");
    assert_eq!(meta.fps_x1000, Some(24_000), "fixture should read 24 fps");

    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    let frame_px = u64::from(meta.resolution.width) * u64::from(meta.resolution.height);
    let (points, spawns) = fallback_spawn_plan(
        &meta.container,
        dur,
        10_000,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        frame_px,
    );
    assert!(
        points > 1,
        "fixture must span several grid points: {points}"
    );
    assert_eq!(
        spawns, points,
        "MKV must plan one ffmpeg spawn per grid point (per-frame route)"
    );

    assert_three_way_identical(&bins, &path, "vp9_mkv", &[1, 6, 10_000]);
}

#[test]
fn vp9_webm_container_routes_per_frame_and_matches_sequential() {
    let Some(bins) = binaries_or_skip("vp9_webm_container_routes_per_frame_and_matches_sequential")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_webm(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 webm");
    assert_eq!(meta.codec, Codec::Vp9, "fixture should probe as VP9");
    assert_eq!(meta.fps_x1000, Some(24_000), "fixture should read 24 fps");

    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    let frame_px = u64::from(meta.resolution.width) * u64::from(meta.resolution.height);
    let (points, spawns) = fallback_spawn_plan(
        &meta.container,
        dur,
        10_000,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        frame_px,
    );
    assert!(
        points > 1,
        "fixture must span several grid points: {points}"
    );
    assert_eq!(
        spawns, points,
        "WebM must plan one ffmpeg spawn per grid point (per-frame route)"
    );

    assert_three_way_identical(&bins, &path, "vp9_webm", &[1, 6, 10_000]);
}

#[test]
fn mpeg2_normal_fps_routes_per_frame_and_matches_sequential() {
    let Some(bins) = binaries_or_skip("mpeg2_normal_fps_routes_per_frame_and_matches_sequential")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_mpeg2_normal(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe mpeg2 normal");
    assert_eq!(meta.codec, Codec::Mpeg2, "fixture should probe as MPEG-2");
    assert_three_way_identical(&bins, &path, "mpeg2_normal", &[1, 6, 10_000]);
}

#[test]
fn h264_no_bframes_batch_matches_sequential_per_frame() {
    let Some(bins) = binaries_or_skip("h264_no_bframes_batch_matches_sequential_per_frame") else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_h264_no_bframes(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe h264 no-bframes");
    assert_eq!(meta.codec, Codec::H264, "fixture should probe as H.264");
    assert_eq!(meta.fps_x1000, Some(24_000), "fixture should read 24 fps");
    assert_eq!(
        meta.has_b_frames,
        Some(false),
        "no-bframe fixture must probe has_b_frames == 0"
    );
    assert_three_way_identical(&bins, &path, "h264_no_bframes", &[1, 6, 10_000]);
}

#[test]
fn h264_with_bframes_routes_per_frame_and_matches_sequential() {
    let Some(bins) = binaries_or_skip("h264_with_bframes_routes_per_frame_and_matches_sequential")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_h264_with_bframes(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe h264 with-bframes");
    assert_eq!(meta.codec, Codec::H264, "fixture should probe as H.264");
    assert_eq!(
        meta.has_b_frames,
        Some(true),
        "with-bframe fixture must probe has_b_frames > 0"
    );
    assert_three_way_identical(&bins, &path, "h264_with_bframes", &[1, 6, 10_000]);
}

#[test]
fn vp9_low_fps_per_frame_fallback_matches_sequential() {
    let Some(bins) = binaries_or_skip("vp9_low_fps_per_frame_fallback_matches_sequential") else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_lowfps(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 lowfps");
    assert_eq!(
        meta.fps_x1000,
        Some(250),
        "low-fps fixture should read 0.25 fps (1/4)"
    );
    assert_three_way_identical(&bins, &path, "vp9_lowfps", &[1, 6, 9]);
}

#[test]
fn vp9_half_fps_batch_eligible_guard_b_matches_sequential() {
    let Some(bins) = binaries_or_skip("vp9_half_fps_batch_eligible_guard_b_matches_sequential")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_half_fps(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 half-fps");
    assert_eq!(
        meta.fps_x1000,
        Some(500),
        "half-fps fixture should read 0.5 fps"
    );
    assert_three_way_identical(&bins, &path, "vp9_half_fps", &[1, 6, 9]);
}

#[test]
fn vp9_normal_fps_batch_keeps_full_grid_and_matches_sequential() {
    let Some(bins) =
        binaries_or_skip("vp9_normal_fps_batch_keeps_full_grid_and_matches_sequential")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_normal(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 normal");
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    let w = meta.resolution.width;
    let h = meta.resolution.height;

    let budget = 10_000usize;
    let seq = decode_sparse(&bins, &path, dur, w, h, budget).expect("decode_sparse");
    assert!(
        seq.len() > 5,
        "fixture must produce several windows: {}",
        seq.len()
    );

    let conc = DecodeConcurrency::new(4);
    let par = decode_sparse_with(
        &bins,
        &path,
        dur,
        w,
        h,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        &conc,
    )
    .expect("decode_sparse_with");

    assert_eq!(
        par.len(),
        seq.len(),
        "batch path must not lose any grid frame"
    );
    for (i, (s, p)) in seq.iter().zip(par.iter()).enumerate() {
        assert_eq!(p.timestamp_ms, s.timestamp_ms, "frame[{i}] timestamp");
        assert_eq!(
            p.pixels, s.pixels,
            "frame[{i}] @ {}ms pixels",
            s.timestamp_ms
        );
    }
}

fn assert_streaming_matches_buffered(
    bins: &FfmpegBinaries,
    path: &Path,
    label: &str,
    budgets: &[usize],
) {
    let meta = probe_fallback(bins, path).unwrap_or_else(|e| panic!("[{label}] probe: {e}"));
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    let w = meta.resolution.width;
    let h = meta.resolution.height;
    assert!(
        dur > 0 && w > 0 && h > 0,
        "[{label}] unusable probe metadata: dur={dur} {w}x{h}"
    );

    for &budget in budgets {
        for cap in [1usize, 4, 16] {
            let conc = DecodeConcurrency::new(cap);
            let buffered = decode_sparse_with(
                bins,
                path,
                dur,
                w,
                h,
                budget,
                &meta.codec,
                meta.fps_x1000,
                meta.has_b_frames,
                &conc,
            )
            .unwrap_or_else(|e| {
                panic!("[{label}] budget={budget} cap={cap} decode_sparse_with: {e}")
            });

            let conc_stream = DecodeConcurrency::new(cap);
            let mut streamed = Vec::new();
            decode_sparse_with_streaming(
                bins,
                path,
                dur,
                w,
                h,
                budget,
                &meta.codec,
                meta.fps_x1000,
                meta.has_b_frames,
                &conc_stream,
                Cancel::default(),
                |frame| {
                    streamed.push(frame.clone());
                    Ok(())
                },
            )
            .unwrap_or_else(|e| {
                panic!("[{label}] budget={budget} cap={cap} decode_sparse_with_streaming: {e}")
            });

            assert_eq!(
                streamed.len(),
                buffered.len(),
                "[{label}] budget={budget} cap={cap}: frame count mismatch (stream={} buf={})",
                streamed.len(),
                buffered.len()
            );
            for (i, (b, s)) in buffered.iter().zip(streamed.iter()).enumerate() {
                assert_eq!(
                    s.timestamp_ms, b.timestamp_ms,
                    "[{label}] budget={budget} cap={cap} frame[{i}]: timestamp mismatch"
                );
                assert_eq!(
                    s.pixels, b.pixels,
                    "[{label}] budget={budget} cap={cap} frame[{i}] @ {}ms: pixel mismatch",
                    b.timestamp_ms
                );
            }
        }
    }
}

#[test]
fn streaming_decode_matches_buffered_vp9_batch() {
    let Some(bins) = binaries_or_skip("streaming_decode_matches_buffered_vp9_batch") else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_normal(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 normal");
    assert_eq!(
        meta.fps_x1000,
        Some(24_000),
        "fixture should read 24 fps (batch path)"
    );
    assert_streaming_matches_buffered(&bins, &path, "vp9_normal_stream", &[1, 6, 10_000]);
}

#[test]
fn streaming_decode_matches_buffered_h264_per_frame() {
    let Some(bins) = binaries_or_skip("streaming_decode_matches_buffered_h264_per_frame") else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_h264_with_bframes(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe h264 with-bframes");
    assert_eq!(
        meta.has_b_frames,
        Some(true),
        "fixture must carry B-frames (per-frame path)"
    );
    assert_streaming_matches_buffered(&bins, &path, "h264_bframes_stream", &[1, 6, 10_000]);
}

#[test]
fn streaming_decode_matches_buffered_vp9_guard_b_collision() {
    let Some(bins) = binaries_or_skip("streaming_decode_matches_buffered_vp9_guard_b_collision")
    else {
        return;
    };
    let dir = TempDir::new().expect("tempdir");
    let path = encode_vp9_half_fps(&bins, dir.path());
    let meta = probe_fallback(&bins, &path).expect("probe vp9 half-fps");
    assert_eq!(
        meta.fps_x1000,
        Some(500),
        "half-fps fixture should read 0.5 fps (batch-eligible, guard (b) fires)"
    );
    assert_streaming_matches_buffered(&bins, &path, "vp9_half_fps_stream", &[1, 6, 9]);
}
