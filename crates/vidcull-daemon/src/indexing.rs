use vidcull_core::types::{Blake3Hash, FileId, NormalizedPath, Resolution, VideoDuration};
use vidcull_core::{Error, Result};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FileRecord, FilesRepo, Fingerprint, FingerprintsRepo, NewFile,
    PartialSkipMarker, RegroupQueueRepo, Task, TaskQueueRepo, TrustLevel,
};
use vidcull_fingerprint::format::{FORMAT_VERSION, decode_tier2, encode_tier1, encode_tier2};
use vidcull_fingerprint::{
    DEFAULT_BAR_LIMIT, GrayFrame, Tier1Builder, Tier2Builder, Tier2Fingerprint, TimedFrame,
    hash_file_cancellable, trim_uniform_borders,
};
use vidcull_matcher::cluster::{Cluster, build_clusters, summarize_clusters};
use vidcull_matcher::exact::rebuild_exact_groups;
use vidcull_matcher::near::{LshParams, rebuild_near_duplicate_groups_incremental};
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
};
use vidcull_matcher::partial::{AnchorParams, partial_clip_params};
use vidcull_matcher::ranking::assign_best_copies;
use vidcull_matcher::whole::{
    WholeFileCandidate, WholeFileParams, rebuild_whole_file_groups, scan_whole_file_candidates,
};
use vidcull_parser::fallback::{
    DecodeConcurrency, DecodePath, FallbackMetrics, FfmpegBinaries,
    decode_sparse_strided_with_streaming, probe_fallback_cancellable,
};
use vidcull_parser::mp4::read_mp4_tolerant_hashing_cancellable;
use vidcull_parser::mkv::probe_mkv_hashing_cancellable;
use vidcull_parser::{
    Cancel, ContainerKind, PreParsedMp4, VideoMetadata, container_kind_from_path, full_grid_len,
    probe_and_decode_sparse_budgets_streaming, probe_and_decode_sparse_budgets_streaming_preparsed,
};

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};

use crate::TaskHandler;
use crate::redact::{redact_fs_path, redact_path};
use crate::thumbnails::ThumbnailProvider;
use crate::watcher::{ChangeKind, ChangeTask, enqueue_changes};

fn fuse_hash_parse_enabled() -> bool {
    fuse_hash_parse_enabled_from(std::env::var("VIDCULL_FUSE_HASH_PARSE").ok().as_deref())
}

fn fuse_hash_parse_enabled_from(raw: Option<&str>) -> bool {
    !raw.map(str::trim)
        .is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
}

fn fusion_eligible(path: &std::path::Path) -> bool {
    matches!(
        container_kind_from_path(path),
        ContainerKind::Mp4
            | ContainerKind::Mov
            | ContainerKind::ThreeGp
            | ContainerKind::Mkv
            | ContainerKind::WebM
    )
}

fn hash_file_fusing_mp4_parse(
    path: &std::path::Path,
    cancel: Cancel<'_>,
) -> Result<(Blake3Hash, PreParsedMp4)> {
    if !(fuse_hash_parse_enabled() && fusion_eligible(path)) {
        let hash = hash_file_cancellable(path, || cancel.fired())?;
        return Ok((hash, PreParsedMp4::NotAttempted));
    }
    let mut hasher = blake3::Hasher::new();
    let kind = container_kind_from_path(path);
    let pre_parsed = match kind {
        ContainerKind::Mp4 | ContainerKind::Mov | ContainerKind::ThreeGp => {
            read_mp4_tolerant_hashing_cancellable(path, cancel, &mut |bytes| {
                hasher.update(bytes);
            })?
        }
        ContainerKind::Mkv | ContainerKind::WebM => {
            match probe_mkv_hashing_cancellable(path, kind, cancel, &mut |bytes| {
                hasher.update(bytes);
            })? {
                Some(metadata) => PreParsedMp4::MkvParsed(metadata),
                None => PreParsedMp4::MkvFailed,
            }
        }
        ContainerKind::UnsupportedFastPath(_) => unreachable!("fusion eligibility checked above"),
    };
    Ok((
        Blake3Hash::from_bytes(*hasher.finalize().as_bytes()),
        pre_parsed,
    ))
}

#[derive(Debug, Default)]
pub struct SingleFlight {
    in_flight: Mutex<HashSet<Blake3Hash>>,
    released: Condvar,
    shutdown: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

const SINGLE_FLIGHT_SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

impl SingleFlight {
    pub fn link_shutdown(&self, flag: Arc<std::sync::atomic::AtomicBool>) {
        *self.shutdown.lock().unwrap_or_else(PoisonError::into_inner) = Some(flag);
    }

    fn shutdown_requested(&self) -> bool {
        self.shutdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn begin(&self, hash: Blake3Hash, should_cancel: impl Fn() -> bool) -> InFlightGuard<'_> {
        let mut set = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while set.contains(&hash) {
            if self.shutdown_requested() || should_cancel() {
                break;
            }
            let (next, _timeout) = self
                .released
                .wait_timeout(set, SINGLE_FLIGHT_SHUTDOWN_POLL)
                .unwrap_or_else(PoisonError::into_inner);
            set = next;
        }
        set.insert(hash);
        InFlightGuard { flight: self, hash }
    }
}

struct InFlightGuard<'a> {
    flight: &'a SingleFlight,
    hash: Blake3Hash,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let mut set = self
            .flight
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        set.remove(&self.hash);
        self.flight.released.notify_all();
    }
}

#[derive(Debug)]
pub struct PartialDecodeGate {
    in_use: AtomicUsize,
    capacity: AtomicUsize,
}

impl PartialDecodeGate {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            in_use: AtomicUsize::new(0),
            capacity: AtomicUsize::new(capacity.max(1)),
        }
    }

    pub(crate) fn set_capacity(&self, capacity: usize) {
        self.capacity.store(capacity.max(1), Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> (usize, usize) {
        (
            self.in_use.load(Ordering::Relaxed),
            self.capacity.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn has_capacity(&self) -> bool {
        self.in_use.load(Ordering::Relaxed) < self.capacity.load(Ordering::Relaxed)
    }

    pub(crate) fn try_acquire(&self) -> Option<PartialGateGuard<'_>> {
        let mut current = self.in_use.load(Ordering::Relaxed);
        loop {
            if current >= self.capacity.load(Ordering::Relaxed) {
                return None;
            }
            match self.in_use.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(PartialGateGuard { gate: self }),
                Err(observed) => current = observed,
            }
        }
    }
}

pub(crate) struct PartialGateGuard<'a> {
    gate: &'a PartialDecodeGate,
}

impl Drop for PartialGateGuard<'_> {
    fn drop(&mut self) {
        self.gate.in_use.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct BaseDecodeGate {
    in_use: AtomicUsize,
    capacity: AtomicUsize,
}

impl BaseDecodeGate {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            in_use: AtomicUsize::new(0),
            capacity: AtomicUsize::new(capacity.max(1)),
        }
    }

    pub(crate) fn set_capacity(&self, capacity: usize) {
        self.capacity.store(capacity.max(1), Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> (usize, usize) {
        (
            self.in_use.load(Ordering::Relaxed),
            self.capacity.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn try_acquire(&self) -> Option<BaseGateGuard<'_>> {
        let mut current = self.in_use.load(Ordering::Relaxed);
        loop {
            if current >= self.capacity.load(Ordering::Relaxed) {
                return None;
            }
            match self.in_use.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(BaseGateGuard { gate: self }),
                Err(observed) => current = observed,
            }
        }
    }
}

pub(crate) struct BaseGateGuard<'a> {
    gate: &'a BaseDecodeGate,
}

impl Drop for BaseGateGuard<'_> {
    fn drop(&mut self) {
        self.gate.in_use.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone, Default)]
pub struct DecodeGateObserver {
    inner: Arc<OnceLock<GateHandles>>,
}

struct GateHandles {
    decode_conc: Arc<DecodeConcurrency>,
    base_gate: Arc<BaseDecodeGate>,
    partial_gate: Arc<PartialDecodeGate>,
    seq_read_gate: Arc<BaseDecodeGate>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeGateSnapshot {
    pub decode_conc_in_use: usize,
    pub decode_conc_cap: usize,
    pub decode_conc_waiters: usize,
    pub base_gate_in_use: usize,
    pub base_gate_cap: usize,
    pub partial_gate_in_use: usize,
    pub partial_gate_cap: usize,
    pub seq_read_in_use: usize,
    pub seq_read_cap: usize,
    pub active_decode_workers: usize,
}

impl DecodeGateObserver {
    fn publish(
        &self,
        decode_conc: Arc<DecodeConcurrency>,
        base_gate: Arc<BaseDecodeGate>,
        partial_gate: Arc<PartialDecodeGate>,
        seq_read_gate: Arc<BaseDecodeGate>,
    ) {
        let _ = self.inner.set(GateHandles {
            decode_conc,
            base_gate,
            partial_gate,
            seq_read_gate,
        });
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<DecodeGateSnapshot> {
        let h = self.inner.get()?;
        let (decode_conc_in_use, decode_conc_cap) = h.decode_conc.snapshot();
        let decode_conc_waiters = h.decode_conc.waiters();
        let (base_gate_in_use, base_gate_cap) = h.base_gate.snapshot();
        let (partial_gate_in_use, partial_gate_cap) = h.partial_gate.snapshot();
        let (seq_read_in_use, seq_read_cap) = h.seq_read_gate.snapshot();
        Some(DecodeGateSnapshot {
            decode_conc_in_use,
            decode_conc_cap,
            decode_conc_waiters,
            base_gate_in_use,
            base_gate_cap,
            partial_gate_in_use,
            partial_gate_cap,
            seq_read_in_use,
            seq_read_cap,
            active_decode_workers: base_gate_in_use + partial_gate_in_use,
        })
    }
}

pub(crate) const BASE_DECODE_CONCURRENCY: usize = 64;

pub(crate) const BASE_DECODE_GATE_BUSY_REASON: &str =
    "base-index decode gate at capacity (concurrent base decodes in flight)";

pub(crate) const SEQ_READ_CONCURRENCY: usize = 4;

pub(crate) const SEQ_READ_GATE_BUSY_REASON: &str =
    "sequential-read gate at capacity (concurrent full-file reads in flight)";

fn seq_read_gate_capacity() -> usize {
    seq_read_cap_from(
        std::env::var("VIDCULL_SEQ_READ_MAX").ok().as_deref(),
        SEQ_READ_CONCURRENCY,
    )
}

/// The seq-read gate must track the live core/throttle-derived `budget` the
/// same way `base_decode_gate`/`partial_gate`/`decode_conc` already do —
/// otherwise it stays pinned at the construction-time default forever and
/// silently caps ingestion concurrency (file hashing) at that fixed number
/// regardless of how many cores or workers are actually available.
/// `VIDCULL_SEQ_READ_MAX` still overrides it when a user wants a fixed cap.
fn seq_read_cap_for_budget(budget: usize) -> usize {
    seq_read_cap_from(
        std::env::var("VIDCULL_SEQ_READ_MAX").ok().as_deref(),
        budget.clamp(1, BASE_DECODE_CONCURRENCY),
    )
}

fn seq_read_cap_from(raw: Option<&str>, default: usize) -> usize {
    match raw.map(str::trim).and_then(|v| v.parse::<usize>().ok()) {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => default,
    }
}

pub const DEFAULT_DECODE_BUDGET: usize = 10000;

pub const DEFAULT_FALLBACK_DECODE_BUDGET: usize = 10000;

pub const DENSIFY_PRIORITY: i32 = -100;

pub const PARTIAL_PRIORITY: i32 = -200;

pub const PARTIAL_CADENCE: usize = 8;

fn partial_headroom_k() -> usize {
    partial_headroom_k_from(std::env::var("VIDCULL_PARTIAL_HEADROOM").ok().as_deref())
}

fn partial_headroom_k_from(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(2)
}

pub(crate) const PARTIAL_GATE_BUSY_REASON: &str =
    "partial-clip decode gate at capacity (concurrent partials in flight)";

const DEFAULT_GATE_HOLD_WARN_MS: u64 = 60_000;

fn env_knob<T>(var: &str, default: T) -> T
where
    T: std::str::FromStr + std::fmt::Display + Copy,
{
    match std::env::var(var).ok().and_then(|v| v.parse::<T>().ok()) {
        Some(v) => {
            tracing::info!(var, value = %v, "env knob override applied");
            v
        }
        None => default,
    }
}

fn gate_hold_warn_ms() -> u64 {
    static CACHED: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        env_knob("VIDCULL_GATE_HOLD_WARN_MS", DEFAULT_GATE_HOLD_WARN_MS)
    });
    *CACHED
}

fn gate_hold_is_excessive(held_ms: u64, warn_ms: u64) -> bool {
    held_ms > warn_ms
}

fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub const PARTIAL_MAX_DURATION_MS: u64 = 4 * 60 * 60 * 1000;

pub(crate) const PARTIAL_SKIP_REASON_UNSUPPORTED_CODEC: &str = "unsupported-codec";

pub(crate) const PARTIAL_SKIP_REASON_DECODE_FAILED: &str = "decode-failed";

pub(crate) const PARTIAL_SKIP_REASON_DURATION_CAP: &str = "duration-cap";

pub(crate) const PARTIAL_SKIP_REASON_UNPROBEABLE: &str = "unprobeable";

pub(crate) const PARTIAL_SKIP_REASON_NO_SCENES: &str = "no-scenes";

pub(crate) const PARTIAL_SKIP_REASON_EXACT_FULL_DUP: &str = "exact-full-dup";

pub(crate) const PARTIAL_SKIP_REASON_RETRY_EXHAUSTED: &str = "retry-exhausted";

pub(crate) const PARTIAL_NON_FAST_PATH_TOKEN: &str = "vidcull-partial-non-fast-path";

fn partial_failure_skip_reason(err: &vidcull_core::Error) -> Option<&'static str> {
    match err {
        vidcull_core::Error::Decode(msg)
            if !msg.contains(vidcull_parser::fallback::TIMEOUT_TOKEN) =>
        {
            Some(PARTIAL_SKIP_REASON_DECODE_FAILED)
        }
        vidcull_core::Error::Unsupported(msg) if msg.contains(PARTIAL_NON_FAST_PATH_TOKEN) => {
            Some(PARTIAL_SKIP_REASON_UNSUPPORTED_CODEC)
        }
        _ => None,
    }
}

fn partial_failure_skip_reason_after_retry(
    err: &vidcull_core::Error,
    retried: bool,
) -> Option<&'static str> {
    partial_failure_skip_reason(err).or({
        if retried && matches!(err, vidcull_core::Error::Parse(_)) {
            Some(PARTIAL_SKIP_REASON_DECODE_FAILED)
        } else {
            None
        }
    })
}

const PARTIAL_RETRY_BUDGET: i32 = 2;

fn partial_retry_budget_reason(
    db: &Database,
    task_kind: &str,
    path: &NormalizedPath,
) -> Result<Option<&'static str>> {
    let payload = ChangeTask {
        path: path.clone(),
        change: ChangeKind::PartialFingerprint,
        size_bytes: 0,
    }
    .to_payload()?;
    let failed_count =
        TaskQueueRepo::new(db.conn()).count_failed_by_payload(task_kind, &payload)?;
    Ok(if failed_count >= i64::from(PARTIAL_RETRY_BUDGET) {
        Some(PARTIAL_SKIP_REASON_RETRY_EXHAUSTED)
    } else {
        None
    })
}

const BASE_RETRY_DISABLE_ENV: &str = "VIDCULL_BASE_RETRY_DISABLE";
fn base_retry_disabled() -> bool {
    base_retry_disable_value(std::env::var(BASE_RETRY_DISABLE_ENV).ok().as_deref())
}

fn base_retry_disable_value(raw: Option<&str>) -> bool {
    raw == Some("1")
}

fn base_retry_reason(err: &vidcull_core::Error) -> Option<&'static str> {
    match err {
        vidcull_core::Error::Cancelled => None,
        vidcull_core::Error::Parse(_) => Some("parse-error"),
        vidcull_core::Error::Unsupported(msg)
            if !msg.contains(vidcull_parser::fallback::TIMEOUT_TOKEN) =>
        {
            Some("unsupported-mid-stream")
        }
        vidcull_core::Error::Decode(msg)
            if !msg.contains(vidcull_parser::fallback::TIMEOUT_TOKEN) =>
        {
            Some("decode-error")
        }
        _ => None,
    }
}

fn partial_failure_nulls_blob(err: &vidcull_core::Error) -> bool {
    matches!(err, vidcull_core::Error::Unsupported(msg) if msg.contains(PARTIAL_NON_FAST_PATH_TOKEN))
}

const PARTIAL_RETRY_DISABLE_ENV: &str = "VIDCULL_PARTIAL_RETRY_DISABLE";
fn partial_retry_disabled() -> bool {
    base_retry_disable_value(std::env::var(PARTIAL_RETRY_DISABLE_ENV).ok().as_deref())
}

fn partial_retry_reason(err: &vidcull_core::Error) -> Option<&'static str> {
    let confirmed_non_fast_path = matches!(
        err,
        vidcull_core::Error::Unsupported(msg) if msg.contains(PARTIAL_NON_FAST_PATH_TOKEN)
    );
    if confirmed_non_fast_path {
        return None;
    }
    base_retry_reason(err)
}

fn retry_partial_via_pure_ffmpeg_fallback(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    budget: usize,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
) -> Result<PartialBuildOutcome> {
    let meta = probe_fallback_cancellable(bins, path, cancel)?;
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    if dur == 0 || meta.resolution.is_empty() {
        return Ok(PartialBuildOutcome::Skipped(
            PARTIAL_SKIP_REASON_UNPROBEABLE,
        ));
    }
    let mut builder = Tier2Builder::new();
    decode_sparse_strided_with_streaming(
        bins,
        path,
        dur,
        meta.resolution.width,
        meta.resolution.height,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        conc,
        cancel,
        |frame| {
            if cancel.fired() {
                return Err(vidcull_core::Error::Cancelled);
            }
            let (w, h, px) =
                trim_uniform_borders(frame.width, frame.height, &frame.pixels, DEFAULT_BAR_LIMIT);
            builder.push(&TimedFrame {
                timestamp_ms: frame.timestamp_ms,
                frame: GrayFrame {
                    width: w,
                    height: h,
                    pixels: &px,
                },
            });
            Ok(())
        },
    )?;
    let tier2 = builder.finish();
    if tier2.is_empty() {
        return Ok(PartialBuildOutcome::Skipped(PARTIAL_SKIP_REASON_NO_SCENES));
    }
    Ok(PartialBuildOutcome::Built(encode_tier2(&tier2)?))
}

fn retry_partial_decode_on_content_failure<T>(
    err: &vidcull_core::Error,
    path: &std::path::Path,
    disabled: bool,
    retry: impl FnOnce() -> Result<T>,
) -> Option<Result<T>> {
    if matches!(err, vidcull_core::Error::Cancelled) {
        return None;
    }
    let reason = partial_retry_reason(err)?;
    if disabled {
        tracing::debug!(
            path = %redact_fs_path(path),
            reason,
            error = %err,
            env = PARTIAL_RETRY_DISABLE_ENV,
            "partial-fingerprint retry skipped: disabled via env",
        );
        return None;
    }
    tracing::info!(
        path = %redact_fs_path(path),
        reason,
        error = %err,
        "partial-fingerprint native decode failed; retrying via pure ffmpeg fallback",
    );
    Some(retry())
}

const BUSY_OR_LOCKED_DISPLAY: &str = "resource busy or locked";

const DB_LOCKED_TOKEN: &str = "is locked";

pub(crate) const QUARANTINE_ATTEMPT_THRESHOLD: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReindexFailureClass {
    TransientSuppressed,
    PermanentSurface,
}

fn reindex_failure_is_transient(last_error: &str) -> bool {
    last_error.contains(vidcull_parser::fallback::TIMEOUT_TOKEN)
        || last_error.contains(BASE_DECODE_GATE_BUSY_REASON)
        || last_error.contains(SEQ_READ_GATE_BUSY_REASON)
        || last_error.contains(PARTIAL_GATE_BUSY_REASON)
        || last_error.contains(BUSY_OR_LOCKED_DISPLAY)
        || last_error.contains(DB_LOCKED_TOKEN)
}

pub(crate) fn classify_reindex_failure(last_error: &str, attempts: i32) -> ReindexFailureClass {
    if reindex_failure_is_transient(last_error) {
        return ReindexFailureClass::TransientSuppressed;
    }
    if attempts >= QUARANTINE_ATTEMPT_THRESHOLD {
        ReindexFailureClass::PermanentSurface
    } else {
        ReindexFailureClass::TransientSuppressed
    }
}

pub struct IndexingWorker {
    db: Database,
    bins: FfmpegBinaries,
    budget: usize,
    fallback_budget: usize,
    task_kind: String,
    now: fn() -> i64,
    metrics: Arc<FallbackMetrics>,
    single_flight: Arc<SingleFlight>,
    decode_conc: Arc<DecodeConcurrency>,
    partial_clips_enabled: bool,
    partial_gate: Arc<PartialDecodeGate>,
    base_decode_gate: Arc<BaseDecodeGate>,
    seq_read_gate: Arc<BaseDecodeGate>,
    cancel_source: Option<Arc<crate::throttle::ThrottleControl>>,
}

impl IndexingWorker {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        bins: FfmpegBinaries,
        budget: usize,
        fallback_budget: usize,
        task_kind: String,
        now: fn() -> i64,
        metrics: Arc<FallbackMetrics>,
        single_flight: Arc<SingleFlight>,
    ) -> Self {
        Self {
            db,
            bins,
            budget,
            fallback_budget,
            task_kind,
            now,
            metrics,
            single_flight,
            decode_conc: Arc::new(DecodeConcurrency::serial()),
            partial_clips_enabled: false,
            partial_gate: Arc::new(PartialDecodeGate::new(1)),
            base_decode_gate: Arc::new(BaseDecodeGate::new(BASE_DECODE_CONCURRENCY)),
            seq_read_gate: Arc::new(BaseDecodeGate::new(seq_read_gate_capacity())),
            cancel_source: None,
        }
    }

    pub fn set_decode_concurrency(&mut self, c: Arc<DecodeConcurrency>) {
        self.decode_conc = c;
    }

    pub(crate) fn set_partial_clips_enabled(&mut self, enabled: bool) {
        self.partial_clips_enabled = enabled;
    }

    pub(crate) fn set_partial_gate(&mut self, gate: Arc<PartialDecodeGate>) {
        self.partial_gate = gate;
    }

    pub(crate) fn set_cancel_source(&mut self, source: Arc<crate::throttle::ThrottleControl>) {
        self.cancel_source = Some(source);
    }

    pub(crate) fn set_base_decode_gate(&mut self, gate: Arc<BaseDecodeGate>) {
        self.base_decode_gate = gate;
    }

    pub(crate) fn set_seq_read_gate(&mut self, gate: Arc<BaseDecodeGate>) {
        self.seq_read_gate = gate;
    }

    pub fn handle_change(&mut self, change: &ChangeTask, task_id: i64) -> Result<()> {
        let _file_span =
            tracing::info_span!("index_file", file = %redact_path(change.path.as_str())).entered();
        let result = match change.change {
            ChangeKind::Upsert => self.index_file(&change.path, false, task_id),
            ChangeKind::ForceUpsert => self.index_file(&change.path, true, task_id),
            ChangeKind::Remove => self.purge_file(&change.path),
            ChangeKind::Densify => self.densify_file(&change.path),
            ChangeKind::PartialFingerprint => self.partial_fingerprint_file(&change.path),
        };
        if let Err(err) = &result {
            if matches!(err, Error::Busy(_)) {
                tracing::debug!(
                    path = %redact_path(change.path.as_str()),
                    error = %err,
                    "indexing deferred: file is busy; will retry on backoff",
                );
            } else if matches!(err, Error::Cancelled) {
                tracing::debug!(
                    path = %redact_path(change.path.as_str()),
                    change = ?change.change,
                    "indexing paused; in-flight decode cancelled — task requeued",
                );
            } else {
                tracing::warn!(
                    path = %redact_path(change.path.as_str()),
                    change = ?change.change,
                    error = %err,
                    "indexing failed for file; recorded for later inspection",
                );
            }
        }
        result
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        clippy::cast_lossless
    )]
    fn index_file(&mut self, path: &NormalizedPath, force: bool, task_id: i64) -> Result<()> {
        let native = path.to_native_path();
        let fs_path = native.as_path();
        if is_file_locked(fs_path) {
            return Err(Error::Busy(format!(
                "file is locked or being written to: {}",
                redact_path(path.as_str())
            )));
        }
        let meta = std::fs::metadata(fs_path).map_err(Error::Io)?;
        let size_bytes = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        let mtime_ns = mtime_nanos(&meta);

        if let Some(existing) = FilesRepo::new(self.db.conn()).find_by_path(path)? {
            let meta_match = existing.size_bytes == size_bytes && existing.mtime_ns == mtime_ns;
            let has_fingerprint = FingerprintsRepo::new(self.db.conn())
                .get(existing.id)?
                .is_some();

            if !force && meta_match && has_fingerprint {
                let now = (self.now)();
                if existing.deleted_at.is_some() {
                    let new_file = NewFile {
                        path: existing.path.clone(),
                        size_bytes: existing.size_bytes,
                        mtime_ns: existing.mtime_ns,
                        inode: existing.inode,
                        content_hash: existing.content_hash,
                        codec: existing.codec.clone(),
                        container: existing.container.clone(),
                        duration: existing.duration,
                        fps_x1000: existing.fps_x1000,
                        bitrate_bps: existing.bitrate_bps,
                        resolution: existing.resolution,
                        first_seen_at: existing.first_seen_at,
                        last_seen_at: now,
                        laplacian_variance: existing.laplacian_variance,
                        dct_energy: existing.dct_energy,
                        bpp: existing.bpp,
                        encoder_tags: existing.encoder_tags.clone(),
                    };
                    self.db.transaction(|conn| {
                        FilesRepo::new(conn).update_metadata(existing.id, &new_file)?;
                        RegroupQueueRepo::new(conn).mark(existing.id, now)?;
                        Ok(())
                    })?;
                } else {
                    self.db.transaction(|conn| {
                        FilesRepo::new(conn).touch_last_seen(existing.id, now)
                    })?;
                }
                return Ok(());
            }
        }

        let cancel_src = self.cancel_source.clone();
        let registration = cancel_src.as_deref().map(|ctrl| ctrl.register_decode(path));
        let cancel = Cancel {
            pause: cancel_src
                .as_deref()
                .map(crate::throttle::ThrottleControl::cancel_flag),
            removal: registration
                .as_ref()
                .map(crate::throttle::DecodeRegistration::token),
        };

        let seq_gate_arc = Arc::clone(&self.seq_read_gate);
        let Some(seq_guard) = seq_gate_arc.try_acquire() else {
            let (in_use, capacity) = seq_gate_arc.snapshot();
            tracing::debug!(
                stage = "seq_read_gate",
                gate_in_use = in_use,
                gate_capacity = capacity,
                "sequential-read gate at capacity; requeueing with backoff",
            );
            return Err(Error::Busy(SEQ_READ_GATE_BUSY_REASON.into()));
        };
        let hash_start = std::time::Instant::now();
        let (content_hash, pre_parsed) = hash_file_fusing_mp4_parse(fs_path, cancel)?;
        let hash_ms = elapsed_ms(hash_start);
        drop(seq_guard);

        let single_flight = Arc::clone(&self.single_flight);
        let _flight = single_flight.begin(content_hash, || cancel.fired());

        if !force {
            if let Some(twin) =
                FilesRepo::new(self.db.conn()).find_active_twin_by_hash(&content_hash, path)?
            {
                if let Some(twin_fp) = FingerprintsRepo::new(self.db.conn()).get(twin.id)? {
                    return self.index_as_twin_copy(
                        path,
                        &twin,
                        &twin_fp,
                        size_bytes,
                        mtime_ns,
                        content_hash,
                    );
                }
            }
        }

        let base_gate_arc = Arc::clone(&self.base_decode_gate);
        let Some(_base_gate) = base_gate_arc.try_acquire() else {
            let (in_use, capacity) = base_gate_arc.snapshot();
            tracing::debug!(
                stage = "base_decode_gate",
                gate_in_use = in_use,
                gate_capacity = capacity,
                "base-decode gate at capacity; requeueing with backoff",
            );
            return Err(Error::Busy(BASE_DECODE_GATE_BUSY_REASON.into()));
        };
        tracing::info!(stage = "index", "base-index fingerprint: decoding");
        let decode_start = std::time::Instant::now();
        let (metadata, decode_path, decoded_frames, artifacts, captured_thumb) =
            probe_decode_fingerprint_streaming_preparsed(
                &self.bins,
                fs_path,
                self.budget,
                self.fallback_budget,
                &self.decode_conc,
                pre_parsed,
                cancel,
            )?;
        let decode_ms = elapsed_ms(decode_start);

        let FingerprintArtifacts {
            tier1_blob,
            tier2_blob,
            laplacian_variance,
            dct_energy,
            bpp,
        } = artifacts;

        self.metrics.record(decode_path);
        tracing::info!(
            stage = "index",
            codec = ?metadata.codec,
            container = %metadata.container.short_name(),
            width = metadata.resolution.width,
            height = metadata.resolution.height,
            decoded_frames,
            hash_ms,
            decode_ms,
            decode_path = ?decode_path,
            "file decoded and fingerprinted",
        );

        let slow_ms = decode_slow_ms();
        let slow_total_ms = decode_slow_total_ms();
        let health = assess_decode(decoded_frames, 0, decode_ms, slow_ms, overdecode_factor());
        if full_index_decode_is_pathological(decode_path, health, decode_ms, slow_total_ms) {
            tracing::warn!(
                stage = "index",
                codec = ?metadata.codec,
                container = %metadata.container.short_name(),
                decoded_frames,
                ms_per_decoded_frame = health.ms_per_frame,
                decode_ms,
                slow_threshold_ms = slow_ms,
                slow_total_threshold_ms = slow_total_ms,
                "full-index fallback decode is pathologically slow; quarantining file",
            );
            return Err(Error::Decode(format!(
                "fallback decode pathologically slow for {} ({} ms/frame over {} ms, {} ms total over {} ms); quarantining",
                redact_path(path.as_str()),
                health.ms_per_frame,
                slow_ms,
                decode_ms,
                slow_total_ms,
            )));
        }

        let now = (self.now)();
        let mut new_file = NewFile {
            path: path.clone(),
            size_bytes,
            mtime_ns,
            inode: None,
            content_hash: Some(content_hash),
            codec: Some(metadata.codec.clone()),
            container: Some(metadata.container.short_name().to_owned()),
            duration: metadata.duration,
            fps_x1000: metadata.fps_x1000.and_then(|f| i32::try_from(f).ok()),
            bitrate_bps: metadata.bitrate_bps.and_then(|b| i64::try_from(b).ok()),
            resolution: Some(metadata.resolution),
            first_seen_at: now,
            last_seen_at: now,
            laplacian_variance,
            dct_energy,
            bpp,
            encoder_tags: metadata.encoder_tags.clone(),
        };
        let created_at = now;

        let file_id = self.db.transaction(|conn| {
            if !TaskQueueRepo::new(conn).exists(task_id)? {
                return Err(vidcull_core::Error::Cancelled);
            }
            let files = FilesRepo::new(conn);
            let file_id = match files.find_by_path(path)? {
                Some(existing) => {
                    if existing.content_hash != Some(content_hash) {
                        let groups = DuplicateGroupsRepo::new(conn);
                        for group in groups.find_groups_containing(existing.id)? {
                            groups.remove_member(group.id, existing.id)?;
                            if groups.list_members(group.id)?.len() < 2 {
                                groups.delete(group.id)?;
                            }
                        }
                    }
                    new_file.first_seen_at = existing.first_seen_at;
                    files.update_metadata(existing.id, &new_file)?;
                    existing.id
                }
                None => files.insert(&new_file)?,
            };
            FingerprintsRepo::new(conn).upsert(&Fingerprint {
                file_id,
                tier1_global: tier1_blob,
                tier2_temporal: Some(tier2_blob),
                format_version: u32::from(FORMAT_VERSION),
                created_at,
            })?;
            RegroupQueueRepo::new(conn).mark(file_id, created_at)?;
            Ok(file_id)
        })?;

        if let (Some(frame), Some(provider)) = (captured_thumb, BASE_INDEX_THUMBS.get()) {
            if let Err(err) = provider.store_decoded_frame(
                &content_hash,
                frame.width,
                frame.height,
                &frame.pixels,
            ) {
                tracing::debug!(
                    path = %redact_path(path.as_str()),
                    error = %err,
                    "L1: index-time thumbnail cache store failed; on-demand decode will still serve it",
                );
            }
        }

        if decode_path == DecodePath::Fallback {
            let duration_ms = metadata.duration.unwrap_or(VideoDuration::ZERO).as_millis();
            let full = full_grid_len(duration_ms);
            if full > self.fallback_budget {
                enqueue_changes(
                    &mut self.db,
                    &[ChangeTask {
                        path: path.clone(),
                        change: ChangeKind::Densify,
                        size_bytes: 0,
                    }],
                    &self.task_kind,
                    DENSIFY_PRIORITY,
                    (self.now)(),
                )?;
                tracing::debug!(
                    path = %redact_path(path.as_str()),
                    full_grid = full,
                    capped = self.fallback_budget,
                    "fallback first pass capped; densify revisit queued",
                );
            }
        }

        self.enqueue_partial_if_missing(path, file_id)?;
        Ok(())
    }

    fn enqueue_partial_if_missing(&mut self, path: &NormalizedPath, file_id: FileId) -> Result<()> {
        if !self.partial_clips_enabled {
            return Ok(());
        }
        if FingerprintsRepo::new(self.db.conn())
            .get_active_partial(file_id)?
            .is_some()
        {
            return Ok(());
        }
        enqueue_changes(
            &mut self.db,
            &[ChangeTask {
                path: path.clone(),
                change: ChangeKind::PartialFingerprint,
                size_bytes: 0,
            }],
            &self.task_kind,
            PARTIAL_PRIORITY,
            (self.now)(),
        )?;
        Ok(())
    }

    fn densify_file(&mut self, path: &NormalizedPath) -> Result<()> {
        let native = path.to_native_path();
        let fs_path = native.as_path();
        let Some(existing) = FilesRepo::new(self.db.conn()).find_by_path(path)? else {
            return Ok(());
        };
        if existing.deleted_at.is_some() || !fs_path.exists() {
            return Ok(());
        }
        if is_file_locked(fs_path) {
            return Err(Error::Busy(format!(
                "file is locked or being written to: {}",
                redact_path(path.as_str())
            )));
        }

        let (_metadata, _decode_path, decoded_frames, artifacts) = {
            let registration = self
                .cancel_source
                .as_deref()
                .map(|ctrl| ctrl.register_decode(path));
            let cancel = Cancel {
                pause: self
                    .cancel_source
                    .as_deref()
                    .map(crate::throttle::ThrottleControl::cancel_flag),
                removal: registration
                    .as_ref()
                    .map(crate::throttle::DecodeRegistration::token),
            };
            probe_decode_fingerprint_streaming(
                &self.bins,
                fs_path,
                self.budget,
                self.budget,
                &self.decode_conc,
                cancel,
            )?
        };
        let FingerprintArtifacts {
            tier1_blob,
            tier2_blob,
            laplacian_variance,
            dct_energy,
            bpp,
        } = artifacts;

        let now = (self.now)();
        self.db.transaction(|conn| {
            let files = FilesRepo::new(conn);
            let rows = match existing.content_hash.as_ref() {
                Some(hash) => files.list_active_by_hash(hash)?,
                None => vec![existing.clone()],
            };
            let fingerprints = FingerprintsRepo::new(conn);
            let regroup = RegroupQueueRepo::new(conn);
            for row in &rows {
                fingerprints.upsert(&Fingerprint {
                    file_id: row.id,
                    tier1_global: tier1_blob.clone(),
                    tier2_temporal: Some(tier2_blob.clone()),
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: now,
                })?;
                files.update_quality_stats(row.id, laplacian_variance, dct_energy, bpp)?;
                regroup.mark(row.id, now)?;
            }
            Ok(())
        })?;
        tracing::debug!(
            path = %redact_path(path.as_str()),
            frames = decoded_frames,
            "densify revisit complete",
        );
        Ok(())
    }

    fn partial_skip_hazard(&self, existing: &FileRecord, path: &NormalizedPath) -> Result<bool> {
        let fingerprints = FingerprintsRepo::new(self.db.conn());
        if let Some(marker) = fingerprints.get_partial_skip(existing.id)? {
            if marker.reason != PARTIAL_SKIP_REASON_EXACT_FULL_DUP
                && marker.size_bytes == existing.size_bytes
                && marker.mtime_ns == existing.mtime_ns
            {
                tracing::debug!(
                    path = %redact_path(path.as_str()),
                    reason = marker.reason,
                    "partial fingerprint skipped: valid skip marker",
                );
                return Ok(true);
            }
        }

        if existing
            .codec
            .as_ref()
            .is_some_and(|c| !c.is_fast_path_eligible())
        {
            let marked = fingerprints.set_partial_skip(
                existing.id,
                &PartialSkipMarker {
                    reason: PARTIAL_SKIP_REASON_UNSUPPORTED_CODEC.into(),
                    size_bytes: existing.size_bytes,
                    mtime_ns: existing.mtime_ns,
                },
            )?;
            tracing::info!(
                path = %redact_path(path.as_str()),
                codec = ?existing.codec,
                marker_rows = marked,
                "partial fingerprint skipped: unsupported codec; skip marker stamped",
            );
            return Ok(true);
        }
        Ok(false)
    }

    #[allow(clippy::too_many_lines)]
    fn partial_fingerprint_file(&mut self, path: &NormalizedPath) -> Result<()> {
        let native = path.to_native_path();
        let fs_path = native.as_path();
        let Some(existing) = FilesRepo::new(self.db.conn()).find_by_path(path)? else {
            return Ok(());
        };
        if existing.deleted_at.is_some() || !fs_path.exists() {
            return Ok(());
        }
        if is_file_locked(fs_path) {
            return Err(Error::Busy(format!(
                "file is locked or being written to: {}",
                redact_path(path.as_str())
            )));
        }

        if self.partial_skip_hazard(&existing, path)? {
            return Ok(());
        }

        if is_confirmed_full_dup(&self.db, existing.id)? {
            let repo = FingerprintsRepo::new(self.db.conn());
            let marker_rows = repo.set_partial_skip(
                existing.id,
                &PartialSkipMarker {
                    reason: PARTIAL_SKIP_REASON_EXACT_FULL_DUP.into(),
                    size_bytes: existing.size_bytes,
                    mtime_ns: existing.mtime_ns,
                },
            )?;
            tracing::debug!(
                path = %redact_path(path.as_str()),
                marker_rows,
                "partial fingerprint skipped: confirmed exact full-dup member; skip marker stamped",
            );
            return Ok(());
        }

        if let Some(dur) = existing.duration {
            if dur.as_millis() > PARTIAL_MAX_DURATION_MS {
                let repo = FingerprintsRepo::new(self.db.conn());
                let marker_rows = repo.set_partial_skip(
                    existing.id,
                    &PartialSkipMarker {
                        reason: PARTIAL_SKIP_REASON_DURATION_CAP.into(),
                        size_bytes: existing.size_bytes,
                        mtime_ns: existing.mtime_ns,
                    },
                )?;
                tracing::info!(
                    path = %redact_path(path.as_str()),
                    duration_ms = dur.as_millis(),
                    cap_ms = PARTIAL_MAX_DURATION_MS,
                    marker_rows,
                    "partial fingerprint skipped: duration exceeds cap; skip marker stamped",
                );
                return Ok(());
            }
        }

        let Some(_partial_gate) = self.partial_gate.try_acquire() else {
            let (in_use, capacity) = self.partial_gate.snapshot();
            tracing::debug!(
                stage = "partial_gate",
                gate_in_use = in_use,
                gate_capacity = capacity,
                "partial-decode gate at capacity; requeueing with backoff",
            );
            return Err(Error::Busy(PARTIAL_GATE_BUSY_REASON.into()));
        };
        let gate_held_start = std::time::Instant::now();
        tracing::info!(path = %redact_path(path.as_str()), "partial-clip fingerprint: decoding");
        let (decode_result, retried) = {
            let registration = self
                .cancel_source
                .as_deref()
                .map(|ctrl| ctrl.register_decode(path));
            let cancel = Cancel {
                pause: self
                    .cancel_source
                    .as_deref()
                    .map(crate::throttle::ThrottleControl::cancel_flag),
                removal: registration
                    .as_ref()
                    .map(crate::throttle::DecodeRegistration::token),
            };
            let first = build_partial_fingerprint(
                &self.bins,
                fs_path,
                self.budget,
                &self.decode_conc,
                cancel,
            );
            match first {
                Ok(o) => (Ok(o), false),
                Err(e) => {
                    match retry_partial_decode_on_content_failure(
                        &e,
                        fs_path,
                        partial_retry_disabled(),
                        || {
                            retry_partial_via_pure_ffmpeg_fallback(
                                &self.bins,
                                fs_path,
                                self.budget,
                                &self.decode_conc,
                                cancel,
                            )
                        },
                    ) {
                        Some(retried_result) => (retried_result, true),
                        None => (Err(e), false),
                    }
                }
            }
        };
        let outcome = match decode_result {
            Ok(o) => o,
            Err(e) => {
                if matches!(e, Error::Cancelled) {
                    return Err(e);
                }
                let reason = match partial_failure_skip_reason_after_retry(&e, retried) {
                    Some(reason) => Some(reason),
                    None => partial_retry_budget_reason(&self.db, &self.task_kind, path)?,
                };
                if let Some(reason) = reason {
                    let repo = FingerprintsRepo::new(self.db.conn());
                    let marker = PartialSkipMarker {
                        reason: reason.into(),
                        size_bytes: existing.size_bytes,
                        mtime_ns: existing.mtime_ns,
                    };
                    let cleared_blob = partial_failure_nulls_blob(&e);
                    let rows_updated = if cleared_blob {
                        repo.clear_partial_and_mark_skip(existing.id, &marker)?
                    } else {
                        repo.set_partial_skip(existing.id, &marker)?
                    };
                    tracing::info!(
                        path = %redact_path(path.as_str()),
                        reason,
                        retried,
                        cleared_blob,
                        marker_rows = rows_updated,
                        "partial fingerprint skipped: decode failure; skip marker stamped \
                    ",
                    );
                    return Ok(());
                }
                return Err(e);
            }
        };
        let stored_bytes = match outcome {
            PartialBuildOutcome::Built(blob) => {
                let now = (self.now)();
                self.db.transaction(|conn| {
                    let files = FilesRepo::new(conn);
                    let rows = match existing.content_hash.as_ref() {
                        Some(hash) => files.list_active_by_hash(hash)?,
                        None => vec![existing.clone()],
                    };
                    let fingerprints = FingerprintsRepo::new(conn);
                    let regroup = RegroupQueueRepo::new(conn);
                    for row in &rows {
                        fingerprints.set_partial(row.id, &blob)?;
                        regroup.mark(row.id, now)?;
                    }
                    Ok(())
                })?;
                blob.len()
            }
            PartialBuildOutcome::Skipped(reason) => {
                let repo = FingerprintsRepo::new(self.db.conn());
                let marker_rows = repo.set_partial_skip(
                    existing.id,
                    &PartialSkipMarker {
                        reason: reason.into(),
                        size_bytes: existing.size_bytes,
                        mtime_ns: existing.mtime_ns,
                    },
                )?;
                tracing::info!(
                    path = %redact_path(path.as_str()),
                    reason,
                    marker_rows,
                    "partial fingerprint skipped: no matchable signal; skip marker stamped",
                );
                0
            }
        };
        let held_ms = elapsed_ms(gate_held_start);
        let (in_use, capacity) = self.partial_gate.snapshot();
        let warn_ms = gate_hold_warn_ms();
        if gate_hold_is_excessive(held_ms, warn_ms) {
            tracing::warn!(
                stage = "partial_gate",
                held_ms,
                hold_warn_ms = warn_ms,
                gate_in_use = in_use,
                gate_capacity = capacity,
                "partial-decode gate slot held far longer than expected — head-of-line risk; other partials blocked while this ran",
            );
        } else {
            tracing::debug!(
                stage = "partial_gate",
                held_ms,
                bytes = stored_bytes,
                "partial-clip fingerprint complete; gate slot released",
            );
        }
        Ok(())
    }

    fn index_as_twin_copy(
        &mut self,
        path: &NormalizedPath,
        twin: &FileRecord,
        twin_fp: &Fingerprint,
        size_bytes: i64,
        mtime_ns: i64,
        content_hash: Blake3Hash,
    ) -> Result<()> {
        let now = (self.now)();
        let mut new_file = NewFile {
            path: path.clone(),
            size_bytes,
            mtime_ns,
            inode: None,
            content_hash: Some(content_hash),
            codec: twin.codec.clone(),
            container: twin.container.clone(),
            duration: twin.duration,
            fps_x1000: twin.fps_x1000,
            bitrate_bps: twin.bitrate_bps,
            resolution: twin.resolution,
            first_seen_at: now,
            last_seen_at: now,
            laplacian_variance: twin.laplacian_variance,
            dct_energy: twin.dct_energy,
            bpp: twin.bpp,
            encoder_tags: twin.encoder_tags.clone(),
        };
        let tier1_blob = twin_fp.tier1_global.clone();
        let tier2_blob = twin_fp.tier2_temporal.clone();
        let format_version = twin_fp.format_version;
        self.db.transaction(|conn| {
            let files = FilesRepo::new(conn);
            let file_id = match files.find_by_path(path)? {
                Some(existing) => {
                    new_file.first_seen_at = existing.first_seen_at;
                    files.update_metadata(existing.id, &new_file)?;
                    existing.id
                }
                None => files.insert(&new_file)?,
            };
            let fingerprints = FingerprintsRepo::new(conn);
            fingerprints.upsert(&Fingerprint {
                file_id,
                tier1_global: tier1_blob,
                tier2_temporal: tier2_blob,
                format_version,
                created_at: now,
            })?;
            fingerprints.set_partial_skip(
                file_id,
                &PartialSkipMarker {
                    reason: PARTIAL_SKIP_REASON_EXACT_FULL_DUP.into(),
                    size_bytes,
                    mtime_ns,
                },
            )?;
            RegroupQueueRepo::new(conn).mark(file_id, now)?;
            Ok(())
        })
    }

    fn purge_file(&mut self, path: &NormalizedPath) -> Result<()> {
        let now = (self.now)();
        self.db.transaction(|conn| {
            let files = FilesRepo::new(conn);
            let doomed = files.list_active_under_root(path)?;
            let regroup = RegroupQueueRepo::new(conn);
            let groups = DuplicateGroupsRepo::new(conn);
            for existing in doomed {
                files.mark_deleted(existing.id, now)?;
                regroup.mark(existing.id, now)?;
                for group in groups.find_groups_containing(existing.id)? {
                    groups.remove_member(group.id, existing.id)?;
                    if groups.list_members(group.id)?.len() < 2 {
                        groups.delete(group.id)?;
                    }
                }
            }
            Ok(())
        })
    }
}

struct FingerprintArtifacts {
    tier1_blob: Vec<u8>,
    tier2_blob: Vec<u8>,
    laplacian_variance: Option<f64>,
    dct_energy: Option<f64>,
    bpp: Option<f64>,
}

struct CapturedThumbFrame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

const THUMB_CAPTURE_MIN_TS_MS: u64 = vidcull_core::SPARSE_GRID_INTERVAL_MS;

static BASE_INDEX_THUMBS: std::sync::OnceLock<Arc<ThumbnailProvider>> = std::sync::OnceLock::new();

fn is_confirmed_full_dup(db: &Database, file_id: FileId) -> Result<bool> {
    let groups = DuplicateGroupsRepo::new(db.conn()).find_groups_containing(file_id)?;
    Ok(groups
        .iter()
        .any(|g| matches!(g.trust_level, TrustLevel::Exact)))
}

const DEFAULT_DECODE_SLOW_MS: u64 = 1000;

const DEFAULT_DECODE_SLOW_TOTAL_MS: u64 = 30_000;

const DEFAULT_OVERDECODE_FACTOR: usize = 4;

fn decode_slow_ms() -> u64 {
    static CACHED: std::sync::LazyLock<u64> =
        std::sync::LazyLock::new(|| env_knob("VIDCULL_DECODE_SLOW_MS", DEFAULT_DECODE_SLOW_MS));
    *CACHED
}

fn decode_slow_total_ms() -> u64 {
    static CACHED: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        env_knob("VIDCULL_DECODE_SLOW_TOTAL_MS", DEFAULT_DECODE_SLOW_TOTAL_MS)
    });
    *CACHED
}

fn overdecode_factor() -> usize {
    static CACHED: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        env_knob("VIDCULL_OVERDECODE_FACTOR", DEFAULT_OVERDECODE_FACTOR)
    });
    *CACHED
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecodeHealth {
    ms_per_frame: u64,
    slow: bool,
    over_decode: bool,
}

impl DecodeHealth {
    fn is_pathological(self) -> bool {
        self.slow || self.over_decode
    }
}

fn assess_decode(
    decoded_frames: usize,
    grid_points: usize,
    fold_ms: u64,
    slow_ms: u64,
    factor: usize,
) -> DecodeHealth {
    let ms_per_frame = if decoded_frames > 0 {
        fold_ms / decoded_frames as u64
    } else {
        0
    };
    DecodeHealth {
        ms_per_frame,
        slow: decoded_frames > 0 && ms_per_frame > slow_ms,
        over_decode: grid_points > 0 && decoded_frames > grid_points.saturating_mul(factor),
    }
}

fn full_index_decode_is_pathological(
    decode_path: DecodePath,
    health: DecodeHealth,
    decode_ms: u64,
    slow_total_ms: u64,
) -> bool {
    matches!(decode_path, DecodePath::Fallback) && health.slow && decode_ms > slow_total_ms
}

#[derive(Debug, PartialEq, Eq)]
enum PartialBuildOutcome {
    Built(Vec<u8>),
    Skipped(&'static str),
}

fn build_partial_fingerprint(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    budget: usize,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
) -> Result<PartialBuildOutcome> {
    let probe_start = std::time::Instant::now();
    let meta = vidcull_parser::fallback::probe_fallback_cancellable(bins, path, cancel)?;
    let probe_ms = elapsed_ms(probe_start);
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    if dur == 0 || meta.resolution.is_empty() {
        return Ok(PartialBuildOutcome::Skipped(
            PARTIAL_SKIP_REASON_UNPROBEABLE,
        ));
    }
    if !meta.codec.is_fast_path_eligible() {
        return Err(vidcull_core::Error::Unsupported(format!(
            "{PARTIAL_NON_FAST_PATH_TOKEN}: codec {:?} is not fast-path eligible",
            meta.codec
        )));
    }
    let frame_px = u64::from(meta.resolution.width) * u64::from(meta.resolution.height);
    let (grid_points, planned_spawns) = vidcull_parser::fallback::fallback_spawn_plan(
        &meta.container,
        dur,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        frame_px,
    );
    tracing::debug!(
        stage = "partial_route",
        route = if planned_spawns < grid_points { "batch" } else { "per_frame" },
        container = %meta.container.short_name(),
        codec = ?meta.codec,
        fps_x1000 = ?meta.fps_x1000,
        frame_px,
        grid_points,
        planned_spawns,
        "fallback decode route decided",
    );
    let mut decoded_frames = 0usize;
    let fold_start = std::time::Instant::now();
    let mut builder = Tier2Builder::new();
    let (_router_meta, decode_path) = probe_and_decode_sparse_budgets_streaming(
        bins,
        path,
        budget,
        budget,
        conc,
        cancel,
        |frame| {
            if cancel.fired() {
                return Err(vidcull_core::Error::Cancelled);
            }
            let (w, h, px) =
                trim_uniform_borders(frame.width, frame.height, &frame.pixels, DEFAULT_BAR_LIMIT);
            builder.push(&TimedFrame {
                timestamp_ms: frame.timestamp_ms,
                frame: GrayFrame {
                    width: w,
                    height: h,
                    pixels: &px,
                },
            });
            decoded_frames += 1;
            Ok(())
        },
    )?;
    let fold_ms = elapsed_ms(fold_start);

    let slow_ms = decode_slow_ms();
    let health = assess_decode(
        decoded_frames,
        grid_points,
        fold_ms,
        slow_ms,
        overdecode_factor(),
    );
    tracing::info!(
        stage = "partial_fingerprint",
        codec = ?meta.codec,
        container = %meta.container.short_name(),
        width = meta.resolution.width,
        height = meta.resolution.height,
        grid_points,
        decoded_frames,
        probe_ms,
        fold_ms,
        decode_path = ?decode_path,
        ms_per_decoded_frame = health.ms_per_frame,
        "partial fingerprint decoded",
    );
    if health.is_pathological() {
        tracing::warn!(
            stage = "partial_fingerprint",
            codec = ?meta.codec,
            container = %meta.container.short_name(),
            grid_points,
            decoded_frames,
            ms_per_decoded_frame = health.ms_per_frame,
            slow = health.slow,
            over_decode = health.over_decode,
            slow_threshold_ms = slow_ms,
            "partial decode is pathologically slow or over-decoding; inspect codec/container routing",
        );
    }

    let tier2 = builder.finish();
    if tier2.is_empty() {
        return Ok(PartialBuildOutcome::Skipped(PARTIAL_SKIP_REASON_NO_SCENES));
    }
    Ok(PartialBuildOutcome::Built(encode_tier2(&tier2)?))
}

const DEFAULT_REGROUP_MIN_FILES: usize = 16;

const DEFAULT_REGROUP_MIN_INTERVAL_SECS: i64 = 10;

const REGROUP_BURST_CHUNK: usize = 64;

struct RebuildCadence {
    min_files: usize,
    min_interval_secs: i64,
    files_since: usize,
    last_rebuild_at: i64,
}

impl RebuildCadence {
    fn from_env(now: i64) -> Self {
        let min_files = std::env::var("VIDCULL_REGROUP_MIN_FILES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map_or(DEFAULT_REGROUP_MIN_FILES, |v| v.max(1));
        let min_interval_secs = std::env::var("VIDCULL_REGROUP_MIN_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .map_or(DEFAULT_REGROUP_MIN_INTERVAL_SECS, |v| v.max(0));
        Self {
            min_files,
            min_interval_secs,
            files_since: 0,
            last_rebuild_at: now,
        }
    }

    fn record_indexed(&mut self, count: usize) {
        self.files_since = self.files_since.saturating_add(count);
    }

    fn should_rebuild(&self, now: i64) -> bool {
        self.files_since >= self.min_files
            && now.saturating_sub(self.last_rebuild_at) >= self.min_interval_secs
    }

    fn reset(&mut self, now: i64) {
        self.files_since = 0;
        self.last_rebuild_at = now;
    }
}

const PREWARM_MAX_PER_CLUSTER: usize = 4;

fn select_prewarm_targets(db: &Database) -> Result<Vec<(NormalizedPath, Blake3Hash)>> {
    let groups = DuplicateGroupsRepo::new(db.conn());
    let files = FilesRepo::new(db.conn());
    let mut targets = Vec::new();
    for cluster in build_clusters(db)? {
        let best = crate::bridge::cluster_best(&groups, &cluster)?;
        let ordered = order_members_for_prewarm(&files, &cluster, best);
        targets.extend(ordered.into_iter().take(PREWARM_MAX_PER_CLUSTER));
    }
    Ok(targets)
}

fn order_members_for_prewarm(
    files: &FilesRepo<'_>,
    cluster: &Cluster,
    best_file_id: Option<i64>,
) -> Vec<(NormalizedPath, Blake3Hash)> {
    let mut scored: Vec<(i64, FileRecord)> = cluster
        .members
        .iter()
        .filter_map(|m| {
            files
                .get(m.file_id)
                .ok()
                .flatten()
                .map(|record| (m.file_id.0, record))
        })
        .collect();
    scored.sort_by(|(id_a, a), (id_b, b)| {
        let a_is_best = Some(*id_a) == best_file_id;
        let b_is_best = Some(*id_b) == best_file_id;
        if a_is_best != b_is_best {
            return b_is_best.cmp(&a_is_best);
        }
        let px_a = a.resolution.map_or(0, Resolution::pixels);
        let px_b = b.resolution.map_or(0, Resolution::pixels);
        px_b.cmp(&px_a)
            .then_with(|| b.size_bytes.cmp(&a.size_bytes))
            .then_with(|| id_a.cmp(id_b))
    });
    scored
        .into_iter()
        .filter_map(|(_, record)| record.content_hash.map(|hash| (record.path, hash)))
        .collect()
}

/// Prewarm used to decode every target on a single background thread, so
/// only one file's thumbnail was ever being decoded at a time — the
/// thumbnail-decode concurrency gate (scaled to cores) sat unused for this
/// caller. Each target is an independent file keyed by its own content
/// hash (thumbnails aren't part of the deterministic fingerprint pipeline),
/// so fanning them out across a small work-stealing pool is safe.
fn prewarm_fan_out(thumbnails: &ThumbnailProvider, targets: &[(NormalizedPath, Blake3Hash)]) {
    let workers = prewarm_fan_out_workers(targets.len());
    if workers <= 1 {
        for (path, hash) in targets {
            let _ = thumbnails.data_uri(&path.to_native_path(), Some(hash));
        }
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((path, hash)) = targets.get(idx) else {
                        break;
                    };
                    let _ = thumbnails.data_uri(&path.to_native_path(), Some(hash));
                }
            });
        }
    });
}

fn prewarm_fan_out_workers(target_count: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    cores.min(target_count).max(1)
}

struct PrewarmInFlightGuard {
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for PrewarmInFlightGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub struct IndexingHandler {
    worker: IndexingWorker,
    partial_index: PartialClipIndex,
    rebuild_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    metrics: Arc<FallbackMetrics>,
    single_flight: Arc<SingleFlight>,
    decode_conc: Arc<DecodeConcurrency>,
    partial_gate: Arc<PartialDecodeGate>,
    base_decode_gate: Arc<BaseDecodeGate>,
    seq_read_gate: Arc<BaseDecodeGate>,
    cadence: RebuildCadence,
    last_foreground_delta_high_water: usize,
    last_partial_delta_high_water: usize,
    whole_file_shadow_ran: bool,
    thumbnails: Option<Arc<ThumbnailProvider>>,
    prewarm_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

impl IndexingHandler {
    #[must_use]
    pub fn new(db: Database, bins: FfmpegBinaries, now: fn() -> i64) -> Self {
        let metrics = Arc::new(FallbackMetrics::default());
        let single_flight = Arc::new(SingleFlight::default());
        let decode_conc = Arc::new(DecodeConcurrency::new(1));
        let partial_gate = Arc::new(PartialDecodeGate::new(1));
        let base_decode_gate = Arc::new(BaseDecodeGate::new(BASE_DECODE_CONCURRENCY));
        let seq_read_gate = Arc::new(BaseDecodeGate::new(seq_read_gate_capacity()));
        let mut worker = IndexingWorker::new(
            db,
            bins,
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            now,
            Arc::clone(&metrics),
            Arc::clone(&single_flight),
        );
        worker.set_decode_concurrency(Arc::clone(&decode_conc));
        worker.set_partial_gate(Arc::clone(&partial_gate));
        worker.set_base_decode_gate(Arc::clone(&base_decode_gate));
        worker.set_seq_read_gate(Arc::clone(&seq_read_gate));
        Self {
            worker,
            partial_index: PartialClipIndex::new(AnchorParams::default()),
            rebuild_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            metrics,
            single_flight,
            decode_conc,
            partial_gate,
            base_decode_gate,
            seq_read_gate,
            cadence: RebuildCadence::from_env(now()),
            last_foreground_delta_high_water: 0,
            last_partial_delta_high_water: 0,
            whole_file_shadow_ran: false,
            thumbnails: None,
            prewarm_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn with_thumbnails(mut self, provider: Arc<ThumbnailProvider>) -> Self {
        let _ = BASE_INDEX_THUMBS.set(Arc::clone(&provider));
        self.thumbnails = Some(provider);
        self
    }

    #[must_use]
    pub fn fallback_metrics(&self) -> &Arc<FallbackMetrics> {
        &self.metrics
    }

    pub fn observe_gates(&self, observer: &DecodeGateObserver) {
        observer.publish(
            Arc::clone(&self.decode_conc),
            Arc::clone(&self.base_decode_gate),
            Arc::clone(&self.partial_gate),
            Arc::clone(&self.seq_read_gate),
        );
    }

    #[must_use]
    pub fn with_budget(mut self, budget: usize) -> Self {
        self.worker.budget = budget.max(1);
        self
    }

    #[must_use]
    pub fn with_fallback_budget(mut self, fallback_budget: usize) -> Self {
        self.worker.fallback_budget = fallback_budget.max(1);
        self
    }

    #[must_use]
    pub fn with_partial_clips(mut self, enabled: bool) -> Self {
        self.worker.partial_clips_enabled = enabled;
        self.configure_partial_index(enabled);
        self
    }

    fn configure_partial_index(&mut self, enabled: bool) {
        self.partial_index = if enabled {
            PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial)
        } else {
            PartialClipIndex::new(AnchorParams::default())
        };
    }

    #[must_use]
    pub fn with_task_kind(mut self, task_kind: impl Into<String>) -> Self {
        self.worker.task_kind = task_kind.into();
        self
    }

    #[must_use]
    pub fn rebuild_count(&self) -> usize {
        self.rebuild_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn queue_drained(&self) -> Result<bool> {
        let repo = TaskQueueRepo::new(self.worker.db.conn());
        Ok(repo.count_pending_min_priority(0)? == 0)
    }

    fn foreground_drained_for_partial_phase(&self) -> Result<bool> {
        let repo = TaskQueueRepo::new(self.worker.db.conn());
        Ok(repo.count_pending_min_priority(PARTIAL_PRIORITY + 1)? == 0)
    }

    fn rebuild_matches(&mut self) -> Result<()> {
        let now = (self.worker.now)();
        let changed = RegroupQueueRepo::new(self.worker.db.conn()).load()?;
        self.rebuild_near_exact(now, &changed)?;
        self.maybe_run_whole_file_shadow_scan();
        let whole_file_mutated = self.rebuild_whole_file()?;
        let partial_mutated = self.rebuild_partial(now, &changed)?;
        if partial_mutated || whole_file_mutated {
            assign_best_copies(&mut self.worker.db, now)?;
        }
        self.worker
            .db
            .transaction(|conn| RegroupQueueRepo::new(conn).clear(changed.iter().copied()))?;
        self.last_foreground_delta_high_water = 0;
        self.last_partial_delta_high_water = 0;
        self.cadence.reset(now);
        self.log_instrumentation();
        self.maybe_prewarm_thumbnails();
        Ok(())
    }

    fn maybe_prewarm_thumbnails(&self) {
        let Some(thumbnails) = self.thumbnails.clone() else {
            return;
        };
        if self
            .prewarm_in_flight
            .swap(true, std::sync::atomic::Ordering::Acquire)
        {
            tracing::debug!(
                "thumbnail prewarm: previous pass still in flight; skipping this trigger"
            );
            return;
        }
        let guard = PrewarmInFlightGuard {
            flag: Arc::clone(&self.prewarm_in_flight),
        };
        let targets = match select_prewarm_targets(&self.worker.db) {
            Ok(t) => t,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "thumbnail prewarm: candidate selection failed; skipping this pass"
                );
                return;
            }
        };
        if targets.is_empty() {
            return;
        }
        let count = targets.len();
        std::thread::spawn(move || {
            let _guard = guard;
            prewarm_fan_out(&thumbnails, &targets);
            tracing::debug!(count, "thumbnail prewarm: pass complete");
        });
    }

    fn rebuild_near_exact(&mut self, now: i64, changed: &BTreeSet<FileId>) -> Result<()> {
        self.rebuild_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        rebuild_exact_groups(&mut self.worker.db, now)?;
        rebuild_near_duplicate_groups_incremental(
            &mut self.worker.db,
            LshParams::default(),
            now,
            changed,
        )?;
        assign_best_copies(&mut self.worker.db, now)?;
        Ok(())
    }

    fn rebuild_partial(&mut self, now: i64, changed: &BTreeSet<FileId>) -> Result<bool> {
        let outcome = rebuild_partial_clip_groups_durable(
            &mut self.partial_index,
            &mut self.worker.db,
            now,
            changed,
        )?;
        tracing::info!(
            groups_created = outcome.groups_created,
            skipped_short = outcome.skipped_short,
            dropped_below_coverage = outcome.dropped_below_coverage,
            dropped_single_vote = outcome.dropped_single_vote,
            "partial rebuild outcome",
        );
        Ok(outcome.groups_created > 0)
    }

    fn rebuild_whole_file(&mut self) -> Result<bool> {
        let now = (self.worker.now)();
        let outcome =
            rebuild_whole_file_groups(&mut self.worker.db, WholeFileParams::default(), now)?;
        Ok(outcome.groups_created > 0)
    }

    fn rebuild_near_exact_foreground(&mut self) -> Result<()> {
        let now = (self.worker.now)();
        let changed = RegroupQueueRepo::new(self.worker.db.conn()).load()?;
        if changed.is_empty() {
            return Ok(());
        }
        self.rebuild_near_exact(now, &changed)?;
        self.log_instrumentation();
        Ok(())
    }

    fn rebuild_partial_foreground(&mut self) -> Result<()> {
        let now = (self.worker.now)();
        let changed = RegroupQueueRepo::new(self.worker.db.conn()).load()?;
        if changed.is_empty() {
            return Ok(());
        }
        let partial_mutated = self.rebuild_partial(now, &changed)?;
        if partial_mutated {
            assign_best_copies(&mut self.worker.db, now)?;
        }
        self.log_instrumentation();
        Ok(())
    }

    fn backfill_partial_on_drain(&mut self) -> Result<()> {
        if !self.worker.partial_clips_enabled {
            return Ok(());
        }
        crate::watcher::enqueue_partial_backfill(
            &mut self.worker.db,
            &self.worker.task_kind,
            (self.worker.now)(),
        )?;
        Ok(())
    }

    fn maybe_run_whole_file_shadow_scan(&mut self) {
        if !whole_file_shadow_should_run(self.whole_file_shadow_ran, whole_file_shadow_enabled()) {
            return;
        }
        self.whole_file_shadow_ran = true;
        self.run_whole_file_shadow_scan();
    }

    fn run_whole_file_shadow_scan(&mut self) {
        let rows = match FingerprintsRepo::new(self.worker.db.conn()).list_active_tier2() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "[whole-shadow] Phase-A corpus read failed, scan skipped",
                );
                return;
            }
        };
        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::with_capacity(rows.len());
        for (file_id, blob) in rows {
            match decode_tier2(&blob) {
                Ok(fp) => corpus.push((file_id, fp)),
                Err(err) => tracing::warn!(
                    file_id = file_id.0,
                    error = %err,
                    "[whole-shadow] Tier 2 decode failed, file excluded from scan",
                ),
            }
        }
        let corpus_files = corpus.len();
        let candidates = scan_whole_file_candidates(&corpus, WholeFileParams::default());
        log_whole_file_shadow(corpus_files, &candidates);
    }

    fn log_instrumentation(&self) {
        let native = self.metrics.native_count();
        let fallback = self.metrics.fallback_count();
        tracing::info!(
            native,
            fallback,
            total = native + fallback,
            fallback_rate = self.metrics.fallback_rate(),
            "decode-path entry-rate snapshot",
        );

        match build_clusters(&self.worker.db) {
            Ok(clusters) => {
                let report = summarize_clusters(&clusters);
                tracing::info!(
                    clusters = report.clusters,
                    members = report.members_total,
                    exact = report.exact_clusters,
                    very_likely = report.very_likely_clusters,
                    possible = report.possible_clusters,
                    largest = report.largest_cluster,
                    cross_trust = report.cross_trust_clusters,
                    multi_group = report.multi_group_clusters,
                    "cluster FP-watch snapshot",
                );
            }
            Err(err) => tracing::warn!(
                error = %err,
                "cluster FP-watch snapshot skipped: could not project clusters",
            ),
        }
    }
}

fn whole_file_shadow_enabled() -> bool {
    std::env::var("VIDCULL_WHOLE_FILE_SHADOW")
        .ok()
        .is_some_and(|v| whole_file_shadow_value_enables(&v))
}

fn whole_file_shadow_value_enables(val: &str) -> bool {
    val == "1"
}

fn whole_file_shadow_should_run(already_ran: bool, enabled: bool) -> bool {
    !already_ran && enabled
}

fn log_whole_file_shadow(corpus_files: usize, candidates: &[WholeFileCandidate]) {
    for c in candidates {
        tracing::info!(
            a = c.a.0,
            b = c.b.0,
            scene_ratio = c.scene_ratio,
            span_coverage_a = c.span_coverage_a,
            span_coverage_b = c.span_coverage_b,
            coverage_ab = c.coverage_ab,
            coverage_ba = c.coverage_ba,
            offset_ab_ms = c.offset_ab_ms,
            offset_ba_ms = c.offset_ba_ms,
            offset_consistency_ab = c.offset_consistency_ab,
            offset_consistency_ba = c.offset_consistency_ba,
            passes_gate = c.passes_gate,
            "[whole-shadow] candidate",
        );
    }

    let gate_pass_density: Vec<f64> = candidates
        .iter()
        .filter(|c| c.passes_gate)
        .map(whole_file_density)
        .collect();
    let scene_ratio_min = WholeFileParams::default().scene_ratio_min;
    let near_equal_nonpass_density: Vec<f64> = candidates
        .iter()
        .filter(|c| !c.passes_gate && c.scene_ratio >= scene_ratio_min)
        .map(whole_file_density)
        .collect();
    let (gate_pass_density_min, gate_pass_density_max) = min_max(&gate_pass_density);
    let (near_equal_nonpass_density_min, near_equal_nonpass_density_max) =
        min_max(&near_equal_nonpass_density);

    tracing::info!(
        corpus_files,
        candidates_total = candidates.len(),
        gate_pass_count = gate_pass_density.len(),
        gate_pass_density_min,
        gate_pass_density_max,
        near_equal_nonpass_count = near_equal_nonpass_density.len(),
        near_equal_nonpass_density_min,
        near_equal_nonpass_density_max,
        "[whole-shadow] summary",
    );
}

fn whole_file_density(c: &WholeFileCandidate) -> f64 {
    c.coverage_ab.min(c.coverage_ba)
}

fn min_max(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}

fn probe_decode_fingerprint_streaming(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    native_budget: usize,
    fallback_budget: usize,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
) -> Result<(VideoMetadata, DecodePath, usize, FingerprintArtifacts)> {
    probe_decode_fingerprint_streaming_preparsed(
        bins,
        path,
        native_budget,
        fallback_budget,
        conc,
        PreParsedMp4::NotAttempted,
        cancel,
    )
    .map(|(metadata, decode_path, frame_count, artifacts, _thumb)| {
        (metadata, decode_path, frame_count, artifacts)
    })
}

fn probe_decode_fingerprint_streaming_preparsed(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    native_budget: usize,
    fallback_budget: usize,
    conc: &DecodeConcurrency,
    pre_parsed: PreParsedMp4,
    cancel: Cancel<'_>,
) -> Result<(
    VideoMetadata,
    DecodePath,
    usize,
    FingerprintArtifacts,
    Option<CapturedThumbFrame>,
)> {
    match probe_decode_fingerprint_streaming_once(
        bins,
        path,
        native_budget,
        fallback_budget,
        conc,
        pre_parsed,
        cancel,
    ) {
        Ok(ok) => Ok(ok),
        Err(err) => retry_base_decode_on_content_failure(&err, path, base_retry_disabled(), || {
            retry_via_pure_ffmpeg_fallback(bins, path, fallback_budget, conc, cancel)
        })
        .unwrap_or(Err(err)),
    }
}

fn probe_decode_fingerprint_streaming_once(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    native_budget: usize,
    fallback_budget: usize,
    conc: &DecodeConcurrency,
    pre_parsed: PreParsedMp4,
    cancel: Cancel<'_>,
) -> Result<(
    VideoMetadata,
    DecodePath,
    usize,
    FingerprintArtifacts,
    Option<CapturedThumbFrame>,
)> {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let path_buf = path.to_path_buf();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut tier1 = Tier1Builder::new();
        let mut tier2 = Tier2Builder::new();
        let mut sum_laplacian = 0.0_f64;
        let mut sum_dct = 0.0_f64;
        let mut frame_count = 0usize;
        let mut captured_thumb: Option<CapturedThumbFrame> = None;
        let mut captured_thumb_done = false;

        let (metadata, decode_path) = probe_and_decode_sparse_budgets_streaming_preparsed(
            bins,
            &path_buf,
            native_budget,
            fallback_budget,
            conc,
            pre_parsed,
            cancel,
            |f: &vidcull_parser::sparse::GrayscaleFrame| {
                if cancel.fired() {
                    return Err(vidcull_core::Error::Cancelled);
                }
                let gray = GrayFrame {
                    width: f.width,
                    height: f.height,
                    pixels: &f.pixels,
                };
                if let Some((phash, energy)) = tier1.push_and_analyze(&gray) {
                    tier2.push_phash(f.timestamp_ms, phash);
                    sum_dct += energy;
                }
                sum_laplacian += vidcull_fingerprint::laplacian_variance(&gray);
                frame_count += 1;
                if !captured_thumb_done {
                    captured_thumb = Some(CapturedThumbFrame {
                        width: f.width,
                        height: f.height,
                        pixels: f.pixels.clone(),
                    });
                    captured_thumb_done = f.timestamp_ms >= THUMB_CAPTURE_MIN_TS_MS;
                }
                Ok(())
            },
        )?;

        let artifacts = finish_streaming_artifacts(
            &metadata,
            tier1,
            tier2,
            sum_laplacian,
            sum_dct,
            frame_count,
        )?;
        Ok::<_, vidcull_core::Error>((
            metadata,
            decode_path,
            frame_count,
            artifacts,
            captured_thumb,
        ))
    }));

    match result {
        Ok(inner) => inner,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(
                path = %redact_fs_path(path),
                panic = %message,
                "native decode panicked; isolating to this file and continuing",
            );
            Err(Error::Decode(format!(
                "native decode panicked for {}: {message}",
                redact_fs_path(path)
            )))
        }
    }
}

fn retry_base_decode_on_content_failure<T>(
    err: &vidcull_core::Error,
    path: &std::path::Path,
    disabled: bool,
    retry: impl FnOnce() -> Result<T>,
) -> Option<Result<T>> {
    if matches!(err, vidcull_core::Error::Cancelled) {
        return None;
    }
    let reason = base_retry_reason(err)?;
    if disabled {
        tracing::debug!(
            path = %redact_fs_path(path),
            reason,
            error = %err,
            env = BASE_RETRY_DISABLE_ENV,
            "base-index retry skipped: disabled via env",
        );
        return None;
    }
    tracing::info!(
        path = %redact_fs_path(path),
        reason,
        error = %err,
        "base-index native decode failed post-delivery; retrying via pure ffmpeg fallback",
    );
    Some(retry())
}

fn retry_via_pure_ffmpeg_fallback(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    fallback_budget: usize,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
) -> Result<(
    VideoMetadata,
    DecodePath,
    usize,
    FingerprintArtifacts,
    Option<CapturedThumbFrame>,
)> {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let path_buf = path.to_path_buf();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let metadata = probe_fallback_cancellable(bins, &path_buf, cancel)?;
        let duration_ms = metadata
            .duration
            .map(vidcull_core::VideoDuration::as_millis)
            .filter(|ms| *ms > 0)
            .ok_or_else(|| {
                vidcull_core::Error::Unsupported(format!(
                    "base-index retry: ffmpeg probe reported no usable duration for {}",
                    redact_fs_path(&path_buf)
                ))
            })?;

        let mut tier1 = Tier1Builder::new();
        let mut tier2 = Tier2Builder::new();
        let mut sum_laplacian = 0.0_f64;
        let mut sum_dct = 0.0_f64;
        let mut frame_count = 0usize;
        let mut captured_thumb: Option<CapturedThumbFrame> = None;
        let mut captured_thumb_done = false;

        decode_sparse_strided_with_streaming(
            bins,
            &path_buf,
            duration_ms,
            metadata.resolution.width,
            metadata.resolution.height,
            fallback_budget,
            &metadata.codec,
            metadata.fps_x1000,
            metadata.has_b_frames,
            conc,
            cancel,
            |f: &vidcull_parser::sparse::GrayscaleFrame| {
                if cancel.fired() {
                    return Err(vidcull_core::Error::Cancelled);
                }
                let gray = GrayFrame {
                    width: f.width,
                    height: f.height,
                    pixels: &f.pixels,
                };
                if let Some((phash, energy)) = tier1.push_and_analyze(&gray) {
                    tier2.push_phash(f.timestamp_ms, phash);
                    sum_dct += energy;
                }
                sum_laplacian += vidcull_fingerprint::laplacian_variance(&gray);
                frame_count += 1;
                if !captured_thumb_done {
                    captured_thumb = Some(CapturedThumbFrame {
                        width: f.width,
                        height: f.height,
                        pixels: f.pixels.clone(),
                    });
                    captured_thumb_done = f.timestamp_ms >= THUMB_CAPTURE_MIN_TS_MS;
                }
                Ok(())
            },
        )?;

        let artifacts = finish_streaming_artifacts(
            &metadata,
            tier1,
            tier2,
            sum_laplacian,
            sum_dct,
            frame_count,
        )?;
        Ok::<_, vidcull_core::Error>((
            metadata,
            DecodePath::Fallback,
            frame_count,
            artifacts,
            captured_thumb,
        ))
    }));

    match result {
        Ok(inner) => inner,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(
                path = %redact_fs_path(path),
                panic = %message,
                "base-index retry: pure-ffmpeg decode panicked; isolating to this file",
            );
            Err(Error::Decode(format!(
                "base-index retry panicked for {}: {message}",
                redact_fs_path(path)
            )))
        }
    }
}

#[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
fn finish_streaming_artifacts(
    metadata: &VideoMetadata,
    tier1: Tier1Builder,
    tier2: Tier2Builder,
    sum_laplacian: f64,
    sum_dct: f64,
    frame_count: usize,
) -> Result<FingerprintArtifacts> {
    let phash = tier1.finish();
    let duration_ms = metadata.duration.unwrap_or(VideoDuration::ZERO).as_millis();
    let tier1_fp = vidcull_fingerprint::tier1::Tier1Fingerprint {
        duration_ms,
        codec: metadata.codec.clone(),
        gop: vidcull_fingerprint::tier1::GopSignature::from_durations(&[]),
        global_phash: phash,
    };
    let tier2_fp = tier2.finish();

    let (laplacian_variance, dct_energy) = if frame_count > 0 {
        (
            Some(sum_laplacian / frame_count as f64),
            Some(sum_dct / frame_count as f64),
        )
    } else {
        (None, None)
    };

    let bpp = if let (Some(bitrate), Some(fps_x1000), w, h) = (
        metadata.bitrate_bps,
        metadata.fps_x1000,
        metadata.resolution.width,
        metadata.resolution.height,
    ) {
        if w > 0 && h > 0 && fps_x1000 > 0 && bitrate > 0 {
            let fps = fps_x1000 as f64 / 1000.0;
            Some(bitrate as f64 / (w as f64 * h as f64 * fps))
        } else {
            None
        }
    } else {
        None
    };

    Ok(FingerprintArtifacts {
        tier1_blob: encode_tier1(&tier1_fp)?,
        tier2_blob: encode_tier2(&tier2_fp)?,
        laplacian_variance,
        dct_energy,
        bpp,
    })
}

#[cfg(test)]
fn isolate_decode_panic(
    path: &std::path::Path,
    decode: impl FnOnce() -> Result<vidcull_parser::DecodedVideo>,
) -> Result<vidcull_parser::DecodedVideo> {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    match catch_unwind(AssertUnwindSafe(decode)) {
        Ok(result) => result,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(
                path = %redact_fs_path(path),
                panic = %message,
                "native decode panicked; isolating to this file and continuing",
            );
            Err(Error::Decode(format!(
                "native decode panicked for {}: {message}",
                redact_fs_path(path)
            )))
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

impl TaskHandler for IndexingHandler {
    fn handle(&mut self, task: &Task) -> Result<()> {
        let payload = task.payload.as_deref().ok_or_else(|| {
            Error::Unsupported(format!("indexing task {} has no change payload", task.id))
        })?;
        let change = ChangeTask::from_payload(payload)?;

        self.worker.handle_change(&change, task.id)?;
        self.cadence.record_indexed(1);

        if self.queue_drained()? || self.cadence.should_rebuild((self.worker.now)()) {
            self.rebuild_matches()?;
        } else {
            let delta_rows = RegroupQueueRepo::new(self.worker.db.conn()).len()?;
            let delta_len = usize::try_from(delta_rows).unwrap_or(usize::MAX);
            if delta_len > self.last_partial_delta_high_water {
                self.rebuild_partial_foreground()?;
                self.last_partial_delta_high_water = delta_len;
            }
        }
        Ok(())
    }

    fn burst_chunk_size(&self) -> Option<usize> {
        Some(REGROUP_BURST_CHUNK)
    }

    fn after_burst_chunk(&mut self, processed: usize, drained: bool) -> Result<()> {
        self.cadence.record_indexed(processed);
        if drained {
            self.backfill_partial_on_drain()?;
        }
        if drained || self.cadence.should_rebuild((self.worker.now)()) {
            self.rebuild_matches()?;
        } else if self.queue_drained()? {
            let delta_rows = RegroupQueueRepo::new(self.worker.db.conn()).len()?;
            let delta_len = usize::try_from(delta_rows).unwrap_or(usize::MAX);
            if delta_len > self.last_foreground_delta_high_water {
                self.rebuild_near_exact_foreground()?;
                self.last_foreground_delta_high_water = delta_len;
            }
            if delta_len > self.last_partial_delta_high_water {
                self.rebuild_partial_foreground()?;
                self.last_partial_delta_high_water = delta_len;
            }
        }
        Ok(())
    }

    fn set_partial_clips_live(&mut self, enabled: bool) {
        if enabled != self.worker.partial_clips_enabled {
            self.configure_partial_index(enabled);
        }
        self.worker.set_partial_clips_enabled(enabled);
    }

    fn set_decode_budget(&mut self, budget: usize) {
        let foreground_drained = self.foreground_drained_for_partial_phase().unwrap_or(false);
        let partial_headroom = partial_headroom_k();
        let decode_capacity = if foreground_drained {
            budget.saturating_sub(partial_headroom).max(1)
        } else {
            budget
        };
        if foreground_drained {
            tracing::debug!(
                stage = "partial_phase_headroom",
                budget,
                partial_headroom,
                decode_capacity,
                "foreground drained — reserving decode_conc headroom for the \
                 partial-clip fold phase",
            );
        }
        self.decode_conc.set_capacity(decode_capacity);
        self.partial_gate
            .set_capacity(budget.saturating_sub(1).max(1));
        self.base_decode_gate
            .set_capacity(budget.clamp(1, BASE_DECODE_CONCURRENCY));
        self.seq_read_gate.set_capacity(seq_read_cap_for_budget(budget));
        if std::env::var_os("VIDCULL_RESOURCE_LOG").is_some() {
            let (dc_in, dc_cap) = self.decode_conc.snapshot();
            let (bg_in, bg_cap) = self.base_decode_gate.snapshot();
            tracing::info!(
                stage = "gates",
                decode_conc_in_use = dc_in,
                decode_conc_cap = dc_cap,
                base_gate_in_use = bg_in,
                base_gate_cap = bg_cap,
                "gate utilisation snapshot",
            );
        }
    }

    fn link_shutdown(&mut self, flag: Arc<std::sync::atomic::AtomicBool>) {
        self.single_flight.link_shutdown(flag);
    }

    fn link_cancel_source(&mut self, control: Arc<crate::throttle::ThrottleControl>) {
        self.worker.set_cancel_source(control);
    }

    fn as_parallel_worker(&self) -> Option<crate::ParallelWorkerConfig> {
        let db_path = self.worker.db.path()?.to_path_buf();
        Some(crate::ParallelWorkerConfig {
            db_path,
            bins: self.worker.bins.clone(),
            budget: self.worker.budget,
            fallback_budget: self.worker.fallback_budget,
            task_kind: self.worker.task_kind.clone(),
            now: self.worker.now,
            metrics: Arc::clone(&self.metrics),
            single_flight: Arc::clone(&self.single_flight),
            partial_clips_enabled: self.worker.partial_clips_enabled,
            decode_concurrency: Arc::clone(&self.decode_conc),
            partial_gate: Arc::clone(&self.partial_gate),
            base_decode_gate: Arc::clone(&self.base_decode_gate),
            seq_read_gate: Arc::clone(&self.seq_read_gate),
        })
    }

    fn trailing_rebuild(&mut self) -> Result<()> {
        self.backfill_partial_on_drain()?;
        self.rebuild_matches()
    }
}

pub(crate) fn mtime_nanos(meta: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos())
            .map(|n| -n)
            .unwrap_or(i64::MIN),
    }
}

fn is_file_locked(path: &std::path::Path) -> bool {
    match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(_) => false,
        Err(err) => {
            #[cfg(target_os = "windows")]
            {
                if let Some(32) = err.raw_os_error() {
                    return true;
                }
            }
            err.kind() == std::io::ErrorKind::PermissionDenied
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cancel, DEFAULT_DECODE_SLOW_MS, DEFAULT_DECODE_SLOW_TOTAL_MS, DEFAULT_GATE_HOLD_WARN_MS,
        DEFAULT_OVERDECODE_FACTOR, DecodeHealth, DecodePath, SingleFlight, assess_decode,
        full_index_decode_is_pathological, gate_hold_is_excessive, isolate_decode_panic,
        panic_message,
    };
    use std::path::Path;
    use vidcull_core::Error;

    #[test]
    fn gate_hold_excessive_above_threshold_only() {
        assert!(
            gate_hold_is_excessive(90_000, DEFAULT_GATE_HOLD_WARN_MS),
            "a 90 s hold past a 60 s threshold is a head-of-line risk"
        );
        assert!(!gate_hold_is_excessive(
            DEFAULT_GATE_HOLD_WARN_MS,
            DEFAULT_GATE_HOLD_WARN_MS
        ));
        assert!(!gate_hold_is_excessive(1_200, DEFAULT_GATE_HOLD_WARN_MS));
    }

    #[test]
    fn assess_decode_flags_slow_above_threshold_only() {
        let h = assess_decode(
            10,
            10,
            12_000,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert!(h.slow, "1200 ms/frame should be slow: {h:?}");
        assert_eq!(h.ms_per_frame, 1200);
        assert!(!h.over_decode, "emitted == planned is not over-decode");

        let at = assess_decode(
            10,
            10,
            10_000,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert!(!at.slow, "at-threshold must not fire: {at:?}");
        assert_eq!(at.ms_per_frame, 1000);
    }

    #[test]
    fn assess_decode_flags_over_decode_above_factor_only() {
        let h = assess_decode(41, 10, 0, DEFAULT_DECODE_SLOW_MS, DEFAULT_OVERDECODE_FACTOR);
        assert!(h.over_decode, "41 > 10*4 should over-decode: {h:?}");

        let at = assess_decode(40, 10, 0, DEFAULT_DECODE_SLOW_MS, DEFAULT_OVERDECODE_FACTOR);
        assert!(!at.over_decode, "at-factor must not fire: {at:?}");
    }

    #[test]
    fn assess_decode_zero_frames_is_not_pathological() {
        let h = assess_decode(
            0,
            10,
            999_999,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert_eq!(h.ms_per_frame, 0);
        assert!(
            !h.is_pathological(),
            "no frames decoded must not warn: {h:?}"
        );
    }

    #[test]
    fn assess_decode_catches_105_mkv_pathology() {
        let grid = 32usize;
        let h = assess_decode(
            grid,
            grid,
            5270 * grid as u64,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert!(h.slow, "5270 ms/frame must trip the slow rule: {h:?}");
        assert!(h.is_pathological(), "case must warrant a WARN: {h:?}");
        assert_eq!(h.ms_per_frame, 5270);
    }

    #[test]
    fn full_index_quarantines_only_slow_fallback() {
        let floor = DEFAULT_DECODE_SLOW_TOTAL_MS;
        let slow = assess_decode(
            12,
            0,
            63_000,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert!(slow.slow, "5250 ms/frame should be slow: {slow:?}");
        assert!(
            !slow.over_decode,
            "grid=0 must disable over_decode: {slow:?}"
        );
        assert!(
            full_index_decode_is_pathological(DecodePath::Fallback, slow, 63_000, floor),
            "a sustained slow fallback decode must quarantine"
        );
        assert!(
            !full_index_decode_is_pathological(DecodePath::Native, slow, 63_000, floor),
            "native decode must never be quarantined by the wall-time floor"
        );
        let short = assess_decode(
            2,
            0,
            2_200,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert!(short.slow, "1100 ms/frame is slow per frame: {short:?}");
        assert!(
            !full_index_decode_is_pathological(DecodePath::Fallback, short, 2_200, floor),
            "a short slow-per-frame clip under the total floor must not quarantine"
        );
        let fast = assess_decode(
            12,
            0,
            1_200,
            DEFAULT_DECODE_SLOW_MS,
            DEFAULT_OVERDECODE_FACTOR,
        );
        assert!(!fast.slow, "100 ms/frame must not be slow: {fast:?}");
        assert!(!full_index_decode_is_pathological(
            DecodePath::Fallback,
            fast,
            1_200,
            floor
        ));
        let over_only = DecodeHealth {
            ms_per_frame: 50,
            slow: false,
            over_decode: true,
        };
        assert!(
            !full_index_decode_is_pathological(DecodePath::Fallback, over_only, 999_999, floor),
            "over_decode without slow must not trip (invalid signal for -copyts)"
        );
    }

    #[test]
    fn whole_file_shadow_value_enables_only_exact_one() {
        assert!(
            super::whole_file_shadow_value_enables("1"),
            "\"1\" enables the scan"
        );
        assert!(!super::whole_file_shadow_value_enables("0"), "\"0\" is off");
        assert!(
            !super::whole_file_shadow_value_enables("true"),
            "\"true\" != \"1\""
        );
        assert!(!super::whole_file_shadow_value_enables(""), "empty is off");
    }

    #[test]
    fn whole_file_shadow_should_run_truth_table() {
        assert!(
            super::whole_file_shadow_should_run(false, true),
            "not-yet-run + gate enabled -> run"
        );
        assert!(
            !super::whole_file_shadow_should_run(true, true),
            "already-run + gate enabled -> latched, do not re-run"
        );
        assert!(
            !super::whole_file_shadow_should_run(false, false),
            "not-yet-run + gate disabled -> do not run"
        );
        assert!(
            !super::whole_file_shadow_should_run(true, false),
            "already-run + gate disabled -> do not run"
        );
    }

    #[test]
    fn whole_file_density_is_min_of_coverages() {
        use super::{FileId, WholeFileCandidate};

        let c = WholeFileCandidate {
            a: FileId(1),
            b: FileId(2),
            scene_count_a: 10,
            scene_count_b: 10,
            scene_ratio: 1.0,
            span_coverage_a: 1.0,
            span_coverage_b: 1.0,
            coverage_ab: 0.30,
            coverage_ba: 0.15,
            offset_ab_ms: 0,
            offset_ba_ms: 0,
            offset_consistency_ab: 1.0,
            offset_consistency_ba: 1.0,
            passes_gate: true,
        };
        assert!((super::whole_file_density(&c) - 0.15).abs() < f64::EPSILON);
    }

    #[test]
    fn min_max_empty_and_populated() {
        assert_eq!(super::min_max(&[]), (0.0, 0.0));
        assert_eq!(super::min_max(&[0.5, 0.1, 0.9, 0.3]), (0.1, 0.9));
        assert_eq!(super::min_max(&[0.42]), (0.42, 0.42));
    }

    #[test]
    fn whole_file_shadow_scan_is_noop_without_env_gate() {
        use super::{FfmpegBinaries, IndexingHandler};

        assert!(
            std::env::var_os("VIDCULL_WHOLE_FILE_SHADOW").is_none(),
            "test assumption: the gate env var is unset in this process"
        );
        let db = vidcull_db::open_in_memory().expect("open db");
        let mut handler = IndexingHandler::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            || 0_i64,
        );
        assert!(!handler.whole_file_shadow_ran, "latch starts unset");
        handler.maybe_run_whole_file_shadow_scan();
        assert!(
            !handler.whole_file_shadow_ran,
            "opt-in gate defaults off: the scan must not run/latch"
        );
    }

    #[test]
    fn panic_in_decode_becomes_decode_error_not_unwind() {
        let path = Path::new("corrupt/clip.mp4");
        let result = isolate_decode_panic(path, || {
            panic!("synthetic native decoder panic");
        });
        match result {
            Err(Error::Decode(msg)) => {
                assert!(!msg.contains("corrupt/clip.mp4"), "raw path leaked: {msg}");
                assert!(msg.contains(".mp4"), "redacted ext missing: {msg}");
                assert!(
                    msg.contains("synthetic native decoder panic"),
                    "panic message missing: {msg}"
                );
            }
            other => panic!("expected Error::Decode from a panicking decode, got {other:?}"),
        }
    }

    #[test]
    fn mp4_mkv_and_webm_containers_enter_their_fused_pass() {
        use super::fusion_eligible;
        for eligible in ["a.mp4", "b.M4V", "c.mov", "d.3gp", "e.mkv", "f.WEBM"] {
            assert!(fusion_eligible(Path::new(eligible)), "{eligible} must fuse");
        }
        for plain in ["c.avi", "d.ts", "e.txt", "noext"] {
            assert!(!fusion_eligible(Path::new(plain)), "{plain} must NOT fuse");
        }
    }

    #[test]
    fn daemon_mkv_fusion_preserves_full_hash_and_preparsed_metadata() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vidcull-parser")
            .join("tests")
            .join("fixtures")
            .join("black_320x180_30fps_1s.mkv");
        let expected = vidcull_fingerprint::hash_file(&path).expect("standalone hash");
        let (actual, pre_parsed) =
            super::hash_file_fusing_mp4_parse(&path, Cancel::default()).expect("fused hash");
        assert_eq!(actual, expected);
        let vidcull_parser::PreParsedMp4::MkvParsed(metadata) = pre_parsed else {
            panic!("MKV fusion must return reusable metadata");
        };
        assert_eq!(metadata.container, vidcull_parser::ContainerKind::Mkv);
        assert!(!metadata.resolution.is_empty());
    }

    #[test]
    fn fuse_kill_switch_parses_env_values() {
        use super::fuse_hash_parse_enabled_from;
        assert!(fuse_hash_parse_enabled_from(None));
        assert!(fuse_hash_parse_enabled_from(Some("1")));
        assert!(fuse_hash_parse_enabled_from(Some("on")));
        assert!(!fuse_hash_parse_enabled_from(Some("0")));
        assert!(!fuse_hash_parse_enabled_from(Some(" 0 ")));
        assert!(!fuse_hash_parse_enabled_from(Some("false")));
        assert!(!fuse_hash_parse_enabled_from(Some("FALSE")));
    }

    #[test]
    fn seq_read_cap_env_parses_and_zero_means_ungated() {
        use super::{SEQ_READ_CONCURRENCY, seq_read_cap_from};
        assert_eq!(
            seq_read_cap_from(None, SEQ_READ_CONCURRENCY),
            SEQ_READ_CONCURRENCY
        );
        assert_eq!(seq_read_cap_from(Some("2"), SEQ_READ_CONCURRENCY), 2);
        assert_eq!(seq_read_cap_from(Some(" 8 "), SEQ_READ_CONCURRENCY), 8);
        assert_eq!(
            seq_read_cap_from(Some("0"), SEQ_READ_CONCURRENCY),
            usize::MAX
        );
        assert_eq!(
            seq_read_cap_from(Some("lots"), SEQ_READ_CONCURRENCY),
            SEQ_READ_CONCURRENCY
        );
        assert_eq!(seq_read_cap_from(None, 17), 17, "falls back to the given default");
    }

    #[test]
    fn prewarm_fan_out_workers_is_bounded_by_cores_and_target_count() {
        use super::prewarm_fan_out_workers;
        assert_eq!(
            prewarm_fan_out_workers(0),
            1,
            "must never return zero workers, even for an empty target list"
        );
        assert_eq!(
            prewarm_fan_out_workers(1),
            1,
            "a single target never needs more than one worker"
        );
        let cores = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        assert_eq!(
            prewarm_fan_out_workers(1_000_000),
            cores,
            "a huge target list must not spawn more workers than available cores"
        );
        assert!(
            prewarm_fan_out_workers(2) <= cores.max(2),
            "worker count is always bounded by core count"
        );
    }

    #[test]
    fn seq_read_gate_caps_reads_independently_of_decode_gate() {
        use super::BaseDecodeGate;
        let seq = BaseDecodeGate::new(2);
        let base = BaseDecodeGate::new(2);
        let r1 = seq.try_acquire().expect("read slot 1");
        let _r2 = seq.try_acquire().expect("read slot 2");
        assert!(
            seq.try_acquire().is_none(),
            "3rd concurrent read must be refused"
        );
        assert!(
            base.try_acquire().is_some(),
            "a full read gate must not block the decode gate"
        );
        drop(r1);
        assert!(
            seq.try_acquire().is_some(),
            "released read slot must be reusable"
        );
    }

    #[test]
    fn single_flight_gates_same_hash_and_frees_distinct() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;
        use vidcull_core::types::Blake3Hash;

        let flight = Arc::new(SingleFlight::default());
        let hash = Blake3Hash::from_bytes([7u8; 32]);
        let other = Blake3Hash::from_bytes([9u8; 32]);

        let held = flight.begin(hash, || false);

        drop(flight.begin(other, || false));

        let entered = Arc::new(AtomicBool::new(false));
        let flight2 = Arc::clone(&flight);
        let entered2 = Arc::clone(&entered);
        let waiter = thread::spawn(move || {
            let _g = flight2.begin(hash, || false);
            entered2.store(true, Ordering::SeqCst);
        });

        thread::sleep(Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::SeqCst),
            "a second claim on the same hash must block while it is held in-flight"
        );

        drop(held);
        waiter.join().expect("waiter thread");
        assert!(
            entered.load(Ordering::SeqCst),
            "waiter proceeds after the in-flight claim is released"
        );
    }

    #[test]
    fn single_flight_shutdown_unblocks_a_parked_waiter() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};
        use vidcull_core::types::Blake3Hash;

        let flight = Arc::new(SingleFlight::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        flight.link_shutdown(Arc::clone(&shutdown));
        let hash = Blake3Hash::from_bytes([3u8; 32]);

        let held = flight.begin(hash, || false);

        let flight2 = Arc::clone(&flight);
        let waiter = thread::spawn(move || {
            let start = Instant::now();
            let _g = flight2.begin(hash, || false);
            start.elapsed()
        });

        thread::sleep(Duration::from_millis(20));
        shutdown.store(true, Ordering::SeqCst);

        let elapsed = waiter.join().expect("waiter thread");
        assert!(
            elapsed < Duration::from_millis(2_000),
            "shutdown-set waiter must unblock within the poll bound, took {elapsed:?}"
        );
        drop(held);
    }

    #[test]
    fn single_flight_without_shutdown_still_serializes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;
        use vidcull_core::types::Blake3Hash;

        let flight = Arc::new(SingleFlight::default());
        let hash = Blake3Hash::from_bytes([5u8; 32]);
        let held = flight.begin(hash, || false);

        let entered = Arc::new(AtomicBool::new(false));
        let flight2 = Arc::clone(&flight);
        let entered2 = Arc::clone(&entered);
        let waiter = thread::spawn(move || {
            let _g = flight2.begin(hash, || false);
            entered2.store(true, Ordering::SeqCst);
        });

        thread::sleep(Duration::from_millis(200));
        assert!(
            !entered.load(Ordering::SeqCst),
            "same-hash claim must stay blocked while held with no shutdown",
        );

        drop(held);
        waiter.join().expect("waiter thread");
        assert!(
            entered.load(Ordering::SeqCst),
            "waiter proceeds after the in-flight claim is released",
        );
    }

    #[test]
    fn non_panicking_error_passes_through_unchanged() {
        let path = Path::new("clip.mkv");
        let result =
            isolate_decode_panic(path, || Err(Error::Parse("ordinary parse failure".into())));
        match result {
            Err(Error::Parse(msg)) => assert_eq!(msg, "ordinary parse failure"),
            other => panic!("expected the original Error::Parse, got {other:?}"),
        }
    }

    #[test]
    fn partial_fingerprint_yields_busy_while_the_gate_is_held() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics, FfmpegBinaries,
            FilesRepo, IndexingWorker, NewFile, NormalizedPath, PartialDecodeGate, SingleFlight,
        };
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("clip.mp4");
        std::fs::write(&file_path, b"stub bytes").expect("write temp file");
        let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());
        assert!(
            norm.to_native_path().exists(),
            "temp fixture must resolve on disk"
        );

        let db = vidcull_db::open_in_memory().expect("open db");
        FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                ..Default::default()
            })
            .expect("insert file row");

        let gate = Arc::new(PartialDecodeGate::new(1));
        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );
        worker.set_partial_clips_enabled(true);
        worker.set_partial_gate(Arc::clone(&gate));

        let held = gate.try_acquire().expect("occupy the only gate slot");
        let result = worker.partial_fingerprint_file(&norm);
        drop(held);

        assert!(
            matches!(result, Err(Error::Busy(_))),
            "partial pass must yield Busy while the gate is held, got {result:?}"
        );

        let result_free = worker.partial_fingerprint_file(&norm);
        assert!(
            !matches!(result_free, Err(Error::Busy(_))),
            "an uncontended gate must not yield Busy, got {result_free:?}"
        );
    }

    #[test]
    fn partial_decode_gate_admits_capacity_concurrent_and_refuses_overflow() {
        use super::PartialDecodeGate;

        let gate = PartialDecodeGate::new(3);
        let a = gate.try_acquire().expect("1st slot");
        let b = gate.try_acquire().expect("2nd slot");
        let c = gate.try_acquire().expect("3rd slot");
        assert!(
            gate.try_acquire().is_none(),
            "a 4th acquire must be refused while three slots are held"
        );

        drop(b);
        let d = gate.try_acquire().expect("a freed slot is reusable");
        assert!(
            gate.try_acquire().is_none(),
            "still at capacity after reusing the freed slot"
        );
        drop((a, c, d));
        assert!(
            gate.try_acquire().is_some(),
            "all slots released → the gate admits again"
        );

        let gate = PartialDecodeGate::new(1);
        let held = gate.try_acquire().expect("slot at capacity 1");
        gate.set_capacity(2);
        let _second = gate.try_acquire().expect("raised capacity opens a slot");
        assert!(
            gate.try_acquire().is_none(),
            "capacity 2 with two held refuses a third"
        );
        drop(held);
        gate.set_capacity(1);
        assert!(
            gate.try_acquire().is_none(),
            "a shrink below the in-flight count refuses without preempting"
        );
    }

    #[test]
    fn partial_gate_has_capacity_tracks_free_slots() {
        use super::PartialDecodeGate;

        let gate = PartialDecodeGate::new(2);
        assert!(gate.has_capacity(), "fresh gate has free slots");
        let a = gate.try_acquire().expect("1st slot");
        assert!(gate.has_capacity(), "one of two slots free");
        let b = gate.try_acquire().expect("2nd slot");
        assert!(!gate.has_capacity(), "full gate reports no capacity");
        assert!(
            gate.try_acquire().is_none(),
            "has_capacity()==false agrees with try_acquire()==None",
        );
        drop(b);
        assert!(gate.has_capacity(), "releasing a slot restores capacity");
        gate.set_capacity(1);
        assert!(!gate.has_capacity(), "shrunk-below-in-flight reads as full");
        drop(a);
        assert!(gate.has_capacity(), "capacity 1, none held → free again");
    }

    #[test]
    fn av1_consumption_skip_stamps_marker_and_self_heals_on_replace() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics, FfmpegBinaries,
            FilesRepo, Fingerprint, FingerprintsRepo, IndexingWorker, NewFile, NormalizedPath,
            SingleFlight,
        };
        use std::sync::Arc;
        use vidcull_core::types::Codec;
        use vidcull_fingerprint::format::FORMAT_VERSION;

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("clip.mp4");
        std::fs::write(&file_path, b"stub bytes").expect("write temp file");
        let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());

        let db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                size_bytes: 4_000_000_000,
                mtime_ns: 12_345,
                codec: Some(Codec::Av1),
                ..Default::default()
            })
            .expect("insert AV1 file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![1, 2, 3],
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row");

        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );
        worker.set_partial_clips_enabled(true);

        worker
            .partial_fingerprint_file(&norm)
            .expect("av1 partial skip returns Ok");
        let marker = FingerprintsRepo::new(worker.db.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker stamped");
        assert_eq!(marker.reason, super::PARTIAL_SKIP_REASON_UNSUPPORTED_CODEC);
        assert_eq!(marker.size_bytes, 4_000_000_000);
        assert_eq!(marker.mtime_ns, 12_345);
        assert!(
            FingerprintsRepo::new(worker.db.conn())
                .get_active_partial(file_id)
                .expect("get partial")
                .is_none(),
            "marker must not populate partial_temporal",
        );

        worker
            .partial_fingerprint_file(&norm)
            .expect("re-run hits valid marker and returns Ok");

        FilesRepo::new(worker.db.conn())
            .update_metadata(
                file_id,
                &NewFile {
                    path: norm.clone(),
                    size_bytes: 50_000,
                    mtime_ns: 99_999,
                    codec: Some(Codec::Av1),
                    ..Default::default()
                },
            )
            .expect("update_metadata replace (still AV1 → marker kept)");
        worker
            .partial_fingerprint_file(&norm)
            .expect("stale marker + AV1 → re-stamp, Ok");
        let refreshed = FingerprintsRepo::new(worker.db.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker re-stamped at new identity");
        assert_eq!(
            refreshed.size_bytes, 50_000,
            "stale marker re-stamped at the replacement's identity",
        );
        assert_eq!(refreshed.mtime_ns, 99_999);
    }

    #[test]
    fn confirmed_exact_full_dup_gate_stamps_marker() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, DuplicateGroupsRepo,
            FallbackMetrics, FfmpegBinaries, FilesRepo, Fingerprint, FingerprintsRepo,
            IndexingWorker, NewFile, NormalizedPath, SingleFlight, TrustLevel,
        };
        use std::sync::Arc;
        use vidcull_fingerprint::format::FORMAT_VERSION;

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("clip.mp4");
        std::fs::write(&file_path, b"stub bytes").expect("write temp file");
        let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());

        let db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                size_bytes: 123_456,
                mtime_ns: 7_777,
                ..Default::default()
            })
            .expect("insert file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![1, 2, 3],
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row");
        let other_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: NormalizedPath::new("/lib/other.mp4"),
                ..Default::default()
            })
            .expect("insert other member");
        let groups = DuplicateGroupsRepo::new(db.conn());
        let gid = groups
            .create(TrustLevel::Exact, 0)
            .expect("create EXACT group");
        groups.add_member(gid, file_id).expect("add member");
        groups.add_member(gid, other_id).expect("add other member");

        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );
        worker.set_partial_clips_enabled(true);

        worker
            .partial_fingerprint_file(&norm)
            .expect("exact full-dup gate returns Ok");
        let marker = FingerprintsRepo::new(worker.db.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker stamped");
        assert_eq!(marker.reason, super::PARTIAL_SKIP_REASON_EXACT_FULL_DUP);
        assert_eq!(marker.size_bytes, 123_456);
        assert_eq!(marker.mtime_ns, 7_777);
        assert!(
            FingerprintsRepo::new(worker.db.conn())
                .get_active_partial(file_id)
                .expect("get partial")
                .is_none(),
            "the gate skip must not populate partial_temporal",
        );
    }

    #[test]
    fn twin_copy_stamps_marker_atomically_with_fingerprint_copy() {
        use super::{
            Blake3Hash, DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics,
            FfmpegBinaries, FilesRepo, Fingerprint, FingerprintsRepo, IndexingWorker, NewFile,
            NormalizedPath, SingleFlight,
        };
        use std::sync::Arc;
        use vidcull_core::types::HASH_LEN;
        use vidcull_fingerprint::format::FORMAT_VERSION;

        let db = vidcull_db::open_in_memory().expect("open db");
        let hash = Blake3Hash::from_bytes([0x42u8; HASH_LEN]);
        let twin_path = NormalizedPath::new("/lib/twin_original.mp4");
        let twin_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: twin_path.clone(),
                size_bytes: 55_000,
                mtime_ns: 111,
                content_hash: Some(hash),
                ..Default::default()
            })
            .expect("insert twin file row");
        let twin_fp = Fingerprint {
            file_id: twin_id,
            tier1_global: vec![9, 9, 9],
            tier2_temporal: Some(vec![1, 1]),
            format_version: u32::from(FORMAT_VERSION),
            created_at: 0,
        };
        FingerprintsRepo::new(db.conn())
            .upsert(&twin_fp)
            .expect("seed twin fingerprint row");
        let twin = FilesRepo::new(db.conn())
            .find_by_path(&twin_path)
            .expect("find twin")
            .expect("twin row");

        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );

        let copy_path = NormalizedPath::new("/lib/twin_copy.mp4");
        worker
            .index_as_twin_copy(&copy_path, &twin, &twin_fp, 55_000, 222, hash)
            .expect("index_as_twin_copy succeeds");

        let copy_id = FilesRepo::new(worker.db.conn())
            .find_by_path(&copy_path)
            .expect("find copy")
            .expect("copy row")
            .id;
        let fingerprints = FingerprintsRepo::new(worker.db.conn());
        let copied_fp = fingerprints
            .get(copy_id)
            .expect("get fp")
            .expect("fp row exists");
        assert_eq!(copied_fp.tier1_global, vec![9, 9, 9]);
        let marker = fingerprints
            .get_partial_skip(copy_id)
            .expect("get marker")
            .expect("marker stamped on the twin copy");
        assert_eq!(marker.reason, super::PARTIAL_SKIP_REASON_EXACT_FULL_DUP);
        assert_eq!(marker.size_bytes, 55_000);
        assert_eq!(marker.mtime_ns, 222);
    }

    #[test]
    fn partial_skip_hazard_exempts_exact_full_dup_marker_even_with_matching_identity() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics, FfmpegBinaries,
            FilesRepo, Fingerprint, FingerprintsRepo, IndexingWorker, NewFile, NormalizedPath,
            PartialSkipMarker, SingleFlight,
        };
        use std::sync::Arc;
        use vidcull_fingerprint::format::FORMAT_VERSION;

        let db = vidcull_db::open_in_memory().expect("open db");
        let norm = NormalizedPath::new("/lib/stranded_twin.mp4");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                size_bytes: 999_000,
                mtime_ns: 55_555,
                ..Default::default()
            })
            .expect("insert file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![1, 2, 3],
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row");
        FingerprintsRepo::new(db.conn())
            .set_partial_skip(
                file_id,
                &PartialSkipMarker {
                    reason: super::PARTIAL_SKIP_REASON_EXACT_FULL_DUP.into(),
                    size_bytes: 999_000,
                    mtime_ns: 55_555,
                },
            )
            .expect("seed exact-full-dup marker at current identity");
        let existing = FilesRepo::new(db.conn())
            .find_by_path(&norm)
            .expect("find")
            .expect("file row");

        let worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );

        assert!(
            !worker
                .partial_skip_hazard(&existing, &norm)
                .expect("hazard check"),
            "an identity-matched exact-full-dup marker must NOT be treated as a \
             hazard — it must fall through to a live is_confirmed_full_dup re-check",
        );
    }

    #[test]
    fn partial_retry_budget_reason_fires_only_at_the_budget() {
        use super::{
            ChangeKind, ChangeTask, NormalizedPath, PARTIAL_RETRY_BUDGET,
            PARTIAL_SKIP_REASON_RETRY_EXHAUSTED, partial_retry_budget_reason,
        };
        use vidcull_db::repo::TaskQueueRepo;

        let db = vidcull_db::open_in_memory().expect("open db");
        let kind = "scan";
        let path = NormalizedPath::new("/lib/flaky.mp4");
        let payload = ChangeTask {
            path: path.clone(),
            change: ChangeKind::PartialFingerprint,
            size_bytes: 0,
        }
        .to_payload()
        .expect("encode payload");

        assert_eq!(
            partial_retry_budget_reason(&db, kind, &path).expect("q"),
            None,
            "zero prior FAILED rows must never exhaust the budget"
        );

        let repo = TaskQueueRepo::new(db.conn());
        for _ in 0..(PARTIAL_RETRY_BUDGET - 1) {
            let id = repo
                .enqueue(&vidcull_db::repo::NewTask {
                    kind: kind.to_owned(),
                    priority: -200,
                    payload: Some(payload.clone()),
                    enqueued_at: 0,
                    size_bytes: 0,
                })
                .expect("enqueue");
            repo.dequeue_next(kind, 0).expect("dq").expect("task");
            repo.mark_failed(id, 0, "io error").expect("fail");
        }
        assert_eq!(
            partial_retry_budget_reason(&db, kind, &path).expect("q"),
            None,
            "below-budget FAILED rows must keep retrying"
        );

        let id = repo
            .enqueue(&vidcull_db::repo::NewTask {
                kind: kind.to_owned(),
                priority: -200,
                payload: Some(payload.clone()),
                enqueued_at: 0,
                size_bytes: 0,
            })
            .expect("enqueue");
        repo.dequeue_next(kind, 0).expect("dq").expect("task");
        repo.mark_failed(id, 0, "io error").expect("fail");
        assert_eq!(
            partial_retry_budget_reason(&db, kind, &path).expect("q"),
            Some(PARTIAL_SKIP_REASON_RETRY_EXHAUSTED),
            "reaching PARTIAL_RETRY_BUDGET FAILED rows must exhaust the budget"
        );

        let other = NormalizedPath::new("/lib/unrelated.mp4");
        assert_eq!(
            partial_retry_budget_reason(&db, kind, &other).expect("q"),
            None,
            "an unrelated path's FAILED rows must not leak across payloads"
        );
    }

    #[test]
    fn enqueue_partial_if_missing_triggers_only_when_on_and_absent() {
        use super::{
            ChangeKind, ChangeTask, DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET,
            FallbackMetrics, FfmpegBinaries, FilesRepo, Fingerprint, FingerprintsRepo,
            IndexingWorker, NewFile, NormalizedPath, SingleFlight,
        };
        use std::sync::Arc;
        use vidcull_db::repo::{TaskQueueRepo, TaskState};
        use vidcull_fingerprint::format::FORMAT_VERSION;

        fn pending_partial_count(worker: &IndexingWorker) -> usize {
            TaskQueueRepo::new(worker.db.conn())
                .list_by_state(TaskState::Pending)
                .expect("pending tasks")
                .iter()
                .filter(|t| {
                    t.payload
                        .as_deref()
                        .and_then(|p| ChangeTask::from_payload(p).ok())
                        .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
                })
                .count()
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("clip.mp4");
        std::fs::write(&file_path, b"stub bytes").expect("write temp file");
        let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());

        let db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                ..Default::default()
            })
            .expect("insert file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![1, 2, 3],
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row");

        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );

        worker.set_partial_clips_enabled(false);
        worker
            .enqueue_partial_if_missing(&norm, file_id)
            .expect("off no-op");
        assert_eq!(pending_partial_count(&worker), 0, "OFF enqueues nothing");

        worker.set_partial_clips_enabled(true);
        worker
            .enqueue_partial_if_missing(&norm, file_id)
            .expect("on + missing enqueues");
        assert_eq!(
            pending_partial_count(&worker),
            1,
            "ON with no partial fingerprint enqueues exactly one PartialFingerprint",
        );

        worker
            .enqueue_partial_if_missing(&norm, file_id)
            .expect("re-call dedups");
        assert_eq!(
            pending_partial_count(&worker),
            1,
            "an already-queued PartialFingerprint is deduped, not duplicated",
        );

        for task in TaskQueueRepo::new(worker.db.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending")
        {
            TaskQueueRepo::new(worker.db.conn())
                .mark_done(task.id, 0)
                .expect("mark done");
        }
        FingerprintsRepo::new(worker.db.conn())
            .set_partial(file_id, &[9, 9, 9])
            .expect("seed an existing partial fingerprint");
        worker
            .enqueue_partial_if_missing(&norm, file_id)
            .expect("on + present is a no-op");
        assert_eq!(
            pending_partial_count(&worker),
            0,
            "a file that already has a partial fingerprint is not re-enqueued",
        );
    }

    #[test]
    fn drain_backfill_recovers_files_missed_by_stale_off_flag() {
        use super::{
            ChangeKind, ChangeTask, FfmpegBinaries, FilesRepo, Fingerprint, FingerprintsRepo,
            IndexingHandler, NewFile, NormalizedPath, TaskHandler, encode_tier1, encode_tier2,
        };
        use vidcull_core::types::Codec;
        use vidcull_db::repo::{TaskQueueRepo, TaskState};
        use vidcull_fingerprint::format::FORMAT_VERSION;
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
        use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

        fn pending_partial_count(db: &vidcull_db::Database) -> usize {
            TaskQueueRepo::new(db.conn())
                .list_by_state(TaskState::Pending)
                .expect("pending tasks")
                .iter()
                .filter(|t| {
                    t.payload
                        .as_deref()
                        .and_then(|p| ChangeTask::from_payload(p).ok())
                        .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
                })
                .count()
        }

        let db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: NormalizedPath::new("/v/missed.mp4"),
                ..Default::default()
            })
            .expect("insert active file row");
        let t1 = Tier1Fingerprint {
            duration_ms: 60_000,
            codec: Codec::H264,
            gop: GopSignature::from_durations(&[]),
            global_phash: 0x1234,
        };
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: encode_tier1(&t1).expect("encode tier1"),
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row (no partial blob)");

        let mut handler = IndexingHandler::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            || 0_i64,
        );

        handler.set_partial_clips_live(false);
        handler.trailing_rebuild().expect("drain rebuild (off)");
        assert_eq!(
            pending_partial_count(&handler.worker.db),
            0,
            "live OFF: drain backfill enqueues nothing",
        );

        handler.set_partial_clips_live(true);
        handler.trailing_rebuild().expect("drain rebuild (on)");
        assert_eq!(
            pending_partial_count(&handler.worker.db),
            1,
            "live ON: drain backfill recovers the file missed by the stale OFF flag",
        );

        handler
            .trailing_rebuild()
            .expect("drain rebuild (on, re-call)");
        assert_eq!(
            pending_partial_count(&handler.worker.db),
            1,
            "an already-queued PartialFingerprint is deduped, not duplicated",
        );

        for task in TaskQueueRepo::new(handler.worker.db.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending")
        {
            TaskQueueRepo::new(handler.worker.db.conn())
                .mark_done(task.id, 0)
                .expect("mark done");
        }
        let partial = Tier2Fingerprint {
            scenes: vec![SceneHash {
                timestamp_ms: 0,
                phash: 0xABCD,
            }],
        };
        FingerprintsRepo::new(handler.worker.db.conn())
            .set_partial(
                file_id,
                &encode_tier2(&partial).expect("encode partial tier2"),
            )
            .expect("seed an existing partial fingerprint");
        handler
            .trailing_rebuild()
            .expect("drain rebuild (on, have_partial)");
        assert_eq!(
            pending_partial_count(&handler.worker.db),
            0,
            "have_partial converges: a file with a partial fingerprint is not re-enqueued",
        );
    }

    #[test]
    fn drain_backfill_loop_converges_once_a_skip_marker_is_stamped() {
        use super::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile, NormalizedPath};
        use crate::watcher::{ChangeKind, ChangeTask};
        use vidcull_db::repo::{PartialSkipMarker, TaskQueueRepo, TaskState};
        use vidcull_fingerprint::format::FORMAT_VERSION;

        fn pending_partial_count(db: &vidcull_db::Database) -> usize {
            TaskQueueRepo::new(db.conn())
                .list_by_state(TaskState::Pending)
                .expect("pending tasks")
                .iter()
                .filter(|t| {
                    t.payload
                        .as_deref()
                        .and_then(|p| ChangeTask::from_payload(p).ok())
                        .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
                })
                .count()
        }

        let mut db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: NormalizedPath::new("/v/broken-stss.mp4"),
                ..Default::default()
            })
            .expect("insert active file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![1, 2, 3],
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row (base index succeeded)");

        let n1 =
            crate::watcher::enqueue_partial_backfill(&mut db, "scan", 0).expect("backfill cycle 1");
        assert_eq!(
            n1, 1,
            "cycle 1: the FAILED-with-no-marker file is (re)enqueued"
        );
        assert_eq!(
            pending_partial_count(&db),
            1,
            "cycle 1 reproduces the defect: no failure memory ⇒ re-enqueued",
        );

        for task in TaskQueueRepo::new(db.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending")
        {
            TaskQueueRepo::new(db.conn())
                .mark_failed(task.id, 0, "container parse error: truncated stss box")
                .expect("mark failed (mirrors task_queue after handle_change Err(e))");
        }
        assert_eq!(
            pending_partial_count(&db),
            0,
            "FAILED task leaves PENDING queue"
        );

        FingerprintsRepo::new(db.conn())
            .set_partial_skip(
                file_id,
                &PartialSkipMarker {
                    reason: super::PARTIAL_SKIP_REASON_DECODE_FAILED.into(),
                    size_bytes: 0,
                    mtime_ns: 0,
                },
            )
            .expect("stamp skip marker");

        let n2 =
            crate::watcher::enqueue_partial_backfill(&mut db, "scan", 1).expect("backfill cycle 2");
        assert_eq!(n2, 0, "cycle 2 (GREEN): a marked file is never re-enqueued");
        assert_eq!(
            pending_partial_count(&db),
            0,
            "fix converges: the loop stops once the failure is remembered",
        );
    }

    #[test]
    fn decode_failed_marker_self_heals_when_file_is_replaced() {
        use super::{
            FilesRepo, Fingerprint, FingerprintsRepo, IndexingWorker, NewFile, NormalizedPath,
        };
        use vidcull_db::repo::PartialSkipMarker;

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("broken-stss.mp4");
        std::fs::write(&file_path, b"stub bytes").expect("write temp file");
        let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());

        let db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                size_bytes: 500_000,
                mtime_ns: 111,
                ..Default::default()
            })
            .expect("insert file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![1, 2, 3],
                tier2_temporal: None,
                format_version: u32::from(vidcull_fingerprint::format::FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint row");
        FingerprintsRepo::new(db.conn())
            .set_partial_skip(
                file_id,
                &PartialSkipMarker {
                    reason: super::PARTIAL_SKIP_REASON_DECODE_FAILED.into(),
                    size_bytes: 500_000,
                    mtime_ns: 111,
                },
            )
            .expect("stamp decode-failed marker");

        let worker = IndexingWorker::new(
            db,
            super::FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            super::DEFAULT_DECODE_BUDGET,
            super::DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            std::sync::Arc::new(super::FallbackMetrics::default()),
            std::sync::Arc::new(super::SingleFlight::default()),
        );

        let unchanged = FilesRepo::new(worker.db.conn())
            .find_by_path(&norm)
            .expect("find")
            .expect("row exists");
        assert!(
            worker
                .partial_skip_hazard(&unchanged, &norm)
                .expect("hazard check"),
            "an unchanged (size, mtime) identity keeps the decode-failed marker valid",
        );

        FilesRepo::new(worker.db.conn())
            .update_metadata(
                file_id,
                &NewFile {
                    path: norm.clone(),
                    size_bytes: 999_999,
                    mtime_ns: 222,
                    ..Default::default()
                },
            )
            .expect("update_metadata replace");
        let replaced = FilesRepo::new(worker.db.conn())
            .find_by_path(&norm)
            .expect("find")
            .expect("row exists");
        assert!(
            !worker
                .partial_skip_hazard(&replaced, &norm)
                .expect("hazard check"),
            "a replaced file's drifted identity invalidates the stale decode-failed marker",
        );
    }

    #[test]
    fn cadence_requires_both_file_and_time_thresholds() {
        use super::RebuildCadence;
        let mut c = RebuildCadence {
            min_files: 3,
            min_interval_secs: 10,
            files_since: 0,
            last_rebuild_at: 100,
        };
        assert!(!c.should_rebuild(105), "neither threshold met");
        c.record_indexed(3);
        assert!(
            !c.should_rebuild(105),
            "files met but time threshold not yet"
        );
        assert!(c.should_rebuild(110), "both thresholds met");

        c.reset(110);
        assert!(
            !c.should_rebuild(200),
            "files threshold not met after reset"
        );
        c.record_indexed(2);
        assert!(!c.should_rebuild(200), "still below min_files");
        c.record_indexed(1);
        assert!(
            c.should_rebuild(200),
            "min_files reached and interval elapsed"
        );
    }

    #[test]
    fn cadence_reset_clears_file_count_and_advances_clock() {
        use super::RebuildCadence;
        let mut c = RebuildCadence {
            min_files: 1,
            min_interval_secs: 0,
            files_since: 5,
            last_rebuild_at: 0,
        };
        assert!(
            c.should_rebuild(0),
            "5 files, zero interval → due immediately"
        );
        c.reset(50);
        assert_eq!(c.files_since, 0, "reset clears the file count");
        assert_eq!(c.last_rebuild_at, 50, "reset advances the interval clock");
        assert!(!c.should_rebuild(50), "no files indexed since reset");
    }

    #[test]
    fn panic_message_extracts_str_and_string_payloads() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(str_payload.as_ref()), "boom");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom"));
        assert_eq!(panic_message(string_payload.as_ref()), "kaboom");

        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(
            panic_message(other_payload.as_ref()),
            "unknown panic payload"
        );
    }

    #[test]
    fn partial_failure_skip_reason_content_failure_returns_reason() {
        use super::{PARTIAL_SKIP_REASON_DECODE_FAILED, partial_failure_skip_reason};
        let err = vidcull_core::Error::Decode("ffmpeg decode failed: invalid NAL unit".into());
        assert_eq!(
            partial_failure_skip_reason(&err),
            Some(PARTIAL_SKIP_REASON_DECODE_FAILED),
            "a genuine content-decode failure must be skip-marked",
        );
    }

    #[test]
    fn partial_failure_skip_reason_timeout_returns_none() {
        use super::partial_failure_skip_reason;
        let timeout_msg = format!(
            "ffmpeg/ffprobe {} {:.1} s — child killed and reaped",
            vidcull_parser::fallback::TIMEOUT_TOKEN,
            0.3_f64,
        );
        let err = vidcull_core::Error::Decode(timeout_msg);
        assert!(
            partial_failure_skip_reason(&err).is_none(),
            "a timeout-class Decode error must never be skip-marked (recall guard)",
        );
    }

    #[test]
    fn partial_failure_skip_reason_io_error_returns_none() {
        use super::partial_failure_skip_reason;
        let err = vidcull_core::Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "ffmpeg not found",
        ));
        assert!(
            partial_failure_skip_reason(&err).is_none(),
            "an I/O error must never be skip-marked",
        );
    }

    #[test]
    fn partial_failure_skip_reason_parse_and_busy_return_none() {
        use super::partial_failure_skip_reason;
        assert!(
            partial_failure_skip_reason(&vidcull_core::Error::Parse("bad ebml".into())).is_none(),
            "Parse error must not be skip-marked",
        );
        assert!(
            partial_failure_skip_reason(&vidcull_core::Error::Busy("gate at capacity".into()))
                .is_none(),
            "Busy must not be skip-marked",
        );
    }

    #[test]
    fn partial_failure_skip_reason_after_retry_covers_post_retry_parse_only() {
        use super::{PARTIAL_SKIP_REASON_DECODE_FAILED, partial_failure_skip_reason_after_retry};

        assert!(
            partial_failure_skip_reason_after_retry(
                &vidcull_core::Error::Parse("truncated stss box".into()),
                false,
            )
            .is_none(),
            "a Parse error must not be skip-marked before a retry has been attempted",
        );

        assert_eq!(
            partial_failure_skip_reason_after_retry(
                &vidcull_core::Error::Parse("truncated stss box".into()),
                true,
            ),
            Some(PARTIAL_SKIP_REASON_DECODE_FAILED),
            "a Parse error that survives the pure-ffmpeg retry must converge to a skip marker",
        );

        assert!(
            partial_failure_skip_reason_after_retry(
                &vidcull_core::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "ffmpeg not found",
                )),
                true,
            )
            .is_none(),
            "an I/O error must never be skip-marked, even after a retry attempt",
        );
        assert!(
            partial_failure_skip_reason_after_retry(
                &vidcull_core::Error::Busy("gate at capacity".into()),
                true,
            )
            .is_none(),
            "Busy must never be skip-marked, even after a retry attempt",
        );

        assert_eq!(
            partial_failure_skip_reason_after_retry(
                &vidcull_core::Error::Decode("ffmpeg decode failed: invalid NAL unit".into()),
                false,
            ),
            Some(PARTIAL_SKIP_REASON_DECODE_FAILED),
            "a Decode content failure skip-marks regardless of retried",
        );
    }

    #[test]
    fn partial_failure_skip_reason_tokened_unsupported_is_skip_marked() {
        use super::{
            PARTIAL_NON_FAST_PATH_TOKEN, PARTIAL_SKIP_REASON_UNSUPPORTED_CODEC,
            partial_failure_skip_reason,
        };
        let err = vidcull_core::Error::Unsupported(format!(
            "{PARTIAL_NON_FAST_PATH_TOKEN}: codec Av1 is not fast-path eligible \
             (no native IDR path)"
        ));
        assert_eq!(
            partial_failure_skip_reason(&err),
            Some(PARTIAL_SKIP_REASON_UNSUPPORTED_CODEC),
            "a confirmed (tokened) non-fast-path codec must be skip-marked, never requeued",
        );
    }

    #[test]
    fn partial_failure_skip_reason_untokened_unsupported_returns_none() {
        use super::partial_failure_skip_reason;
        assert!(
            partial_failure_skip_reason(&vidcull_core::Error::Unsupported(
                "native HEVC: tiles not supported".into()
            ))
            .is_none(),
            "a mid-stream native feature gap must retry, not skip-mark a fast-path file",
        );
        assert!(
            partial_failure_skip_reason(&vidcull_core::Error::Unsupported(
                "ffprobe binary not available".into()
            ))
            .is_none(),
            "a missing-binary Unsupported is environmental and must retry, not skip-mark",
        );
    }

    #[test]
    fn partial_failure_nulls_blob_only_for_tokened_non_fast_path() {
        use super::{PARTIAL_NON_FAST_PATH_TOKEN, partial_failure_nulls_blob};
        assert!(
            partial_failure_nulls_blob(&vidcull_core::Error::Unsupported(format!(
                "{PARTIAL_NON_FAST_PATH_TOKEN}: codec Av1 is not fast-path eligible"
            ))),
            "a confirmed (tokened) non-fast-path codec must null the legacy residual blob",
        );
        assert!(
            !partial_failure_nulls_blob(&vidcull_core::Error::Unsupported(
                "native HEVC: tiles not supported".into()
            )),
            "an untokened Unsupported (feature gap / missing binary) must keep its blob",
        );
        assert!(
            !partial_failure_nulls_blob(&vidcull_core::Error::Decode("invalid NAL".into())),
            "a fast-path content-decode failure must keep its blob",
        );
        assert!(
            !partial_failure_nulls_blob(&vidcull_core::Error::Parse("bad ebml".into())),
            "a Parse error must keep its blob",
        );
        assert!(
            !partial_failure_nulls_blob(&vidcull_core::Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "missing",
            ))),
            "an I/O error must keep its blob (retry)",
        );
    }

    #[test]
    fn classify_reindex_failure_timeout_is_transient_suppressed() {
        use super::{ReindexFailureClass, classify_reindex_failure};
        let last_error = format!(
            "decode error: ffmpeg/ffprobe {} {:.1} s — child killed and reaped",
            vidcull_parser::fallback::TIMEOUT_TOKEN,
            90.0_f64,
        );
        assert_eq!(
            classify_reindex_failure(&last_error, 3),
            ReindexFailureClass::TransientSuppressed,
            "a timeout reindex failure must stay suppressed, \
             never surfaced as permanent",
        );
    }

    #[test]
    fn classify_reindex_failure_corrupt_content_is_permanent_surface() {
        use super::{ReindexFailureClass, classify_reindex_failure};
        let last_error = "decode error: ffmpeg decode failed: invalid NAL unit (corrupt)";
        assert_eq!(
            classify_reindex_failure(last_error, 1),
            ReindexFailureClass::PermanentSurface,
            "a genuine non-timeout decode failure on stable content must be surfaced",
        );
    }

    #[test]
    fn classify_reindex_failure_lock_and_busy_are_transient() {
        use super::{
            BASE_DECODE_GATE_BUSY_REASON, PARTIAL_GATE_BUSY_REASON, ReindexFailureClass,
            classify_reindex_failure,
        };
        for last_error in [
            "resource busy or locked: file is being written".to_owned(),
            "database error: database is locked".to_owned(),
            format!("decode error: {BASE_DECODE_GATE_BUSY_REASON}"),
            format!("decode error: {PARTIAL_GATE_BUSY_REASON}"),
        ] {
            assert_eq!(
                classify_reindex_failure(&last_error, 5),
                ReindexFailureClass::TransientSuppressed,
                "a lock/Busy reindex failure must stay suppressed: {last_error}",
            );
        }
    }

    #[test]
    fn classify_reindex_failure_non_token_residual_defaults_permanent() {
        use super::{QUARANTINE_ATTEMPT_THRESHOLD, ReindexFailureClass, classify_reindex_failure};
        for last_error in [
            "container parse error: bad ebml header",
            "I/O error: permission denied",
            "unsupported: vp9 container variant",
            "",
            "task failed",
        ] {
            assert_eq!(
                classify_reindex_failure(last_error, QUARANTINE_ATTEMPT_THRESHOLD),
                ReindexFailureClass::PermanentSurface,
                "an ambiguous non-token residual must default to permanent/surface: {last_error:?}",
            );
        }
    }

    #[test]
    fn classify_reindex_failure_below_threshold_is_transient() {
        use super::{ReindexFailureClass, classify_reindex_failure};
        assert_eq!(
            classify_reindex_failure("decode error: invalid NAL", 0),
            ReindexFailureClass::TransientSuppressed,
            "below the stability threshold a non-token failure is not yet permanent",
        );
    }

    fn parser_fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vidcull-parser")
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn build_partial_fingerprint_native_path_is_deterministic() {
        use super::{DecodeConcurrency, FfmpegBinaries, build_partial_fingerprint};
        use vidcull_parser::probe_and_decode_sparse_budgets_streaming;

        let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
        let budget = 16usize;

        for fixture in ["black_320x180_30fps_1s.mp4", "black_320x180_30fps_1s.mkv"] {
            let path = parser_fixture(fixture);
            if !path.exists() {
                eprintln!("SKIP build_partial_fingerprint_native_path: {fixture} missing");
                return;
            }

            let serial = DecodeConcurrency::serial();
            let route = probe_and_decode_sparse_budgets_streaming(
                &bins,
                &path,
                budget,
                budget,
                &serial,
                Cancel::default(),
                |_f| Ok(()),
            );
            let Ok((_meta, decode_path)) = route else {
                eprintln!("SKIP build_partial_fingerprint_native_path: ffmpeg/ffprobe unavailable");
                return;
            };
            assert_eq!(
                decode_path,
                DecodePath::Native,
                "{fixture}: partial decode must take the native path",
            );

            let conc1 = DecodeConcurrency::new(1);
            let conc4 = DecodeConcurrency::new(4);
            let blob1 = build_partial_fingerprint(&bins, &path, budget, &conc1, Cancel::default())
                .expect("build conc=1 must not error");
            let blob4 = build_partial_fingerprint(&bins, &path, budget, &conc4, Cancel::default())
                .expect("build conc=4 must not error");
            assert_eq!(
                blob1, blob4,
                "{fixture}: native+trim fold must be concurrency-invariant (§J)",
            );
            assert!(
                matches!(blob1, super::PartialBuildOutcome::Built(_)),
                "{fixture}: a content fixture must yield a partial fingerprint",
            );
        }
    }

    #[test]
    fn retry_partial_via_pure_ffmpeg_fallback_matches_manual_fallback_fold() {
        use super::{
            DecodeConcurrency, FfmpegBinaries, Tier2Builder, encode_tier2,
            retry_partial_via_pure_ffmpeg_fallback,
        };
        use vidcull_fingerprint::{GrayFrame, TimedFrame, trim_uniform_borders};
        use vidcull_parser::fallback::{
            decode_sparse_strided_with_streaming, probe_fallback_cancellable,
        };

        let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
        let budget = 16usize;
        let conc = DecodeConcurrency::serial();
        let path = parser_fixture("black_320x180_30fps_1s.mp4");
        if !path.exists() {
            eprintln!("SKIP retry_partial_via_pure_ffmpeg_fallback: fixture missing");
            return;
        }

        let retried =
            retry_partial_via_pure_ffmpeg_fallback(&bins, &path, budget, &conc, Cancel::default());
        let Ok(super::PartialBuildOutcome::Built(retried_blob)) = retried else {
            eprintln!("SKIP retry_partial_via_pure_ffmpeg_fallback: ffmpeg/ffprobe unavailable");
            return;
        };

        let Ok(meta) = probe_fallback_cancellable(&bins, &path, Cancel::default()) else {
            eprintln!(
                "SKIP retry_partial_via_pure_ffmpeg_fallback: ffprobe unavailable for reference"
            );
            return;
        };
        let duration_ms = meta.duration.unwrap().as_millis();
        let mut builder = Tier2Builder::new();
        decode_sparse_strided_with_streaming(
            &bins,
            &path,
            duration_ms,
            meta.resolution.width,
            meta.resolution.height,
            budget,
            &meta.codec,
            meta.fps_x1000,
            meta.has_b_frames,
            &conc,
            Cancel::default(),
            |frame| {
                let (w, h, px) = trim_uniform_borders(
                    frame.width,
                    frame.height,
                    &frame.pixels,
                    super::DEFAULT_BAR_LIMIT,
                );
                builder.push(&TimedFrame {
                    timestamp_ms: frame.timestamp_ms,
                    frame: GrayFrame {
                        width: w,
                        height: h,
                        pixels: &px,
                    },
                });
                Ok(())
            },
        )
        .expect("reference direct-fallback decode must succeed");
        let reference_blob =
            encode_tier2(&builder.finish()).expect("reference encode_tier2 must succeed");

        assert_eq!(
            retried_blob, reference_blob,
            "§J: retry-path partial fingerprint must be byte-identical to a manual \
             pure-fallback fold using the same trim + Tier2Builder",
        );
    }

    #[test]
    fn base_index_decode_honours_cancel_and_false_flag_is_byte_identical() {
        use super::{DecodeConcurrency, FfmpegBinaries, probe_decode_fingerprint_streaming};
        use std::sync::atomic::AtomicBool;

        let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
        let budget = 16usize;
        let conc = DecodeConcurrency::serial();
        let path = parser_fixture("black_320x180_30fps_1s.mp4");
        if !path.exists() {
            eprintln!("SKIP base_index_decode_honours_cancel: fixture missing");
            return;
        }

        let baseline = probe_decode_fingerprint_streaming(
            &bins,
            &path,
            budget,
            budget,
            &conc,
            Cancel::default(),
        );
        let Ok((_, decode_path, fc_none, art_none)) = baseline else {
            eprintln!("SKIP base_index_decode_honours_cancel: ffmpeg/ffprobe unavailable");
            return;
        };
        assert_eq!(
            decode_path,
            DecodePath::Native,
            "fixture must take the native path for the cancel/§J claims",
        );

        let removed = AtomicBool::new(true);
        let cancelled = probe_decode_fingerprint_streaming(
            &bins,
            &path,
            budget,
            budget,
            &conc,
            Cancel {
                pause: None,
                removal: Some(&removed),
            },
        )
        .map(|(_, dp, fc, _)| (dp, fc));
        assert!(
            matches!(cancelled, Err(Error::Cancelled)),
            "pre-set removal token must abort the base-index decode with Cancelled, got {cancelled:?}",
        );

        let paused = AtomicBool::new(true);
        let cancelled_pause = probe_decode_fingerprint_streaming(
            &bins,
            &path,
            budget,
            budget,
            &conc,
            Cancel {
                pause: Some(&paused),
                removal: None,
            },
        )
        .map(|(_, dp, fc, _)| (dp, fc));
        assert!(
            matches!(cancelled_pause, Err(Error::Cancelled)),
            "pre-set pause flag must abort the base-index decode with Cancelled, got {cancelled_pause:?}",
        );

        let flag_false = AtomicBool::new(false);
        let (_, _, fc_false, art_false) = probe_decode_fingerprint_streaming(
            &bins,
            &path,
            budget,
            budget,
            &conc,
            Cancel {
                pause: Some(&flag_false),
                removal: None,
            },
        )
        .expect("never-set flag must decode identically to the default");
        assert_eq!(fc_false, fc_none, "frame count must match default (§J)");
        assert_eq!(
            art_false.tier1_blob, art_none.tier1_blob,
            "tier1 blob must match None (§J)",
        );
        assert_eq!(
            art_false.tier2_blob, art_none.tier2_blob,
            "tier2 blob must match None (§J)",
        );
    }

    #[test]
    fn index_file_cancelled_writes_no_fingerprint() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics, FfmpegBinaries,
            FilesRepo, IndexingWorker, NormalizedPath, SingleFlight,
        };
        use crate::throttle::ThrottleControl;
        use crate::watcher::{ChangeKind, ChangeTask};
        use std::sync::Arc;

        let fixture = parser_fixture("black_320x180_30fps_1s.mp4");
        if !fixture.exists() {
            eprintln!("SKIP index_file_cancelled_writes_no_fingerprint: fixture missing");
            return;
        }
        let norm = NormalizedPath::new(&fixture);
        if !norm.to_native_path().exists() {
            eprintln!("SKIP index_file_cancelled_writes_no_fingerprint: path round-trip failed");
            return;
        }

        let db = vidcull_db::open_in_memory().expect("open db");
        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );
        let ctrl = Arc::new(ThrottleControl::default());
        ctrl.set_indexing_enabled(false);
        worker.set_cancel_source(Arc::clone(&ctrl));

        let task_id = vidcull_db::repo::TaskQueueRepo::new(worker.db.conn())
            .enqueue(&vidcull_db::repo::NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: None,
                enqueued_at: 0,
                size_bytes: 0,
            })
            .expect("enqueue task");
        let change = ChangeTask {
            path: norm.clone(),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        };
        match worker.handle_change(&change, task_id) {
            Err(Error::Cancelled) => {}
            Err(other) => {
                eprintln!("SKIP index_file_cancelled_writes_no_fingerprint: {other}");
                return;
            }
            Ok(()) => panic!("a paused Upsert must not complete the index"),
        }
        assert!(
            FilesRepo::new(worker.db.conn())
                .find_by_path(&norm)
                .expect("find_by_path")
                .is_none(),
            "a cancelled base-index decode must not write a files row (or fingerprint)",
        );
    }

    #[test]
    fn densify_cancelled_preserves_existing_fingerprint() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics, FfmpegBinaries,
            FilesRepo, Fingerprint, FingerprintsRepo, IndexingWorker, NewFile, NormalizedPath,
            SingleFlight,
        };
        use crate::throttle::ThrottleControl;
        use crate::watcher::{ChangeKind, ChangeTask};
        use std::sync::Arc;
        use vidcull_fingerprint::format::FORMAT_VERSION;

        let fixture = parser_fixture("black_320x180_30fps_1s.mp4");
        if !fixture.exists() {
            eprintln!("SKIP densify_cancelled_preserves_existing_fingerprint: fixture missing");
            return;
        }
        let norm = NormalizedPath::new(&fixture);
        if !norm.to_native_path().exists() {
            eprintln!("SKIP densify_cancelled_preserves_existing_fingerprint: path round-trip");
            return;
        }

        let db = vidcull_db::open_in_memory().expect("open db");
        let file_id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: norm.clone(),
                size_bytes: 1_000,
                ..Default::default()
            })
            .expect("insert file row");
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id,
                tier1_global: vec![9, 9, 9],
                tier2_temporal: Some(vec![7, 7, 7]),
                format_version: u32::from(FORMAT_VERSION),
                created_at: 0,
            })
            .expect("seed fingerprint");

        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );
        let ctrl = Arc::new(ThrottleControl::default());
        ctrl.set_indexing_enabled(false);
        worker.set_cancel_source(Arc::clone(&ctrl));

        let change = ChangeTask {
            path: norm.clone(),
            change: ChangeKind::Densify,
            size_bytes: 0,
        };
        match worker.handle_change(&change, 0) {
            Err(Error::Cancelled) => {}
            Err(other) => {
                eprintln!("SKIP densify_cancelled_preserves_existing_fingerprint: {other}");
                return;
            }
            Ok(()) => panic!("a paused densify must not complete"),
        }
        let fp = FingerprintsRepo::new(worker.db.conn())
            .get(file_id)
            .expect("get fingerprint")
            .expect("fingerprint still present");
        assert_eq!(
            fp.tier1_global,
            vec![9, 9, 9],
            "tier1 must be the seeded blob (untouched)"
        );
        assert_eq!(
            fp.tier2_temporal,
            Some(vec![7, 7, 7]),
            "tier2 must be the seeded blob (untouched)",
        );
    }

    #[test]
    fn index_file_liveness_gate_rolls_back_when_task_deleted() {
        use super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, FallbackMetrics, FfmpegBinaries,
            FilesRepo, IndexingWorker, NormalizedPath, SingleFlight,
        };
        use crate::watcher::{ChangeKind, ChangeTask};
        use std::sync::Arc;

        let fixture = parser_fixture("black_320x180_30fps_1s.mp4");
        if !fixture.exists() {
            eprintln!("SKIP index_file_liveness_gate: fixture missing");
            return;
        }
        let norm = NormalizedPath::new(&fixture);
        if !norm.to_native_path().exists() {
            eprintln!("SKIP index_file_liveness_gate: path round-trip failed");
            return;
        }

        let db = vidcull_db::open_in_memory().expect("open db");
        let mut worker = IndexingWorker::new(
            db,
            FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
            DEFAULT_DECODE_BUDGET,
            DEFAULT_FALLBACK_DECODE_BUDGET,
            "scan".to_owned(),
            || 0_i64,
            Arc::new(FallbackMetrics::default()),
            Arc::new(SingleFlight::default()),
        );
        let change = ChangeTask {
            path: norm.clone(),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        };
        match worker.handle_change(&change, 999) {
            Err(Error::Cancelled) => {}
            Err(other) => {
                eprintln!("SKIP index_file_liveness_gate: {other}");
                return;
            }
            Ok(()) => panic!("a deleted task must roll the base store back, not complete"),
        }
        assert!(
            FilesRepo::new(worker.db.conn())
                .find_by_path(&norm)
                .expect("find_by_path")
                .is_none(),
            "the liveness gate must prevent a phantom active files row",
        );
    }

    #[cfg(test)]
    mod purge_ungroups_removed_file {
        use super::super::{
            DuplicateGroupsRepo, FfmpegBinaries, FilesRepo, Fingerprint, FingerprintsRepo,
            IndexingHandler, NewFile, TrustLevel,
        };
        use vidcull_core::types::{Blake3Hash, FileId, NormalizedPath};
        use vidcull_db::Database;

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn seed_hashed(db: &Database, path: &str, tag: u8) -> FileId {
            let id = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    size_bytes: 1_000,
                    content_hash: Some(Blake3Hash::from_bytes([tag; 32])),
                    ..Default::default()
                })
                .expect("insert file row");
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: vec![tag, tag, tag],
                    tier2_temporal: None,
                    format_version: 1,
                    created_at: T0,
                })
                .expect("upsert fingerprint");
            id
        }

        fn handler_over(db: Database) -> IndexingHandler {
            IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
        }

        fn assert_cache_preserved(db: &Database, id: FileId, tag: u8) {
            let file = FilesRepo::new(db.conn())
                .get(id)
                .expect("get file")
                .expect("file row survives soft-delete");
            assert_eq!(
                file.content_hash,
                Some(Blake3Hash::from_bytes([tag; 32])),
                "content_hash must be preserved for re-add efficiency",
            );
            assert!(
                FingerprintsRepo::new(db.conn())
                    .get(id)
                    .expect("get fingerprint")
                    .is_some(),
                "fingerprint row must be preserved",
            );
        }

        #[test]
        fn purging_one_of_two_members_deletes_the_now_undersized_group() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            let f1 = seed_hashed(&db, "/lib/a.mp4", 0xa1);
            let f2 = seed_hashed(&db, "/lib/b.mp4", 0xb2);
            let gid = {
                let groups = DuplicateGroupsRepo::new(db.conn());
                let gid = groups.create(TrustLevel::Exact, T0).expect("create group");
                groups.add_member(gid, f1).expect("add f1");
                groups.add_member(gid, f2).expect("add f2");
                gid
            };

            let mut handler = handler_over(db);
            handler
                .worker
                .purge_file(&NormalizedPath::new("/lib/a.mp4"))
                .expect("purge removed file");

            let db = &handler.worker.db;
            let groups = DuplicateGroupsRepo::new(db.conn());
            assert!(
                groups.get(gid).expect("get group").is_none(),
                "the EXACT group drops once it falls below two members",
            );
            assert!(
                groups
                    .find_groups_containing(f2)
                    .expect("groups of survivor")
                    .is_empty(),
                "the surviving member is no longer attached to the dropped group",
            );
            assert_cache_preserved(db, f1, 0xa1);
            assert_cache_preserved(db, f2, 0xb2);
        }

        #[test]
        fn purging_one_of_three_members_keeps_the_still_valid_group() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            let f1 = seed_hashed(&db, "/lib/a.mp4", 0xa1);
            let f2 = seed_hashed(&db, "/lib/b.mp4", 0xb2);
            let f3 = seed_hashed(&db, "/lib/c.mp4", 0xc3);
            let gid = {
                let groups = DuplicateGroupsRepo::new(db.conn());
                let gid = groups.create(TrustLevel::Exact, T0).expect("create group");
                groups.add_member(gid, f1).expect("add f1");
                groups.add_member(gid, f2).expect("add f2");
                groups.add_member(gid, f3).expect("add f3");
                gid
            };

            let mut handler = handler_over(db);
            handler
                .worker
                .purge_file(&NormalizedPath::new("/lib/a.mp4"))
                .expect("purge removed file");

            let db = &handler.worker.db;
            let groups = DuplicateGroupsRepo::new(db.conn());
            assert_eq!(
                groups.list_members(gid).expect("members"),
                vec![f2, f3],
                "only the removed file is unlinked; the 2-member group survives",
            );
            assert_cache_preserved(db, f1, 0xa1);
        }
    }

    #[cfg(test)]
    mod force_rescan_eager_teardown {
        use super::super::{
            DuplicateGroupsRepo, FfmpegBinaries, FilesRepo, IndexingHandler, NewFile,
            RegroupQueueRepo, TrustLevel,
        };
        use crate::bridge::force_rescan_teardown;
        use crate::watcher::{ChangeKind, ChangeTask};
        use vidcull_core::types::{Blake3Hash, NormalizedPath};

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn exact_groups(db: &vidcull_db::Database) -> Vec<Vec<i64>> {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let mut out: Vec<Vec<i64>> = repo
                .list_all()
                .expect("list groups")
                .into_iter()
                .filter(|g| g.trust_level == TrustLevel::Exact)
                .map(|g| {
                    let mut members: Vec<i64> = repo
                        .list_members(g.id)
                        .expect("members")
                        .into_iter()
                        .map(|f| f.0)
                        .collect();
                    members.sort_unstable();
                    members
                })
                .collect();
            out.sort();
            out
        }

        #[test]
        fn teardown_blocks_resurrection_then_reverify_reforms() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let (a, b) = {
                let files = FilesRepo::new(db.conn());
                let a = files
                    .insert(&NewFile {
                        path: NormalizedPath::new("/v/fr_a.mp4"),
                        size_bytes: 7,
                        content_hash: Some(Blake3Hash::from_bytes([0xCC; 32])),
                        ..Default::default()
                    })
                    .expect("insert a");
                let b = files
                    .insert(&NewFile {
                        path: NormalizedPath::new("/v/fr_b.mp4"),
                        size_bytes: 7,
                        content_hash: Some(Blake3Hash::from_bytes([0xCC; 32])),
                        ..Default::default()
                    })
                    .expect("insert b");
                let groups = DuplicateGroupsRepo::new(db.conn());
                let gid = groups.create(TrustLevel::Exact, T0).expect("create group");
                groups.add_member(gid, a).expect("add a");
                groups.add_member(gid, b).expect("add b");
                (a, b)
            };

            let changes: Vec<ChangeTask> = ["/v/fr_a.mp4", "/v/fr_b.mp4"]
                .iter()
                .map(|p| ChangeTask {
                    path: NormalizedPath::new(p),
                    change: ChangeKind::ForceUpsert,
                    size_bytes: 7,
                })
                .collect();
            let mut db = db;
            force_rescan_teardown(&mut db, &changes).expect("teardown");
            assert!(
                exact_groups(&db).is_empty(),
                "teardown drops the group immediately",
            );

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(false);
            handler.rebuild_matches().expect("rebuild before re-verify");
            assert!(
                exact_groups(&handler.worker.db).is_empty(),
                "a rebuild before re-verification must not resurrect the \
                 group from stale artefacts",
            );

            {
                let files = FilesRepo::new(handler.worker.db.conn());
                files
                    .set_content_hash(a, Blake3Hash::from_bytes([0xDD; 32]))
                    .expect("rehash a");
                files
                    .set_content_hash(b, Blake3Hash::from_bytes([0xDD; 32]))
                    .expect("rehash b");
                let regroup = RegroupQueueRepo::new(handler.worker.db.conn());
                regroup.mark(a, T0).expect("mark a");
                regroup.mark(b, T0).expect("mark b");
            }
            handler.rebuild_matches().expect("rebuild after re-verify");
            assert_eq!(
                exact_groups(&handler.worker.db),
                vec![vec![a.0, b.0]],
                "re-verified evidence must re-form the group (progressive \
                 repopulate, )",
            );
        }
    }

    #[cfg(test)]
    mod force_rescan_ungroups_content_change {
        use super::super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, DuplicateGroupsRepo,
            FallbackMetrics, FfmpegBinaries, FilesRepo, IndexingWorker, NewFile, NormalizedPath,
            SingleFlight, TrustLevel,
        };
        use crate::watcher::{ChangeKind, ChangeTask};
        use std::sync::Arc;
        use vidcull_core::types::{Blake3Hash, FileId};
        use vidcull_db::repo::{NewTask, TaskQueueRepo};

        fn fresh_worker(db: vidcull_db::Database) -> IndexingWorker {
            IndexingWorker::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                DEFAULT_DECODE_BUDGET,
                DEFAULT_FALLBACK_DECODE_BUDGET,
                "scan".to_owned(),
                || 0_i64,
                Arc::new(FallbackMetrics::default()),
                Arc::new(SingleFlight::default()),
            )
        }

        fn live_task(worker: &IndexingWorker) -> i64 {
            TaskQueueRepo::new(worker.db.conn())
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: 0,
                    payload: None,
                    enqueued_at: 0,
                    size_bytes: 0,
                })
                .expect("enqueue task")
        }

        fn try_index(
            worker: &mut IndexingWorker,
            norm: &NormalizedPath,
            force: bool,
            task: i64,
        ) -> bool {
            let kind = if force {
                ChangeKind::ForceUpsert
            } else {
                ChangeKind::Upsert
            };
            let change = ChangeTask {
                path: norm.clone(),
                change: kind,
                size_bytes: 0,
            };
            match worker.handle_change(&change, task) {
                Ok(()) => true,
                Err(other) => {
                    eprintln!("SKIP force_rescan_ungroups_content_change: {other}");
                    false
                }
            }
        }

        fn seed_exact_group(
            db: &vidcull_db::Database,
            primary: FileId,
            hash: Option<Blake3Hash>,
        ) -> (i64, FileId) {
            let sibling = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new("/lib/exact-twin.mp4"),
                    size_bytes: 1_000,
                    content_hash: hash,
                    ..Default::default()
                })
                .expect("insert sibling row");
            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups.create(TrustLevel::Exact, 0).expect("create group");
            groups.add_member(gid, primary).expect("add primary");
            groups.add_member(gid, sibling).expect("add sibling");
            (gid, sibling)
        }

        #[test]
        fn content_change_unlinks_member_and_drops_undersized_group() {
            let dir = tempfile::tempdir().expect("tempdir");
            let p = dir.path().join("clip.mp4");
            let fixture_a = super::parser_fixture("black_320x180_30fps_1s.mp4");
            let fixture_b = super::parser_fixture("h264-native-e2e/testsrc2_160_90.mp4");
            if !fixture_a.exists() || !fixture_b.exists() {
                eprintln!("SKIP content_change_unlinks_member: fixtures missing");
                return;
            }
            std::fs::copy(&fixture_a, &p).expect("seed clip with content A");
            let norm = NormalizedPath::new(&p);
            if !norm.to_native_path().exists() {
                eprintln!("SKIP content_change_unlinks_member: path round-trip failed");
                return;
            }

            let db = vidcull_db::open_in_memory().expect("open db");
            let mut worker = fresh_worker(db);
            let task = live_task(&worker);
            if !try_index(&mut worker, &norm, false, task) {
                return;
            }

            let file = FilesRepo::new(worker.db.conn())
                .find_by_path(&norm)
                .expect("find_by_path")
                .expect("indexed file row");
            let (gid, _sibling) = seed_exact_group(&worker.db, file.id, file.content_hash);

            std::fs::copy(&fixture_b, &p).expect("overwrite clip with content B");
            if !try_index(&mut worker, &norm, true, task) {
                return;
            }

            let groups = DuplicateGroupsRepo::new(worker.db.conn());
            assert!(
                groups
                    .find_groups_containing(file.id)
                    .expect("groups of changed file")
                    .is_empty(),
                "a force re-scan whose content changed must un-group the file",
            );
            assert!(
                groups.get(gid).expect("get group").is_none(),
                "the stale EXACT group drops once it falls below two members",
            );
        }

        #[test]
        fn unchanged_force_reindex_leaves_the_group_intact() {
            let dir = tempfile::tempdir().expect("tempdir");
            let p = dir.path().join("clip.mp4");
            let fixture_a = super::parser_fixture("black_320x180_30fps_1s.mp4");
            if !fixture_a.exists() {
                eprintln!("SKIP unchanged_force_reindex: fixture missing");
                return;
            }
            std::fs::copy(&fixture_a, &p).expect("seed clip");
            let norm = NormalizedPath::new(&p);
            if !norm.to_native_path().exists() {
                eprintln!("SKIP unchanged_force_reindex: path round-trip failed");
                return;
            }

            let db = vidcull_db::open_in_memory().expect("open db");
            let mut worker = fresh_worker(db);
            let task = live_task(&worker);
            if !try_index(&mut worker, &norm, false, task) {
                return;
            }

            let file = FilesRepo::new(worker.db.conn())
                .find_by_path(&norm)
                .expect("find_by_path")
                .expect("indexed file row");
            let (gid, sibling) = seed_exact_group(&worker.db, file.id, file.content_hash);

            if !try_index(&mut worker, &norm, true, task) {
                return;
            }

            let groups = DuplicateGroupsRepo::new(worker.db.conn());
            assert!(
                groups.get(gid).expect("get group").is_some(),
                "the group must survive an unchanged force re-index",
            );
            assert_eq!(
                groups.list_members(gid).expect("members"),
                vec![file.id, sibling],
                "an unchanged force re-index must not disturb group membership",
            );
        }
    }

    #[cfg(test)]
    mod force_rescan_purges_deleted_member {
        use super::super::{
            DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, DuplicateGroupsRepo,
            FallbackMetrics, FfmpegBinaries, FilesRepo, IndexingWorker, NewFile, NormalizedPath,
            SingleFlight, TrustLevel,
        };
        use crate::bridge::reconcile_deleted_under_root;
        use crate::watcher::ChangeKind;
        use std::collections::HashSet;
        use std::sync::Arc;
        use vidcull_core::types::{Blake3Hash, FileId};

        fn fresh_worker(db: vidcull_db::Database) -> IndexingWorker {
            IndexingWorker::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                DEFAULT_DECODE_BUDGET,
                DEFAULT_FALLBACK_DECODE_BUDGET,
                "scan".to_owned(),
                || 0_i64,
                Arc::new(FallbackMetrics::default()),
                Arc::new(SingleFlight::default()),
            )
        }

        fn seed_file(db: &vidcull_db::Database, path: &NormalizedPath, tag: u8) -> FileId {
            FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: path.clone(),
                    size_bytes: 1_000,
                    content_hash: Some(Blake3Hash::from_bytes([tag; 32])),
                    ..Default::default()
                })
                .expect("insert file row")
        }

        fn seed_exact_group_ab(db: &vidcull_db::Database, a: FileId, b: FileId) -> i64 {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups.create(TrustLevel::Exact, 0).expect("create group");
            groups.add_member(gid, a).expect("add member a");
            groups.add_member(gid, b).expect("add member b");
            gid
        }

        #[test]
        fn disk_deleted_member_removed_from_exact_group() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path_a = dir.path().join("a.mp4");
            let path_b = dir.path().join("b.mp4");
            std::fs::write(&path_a, b"dummy-a").expect("write a");
            std::fs::write(&path_b, b"dummy-b").expect("write b");
            let norm_a = NormalizedPath::new(&path_a);
            let norm_b = NormalizedPath::new(&path_b);
            let norm_root = NormalizedPath::new(dir.path());

            let db = vidcull_db::open_in_memory().expect("open db");
            let mut worker = fresh_worker(db);
            let id_a = seed_file(&worker.db, &norm_a, 0xAA);
            let id_b = seed_file(&worker.db, &norm_b, 0xBB);
            let gid = seed_exact_group_ab(&worker.db, id_a, id_b);

            std::fs::remove_file(&path_b).expect("remove b");

            let on_disk: HashSet<String> = [norm_a.as_str().to_owned()].into_iter().collect();
            let removes =
                reconcile_deleted_under_root(&worker.db, &norm_root, &on_disk).expect("reconcile");

            assert_eq!(removes.len(), 1, "reconcile must detect one missing file");
            assert_eq!(
                removes[0].path.as_str(),
                norm_b.as_str(),
                "the missing path must be B",
            );
            assert_eq!(removes[0].change, ChangeKind::Remove, "task must be Remove");

            worker.handle_change(&removes[0], 0).expect("handle Remove");

            let groups = DuplicateGroupsRepo::new(worker.db.conn());
            assert!(
                groups
                    .find_groups_containing(id_b)
                    .expect("groups of b")
                    .is_empty(),
                "purge must un-group B",
            );
            assert!(
                groups.get(gid).expect("get group").is_none(),
                "stale EXACT group must be deleted when it falls below two members",
            );
        }

        #[test]
        fn transient_inaccessible_does_not_purge() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path_a = dir.path().join("a.mp4");
            let path_b = dir.path().join("b.mp4");
            std::fs::write(&path_a, b"dummy-a").expect("write a");
            std::fs::write(&path_b, b"dummy-b").expect("write b");
            let norm_a = NormalizedPath::new(&path_a);
            let norm_b = NormalizedPath::new(&path_b);
            let norm_root = NormalizedPath::new(dir.path());

            let db = vidcull_db::open_in_memory().expect("open db");
            let worker = fresh_worker(db);
            let id_a = seed_file(&worker.db, &norm_a, 0xAA);
            let id_b = seed_file(&worker.db, &norm_b, 0xBB);
            let gid = seed_exact_group_ab(&worker.db, id_a, id_b);

            let on_disk: HashSet<String> = HashSet::new();
            let removes =
                reconcile_deleted_under_root(&worker.db, &norm_root, &on_disk).expect("reconcile");

            assert!(
                removes.is_empty(),
                "files that still exist on disk must not be purged (non-ENOENT guard)",
            );

            let groups = DuplicateGroupsRepo::new(worker.db.conn());
            assert!(
                groups.get(gid).expect("get group").is_some(),
                "group must survive when no files are confirmed gone from disk",
            );
            assert_eq!(
                groups.list_members(gid).expect("members").len(),
                2,
                "both members must remain intact",
            );
        }

        #[test]
        fn parent_inaccessible_does_not_purge() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path_a = dir.path().join("a.mp4");
            std::fs::write(&path_a, b"dummy-a").expect("write a");
            let norm_a = NormalizedPath::new(&path_a);

            let missing_sub = dir.path().join("nonexistent_subdir").join("c.mp4");
            let norm_missing = NormalizedPath::new(&missing_sub);

            let db = vidcull_db::open_in_memory().expect("open db");
            let worker = fresh_worker(db);
            let id_a = seed_file(&worker.db, &norm_a, 0xAA);
            let id_c = seed_file(&worker.db, &norm_missing, 0xCC);
            let gid = seed_exact_group_ab(&worker.db, id_a, id_c);

            let on_disk: HashSet<String> = [norm_a.as_str().to_owned()].into_iter().collect();

            assert!(!missing_sub.exists(), "c.mp4 must not exist on disk");
            assert!(
                !missing_sub.parent().is_some_and(std::path::Path::is_dir),
                "parent subdir must not be accessible",
            );

            let norm_root = NormalizedPath::new(dir.path());
            let removes =
                reconcile_deleted_under_root(&worker.db, &norm_root, &on_disk).expect("reconcile");

            assert!(
                removes.is_empty(),
                "ENOENT with inaccessible parent must not emit Remove \
                 (guard ii — transient / volume-offline path)",
            );

            let groups = DuplicateGroupsRepo::new(worker.db.conn());
            assert!(
                groups.get(gid).expect("get group").is_some(),
                "group must survive when parent-inaccessible guard fires",
            );
            assert_eq!(
                groups.list_members(gid).expect("members").len(),
                2,
                "both members must remain intact after parent-guard no-op",
            );
        }

        #[test]
        fn folder_remove_purges_every_indexed_file_under_it() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let mut worker = fresh_worker(db);

            let inside_a = NormalizedPath::new("C:/lib/clips/a.mp4");
            let inside_b = NormalizedPath::new("C:/lib/clips/b.mp4");
            let inside_c = NormalizedPath::new("C:/lib/clips/sub/c.mp4");
            let outside = NormalizedPath::new("C:/lib/other/z.mp4");
            let id_a = seed_file(&worker.db, &inside_a, 0xA1);
            let id_b = seed_file(&worker.db, &inside_b, 0xB2);
            let _id_c = seed_file(&worker.db, &inside_c, 0xC3);
            let _id_out = seed_file(&worker.db, &outside, 0xD4);
            seed_exact_group_ab(&worker.db, id_a, id_b);

            let remove = crate::watcher::ChangeTask {
                path: NormalizedPath::new("C:/lib/clips"),
                change: ChangeKind::Remove,
                size_bytes: 0,
            };
            worker
                .handle_change(&remove, 0)
                .expect("handle folder Remove");

            let active: HashSet<String> = FilesRepo::new(worker.db.conn())
                .list_active()
                .expect("list active")
                .into_iter()
                .map(|f| f.path.as_str().to_owned())
                .collect();
            assert!(!active.contains(inside_a.as_str()), "a purged");
            assert!(!active.contains(inside_b.as_str()), "b purged");
            assert!(!active.contains(inside_c.as_str()), "nested c purged");
            assert!(
                active.contains(outside.as_str()),
                "a sibling video outside the deleted folder must survive",
            );

            let groups = DuplicateGroupsRepo::new(worker.db.conn());
            assert!(
                groups
                    .find_groups_containing(id_a)
                    .expect("groups a")
                    .is_empty(),
                "member A must be un-grouped",
            );
            assert!(
                groups
                    .find_groups_containing(id_b)
                    .expect("groups b")
                    .is_empty(),
                "member B must be un-grouped",
            );
        }
    }

    #[cfg(test)]
    mod prewarm_candidate_selection {
        use super::super::{
            DuplicateGroupsRepo, FilesRepo, NewFile, NormalizedPath, TrustLevel,
            order_members_for_prewarm, select_prewarm_targets,
        };
        use vidcull_core::types::{Blake3Hash, FileId, Resolution};

        fn seed(
            db: &vidcull_db::Database,
            path: &str,
            hash_tag: Option<u8>,
            resolution: Option<Resolution>,
            size_bytes: i64,
        ) -> FileId {
            FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    size_bytes,
                    content_hash: hash_tag.map(|t| Blake3Hash::from_bytes([t; 32])),
                    resolution,
                    ..Default::default()
                })
                .expect("insert file row")
        }

        fn hd() -> Resolution {
            Resolution::new(1920, 1080)
        }
        fn sd() -> Resolution {
            Resolution::new(640, 360)
        }

        #[test]
        fn best_first_then_by_pixel_count_descending() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let low = seed(&db, "/c/low.mp4", Some(1), Some(sd()), 100);
            let high = seed(&db, "/c/high.mp4", Some(2), Some(hd()), 200);
            let mid = seed(&db, "/c/mid.mp4", Some(3), Some(sd()), 500);

            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups
                .create(TrustLevel::VeryLikely, 0)
                .expect("create group");
            groups.add_member(gid, low).expect("add low");
            groups.add_member(gid, high).expect("add high");
            groups.add_member(gid, mid).expect("add mid");
            groups.set_best(gid, Some(low), 0).expect("set best");

            let clusters = super::super::build_clusters(&db).expect("build clusters");
            assert_eq!(clusters.len(), 1, "one transitive cluster expected");
            let cluster = &clusters[0];

            let files = FilesRepo::new(db.conn());
            let best = crate::bridge::cluster_best(&groups, cluster)
                .expect("cluster_best")
                .expect("a best pick exists");
            assert_eq!(best, low.0, "cluster_best must surface the server's pick");

            let ordered = order_members_for_prewarm(&files, cluster, Some(best));
            let ordered_ids: Vec<i64> = ordered
                .iter()
                .map(|(path, _)| {
                    if path.as_str().contains("low") {
                        low.0
                    } else if path.as_str().contains("high") {
                        high.0
                    } else {
                        mid.0
                    }
                })
                .collect();
            assert_eq!(
                ordered_ids,
                vec![low.0, high.0, mid.0],
                "best-pick first, then pixel-count descending (high 1920x1080 > mid/low 640x360, \
                 mid/low tie broken by size desc: mid=500 > low=100)",
            );
        }

        #[test]
        fn members_without_a_content_hash_are_skipped() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let hashed = seed(&db, "/c/hashed.mp4", Some(9), None, 100);
            let unhashed = seed(&db, "/c/unhashed.mp4", None, None, 100);

            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups.create(TrustLevel::Exact, 0).expect("create group");
            groups.add_member(gid, hashed).expect("add hashed");
            groups.add_member(gid, unhashed).expect("add unhashed");

            let clusters = super::super::build_clusters(&db).expect("build clusters");
            let files = FilesRepo::new(db.conn());
            let ordered = order_members_for_prewarm(&files, &clusters[0], None);
            assert_eq!(ordered.len(), 1, "only the hashed member survives");
            assert!(ordered[0].0.as_str().contains("hashed"));
        }

        #[test]
        fn caps_at_four_members_per_cluster() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups
                .create(TrustLevel::VeryLikely, 0)
                .expect("create group");
            for i in 0..6u8 {
                let id = seed(
                    &db,
                    &format!("/c/m{i}.mp4"),
                    Some(i + 1),
                    Some(sd()),
                    i64::from(i) * 10,
                );
                groups.add_member(gid, id).expect("add member");
            }

            let targets = select_prewarm_targets(&db).expect("select targets");
            assert_eq!(
                targets.len(),
                4,
                "6-member cluster capped at 4 prewarm targets"
            );
        }

        #[test]
        fn covers_every_current_cluster() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let groups = DuplicateGroupsRepo::new(db.conn());

            let gid_a = groups.create(TrustLevel::Exact, 0).expect("create a");
            let a1 = seed(&db, "/c/a1.mp4", Some(1), None, 100);
            let a2 = seed(&db, "/c/a2.mp4", Some(2), None, 100);
            groups.add_member(gid_a, a1).expect("add a1");
            groups.add_member(gid_a, a2).expect("add a2");

            let gid_b = groups.create(TrustLevel::Exact, 0).expect("create b");
            let b1 = seed(&db, "/c/b1.mp4", Some(3), None, 100);
            let b2 = seed(&db, "/c/b2.mp4", Some(4), None, 100);
            groups.add_member(gid_b, b1).expect("add b1");
            groups.add_member(gid_b, b2).expect("add b2");

            let targets = select_prewarm_targets(&db).expect("select targets");
            assert_eq!(
                targets.len(),
                4,
                "both 2-member clusters fully covered (2+2)"
            );
        }
    }

    #[cfg(test)]
    mod prewarm_in_flight_guard {
        use super::super::{
            DuplicateGroupsRepo, FfmpegBinaries, FilesRepo, IndexingHandler, NewFile,
            NormalizedPath, PrewarmInFlightGuard, TrustLevel,
        };
        use crate::thumbnails::ThumbnailProvider;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use vidcull_core::types::Blake3Hash;

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        #[test]
        fn skips_a_second_trigger_while_a_pass_is_in_flight() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups.create(TrustLevel::Exact, 0).expect("create group");
            let files = FilesRepo::new(db.conn());
            for (i, tag) in [1u8, 2u8].into_iter().enumerate() {
                let id = files
                    .insert(&NewFile {
                        path: NormalizedPath::new(format!("/c/m{i}.mp4")),
                        size_bytes: 100,
                        content_hash: Some(Blake3Hash::from_bytes([tag; 32])),
                        ..Default::default()
                    })
                    .expect("insert member");
                groups.add_member(gid, id).expect("add member");
            }

            let provider = Arc::new(ThumbnailProvider::new(std::env::temp_dir(), None));
            let handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_thumbnails(Arc::clone(&provider));

            handler.prewarm_in_flight.store(true, Ordering::Release);

            let before = Arc::strong_count(&provider);
            handler.maybe_prewarm_thumbnails();
            let after = Arc::strong_count(&provider);

            assert_eq!(
                after, before,
                "a second trigger while a pass is in flight must never clone \
                 the provider into a new spawned pass (observed before={before}, \
                 after={after})",
            );
            assert!(
                handler.prewarm_in_flight.load(Ordering::Acquire),
                "the guarded call must leave the in-flight flag exactly as \
                 the (still-running) first pass left it",
            );
        }

        #[test]
        fn prewarm_in_flight_guard_releases_the_flag_on_drop() {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
            {
                let _guard = PrewarmInFlightGuard {
                    flag: Arc::clone(&flag),
                };
                assert!(
                    flag.load(Ordering::Acquire),
                    "flag must stay set while the guard is alive"
                );
            }
            assert!(
                !flag.load(Ordering::Acquire),
                "guard must release the flag when it drops"
            );
        }

        #[test]
        fn prewarm_in_flight_guard_releases_the_flag_even_on_panic_unwind() {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let flag_for_thread = Arc::clone(&flag);
            let result = std::thread::spawn(move || {
                let _guard = PrewarmInFlightGuard {
                    flag: flag_for_thread,
                };
                panic!("simulated failure partway through a prewarm pass");
            })
            .join();

            assert!(
                result.is_err(),
                "sanity: the panic must have actually propagated"
            );
            assert!(
                !flag.load(Ordering::Acquire),
                "a panic partway through the spawned pass must still release \
                 prewarm_in_flight via Drop — the old bare store(false, ...) \
                 at the end of the closure would never run here, wedging the \
                 flag at true forever"
            );
        }
    }

    #[cfg(test)]
    mod partial_off_inertness_guards {
        use super::super::{
            DuplicateGroupsRepo, FORMAT_VERSION, FfmpegBinaries, FilesRepo, Fingerprint,
            FingerprintsRepo, IndexingHandler, NewFile, RegroupQueueRepo, TrustLevel, encode_tier1,
            encode_tier2,
        };
        use crate::TaskHandler;
        use vidcull_core::types::{Codec, FileId, NormalizedPath};
        use vidcull_db::Database;
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
        use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn flip_low_bits(h: u64, n: u32) -> u64 {
            if n == 0 {
                return h;
            }
            let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
            h ^ mask
        }

        fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
            let mut state = seed;
            let scenes = (0..n)
                .map(|i| SceneHash {
                    timestamp_ms: i as u64 * 1000,
                    phash: splitmix64(&mut state) | 1,
                })
                .collect();
            Tier2Fingerprint { scenes }
        }

        fn clip_of(
            source: &Tier2Fingerprint,
            start: usize,
            len: usize,
            perturb: u32,
        ) -> Tier2Fingerprint {
            let scenes = source.scenes[start..start + len]
                .iter()
                .enumerate()
                .map(|(i, s)| SceneHash {
                    timestamp_ms: i as u64 * 1000,
                    phash: flip_low_bits(s.phash, perturb),
                })
                .collect();
            Tier2Fingerprint { scenes }
        }

        fn seed_tier2(db: &Database, path: &str, tier2: &Tier2Fingerprint) -> FileId {
            let id = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    ..Default::default()
                })
                .expect("insert file row");
            let t1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash: tier2.scenes.first().map_or(0, |s| s.phash),
            };
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: encode_tier1(&t1).expect("encode tier1"),
                    tier2_temporal: Some(encode_tier2(tier2).expect("encode tier2")),
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: T0,
                })
                .expect("upsert fingerprint");
            RegroupQueueRepo::new(db.conn())
                .mark(id, T0)
                .expect("mark regroup");
            id
        }

        fn set_partial(db: &Database, id: FileId, tier2: &Tier2Fingerprint) {
            let written = FingerprintsRepo::new(db.conn())
                .set_partial(id, &encode_tier2(tier2).expect("encode partial"))
                .expect("set partial");
            assert_eq!(written, 1, "partial blob must land on the existing row");
        }

        fn sorted_pair(a: FileId, b: FileId) -> Vec<i64> {
            let mut v = vec![a.0, b.0];
            v.sort_unstable();
            v
        }

        fn possible_groups(db: &Database) -> Vec<Vec<i64>> {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let mut out: Vec<Vec<i64>> = repo
                .list_all()
                .expect("list groups")
                .into_iter()
                .filter(|g| g.trust_level == TrustLevel::Possible)
                .map(|g| {
                    let mut members: Vec<i64> = repo
                        .list_members(g.id)
                        .expect("members")
                        .into_iter()
                        .map(|f| f.0)
                        .collect();
                    members.sort_unstable();
                    members
                })
                .collect();
            out.sort();
            out
        }

        fn seed_dual_signal_corpus(
            enabled: bool,
        ) -> (IndexingHandler, FileId, FileId, FileId, FileId) {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");

            let src_ab = source_seq(0x1234, 40);
            let clip_ab = clip_of(&src_ab, 10, 6, 3);
            let a = seed_tier2(&db, "/v/a.mp4", &src_ab);
            let b = seed_tier2(&db, "/v/b.mp4", &clip_ab);

            let c = seed_tier2(&db, "/v/c.mp4", &source_seq(0xAAAA, 40));
            let d = seed_tier2(&db, "/v/d.mp4", &source_seq(0xBBBB, 40));
            let src_cd = source_seq(0x5678, 40);
            let clip_cd = clip_of(&src_cd, 10, 6, 3);
            set_partial(&db, c, &src_cd);
            set_partial(&db, d, &clip_cd);

            let handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(enabled);
            (handler, a, b, c, d)
        }

        #[test]
        fn off_groups_possible_from_native_tier2_ignoring_stale_partial() {
            let (mut handler, a, b, _c, _d) = seed_dual_signal_corpus(false);
            handler.rebuild_matches().expect("rebuild with partial OFF");

            assert_eq!(
                possible_groups(&handler.worker.db),
                vec![sorted_pair(a, b)],
                "partial OFF: the POSSIBLE group must come from the durable native \
                 tier2 path (a⊃b); the stale partial rows on c/d are not read",
            );
        }

        #[test]
        fn on_groups_possible_from_partial_fingerprints() {
            let (mut handler, _a, _b, c, d) = seed_dual_signal_corpus(true);
            handler.rebuild_matches().expect("rebuild with partial ON");

            assert_eq!(
                possible_groups(&handler.worker.db),
                vec![sorted_pair(c, d)],
                "partial ON: the POSSIBLE group must come from the frame-accurate \
                 partial fingerprints (c⊃d); a/b carry only a tier2 signal the \
                 partial path does not consume",
            );
        }

        #[test]
        fn live_toggle_round_trip_reshapes_possible_groups() {
            let (mut handler, a, b, c, d) = seed_dual_signal_corpus(false);

            handler.rebuild_matches().expect("OFF rebuild");
            assert_eq!(
                possible_groups(&handler.worker.db),
                vec![sorted_pair(a, b)],
                "OFF: only the native tier2 pair (a⊃b)",
            );

            handler.set_partial_clips_live(true);
            handler.rebuild_matches().expect("ON rebuild after toggle");
            assert_eq!(
                possible_groups(&handler.worker.db),
                vec![sorted_pair(c, d)],
                "ON after toggle: only the partial pair (c⊃d); the tier2 a⊃b group is gone",
            );

            handler.set_partial_clips_live(false);
            handler
                .rebuild_matches()
                .expect("OFF rebuild after toggle back");
            assert_eq!(
                possible_groups(&handler.worker.db),
                vec![sorted_pair(a, b)],
                "OFF again: the tier2 pair is restored with no partial residue leaked",
            );
        }
    }

    #[cfg(test)]
    mod foreground_drain_surfaces_near_exact {
        use super::super::{
            DuplicateGroupsRepo, FORMAT_VERSION, FfmpegBinaries, FilesRepo, Fingerprint,
            FingerprintsRepo, IndexingHandler, NewFile, PARTIAL_PRIORITY, RegroupQueueRepo,
            TrustLevel, encode_tier1,
        };
        use crate::TaskHandler;
        use vidcull_core::types::{Codec, FileId, NormalizedPath};
        use vidcull_db::Database;
        use vidcull_db::repo::{NewTask, TaskQueueRepo, TaskState};
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn seed_tier1(db: &Database, path: &str, global_phash: u64) -> FileId {
            let id = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    ..Default::default()
                })
                .expect("insert file row");
            let t1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash,
            };
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: encode_tier1(&t1).expect("encode tier1"),
                    tier2_temporal: None,
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: T0,
                })
                .expect("upsert fingerprint");
            RegroupQueueRepo::new(db.conn())
                .mark(id, T0)
                .expect("mark regroup");
            id
        }

        fn very_likely_groups(db: &Database) -> Vec<Vec<i64>> {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let mut out: Vec<Vec<i64>> = repo
                .list_all()
                .expect("list groups")
                .into_iter()
                .filter(|g| g.trust_level == TrustLevel::VeryLikely)
                .map(|g| {
                    let mut members: Vec<i64> = repo
                        .list_members(g.id)
                        .expect("members")
                        .into_iter()
                        .map(|f| f.0)
                        .collect();
                    members.sort_unstable();
                    members
                })
                .collect();
            out.sort();
            out
        }

        #[test]
        fn pending_partial_does_not_block_near_exact_on_foreground_drain() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");

            let a = seed_tier1(&db, "/v/reencode_a.mp4", 0x0000_0000_0000_0F00);
            let b = seed_tier1(&db, "/v/reencode_b.mp4", 0x0000_0000_0000_0F07);

            TaskQueueRepo::new(db.conn())
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL_PRIORITY,
                    payload: Some(b"partial-task".to_vec()),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue a low-priority partial task");

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            );

            handler
                .after_burst_chunk(2, false)
                .expect("after_burst_chunk with partial still pending");

            assert_eq!(
                very_likely_groups(&handler.worker.db),
                vec![vec![a.0, b.0]],
                "foreground drain must surface the near/exact (VERY_LIKELY) group even \
                 while a low-priority partial task is still PENDING in the queue",
            );

            assert_eq!(
                TaskQueueRepo::new(handler.worker.db.conn())
                    .list_by_state(TaskState::Pending)
                    .expect("pending")
                    .len(),
                1,
                "the low-priority partial task is untouched, only deferred",
            );
        }
    }

    #[cfg(test)]
    mod best_copy_and_high_water {
        use super::super::{
            DuplicateGroupsRepo, FORMAT_VERSION, FfmpegBinaries, FilesRepo, Fingerprint,
            FingerprintsRepo, IndexingHandler, NewFile, PARTIAL_PRIORITY, RegroupQueueRepo,
            TrustLevel, encode_tier1, encode_tier2,
        };
        use crate::TaskHandler;
        use vidcull_core::types::{Codec, FileId, NormalizedPath};
        use vidcull_db::Database;
        use vidcull_db::repo::{NewTask, TaskQueueRepo};
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
        use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
            let mut state = seed;
            let scenes = (0..n)
                .map(|i| SceneHash {
                    timestamp_ms: i as u64 * 1000,
                    phash: splitmix64(&mut state) | 1,
                })
                .collect();
            Tier2Fingerprint { scenes }
        }

        fn clean_clip(source: &Tier2Fingerprint, start: usize, len: usize) -> Tier2Fingerprint {
            let scenes = source.scenes[start..start + len]
                .iter()
                .enumerate()
                .map(|(i, s)| SceneHash {
                    timestamp_ms: i as u64 * 1000,
                    phash: s.phash,
                })
                .collect();
            Tier2Fingerprint { scenes }
        }

        fn seed_tier1(db: &Database, path: &str, global_phash: u64) -> FileId {
            let id = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    ..Default::default()
                })
                .expect("insert file row");
            let t1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash,
            };
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: encode_tier1(&t1).expect("encode tier1"),
                    tier2_temporal: None,
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: T0,
                })
                .expect("upsert fingerprint");
            RegroupQueueRepo::new(db.conn())
                .mark(id, T0)
                .expect("mark regroup");
            id
        }

        fn set_partial(db: &Database, id: FileId, tier2: &Tier2Fingerprint) {
            let written = FingerprintsRepo::new(db.conn())
                .set_partial(id, &encode_tier2(tier2).expect("encode partial"))
                .expect("set partial");
            assert_eq!(written, 1, "partial blob must land on the existing row");
        }

        fn group_bests(db: &Database) -> Vec<(TrustLevel, Option<FileId>)> {
            DuplicateGroupsRepo::new(db.conn())
                .list_all()
                .expect("list groups")
                .into_iter()
                .map(|g| (g.trust_level, g.best_file_id))
                .collect()
        }

        #[test]
        fn full_rebuild_stamps_best_on_near_exact_and_possible_groups() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");

            seed_tier1(&db, "/v/reencode_a.mp4", 0x0000_0000_0000_0F00);
            seed_tier1(&db, "/v/reencode_b.mp4", 0x0000_0000_0000_0F07);

            let src = source_seq(0x5678, 40);
            let clip = clean_clip(&src, 10, 8);
            let c = seed_tier1(&db, "/v/src_c.mp4", 0x0102_0304_0506_0708);
            let d = seed_tier1(&db, "/v/clip_d.mp4", 0xF1F2_F3F4_F5F6_F7F8);
            set_partial(&db, c, &src);
            set_partial(&db, d, &clip);

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(true);

            handler.rebuild_matches().expect("full rebuild");

            let bests = group_bests(&handler.worker.db);
            let very_likely: Vec<_> = bests
                .iter()
                .filter(|(t, _)| *t == TrustLevel::VeryLikely)
                .collect();
            let possible: Vec<_> = bests
                .iter()
                .filter(|(t, _)| *t == TrustLevel::Possible)
                .collect();
            assert_eq!(very_likely.len(), 1, "one VERY_LIKELY group: {bests:?}");
            assert_eq!(possible.len(), 1, "one POSSIBLE group: {bests:?}");
            assert!(
                very_likely[0].1.is_some(),
                "near/exact group best stamped (no-flicker pass): {bests:?}",
            );
            assert!(
                possible[0].1.is_some(),
                "#1: POSSIBLE (partial-clip) group best must be stamped by the \
                 second assign that runs after the partial pass: {bests:?}",
            );
        }

        #[test]
        fn foreground_branch_assigns_near_exact_best_with_partial_pending() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            seed_tier1(&db, "/v/reencode_a.mp4", 0x0000_0000_0000_0F00);
            seed_tier1(&db, "/v/reencode_b.mp4", 0x0000_0000_0000_0F07);

            TaskQueueRepo::new(db.conn())
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL_PRIORITY,
                    payload: Some(b"partial-task".to_vec()),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue low-priority partial task");

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(true);

            handler
                .after_burst_chunk(2, false)
                .expect("foreground drain with partial pending");

            let very_likely: Vec<_> = group_bests(&handler.worker.db)
                .into_iter()
                .filter(|(t, _)| *t == TrustLevel::VeryLikely)
                .collect();
            assert_eq!(
                very_likely.len(),
                1,
                "near/exact group surfaced on foreground"
            );
            assert!(
                very_likely[0].1.is_some(),
                "foreground pass stamps near/exact best even while partial is PENDING",
            );
        }

        #[test]
        fn high_water_skips_redundant_foreground_until_delta_grows() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            seed_tier1(&db, "/v/reencode_a.mp4", 0x0000_0000_0000_0F00);
            seed_tier1(&db, "/v/reencode_b.mp4", 0x0000_0000_0000_0F07);
            TaskQueueRepo::new(db.conn())
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL_PRIORITY,
                    payload: Some(b"partial-task".to_vec()),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue low-priority partial task");

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(true);

            handler.after_burst_chunk(2, false).expect("first chunk");
            let after_first = handler.rebuild_count();
            assert!(after_first >= 1, "first foreground chunk runs a rebuild");

            handler.after_burst_chunk(0, false).expect("second chunk");
            assert_eq!(
                handler.rebuild_count(),
                after_first,
                "#6: unchanged delta must skip the foreground rebuild",
            );

            seed_tier1(
                &handler.worker.db,
                "/v/reencode_c.mp4",
                0x0000_0000_0000_0F03,
            );
            handler.after_burst_chunk(1, false).expect("third chunk");
            assert!(
                handler.rebuild_count() > after_first,
                "#6: a grown delta must NOT be skipped",
            );
        }
    }

    #[cfg(test)]
    mod partial_foreground_determinism {
        use super::super::{
            DuplicateGroupsRepo, FORMAT_VERSION, FfmpegBinaries, FilesRepo, Fingerprint,
            FingerprintsRepo, IndexingHandler, NewFile, PARTIAL_PRIORITY, RegroupQueueRepo,
            TrustLevel, encode_tier1, encode_tier2,
        };
        use crate::TaskHandler;
        use vidcull_core::types::{Codec, FileId, NormalizedPath};
        use vidcull_db::Database;
        use vidcull_db::repo::{NewTask, TaskQueueRepo};
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
        use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn trust_rank(t: TrustLevel) -> u8 {
            match t {
                TrustLevel::Exact => 0,
                TrustLevel::VeryLikely => 1,
                TrustLevel::Possible => 2,
            }
        }

        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
            let mut state = seed;
            let scenes = (0..n)
                .map(|i| SceneHash {
                    timestamp_ms: i as u64 * 1000,
                    phash: splitmix64(&mut state) | 1,
                })
                .collect();
            Tier2Fingerprint { scenes }
        }

        fn clean_clip(source: &Tier2Fingerprint, start: usize, len: usize) -> Tier2Fingerprint {
            let scenes = source.scenes[start..start + len]
                .iter()
                .enumerate()
                .map(|(i, s)| SceneHash {
                    timestamp_ms: i as u64 * 1000,
                    phash: s.phash,
                })
                .collect();
            Tier2Fingerprint { scenes }
        }

        fn seed_tier1(db: &Database, path: &str, global_phash: u64) -> FileId {
            let id = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    ..Default::default()
                })
                .expect("insert file row");
            let t1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash,
            };
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: encode_tier1(&t1).expect("encode tier1"),
                    tier2_temporal: None,
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: T0,
                })
                .expect("upsert fingerprint");
            RegroupQueueRepo::new(db.conn())
                .mark(id, T0)
                .expect("mark regroup");
            id
        }

        fn set_partial(db: &Database, id: FileId, tier2: &Tier2Fingerprint) {
            let written = FingerprintsRepo::new(db.conn())
                .set_partial(id, &encode_tier2(tier2).expect("encode partial"))
                .expect("set partial");
            assert_eq!(written, 1, "partial blob must land on the existing row");
        }

        fn group_snapshot(db: &Database) -> Vec<(u8, Vec<i64>, bool)> {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let mut out: Vec<(u8, Vec<i64>, bool)> = repo
                .list_all()
                .expect("list groups")
                .into_iter()
                .map(|g| {
                    let mut members: Vec<i64> = repo
                        .list_members(g.id)
                        .expect("members")
                        .into_iter()
                        .map(|f| f.0)
                        .collect();
                    members.sort_unstable();
                    (trust_rank(g.trust_level), members, g.best_file_id.is_some())
                })
                .collect();
            out.sort();
            out
        }

        fn seed_fixture() -> Database {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            seed_tier1(&db, "/v/reencode_a.mp4", 0x0000_0000_0000_0F00);
            seed_tier1(&db, "/v/reencode_b.mp4", 0x0000_0000_0000_0F07);

            let src = source_seq(0x5678, 40);
            let clip = clean_clip(&src, 10, 8);
            let source_id = seed_tier1(&db, "/v/source.mp4", 0x0102_0304_0506_0708);
            let clip_id = seed_tier1(&db, "/v/clip.mp4", 0xF1F2_F3F4_F5F6_F7F8);
            set_partial(&db, source_id, &src);
            set_partial(&db, clip_id, &clip);

            TaskQueueRepo::new(db.conn())
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL_PRIORITY,
                    payload: Some(b"partial-task".to_vec()),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue low-priority partial task");
            db
        }

        #[test]
        fn early_partial_foreground_trigger_does_not_change_final_drain_state() {
            let db_early = seed_fixture();
            let bins_early = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
            let mut handler_early =
                IndexingHandler::new(db_early, bins_early, now_t0).with_partial_clips(true);
            handler_early
                .after_burst_chunk(2, false)
                .expect("early foreground-only chunk takes the new partial trigger");
            assert!(
                !group_snapshot(&handler_early.worker.db).is_empty(),
                "the early trigger must have already produced groups before the full drain",
            );
            handler_early
                .rebuild_matches()
                .expect("full drain rebuild after the early trigger");
            let final_early = group_snapshot(&handler_early.worker.db);

            let db_drain_only = seed_fixture();
            let mut handler_drain_only = IndexingHandler::new(
                db_drain_only,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(true);
            handler_drain_only
                .rebuild_matches()
                .expect("drain-only full rebuild, no early trigger");
            let final_drain_only = group_snapshot(&handler_drain_only.worker.db);

            assert_eq!(
                final_early, final_drain_only,
                "the early partial-foreground trigger must only change \
                 timing, never the final group set a full drain settles on",
            );
        }
    }

    #[cfg(test)]
    mod whole_file_emit_integration {
        use super::super::{
            DuplicateGroupsRepo, FORMAT_VERSION, FfmpegBinaries, FilesRepo, Fingerprint,
            FingerprintsRepo, IndexingHandler, NewFile, RegroupQueueRepo, TrustLevel, encode_tier1,
            encode_tier2,
        };
        use vidcull_core::types::{Codec, FileId, NormalizedPath};
        use vidcull_db::Database;
        use vidcull_db::repo::DuplicateGroup;
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
        use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

        const T0: i64 = 1_700_000_000;

        fn now_t0() -> i64 {
            T0
        }

        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        const GRID_MS: u64 = vidcull_core::SPARSE_GRID_INTERVAL_MS;

        fn reencode_pair(seed: u64, n: usize) -> (Tier2Fingerprint, Tier2Fingerprint) {
            let mut st = seed;
            let a: Vec<SceneHash> = (0..n)
                .map(|i| SceneHash {
                    timestamp_ms: i as u64 * GRID_MS,
                    phash: splitmix64(&mut st) | 1,
                })
                .collect();
            let b: Vec<SceneHash> = a
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let ph = if i % 4 == 0 {
                        s.phash ^ 0b110
                    } else {
                        splitmix64(&mut st) | 1
                    };
                    SceneHash {
                        timestamp_ms: s.timestamp_ms + GRID_MS,
                        phash: ph,
                    }
                })
                .collect();
            (
                Tier2Fingerprint { scenes: a },
                Tier2Fingerprint { scenes: b },
            )
        }

        fn seed_tier2(
            db: &Database,
            path: &str,
            tier1_global: u64,
            tier2: &Tier2Fingerprint,
        ) -> FileId {
            let id = FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    ..Default::default()
                })
                .expect("insert file row");
            let t1 = Tier1Fingerprint {
                duration_ms: (tier2.scenes.len() as u64) * 1000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash: tier1_global,
            };
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: encode_tier1(&t1).expect("encode tier1"),
                    tier2_temporal: Some(encode_tier2(tier2).expect("encode tier2")),
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: T0,
                })
                .expect("upsert fingerprint");
            RegroupQueueRepo::new(db.conn())
                .mark(id, T0)
                .expect("mark regroup");
            id
        }

        fn very_likely_groups(db: &Database) -> Vec<(Vec<i64>, bool)> {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let mut out: Vec<(Vec<i64>, bool)> = repo
                .list_all()
                .expect("list groups")
                .into_iter()
                .filter(|g: &DuplicateGroup| g.trust_level == TrustLevel::VeryLikely)
                .map(|g| {
                    let mut members: Vec<i64> = repo
                        .list_members(g.id)
                        .expect("members")
                        .into_iter()
                        .map(|f| f.0)
                        .collect();
                    members.sort_unstable();
                    (members, g.non_transitive)
                })
                .collect();
            out.sort();
            out
        }

        #[test]
        fn reencode_pair_surfaces_as_non_transitive_very_likely_group() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            let (a, b) = reencode_pair(0x1234_5678, 400);
            let fid_a = seed_tier2(&db, "/v/reencode_a.mp4", 0x0102_0304_0506_0708, &a);
            let fid_b = seed_tier2(&db, "/v/reencode_b.mp4", 0xF1F2_F3F4_F5F6_F7F8, &b);

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(false);

            handler.rebuild_matches().expect("rebuild");

            let mut expected = [fid_a.0, fid_b.0];
            expected.sort_unstable();
            assert_eq!(
                very_likely_groups(&handler.worker.db),
                vec![(expected.to_vec(), true)],
                "a whole-file re-encode pair must surface as exactly one \
                 VERY_LIKELY group flagged non_transitive=true",
            );
        }

        #[test]
        fn near_covered_reencode_pair_is_not_duplicated_as_whole_file_card() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            let (a, b) = reencode_pair(0x00C0_FFEE, 400);
            let fid_a = seed_tier2(&db, "/v/near_covered_a.mp4", 0x0102_0304_0506_0708, &a);
            let fid_b = seed_tier2(&db, "/v/near_covered_b.mp4", 0x0102_0304_0506_0708, &b);

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(false);

            handler.rebuild_matches().expect("rebuild");

            let mut expected = [fid_a.0, fid_b.0];
            expected.sort_unstable();
            assert_eq!(
                very_likely_groups(&handler.worker.db),
                vec![(expected.to_vec(), false)],
                "a pair the near matcher already groups transitively must \
                 surface as exactly that one card — never plus a duplicate \
                 non_transitive whole-file twin",
            );
        }

        #[test]
        fn second_burst_adding_only_the_partner_still_discovers_the_pair() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            let (a, b) = reencode_pair(0x9876_5432, 400);

            let fid_a = seed_tier2(&db, "/v/reencode_a.mp4", 0x0102_0304_0506_0708, &a);
            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(false);
            handler.rebuild_matches().expect("first rebuild (a only)");
            assert!(
                very_likely_groups(&handler.worker.db).is_empty(),
                "no pair to find with only one side indexed",
            );

            let fid_b = seed_tier2(
                &handler.worker.db,
                "/v/reencode_b.mp4",
                0xF1F2_F3F4_F5F6_F7F8,
                &b,
            );
            handler.rebuild_matches().expect("second rebuild (b added)");

            let mut expected = [fid_a.0, fid_b.0];
            expected.sort_unstable();
            assert_eq!(
                very_likely_groups(&handler.worker.db),
                vec![(expected.to_vec(), true)],
                "a burst that only touches the newly-added re-encoded partner must \
                 still discover its unchanged sibling and emit the pair",
            );
        }

        #[test]
        fn unrelated_pair_never_surfaces_as_whole_file_group() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            let mut sa = 0xC0FF_EE00_u64;
            let mut sb = 0xFACE_0FF1_u64;
            let a = Tier2Fingerprint {
                scenes: (0u64..400)
                    .map(|i| SceneHash {
                        timestamp_ms: i * 1000,
                        phash: splitmix64(&mut sa) | 1,
                    })
                    .collect(),
            };
            let b = Tier2Fingerprint {
                scenes: (0u64..400)
                    .map(|i| SceneHash {
                        timestamp_ms: i * 1000,
                        phash: splitmix64(&mut sb) | 1,
                    })
                    .collect(),
            };
            seed_tier2(&db, "/v/unrelated_a.mp4", 0x0102_0304_0506_0708, &a);
            seed_tier2(&db, "/v/unrelated_b.mp4", 0xF1F2_F3F4_F5F6_F7F8, &b);

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(false);

            handler.rebuild_matches().expect("rebuild");

            assert!(
                very_likely_groups(&handler.worker.db).is_empty(),
                "two unrelated videos must never surface as a whole-file VERY_LIKELY group",
            );
        }

        #[test]
        fn whole_file_group_does_not_absorb_or_merge_with_a_near_dup_cluster() {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");

            seed_tier2(
                &db,
                "/v/near_a.mp4",
                0x0000_0000_0000_0F00,
                &Tier2Fingerprint {
                    scenes: vec![SceneHash {
                        timestamp_ms: 0,
                        phash: 0x0000_0000_0000_0F00,
                    }],
                },
            );
            seed_tier2(
                &db,
                "/v/near_b.mp4",
                0x0000_0000_0000_0F07,
                &Tier2Fingerprint {
                    scenes: vec![SceneHash {
                        timestamp_ms: 0,
                        phash: 0x0000_0000_0000_0F07,
                    }],
                },
            );

            let (a, b) = reencode_pair(0xAAAA_BBBB, 400);
            let whole_first = seed_tier2(&db, "/v/whole_a.mp4", 0x1111_2222_3333_4444, &a);
            let whole_second = seed_tier2(&db, "/v/whole_b.mp4", 0x5555_6666_7777_8888, &b);

            let mut handler = IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_t0,
            )
            .with_partial_clips(false);

            handler.rebuild_matches().expect("rebuild");

            let groups = very_likely_groups(&handler.worker.db);
            let mut expected_whole = [whole_first.0, whole_second.0];
            expected_whole.sort_unstable();
            assert_eq!(
                groups.len(),
                2,
                "the ordinary near-dup pair and the whole-file pair must stay two \
                 separate VERY_LIKELY groups (no cascade-merge): {groups:?}",
            );
            assert!(
                groups.contains(&(expected_whole.to_vec(), true)),
                "the whole-file pair's own group must be present and flagged \
                 non_transitive: {groups:?}",
            );
            assert!(
                groups.iter().any(|(_, nt)| !nt),
                "the ordinary near-dup pair's group must stay transitive \
                 (non_transitive=false), untouched by the whole-file emit pass: {groups:?}",
            );
        }
    }

    mod base_decode_gate {
        use super::super::{BASE_DECODE_CONCURRENCY, BASE_DECODE_GATE_BUSY_REASON, BaseDecodeGate};

        #[test]
        fn try_acquire_succeeds_within_capacity() {
            let gate = BaseDecodeGate::new(2);
            let g1 = gate.try_acquire();
            assert!(g1.is_some(), "first acquire must succeed (capacity=2)");
            let g2 = gate.try_acquire();
            assert!(g2.is_some(), "second acquire must succeed (capacity=2)");
            let g3 = gate.try_acquire();
            assert!(g3.is_none(), "third acquire must fail — gate at capacity");
        }

        #[test]
        fn drop_releases_slot() {
            let gate = BaseDecodeGate::new(1);
            {
                let g = gate.try_acquire();
                assert!(g.is_some(), "first acquire must succeed");
                assert!(gate.try_acquire().is_none(), "gate must be full while held");
            }
            assert!(
                gate.try_acquire().is_some(),
                "acquire must succeed again after guard drop"
            );
        }

        #[test]
        fn set_capacity_adjusts_limit() {
            let gate = BaseDecodeGate::new(1);
            let _g1 = gate.try_acquire().expect("first slot");
            assert!(gate.try_acquire().is_none(), "should be full at capacity=1");
            gate.set_capacity(2);
            assert!(
                gate.try_acquire().is_some(),
                "capacity raised to 2 — second slot must be available"
            );
        }

        #[test]
        fn snapshot_reflects_in_use_and_capacity() {
            let gate = BaseDecodeGate::new(3);
            let (in_use, cap) = gate.snapshot();
            assert_eq!(in_use, 0);
            assert_eq!(cap, 3);
            let _g = gate.try_acquire().expect("acquire");
            let (in_use2, cap2) = gate.snapshot();
            assert_eq!(in_use2, 1);
            assert_eq!(cap2, 3);
        }

        #[test]
        fn busy_reason_constant_matches_expected_prefix() {
            assert!(
                BASE_DECODE_GATE_BUSY_REASON.contains("base-index decode gate"),
                "busy reason must identify base-index gate for short-backoff routing"
            );
        }

        #[test]
        fn default_concurrency_ceiling_allows_full_worker_budget() {
            const {
                assert!(
                    BASE_DECODE_CONCURRENCY >= 16 && BASE_DECODE_CONCURRENCY <= 256,
                    "BASE_DECODE_CONCURRENCY ceiling must be in [16,256]"
                );
            };
        }
    }

    mod decode_gate_observer {
        use super::super::{
            BaseDecodeGate, DecodeConcurrency, DecodeGateObserver, PartialDecodeGate,
        };
        use std::sync::Arc;

        #[test]
        fn unpublished_observer_yields_none() {
            let observer = DecodeGateObserver::default();
            assert!(
                observer.snapshot().is_none(),
                "an observer with no published handles must report None"
            );
        }

        #[test]
        fn snapshot_reflects_live_in_use_across_gates() {
            let observer = DecodeGateObserver::default();
            let decode_conc = Arc::new(DecodeConcurrency::new(8));
            let base_gate = Arc::new(BaseDecodeGate::new(8));
            let partial_gate = Arc::new(PartialDecodeGate::new(8));
            let seq_read_gate = Arc::new(BaseDecodeGate::new(4));
            observer.publish(
                Arc::clone(&decode_conc),
                Arc::clone(&base_gate),
                Arc::clone(&partial_gate),
                Arc::clone(&seq_read_gate),
            );

            let _b1 = base_gate.try_acquire().expect("base slot 1");
            let _b2 = base_gate.try_acquire().expect("base slot 2");
            let _p1 = partial_gate.try_acquire().expect("partial slot 1");

            let snap = observer
                .snapshot()
                .expect("published observer yields a snapshot");
            assert_eq!(snap.base_gate_in_use, 2, "two base-gate slots held");
            assert_eq!(snap.base_gate_cap, 8);
            assert_eq!(snap.partial_gate_in_use, 1, "one partial-gate slot held");
            assert_eq!(snap.partial_gate_cap, 8);
            assert_eq!(
                snap.active_decode_workers, 3,
                "active workers = base (2) + partial (1) gate holders"
            );
            assert_eq!(snap.decode_conc_in_use, 0);
            assert_eq!(snap.decode_conc_cap, 8);
        }

        #[test]
        fn releasing_a_slot_lowers_in_use() {
            let observer = DecodeGateObserver::default();
            let decode_conc = Arc::new(DecodeConcurrency::new(4));
            let base_gate = Arc::new(BaseDecodeGate::new(4));
            let partial_gate = Arc::new(PartialDecodeGate::new(4));
            let seq_read_gate = Arc::new(BaseDecodeGate::new(4));
            observer.publish(
                Arc::clone(&decode_conc),
                Arc::clone(&base_gate),
                Arc::clone(&partial_gate),
                Arc::clone(&seq_read_gate),
            );

            {
                let _b = base_gate.try_acquire().expect("base slot");
                assert_eq!(observer.snapshot().unwrap().base_gate_in_use, 1);
                assert_eq!(observer.snapshot().unwrap().active_decode_workers, 1);
            }
            let snap = observer.snapshot().unwrap();
            assert_eq!(snap.base_gate_in_use, 0, "release drops the in-use count");
            assert_eq!(snap.active_decode_workers, 0);
        }
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
    fn batch_artifacts(
        metadata: &super::VideoMetadata,
        frames: &[vidcull_parser::sparse::GrayscaleFrame],
    ) -> super::FingerprintArtifacts {
        use vidcull_core::types::VideoDuration;
        use vidcull_fingerprint::format::{encode_tier1, encode_tier2};
        use vidcull_fingerprint::{GrayFrame, TimedFrame, build_tier1, build_tier2};

        let gray: Vec<GrayFrame<'_>> = frames
            .iter()
            .map(|f| GrayFrame {
                width: f.width,
                height: f.height,
                pixels: &f.pixels,
            })
            .collect();
        let tier1 = build_tier1(
            metadata.duration.unwrap_or(VideoDuration::ZERO),
            metadata.codec.clone(),
            &[],
            &gray,
        );
        let timed: Vec<TimedFrame<'_>> = frames
            .iter()
            .map(|f| TimedFrame {
                timestamp_ms: f.timestamp_ms,
                frame: GrayFrame {
                    width: f.width,
                    height: f.height,
                    pixels: &f.pixels,
                },
            })
            .collect();
        let tier2 = build_tier2(&timed);

        let mut sum_lap = 0.0_f64;
        let mut sum_dct = 0.0_f64;
        for g in &gray {
            sum_lap += vidcull_fingerprint::laplacian_variance(g);
            sum_dct += vidcull_fingerprint::dct_energy(g);
        }
        let n = gray.len();
        let (laplacian_variance, dct_energy) = if n > 0 {
            (Some(sum_lap / n as f64), Some(sum_dct / n as f64))
        } else {
            (None, None)
        };

        let bpp = if let (Some(bitrate), Some(fps_x1000), w, h) = (
            metadata.bitrate_bps,
            metadata.fps_x1000,
            metadata.resolution.width,
            metadata.resolution.height,
        ) {
            if w > 0 && h > 0 && fps_x1000 > 0 && bitrate > 0 {
                let fps = fps_x1000 as f64 / 1000.0;
                Some(bitrate as f64 / (w as f64 * h as f64 * fps))
            } else {
                None
            }
        } else {
            None
        };

        super::FingerprintArtifacts {
            tier1_blob: encode_tier1(&tier1).unwrap(),
            tier2_blob: encode_tier2(&tier2).unwrap(),
            laplacian_variance,
            dct_energy,
            bpp,
        }
    }

    #[test]
    fn streaming_fingerprint_is_byte_identical_to_batch_for_synthetic_frames() {
        use super::{Tier1Builder, Tier2Builder, finish_streaming_artifacts};
        use vidcull_core::types::{Codec, Resolution, VideoDuration};
        use vidcull_fingerprint::{GrayFrame, TimedFrame};
        use vidcull_parser::sparse::GrayscaleFrame;
        use vidcull_parser::{ContainerKind, VideoMetadata};

        let metadata = VideoMetadata {
            codec: Codec::H264,
            container: ContainerKind::Mp4,
            duration: Some(VideoDuration::from_millis(5000)),
            fps_x1000: Some(30_000),
            bitrate_bps: Some(2_000_000),
            resolution: Resolution {
                width: 64,
                height: 64,
            },
            has_b_frames: None,
            encoder_tags: None,
        };

        let mut state = 0x000C_0FFE_E141_u64;
        let next_px = |s: &mut u64| -> Vec<u8> {
            (0..64 * 64)
                .map(|_| {
                    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
                    let mut z = *s;
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                    ((z ^ (z >> 31)) & 0xFF) as u8
                })
                .collect()
        };

        let pix: Vec<Vec<u8>> = (0..4).map(|_| next_px(&mut state)).collect();
        let frames: Vec<GrayscaleFrame> = vec![
            GrayscaleFrame {
                width: 64,
                height: 64,
                timestamp_ms: 0,
                pixels: pix[0].clone(),
            },
            GrayscaleFrame {
                width: 64,
                height: 64,
                timestamp_ms: 2500,
                pixels: pix[1].clone(),
            },
            GrayscaleFrame {
                width: 0,
                height: 64,
                timestamp_ms: 3000,
                pixels: vec![],
            },
            GrayscaleFrame {
                width: 64,
                height: 64,
                timestamp_ms: 5000,
                pixels: pix[2].clone(),
            },
            GrayscaleFrame {
                width: 64,
                height: 64,
                timestamp_ms: 7500,
                pixels: pix[3].clone(),
            },
        ];

        let batch = batch_artifacts(&metadata, &frames);

        let mut tier1 = Tier1Builder::new();
        let mut tier2 = Tier2Builder::new();
        let mut sum_lap = 0.0_f64;
        let mut sum_dct = 0.0_f64;
        let mut frame_count = 0usize;
        for f in &frames {
            let gray = GrayFrame {
                width: f.width,
                height: f.height,
                pixels: &f.pixels,
            };
            tier1.push(&gray);
            tier2.push(&TimedFrame {
                timestamp_ms: f.timestamp_ms,
                frame: gray,
            });
            sum_lap += vidcull_fingerprint::laplacian_variance(&gray);
            sum_dct += vidcull_fingerprint::dct_energy(&gray);
            frame_count += 1;
        }
        let streamed =
            finish_streaming_artifacts(&metadata, tier1, tier2, sum_lap, sum_dct, frame_count)
                .unwrap();

        assert_eq!(
            batch.tier1_blob, streamed.tier1_blob,
            "§J: streaming tier1 blob must be byte-identical to batch"
        );
        assert_eq!(
            batch.tier2_blob, streamed.tier2_blob,
            "§J: streaming tier2 blob must be byte-identical to batch"
        );
        assert_eq!(
            batch.laplacian_variance, streamed.laplacian_variance,
            "§J: laplacian_variance must match"
        );
        assert_eq!(
            batch.dct_energy.map(f64::to_bits),
            streamed.dct_energy.map(f64::to_bits),
            "§J: dct_energy must be bit-identical"
        );
        assert_eq!(
            batch.bpp.map(f64::to_bits),
            streamed.bpp.map(f64::to_bits),
            "§J: bpp must be bit-identical"
        );
    }

    #[test]
    fn base_retry_reason_classifies_every_error_variant() {
        use super::base_retry_reason;
        use vidcull_parser::fallback::TIMEOUT_TOKEN;

        assert!(
            base_retry_reason(&Error::Parse("truncated stss box".into())).is_some(),
            "Parse error must be retried",
        );

        assert!(
            base_retry_reason(&Error::Unsupported("HEVC tiles unsupported".into())).is_some(),
            "untokened Unsupported must be retried",
        );

        assert!(
            base_retry_reason(&Error::Decode(
                "native stream: recoverable IDR/fetch failure after frames delivered; \
                 cannot fall back without double-folding"
                    .into()
            ))
            .is_some(),
            "untokened post-delivery Decode error must be retried",
        );

        let timeout_msg =
            format!("ffmpeg/ffprobe {TIMEOUT_TOKEN} 90.0 s — child killed and reaped");
        assert!(
            base_retry_reason(&Error::Decode(timeout_msg.clone())).is_none(),
            "TIMEOUT_TOKEN Decode error must NOT be retried (re-timeout storm guard)",
        );

        assert!(
            base_retry_reason(&Error::Unsupported(timeout_msg)).is_none(),
            "TIMEOUT_TOKEN Unsupported error must NOT be retried",
        );

        assert!(
            base_retry_reason(&Error::Cancelled).is_none(),
            "Cancelled must NEVER be retried",
        );

        assert!(base_retry_reason(&Error::Io(std::io::Error::other("disk"))).is_none());
        assert!(base_retry_reason(&Error::Busy("gate at capacity".into())).is_none());
        assert!(base_retry_reason(&Error::Database("locked".into())).is_none());
        assert!(base_retry_reason(&Error::Serialization("bad postcard".into())).is_none());
        assert!(base_retry_reason(&Error::InvalidHash("short hash".into())).is_none());
    }

    #[test]
    fn retry_wrapper_invokes_retry_closure_on_content_failure() {
        use super::retry_base_decode_on_content_failure;
        use std::path::Path;

        let err = Error::Decode("mid-stream native failure".into());
        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> =
            retry_base_decode_on_content_failure(&err, Path::new("/v/broken.mp4"), false, || {
                calls += 1;
                Ok(42)
            });
        assert_eq!(calls, 1, "retry closure must run exactly once");
        assert!(
            matches!(result, Some(Ok(42))),
            "retry success must surface: {result:?}"
        );
    }

    #[test]
    fn retry_wrapper_surfaces_retry_failure_without_looping() {
        use super::retry_base_decode_on_content_failure;
        use std::path::Path;

        let err = Error::Decode("mid-stream native failure".into());
        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> =
            retry_base_decode_on_content_failure(&err, Path::new("/v/broken.mp4"), false, || {
                calls += 1;
                Err(Error::Decode("pure-ffmpeg redecode also failed".into()))
            });
        assert_eq!(
            calls, 1,
            "retry closure must run exactly once, never retried again"
        );
        assert!(
            matches!(result, Some(Err(Error::Decode(_)))),
            "retry failure must surface as the retry's own error: {result:?}",
        );
    }

    #[test]
    fn retry_wrapper_never_retries_cancelled() {
        use super::retry_base_decode_on_content_failure;
        use std::path::Path;

        for disabled in [false, true] {
            let mut calls = 0usize;
            let result: Option<vidcull_core::Result<u32>> = retry_base_decode_on_content_failure(
                &Error::Cancelled,
                Path::new("/v/paused.mp4"),
                disabled,
                || {
                    calls += 1;
                    Ok(0)
                },
            );
            assert_eq!(calls, 0, "Cancelled must never invoke the retry closure");
            assert!(
                result.is_none(),
                "Cancelled must yield None so the caller re-surfaces it unchanged",
            );
        }
    }

    #[test]
    fn retry_wrapper_disabled_gate_skips_retry() {
        use super::retry_base_decode_on_content_failure;
        use std::path::Path;

        let err = Error::Decode("mid-stream native failure".into());
        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> =
            retry_base_decode_on_content_failure(&err, Path::new("/v/broken.mp4"), true, || {
                calls += 1;
                Ok(42)
            });
        assert_eq!(
            calls, 0,
            "disabled gate must skip the retry closure entirely"
        );
        assert!(
            result.is_none(),
            "disabled gate must yield None (caller keeps original error)"
        );
    }

    #[test]
    fn retry_wrapper_skips_non_retryable_errors() {
        use super::retry_base_decode_on_content_failure;
        use std::path::Path;

        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> = retry_base_decode_on_content_failure(
            &Error::Busy("gate at capacity".into()),
            Path::new("/v/busy.mp4"),
            false,
            || {
                calls += 1;
                Ok(0)
            },
        );
        assert_eq!(
            calls, 0,
            "a non-retryable error must not invoke the retry closure"
        );
        assert!(result.is_none());
    }

    #[test]
    fn base_retry_disable_value_parses_literal_one_only() {
        use super::base_retry_disable_value;

        assert!(
            !base_retry_disable_value(None),
            "unset must default to enabled (not disabled)"
        );
        assert!(
            base_retry_disable_value(Some("1")),
            "\"1\" must disable the retry"
        );
        assert!(
            !base_retry_disable_value(Some("true")),
            "only the literal \"1\" disables (mirrors IDLE_SINGLE_WORKER_ENV convention)",
        );
        assert!(
            !base_retry_disable_value(Some("0")),
            "\"0\" must not disable"
        );
        assert!(
            !base_retry_disable_value(Some("")),
            "empty string must not disable"
        );
    }

    #[test]
    fn retry_via_pure_ffmpeg_fallback_matches_direct_fallback_decode() {
        use super::{
            DecodeConcurrency, FfmpegBinaries, Tier1Builder, Tier2Builder,
            finish_streaming_artifacts, retry_via_pure_ffmpeg_fallback,
        };
        use vidcull_fingerprint::{GrayFrame, TimedFrame};
        use vidcull_parser::fallback::probe_fallback_cancellable;

        let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
        let budget = 16usize;
        let conc = DecodeConcurrency::serial();
        let path = parser_fixture("black_320x180_30fps_1s.mp4");
        if !path.exists() {
            eprintln!("SKIP retry_via_pure_ffmpeg_fallback: fixture missing");
            return;
        }

        let retried =
            retry_via_pure_ffmpeg_fallback(&bins, &path, budget, &conc, Cancel::default());
        let Ok((_, decode_path, _, retried_artifacts, _thumb)) = retried else {
            eprintln!("SKIP retry_via_pure_ffmpeg_fallback: ffmpeg/ffprobe unavailable");
            return;
        };
        assert_eq!(
            decode_path,
            DecodePath::Fallback,
            "the retry must always report Fallback, never Native",
        );

        let Ok(meta) = probe_fallback_cancellable(&bins, &path, Cancel::default()) else {
            eprintln!("SKIP retry_via_pure_ffmpeg_fallback: ffprobe unavailable for reference");
            return;
        };
        let duration_ms = meta.duration.unwrap().as_millis();
        let mut tier1 = Tier1Builder::new();
        let mut tier2 = Tier2Builder::new();
        let mut sum_lap = 0.0_f64;
        let mut sum_dct = 0.0_f64;
        let mut frame_count = 0usize;
        vidcull_parser::fallback::decode_sparse_strided_with_streaming(
            &bins,
            &path,
            duration_ms,
            meta.resolution.width,
            meta.resolution.height,
            budget,
            &meta.codec,
            meta.fps_x1000,
            meta.has_b_frames,
            &conc,
            Cancel::default(),
            |f| {
                let gray = GrayFrame {
                    width: f.width,
                    height: f.height,
                    pixels: &f.pixels,
                };
                tier1.push(&gray);
                tier2.push(&TimedFrame {
                    timestamp_ms: f.timestamp_ms,
                    frame: gray,
                });
                sum_lap += vidcull_fingerprint::laplacian_variance(&gray);
                sum_dct += vidcull_fingerprint::dct_energy(&gray);
                frame_count += 1;
                Ok(())
            },
        )
        .expect("reference direct-fallback decode must succeed");
        let reference_artifacts =
            finish_streaming_artifacts(&meta, tier1, tier2, sum_lap, sum_dct, frame_count)
                .expect("reference finish_streaming_artifacts must succeed");

        assert_eq!(
            retried_artifacts.tier1_blob, reference_artifacts.tier1_blob,
            "§J: retry path tier1 blob must be byte-identical to a direct pure-fallback decode",
        );
        assert_eq!(
            retried_artifacts.tier2_blob, reference_artifacts.tier2_blob,
            "§J: retry path tier2 blob must be byte-identical to a direct pure-fallback decode",
        );
    }

    #[test]
    fn probe_decode_fingerprint_streaming_disabled_gate_is_hard_fail_passthrough() {
        use super::{DecodeConcurrency, FfmpegBinaries, probe_decode_fingerprint_streaming};

        let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
        let conc = DecodeConcurrency::serial();
        let missing = Path::new("/v/does-not-exist-187.mp4");
        let result =
            probe_decode_fingerprint_streaming(&bins, missing, 16, 16, &conc, Cancel::default());
        assert!(
            result.is_err(),
            "a nonexistent path must still return Err, not panic/hang"
        );
    }

    #[test]
    #[ignore = "manual verification against a local real-world fixture"]
    fn retry_recovers_real_broken_stss_file() {
        use super::{DecodeConcurrency, FfmpegBinaries, probe_decode_fingerprint_streaming};

        let Ok(raw_path) = std::env::var("VIDCULL_TEST_REAL_FILE") else {
            eprintln!(
                "SKIP retry_recovers_real_broken_stss_file: set VIDCULL_TEST_REAL_FILE=<path>",
            );
            return;
        };
        let path = std::path::PathBuf::from(raw_path);
        assert!(
            path.exists(),
            "VIDCULL_TEST_REAL_FILE path does not exist: {}",
            path.display()
        );

        let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
        let conc = DecodeConcurrency::serial();
        let (metadata, decode_path, frame_count, artifacts) = probe_decode_fingerprint_streaming(
            &bins,
            &path,
            super::DEFAULT_DECODE_BUDGET,
            super::DEFAULT_FALLBACK_DECODE_BUDGET,
            &conc,
            Cancel::default(),
        )
        .expect(
            "base-index decode must succeed via the retry \
                 (previously hard-FAILED with no ffmpeg retry)",
        );
        eprintln!(
            "retry_recovers_real_broken_stss_file: codec={:?} decode_path={decode_path:?} \
             frames={frame_count}",
            metadata.codec,
        );
        assert!(frame_count > 0, "must decode at least one frame");
        assert!(
            !artifacts.tier1_blob.is_empty(),
            "tier1 blob must be non-empty"
        );
    }

    #[test]
    fn partial_retry_reason_mirrors_base_retry_reason_for_every_variant() {
        use super::{base_retry_reason, partial_retry_reason};
        use vidcull_parser::fallback::TIMEOUT_TOKEN;

        let timeout_msg =
            format!("ffmpeg/ffprobe {TIMEOUT_TOKEN} 90.0 s — child killed and reaped");
        let cases = [
            Error::Parse("truncated stss box".into()),
            Error::Unsupported("HEVC tiles unsupported".into()),
            Error::Decode(
                "native stream: recoverable IDR/fetch failure after frames delivered; \
                 cannot fall back without double-folding"
                    .into(),
            ),
            Error::Decode(timeout_msg.clone()),
            Error::Unsupported(timeout_msg),
            Error::Cancelled,
            Error::Io(std::io::Error::other("disk")),
            Error::Busy("gate at capacity".into()),
            Error::Database("locked".into()),
            Error::Serialization("bad postcard".into()),
            Error::InvalidHash("short hash".into()),
        ];
        for err in &cases {
            assert_eq!(
                partial_retry_reason(err).is_some(),
                base_retry_reason(err).is_some(),
                "partial_retry_reason must mirror base_retry_reason for {err:?}",
            );
        }

        assert!(
            partial_retry_reason(&Error::Parse("truncated stss box".into())).is_some(),
            "Parse must be retried on the partial path",
        );
        assert!(
            partial_retry_reason(&Error::Cancelled).is_none(),
            "Cancelled must never be retried on the partial path",
        );
    }

    #[test]
    fn partial_retry_reason_excludes_confirmed_non_fast_path_codec() {
        use super::{PARTIAL_NON_FAST_PATH_TOKEN, partial_retry_reason};

        let tokened = Error::Unsupported(format!(
            "{PARTIAL_NON_FAST_PATH_TOKEN}: codec Av1 is not fast-path eligible"
        ));
        assert!(
            partial_retry_reason(&tokened).is_none(),
            "a confirmed non-fast-path codec must never trigger the pure-ffmpeg retry \
             (would pay the exact hazardous decode the guard avoids)",
        );

        let untokened = Error::Unsupported("native HEVC: tiles not supported".into());
        assert!(
            partial_retry_reason(&untokened).is_some(),
            "an untokened Unsupported (genuine feature gap) must still retry",
        );
    }

    #[test]
    fn partial_retry_wrapper_invokes_retry_closure_on_content_failure() {
        use super::retry_partial_decode_on_content_failure;

        let err = Error::Parse("truncated stss box".into());
        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> = retry_partial_decode_on_content_failure(
            &err,
            Path::new("/v/broken-stss.mp4"),
            false,
            || {
                calls += 1;
                Ok(7)
            },
        );
        assert_eq!(calls, 1, "retry closure must run exactly once");
        assert!(
            matches!(result, Some(Ok(7))),
            "retry success must surface: {result:?}"
        );
    }

    #[test]
    fn partial_retry_wrapper_never_retries_cancelled() {
        use super::retry_partial_decode_on_content_failure;

        for disabled in [false, true] {
            let mut calls = 0usize;
            let result: Option<vidcull_core::Result<u32>> = retry_partial_decode_on_content_failure(
                &Error::Cancelled,
                Path::new("/v/paused.mp4"),
                disabled,
                || {
                    calls += 1;
                    Ok(0)
                },
            );
            assert_eq!(calls, 0, "Cancelled must never invoke the retry closure");
            assert!(
                result.is_none(),
                "Cancelled must yield None so the caller propagates it"
            );
        }
    }

    #[test]
    fn partial_retry_wrapper_skips_non_retryable_errors() {
        use super::retry_partial_decode_on_content_failure;

        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> = retry_partial_decode_on_content_failure(
            &Error::Busy("gate at capacity".into()),
            Path::new("/v/busy.mp4"),
            false,
            || {
                calls += 1;
                Ok(0)
            },
        );
        assert_eq!(
            calls, 0,
            "a non-retryable error must not invoke the retry closure"
        );
        assert!(result.is_none());
    }

    #[test]
    fn partial_retry_wrapper_disabled_gate_skips_retry() {
        use super::retry_partial_decode_on_content_failure;

        let err = Error::Parse("truncated stss box".into());
        let mut calls = 0usize;
        let result: Option<vidcull_core::Result<u32>> = retry_partial_decode_on_content_failure(
            &err,
            Path::new("/v/broken-stss.mp4"),
            true,
            || {
                calls += 1;
                Ok(7)
            },
        );
        assert_eq!(
            calls, 0,
            "disabled gate must skip the retry closure entirely"
        );
        assert!(
            result.is_none(),
            "disabled gate must yield None (caller keeps original error)"
        );
    }

    #[test]
    fn partial_retry_disable_value_parses_literal_one_only() {
        use super::base_retry_disable_value;

        assert!(
            !base_retry_disable_value(None),
            "unset must default to enabled"
        );
        assert!(
            base_retry_disable_value(Some("1")),
            "\"1\" must disable the retry"
        );
        assert!(
            !base_retry_disable_value(Some("true")),
            "only literal \"1\" disables"
        );
    }

    mod partial_phase_headroom {
        use crate::TaskHandler as _;
        use crate::indexing::{IndexingHandler, PARTIAL_PRIORITY, partial_headroom_k};
        use vidcull_db::repo::{NewTask, TaskQueueRepo};
        use vidcull_parser::fallback::FfmpegBinaries;

        const NOW: i64 = 1_700_000_000;

        fn now_fixed() -> i64 {
            NOW
        }

        fn handler_over_fresh_db() -> IndexingHandler {
            let db = vidcull_db::open_in_memory().expect("open in-memory db");
            IndexingHandler::new(
                db,
                FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()),
                now_fixed,
            )
        }

        fn enqueue(handler: &IndexingHandler, priority: i32) -> i64 {
            TaskQueueRepo::new(handler_db_conn(handler))
                .enqueue(&NewTask {
                    kind: "scan".into(),
                    priority,
                    payload: None,
                    enqueued_at: NOW,
                    size_bytes: 0,
                })
                .expect("enqueue")
        }

        fn handler_db_conn(handler: &IndexingHandler) -> &rusqlite::Connection {
            handler.worker.db.conn()
        }

        #[test]
        fn predicate_false_while_a_foreground_task_is_pending() {
            let handler = handler_over_fresh_db();
            enqueue(&handler, 0);
            enqueue(&handler, PARTIAL_PRIORITY);
            assert!(
                !handler
                    .foreground_drained_for_partial_phase()
                    .expect("predicate"),
                "a fresh (priority=0) PENDING row must keep this false"
            );
        }

        #[test]
        fn predicate_false_while_only_a_densify_task_is_pending() {
            let handler = handler_over_fresh_db();
            let densify_priority = PARTIAL_PRIORITY + 50;
            enqueue(&handler, densify_priority);
            assert!(
                !handler
                    .foreground_drained_for_partial_phase()
                    .expect("predicate"),
                "a densify-priority PENDING row (> PARTIAL_PRIORITY) must keep this false"
            );
        }

        #[test]
        fn predicate_true_while_only_partial_priority_tasks_are_pending() {
            let handler = handler_over_fresh_db();
            enqueue(&handler, PARTIAL_PRIORITY);
            enqueue(&handler, PARTIAL_PRIORITY);
            assert!(
                handler
                    .foreground_drained_for_partial_phase()
                    .expect("predicate"),
                "only PARTIAL_PRIORITY rows pending must read as foreground-drained"
            );
        }

        #[test]
        fn predicate_true_on_an_empty_queue() {
            let handler = handler_over_fresh_db();
            assert!(
                handler
                    .foreground_drained_for_partial_phase()
                    .expect("predicate"),
                "an empty queue is vacuously foreground-drained"
            );
        }

        #[test]
        fn predicate_ignores_a_foreground_task_already_claimed_running() {
            let handler = handler_over_fresh_db();
            let repo = TaskQueueRepo::new(handler_db_conn(&handler));
            enqueue(&handler, 0);
            repo.dequeue_next("scan", NOW).expect("claim to RUNNING");
            enqueue(&handler, PARTIAL_PRIORITY);
            assert!(
                handler
                    .foreground_drained_for_partial_phase()
                    .expect("predicate"),
                "a RUNNING (already-claimed) foreground task is not PENDING — \
                 predicate reads foreground-drained"
            );
        }

        #[test]
        fn set_decode_budget_shrinks_decode_conc_only_when_predicate_true() {
            let mut handler = handler_over_fresh_db();
            let budget = 10usize;
            let k = partial_headroom_k();

            enqueue(&handler, 0);
            handler.set_decode_budget(budget);
            let (_, cap) = handler.decode_conc.snapshot();
            assert_eq!(
                cap, budget,
                "foreground pending must NOT shrink decode_conc"
            );

            {
                let repo = TaskQueueRepo::new(handler_db_conn(&handler));
                let claimed = repo
                    .dequeue_next("scan", NOW)
                    .expect("claim")
                    .expect("a row");
                repo.mark_done(claimed.id, NOW)
                    .expect("resolve foreground task");
            }
            enqueue(&handler, PARTIAL_PRIORITY);
            handler.set_decode_budget(budget);
            let (_, cap) = handler.decode_conc.snapshot();
            assert_eq!(
                cap,
                budget.saturating_sub(k).max(1),
                "foreground-drained (partial-only) must shrink decode_conc by the headroom k"
            );

            enqueue(&handler, 0);
            handler.set_decode_budget(budget);
            let (_, cap) = handler.decode_conc.snapshot();
            assert_eq!(
                cap, budget,
                "a fresh foreground task must restore full decode_conc capacity \
                 on the very next burst"
            );
        }

        #[test]
        fn partial_headroom_k_pure_defaults_to_two_and_honours_a_valid_override() {
            use super::super::partial_headroom_k_from as headroom_from;
            assert_eq!(headroom_from(None), 2, "unset must default to 2");
            assert_eq!(
                headroom_from(Some("5")),
                5,
                "a valid override must take effect"
            );
            assert_eq!(
                headroom_from(Some("bogus")),
                2,
                "an unparsable value falls back to 2"
            );
            assert_eq!(headroom_from(Some("")), 2, "an empty value falls back to 2");
        }

        #[test]
        fn set_decode_budget_never_reduces_capacity_below_one() {
            let mut handler = handler_over_fresh_db();
            enqueue(&handler, PARTIAL_PRIORITY);
            handler.set_decode_budget(1);
            let (_, cap) = handler.decode_conc.snapshot();
            assert_eq!(
                cap, 1,
                "capacity must clamp to >= 1 even when k exceeds budget"
            );
        }

        #[test]
        fn set_decode_budget_rescales_seq_read_gate_with_budget() {
            // Regression test: seq_read_gate used to be fixed at construction
            // time (SEQ_READ_CONCURRENCY = 4) and never resized, which capped
            // concurrent file-hashing at 4 regardless of core count/budget —
            // cores sat idle during large-library indexing even though
            // base_decode_gate/partial_gate/decode_conc all scaled correctly.
            let mut handler = handler_over_fresh_db();

            handler.set_decode_budget(16);
            let (_, cap) = handler.seq_read_gate.snapshot();
            assert_eq!(
                cap, 16,
                "seq_read_gate must track a budget above the old fixed default of 4"
            );

            handler.set_decode_budget(2);
            let (_, cap) = handler.seq_read_gate.snapshot();
            assert_eq!(cap, 2, "seq_read_gate must also shrink back down with budget");
        }
    }

    #[cfg(test)]
    mod exact_exclusion_set_matches_is_confirmed_full_dup {
        use super::super::{
            Database, DuplicateGroupsRepo, FileId, FilesRepo, NewFile, NormalizedPath, TrustLevel,
            is_confirmed_full_dup,
        };
        use std::collections::BTreeSet;

        fn insert_file(db: &Database, path: &str) -> FileId {
            FilesRepo::new(db.conn())
                .insert(&NewFile {
                    path: NormalizedPath::new(path),
                    size_bytes: 1_024,
                    ..Default::default()
                })
                .expect("insert file")
        }

        #[test]
        fn join_exclusion_set_equals_per_file_gate_across_mixed_fixtures() {
            let db = vidcull_db::open_in_memory().expect("open db");
            let groups = DuplicateGroupsRepo::new(db.conn());

            let exact_a = insert_file(&db, "/root1/exact_a.mp4");
            let exact_b = insert_file(&db, "/root1/exact_b.mp4");
            let exact_gid = groups
                .create(TrustLevel::Exact, 0)
                .expect("create EXACT group");
            groups.add_member(exact_gid, exact_a).expect("add a");
            groups.add_member(exact_gid, exact_b).expect("add b");

            let exact_deleted = insert_file(&db, "/root2/exact_deleted.mp4");
            groups
                .add_member(exact_gid, exact_deleted)
                .expect("add deleted member");
            FilesRepo::new(db.conn())
                .mark_deleted(exact_deleted, 1)
                .expect("soft-delete member");

            let very_likely = insert_file(&db, "/root1/very_likely.mp4");
            let vl_gid = groups
                .create(TrustLevel::VeryLikely, 0)
                .expect("create VL group");
            groups
                .add_member(vl_gid, very_likely)
                .expect("add VL member");

            let possible = insert_file(&db, "/root2/possible.mp4");
            let poss_gid = groups
                .create(TrustLevel::Possible, 0)
                .expect("create POSSIBLE group");
            groups
                .add_member(poss_gid, possible)
                .expect("add POSSIBLE member");

            let dual = insert_file(&db, "/root3/dual_member.mp4");
            groups
                .add_member(exact_gid, dual)
                .expect("add dual to exact");
            groups
                .add_member(poss_gid, dual)
                .expect("add dual to possible");

            let lone = insert_file(&db, "/root3/lone.mp4");

            let all_files = [
                exact_a,
                exact_b,
                exact_deleted,
                very_likely,
                possible,
                dual,
                lone,
            ];

            let expected: BTreeSet<FileId> = all_files
                .iter()
                .copied()
                .filter(|&f| is_confirmed_full_dup(&db, f).expect("is_confirmed_full_dup"))
                .collect();

            let actual: BTreeSet<FileId> = groups
                .list_exact_group_member_ids()
                .expect("list_exact_group_member_ids")
                .into_iter()
                .collect();

            assert_eq!(
                actual, expected,
                "JOIN exclusion set must equal the is_confirmed_full_dup true-set exactly"
            );
            assert_eq!(
                expected,
                [exact_a, exact_b, exact_deleted, dual]
                    .into_iter()
                    .collect(),
                "sanity: only EXACT members qualify (incl. soft-deleted, incl. dual-membership)"
            );
        }
    }
}
