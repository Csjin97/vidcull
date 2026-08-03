use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use vidcull_core::types::Codec;
use vidcull_core::{Error, Result};

use super::binary::FfmpegBinaries;
use super::concurrency::DecodeConcurrency;
use super::sidecar;
use super::timeout::{
    BATCH_DECODE_TIMEOUT_SECS, DECODE_FRAME_TIMEOUT_SECS, effective_timeout, run_with_timeout,
    run_with_timeout_cancellable,
};
use crate::cancel::Cancel;
use crate::probe::{ContainerKind, container_kind_from_path};
use crate::sparse::GrayscaleFrame;

pub(crate) fn scrub_input_path(stderr: &str, path: &Path) -> String {
    let display = path.to_string_lossy();
    let mut scrubbed = stderr.replace(display.as_ref(), "<input>");
    let slashed = display.replace('\\', "/");
    if slashed != display.as_ref() {
        scrubbed = scrubbed.replace(&slashed, "<input>");
    }
    scrubbed
}

pub fn decode_frame_at(
    bins: &FfmpegBinaries,
    path: &Path,
    timestamp_ms: u64,
    width: u32,
    height: u32,
) -> Result<GrayscaleFrame> {
    decode_frame_at_cancel(bins, path, timestamp_ms, width, height, Cancel::default())
}

fn decode_frame_at_cancel(
    bins: &FfmpegBinaries,
    path: &Path,
    timestamp_ms: u64,
    width: u32,
    height: u32,
    cancel: Cancel<'_>,
) -> Result<GrayscaleFrame> {
    if width == 0 || height == 0 {
        return Err(Error::Decode(
            "fallback decode: source has a zero dimension".into(),
        ));
    }
    let output = run_with_timeout_cancellable(
        Command::new(bins.ffmpeg()).args(decode_args(timestamp_ms, path)),
        effective_timeout(DECODE_FRAME_TIMEOUT_SECS),
        cancel,
        "decode",
    )?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let stderr_lossy = scrub_input_path(&stderr_raw, path);
        let trimmed = stderr_lossy.trim();
        let limit = 2000;
        let truncated = if trimmed.len() > limit {
            let skip_chars = trimmed.chars().count().saturating_sub(limit);
            let suffix: String = trimmed.chars().skip(skip_chars).collect();
            format!("... (truncated) ...\n{suffix}")
        } else {
            trimmed.to_owned()
        };
        return Err(Error::Decode(format!(
            "ffmpeg decode failed ({}): {}",
            output.status, truncated
        )));
    }
    raw_gray_to_frame(output.stdout, width, height, timestamp_ms)
}

pub fn decode_batch_head(
    bins: &FfmpegBinaries,
    path: &Path,
    width: u32,
    height: u32,
    count: usize,
) -> Result<Vec<GrayscaleFrame>> {
    if width == 0 || height == 0 {
        return Err(Error::Decode(
            "batch decode: source has a zero dimension".into(),
        ));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let output = run_with_timeout(
        Command::new(bins.ffmpeg()).args(batch_decode_args(count, path)),
        effective_timeout(BATCH_DECODE_TIMEOUT_SECS),
        "batch",
    )?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let stderr_lossy = scrub_input_path(&stderr_raw, path);
        return Err(Error::Decode(format!(
            "ffmpeg batch decode failed ({}): {}",
            output.status,
            stderr_lossy.trim()
        )));
    }
    raw_gray_to_frames(&output.stdout, width, height)
}

pub fn decode_sparse(
    bins: &FfmpegBinaries,
    path: &Path,
    duration_ms: u64,
    width: u32,
    height: u32,
    budget: usize,
) -> Result<Vec<GrayscaleFrame>> {
    let timestamps = plan_fallback_timestamps(duration_ms, budget);
    let mut out = Vec::with_capacity(timestamps.len());
    for ts in timestamps {
        out.push(decode_frame_at(bins, path, ts, width, height)?);
    }
    Ok(out)
}

pub fn decode_sparse_strided(
    bins: &FfmpegBinaries,
    path: &Path,
    duration_ms: u64,
    width: u32,
    height: u32,
    cap: usize,
) -> Result<Vec<GrayscaleFrame>> {
    let timestamps = plan_fallback_timestamps_strided(duration_ms, cap);
    let mut out = Vec::with_capacity(timestamps.len());
    for ts in timestamps {
        out.push(decode_frame_at(bins, path, ts, width, height)?);
    }
    Ok(out)
}

fn decode_grid<F>(
    timestamps: &[u64],
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
    decode: F,
) -> Result<Vec<GrayscaleFrame>>
where
    F: Fn(u64) -> Result<GrayscaleFrame> + Sync,
{
    if timestamps.is_empty() {
        return Ok(vec![]);
    }

    let n = timestamps.len();
    let cells: Vec<Mutex<Option<Result<GrayscaleFrame>>>> =
        (0..n).map(|_| Mutex::new(None)).collect();

    let counter = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);

    let workers = n.min(conc.capacity()).max(1);

    let session = conc.new_session();

    std::thread::scope(|s| {
        for _ in 0..workers {
            let counter = &counter;
            let abort = &abort;
            let cells = &cells;
            let decode = &decode;
            let session = &session;

            s.spawn(move || {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    if cancel.fired() {
                        break;
                    }
                    let idx = counter.fetch_add(1, Ordering::Relaxed);
                    if idx >= timestamps.len() {
                        break;
                    }
                    let ts = timestamps[idx];
                    let _permit = conc.acquire_fair(session);
                    let result = decode(ts);
                    if result.is_err() {
                        abort.store(true, Ordering::Relaxed);
                    }
                    let mut cell = cells[idx]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *cell = Some(result);
                }
            });
        }
    });

    if cancel.fired() {
        return Err(Error::Cancelled);
    }

    let mut frames = Vec::with_capacity(n);
    for cell in &cells {
        match cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(Ok(frame)) => frames.push(frame),
            Some(Err(err)) => {
                return Err(Error::Decode(err.to_string()));
            }
            None => {
                return Err(Error::Decode(
                    "decode_grid: worker did not fill expected cell".into(),
                ));
            }
        }
    }
    Ok(frames)
}

fn decode_windows_grid<F>(
    windows: &[&[u64]],
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
    decode: F,
) -> Result<Vec<GrayscaleFrame>>
where
    F: Fn(&[u64]) -> Result<Vec<GrayscaleFrame>> + Sync,
{
    if windows.is_empty() {
        return Ok(vec![]);
    }

    let n = windows.len();
    let cells: Vec<Mutex<Option<Result<Vec<GrayscaleFrame>>>>> =
        (0..n).map(|_| Mutex::new(None)).collect();

    let counter = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);

    let workers = n.min(conc.capacity()).max(1);

    let session = conc.new_session();

    std::thread::scope(|s| {
        for _ in 0..workers {
            let counter = &counter;
            let abort = &abort;
            let cells = &cells;
            let decode = &decode;
            let session = &session;

            s.spawn(move || {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    if cancel.fired() {
                        break;
                    }
                    let idx = counter.fetch_add(1, Ordering::Relaxed);
                    if idx >= windows.len() {
                        break;
                    }
                    let window = windows[idx];
                    let _permit = conc.acquire_fair(session);
                    let result = decode(window);
                    if result.is_err() {
                        abort.store(true, Ordering::Relaxed);
                    }
                    let mut cell = cells[idx]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *cell = Some(result);
                }
            });
        }
    });

    if cancel.fired() {
        return Err(Error::Cancelled);
    }

    let mut frames = Vec::new();
    for cell in &cells {
        match cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(Ok(window_frames)) => frames.extend(window_frames),
            Some(Err(err)) => {
                return Err(Error::Decode(err.to_string()));
            }
            None => {
                return Err(Error::Decode(
                    "decode_windows_grid: worker did not fill expected cell".into(),
                ));
            }
        }
    }
    Ok(frames)
}

const GRID_INTERVAL_MS: u64 = vidcull_core::SPARSE_GRID_INTERVAL_MS;

const BATCH_MAX_FRAME_PX: u64 = 1920 * 1080;

fn use_batch_path(
    container: &ContainerKind,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    frame_px: u64,
) -> bool {
    if matches!(container, ContainerKind::Mkv | ContainerKind::WebM) {
        return false;
    }
    let codec_batch_safe = match codec {
        Codec::Av1 | Codec::Vp9 => true,
        Codec::H264 | Codec::H265 => has_b_frames == Some(false),
        _ => false,
    };
    if !codec_batch_safe {
        return false;
    }
    if frame_px >= BATCH_MAX_FRAME_PX {
        return false;
    }
    match fps_x1000 {
        Some(fps) if fps > 0 => {
            let interval_ms = (1_000_000 + u64::from(fps) / 2) / u64::from(fps);
            interval_ms < GRID_INTERVAL_MS
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn fan_out_timestamps(
    bins: &FfmpegBinaries,
    path: &Path,
    timestamps: &[u64],
    width: u32,
    height: u32,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
) -> Result<Vec<GrayscaleFrame>> {
    let frame_px = u64::from(width) * u64::from(height);
    let container = container_kind_from_path(path);
    if use_batch_path(&container, codec, fps_x1000, has_b_frames, frame_px) {
        let windows = plan_windows(timestamps);
        decode_windows_grid(&windows, conc, cancel, |window| {
            decode_window_batch(bins, path, window, width, height, cancel)
        })
    } else {
        decode_grid(timestamps, conc, cancel, |ts| {
            decode_frame_at_cancel(bins, path, ts, width, height, cancel)
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub fn decode_sparse_with(
    bins: &FfmpegBinaries,
    path: &Path,
    duration_ms: u64,
    width: u32,
    height: u32,
    budget: usize,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    conc: &DecodeConcurrency,
) -> Result<Vec<GrayscaleFrame>> {
    let timestamps = plan_fallback_timestamps(duration_ms, budget);
    fan_out_timestamps(
        bins,
        path,
        &timestamps,
        width,
        height,
        codec,
        fps_x1000,
        has_b_frames,
        conc,
        Cancel::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn decode_sparse_strided_with(
    bins: &FfmpegBinaries,
    path: &Path,
    duration_ms: u64,
    width: u32,
    height: u32,
    cap: usize,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    conc: &DecodeConcurrency,
) -> Result<Vec<GrayscaleFrame>> {
    let timestamps = plan_fallback_timestamps_strided(duration_ms, cap);
    fan_out_timestamps(
        bins,
        path,
        &timestamps,
        width,
        height,
        codec,
        fps_x1000,
        has_b_frames,
        conc,
        Cancel::default(),
    )
}

const STREAM_CHUNK_WAVES: usize = 2;

fn stream_chunk_len(capacity: usize) -> usize {
    capacity.max(1).saturating_mul(STREAM_CHUNK_WAVES)
}

#[cfg(test)]
fn plan_sidecar_subchunks(points: &[u64], conc_cap: usize) -> Vec<&[u64]> {
    if points.is_empty() {
        return Vec::new();
    }
    let width = points.len().div_ceil(conc_cap.max(1)).max(1);
    points.chunks(width).collect()
}

#[allow(clippy::too_many_arguments)]
fn decode_chunk_sidecar_or_fanout(
    bins: &FfmpegBinaries,
    path: &Path,
    chunk_points: &[u64],
    width: u32,
    height: u32,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
) -> Result<Vec<GrayscaleFrame>> {
    if !matches!(codec, Codec::Av1) {
        if let Some(exe) = sidecar::resolve_sidecar(bins) {
            // Keep one demuxer/decoder context alive for the entire streaming chunk.
            match sidecar::decode_chunk_gray(&exe, path, chunk_points, width, height) {
                Ok(frames) => return Ok(frames),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(_) => {}
            }
        }
    }
    fan_out_timestamps(
        bins,
        path,
        chunk_points,
        width,
        height,
        codec,
        fps_x1000,
        has_b_frames,
        conc,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn decode_sparse_with_streaming<F>(
    bins: &FfmpegBinaries,
    path: &Path,
    duration_ms: u64,
    width: u32,
    height: u32,
    budget: usize,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
    on_frame: F,
) -> Result<()>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let timestamps = plan_fallback_timestamps(duration_ms, budget);
    let chunk = stream_chunk_len(conc.capacity());
    stream_decoded_chunks(
        &timestamps,
        chunk,
        cancel,
        |chunk_points| {
            decode_chunk_sidecar_or_fanout(
                bins,
                path,
                chunk_points,
                width,
                height,
                codec,
                fps_x1000,
                has_b_frames,
                conc,
                cancel,
            )
        },
        on_frame,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn decode_sparse_strided_with_streaming<F>(
    bins: &FfmpegBinaries,
    path: &Path,
    duration_ms: u64,
    width: u32,
    height: u32,
    cap: usize,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
    on_frame: F,
) -> Result<()>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let timestamps = plan_fallback_timestamps_strided(duration_ms, cap);
    let chunk = stream_chunk_len(conc.capacity());
    stream_decoded_chunks(
        &timestamps,
        chunk,
        cancel,
        |chunk_points| {
            decode_chunk_sidecar_or_fanout(
                bins,
                path,
                chunk_points,
                width,
                height,
                codec,
                fps_x1000,
                has_b_frames,
                conc,
                cancel,
            )
        },
        on_frame,
    )
}

fn stream_decoded_chunks<T, D, F>(
    timestamps: &[u64],
    chunk_len: usize,
    cancel: Cancel<'_>,
    mut decode_chunk: D,
    mut on_frame: F,
) -> Result<()>
where
    D: FnMut(&[u64]) -> Result<Vec<T>>,
    F: FnMut(&T) -> Result<()>,
{
    for chunk_points in timestamps.chunks(chunk_len.max(1)) {
        if cancel.fired() {
            return Err(Error::Cancelled);
        }
        let frames = decode_chunk(chunk_points)?;
        for frame in &frames {
            on_frame(frame)?;
        }
    }
    Ok(())
}

#[must_use]
pub fn fallback_spawn_plan(
    container: &ContainerKind,
    duration_ms: u64,
    budget: usize,
    codec: &Codec,
    fps_x1000: Option<u32>,
    has_b_frames: Option<bool>,
    frame_px: u64,
) -> (usize, usize) {
    let timestamps = plan_fallback_timestamps(duration_ms, budget);
    let points = timestamps.len();
    let spawns = if use_batch_path(container, codec, fps_x1000, has_b_frames, frame_px) {
        plan_windows(&timestamps).len()
    } else {
        points
    };
    (points, spawns)
}

fn decode_args(timestamp_ms: u64, path: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::with_capacity(16);
    for flag in ["-v", "error", "-hide_banner", "-nostdin", "-ss"] {
        args.push(flag.into());
    }
    args.push(format_seconds(timestamp_ms).into());
    args.push("-i".into());
    args.push(path.as_os_str().to_owned());
    for flag in [
        "-frames:v",
        "1",
        "-an",
        "-vf",
        "format=gray",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "gray",
        "-",
    ] {
        args.push(flag.into());
    }
    args
}

#[must_use]
pub fn thumb_decode_args(timestamp_ms: u64, path: &Path, hwaccel: bool) -> Vec<OsString> {
    let capacity = if hwaccel { 18 } else { 16 };
    let mut args: Vec<OsString> = Vec::with_capacity(capacity);
    for flag in ["-v", "error", "-hide_banner", "-nostdin"] {
        args.push(flag.into());
    }
    if hwaccel {
        args.push("-hwaccel".into());
        args.push("auto".into());
    }
    args.push("-ss".into());
    args.push(format_seconds(timestamp_ms).into());
    args.push("-i".into());
    args.push(path.as_os_str().to_owned());
    for flag in [
        "-frames:v",
        "1",
        "-an",
        "-vf",
        "format=gray",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "gray",
        "-",
    ] {
        args.push(flag.into());
    }
    args
}

pub fn decode_thumb_frame_at(
    bins: &FfmpegBinaries,
    path: &Path,
    timestamp_ms: u64,
    width: u32,
    height: u32,
    hwaccel: bool,
) -> Result<GrayscaleFrame> {
    if width == 0 || height == 0 {
        return Err(Error::Decode(
            "thumb decode: source has a zero dimension".into(),
        ));
    }
    let output = run_with_timeout(
        Command::new(bins.ffmpeg()).args(thumb_decode_args(timestamp_ms, path, hwaccel)),
        effective_timeout(DECODE_FRAME_TIMEOUT_SECS),
        "thumb",
    )?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let stderr_lossy = scrub_input_path(&stderr_raw, path);
        let trimmed = stderr_lossy.trim();
        let limit = 2000;
        let truncated = if trimmed.len() > limit {
            let skip_chars = trimmed.chars().count().saturating_sub(limit);
            let suffix: String = trimmed.chars().skip(skip_chars).collect();
            format!("... (truncated) ...\n{suffix}")
        } else {
            trimmed.to_owned()
        };
        return Err(Error::Decode(format!(
            "ffmpeg thumb decode failed ({}): {}",
            output.status, truncated
        )));
    }
    raw_gray_to_frame(output.stdout, width, height, timestamp_ms)
}

const WINDOW_TO_PAD_MS: u64 = 200;

const WINDOW_SPAN_MS: u64 = 10_000;

const WINDOW_MAX_POINTS: usize = 8;

fn batch_window_decode_args(window_pts_ms: &[u64], path: &Path) -> Vec<OsString> {
    if window_pts_ms.is_empty() {
        return Vec::new();
    }
    let start = window_pts_ms[0];
    let last = *window_pts_ms.last().expect("non-empty");

    let mut select = String::from("eq(n,0)");
    for &pt in &window_pts_ms[1..] {
        let sec = format_seconds(pt);
        write!(select, "+gte(t,{sec})*lt(prev_t,{sec})").expect("write to String is infallible");
    }
    let vf = format!("select='{select}',format=gray");

    let mut args: Vec<OsString> = Vec::with_capacity(20);
    for flag in ["-v", "error", "-hide_banner", "-nostdin", "-copyts", "-ss"] {
        args.push(flag.into());
    }
    args.push(format_seconds(start).into());
    args.push("-i".into());
    args.push(path.as_os_str().to_owned());
    args.push("-to".into());
    args.push(format_seconds(last + WINDOW_TO_PAD_MS).into());
    args.push("-an".into());
    args.push("-vf".into());
    args.push(vf.into());
    for flag in ["-vsync", "0", "-f", "rawvideo", "-pix_fmt", "gray", "-"] {
        args.push(flag.into());
    }
    args
}

fn plan_windows(timestamps: &[u64]) -> Vec<&[u64]> {
    let mut windows = Vec::new();
    let mut start = 0usize;
    let n = timestamps.len();
    while start < n {
        let window_start_ts = timestamps[start];
        let mut end = start + 1;
        while end < n
            && (end - start) < WINDOW_MAX_POINTS
            && timestamps[end].saturating_sub(window_start_ts) <= WINDOW_SPAN_MS
        {
            end += 1;
        }
        windows.push(&timestamps[start..end]);
        start = end;
    }
    windows
}

pub(crate) fn decode_window_batch(
    bins: &FfmpegBinaries,
    path: &Path,
    window_pts_ms: &[u64],
    width: u32,
    height: u32,
    cancel: Cancel<'_>,
) -> Result<Vec<GrayscaleFrame>> {
    if window_pts_ms.is_empty() {
        return Ok(Vec::new());
    }
    if width == 0 || height == 0 {
        return Err(Error::Decode(
            "window batch decode: source has a zero dimension".into(),
        ));
    }

    let per_frame_fallback = || -> Result<Vec<GrayscaleFrame>> {
        let mut out = Vec::with_capacity(window_pts_ms.len());
        for &ts in window_pts_ms {
            out.push(decode_frame_at_cancel(
                bins, path, ts, width, height, cancel,
            )?);
        }
        Ok(out)
    };

    let batch_result = run_with_timeout_cancellable(
        Command::new(bins.ffmpeg()).args(batch_window_decode_args(window_pts_ms, path)),
        effective_timeout(BATCH_DECODE_TIMEOUT_SECS),
        cancel,
        "stream",
    );
    if matches!(batch_result, Err(Error::Cancelled)) {
        return Err(Error::Cancelled);
    }
    let batched = batch_result
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| raw_gray_to_frames(&output.stdout, width, height).ok())
        .filter(|frames| frames.len() == window_pts_ms.len());

    match batched {
        Some(frames) => {
            let stamped = frames
                .into_iter()
                .zip(window_pts_ms.iter())
                .map(|(mut frame, &ts)| {
                    frame.timestamp_ms = ts;
                    frame
                })
                .collect();
            Ok(stamped)
        }
        None => per_frame_fallback(),
    }
}

fn batch_decode_args(count: usize, path: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::with_capacity(16);
    for flag in ["-v", "error", "-hide_banner", "-nostdin", "-i"] {
        args.push(flag.into());
    }
    args.push(path.as_os_str().to_owned());
    args.push("-frames:v".into());
    args.push(count.to_string().into());
    for flag in [
        "-an",
        "-vf",
        "format=gray",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "gray",
        "-",
    ] {
        args.push(flag.into());
    }
    args
}

fn raw_gray_to_frames(bytes: &[u8], width: u32, height: u32) -> Result<Vec<GrayscaleFrame>> {
    let frame_len = width as usize * height as usize;
    if frame_len == 0 || bytes.len() % frame_len != 0 {
        return Err(Error::Decode(format!(
            "batch decode: {} raw bytes is not a multiple of the {width}x{height} frame size ({frame_len})",
            bytes.len()
        )));
    }
    let frames = bytes
        .chunks_exact(frame_len)
        .enumerate()
        .map(|(i, chunk)| GrayscaleFrame {
            width,
            height,
            timestamp_ms: i as u64,
            pixels: chunk.to_vec(),
        })
        .collect();
    Ok(frames)
}

fn format_seconds(timestamp_ms: u64) -> String {
    format!("{}.{:03}", timestamp_ms / 1000, timestamp_ms % 1000)
}

fn raw_gray_to_frame(
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    timestamp_ms: u64,
) -> Result<GrayscaleFrame> {
    let expected = width as usize * height as usize;
    if bytes.len() != expected {
        return Err(Error::Decode(format!(
            "fallback decode: expected {expected} gray bytes ({width}x{height}), got {}",
            bytes.len()
        )));
    }
    Ok(GrayscaleFrame {
        width,
        height,
        timestamp_ms,
        pixels: bytes,
    })
}

pub(crate) fn plan_fallback_timestamps(duration_ms: u64, budget: usize) -> Vec<u64> {
    if budget == 0 || duration_ms == 0 {
        return Vec::new();
    }
    let interval_ms = GRID_INTERVAL_MS;
    let mut timestamps = Vec::new();
    for i in 0..budget {
        let ts = i as u64 * interval_ms;
        if ts < duration_ms {
            timestamps.push(ts);
        } else {
            break;
        }
    }
    timestamps
}

#[must_use]
pub fn full_grid_len(duration_ms: u64) -> usize {
    usize::try_from(duration_ms.div_ceil(GRID_INTERVAL_MS)).unwrap_or(usize::MAX)
}

pub(crate) fn plan_fallback_timestamps_strided(duration_ms: u64, cap: usize) -> Vec<u64> {
    if cap == 0 || duration_ms == 0 {
        return Vec::new();
    }
    let full = full_grid_len(duration_ms);
    if full <= cap {
        return plan_fallback_timestamps(duration_ms, cap);
    }
    let stride = full.div_ceil(cap) as u64;
    let interval_ms = GRID_INTERVAL_MS;
    (0..cap as u64)
        .map(|i| i * stride * interval_ms)
        .filter(|&ts| ts < duration_ms)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn paused(flag: &std::sync::atomic::AtomicBool) -> Cancel<'_> {
        Cancel {
            pause: Some(flag),
            removal: None,
        }
    }

    #[test]
    fn scrub_input_path_removes_the_input_path_from_stderr() {
        let path = std::path::Path::new(r"C:\Users\alice\Videos\holiday.mp4");
        let stderr =
            "C:\\Users\\alice\\Videos\\holiday.mp4: No such file or directory\nconversion failed";
        let scrubbed = scrub_input_path(stderr, path);
        assert!(!scrubbed.contains("alice"), "username leaked: {scrubbed}");
        assert!(!scrubbed.contains("holiday"), "filename leaked: {scrubbed}");
        assert!(
            scrubbed.contains("<input>"),
            "placeholder missing: {scrubbed}"
        );
        assert!(
            scrubbed.contains("No such file or directory"),
            "reason lost"
        );

        let slashed = "C:/Users/alice/Videos/holiday.mp4: Invalid data";
        let scrubbed2 = scrub_input_path(slashed, path);
        assert!(
            !scrubbed2.contains("alice"),
            "forward-slash path leaked: {scrubbed2}"
        );
        assert!(scrubbed2.contains("<input>"));
    }

    #[test]
    fn strided_plan_matches_full_grid_when_cap_is_not_hit() {
        for cap in [3, 4, 100] {
            assert_eq!(
                plan_fallback_timestamps_strided(6000, cap),
                plan_fallback_timestamps(6000, cap),
                "cap={cap}"
            );
        }
        assert_eq!(full_grid_len(6000), 3);
        assert_eq!(full_grid_len(0), 0);
        assert_eq!(full_grid_len(2500), 1);
        assert_eq!(full_grid_len(2501), 2);
    }

    #[test]
    fn strided_plan_spreads_a_capped_budget_across_the_whole_clip() {
        let duration_ms = 3_600_000;
        let cap = 32;
        let plan = plan_fallback_timestamps_strided(duration_ms, cap);
        assert_eq!(plan.len(), cap);
        assert_eq!(plan[0], 0, "anchored at t=0");
        let stride_ms = 45 * 2500;
        for (i, ts) in plan.iter().enumerate() {
            assert_eq!(ts % 2500, 0, "on the canonical grid: {ts}");
            assert_eq!(*ts, i as u64 * stride_ms, "evenly strided at {i}");
            assert!(*ts < duration_ms);
        }
        assert!(
            *plan.last().expect("non-empty") > duration_ms - 2 * stride_ms,
            "last sample must sit near the end: {:?}",
            plan.last()
        );
        assert_eq!(
            plan_fallback_timestamps(duration_ms, cap).last(),
            Some(&77_500),
            "the truncating planner stops at the head — the contrast this fixes"
        );
    }

    #[test]
    fn strided_plan_zero_inputs_yield_empty() {
        assert!(plan_fallback_timestamps_strided(0, 8).is_empty());
        assert!(plan_fallback_timestamps_strided(10_000, 0).is_empty());
    }

    #[test]
    fn format_seconds_pads_milliseconds() {
        assert_eq!(format_seconds(0), "0.000");
        assert_eq!(format_seconds(500), "0.500");
        assert_eq!(format_seconds(1500), "1.500");
        assert_eq!(format_seconds(1234), "1.234");
        assert_eq!(format_seconds(60_007), "60.007");
    }

    #[test]
    fn decode_args_seek_before_input_and_single_grayscale_frame() {
        let args = decode_args(500, Path::new("/clip.webm"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let ss = rendered.iter().position(|a| a == "-ss").expect("-ss");
        let i = rendered.iter().position(|a| a == "-i").expect("-i");
        assert!(ss < i, "-ss must come before -i: {rendered:?}");
        assert_eq!(rendered[ss + 1], "0.500");
        assert_eq!(rendered[i + 1], "/clip.webm");
        assert!(rendered.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert!(rendered.contains(&"format=gray".to_string()));
        assert!(rendered.contains(&"rawvideo".to_string()));
        assert_eq!(rendered.last().expect("last"), "-");
    }

    #[test]
    fn fingerprint_decode_args_never_contain_hwaccel() {
        let args = decode_args(500, Path::new("/clip.mp4"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !rendered.iter().any(|a| a == "-hwaccel"),
            "fingerprint decode_args must not contain -hwaccel: {rendered:?}"
        );
    }

    #[test]
    fn thumb_decode_args_without_hwaccel_matches_fingerprint_layout() {
        let args = thumb_decode_args(500, Path::new("/clip.mp4"), false);
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !rendered.iter().any(|a| a == "-hwaccel"),
            "SW thumb args must not contain -hwaccel: {rendered:?}"
        );
        let ss = rendered.iter().position(|a| a == "-ss").expect("-ss");
        let i = rendered.iter().position(|a| a == "-i").expect("-i");
        assert!(ss < i, "-ss must come before -i: {rendered:?}");
        assert_eq!(rendered[ss + 1], "0.500");
        assert!(rendered.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert!(rendered.contains(&"format=gray".to_string()));
        assert_eq!(rendered.last().expect("last"), "-");
    }

    #[test]
    fn thumb_decode_args_with_hwaccel_inserts_flag_before_input() {
        let args = thumb_decode_args(1500, Path::new("/clip.mkv"), true);
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let hw_pos = rendered
            .iter()
            .position(|a| a == "-hwaccel")
            .expect("-hwaccel");
        assert_eq!(rendered[hw_pos + 1], "auto");
        let i_pos = rendered.iter().position(|a| a == "-i").expect("-i");
        assert!(hw_pos < i_pos, "-hwaccel must precede -i: {rendered:?}");
        let ss_pos = rendered.iter().position(|a| a == "-ss").expect("-ss");
        assert!(ss_pos < i_pos, "-ss must precede -i: {rendered:?}");
        assert_eq!(rendered[ss_pos + 1], "1.500");
        assert!(rendered.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert!(rendered.contains(&"format=gray".to_string()));
        assert_eq!(rendered.last().expect("last"), "-");
    }

    #[test]
    fn batch_decode_args_no_seek_consecutive_grayscale() {
        let args = batch_decode_args(100, Path::new("/clip.mp4"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!rendered.iter().any(|a| a == "-ss"), "batch must not seek");
        let i = rendered.iter().position(|a| a == "-i").expect("-i");
        assert_eq!(rendered[i + 1], "/clip.mp4");
        assert!(rendered.windows(2).any(|w| w == ["-frames:v", "100"]));
        assert!(rendered.contains(&"format=gray".to_string()));
        assert!(rendered.contains(&"rawvideo".to_string()));
        assert_eq!(rendered.last().expect("last"), "-");
    }

    #[test]
    fn batch_window_decode_args_multi_point_select_and_seek() {
        let args = batch_window_decode_args(&[7500, 10_000, 12_500], Path::new("/clip.webm"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(
            rendered.iter().any(|a| a == "-copyts"),
            "batch window must use -copyts: {rendered:?}"
        );
        let ss = rendered.iter().position(|a| a == "-ss").expect("-ss");
        let i = rendered.iter().position(|a| a == "-i").expect("-i");
        assert!(ss < i, "-ss must precede -i: {rendered:?}");
        assert_eq!(rendered[ss + 1], "7.500");
        assert_eq!(rendered[i + 1], "/clip.webm");
        let to = rendered.iter().position(|a| a == "-to").expect("-to");
        assert_eq!(rendered[to + 1], "12.700");
        assert!(
            rendered.windows(2).any(|w| w == ["-vsync", "0"]),
            "batch window must use -vsync 0: {rendered:?}"
        );
        let vf = rendered.iter().position(|a| a == "-vf").expect("-vf");
        let expr = &rendered[vf + 1];
        assert_eq!(
            expr,
            "select='eq(n,0)+gte(t,10.000)*lt(prev_t,10.000)+gte(t,12.500)*lt(prev_t,12.500)',format=gray",
            "select expr mismatch: {expr}"
        );
        assert!(rendered.contains(&"rawvideo".to_string()));
        assert!(rendered.contains(&"gray".to_string()));
        assert_eq!(rendered.last().expect("last"), "-");
    }

    #[test]
    fn batch_window_decode_args_single_point_is_eq_n0_only() {
        let args = batch_window_decode_args(&[2500], Path::new("/clip.mp4"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let vf = rendered.iter().position(|a| a == "-vf").expect("-vf");
        assert_eq!(rendered[vf + 1], "select='eq(n,0)',format=gray");
        let ss = rendered.iter().position(|a| a == "-ss").expect("-ss");
        assert_eq!(rendered[ss + 1], "2.500");
        let to = rendered.iter().position(|a| a == "-to").expect("-to");
        assert_eq!(rendered[to + 1], "2.700", "single point -to = pt + pad");
        assert!(rendered.iter().any(|a| a == "-copyts"));
        assert!(rendered.windows(2).any(|w| w == ["-vsync", "0"]));
    }

    #[test]
    fn batch_window_decode_args_empty_is_empty() {
        assert!(batch_window_decode_args(&[], Path::new("/clip.mp4")).is_empty());
    }

    #[test]
    fn batch_window_select_seconds_match_format_seconds() {
        let pts = [0u64, 2500, 9130, 11_770];
        let args = batch_window_decode_args(&pts, Path::new("/c.mkv"));
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let vf = rendered.iter().position(|a| a == "-vf").expect("-vf");
        let expr = &rendered[vf + 1];
        for &pt in &pts[1..] {
            let sec = format_seconds(pt);
            assert!(
                expr.contains(&format!("gte(t,{sec})*lt(prev_t,{sec})")),
                "expr {expr} must use format_seconds({pt})={sec}"
            );
        }
        assert!(expr.starts_with("select='eq(n,0)"));
    }

    fn assert_partition(timestamps: &[u64], windows: &[&[u64]]) {
        let flat: Vec<u64> = windows.iter().flat_map(|w| w.iter().copied()).collect();
        assert_eq!(flat, timestamps, "windows must partition the input exactly");
    }

    #[test]
    fn plan_windows_chunks_by_span_then_count() {
        let grid: Vec<u64> = (0..12).map(|i| i * 2500).collect();
        let windows = plan_windows(&grid);
        assert_partition(&grid, &windows);
        assert_eq!(windows[0], &[0, 2500, 5000, 7500, 10_000]);
        for w in &windows {
            let span = w.last().unwrap() - w.first().unwrap();
            assert!(span <= WINDOW_SPAN_MS, "span {span} over bound in {w:?}");
            assert!(w.len() <= WINDOW_MAX_POINTS, "len {} over bound", w.len());
        }
    }

    #[test]
    fn plan_windows_point_count_bound_bites_for_dense_grid() {
        let dense: Vec<u64> = (0..20).collect();
        let windows = plan_windows(&dense);
        assert_partition(&dense, &windows);
        assert_eq!(windows[0].len(), WINDOW_MAX_POINTS);
        assert_eq!(windows[1].len(), WINDOW_MAX_POINTS);
        assert_eq!(windows[2].len(), 4, "20 = 8 + 8 + 4");
    }

    #[test]
    fn plan_windows_single_and_empty() {
        let single = [4242u64];
        let w = plan_windows(&single);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0], &[4242]);
        assert!(plan_windows(&[]).is_empty());
    }

    #[test]
    fn stream_chunk_len_is_bounded_and_at_least_capacity() {
        assert_eq!(stream_chunk_len(1), 2);
        assert_eq!(stream_chunk_len(8), 16);
        assert_eq!(stream_chunk_len(16), 32);
        assert!(stream_chunk_len(0) >= 1);
        for cap in [1usize, 4, 8, 16, 64] {
            assert!(
                stream_chunk_len(cap) >= cap,
                "chunk {} below capacity {cap}",
                stream_chunk_len(cap)
            );
        }
    }

    #[test]
    fn streaming_peak_live_frames_is_bounded_by_chunk_len() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct LiveFrame {
            live: Rc<Cell<usize>>,
        }
        impl LiveFrame {
            fn new(live: &Rc<Cell<usize>>) -> Self {
                live.set(live.get() + 1);
                Self {
                    live: Rc::clone(live),
                }
            }
        }
        impl Drop for LiveFrame {
            fn drop(&mut self) {
                self.live.set(self.live.get() - 1);
            }
        }

        let chunk_len = stream_chunk_len(2);
        let total = chunk_len * 50 + 7;
        assert!(total > chunk_len, "test must span multiple chunks");
        let timestamps: Vec<u64> = (0..total as u64).collect();

        let live = Rc::new(Cell::new(0usize));
        let peak = Cell::new(0usize);
        let mut emitted = 0usize;

        stream_decoded_chunks(
            &timestamps,
            chunk_len,
            Cancel::default(),
            |pts| {
                Ok(pts
                    .iter()
                    .map(|_| LiveFrame::new(&live))
                    .collect::<Vec<_>>())
            },
            |_frame: &LiveFrame| {
                peak.set(peak.get().max(live.get()));
                emitted += 1;
                Ok(())
            },
        )
        .expect("fake streaming decode");

        assert_eq!(
            emitted, total,
            "every timestamp must be folded exactly once"
        );
        assert_eq!(live.get(), 0, "all frames dropped after the fold");
        assert!(
            peak.get() <= chunk_len,
            "peak frames alive at once was {} — exceeded the chunk bound {chunk_len}; \
             the fold accumulated beyond one chunk",
            peak.get(),
        );
    }

    #[test]
    fn plan_windows_deterministic() {
        let grid: Vec<u64> = (0..50).map(|i| i * 2500).collect();
        let a = plan_windows(&grid);
        let b = plan_windows(&grid);
        assert_eq!(a, b, "planner must be deterministic");
    }

    #[test]
    fn decode_grid_mid_cancel_returns_cancelled_and_resume_is_byte_identical() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let fake = |ts: u64| -> Result<GrayscaleFrame> {
            Ok(GrayscaleFrame {
                width: 2,
                height: 1,
                timestamp_ms: ts,
                pixels: vec![(ts & 0xff) as u8, ((ts >> 8) & 0xff) as u8],
            })
        };
        let timestamps: Vec<u64> = (0..64u64).map(|i| i * 2500).collect();
        let conc = DecodeConcurrency::serial();

        let full =
            decode_grid(&timestamps, &conc, Cancel::default(), fake).expect("uninterrupted grid");
        assert_eq!(full.len(), timestamps.len());

        let cancel = AtomicBool::new(false);
        let decoded = AtomicUsize::new(0);
        let cancelling = |ts: u64| -> Result<GrayscaleFrame> {
            if decoded.fetch_add(1, Ordering::Relaxed) == 0 {
                cancel.store(true, Ordering::Relaxed);
            }
            fake(ts)
        };
        let cancelled = decode_grid(&timestamps, &conc, paused(&cancel), cancelling);
        assert!(
            matches!(cancelled, Err(Error::Cancelled)),
            "mid-grid cancel must surface as the typed Cancelled, got {cancelled:?}",
        );

        cancel.store(false, Ordering::Relaxed);
        let resumed = decode_grid(&timestamps, &conc, paused(&cancel), fake).expect("resumed grid");
        assert_eq!(
            resumed, full,
            "resume after cancel must be byte-identical to the uninterrupted run",
        );
    }

    #[test]
    fn streaming_mid_cancel_folds_only_a_prefix_and_resume_is_byte_identical() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let chunk_len = stream_chunk_len(2);
        let total = chunk_len * 4 + 3;
        assert!(total > chunk_len, "test must span multiple chunks");
        let timestamps: Vec<u64> = (0..total as u64).collect();

        let decode_chunk = |pts: &[u64]| -> Result<Vec<u64>> { Ok(pts.to_vec()) };

        let mut full = Vec::new();
        stream_decoded_chunks(
            &timestamps,
            chunk_len,
            Cancel::default(),
            decode_chunk,
            |f: &u64| {
                full.push(*f);
                Ok(())
            },
        )
        .expect("uninterrupted stream");
        assert_eq!(full.len(), total);

        let cancel = AtomicBool::new(false);
        let mut partial = Vec::new();
        let res = stream_decoded_chunks(
            &timestamps,
            chunk_len,
            paused(&cancel),
            |pts: &[u64]| {
                let frames = decode_chunk(pts);
                cancel.store(true, Ordering::Relaxed);
                frames
            },
            |f: &u64| {
                partial.push(*f);
                Ok(())
            },
        );
        assert!(
            matches!(res, Err(Error::Cancelled)),
            "mid-stream cancel must surface as Cancelled, got {res:?}",
        );
        assert!(
            partial.len() <= chunk_len && partial.len() < total,
            "cancel must fold only the first chunk ({} frames, chunk_len {chunk_len}, total {total})",
            partial.len(),
        );
        assert_eq!(
            &full[..partial.len()],
            partial.as_slice(),
            "the folded frames must be a strict prefix of the uninterrupted run",
        );

        cancel.store(false, Ordering::Relaxed);
        let mut resumed = Vec::new();
        stream_decoded_chunks(
            &timestamps,
            chunk_len,
            paused(&cancel),
            decode_chunk,
            |f: &u64| {
                resumed.push(*f);
                Ok(())
            },
        )
        .expect("resumed stream");
        assert_eq!(
            resumed, full,
            "resume after cancel must be byte-identical to the uninterrupted run",
        );
    }

    #[test]
    fn plan_windows_span_bound_holds_for_strided_grid() {
        let strided = [0u64, 112_500, 225_000, 337_500];
        let windows = plan_windows(&strided);
        assert_partition(&strided, &windows);
        for w in &windows {
            assert_eq!(w.len(), 1, "wide-gap points must not batch: {w:?}");
        }
    }

    #[test]
    fn raw_gray_to_frames_splits_into_ordinal_tagged_frames() {
        let bytes: Vec<u8> = (0u8..12).collect();
        let frames = raw_gray_to_frames(&bytes, 2, 2).expect("frames");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].pixels, vec![0, 1, 2, 3]);
        assert_eq!(frames[0].timestamp_ms, 0);
        assert_eq!(frames[2].pixels, vec![8, 9, 10, 11]);
        assert_eq!(frames[2].timestamp_ms, 2);
    }

    #[test]
    fn raw_gray_to_frames_rejects_non_multiple_length() {
        let err = raw_gray_to_frames(&[0u8; 10], 2, 2).expect_err("ragged");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn raw_gray_to_frame_accepts_exact_length() {
        let frame = raw_gray_to_frame(vec![7u8; 12], 4, 3, 250).expect("frame");
        assert_eq!(frame.width, 4);
        assert_eq!(frame.height, 3);
        assert_eq!(frame.timestamp_ms, 250);
        assert_eq!(frame.pixels.len(), 12);
    }

    #[test]
    fn raw_gray_to_frame_rejects_length_mismatch() {
        let err = raw_gray_to_frame(vec![0u8; 11], 4, 3, 0).expect_err("short buffer");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn plan_anchors_on_zero_and_spreads() {
        assert_eq!(plan_fallback_timestamps(1000, 1), vec![0]);
        assert_eq!(plan_fallback_timestamps(6000, 4), vec![0, 2500, 5000]);
        assert_eq!(plan_fallback_timestamps(0, 4), Vec::<u64>::new());
        assert_eq!(plan_fallback_timestamps(1000, 0), Vec::<u64>::new());
    }

    #[allow(clippy::unnecessary_wraps)]
    fn fake_decode(ts: u64) -> Result<GrayscaleFrame> {
        Ok(GrayscaleFrame {
            width: 1,
            height: 1,
            timestamp_ms: ts,
            pixels: vec![(ts % 251) as u8],
        })
    }

    fn sequential_decode(timestamps: &[u64]) -> Vec<GrayscaleFrame> {
        timestamps
            .iter()
            .map(|&ts| fake_decode(ts).unwrap())
            .collect()
    }

    #[test]
    fn decode_grid_deterministic_across_capacities() {
        let ts: Vec<u64> = vec![0, 2500, 5000, 7500, 10000, 12500];
        let expected = sequential_decode(&ts);

        for cap in [1, 2, 4, 16] {
            let conc = DecodeConcurrency::new(cap);
            let got = decode_grid(&ts, &conc, Cancel::default(), fake_decode)
                .unwrap_or_else(|e| panic!("decode_grid failed at cap={cap}: {e}"));
            assert_eq!(got.len(), expected.len(), "cap={cap}: frame count mismatch");
            for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
                assert_eq!(
                    g.timestamp_ms, e.timestamp_ms,
                    "cap={cap} frame[{i}]: timestamp mismatch"
                );
                assert_eq!(g.pixels, e.pixels, "cap={cap} frame[{i}]: pixel mismatch");
            }
        }
    }

    #[test]
    fn decode_grid_empty_timestamps_returns_ok_empty() {
        let conc = DecodeConcurrency::new(4);
        let got =
            decode_grid(&[], &conc, Cancel::default(), fake_decode).expect("empty should be Ok");
        assert!(got.is_empty());
    }

    #[test]
    fn decode_grid_error_propagates() {
        let ts: Vec<u64> = vec![0, 2500, 5000, 7500];
        let error_at = 5000u64;
        let decode = |t: u64| -> Result<GrayscaleFrame> {
            if t == error_at {
                Err(Error::Decode(format!("injected error at ts={t}")))
            } else {
                fake_decode(t)
            }
        };
        for cap in [1, 2, 4] {
            let conc = DecodeConcurrency::new(cap);
            let result = decode_grid(&ts, &conc, Cancel::default(), decode);
            assert!(result.is_err(), "cap={cap}: expected Err but got Ok");
            match result {
                Err(Error::Decode(_)) => {}
                other => panic!("cap={cap}: expected Error::Decode, got {other:?}"),
            }
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn fake_window_decode(window: &[u64]) -> Result<Vec<GrayscaleFrame>> {
        Ok(window.iter().map(|&ts| fake_decode(ts).unwrap()).collect())
    }

    #[test]
    fn decode_windows_grid_flattens_in_window_index_order_across_capacities() {
        let grid: Vec<u64> = (0..12).map(|i| i * 2500).collect();
        let windows = plan_windows(&grid);
        assert!(windows.len() > 1, "fixture must span several windows");
        let expected = sequential_decode(&grid);

        for cap in [1, 2, 4, 16] {
            let conc = DecodeConcurrency::new(cap);
            let got = decode_windows_grid(&windows, &conc, Cancel::default(), fake_window_decode)
                .unwrap_or_else(|e| panic!("decode_windows_grid failed at cap={cap}: {e}"));
            assert_eq!(got.len(), expected.len(), "cap={cap}: frame count mismatch");
            for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
                assert_eq!(
                    g.timestamp_ms, e.timestamp_ms,
                    "cap={cap} frame[{i}]: timestamp mismatch"
                );
                assert_eq!(g.pixels, e.pixels, "cap={cap} frame[{i}]: pixel mismatch");
            }
        }
    }

    #[test]
    fn decode_windows_grid_empty_returns_ok_empty() {
        let conc = DecodeConcurrency::new(4);
        let got = decode_windows_grid(&[], &conc, Cancel::default(), fake_window_decode)
            .expect("empty should be Ok");
        assert!(got.is_empty());
    }

    #[test]
    fn decode_windows_grid_error_propagates() {
        let grid: Vec<u64> = (0..12).map(|i| i * 2500).collect();
        let windows = plan_windows(&grid);
        let decode = |window: &[u64]| -> Result<Vec<GrayscaleFrame>> {
            if window.contains(&5000) {
                Err(Error::Decode("injected window error".into()))
            } else {
                fake_window_decode(window)
            }
        };
        for cap in [1, 2, 4] {
            let conc = DecodeConcurrency::new(cap);
            let result = decode_windows_grid(&windows, &conc, Cancel::default(), decode);
            assert!(result.is_err(), "cap={cap}: expected Err but got Ok");
            match result {
                Err(Error::Decode(_)) => {}
                other => panic!("cap={cap}: expected Error::Decode, got {other:?}"),
            }
        }
    }

    #[test]
    fn plan_sidecar_subchunks_partitions_in_order_and_bounds_count() {
        let pts: Vec<u64> = (0..16u64).map(|i| i * 100).collect();
        let subs = plan_sidecar_subchunks(&pts, 8);
        let flat: Vec<u64> = subs.iter().flat_map(|s| s.iter().copied()).collect();
        assert_eq!(
            flat, pts,
            "sub-chunks must partition the chunk in grid order"
        );
        assert!(
            subs.len() <= 8,
            "at most cap sub-chunks, got {}",
            subs.len()
        );
        assert!(subs.iter().all(|s| !s.is_empty()), "no empty sub-chunk");
        let short = [0u64, 2500, 5000];
        let subs2 = plan_sidecar_subchunks(&short, 8);
        assert_eq!(subs2.len(), short.len());
        let subs3 = plan_sidecar_subchunks(&pts, 1);
        assert_eq!(subs3.len(), 1);
        assert_eq!(subs3[0], pts.as_slice());
        assert!(!plan_sidecar_subchunks(&pts, 0).is_empty());
        assert!(plan_sidecar_subchunks(&[], 8).is_empty());
    }

    #[test]
    fn sidecar_subchunk_fanout_is_byte_identical_to_serial_across_caps() {
        let grid: Vec<u64> = (0..40u64).map(|i| i * 2500).collect();
        let expected = sequential_decode(&grid);
        for cap in [1usize, 2, 4, 8, 40] {
            let conc = DecodeConcurrency::new(cap);
            let chunk_len = stream_chunk_len(cap);
            let mut got = Vec::new();
            stream_decoded_chunks(
                &grid,
                chunk_len,
                Cancel::default(),
                |chunk_points| {
                    let subs = plan_sidecar_subchunks(chunk_points, conc.capacity());
                    decode_windows_grid(&subs, &conc, Cancel::default(), |sub| {
                        Ok(sub.iter().map(|&ts| fake_decode(ts).unwrap()).collect())
                    })
                },
                |f: &GrayscaleFrame| {
                    got.push(f.clone());
                    Ok(())
                },
            )
            .unwrap_or_else(|e| panic!("cap={cap}: {e}"));
            assert_eq!(got.len(), expected.len(), "cap={cap}: frame count mismatch");
            for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
                assert_eq!(
                    g.timestamp_ms, e.timestamp_ms,
                    "cap={cap} frame[{i}]: ts mismatch"
                );
                assert_eq!(g.pixels, e.pixels, "cap={cap} frame[{i}]: pixel mismatch");
            }
        }
    }

    #[test]
    fn sidecar_subchunk_fanout_mid_cancel_is_cancelled_and_resume_is_byte_identical() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let grid: Vec<u64> = (0..16u64).map(|i| i * 2500).collect();
        let conc = DecodeConcurrency::serial();
        let subs = plan_sidecar_subchunks(&grid, conc.capacity());

        let full = decode_windows_grid(&subs, &conc, Cancel::default(), |sub| {
            Ok(sub.iter().map(|&ts| fake_decode(ts).unwrap()).collect())
        })
        .expect("uninterrupted fan");
        assert_eq!(full, sequential_decode(&grid));

        let cancel = AtomicBool::new(false);
        let decoded = AtomicUsize::new(0);
        let res = decode_windows_grid(&subs, &conc, paused(&cancel), |sub| {
            if decoded.fetch_add(1, Ordering::Relaxed) == 0 {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(sub.iter().map(|&ts| fake_decode(ts).unwrap()).collect())
        });
        assert!(
            matches!(res, Err(Error::Cancelled)),
            "mid-fan cancel must surface as the typed Cancelled, got {res:?}",
        );

        cancel.store(false, Ordering::Relaxed);
        let resumed = decode_windows_grid(&subs, &conc, paused(&cancel), |sub| {
            Ok(sub.iter().map(|&ts| fake_decode(ts).unwrap()).collect())
        })
        .expect("resumed fan");
        assert_eq!(resumed, full, "resume after cancel must be byte-identical");
    }

    #[cfg(windows)]
    fn stub_ffmpeg(dir: &Path, frame_file: &Path, batch_should_fail: bool) -> FfmpegBinaries {
        let batch_action = if batch_should_fail {
            "exit /b 1".to_string()
        } else {
            format!(
                "type \"{}\" & type \"{}\" & exit /b 0",
                frame_file.display(),
                frame_file.display()
            )
        };
        let script = format!(
            "@echo off\r\n\
             echo %* | findstr /C:\"-copyts\" >nul\r\n\
             if %errorlevel%==0 (\r\n  {batch_action}\r\n) else (\r\n  type \"{}\"\r\n  exit /b 0\r\n)\r\n",
            frame_file.display(),
        );
        let ff = dir.join("ffmpeg.cmd");
        std::fs::write(&ff, script.as_bytes()).expect("write stub ffmpeg.cmd");
        FfmpegBinaries::new(ff.clone(), ff)
    }

    fn missing_ffmpeg() -> FfmpegBinaries {
        FfmpegBinaries::new(
            PathBuf::from("/no/such/vidcull-stub-ffmpeg-batch"),
            PathBuf::from("/no/such/vidcull-stub-ffprobe-batch"),
        )
    }

    #[test]
    #[cfg(windows)]
    fn decode_window_batch_falls_back_per_frame_when_batch_exits_nonzero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let frame = dir.path().join("frame.raw");
        std::fs::write(&frame, [11u8, 22, 33, 44]).expect("write frame");
        let bins = stub_ffmpeg(dir.path(), &frame, true);

        let window = [2500u64, 5000, 7500];
        let frames = decode_window_batch(
            &bins,
            Path::new("/clip.webm"),
            &window,
            2,
            2,
            Cancel::default(),
        )
        .expect("exit≠0 batch must fall back per-frame, not error");

        assert_eq!(frames.len(), window.len());
        for (f, &ts) in frames.iter().zip(window.iter()) {
            assert_eq!(f.timestamp_ms, ts, "per-frame fallback must stamp grid ts");
            assert_eq!(f.pixels, vec![11, 22, 33, 44]);
            assert_eq!((f.width, f.height), (2, 2));
        }
    }

    #[test]
    #[cfg(windows)]
    fn decode_window_batch_falls_back_per_frame_when_batch_output_is_ragged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let frame = dir.path().join("frame.raw");
        std::fs::write(&frame, [9u8, 9, 9, 9]).expect("write frame");
        let bins = stub_ffmpeg(dir.path(), &frame, false);

        let window = [0u64, 2500, 5000];
        let frames = decode_window_batch(
            &bins,
            Path::new("/clip.webm"),
            &window,
            2,
            2,
            Cancel::default(),
        )
        .expect("count-mismatch batch must fall back per-frame");
        assert_eq!(frames.len(), window.len());
        for (f, &ts) in frames.iter().zip(window.iter()) {
            assert_eq!(f.timestamp_ms, ts);
            assert_eq!(f.pixels, vec![9, 9, 9, 9]);
        }
    }

    #[test]
    fn decode_window_batch_propagates_error_when_per_frame_also_fails() {
        let bins = missing_ffmpeg();
        let window = [0u64, 2500];
        let err = decode_window_batch(
            &bins,
            Path::new("/clip.webm"),
            &window,
            2,
            2,
            Cancel::default(),
        )
        .expect_err("undecodable window must error after per-frame also fails");
        assert!(
            matches!(err, Error::Io(_) | Error::Decode(_)),
            "expected a spawn/decode error from the per-frame fallback, got {err:?}"
        );
    }

    #[test]
    fn decode_window_batch_empty_window_is_ok_empty_without_spawning() {
        let bins = missing_ffmpeg();
        let frames =
            decode_window_batch(&bins, Path::new("/clip.webm"), &[], 2, 2, Cancel::default())
                .expect("empty window must be Ok(empty)");
        assert!(frames.is_empty());
    }

    #[test]
    fn decode_window_batch_zero_dimension_errors_before_spawning() {
        let bins = missing_ffmpeg();
        let err = decode_window_batch(
            &bins,
            Path::new("/clip.webm"),
            &[0],
            0,
            10,
            Cancel::default(),
        )
        .expect_err("zero dimension must error");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    const BATCHABLE_PX: u64 = 320 * 180;

    fn batches(
        codec: &Codec,
        fps_x1000: Option<u32>,
        has_b_frames: Option<bool>,
        frame_px: u64,
    ) -> bool {
        use_batch_path(
            &ContainerKind::Mp4,
            codec,
            fps_x1000,
            has_b_frames,
            frame_px,
        )
    }

    #[test]
    fn use_batch_path_only_for_intervals_below_the_grid() {
        for codec in [Codec::Av1, Codec::Vp9] {
            assert!(
                batches(&codec, Some(24_000), None, BATCHABLE_PX),
                "{codec:?} 24fps"
            );
            assert!(
                batches(&codec, Some(29_970), None, BATCHABLE_PX),
                "{codec:?} 29.97fps"
            );
            assert!(
                batches(&codec, Some(500), None, BATCHABLE_PX),
                "{codec:?} 0.5fps"
            );
            assert!(
                !batches(&codec, Some(400), None, BATCHABLE_PX),
                "{codec:?} 0.4fps"
            );
            assert!(
                !batches(&codec, Some(250), None, BATCHABLE_PX),
                "{codec:?} 0.25fps"
            );
            assert!(
                !batches(&codec, None, None, BATCHABLE_PX),
                "{codec:?} unknown"
            );
            assert!(
                !batches(&codec, Some(0), None, BATCHABLE_PX),
                "{codec:?} zero"
            );
        }
    }

    #[test]
    fn use_batch_path_excludes_bframe_and_unmodelled_codecs() {
        for bf in [None, Some(false), Some(true)] {
            assert!(
                !batches(&Codec::Mpeg2, Some(24_000), bf, BATCHABLE_PX),
                "mpeg2 {bf:?}"
            );
            assert!(
                !batches(
                    &Codec::Other("prores".into()),
                    Some(24_000),
                    bf,
                    BATCHABLE_PX
                ),
                "prores {bf:?}"
            );
        }
    }

    #[test]
    fn use_batch_path_h264_h265_only_without_b_frames() {
        for codec in [Codec::H264, Codec::H265] {
            assert!(
                batches(&codec, Some(24_000), Some(false), BATCHABLE_PX),
                "{codec:?} bf=false 24fps"
            );
            assert!(
                !batches(&codec, Some(24_000), Some(true), BATCHABLE_PX),
                "{codec:?} bf=true 24fps"
            );
            assert!(
                !batches(&codec, Some(24_000), None, BATCHABLE_PX),
                "{codec:?} bf=unknown 24fps"
            );
            assert!(
                !batches(&codec, Some(250), Some(false), BATCHABLE_PX),
                "{codec:?} bf=false 0.25fps"
            );
        }
    }

    #[test]
    fn use_batch_path_excludes_decode_dominated_large_frames() {
        let threshold = BATCH_MAX_FRAME_PX;
        let batchable = [
            (Codec::Av1, None),
            (Codec::Vp9, None),
            (Codec::H264, Some(false)),
            (Codec::H265, Some(false)),
        ];
        for (codec, bf) in &batchable {
            assert!(
                batches(codec, Some(24_000), *bf, threshold - 1),
                "{codec:?} just-below threshold must batch"
            );
            assert!(
                !batches(codec, Some(24_000), *bf, threshold),
                "{codec:?} at threshold must be per-frame"
            );
            assert!(
                batches(codec, Some(25_000), *bf, 720 * 960),
                "{codec:?} 720x960 control must batch"
            );
            assert!(
                !batches(codec, Some(25_000), *bf, 2160 * 2794),
                "{codec:?} V1 2160x2794 must be per-frame"
            );
        }
    }

    #[test]
    fn use_batch_path_matroska_containers_always_per_frame() {
        let batch_eligible = [
            (Codec::Av1, None),
            (Codec::Vp9, None),
            (Codec::H264, Some(false)),
            (Codec::H265, Some(false)),
        ];
        for (codec, bf) in &batch_eligible {
            assert!(
                use_batch_path(&ContainerKind::Mp4, codec, Some(24_000), *bf, BATCHABLE_PX),
                "{codec:?} must batch on MP4 (control)"
            );
            for matroska in [ContainerKind::Mkv, ContainerKind::WebM] {
                assert!(
                    !use_batch_path(&matroska, codec, Some(24_000), *bf, BATCHABLE_PX),
                    "{codec:?} must route per-frame on {matroska:?}"
                );
            }
        }
    }

    #[test]
    fn use_batch_path_non_matroska_containers_unaffected() {
        for container in [
            ContainerKind::Mp4,
            ContainerKind::Mov,
            ContainerKind::ThreeGp,
        ] {
            assert!(
                use_batch_path(&container, &Codec::Vp9, Some(24_000), None, BATCHABLE_PX),
                "VP9 24fps must batch on {container:?}"
            );
        }
        assert!(
            use_batch_path(
                &ContainerKind::UnsupportedFastPath("ts".into()),
                &Codec::Vp9,
                Some(24_000),
                None,
                BATCHABLE_PX
            ),
            "VP9 still batches on an unmodelled non-Matroska container (.ts) — not gated by "
        );
        assert!(
            !use_batch_path(
                &ContainerKind::UnsupportedFastPath("ts".into()),
                &Codec::Mpeg2,
                Some(24_000),
                None,
                BATCHABLE_PX
            ),
            "MPEG-2 stays per-frame by codec gate regardless of container"
        );
    }
}
