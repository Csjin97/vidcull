#![deny(unsafe_code)]
#![allow(missing_docs)]

pub mod activity;
pub mod autostart;
pub mod backup;
pub mod bridge;
pub mod delete;
pub mod diagnostics;
pub mod indexing;
pub mod logbuf;
pub mod logctl;
pub mod metrics;
pub mod migrate_native_swap;
pub mod priority;
pub mod redact;
pub mod scan_exec;
pub mod settings;
pub mod storage;
pub mod throttle;
pub mod thumbnails;
pub mod watcher;
pub mod worker_health;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use indexing::IndexingWorker;
use vidcull_core::{Error, Result};
use vidcull_db::Database;
use vidcull_db::repo::{Task, TaskQueueRepo, TaskState};

pub use autostart::Autostart;
pub use bridge::DaemonRequestHandler;
pub use delete::{DeleteMode, DeleteReject, FileRemover, OsFileRemover, plan_deletion};
pub use indexing::{
    DEFAULT_DECODE_BUDGET, DEFAULT_FALLBACK_DECODE_BUDGET, DENSIFY_PRIORITY, DecodeGateObserver,
    DecodeGateSnapshot, IndexingHandler,
};
pub use logbuf::{LogBuffer, LogBufferLayer};
pub use scan_exec::ScanExecutor;
pub use throttle::{
    Activity, RateLimiter, ThrottleConfig, ThrottleControl, cpu_throttle_cooldown,
    cpu_throttle_idle_budget,
};
pub use thumbnails::ThumbnailProvider;
pub use watcher::{
    ChangeKind, ChangeTask, Debouncer, FileWatcher, WatchConfig, WatchStats, classify_event,
    enqueue_changes, enqueue_changes_guarded, enqueue_initial_scan, enqueue_initial_scan_until,
    enqueue_partial_backfill, run_event_loop,
};

#[derive(Debug, Clone)]
pub struct ParallelWorkerConfig {
    pub db_path: PathBuf,
    pub bins: vidcull_parser::fallback::FfmpegBinaries,
    pub budget: usize,
    pub fallback_budget: usize,
    pub task_kind: String,
    pub now: fn() -> i64,
    pub metrics: Arc<vidcull_parser::fallback::FallbackMetrics>,
    pub single_flight: Arc<crate::indexing::SingleFlight>,
    pub partial_clips_enabled: bool,
    pub decode_concurrency: std::sync::Arc<vidcull_parser::fallback::DecodeConcurrency>,
    pub partial_gate: std::sync::Arc<crate::indexing::PartialDecodeGate>,
    pub base_decode_gate: std::sync::Arc<crate::indexing::BaseDecodeGate>,
    pub seq_read_gate: std::sync::Arc<crate::indexing::BaseDecodeGate>,
}

pub trait TaskHandler {
    fn handle(&mut self, task: &Task) -> Result<()>;

    #[must_use]
    fn as_parallel_worker(&self) -> Option<ParallelWorkerConfig> {
        None
    }

    fn link_shutdown(&mut self, _flag: Arc<std::sync::atomic::AtomicBool>) {}

    fn link_cancel_source(&mut self, _control: Arc<ThrottleControl>) {}

    fn set_partial_clips_live(&mut self, _enabled: bool) {}

    fn set_decode_budget(&mut self, _budget: usize) {}

    fn trailing_rebuild(&mut self) -> Result<()> {
        Ok(())
    }

    #[must_use]
    fn burst_chunk_size(&self) -> Option<usize> {
        None
    }

    fn after_burst_chunk(&mut self, processed: usize, drained: bool) -> Result<()> {
        let _ = processed;
        if drained {
            self.trailing_rebuild()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub kind: String,
    pub poll_interval: Duration,
    pub throttle: ThrottleConfig,
    pub throttle_control: Arc<ThrottleControl>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            kind: "scan".to_owned(),
            poll_interval: Duration::from_millis(250),
            throttle: ThrottleConfig::default(),
            throttle_control: Arc::new(ThrottleControl::default()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult {
    pub id: i64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunStats {
    pub recovered: usize,
    pub processed: usize,
    pub failed: usize,
}

#[derive(Clone)]
pub struct ShutdownToken {
    inner: Arc<Shared>,
}

struct Shared {
    triggered: Mutex<bool>,
    condvar: Condvar,
    notify: tokio::sync::Notify,
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl ShutdownToken {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Shared {
                triggered: Mutex::new(false),
                condvar: Condvar::new(),
                notify: tokio::sync::Notify::new(),
                flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
        }
    }

    pub fn trigger(&self) {
        self.inner
            .flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut guard = self
            .inner
            .triggered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = true;
        self.inner.condvar.notify_all();
        self.inner.notify.notify_waiters();
    }

    #[must_use]
    pub fn shutdown_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.inner.flag)
    }

    pub async fn cancelled(&self) {
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_triggered() {
            return;
        }
        notified.await;
    }

    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self
            .inner
            .triggered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn wait_timeout(&self, dur: Duration) {
        let guard = self
            .inner
            .triggered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *guard {
            return;
        }
        let (_guard, _timeout) = self
            .inner
            .condvar
            .wait_timeout(guard, dur)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

impl Default for ShutdownToken {
    fn default() -> Self {
        Self::new()
    }
}

pub const BUSY_LIVELOCK_THRESHOLD: u32 = 10;

const WORKER_IDLE_RETRY_INTERVAL: Duration = Duration::from_millis(75);

const WORKER_NO_PROGRESS_CYCLE_LIMIT: u32 = 40;

const NO_PROGRESS_DUE_HORIZON_SECS: i64 = 5;

fn busy_backoff_secs(reason: &str) -> i64 {
    if reason == crate::indexing::SEQ_READ_GATE_BUSY_REASON {
        1
    } else if reason == crate::indexing::PARTIAL_GATE_BUSY_REASON
        || reason == crate::indexing::BASE_DECODE_GATE_BUSY_REASON
    {
        3
    } else if reason == DB_LOCK_BUSY_REASON {
        2
    } else {
        30
    }
}

const DB_LOCK_BUSY_REASON: &str = "sqlite write contention";

fn normalize_db_lock(result: Result<()>) -> Result<()> {
    match result {
        Err(Error::Database(msg)) if msg.contains("is locked") || msg.contains("is busy") => {
            Err(Error::Busy(DB_LOCK_BUSY_REASON.to_owned()))
        }
        other => other,
    }
}

pub struct Daemon {
    config: DaemonConfig,
    busy_counts: Arc<Mutex<HashMap<i64, u32>>>,
}

impl Daemon {
    #[must_use]
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            busy_counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub fn busy_count_for(&self, task_id: i64) -> u32 {
        *self
            .busy_counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&task_id)
            .unwrap_or(&0)
    }

    pub fn recover(db: &Database) -> Result<usize> {
        let repo = TaskQueueRepo::new(db.conn());
        let recovered = repo.requeue_running()?;
        let now = unix_now();
        for (enqueued_at, attempts) in repo.future_enqueued_pending(now)? {
            tracing::warn!(
                gate = "GATE-159",
                delta_secs = enqueued_at - now,
                attempts,
                "recovery found a future-enqueued PENDING task"
            );
        }
        repo.reset_future_enqueued_pending(now)?;
        Ok(recovered)
    }

    pub fn step<H: TaskHandler>(
        &self,
        db: &Database,
        handler: &mut H,
        now: i64,
    ) -> Result<Option<StepResult>> {
        self.step_inner(db, handler, now, false)
    }

    #[allow(clippy::too_many_lines)]
    fn step_inner<H: TaskHandler>(
        &self,
        db: &Database,
        handler: &mut H,
        now: i64,
        prefer_partial: bool,
    ) -> Result<Option<StepResult>> {
        let repo = TaskQueueRepo::new(db.conn());
        let claimed = if prefer_partial {
            match repo.dequeue_next_at_priority(
                &self.config.kind,
                crate::indexing::PARTIAL_PRIORITY,
                now,
            )? {
                Some(task) => Some(task),
                None => repo.dequeue_next(&self.config.kind, now)?,
            }
        } else {
            repo.dequeue_next(&self.config.kind, now)?
        };
        let Some(task) = claimed else {
            return Ok(None);
        };
        if task.enqueued_at > now {
            repo.requeue_busy_task(task.id, task.enqueued_at, task.attempts - 1)?;
            return Ok(None);
        }
        let id = task.id;
        if let Some(change) = task
            .payload
            .as_deref()
            .and_then(|p| ChangeTask::from_payload(p).ok())
        {
            if change.change != ChangeKind::Remove
                && self.config.throttle_control.is_path_removed(&change.path)
            {
                repo.mark_done(id, now)?;
                return Ok(None);
            }
        }
        let outcome = match normalize_db_lock(handler.handle(&task)) {
            Ok(()) => {
                repo.mark_done(id, now)?;
                self.busy_counts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                Outcome::Done
            }
            Err(Error::Busy(reason)) => {
                let backoff_secs = busy_backoff_secs(&reason);
                repo.requeue_busy_task(id, now + backoff_secs, task.attempts - 1)?;
                {
                    let mut counts = self
                        .busy_counts
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let n = counts.entry(id).or_insert(0);
                    *n += 1;
                    if *n == BUSY_LIVELOCK_THRESHOLD {
                        tracing::warn!(
                            gate = "GATE-159-mid",
                            task_id = id,
                            attempts = task.attempts,
                            consecutive_busy = *n,
                            backoff_secs,
                            "task may be stuck in a mid-session busy-backoff livelock; \
                             no action taken — collect this log alongside any GATE-159 \
                             restart sample to determine the root-cause fix"
                        );
                    }
                }
                tracing::info!(id, reason = %reason, "task is busy; enqueued backoff retry in {}s", backoff_secs);
                return Ok(None);
            }
            Err(Error::Cancelled) => {
                let is_removal = task
                    .payload
                    .as_deref()
                    .and_then(|p| ChangeTask::from_payload(p).ok())
                    .is_some_and(|c| self.config.throttle_control.is_path_removed(&c.path));
                let ran_secs = task.started_at.map_or(-1, |s| now - s);
                self.busy_counts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                if is_removal {
                    repo.mark_done(id, now)?;
                    tracing::info!(
                        id,
                        ran_secs,
                        "in-flight decode cancelled by folder removal — task dropped"
                    );
                } else {
                    repo.requeue_busy_task(id, now, task.attempts - 1)?;
                    tracing::info!(
                        id,
                        ran_secs,
                        "indexing paused; in-flight decode cancelled — task requeued"
                    );
                }
                return Ok(None);
            }
            Err(err) => {
                let err_str = err.to_string();
                let limit = 2000;
                let trimmed_err = if err_str.len() > limit {
                    let suffix: String = err_str.chars().take(limit).collect();
                    format!("{suffix} ... (truncated)")
                } else {
                    err_str
                };
                repo.mark_failed(id, now, &trimmed_err)?;
                self.busy_counts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                Outcome::Failed
            }
        };
        Ok(Some(StepResult { id, outcome }))
    }

    pub fn run<H, C>(
        &self,
        db: &mut Database,
        handler: &mut H,
        shutdown: &ShutdownToken,
        clock: C,
    ) -> Result<RunStats>
    where
        H: TaskHandler,
        C: Fn() -> i64 + Send + Sync,
    {
        self.run_throttled(db, handler, shutdown, clock, || Activity::Idle)
    }

    #[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
    pub fn run_throttled<H, C, A>(
        &self,
        db: &mut Database,
        handler: &mut H,
        shutdown: &ShutdownToken,
        clock: C,
        activity: A,
    ) -> Result<RunStats>
    where
        H: TaskHandler,
        C: Fn() -> i64 + Send + Sync,
        A: Fn() -> Activity,
    {
        let recovered = Self::recover(&*db)?;
        handler.link_shutdown(shutdown.shutdown_flag());
        handler.link_cancel_source(Arc::clone(&self.config.throttle_control));
        let mut stats = RunStats {
            recovered,
            processed: 0,
            failed: 0,
        };
        let mut seq_claims: usize = 0;
        let idle_single_worker = idle_single_worker_forced();
        while !shutdown.is_triggered() {
            if !self.config.throttle_control.indexing_enabled() {
                if self.config.throttle_control.removed_cleanup_pending() {
                    let repo = TaskQueueRepo::new(db.conn());
                    let pending_tasks = match repo.list_by_state(TaskState::Pending) {
                        Ok(tasks) => tasks,
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "paused drain: list_by_state failed; skipping pass",
                            );
                            shutdown.wait_timeout(self.config.poll_interval);
                            continue;
                        }
                    };
                    let now_val = clock();
                    let mut drained = 0usize;
                    for task in pending_tasks {
                        if let Some(change) = task
                            .payload
                            .as_deref()
                            .and_then(|p| ChangeTask::from_payload(p).ok())
                        {
                            if self.config.throttle_control.is_path_removed(&change.path) {
                                if change.change == ChangeKind::Remove {
                                    match handler.handle(&task) {
                                        Ok(()) => {
                                            if let Err(err) = repo.mark_done(task.id, now_val) {
                                                tracing::warn!(
                                                    task_id = task.id,
                                                    error = %err,
                                                    "paused drain: mark_done failed for \
                                                     Remove task; left PENDING",
                                                );
                                            } else {
                                                drained += 1;
                                            }
                                        }
                                        Err(err) => {
                                            tracing::warn!(
                                                task_id = task.id,
                                                error = %err,
                                                "paused drain: Remove handler failed; \
                                                 task left PENDING for retry",
                                            );
                                        }
                                    }
                                } else if let Err(err) = repo.mark_done(task.id, now_val) {
                                    tracing::warn!(
                                        task_id = task.id,
                                        error = %err,
                                        "paused drain: mark_done failed for stale task; \
                                         left PENDING",
                                    );
                                } else {
                                    drained += 1;
                                }
                            }
                        }
                    }
                    if drained == 0 {
                        self.config.throttle_control.clear_removed_cleanup_pending();
                    }
                }
                shutdown.wait_timeout(self.config.poll_interval);
                continue;
            }
            handler.set_partial_clips_live(self.config.throttle_control.partial_clips_enabled());
            let is_full = self.config.throttle_control.is_max_performance();
            let current_activity = activity();
            let cores = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1);
            let idle_budget = self
                .config
                .throttle_control
                .idle_workers_override()
                .unwrap_or_else(|| {
                    cpu_throttle_idle_budget(
                        self.config.throttle_control.level(),
                        self.config.throttle.idle_workers,
                        cores,
                    )
                })
                .max(1);
            let budget = if is_full {
                idle_budget
            } else {
                match current_activity {
                    Activity::Idle => idle_budget,
                    Activity::UserActive => {
                        self.config.throttle.worker_budget(Activity::UserActive)
                    }
                }
            };
            let budget = self
                .config
                .throttle_control
                .io_budget_cap()
                .map_or(budget, |cap| crate::storage::clamp_budget(budget, cap));

            handler.set_decode_budget(budget.max(1));

            if should_spawn_parallel_workers(is_full, current_activity, budget, idle_single_worker)
            {
                if let Some(worker_config) = handler.as_parallel_worker() {
                    let queue_repo = TaskQueueRepo::new(db.conn());
                    let has_pending = queue_repo.count_by_state(TaskState::Pending)? > 0;

                    if has_pending {
                        let processed_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let failed_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let error_signal = Arc::new(Mutex::new(None));
                        let burst_chunk = handler.burst_chunk_size();
                        let per_worker_cap = burst_chunk.map(|chunk| chunk.div_ceil(budget).max(1));
                        let queue_exhausted = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let yield_now = Arc::new(std::sync::atomic::AtomicBool::new(false));

                        let clock_ref = &clock;
                        let kind_ref = &self.config.kind;
                        let throttle_for_cancel = Arc::clone(&self.config.throttle_control);
                        let busy_counts_ref = Arc::clone(&self.busy_counts);

                        std::thread::scope(|s| {
                            for worker_idx in 0..budget {
                                let worker_config = worker_config.clone();
                                let processed = Arc::clone(&processed_counter);
                                let failed = Arc::clone(&failed_counter);
                                let error_signal = Arc::clone(&error_signal);
                                let queue_exhausted = Arc::clone(&queue_exhausted);
                                let yield_now = Arc::clone(&yield_now);
                                let cancel_control = Arc::clone(&throttle_for_cancel);
                                let busy_counts_arc = Arc::clone(&busy_counts_ref);

                                s.spawn(move || {
                                    let mut local_done = 0usize;
                                    let mut no_progress_cycles: u32 = 0;
                                    let w_queue_db = match vidcull_db::open_file(&worker_config.db_path)
                                    {
                                        Ok(d) => d,
                                        Err(e) => {
                                            let mut guard = error_signal.lock().unwrap_or_else(PoisonError::into_inner);
                                            if guard.is_none() {
                                                *guard = Some(e);
                                            }
                                            return;
                                        }
                                    };
                                    let w_indexing_db =
                                        match vidcull_db::open_file(&worker_config.db_path) {
                                            Ok(d) => d,
                                            Err(e) => {
                                                let mut guard = error_signal.lock().unwrap_or_else(PoisonError::into_inner);
                                                if guard.is_none() {
                                                    *guard = Some(e);
                                                }
                                                return;
                                            }
                                        };

                                    let mut worker = IndexingWorker::new(
                                        w_indexing_db,
                                        worker_config.bins.clone(),
                                        worker_config.budget,
                                        worker_config.fallback_budget,
                                        worker_config.task_kind.clone(),
                                        worker_config.now,
                                        Arc::clone(&worker_config.metrics),
                                        Arc::clone(&worker_config.single_flight),
                                    );
                                    worker.set_decode_concurrency(
                                        Arc::clone(&worker_config.decode_concurrency),
                                    );
                                    worker.set_partial_clips_enabled(
                                        worker_config.partial_clips_enabled,
                                    );
                                    worker.set_partial_gate(Arc::clone(
                                        &worker_config.partial_gate,
                                    ));
                                    worker.set_base_decode_gate(Arc::clone(
                                        &worker_config.base_decode_gate,
                                    ));
                                    worker.set_seq_read_gate(Arc::clone(
                                        &worker_config.seq_read_gate,
                                    ));
                                    worker.set_cancel_source(Arc::clone(&cancel_control));

                                    tracing::debug!(
                                        stage = "worker_lifecycle",
                                        worker_idx,
                                        "parallel worker started",
                                    );
                                    let check_burst_cadence = || {
                                        if let Some(target) = burst_chunk {
                                            let done = processed
                                                .load(std::sync::atomic::Ordering::Relaxed)
                                                + failed
                                                    .load(std::sync::atomic::Ordering::Relaxed);
                                            if done >= target {
                                                yield_now.store(
                                                    true,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                            }
                                        }
                                    };

                                    while !shutdown.is_triggered() {
                                        if !cancel_control.indexing_enabled() {
                                            tracing::debug!(
                                                stage = "worker_lifecycle",
                                                worker_idx,
                                                local_done,
                                                reason = "pause",
                                                "parallel worker exiting",
                                            );
                                            break;
                                        }
                                        if yield_now.load(std::sync::atomic::Ordering::Relaxed) {
                                            tracing::debug!(
                                                stage = "worker_lifecycle",
                                                worker_idx,
                                                local_done,
                                                reason = "burst_cadence",
                                                "parallel worker exiting",
                                            );
                                            break;
                                        }
                                        if per_worker_cap.is_some_and(|cap| local_done >= cap) {
                                            tracing::debug!(
                                                stage = "worker_lifecycle",
                                                worker_idx,
                                                local_done,
                                                reason = "chunk_cap",
                                                "parallel worker exiting",
                                            );
                                            break;
                                        }
                                        let repo = TaskQueueRepo::new(w_queue_db.conn());
                                        let now_val = clock_ref();
                                        let record_no_progress_cycle = |cycles: &mut u32| -> bool {
                                            let due_soon = repo
                                                .min_pending_enqueued_at(kind_ref)
                                                .ok()
                                                .flatten()
                                                .is_some_and(|min_at| {
                                                    min_at <= now_val + NO_PROGRESS_DUE_HORIZON_SECS
                                                });
                                            if due_soon {
                                                *cycles = 0;
                                                false
                                            } else {
                                                *cycles += 1;
                                                *cycles >= WORKER_NO_PROGRESS_CYCLE_LIMIT
                                            }
                                        };
                                        let partial_gate_open =
                                            worker_config.partial_gate.has_capacity();
                                        let prefer_partial = partial_gate_open
                                            && local_done
                                                % crate::indexing::PARTIAL_CADENCE
                                                == 0;
                                        let preferred = if prefer_partial {
                                            match repo.dequeue_next_at_priority(
                                                kind_ref,
                                                crate::indexing::PARTIAL_PRIORITY,
                                                now_val,
                                            ) {
                                                Ok(found) => found,
                                                Err(e) => {
                                                    let mut guard = error_signal.lock().unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(e);
                                                    }
                                                    tracing::debug!(
                                                        stage = "worker_lifecycle",
                                                        worker_idx,
                                                        local_done,
                                                        reason = "error",
                                                        "parallel worker exiting",
                                                    );
                                                    break;
                                                }
                                            }
                                        } else {
                                            None
                                        };
                                        let task = match preferred {
                                            Some(t) => t,
                                            None => match if partial_gate_open {
                                                repo.dequeue_next(kind_ref, now_val)
                                            } else {
                                                repo.dequeue_next_above_priority(
                                                    kind_ref,
                                                    crate::indexing::PARTIAL_PRIORITY,
                                                    now_val,
                                                )
                                            } {
                                                Ok(Some(t)) => t,
                                                Ok(None) => {
                                                    match repo.count_pending_by_kind(kind_ref) {
                                                        Ok(0) => {
                                                            queue_exhausted.store(
                                                                true,
                                                                std::sync::atomic::Ordering::Relaxed,
                                                            );
                                                            tracing::debug!(
                                                                stage = "worker_lifecycle",
                                                                worker_idx,
                                                                local_done,
                                                                reason = "kind_drained",
                                                                "parallel worker exiting",
                                                            );
                                                            break;
                                                        }
                                                        Ok(_) => {
                                                            if record_no_progress_cycle(&mut no_progress_cycles) {
                                                                tracing::debug!(
                                                                    stage = "worker_lifecycle",
                                                                    worker_idx,
                                                                    local_done,
                                                                    no_progress_cycles,
                                                                    reason = "no_progress",
                                                                    "parallel worker exiting",
                                                                );
                                                                break;
                                                            }
                                                            shutdown.wait_timeout(
                                                                WORKER_IDLE_RETRY_INTERVAL,
                                                            );
                                                            continue;
                                                        }
                                                        Err(e) => {
                                                            let mut guard = error_signal
                                                                .lock()
                                                                .unwrap_or_else(PoisonError::into_inner);
                                                            if guard.is_none() {
                                                                *guard = Some(e);
                                                            }
                                                            tracing::debug!(
                                                                stage = "worker_lifecycle",
                                                                worker_idx,
                                                                local_done,
                                                                reason = "error",
                                                                "parallel worker exiting",
                                                            );
                                                            break;
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    let mut guard = error_signal.lock().unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(e);
                                                    }
                                                    tracing::debug!(
                                                        stage = "worker_lifecycle",
                                                        worker_idx,
                                                        local_done,
                                                        reason = "error",
                                                        "parallel worker exiting",
                                                    );
                                                    break;
                                                }
                                            },
                                        };

                                        if task.enqueued_at > now_val {
                                            if let Err(e) = repo.requeue_busy_task(
                                                task.id,
                                                task.enqueued_at,
                                                task.attempts - 1,
                                            ) {
                                                let mut guard = error_signal
                                                    .lock()
                                                    .unwrap_or_else(PoisonError::into_inner);
                                                if guard.is_none() {
                                                    *guard = Some(e);
                                                }
                                                tracing::debug!(
                                                    stage = "worker_lifecycle",
                                                    worker_idx,
                                                    local_done,
                                                    reason = "error",
                                                    "parallel worker exiting",
                                                );
                                                break;
                                            }
                                            if record_no_progress_cycle(&mut no_progress_cycles) {
                                                tracing::debug!(
                                                    stage = "worker_lifecycle",
                                                    worker_idx,
                                                    local_done,
                                                    no_progress_cycles,
                                                    reason = "no_progress",
                                                    "parallel worker exiting",
                                                );
                                                break;
                                            }
                                            shutdown.wait_timeout(WORKER_IDLE_RETRY_INTERVAL);
                                            continue;
                                        }

                                        local_done += 1;
                                        no_progress_cycles = 0;

                                        let id = task.id;
                                        let payload =
                                            match task.payload.as_deref().ok_or_else(|| {
                                                Error::Unsupported(format!(
                                                    "indexing task {id} has no change payload"
                                                ))
                                            }) {
                                                Ok(p) => p,
                                                Err(e) => {
                                                    if let Err(mark_err) =
                                                        repo.mark_failed(id, now_val, &e.to_string())
                                                    {
                                                        let mut guard = error_signal
                                                            .lock()
                                                            .unwrap_or_else(PoisonError::into_inner);
                                                        if guard.is_none() {
                                                            *guard = Some(mark_err);
                                                        }
                                                        tracing::debug!(
                                                            stage = "worker_lifecycle",
                                                            worker_idx,
                                                            local_done,
                                                            reason = "error",
                                                            "parallel worker exiting",
                                                        );
                                                        break;
                                                    }
                                                    failed.fetch_add(
                                                        1,
                                                        std::sync::atomic::Ordering::Relaxed,
                                                    );
                                                    check_burst_cadence();
                                                    continue;
                                                }
                                            };

                                        let change = match ChangeTask::from_payload(payload) {
                                            Ok(c) => c,
                                            Err(e) => {
                                                if let Err(mark_err) =
                                                    repo.mark_failed(id, now_val, &e.to_string())
                                                {
                                                    let mut guard = error_signal
                                                        .lock()
                                                        .unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(mark_err);
                                                    }
                                                    tracing::debug!(
                                                        stage = "worker_lifecycle",
                                                        worker_idx,
                                                        local_done,
                                                        reason = "error",
                                                        "parallel worker exiting",
                                                    );
                                                    break;
                                                }
                                                failed.fetch_add(
                                                    1,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                                check_burst_cadence();
                                                continue;
                                            }
                                        };

                                        if change.change != ChangeKind::Remove
                                            && cancel_control.is_path_removed(&change.path)
                                        {
                                            if let Err(e) = repo.mark_done(id, now_val) {
                                                let mut guard = error_signal
                                                    .lock()
                                                    .unwrap_or_else(PoisonError::into_inner);
                                                if guard.is_none() {
                                                    *guard = Some(e);
                                                }
                                                tracing::debug!(
                                                    stage = "worker_lifecycle",
                                                    worker_idx,
                                                    local_done,
                                                    reason = "error",
                                                    "parallel worker exiting",
                                                );
                                                break;
                                            }
                                            continue;
                                        }

                                        let task_start =
                                            std::time::Instant::now();
                                        match normalize_db_lock(worker.handle_change(&change, id)) {
                                            Ok(()) => {
                                                if let Err(e) = repo.mark_done(id, now_val) {
                                                    let mut guard = error_signal.lock().unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(e);
                                                    }
                                                    tracing::debug!(
                                                        stage = "worker_lifecycle",
                                                        worker_idx,
                                                        local_done,
                                                        reason = "error",
                                                        "parallel worker exiting",
                                                    );
                                                    break;
                                                }
                                                busy_counts_arc.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                                                processed.fetch_add(
                                                    1,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                                check_burst_cadence();
                                            }
                                            Err(Error::Busy(reason)) => {
                                                let backoff_secs = busy_backoff_secs(&reason);
                                                if let Err(e) = repo.requeue_busy_task(
                                                    id,
                                                    now_val + backoff_secs,
                                                    task.attempts - 1,
                                                ) {
                                                    let mut guard = error_signal
                                                        .lock()
                                                        .unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(e);
                                                    }
                                                    tracing::debug!(
                                                        stage = "worker_lifecycle",
                                                        worker_idx,
                                                        local_done,
                                                        reason = "error",
                                                        "parallel worker exiting",
                                                    );
                                                    break;
                                                }
                                                {
                                                    let mut counts = busy_counts_arc
                                                        .lock()
                                                        .unwrap_or_else(PoisonError::into_inner);
                                                    let n = counts.entry(id).or_insert(0);
                                                    *n += 1;
                                                    if *n == BUSY_LIVELOCK_THRESHOLD {
                                                        tracing::warn!(
                                                            gate = "GATE-159-mid",
                                                            task_id = id,
                                                            attempts = task.attempts,
                                                            consecutive_busy = *n,
                                                            backoff_secs,
                                                            "task may be stuck in a mid-session \
                                                             busy-backoff livelock; no action taken \
                                                             — collect this log alongside any \
                                                             GATE-159 restart sample to determine \
                                                             the root-cause fix"
                                                        );
                                                    }
                                                }
                                                tracing::info!(id, reason = %reason, "task is busy; enqueued backoff retry in {}s", backoff_secs);
                                                local_done = local_done.saturating_sub(1);
                                                continue;
                                            }
                                            Err(Error::Cancelled) => {
                                                let ran_secs = i64::try_from(
                                                    task_start.elapsed().as_secs(),
                                                )
                                                .unwrap_or(i64::MAX);
                                                busy_counts_arc.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                                                let lifecycle_reason = if cancel_control.is_path_removed(&change.path) {
                                                    if let Err(e) = repo.mark_done(id, now_val) {
                                                        let mut guard = error_signal
                                                            .lock()
                                                            .unwrap_or_else(PoisonError::into_inner);
                                                        if guard.is_none() {
                                                            *guard = Some(e);
                                                        }
                                                        "error"
                                                    } else {
                                                        tracing::info!(id, ran_secs, "in-flight decode cancelled by folder removal — task dropped");
                                                        "folder_removed"
                                                    }
                                                } else if let Err(e) =
                                                    repo.requeue_busy_task(id, now_val, task.attempts - 1)
                                                {
                                                    let mut guard = error_signal
                                                        .lock()
                                                        .unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(e);
                                                    }
                                                    "error"
                                                } else {
                                                    tracing::info!(id, ran_secs, "indexing paused; in-flight decode cancelled — task requeued");
                                                    "pause"
                                                };
                                                tracing::debug!(
                                                    stage = "worker_lifecycle",
                                                    worker_idx,
                                                    local_done,
                                                    reason = lifecycle_reason,
                                                    "parallel worker exiting",
                                                );
                                                break;
                                            }
                                            Err(e) => {
                                                let err_str = e.to_string();
                                                let limit = 2000;
                                                let trimmed_err = if err_str.len() > limit {
                                                    let suffix: String =
                                                        err_str.chars().take(limit).collect();
                                                    format!("{suffix} ... (truncated)")
                                                } else {
                                                    err_str
                                                };
                                                if let Err(e) =
                                                    repo.mark_failed(id, now_val, &trimmed_err)
                                                {
                                                    let mut guard = error_signal.lock().unwrap_or_else(PoisonError::into_inner);
                                                    if guard.is_none() {
                                                        *guard = Some(e);
                                                    }
                                                    tracing::debug!(
                                                        stage = "worker_lifecycle",
                                                        worker_idx,
                                                        local_done,
                                                        reason = "error",
                                                        "parallel worker exiting",
                                                    );
                                                    break;
                                                }
                                                busy_counts_arc.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                                                failed.fetch_add(
                                                    1,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                                check_burst_cadence();
                                            }
                                        }
                                    }
                                });
                            }
                        });

                        if let Some(err) = error_signal
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            return Err(err);
                        }

                        let processed_this_chunk =
                            processed_counter.load(std::sync::atomic::Ordering::Relaxed);
                        let failed_this_chunk =
                            failed_counter.load(std::sync::atomic::Ordering::Relaxed);
                        stats.processed += processed_this_chunk;
                        stats.failed += failed_this_chunk;

                        let drained = queue_exhausted.load(std::sync::atomic::Ordering::Relaxed);
                        handler.after_burst_chunk(processed_this_chunk, drained)?;

                        if processed_this_chunk == 0 && failed_this_chunk == 0 {
                            shutdown.wait_timeout(self.config.poll_interval);
                        }
                    } else {
                        shutdown.wait_timeout(self.config.poll_interval);
                    }
                    continue;
                }
            }

            let prefer_partial = seq_claims % crate::indexing::PARTIAL_CADENCE == 0;
            seq_claims = seq_claims.wrapping_add(1);
            match self.step_inner(&*db, handler, clock(), prefer_partial)? {
                Some(result) => {
                    match result.outcome {
                        Outcome::Done => stats.processed += 1,
                        Outcome::Failed => stats.failed += 1,
                    }
                    let cooldown = self
                        .config
                        .throttle_control
                        .effective_cooldown(self.config.throttle.cooldown(current_activity));
                    if !cooldown.is_zero() {
                        shutdown.wait_timeout(cooldown);
                    }
                }
                None => shutdown.wait_timeout(self.config.poll_interval),
            }
        }
        Ok(stats)
    }

    pub async fn run_async<H, C>(
        self,
        db: Database,
        handler: H,
        shutdown: ShutdownToken,
        clock: C,
    ) -> Result<RunStats>
    where
        H: TaskHandler + Send + 'static,
        C: Fn() -> i64 + Send + Sync + 'static,
    {
        self.run_async_throttled(db, handler, shutdown, clock, || Activity::Idle)
            .await
    }

    pub async fn run_async_throttled<H, C, A>(
        self,
        mut db: Database,
        mut handler: H,
        shutdown: ShutdownToken,
        clock: C,
        activity: A,
    ) -> Result<RunStats>
    where
        H: TaskHandler + Send + 'static,
        C: Fn() -> i64 + Send + Sync + 'static,
        A: Fn() -> Activity + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            self.run_throttled(&mut db, &mut handler, &shutdown, clock, activity)
        })
        .await
        .map_err(|err| Error::Database(format!("daemon worker task panicked: {err}")))?
    }
}

pub fn install_signal_handlers(shutdown: ShutdownToken) {
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown signal received; draining in-flight task then stopping");
        shutdown.trigger();
    });
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    res = tokio::signal::ctrl_c() => {
                        if let Err(err) = res {
                            tracing::warn!(error = %err, "error awaiting Ctrl+C");
                        }
                    }
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to install SIGTERM handler; Ctrl+C only");
                if let Err(err) = tokio::signal::ctrl_c().await {
                    tracing::warn!(error = %err, "error awaiting Ctrl+C");
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "error awaiting Ctrl+C");
        }
    }
}

#[must_use]
pub fn build_stamp() -> &'static str {
    env!("VIDCULL_BUILD_STAMP")
}

#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn should_spawn_parallel_workers(
    is_full: bool,
    activity: Activity,
    budget: usize,
    idle_single_worker: bool,
) -> bool {
    if budget <= 1 {
        return false;
    }
    match activity {
        Activity::UserActive => is_full,
        Activity::Idle => is_full || !idle_single_worker,
    }
}

const IDLE_SINGLE_WORKER_ENV: &str = "VIDCULL_IDLE_SINGLE_WORKER";
fn idle_single_worker_forced() -> bool {
    std::env::var(IDLE_SINGLE_WORKER_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn normalize_db_lock_maps_sqlite_busy_to_retryable_busy() {
        let locked = normalize_db_lock(Err(Error::Database("database is locked".into())));
        assert!(
            matches!(locked, Err(Error::Busy(ref r)) if r == DB_LOCK_BUSY_REASON),
            "SQLITE_BUSY must normalize to a retryable Error::Busy, got {locked:?}"
        );
        let table_locked =
            normalize_db_lock(Err(Error::Database("database table is locked".into())));
        assert!(matches!(table_locked, Err(Error::Busy(_))));
    }

    #[test]
    fn normalize_db_lock_passes_through_real_faults_and_ok() {
        let constraint = normalize_db_lock(Err(Error::Database("UNIQUE constraint failed".into())));
        assert!(
            matches!(constraint, Err(Error::Database(_))),
            "a non-lock DB fault must NOT be reclassified as retryable, got {constraint:?}"
        );
        assert!(matches!(normalize_db_lock(Ok(())), Ok(())));
        assert!(matches!(
            normalize_db_lock(Err(Error::Busy("gate".into()))),
            Err(Error::Busy(_))
        ));
    }

    #[test]
    fn db_lock_backoff_is_short_not_the_locked_file_fallback() {
        assert_eq!(busy_backoff_secs(DB_LOCK_BUSY_REASON), 2);
        assert_eq!(busy_backoff_secs("some unknown reason"), 30);
    }

    #[test]
    fn shutdown_token_starts_untriggered() {
        let token = ShutdownToken::new();
        assert!(!token.is_triggered());
    }

    #[test]
    fn shutdown_token_trigger_sets_flag() {
        let token = ShutdownToken::new();
        token.trigger();
        assert!(token.is_triggered());
    }

    #[test]
    fn shutdown_token_wait_timeout_returns_immediately_when_triggered() {
        let token = ShutdownToken::new();
        token.trigger();
        let start = Instant::now();
        token.wait_timeout(Duration::from_millis(500));
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn shutdown_token_wait_timeout_returns_after_duration_when_not_triggered() {
        let token = ShutdownToken::new();
        let start = Instant::now();
        token.wait_timeout(Duration::from_millis(100));
        assert!(start.elapsed() >= Duration::from_millis(70));
    }

    #[test]
    fn test_unix_now() {
        let now = unix_now();
        assert!(now > 0);
    }

    #[test]
    fn build_stamp_matches_expected_shape() {
        let stamp = build_stamp();
        assert!(!stamp.is_empty());

        let mut parts = stamp.split_whitespace();
        let sha_part = parts.next().expect("stamp has a sha token");
        let epoch_part = parts.next().expect("stamp has an epoch token");
        assert!(parts.next().is_none(), "stamp has exactly two tokens");

        let sha = sha_part.strip_suffix("-dirty").unwrap_or(sha_part);
        let sha_ok =
            sha == "unknown" || (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(sha_ok, "unexpected sha token: {sha_part}");

        assert!(
            !epoch_part.is_empty() && epoch_part.chars().all(|c| c.is_ascii_digit()),
            "unexpected epoch token: {epoch_part}"
        );
    }

    #[test]
    fn parallel_worker_gate_holds_89_invariants() {
        use super::Activity::{Idle, UserActive};
        let spawn = super::should_spawn_parallel_workers;
        assert!(!spawn(false, UserActive, 8, false));
        assert!(!spawn(false, UserActive, 2, true));
        assert!(spawn(false, Idle, 4, false));
        assert!(!spawn(false, Idle, 4, true));
        assert!(!spawn(false, Idle, 1, false));
        assert!(spawn(true, UserActive, 2, false));
        assert!(spawn(true, Idle, 2, true));
        assert!(!spawn(true, Idle, 1, false));
    }

    #[test]
    fn gate_159_mid_busy_count_accumulates_and_thresholds_once() {
        use vidcull_core::Error;
        use vidcull_db::repo::{NewTask, TaskQueueRepo};

        const BASE: i64 = 1_700_000_000;
        const STEP: i64 = 31;

        struct AlwaysBusy;
        impl TaskHandler for AlwaysBusy {
            fn handle(&mut self, _task: &vidcull_db::repo::Task) -> vidcull_core::Result<()> {
                Err(Error::Busy("synthetic-gate-busy".to_owned()))
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let db = vidcull_db::open_file(&dir.path().join("gate159.db")).expect("open db");
        let repo = TaskQueueRepo::new(db.conn());
        let task_id = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: None,
                enqueued_at: BASE,
                size_bytes: 0,
            })
            .expect("enqueue");

        let daemon = Daemon::new(DaemonConfig {
            kind: "scan".to_owned(),
            poll_interval: Duration::from_millis(5),
            ..DaemonConfig::default()
        });
        let mut handler = AlwaysBusy;

        for step in 0..BUSY_LIVELOCK_THRESHOLD {
            let now = BASE + i64::from(step) * STEP;
            let res = daemon.step(&db, &mut handler, now).expect("step");
            assert!(res.is_none(), "busy step should return None");
            assert_eq!(
                daemon.busy_count_for(task_id),
                step + 1,
                "count should be {} after step {}",
                step + 1,
                step,
            );
        }

        let now_extra = BASE + i64::from(BUSY_LIVELOCK_THRESHOLD) * STEP;
        let res = daemon
            .step(&db, &mut handler, now_extra)
            .expect("extra step");
        assert!(res.is_none());
        assert_eq!(
            daemon.busy_count_for(task_id),
            BUSY_LIVELOCK_THRESHOLD + 1,
            "count should be threshold+1 after one extra step (no second WARN)",
        );
    }

    #[test]
    fn seq_read_gate_busy_takes_the_short_gate_backoff() {
        use vidcull_core::Error;
        use vidcull_db::repo::{NewTask, TaskQueueRepo};

        const BASE: i64 = 1_700_000_000;

        struct SeqReadBusy;
        impl TaskHandler for SeqReadBusy {
            fn handle(&mut self, _task: &vidcull_db::repo::Task) -> vidcull_core::Result<()> {
                Err(Error::Busy(
                    crate::indexing::SEQ_READ_GATE_BUSY_REASON.to_owned(),
                ))
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let db = vidcull_db::open_file(&dir.path().join("seqread-backoff.db")).expect("open db");
        let repo = TaskQueueRepo::new(db.conn());
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: 0,
            payload: None,
            enqueued_at: BASE,
            size_bytes: 0,
        })
        .expect("enqueue");

        let daemon = Daemon::new(DaemonConfig {
            kind: "scan".to_owned(),
            poll_interval: Duration::from_millis(5),
            ..DaemonConfig::default()
        });
        let mut handler = SeqReadBusy;

        let res = daemon.step(&db, &mut handler, BASE).expect("busy step");
        assert!(res.is_none(), "busy step should return None");

        assert!(
            repo.dequeue_next("scan", BASE)
                .expect("dequeue at +0s")
                .is_none(),
            "must not be claimable immediately after a busy-requeue"
        );
        let claimed = repo.dequeue_next("scan", BASE + 1).expect("dequeue at +1s");
        assert!(
            claimed.is_some(),
            "seq-read gate Busy must requeue on the shortened 1s gate backoff \
, not the old 3s value or the 30s locked-file one",
        );
    }

    #[test]
    fn busy_backoff_secs_only_seq_read_reason_changed() {
        assert_eq!(
            busy_backoff_secs(crate::indexing::SEQ_READ_GATE_BUSY_REASON),
            1,
            "seq-read gate reason must use the shortened -1a-lite backoff"
        );
        assert_eq!(
            busy_backoff_secs(crate::indexing::PARTIAL_GATE_BUSY_REASON),
            3,
            "partial-decode gate backoff must be unchanged by -1a-lite"
        );
        assert_eq!(
            busy_backoff_secs(crate::indexing::BASE_DECODE_GATE_BUSY_REASON),
            3,
            "base-decode gate backoff must be unchanged by -1a-lite"
        );
        assert_eq!(
            busy_backoff_secs("some genuinely-locked-file reason, not a gate"),
            30,
            "the unlisted (locked-file) fallback must be unchanged by -1a-lite"
        );
    }

    #[test]
    fn step_propagates_mark_done_failure_on_cancelled_removal_instead_of_swallowing_it() {
        use vidcull_core::Error;
        use vidcull_core::types::NormalizedPath;
        use vidcull_db::repo::{NewTask, Task, TaskQueueRepo};

        // 데코드 도중에 폴더가 삭제된 상황을 흉내낸다: dequeue 시점에는 아직
        // is_path_removed가 false라서 step_inner의 사전 스킵 분기를 타지 않고
        // handler.handle()까지 도달해야 하며, handle() 안에서 비로소 removed로
        // 표시하고 테이블을 지운 뒤 Cancelled를 반환해야 Cancelled 분기의
        // mark_done 실패 경로를 재현할 수 있다.
        struct CancelViaMidFlightRemoval<'a> {
            conn: &'a rusqlite::Connection,
            throttle_control: Arc<ThrottleControl>,
            removed_root: NormalizedPath,
        }

        impl TaskHandler for CancelViaMidFlightRemoval<'_> {
            fn handle(&mut self, _task: &Task) -> vidcull_core::Result<()> {
                self.throttle_control
                    .mark_roots_removed(std::slice::from_ref(&self.removed_root));
                self.conn
                    .execute("DROP TABLE task_queue", [])
                    .expect("drop task_queue to force a mark_done fault");
                Err(Error::Cancelled)
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let db =
            vidcull_db::open_file(&dir.path().join("cancel-removal-fault.db")).expect("open db");
        let repo = TaskQueueRepo::new(db.conn());

        let removed_root = NormalizedPath::new("C:/removed-root");
        let change = crate::watcher::ChangeTask {
            path: NormalizedPath::new("C:/removed-root/clip.mp4"),
            change: crate::watcher::ChangeKind::Upsert,
            size_bytes: 0,
        };
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: 0,
            payload: Some(change.to_payload().expect("encode change")),
            enqueued_at: 1_700_000_000,
            size_bytes: 0,
        })
        .expect("enqueue upsert task");

        let throttle_control = Arc::new(ThrottleControl::default());
        let daemon = Daemon::new(DaemonConfig {
            kind: "scan".to_owned(),
            poll_interval: Duration::from_millis(5),
            throttle_control: Arc::clone(&throttle_control),
            ..DaemonConfig::default()
        });
        let mut handler = CancelViaMidFlightRemoval {
            conn: db.conn(),
            throttle_control,
            removed_root,
        };

        let res = daemon.step(&db, &mut handler, 1_700_000_100);
        assert!(
            res.is_err(),
            "mark_done failure on the cancelled+folder-removed path must propagate \
             as an error, not be silently swallowed and return Ok(None): {res:?}"
        );
    }

    #[test]
    fn parallel_workers_survive_short_gate_contention_without_no_progress_exit() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::time::{Duration as StdDuration, Instant};
        use tracing_subscriber::layer::SubscriberExt;
        use vidcull_db::repo::{NewTask, TaskQueueRepo};
        use vidcull_parser::fallback::{DecodeConcurrency, FallbackMetrics, FfmpegBinaries};

        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("contended.mp4");
        std::fs::write(&file_path, b"placeholder").expect("create file");

        let db_path = dir.path().join("no_progress_horizon.db");
        {
            let setup_db = vidcull_db::open_file(&db_path).expect("open setup db");
            let repo = TaskQueueRepo::new(setup_db.conn());
            let change = crate::watcher::ChangeTask {
                path: vidcull_core::NormalizedPath::new(&file_path),
                change: crate::watcher::ChangeKind::Upsert,
                size_bytes: 0,
            };
            repo.enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(change.to_payload().expect("encode change")),
                enqueued_at: 1_700_000_000,
                size_bytes: 0,
            })
            .expect("enqueue upsert task");
        }

        let log_buffer = crate::logbuf::LogBuffer::new(65536);
        let subscriber = tracing_subscriber::registry().with(log_buffer.layer());
        let _ = tracing::subscriber::set_global_default(subscriber);

        let seq_gate = Arc::new(crate::indexing::BaseDecodeGate::new(1));
        let held_permit = seq_gate
            .try_acquire()
            .expect("acquire the only seq_read_gate permit");

        let config = ParallelWorkerConfig {
            db_path: db_path.clone(),
            bins: FfmpegBinaries::new(
                PathBuf::from("nonexistent-ffmpeg"),
                PathBuf::from("nonexistent-ffprobe"),
            ),
            budget: DEFAULT_DECODE_BUDGET,
            fallback_budget: DEFAULT_FALLBACK_DECODE_BUDGET,
            task_kind: "scan".to_owned(),
            now: unix_now,
            metrics: Arc::new(FallbackMetrics::default()),
            single_flight: Arc::new(crate::indexing::SingleFlight::default()),
            partial_clips_enabled: false,
            decode_concurrency: Arc::new(DecodeConcurrency::new(4)),
            partial_gate: Arc::new(crate::indexing::PartialDecodeGate::new(1)),
            base_decode_gate: Arc::new(crate::indexing::BaseDecodeGate::new(4)),
            seq_read_gate: Arc::clone(&seq_gate),
        };

        struct ParallelOnlyHandler {
            config: ParallelWorkerConfig,
        }
        impl TaskHandler for ParallelOnlyHandler {
            fn handle(&mut self, _task: &vidcull_db::repo::Task) -> Result<()> {
                unreachable!("parallel-only test handler: sequential path never used")
            }
            fn as_parallel_worker(&self) -> Option<ParallelWorkerConfig> {
                Some(self.config.clone())
            }
        }
        let mut handler = ParallelOnlyHandler { config };

        let mut run_db = vidcull_db::open_file(&db_path).expect("open run db");
        let shutdown = ShutdownToken::new();
        let throttle_control = Arc::new(ThrottleControl::default());
        throttle_control.set_level(vidcull_ipc::CpuThrottle::Full);
        throttle_control.set_idle_workers(Some(8));
        let daemon = Daemon::new(DaemonConfig {
            kind: "scan".to_owned(),
            throttle_control,
            ..DaemonConfig::default()
        });

        let worker_shutdown = shutdown.clone();
        let worker_thread = std::thread::spawn(move || {
            daemon.run_throttled(
                &mut run_db,
                &mut handler,
                &worker_shutdown,
                unix_now,
                || Activity::UserActive,
            )
        });

        let observe_until = Instant::now() + StdDuration::from_secs(8);
        let denial_count = AtomicUsize::new(0);
        while Instant::now() < observe_until {
            std::thread::sleep(StdDuration::from_millis(200));
            let denials = log_buffer
                .snapshot(usize::MAX)
                .iter()
                .filter(|r| r.message.contains("sequential-read gate at capacity"))
                .count();
            denial_count.store(denials, AtomicOrdering::Relaxed);
        }

        let snapshot = log_buffer.snapshot(usize::MAX);
        let no_progress_hits: Vec<String> = snapshot
            .iter()
            .filter(|r| r.message.contains("no_progress"))
            .map(|r| r.message.clone())
            .collect();
        let final_denials = denial_count.load(AtomicOrdering::Relaxed);

        drop(held_permit);
        shutdown.trigger();
        let join_result = worker_thread.join();

        assert!(
            final_denials >= 3,
            "expected repeated seq_read_gate denials over the 8s observation \
             window (sanity: workers must still be genuinely alive and \
             polling) — got {final_denials}, the harness itself may be broken"
        );
        assert!(
            no_progress_hits.is_empty(),
            "task #27 regression reproduced: {} worker(s) exited via \
             reason=\"no_progress\" despite the single pending task being \
             genuinely due again every ~1s (short gate-contention backoff) — \
             the due-horizon carve-out should have kept them alive: {:?}",
            no_progress_hits.len(),
            no_progress_hits
        );

        join_result
            .expect("worker thread panicked")
            .expect("run_throttled returned an error");
    }
}
