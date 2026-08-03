/**
 * @file    `watcher.rs`
 * @brief   파일 변경 감시와 초기 스캔 작업 등록
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 대용량 스캔의 활성 큐 증분 인덱스와 경량 파일 조회 적용
 */
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_core::{Error, Result, decode, encode};
use vidcull_db::Database;
use vidcull_db::repo::{DuplicateGroupsRepo, FilesRepo, FingerprintsRepo, NewTask, TaskQueueRepo};
use vidcull_scanner::{FsFingerprint, ScanOptions, walk};

use crate::ShutdownToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ChangeKind {
    Upsert = 1,
    Remove = 2,
    Densify = 3,
    ForceUpsert = 4,
    PartialFingerprint = 5,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeTask {
    pub path: NormalizedPath,
    pub change: ChangeKind,
    #[serde(skip)]
    pub size_bytes: i64,
}

impl ChangeTask {
    pub fn to_payload(&self) -> Result<Vec<u8>> {
        encode(self)
    }

    pub fn from_payload(bytes: &[u8]) -> Result<Self> {
        decode(bytes)
    }
}

#[must_use]
pub fn classify_event(event: &Event, options: &ScanOptions) -> Vec<(NormalizedPath, ChangeKind)> {
    let mut out = Vec::new();
    match &event.kind {
        EventKind::Create(_)
        | EventKind::Modify(
            ModifyKind::Data(_)
            | ModifyKind::Any
            | ModifyKind::Name(RenameMode::To | RenameMode::Any | RenameMode::Other),
        ) => {
            push_video_paths(&mut out, &event.paths, ChangeKind::Upsert, options);
        }
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            push_video_paths(&mut out, &event.paths, ChangeKind::Remove, options);
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let Some(from) = event.paths.first() {
                push_video_paths(
                    &mut out,
                    std::slice::from_ref(from),
                    ChangeKind::Remove,
                    options,
                );
            }
            if let Some(to) = event.paths.get(1) {
                push_video_paths(
                    &mut out,
                    std::slice::from_ref(to),
                    ChangeKind::Upsert,
                    options,
                );
            }
        }
        _ => {}
    }
    out
}

fn push_video_paths(
    out: &mut Vec<(NormalizedPath, ChangeKind)>,
    paths: &[std::path::PathBuf],
    change: ChangeKind,
    options: &ScanOptions,
) {
    let is_remove = matches!(change, ChangeKind::Remove);
    let gate_excludes = !is_remove;
    for path in paths {
        let admit = if is_remove {
            is_video(path, options) || path.extension().is_none()
        } else {
            is_video(path, options)
        };
        if !admit {
            continue;
        }
        if gate_excludes && is_under_excluded_dir(path, options) {
            continue;
        }
        out.push((NormalizedPath::new(path), change));
    }
}

fn is_video(path: &Path, options: &ScanOptions) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| options.accepts_extension(&ext.to_ascii_lowercase()))
}

fn is_under_excluded_dir(path: &Path, options: &ScanOptions) -> bool {
    path.parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|c| match c {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .any(|name| options.is_excluded_dir_name(name))
}

#[derive(Debug)]
pub struct Debouncer {
    quiet_ns: i64,
    pending: HashMap<NormalizedPath, Pending>,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    change: ChangeKind,
    last_event_ns: i64,
}

impl Debouncer {
    #[must_use]
    pub fn new(quiet_period: Duration) -> Self {
        Self {
            quiet_ns: i64::try_from(quiet_period.as_nanos()).unwrap_or(i64::MAX),
            pending: HashMap::new(),
        }
    }

    pub fn record(&mut self, path: NormalizedPath, change: ChangeKind, now_ns: i64) {
        let entry = self.pending.entry(path).or_insert(Pending {
            change,
            last_event_ns: now_ns,
        });
        entry.change = change;
        entry.last_event_ns = now_ns;
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn drain_ready(&mut self, now_ns: i64) -> Vec<ChangeTask> {
        let quiet_ns = self.quiet_ns;
        self.drain_filtered(|pending| now_ns.saturating_sub(pending.last_event_ns) >= quiet_ns)
    }

    pub fn drain_all(&mut self) -> Vec<ChangeTask> {
        self.drain_filtered(|_| true)
    }

    fn drain_filtered<F: Fn(&Pending) -> bool>(&mut self, ready: F) -> Vec<ChangeTask> {
        let mut released: Vec<ChangeTask> = self
            .pending
            .iter()
            .filter(|(_, pending)| ready(pending))
            .map(|(path, pending)| ChangeTask {
                path: path.clone(),
                change: pending.change,
                size_bytes: change_size_bytes(path, pending.change),
            })
            .collect();
        released.sort_by(|a, b| a.path.cmp(&b.path));
        for task in &released {
            self.pending.remove(&task.path);
        }
        released
    }
}

pub(crate) fn change_size_bytes(path: &NormalizedPath, change: ChangeKind) -> i64 {
    if !matches!(change, ChangeKind::Upsert | ChangeKind::ForceUpsert) {
        return 0;
    }
    std::fs::metadata(path.to_native_path())
        .ok()
        .and_then(|m| i64::try_from(m.len()).ok())
        .unwrap_or(0)
}

enum UpsertReconcile {
    Proceed,
    Skip,
}

/// Active (PENDING/RUNNING) tasks of one `kind`, decoded once per batch and
/// indexed by path so per-change reconciliation is an in-memory lookup
/// instead of a fresh full-queue scan+decode for every changed file.
struct ActiveByPath {
    by_path: HashMap<NormalizedPath, Vec<(i64, ChangeKind)>>,
    high_watermark: i64,
}

impl ActiveByPath {
    fn load(repo: &TaskQueueRepo<'_>, kind: &str) -> Result<Self> {
        let high_watermark = repo.max_id(kind)?;
        let mut by_path: HashMap<NormalizedPath, Vec<(i64, ChangeKind)>> = HashMap::new();
        for (id, payload) in repo.list_active_payloads(kind)? {
            let Ok(existing) = ChangeTask::from_payload(&payload) else {
                continue;
            };
            by_path
                .entry(existing.path)
                .or_default()
                .push((id, existing.change));
        }
        Ok(Self {
            by_path,
            high_watermark,
        })
    }

    fn refresh(&mut self, repo: &TaskQueueRepo<'_>, kind: &str) -> Result<()> {
        for (id, payload, state) in repo.list_payloads_after(kind, self.high_watermark)? {
            self.high_watermark = self.high_watermark.max(id);
            if state == "FAILED" {
                continue;
            }
            let Ok(change) = ChangeTask::from_payload(&payload) else {
                continue;
            };
            if !self.exists_exact(&change.path, change.change) {
                self.note_inserted(&change.path, id, change.change);
            }
        }
        Ok(())
    }

    fn exists_exact(&self, path: &NormalizedPath, change: ChangeKind) -> bool {
        self.by_path
            .get(path)
            .is_some_and(|entries| entries.iter().any(|&(_, c)| c == change))
    }

    fn note_inserted(&mut self, path: &NormalizedPath, id: i64, change: ChangeKind) {
        self.by_path
            .entry(path.clone())
            .or_default()
            .push((id, change));
    }

    fn note_deleted(&mut self, path: &NormalizedPath, id: i64) {
        if let Some(entries) = self.by_path.get_mut(path) {
            entries.retain(|&(existing_id, _)| existing_id != id);
        }
    }
}

fn reconcile_upsert_class(
    repo: &TaskQueueRepo<'_>,
    active: &mut ActiveByPath,
    incoming: &ChangeTask,
) -> Result<UpsertReconcile> {
    let Some(entries) = active.by_path.get(&incoming.path) else {
        return Ok(UpsertReconcile::Proceed);
    };
    let mut to_delete: Vec<i64> = Vec::new();
    for &(id, existing_change) in entries {
        match existing_change {
            ChangeKind::Upsert => {
                if incoming.change == ChangeKind::ForceUpsert {
                    to_delete.push(id);
                } else {
                    return Ok(UpsertReconcile::Skip);
                }
            }
            ChangeKind::ForceUpsert => {
                return Ok(UpsertReconcile::Skip);
            }
            _ => {}
        }
    }
    for id in to_delete {
        repo.delete(id)?;
        active.note_deleted(&incoming.path, id);
    }
    Ok(UpsertReconcile::Proceed)
}

pub fn enqueue_changes(
    db: &mut Database,
    changes: &[ChangeTask],
    kind: &str,
    priority: i32,
    enqueued_at: i64,
) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    db.transaction(|conn| {
        let repo = TaskQueueRepo::new(conn);
        enqueue_changes_into(&repo, changes, kind, priority, enqueued_at)
    })
}

pub fn enqueue_changes_guarded(
    db: &mut Database,
    changes: &[ChangeTask],
    kind: &str,
    priority: i32,
    enqueued_at: i64,
    control: &crate::throttle::ThrottleControl,
) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    let filtered: Vec<ChangeTask> = changes
        .iter()
        .filter(|c| {
            if matches!(c.change, ChangeKind::Upsert | ChangeKind::ForceUpsert) {
                !control.is_path_removed(&c.path)
            } else {
                true
            }
        })
        .cloned()
        .collect();
    enqueue_changes(db, &filtered, kind, priority, enqueued_at)
}

pub(crate) fn enqueue_changes_into(
    repo: &TaskQueueRepo<'_>,
    changes: &[ChangeTask],
    kind: &str,
    priority: i32,
    enqueued_at: i64,
) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    let mut active = ActiveByPath::load(repo, kind)?;
    enqueue_changes_into_active(repo, changes, kind, priority, enqueued_at, &mut active)
}

fn enqueue_changes_into_active(
    repo: &TaskQueueRepo<'_>,
    changes: &[ChangeTask],
    kind: &str,
    priority: i32,
    enqueued_at: i64,
    active: &mut ActiveByPath,
) -> Result<usize> {
    active.refresh(repo, kind)?;

    let mut inserted = 0;
    for change in changes {
        let payload = change.to_payload()?;
        if active.exists_exact(&change.path, change.change) {
            continue;
        }
        if matches!(change.change, ChangeKind::Upsert | ChangeKind::ForceUpsert) {
            match reconcile_upsert_class(repo, active, change)? {
                UpsertReconcile::Skip => continue,
                UpsertReconcile::Proceed => {}
            }
        }
        if change.change == ChangeKind::Upsert
            && repo.has_failed_with_size(&payload, change.size_bytes)?
        {
            continue;
        }
        let id = repo.enqueue(&NewTask {
            kind: kind.to_owned(),
            priority,
            payload: Some(payload),
            enqueued_at,
            size_bytes: change.size_bytes,
        })?;
        active.note_inserted(&change.path, id, change.change);
        inserted += 1;
    }
    Ok(inserted)
}

fn enqueue_changes_with_active(
    db: &mut Database,
    changes: &[ChangeTask],
    kind: &str,
    priority: i32,
    enqueued_at: i64,
    active: &mut ActiveByPath,
) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    db.transaction(|conn| {
        enqueue_changes_into_active(
            &TaskQueueRepo::new(conn),
            changes,
            kind,
            priority,
            enqueued_at,
            active,
        )
    })
}

pub fn enqueue_partial_backfill(db: &mut Database, kind: &str, enqueued_at: i64) -> Result<usize> {
    let have_partial: BTreeSet<FileId> = {
        let fingerprints = FingerprintsRepo::new(db.conn());
        fingerprints
            .list_active_partial_or_skipped_excluding_reason(
                crate::indexing::PARTIAL_SKIP_REASON_EXACT_FULL_DUP,
            )?
            .into_iter()
            .collect()
    };
    let exact_full_dup: BTreeSet<FileId> = {
        let groups = DuplicateGroupsRepo::new(db.conn());
        groups.list_exact_group_member_ids()?.into_iter().collect()
    };
    let missing: Vec<ChangeTask> = {
        let files = FilesRepo::new(db.conn());
        files
            .list_active()?
            .into_iter()
            .filter(|f| !have_partial.contains(&f.id) && !exact_full_dup.contains(&f.id))
            .map(|f| ChangeTask {
                path: f.path,
                change: ChangeKind::PartialFingerprint,
                size_bytes: 0,
            })
            .collect()
    };
    enqueue_changes(
        db,
        &missing,
        kind,
        crate::indexing::PARTIAL_PRIORITY,
        enqueued_at,
    )
}

const SCAN_BATCH: usize = 512;

pub fn enqueue_initial_scan(
    db: &mut Database,
    roots: &[PathBuf],
    kind: &str,
    enqueued_at: i64,
    exclude_rules: &[String],
) -> Result<usize> {
    enqueue_initial_scan_until(
        db,
        roots,
        kind,
        enqueued_at,
        exclude_rules,
        &|| false,
        &std::sync::atomic::AtomicU64::new(0),
    )
}

pub fn enqueue_initial_scan_until(
    db: &mut Database,
    roots: &[PathBuf],
    kind: &str,
    enqueued_at: i64,
    exclude_rules: &[String],
    should_stop: &dyn Fn() -> bool,
    discovered: &std::sync::atomic::AtomicU64,
) -> Result<usize> {
    let options = ScanOptions::default().with_excludes(exclude_rules);

    let mut previous: HashMap<NormalizedPath, FsFingerprint> = HashMap::new();
    {
        let files = FilesRepo::new(db.conn());
        previous.reserve(files.count_active()?);
        files.visit_active_scan_fingerprints(|record| {
            let fp = FsFingerprint::new(
                u64::try_from(record.size_bytes).unwrap_or(0),
                i128::from(record.mtime_ns),
                record.inode.and_then(|i| u64::try_from(i).ok()),
            );
            previous.insert(record.path, fp);
        })?;
    }

    let mut active = ActiveByPath::load(&TaskQueueRepo::new(db.conn()), kind)?;

    let mut batch: Vec<ChangeTask> = Vec::with_capacity(SCAN_BATCH);
    let mut total = 0usize;
    for root in roots {
        for entry in walk(root, &options) {
            if should_stop() {
                if !batch.is_empty() {
                    total +=
                        enqueue_changes_with_active(db, &batch, kind, 0, enqueued_at, &mut active)?;
                    discovered.store(total as u64, std::sync::atomic::Ordering::Relaxed);
                }
                return Ok(total);
            }
            let Ok(entry) = entry else {
                continue;
            };
            let changed = previous
                .get(&entry.path)
                .is_none_or(|prev| !prev.matches(&entry.fingerprint));
            if !changed {
                continue;
            }
            batch.push(ChangeTask {
                path: entry.path,
                change: ChangeKind::Upsert,
                size_bytes: i64::try_from(entry.fingerprint.size_bytes).unwrap_or(i64::MAX),
            });
            if batch.len() >= SCAN_BATCH {
                total +=
                    enqueue_changes_with_active(db, &batch, kind, 0, enqueued_at, &mut active)?;
                discovered.store(total as u64, std::sync::atomic::Ordering::Relaxed);
                batch.clear();
            }
        }
    }
    if !batch.is_empty() {
        total += enqueue_changes_with_active(db, &batch, kind, 0, enqueued_at, &mut active)?;
        discovered.store(total as u64, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(total)
}

const RECONCILE_INTERVAL_NS: i64 = 60 * 1_000_000_000;

pub(crate) fn reconcile_watched_roots(
    db: &Database,
    roots: &std::collections::HashSet<PathBuf>,
) -> Result<Vec<ChangeTask>> {
    let mut removes = Vec::new();
    for root in roots {
        if !root.is_dir() {
            if root.parent().is_some_and(std::path::Path::is_dir) {
                tracing::info!(
                    root = %crate::redact::redact_fs_path(root),
                    "reconcile: watched root is gone (parent live) — purging its indexed files",
                );
            } else {
                tracing::debug!(
                    root = %crate::redact::redact_fs_path(root),
                    "reconcile: watched root inaccessible, parent also gone — skipping (offline guard)",
                );
                continue;
            }
        }
        let root_norm = NormalizedPath::new(root);
        let files = FilesRepo::new(db.conn());
        for path in files.list_active_paths_under_root(&root_norm)? {
            match std::fs::symlink_metadata(path.to_native_path()) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    removes.push(ChangeTask {
                        path,
                        change: ChangeKind::Remove,
                        size_bytes: 0,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(removes)
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub options: ScanOptions,
    pub quiet_period: Duration,
    pub poll_interval: Duration,
    pub task_kind: String,
    pub priority: i32,
    pub throttle_control: Option<std::sync::Arc<crate::throttle::ThrottleControl>>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            options: ScanOptions::default(),
            quiet_period: Duration::from_millis(500),
            poll_interval: Duration::from_millis(200),
            task_kind: "scan".to_owned(),
            priority: 0,
            throttle_control: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WatchStats {
    pub events_seen: usize,
    pub changes_enqueued: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn run_event_loop<M, W>(
    rx: &Receiver<notify::Result<Event>>,
    config: &WatchConfig,
    db: &mut Database,
    shutdown: &ShutdownToken,
    mut watcher: Option<&mut RecommendedWatcher>,
    initial_roots: &[PathBuf],
    monotonic_ns: M,
    wall_clock: W,
) -> Result<WatchStats>
where
    M: Fn() -> i64,
    W: Fn() -> i64,
{
    const ONE_SEC_NS: i64 = 1_000_000_000;

    let mut debouncer = Debouncer::new(config.quiet_period);
    let mut stats = WatchStats::default();

    let mut last_settings_check = monotonic_ns();
    let mut current_settings = crate::settings::load(db);
    let mut current_options = config.options.clone();
    let mut active_watched: std::collections::HashSet<PathBuf> =
        initial_roots.iter().cloned().collect();
    let mut last_reconcile: Option<i64> = None;

    while !shutdown.is_triggered() {
        if monotonic_ns().saturating_sub(last_settings_check) >= ONE_SEC_NS {
            last_settings_check = monotonic_ns();
            let new_settings = crate::settings::load(db);
            current_settings.auto_index = new_settings.auto_index;
            if current_settings.scan_folders != new_settings.scan_folders {
                last_reconcile = None;
            }
            current_settings
                .scan_folders
                .clone_from(&new_settings.scan_folders);
            current_options.exclude_dirs = vidcull_scanner::default_excluded_dirs();
            current_options = current_options.with_excludes(&new_settings.exclude_rules);

            if let Some(ref mut w) = watcher {
                if new_settings.auto_index {
                    for folder in &new_settings.scan_folders {
                        let path = PathBuf::from(folder);
                        if !active_watched.contains(&path) {
                            if let Err(err) = w.watch(&path, RecursiveMode::Recursive) {
                                tracing::warn!(path = %crate::redact::redact_fs_path(&path), error = %err, "could not dynamically watch path");
                            } else {
                                tracing::info!(path = %crate::redact::redact_fs_path(&path), "dynamically watching for changes");
                                active_watched.insert(path);
                            }
                        }
                    }

                    let mut to_unwatch = Vec::new();
                    for path in &active_watched {
                        let path_str = path.to_string_lossy().replace('\\', "/");
                        let in_settings = new_settings.scan_folders.contains(&path_str);
                        let in_initial = initial_roots.contains(path);
                        if !in_settings && !in_initial {
                            to_unwatch.push(path.clone());
                        }
                    }

                    for path in to_unwatch {
                        if let Err(err) = w.unwatch(&path) {
                            tracing::warn!(path = %crate::redact::redact_fs_path(&path), error = %err, "could not dynamically unwatch path");
                        } else {
                            tracing::info!(path = %crate::redact::redact_fs_path(&path), "dynamically unwatched path");
                            active_watched.remove(&path);
                        }
                    }
                }
            }
        }

        match rx.recv_timeout(config.poll_interval) {
            Ok(first) => {
                let now = monotonic_ns();
                let auto_index = current_settings.auto_index;
                let options = &current_options;
                record_event(first, &mut stats, &mut debouncer, auto_index, now, options);
                let drain =
                    drain_available(rx, &mut stats, &mut debouncer, auto_index, now, options);
                if let Err(TryRecvError::Disconnected) = drain {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let ready = debouncer.drain_ready(monotonic_ns());
        stats.changes_enqueued += enqueue_batch(db, &ready, config, &wall_clock)?;

        let now_mono = monotonic_ns();
        let reconcile_due =
            last_reconcile.is_none_or(|t| now_mono.saturating_sub(t) >= RECONCILE_INTERVAL_NS);
        if reconcile_due && current_settings.auto_index {
            last_reconcile = Some(now_mono);
            let mut reconcile_roots = active_watched.clone();
            for folder in &current_settings.scan_folders {
                reconcile_roots.insert(PathBuf::from(folder));
            }
            match reconcile_watched_roots(db, &reconcile_roots) {
                Ok(removes) => match enqueue_batch(db, &removes, config, &wall_clock) {
                    Ok(n) => {
                        stats.changes_enqueued += n;
                        if n > 0 {
                            tracing::info!(
                                purged = n,
                                "reconcile: files missing from disk soft-deleted"
                            );
                        }
                    }
                    Err(err) => tracing::warn!(error = %err, "reconcile enqueue failed"),
                },
                Err(err) => tracing::warn!(error = %err, "reconcile sweep failed"),
            }
        }
    }
    let remaining = debouncer.drain_all();
    stats.changes_enqueued += enqueue_batch(db, &remaining, config, &wall_clock)?;
    Ok(stats)
}

fn record_event(
    item: notify::Result<Event>,
    stats: &mut WatchStats,
    debouncer: &mut Debouncer,
    auto_index: bool,
    now_ns: i64,
    options: &ScanOptions,
) {
    match item {
        Ok(event) => {
            stats.events_seen += 1;
            if auto_index {
                for (path, change) in classify_event(&event, options) {
                    debouncer.record(path, change, now_ns);
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "filesystem watch error; continuing");
        }
    }
}

fn drain_available(
    rx: &Receiver<notify::Result<Event>>,
    stats: &mut WatchStats,
    debouncer: &mut Debouncer,
    auto_index: bool,
    now_ns: i64,
    options: &ScanOptions,
) -> std::result::Result<(), TryRecvError> {
    loop {
        match rx.try_recv() {
            Ok(item) => record_event(item, stats, debouncer, auto_index, now_ns, options),
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Err(TryRecvError::Disconnected),
        }
    }
}

fn enqueue_batch<W: Fn() -> i64>(
    db: &mut Database,
    changes: &[ChangeTask],
    config: &WatchConfig,
    wall_clock: &W,
) -> Result<usize> {
    if let Some(control) = &config.throttle_control {
        return enqueue_changes_guarded(
            db,
            changes,
            &config.task_kind,
            config.priority,
            wall_clock(),
            control,
        );
    }
    enqueue_changes(
        db,
        changes,
        &config.task_kind,
        config.priority,
        wall_clock(),
    )
}

#[must_use]
pub fn monotonic_ns_since(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_nanos()).unwrap_or(i64::MAX)
}

pub struct FileWatcher {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    config: WatchConfig,
    roots: Vec<PathBuf>,
}

impl FileWatcher {
    pub fn new(config: WatchConfig) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|err| notify_err(&err))?;
        Ok(Self {
            watcher,
            rx,
            config,
            roots: Vec::new(),
        })
    }

    pub fn watch(&mut self, root: &Path) -> Result<()> {
        self.watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|err| notify_err(&err))?;
        self.roots.push(root.to_path_buf());
        Ok(())
    }

    pub fn run(&mut self, db: &mut Database, shutdown: &ShutdownToken) -> Result<WatchStats> {
        let start = Instant::now();
        run_event_loop(
            &self.rx,
            &self.config,
            db,
            shutdown,
            Some(&mut self.watcher),
            &self.roots,
            move || monotonic_ns_since(start),
            crate::unix_now,
        )
    }
}

fn notify_err(err: &notify::Error) -> Error {
    Error::Io(std::io::Error::other(format!(
        "filesystem watch error: {err}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind};
    use std::path::PathBuf;

    const SEC_NS: i64 = 1_000_000_000;

    #[test]
    fn active_index_refreshes_only_new_rows_and_advances_past_failures() {
        let db = vidcull_db::open_in_memory().expect("db");
        let repo = TaskQueueRepo::new(db.conn());
        let mut active = ActiveByPath::load(&repo, "scan").expect("load");

        let done = ChangeTask {
            path: NormalizedPath::new("/videos/done.mp4"),
            change: ChangeKind::Upsert,
            size_bytes: 10,
        };
        let done_id = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(done.to_payload().expect("payload")),
                enqueued_at: 1,
                size_bytes: 10,
            })
            .expect("enqueue done");
        repo.mark_done(done_id, 2).expect("mark done");

        let failed = ChangeTask {
            path: NormalizedPath::new("/videos/failed.mp4"),
            change: ChangeKind::Upsert,
            size_bytes: 20,
        };
        let failed_id = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(failed.to_payload().expect("payload")),
                enqueued_at: 1,
                size_bytes: 20,
            })
            .expect("enqueue failed");
        repo.mark_failed(failed_id, 2, "decode failed")
            .expect("mark failed");

        let pending = ChangeTask {
            path: NormalizedPath::new("/videos/pending.mp4"),
            change: ChangeKind::Upsert,
            size_bytes: 30,
        };
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: 0,
            payload: Some(pending.to_payload().expect("payload")),
            enqueued_at: 1,
            size_bytes: 30,
        })
        .expect("enqueue pending");

        active.refresh(&repo, "scan").expect("refresh");
        assert!(active.exists_exact(&done.path, done.change));
        assert!(active.exists_exact(&pending.path, pending.change));
        assert!(!active.exists_exact(&failed.path, failed.change));
        assert_eq!(active.high_watermark, repo.max_id("scan").expect("max id"));
    }

    fn create_event(path: &str) -> Event {
        Event::new(EventKind::Create(CreateKind::File)).add_path(PathBuf::from(path))
    }

    fn modify_event(path: &str) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(PathBuf::from(path))
    }

    fn remove_event(path: &str) -> Event {
        Event::new(EventKind::Remove(RemoveKind::File)).add_path(PathBuf::from(path))
    }

    #[test]
    fn classify_create_of_video_is_upsert() {
        let out = classify_event(&create_event("/a/b/clip.mp4"), &ScanOptions::default());
        assert_eq!(
            out,
            vec![(NormalizedPath::new("/a/b/clip.mp4"), ChangeKind::Upsert)]
        );
    }

    #[test]
    fn classify_ignores_non_video_extensions() {
        let opts = ScanOptions::default();
        assert!(classify_event(&create_event("/a/readme.txt"), &opts).is_empty());
        assert!(classify_event(&create_event("/a/subdir"), &opts).is_empty());
    }

    #[test]
    fn classify_data_modify_is_upsert() {
        let out = classify_event(&modify_event("/a/clip.mp4"), &ScanOptions::default());
        assert_eq!(
            out,
            vec![(NormalizedPath::new("/a/clip.mp4"), ChangeKind::Upsert)]
        );
    }

    #[test]
    fn classify_remove_is_remove() {
        let out = classify_event(&remove_event("/a/clip.mkv"), &ScanOptions::default());
        assert_eq!(
            out,
            vec![(NormalizedPath::new("/a/clip.mkv"), ChangeKind::Remove)]
        );
    }

    #[test]
    fn classify_rename_both_emits_remove_then_upsert() {
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/a/old.mp4"))
            .add_path(PathBuf::from("/a/new.mp4"));
        let out = classify_event(&event, &ScanOptions::default());
        assert_eq!(
            out,
            vec![
                (NormalizedPath::new("/a/old.mp4"), ChangeKind::Remove),
                (NormalizedPath::new("/a/new.mp4"), ChangeKind::Upsert),
            ]
        );
    }

    #[test]
    fn classify_rename_from_is_remove_to_is_upsert() {
        let opts = ScanOptions::default();
        let from = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(PathBuf::from("/a/gone.mp4"));
        let to = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
            .add_path(PathBuf::from("/a/here.mp4"));
        assert_eq!(classify_event(&from, &opts)[0].1, ChangeKind::Remove);
        assert_eq!(classify_event(&to, &opts)[0].1, ChangeKind::Upsert);
    }

    #[test]
    fn classify_skips_a_video_under_an_excluded_directory() {
        let opts = ScanOptions::default().with_excludes([".trash"]);
        assert!(classify_event(&create_event("/lib/.trash/clip.mp4"), &opts).is_empty());
        assert_eq!(
            classify_event(&create_event("/lib/keep/clip.mp4"), &opts),
            vec![(
                NormalizedPath::new("/lib/keep/clip.mp4"),
                ChangeKind::Upsert
            )]
        );
        assert!(
            classify_event(
                &create_event("/lib/node_modules/x.mp4"),
                &ScanOptions::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn classify_passes_remove_under_excluded_directory() {
        let opts = ScanOptions::default().with_excludes([".trash"]);
        let remove = Event::new(EventKind::Remove(notify::event::RemoveKind::File))
            .add_path(PathBuf::from("/lib/.trash/clip.mp4"));
        assert_eq!(
            classify_event(&remove, &opts),
            vec![(
                NormalizedPath::new("/lib/.trash/clip.mp4"),
                ChangeKind::Remove
            )]
        );
        assert!(classify_event(&create_event("/lib/.trash/clip.mp4"), &opts).is_empty());
    }

    #[test]
    fn classify_metadata_only_modify_is_ignored() {
        let event = Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)))
            .add_path(PathBuf::from("/a/clip.mp4"));
        assert!(classify_event(&event, &ScanOptions::default()).is_empty());
    }

    #[test]
    fn classify_directory_remove_admits_extensionless_path() {
        let opts = ScanOptions::default();
        let remove_dir = Event::new(EventKind::Remove(RemoveKind::Folder))
            .add_path(PathBuf::from("/lib/MyClips"));
        assert_eq!(
            classify_event(&remove_dir, &opts),
            vec![(NormalizedPath::new("/lib/MyClips"), ChangeKind::Remove)]
        );
        let remove_txt = Event::new(EventKind::Remove(RemoveKind::File))
            .add_path(PathBuf::from("/lib/notes.txt"));
        assert!(classify_event(&remove_txt, &opts).is_empty());
        assert!(classify_event(&create_event("/lib/MyClips"), &opts).is_empty());
    }

    #[test]
    fn change_task_payload_round_trips() {
        let task = ChangeTask {
            path: NormalizedPath::new(r"C:\lib\a.mp4"),
            change: ChangeKind::Remove,
            size_bytes: 0,
        };
        let bytes = task.to_payload().expect("encode");
        assert_eq!(ChangeTask::from_payload(&bytes).expect("decode"), task);
    }

    #[test]
    fn debouncer_holds_change_until_quiet_period_elapses() {
        let mut d = Debouncer::new(Duration::from_nanos(SEC_NS as u64));
        d.record(NormalizedPath::new("/a.mp4"), ChangeKind::Upsert, 0);
        assert!(d.drain_ready(SEC_NS - 1).is_empty());
        assert_eq!(d.pending_len(), 1);
        let ready = d.drain_ready(SEC_NS);
        assert_eq!(ready.len(), 1);
        assert_eq!(d.pending_len(), 0);
    }

    #[test]
    fn debouncer_resets_quiet_timer_on_repeat_event() {
        let mut d = Debouncer::new(Duration::from_nanos(SEC_NS as u64));
        d.record(NormalizedPath::new("/a.mp4"), ChangeKind::Upsert, 0);
        d.record(
            NormalizedPath::new("/a.mp4"),
            ChangeKind::Upsert,
            SEC_NS / 2,
        );
        assert!(d.drain_ready(SEC_NS).is_empty(), "timer should have reset");
        assert_eq!(d.drain_ready(SEC_NS + SEC_NS / 2).len(), 1);
    }

    #[test]
    fn debouncer_coalesces_per_path_and_last_kind_wins() {
        let mut d = Debouncer::new(Duration::ZERO);
        let p = NormalizedPath::new("/a.mp4");
        d.record(p.clone(), ChangeKind::Upsert, 0);
        d.record(p.clone(), ChangeKind::Remove, 0);
        assert_eq!(d.pending_len(), 1, "same path coalesces to one entry");
        let ready = d.drain_ready(0);
        assert_eq!(
            ready,
            vec![ChangeTask {
                path: p,
                change: ChangeKind::Remove,
                size_bytes: 0
            }]
        );
    }

    #[test]
    fn burst_of_a_thousand_events_collapses_per_path() {
        let mut d = Debouncer::new(Duration::ZERO);
        let paths = ["/a.mp4", "/b.mkv", "/c.mov", "/d.webm"];
        for i in 0..1000_i64 {
            let idx = usize::try_from(i).expect("fits") % paths.len();
            d.record(NormalizedPath::new(paths[idx]), ChangeKind::Upsert, i);
        }
        assert_eq!(d.pending_len(), paths.len());
        let ready = d.drain_ready(i64::MAX);
        assert_eq!(ready.len(), paths.len());
    }

    #[test]
    fn drain_available_empties_the_channel_in_one_call_and_coalesces() {
        use std::sync::mpsc::channel;

        const BURST: usize = 5000;
        let (tx, rx) = channel::<notify::Result<Event>>();
        let paths = ["/lib/a.mp4", "/lib/b.mkv", "/lib/c.mov"];
        for i in 0..BURST {
            tx.send(Ok(create_event(paths[i % paths.len()])))
                .expect("send");
        }

        let mut debouncer = Debouncer::new(Duration::from_secs(3_600));
        let mut stats = WatchStats::default();
        let opts = ScanOptions::default();

        let result = drain_available(&rx, &mut stats, &mut debouncer, true, 0, &opts);
        assert!(
            result.is_ok(),
            "channel still connected → Ok(()), got {result:?}"
        );
        assert_eq!(
            stats.events_seen, BURST,
            "a single drain must consume the whole burst, not one event per call",
        );

        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "channel must be drained empty after one drain_available call",
        );

        assert_eq!(
            debouncer.pending_len(),
            paths.len(),
            "debouncer map must be bounded by distinct paths, not total events",
        );

        let ready = debouncer.drain_ready(i64::MAX);
        assert_eq!(ready.len(), paths.len());
    }

    #[test]
    fn drain_available_reports_disconnect_so_the_loop_stops() {
        use std::sync::mpsc::channel;

        let (tx, rx) = channel::<notify::Result<Event>>();
        tx.send(Ok(create_event("/lib/a.mp4"))).expect("send");
        drop(tx);

        let mut debouncer = Debouncer::new(Duration::ZERO);
        let mut stats = WatchStats::default();
        let opts = ScanOptions::default();

        let result = drain_available(&rx, &mut stats, &mut debouncer, true, 0, &opts);
        assert_eq!(result, Err(TryRecvError::Disconnected));
        assert_eq!(stats.events_seen, 1, "the buffered event is still recorded");
        assert_eq!(debouncer.pending_len(), 1);
    }

    #[test]
    fn drain_ready_is_sorted_by_path() {
        let mut d = Debouncer::new(Duration::ZERO);
        d.record(NormalizedPath::new("/z.mp4"), ChangeKind::Upsert, 0);
        d.record(NormalizedPath::new("/a.mp4"), ChangeKind::Upsert, 0);
        d.record(NormalizedPath::new("/m.mp4"), ChangeKind::Upsert, 0);
        let ready = d.drain_ready(0);
        let paths: Vec<_> = ready.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/a.mp4", "/m.mp4", "/z.mp4"]);
    }

    #[test]
    fn drain_all_flushes_regardless_of_timer() {
        let mut d = Debouncer::new(Duration::from_nanos(SEC_NS as u64));
        d.record(NormalizedPath::new("/a.mp4"), ChangeKind::Upsert, 0);
        assert!(d.drain_ready(1).is_empty());
        assert_eq!(d.drain_all().len(), 1);
        assert_eq!(d.pending_len(), 0);
    }

    #[test]
    fn reconcile_purges_files_of_a_moved_away_watched_root() {
        use std::collections::HashSet;
        use vidcull_db::repo::NewFile;

        let dir = tempfile::tempdir().expect("tempdir");
        let moved_root = dir.path().join("iuueas");
        let db = vidcull_db::open_in_memory().expect("open db");
        {
            let files = FilesRepo::new(db.conn());
            for name in ["a.mp4", "b.mp4", "sub/c.mp4"] {
                files
                    .insert(&NewFile {
                        path: NormalizedPath::new(moved_root.join(name)),
                        size_bytes: 1,
                        ..Default::default()
                    })
                    .expect("seed file under moved root");
            }
        }

        let mut roots = HashSet::new();
        roots.insert(moved_root.clone());
        let removes = reconcile_watched_roots(&db, &roots).expect("reconcile");
        let removed_paths: HashSet<String> =
            removes.iter().map(|r| r.path.as_str().to_owned()).collect();
        assert_eq!(
            removes.len(),
            3,
            "all files under the moved-away root must be reconciled"
        );
        for name in ["a.mp4", "b.mp4", "sub/c.mp4"] {
            assert!(
                removed_paths.contains(NormalizedPath::new(moved_root.join(name)).as_str()),
                "missing Remove for {name}",
            );
        }
        assert!(removes.iter().all(|r| r.change == ChangeKind::Remove));
    }

    #[test]
    fn reconcile_watched_roots_purges_vanished_files_and_guards_offline_root() {
        use std::collections::HashSet;
        use vidcull_db::repo::NewFile;

        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir sub");
        let keep = dir.path().join("keep.mp4");
        let gone = sub.join("gone.mp4");
        std::fs::write(&keep, b"k").expect("write keep");
        std::fs::write(&gone, b"g").expect("write gone");

        let db = vidcull_db::open_in_memory().expect("open db");
        {
            let files = FilesRepo::new(db.conn());
            for p in [&keep, &gone] {
                files
                    .insert(&NewFile {
                        path: NormalizedPath::new(p),
                        size_bytes: 1,
                        ..Default::default()
                    })
                    .expect("seed file");
            }
        }

        std::fs::remove_dir_all(&sub).expect("rm sub");

        let mut roots = HashSet::new();
        roots.insert(dir.path().to_path_buf());
        let removes = reconcile_watched_roots(&db, &roots).expect("reconcile");
        assert_eq!(removes.len(), 1, "only the vanished file is reconciled");
        assert_eq!(removes[0].path, NormalizedPath::new(&gone));
        assert_eq!(removes[0].change, ChangeKind::Remove);
        assert!(removes.iter().all(|r| r.path != NormalizedPath::new(&keep)));

        let offline_parent = dir.path().join("offline-vol");
        let offline_root = offline_parent.join("subroot");
        {
            let files = FilesRepo::new(db.conn());
            for name in ["a.mp4", "b.mp4"] {
                files
                    .insert(&NewFile {
                        path: NormalizedPath::new(offline_root.join(name)),
                        size_bytes: 1,
                        ..Default::default()
                    })
                    .expect("seed offline file");
            }
        }
        let mut offline = HashSet::new();
        offline.insert(offline_root);
        assert!(
            reconcile_watched_roots(&db, &offline)
                .expect("guard")
                .is_empty(),
            "an inaccessible root must never purge its (ENOENT) files",
        );
    }
}
