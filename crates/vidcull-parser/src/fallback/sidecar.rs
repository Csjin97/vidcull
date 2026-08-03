use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use vidcull_core::{Error, Result};

use super::binary::{EXE_SUFFIX, FfmpegBinaries};
use super::timeout::{BATCH_DECODE_TIMEOUT_SECS, effective_timeout, run_with_timeout};
use crate::sparse::GrayscaleFrame;

const SIDECAR_STEM: &str = "vidcull-decode-sidecar";
const ENV_SIDECAR: &str = "VIDCULL_DECODE_SIDECAR";
const HEADER_TAG: &str = "VIDCULL-SIDECAR-1";

const PROTOCOL_FAMILY: &str = "VIDCULL-SIDECAR-";

const SUPPORTED_VERSIONS: &[u32] = &[1];

const SIDECAR_LOAD_FAILURE_LIMIT: usize = 3;

static SIDECAR_DISABLED: AtomicBool = AtomicBool::new(false);

static SIDECAR_LOAD_FAILURES: AtomicUsize = AtomicUsize::new(0);

static SIDECAR_OK_CHUNKS: AtomicUsize = AtomicUsize::new(0);
static SIDECAR_BAD_HEADER_CHURN: AtomicUsize = AtomicUsize::new(0);
static SIDECAR_ABSENT: AtomicUsize = AtomicUsize::new(0);
static SIDECAR_VERSION_MISMATCH: AtomicUsize = AtomicUsize::new(0);
static SIDECAR_HEALTHY_LOGGED: AtomicBool = AtomicBool::new(false);
static SIDECAR_ABSENT_LOGGED: AtomicBool = AtomicBool::new(false);

fn resource_log_enabled() -> bool {
    std::env::var_os("VIDCULL_RESOURCE_LOG").is_some()
}

fn redacted_stdout_prefix(stdout: &[u8], path: &Path) -> String {
    const MAX: usize = 120;
    let end = stdout
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(stdout.len());
    let slice = &stdout[..end.min(MAX)];
    let lossy = String::from_utf8_lossy(slice);
    super::decode::scrub_input_path(&lossy, path)
}

#[must_use]
pub(crate) fn resolve_sidecar(bins: &FfmpegBinaries) -> Option<PathBuf> {
    if SIDECAR_DISABLED.load(Ordering::Acquire) {
        return None;
    }
    if let Some(explicit) = std::env::var_os(ENV_SIDECAR) {
        let candidate = PathBuf::from(explicit);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let name = format!("{SIDECAR_STEM}{EXE_SUFFIX}");
    if let Some(dir) = bins.ffmpeg().parent() {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    note_sidecar_absent();
    None
}

pub(crate) fn decode_chunk_gray(
    exe: &Path,
    path: &Path,
    timestamps: &[u64],
    width: u32,
    height: u32,
) -> Result<Vec<GrayscaleFrame>> {
    if timestamps.is_empty() {
        return Ok(Vec::new());
    }
    let ts_arg = timestamps
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new(exe);
    cmd.arg(path).arg(&ts_arg);
    let output = run_with_timeout(
        &mut cmd,
        effective_timeout(BATCH_DECODE_TIMEOUT_SECS),
        "sidecar",
    )?;
    if !output.status.success() {
        if classify_sidecar_failure(&output.stdout) == SidecarFailure::LoadFailure {
            let prev = SIDECAR_LOAD_FAILURES.fetch_add(1, Ordering::AcqRel);
            let count = prev + 1;
            if count >= SIDECAR_LOAD_FAILURE_LIMIT
                && disable_sidecar_for_session()
                && resource_log_enabled()
            {
                tracing::info!(
                    stage = "sidecar",
                    mode = "load_failure_disabled",
                    load_failures = count,
                    received = %redacted_stdout_prefix(&output.stdout, path),
                    "sidecar latch-disabled after consecutive load failures (no protocol header)",
                );
            }
        }
        return Err(Error::Decode(format!(
            "decode sidecar exited unsuccessfully ({})",
            output.status
        )));
    }
    match peek_header_tag(&output.stdout).map(classify_header) {
        Some(HeaderClass::VersionMismatch) => {
            let tag = peek_header_tag(&output.stdout).unwrap_or("");
            note_version_mismatch(&output.stdout, path);
            return Err(Error::Decode(format!(
                "decode sidecar: protocol version mismatch (got '{tag}', daemon supports \
                 {SUPPORTED_VERSIONS:?}); falling back to per-frame decode"
            )));
        }
        Some(HeaderClass::Foreign) | None => {
            note_bad_header_churn(&output.stdout, path);
        }
        Some(HeaderClass::Supported) => {}
    }
    let result = parse_sidecar_output(&output.stdout, timestamps, width, height);
    if result.is_ok() {
        SIDECAR_LOAD_FAILURES.store(0, Ordering::Release);
        note_sidecar_healthy();
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarFailure {
    LoadFailure,
    Transient,
}

fn classify_sidecar_failure(stdout: &[u8]) -> SidecarFailure {
    if stdout.starts_with(PROTOCOL_FAMILY.as_bytes()) {
        SidecarFailure::Transient
    } else {
        SidecarFailure::LoadFailure
    }
}

fn peek_header_tag(stdout: &[u8]) -> Option<&str> {
    let end = stdout
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(stdout.len());
    let line = std::str::from_utf8(&stdout[..end]).ok()?;
    line.split_whitespace().next()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderClass {
    Supported,
    VersionMismatch,
    Foreign,
}

fn classify_header(tag: &str) -> HeaderClass {
    if tag == HEADER_TAG {
        return HeaderClass::Supported;
    }
    match tag.strip_prefix(PROTOCOL_FAMILY) {
        Some(ver)
            if ver
                .parse::<u32>()
                .is_ok_and(|v| SUPPORTED_VERSIONS.contains(&v)) =>
        {
            HeaderClass::Supported
        }
        Some(_) => HeaderClass::VersionMismatch,
        None => HeaderClass::Foreign,
    }
}

fn disable_sidecar_for_session() -> bool {
    let newly_disabled = !SIDECAR_DISABLED.swap(true, Ordering::Release);
    if newly_disabled {
        tracing::warn!(
            "decode sidecar unavailable (likely missing libav DLLs); disabling for this \
             session, falling back to per-frame ffmpeg"
        );
    }
    newly_disabled
}

fn note_sidecar_healthy() {
    let n = SIDECAR_OK_CHUNKS.fetch_add(1, Ordering::Relaxed) + 1;
    if resource_log_enabled() && !SIDECAR_HEALTHY_LOGGED.swap(true, Ordering::Release) {
        tracing::info!(
            stage = "sidecar",
            mode = "healthy",
            ok_chunks = n,
            "sidecar decode path operating normally (baseline)",
        );
    }
}

fn note_sidecar_absent() {
    let n = SIDECAR_ABSENT.fetch_add(1, Ordering::Relaxed) + 1;
    if resource_log_enabled() && !SIDECAR_ABSENT_LOGGED.swap(true, Ordering::Release) {
        tracing::info!(
            stage = "sidecar",
            mode = "absent",
            resolutions = n,
            "sidecar executable not found; using per-frame ffmpeg path",
        );
    }
}

fn note_bad_header_churn(stdout: &[u8], path: &Path) {
    let n = SIDECAR_BAD_HEADER_CHURN.fetch_add(1, Ordering::Relaxed) + 1;
    if resource_log_enabled() {
        tracing::info!(
            stage = "sidecar",
            mode = "bad_header_churn",
            events = n,
            received = %redacted_stdout_prefix(stdout, path),
            "sidecar zero-exit with unrecognized header; per-frame fallback (churn)",
        );
    }
}

fn note_version_mismatch(stdout: &[u8], path: &Path) {
    let n = SIDECAR_VERSION_MISMATCH.fetch_add(1, Ordering::Relaxed) + 1;
    let newly_disabled = !SIDECAR_DISABLED.swap(true, Ordering::Release);
    if newly_disabled {
        tracing::warn!(
            "decode sidecar protocol version mismatch; disabling for this session and falling \
             back to per-frame ffmpeg (rebundle a vidcull-decode-sidecar matching this daemon)"
        );
    }
    if resource_log_enabled() {
        tracing::info!(
            stage = "sidecar",
            mode = "version_mismatch",
            events = n,
            latched = newly_disabled,
            received = %redacted_stdout_prefix(stdout, path),
            "sidecar emitted an unsupported protocol version",
        );
    }
}

fn parse_sidecar_output(
    stdout: &[u8],
    timestamps: &[u64],
    width: u32,
    height: u32,
) -> Result<Vec<GrayscaleFrame>> {
    let newline = stdout
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| Error::Decode("decode sidecar: missing header line".into()))?;
    let header = std::str::from_utf8(&stdout[..newline])
        .map_err(|_| Error::Decode("decode sidecar: non-utf8 header".into()))?;
    let mut fields = header.split_whitespace();
    if classify_header(fields.next().unwrap_or("")) != HeaderClass::Supported {
        return Err(Error::Decode("decode sidecar: bad header tag".into()));
    }
    let parse_u32 = |o: Option<&str>, what: &str| -> Result<u32> {
        o.and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| Error::Decode(format!("decode sidecar: bad header {what}")))
    };
    let reported_w = parse_u32(fields.next(), "width")?;
    let reported_h = parse_u32(fields.next(), "height")?;
    let reported_n = fields
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| Error::Decode("decode sidecar: bad header count".into()))?;
    if reported_w != width || reported_h != height {
        return Err(Error::Decode(format!(
            "decode sidecar: reported {reported_w}x{reported_h} != expected {width}x{height}"
        )));
    }
    if reported_n != timestamps.len() {
        return Err(Error::Decode(format!(
            "decode sidecar: reported {reported_n} frames != requested {}",
            timestamps.len()
        )));
    }

    let frame_len = width as usize * height as usize;
    let mut body = &stdout[newline + 1..];
    let mut frames = Vec::with_capacity(timestamps.len());
    for &ts in timestamps {
        let (status, rest) = body
            .split_first()
            .ok_or_else(|| Error::Decode("decode sidecar: truncated status".into()))?;
        body = rest;
        match status {
            1 => {
                if body.len() < frame_len {
                    return Err(Error::Decode("decode sidecar: truncated frame body".into()));
                }
                let (pixels, rest) = body.split_at(frame_len);
                body = rest;
                frames.push(GrayscaleFrame {
                    width,
                    height,
                    timestamp_ms: ts,
                    pixels: pixels.to_vec(),
                });
            }
            0 => {
                return Err(Error::Decode(format!(
                    "decode sidecar: frame decode failed at ts={ts}ms"
                )));
            }
            other => {
                return Err(Error::Decode(format!(
                    "decode sidecar: invalid status byte {other}"
                )));
            }
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(width: u32, height: u32, frames: &[Option<u8>]) -> Vec<u8> {
        let mut out = format!("{HEADER_TAG} {width} {height} {}\n", frames.len()).into_bytes();
        let len = (width * height) as usize;
        for f in frames {
            match f {
                Some(fill) => {
                    out.push(1);
                    out.extend(std::iter::repeat_n(*fill, len));
                }
                None => out.push(0),
            }
        }
        out
    }

    #[test]
    fn parses_well_formed_stream_into_frames() {
        let ts = [1000u64, 2000, 3000];
        let bytes = stream(2, 2, &[Some(10), Some(20), Some(30)]);
        let frames = parse_sidecar_output(&bytes, &ts, 2, 2).expect("parse");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].timestamp_ms, 1000);
        assert_eq!(frames[0].pixels, vec![10, 10, 10, 10]);
        assert_eq!(frames[2].pixels, vec![30, 30, 30, 30]);
        assert!(frames.iter().all(|f| f.width == 2 && f.height == 2));
    }

    #[test]
    fn dimension_mismatch_is_error() {
        let bytes = stream(4, 4, &[Some(0)]);
        let err = parse_sidecar_output(&bytes, &[0], 2, 2).expect_err("dim mismatch");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn count_mismatch_is_error() {
        let bytes = stream(2, 2, &[Some(0)]);
        let err = parse_sidecar_output(&bytes, &[0, 1], 2, 2).expect_err("count mismatch");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn failed_status_byte_is_error() {
        let bytes = stream(2, 2, &[None]);
        let err = parse_sidecar_output(&bytes, &[7000], 2, 2).expect_err("status 0");
        assert!(
            err.to_string().contains("7000"),
            "should name the failing ts: {err}"
        );
    }

    #[test]
    fn truncated_frame_body_is_error() {
        let mut bytes = format!("{HEADER_TAG} 2 2 1\n").into_bytes();
        bytes.push(1);
        bytes.extend([5, 5]);
        let err = parse_sidecar_output(&bytes, &[0], 2, 2).expect_err("truncated");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn missing_header_is_error() {
        let err = parse_sidecar_output(b"no newline here", &[0], 2, 2).expect_err("no header");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn bad_header_tag_is_error() {
        let err = parse_sidecar_output(b"WRONG-TAG 2 2 1\n\x01", &[0], 2, 2).expect_err("bad tag");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    fn load_failure_classification_disables_only_on_missing_header() {
        assert_eq!(
            classify_sidecar_failure(b""),
            SidecarFailure::LoadFailure,
            "empty stdout (process never ran) is a load failure",
        );
        assert_eq!(
            classify_sidecar_failure(b"some libav error to stderr-ish garbage"),
            SidecarFailure::LoadFailure,
            "no header tag means the binary died before emitting protocol output",
        );
    }

    #[test]
    fn transient_failure_classification_keeps_sidecar_enabled() {
        let header_only = format!("{HEADER_TAG} 2 2 1\n").into_bytes();
        assert_eq!(
            classify_sidecar_failure(&header_only),
            SidecarFailure::Transient,
            "a stream that emitted the header ran the binary fine",
        );
        let mut header_with_status = format!("{HEADER_TAG} 2 2 1\n").into_bytes();
        header_with_status.push(0);
        assert_eq!(
            classify_sidecar_failure(&header_with_status),
            SidecarFailure::Transient,
        );
    }

    #[test]
    fn empty_timestamps_short_circuits_without_spawn() {
        let frames =
            decode_chunk_gray(Path::new("/no/such/sidecar"), Path::new("x.mkv"), &[], 2, 2)
                .expect("empty is ok");
        assert!(frames.is_empty());
    }

    #[test]
    fn load_failure_classification_triggers_disable_path() {
        assert_eq!(
            classify_sidecar_failure(b""),
            SidecarFailure::LoadFailure,
            "empty stdout must be LoadFailure — the trigger for session disable"
        );
        assert_eq!(
            classify_sidecar_failure(b"The program can't start because avcodec-61.dll is missing"),
            SidecarFailure::LoadFailure,
            "DLL-error text without header tag must be LoadFailure"
        );
    }

    #[test]
    fn disable_for_session_sets_global_flag() {
        disable_sidecar_for_session();
        assert!(
            SIDECAR_DISABLED.load(Ordering::Acquire),
            "SIDECAR_DISABLED must be true after disable_for_session; \
             reverting the swap() call would leave it false and re-enable storms"
        );
    }

    #[test]
    fn disable_for_session_is_idempotent() {
        disable_sidecar_for_session();
        disable_sidecar_for_session();
        assert!(
            SIDECAR_DISABLED.load(Ordering::Acquire),
            "SIDECAR_DISABLED must remain true after repeated disable calls"
        );
    }

    #[test]
    fn load_failure_chain_sets_disabled_exactly_once() {
        let bad_stdout: &[u8] = b"";

        let failure_kind = classify_sidecar_failure(bad_stdout);
        assert_eq!(
            failure_kind,
            SidecarFailure::LoadFailure,
            "Step 1 failed: empty stdout must classify as LoadFailure"
        );

        if failure_kind == SidecarFailure::LoadFailure {
            disable_sidecar_for_session();
        }

        assert!(
            SIDECAR_DISABLED.load(Ordering::Acquire),
            "Step 3 failed: SIDECAR_DISABLED must be set after load-failure chain; \
             reverting disable_for_session() from the branch would leave it false"
        );

        let failure_kind2 = classify_sidecar_failure(bad_stdout);
        assert_eq!(failure_kind2, SidecarFailure::LoadFailure);
        disable_sidecar_for_session();
        assert!(
            SIDECAR_DISABLED.load(Ordering::Acquire),
            "Flag must remain set after second load-failure chunk (idempotent)"
        );
    }

    fn reset_failure_state() {
        SIDECAR_LOAD_FAILURES.store(0, Ordering::Release);
        SIDECAR_DISABLED.store(false, Ordering::Release);
        SIDECAR_OK_CHUNKS.store(0, Ordering::Release);
        SIDECAR_BAD_HEADER_CHURN.store(0, Ordering::Release);
        SIDECAR_ABSENT.store(0, Ordering::Release);
        SIDECAR_VERSION_MISMATCH.store(0, Ordering::Release);
        SIDECAR_HEALTHY_LOGGED.store(false, Ordering::Release);
        SIDECAR_ABSENT_LOGGED.store(false, Ordering::Release);
    }

    #[test]
    fn classify_header_only_rule_unchanged_after_5() {
        let with_header = format!("{HEADER_TAG} 2 2 1\n").into_bytes();
        assert_eq!(
            classify_sidecar_failure(&with_header),
            SidecarFailure::Transient,
            "header present must always be Transient"
        );
        assert_eq!(
            classify_sidecar_failure(b""),
            SidecarFailure::LoadFailure,
            "empty stdout must always be LoadFailure"
        );
        assert_eq!(
            classify_sidecar_failure(b"avcodec-61.dll is missing"),
            SidecarFailure::LoadFailure,
            "non-header text must be LoadFailure"
        );
    }

    #[test]
    fn counter_resets_on_success_before_limit() {
        reset_failure_state();

        for _ in 0..(SIDECAR_LOAD_FAILURE_LIMIT - 1) {
            let prev = SIDECAR_LOAD_FAILURES.fetch_add(1, Ordering::AcqRel);
            if prev + 1 >= SIDECAR_LOAD_FAILURE_LIMIT {
                disable_sidecar_for_session();
            }
        }
        assert!(
            !SIDECAR_DISABLED.load(Ordering::Acquire),
            "N-1 failures must not latch the sidecar"
        );
        assert_eq!(
            SIDECAR_LOAD_FAILURES.load(Ordering::Acquire),
            SIDECAR_LOAD_FAILURE_LIMIT - 1,
            "counter should be N-1 before success"
        );

        SIDECAR_LOAD_FAILURES.store(0, Ordering::Release);
        assert_eq!(
            SIDECAR_LOAD_FAILURES.load(Ordering::Acquire),
            0,
            "counter must be 0 after success reset"
        );
        assert!(
            !SIDECAR_DISABLED.load(Ordering::Acquire),
            "sidecar must remain enabled after success"
        );

        let prev = SIDECAR_LOAD_FAILURES.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= SIDECAR_LOAD_FAILURE_LIMIT {
            disable_sidecar_for_session();
        }
        assert!(
            !SIDECAR_DISABLED.load(Ordering::Acquire),
            "1 failure after reset must not latch"
        );
    }

    #[test]
    fn counter_latches_after_n_consecutive_load_failures() {
        reset_failure_state();

        for i in 0..SIDECAR_LOAD_FAILURE_LIMIT {
            let prev = SIDECAR_LOAD_FAILURES.fetch_add(1, Ordering::AcqRel);
            if prev + 1 >= SIDECAR_LOAD_FAILURE_LIMIT {
                disable_sidecar_for_session();
            }
            if i < SIDECAR_LOAD_FAILURE_LIMIT - 1 {
                assert!(
                    !SIDECAR_DISABLED.load(Ordering::Acquire),
                    "should not latch before reaching limit (i={i})"
                );
            }
        }
        assert!(
            SIDECAR_DISABLED.load(Ordering::Acquire),
            "must latch after {SIDECAR_LOAD_FAILURE_LIMIT} consecutive load failures"
        );
    }

    #[test]
    fn classify_header_accepts_current_supported_version() {
        assert_eq!(classify_header(HEADER_TAG), HeaderClass::Supported);
        assert_eq!(classify_header("VIDCULL-SIDECAR-1"), HeaderClass::Supported);
    }

    #[test]
    fn classify_header_flags_version_drift_as_mismatch() {
        assert_eq!(
            classify_header("VIDCULL-SIDECAR-2"),
            HeaderClass::VersionMismatch
        );
        assert_eq!(
            classify_header("VIDCULL-SIDECAR-99"),
            HeaderClass::VersionMismatch
        );
        assert_eq!(
            classify_header("VIDCULL-SIDECAR-1x"),
            HeaderClass::VersionMismatch
        );
        assert_eq!(
            classify_header("VIDCULL-SIDECAR-"),
            HeaderClass::VersionMismatch
        );
    }

    #[test]
    fn classify_header_treats_non_family_tags_as_foreign() {
        assert_eq!(classify_header("WRONG-TAG"), HeaderClass::Foreign);
        assert_eq!(classify_header(""), HeaderClass::Foreign);
        assert_eq!(classify_header("VIDCULL-OTHER-1"), HeaderClass::Foreign);
    }

    #[test]
    fn version_drift_nonzero_exit_is_transient_not_load_failure() {
        let drift = format!("{PROTOCOL_FAMILY}2 1920 1080 3\n").into_bytes();
        assert_eq!(
            classify_sidecar_failure(&drift),
            SidecarFailure::Transient,
            "drifted-version header (family present) must NOT count as a load failure",
        );
        assert_eq!(
            classify_sidecar_failure(b"avcodec-61.dll is missing"),
            SidecarFailure::LoadFailure,
        );
    }

    #[test]
    fn peek_header_tag_extracts_first_token() {
        assert_eq!(
            peek_header_tag(b"VIDCULL-SIDECAR-1 2 2 1\n\x01"),
            Some("VIDCULL-SIDECAR-1"),
        );
        assert_eq!(
            peek_header_tag(b"VIDCULL-SIDECAR-2 2 2 1"),
            Some("VIDCULL-SIDECAR-2")
        );
        assert_eq!(peek_header_tag(b""), None);
        assert_eq!(peek_header_tag(b"\n"), None);
    }

    #[test]
    fn note_version_mismatch_latches_without_touching_load_failures() {
        reset_failure_state();
        note_version_mismatch(b"VIDCULL-SIDECAR-2 2 2 1\n", Path::new("clip.mkv"));
        assert!(
            SIDECAR_DISABLED.load(Ordering::Acquire),
            "version mismatch must latch-disable (graceful degrade, stop churn)",
        );
        assert_eq!(SIDECAR_VERSION_MISMATCH.load(Ordering::Acquire), 1);
        assert_eq!(
            SIDECAR_LOAD_FAILURES.load(Ordering::Acquire),
            0,
            "version mismatch must NOT increment the load-failure latch counter",
        );
    }

    #[test]
    fn missing_header_nonzero_exit_still_load_failure() {
        assert_eq!(classify_sidecar_failure(b""), SidecarFailure::LoadFailure);
        assert_eq!(
            classify_sidecar_failure(b"The program can't start because avcodec-61.dll is missing"),
            SidecarFailure::LoadFailure,
        );
    }

    #[test]
    fn redacted_stdout_prefix_scrubs_input_path() {
        let path = Path::new("C:/Users/alice/holiday.mkv");
        let stdout = b"could not open C:/Users/alice/holiday.mkv for reading\nmore\n";
        let red = redacted_stdout_prefix(stdout, path);
        assert!(!red.contains("alice"), "username leaked: {red}");
        assert!(!red.contains("holiday"), "filename leaked: {red}");
        assert!(red.contains("<input>"), "placeholder missing: {red}");
        assert!(
            !red.contains("more"),
            "should only capture the first line: {red}"
        );
    }

    #[test]
    fn parse_rejects_unsupported_version_header() {
        let bytes = b"VIDCULL-SIDECAR-2 2 2 1\n\x01\x00\x00\x00\x00".to_vec();
        let err = parse_sidecar_output(&bytes, &[0], 2, 2).expect_err("unsupported version");
        assert!(matches!(err, Error::Decode(_)), "got {err:?}");
    }

    #[test]
    #[ignore = "needs built sidecar (VIDCULL_DECODE_SIDECAR) + ffmpeg + a real fixture (AV1_FIXTURE)"]
    fn sidecar_chunk_is_byte_identical_to_per_frame_ss() {
        let (Some(exe), Some(fixtures)) = (
            std::env::var_os("VIDCULL_DECODE_SIDECAR"),
            std::env::var("AV1_FIXTURE").ok(),
        ) else {
            eprintln!(
                "skip: set VIDCULL_DECODE_SIDECAR + AV1_FIXTURE (comma-separated paths, \
                 one per codec class) to run"
            );
            return;
        };
        let exe = PathBuf::from(exe);
        let bins = FfmpegBinaries::resolve().expect("ffmpeg for the -ss baseline");
        let mut checked = 0usize;
        for fixture in fixtures.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let path = PathBuf::from(fixture);
            let meta = super::super::probe::probe_fallback(&bins, &path)
                .unwrap_or_else(|e| panic!("probe {fixture}: {e}"));
            let (w, h) = (meta.resolution.width, meta.resolution.height);
            let dur = meta.duration.expect("fixture duration").as_millis();
            let timestamps: Vec<u64> = (1..=5u64).map(|i| dur * i / 7).collect();

            let via_sidecar = decode_chunk_gray(&exe, &path, &timestamps, w, h)
                .unwrap_or_else(|e| panic!("sidecar decode {fixture}: {e}"));
            assert_eq!(via_sidecar.len(), timestamps.len());
            for (i, &ts) in timestamps.iter().enumerate() {
                let baseline = super::super::decode::decode_frame_at(&bins, &path, ts, w, h)
                    .unwrap_or_else(|e| panic!("per-frame baseline {fixture} ts={ts}: {e}"));
                assert_eq!(via_sidecar[i].timestamp_ms, ts);
                assert_eq!(
                    via_sidecar[i].pixels, baseline.pixels,
                    "{fixture} ts={ts}ms: sidecar frame != per-frame -ss frame (§J violated)"
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "AV1_FIXTURE held no usable paths");
    }
}
