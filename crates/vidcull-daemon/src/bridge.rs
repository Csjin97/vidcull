/**
 * @file    `bridge.rs`
 * @brief   데몬 IPC 요청 처리 및 데이터 변환
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 클러스터 목록과 멤버 상세를 단일 계산으로 통합
 * - 2026-08-03 : 실패 작업의 파일 상태 조회를 일괄 처리하여 대량 실패 시 DB 부하 감소
 */
// 2026-08-03: 진행 상태 조회의 대용량 작업 큐를 스트리밍 처리하도록 개선.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use vidcull_core::Result;
use vidcull_core::types::{FileId, NormalizedPath, VideoDuration};
use vidcull_db::Database;
use vidcull_db::repo::{
    BatchFileRole, DeleteBatchMode, DeleteJournalRepo, DuplicateGroupsRepo, FileRecord, FilesRepo,
    FingerprintsRepo, PartialMihRepo, SimilarityEdgesRepo, TaskQueueRepo, TaskState,
    TrustLevel as DbTrust,
};
use vidcull_fingerprint::format::decode_tier2;
use vidcull_ipc::protocol::PROTOCOL_VERSION;
use vidcull_ipc::{
    Action, ActionResult, ClipOverlap, ClusterMemberDetail, ClusterStats, ClusterSummary,
    CrossGroupConflict, DaemonSettings, DeleteRequest, DeleteResult, FailedTask, FileDetail,
    GroupRole, GroupStats, GroupSummary, IpcError, IpcErrorKind, ProgressSnapshot, Reply, Request,
    RequestHandler, Response, TrustLevel as IpcTrust, UndoResult,
};
use vidcull_matcher::cluster::{Cluster, build_clusters};
use vidcull_matcher::partial::{is_intro_outro, partial_clip_params, plan_partial_clips};
use vidcull_matcher::ranking::assign_best_copies;

use crate::delete::{DeleteMode, DeleteReject, FileRemover, plan_deletion};
use crate::logbuf::LogBuffer;

const SNAPSHOT_EVERY_BATCHES: i64 = 10;

const AUTOSTART_NAME: &str = "vidcull";

const DETAIL_CHUNK_SIZE: usize = 200;
use crate::thumbnails::ThumbnailProvider;
use crate::watcher::{ChangeKind, ChangeTask};
use crate::{ShutdownToken, unix_now};

pub struct DaemonRequestHandler {
    db: Arc<Mutex<Database>>,
    shutdown: ShutdownToken,
    logs: LogBuffer,
    task_kind: String,
    remover: Arc<dyn FileRemover>,
    throttle_control: Arc<crate::throttle::ThrottleControl>,
    thumbnails: Option<Arc<ThumbnailProvider>>,
    backup_dir: Option<std::path::PathBuf>,
    metrics: crate::metrics::MetricsCollector,
    throughput: crate::metrics::ThroughputTracker,
    worker_health: Option<crate::worker_health::WorkerHealth>,
    autostart_command: Option<String>,
    scan_executor: Option<crate::scan_exec::ScanExecutor>,
}

impl DaemonRequestHandler {
    #[must_use]
    pub fn new(
        db: Arc<Mutex<Database>>,
        shutdown: ShutdownToken,
        logs: LogBuffer,
        task_kind: String,
        remover: Arc<dyn FileRemover>,
    ) -> Self {
        Self {
            db,
            shutdown,
            logs,
            task_kind,
            remover,
            throttle_control: Arc::new(crate::throttle::ThrottleControl::default()),
            thumbnails: None,
            backup_dir: None,
            metrics: crate::metrics::MetricsCollector::new(),
            throughput: crate::metrics::ThroughputTracker::new(),
            worker_health: None,
            autostart_command: None,
            scan_executor: None,
        }
    }

    #[must_use]
    pub fn with_autostart_command(mut self, command: impl Into<String>) -> Self {
        self.autostart_command = Some(command.into());
        self
    }

    #[must_use]
    pub fn with_worker_health(mut self, health: crate::worker_health::WorkerHealth) -> Self {
        self.worker_health = Some(health);
        self
    }

    #[must_use]
    pub fn with_backup_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.backup_dir = Some(dir);
        self
    }

    #[must_use]
    pub fn with_thumbnails(mut self, provider: Arc<ThumbnailProvider>) -> Self {
        self.thumbnails = Some(provider);
        self
    }

    #[must_use]
    pub fn with_throttle_control(mut self, control: Arc<crate::throttle::ThrottleControl>) -> Self {
        self.throttle_control = control;
        self
    }

    #[must_use]
    pub fn with_scan_executor(mut self, executor: crate::scan_exec::ScanExecutor) -> Self {
        self.scan_executor = Some(executor);
        self
    }

    fn with_db<T>(&self, f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let wait_started = std::time::Instant::now();
        let db = self.db.lock().unwrap_or_else(PoisonError::into_inner);
        let wait_ms = duration_ms(wait_started.elapsed());
        let hold_started = std::time::Instant::now();
        let result = f(&db);
        record_db_access(wait_ms, duration_ms(hold_started.elapsed()));
        result
    }

    fn with_db_mut<T>(&self, f: impl FnOnce(&mut Database) -> Result<T>) -> Result<T> {
        let wait_started = std::time::Instant::now();
        let mut db = self.db.lock().unwrap_or_else(PoisonError::into_inner);
        let wait_ms = duration_ms(wait_started.elapsed());
        let hold_started = std::time::Instant::now();
        let result = f(&mut db);
        record_db_access(wait_ms, duration_ms(hold_started.elapsed()));
        result
    }

    #[allow(clippy::too_many_lines)]
    fn progress(&self) -> Reply {
        let process_metrics = self.metrics.sample();
        let (dead_workers, panic_count) = self
            .worker_health
            .as_ref()
            .map_or((0, 0), |h| (h.dead_workers(), h.panic_count()));
        let (folder_scanning, scan_discovered) = self
            .scan_executor
            .as_ref()
            .map_or((false, 0), |e| (e.is_scanning(), e.discovered()));
        let snapshot = self.with_db(|db| {
            let repo = TaskQueueRepo::new(db.conn());
            let files_repo = FilesRepo::new(db.conn());
            let total_bytes_indexed = files_repo.sum_active_size_bytes()?;
            let throughput = self.throughput.update(total_bytes_indexed);
            let pending_bytes = repo.sum_outstanding_size_bytes()?;

            let decode_path = |priority: i32, payload: Option<&[u8]>| -> Option<String> {
                if priority < 0 {
                    return None;
                }
                payload
                    .and_then(|bytes| ChangeTask::from_payload(bytes).ok())
                    .map(|change| change.path.as_str().to_owned())
            };
            let mut current_files = Vec::new();
            let mut inflight = std::collections::BTreeSet::new();
            repo.visit_priority_payload_by_state(TaskState::Running, |priority, payload| {
                if let Some(path) = decode_path(priority, payload) {
                    inflight.insert(path.clone());
                    current_files.push(path);
                }
                Ok(())
            })?;
            repo.visit_priority_payload_by_state(TaskState::Pending, |priority, payload| {
                if let Some(path) = decode_path(priority, payload) {
                    inflight.insert(path);
                }
                Ok(())
            })?;

            let active_indexed = files_repo.count_active_indexed()?;
            let inflight_paths: Vec<NormalizedPath> = inflight
                .iter()
                .map(|p| NormalizedPath::new(p.as_str()))
                .collect();
            let inflight_active_hashed = files_repo.active_hashed_paths_in(&inflight_paths)?;
            let reprocessing = u64::try_from(
                inflight_paths
                    .iter()
                    .filter(|np| inflight_active_hashed.contains(np.as_str()))
                    .count(),
            )
            .unwrap_or(u64::MAX);
            let done = active_indexed.saturating_sub(reprocessing);

            let mut failed_foreground = Vec::new();
            repo.visit_by_state(TaskState::Failed, |task| {
                if task.priority >= 0 {
                    if let Some(path) = task_path(task) {
                        failed_foreground.push((
                            path,
                            task.last_error.clone().unwrap_or_default(),
                            task.attempts,
                        ));
                    }
                }
                Ok(())
            })?;
            let failed_paths: Vec<NormalizedPath> = failed_foreground
                .iter()
                .map(|(p, _, _)| NormalizedPath::new(p.as_str()))
                .collect();
            let failed_active_hashed = files_repo.active_hashed_paths_in(&failed_paths)?;
            let mut failed_files = std::collections::BTreeSet::new();
            let mut excluded_class: std::collections::BTreeMap<
                String,
                crate::indexing::ReindexFailureClass,
            > = std::collections::BTreeMap::new();
            for (path, reason, attempts) in failed_foreground {
                let normalized = NormalizedPath::new(path.as_str());
                if failed_active_hashed.contains(normalized.as_str()) {
                    excluded_class.insert(
                        path,
                        crate::indexing::classify_reindex_failure(&reason, attempts),
                    );
                } else {
                    failed_files.insert(path);
                }
            }
            let failed = u64::try_from(failed_files.len()).unwrap_or(u64::MAX);
            let partial_failed = u64::try_from(
                excluded_class
                    .values()
                    .filter(|c| matches!(c, crate::indexing::ReindexFailureClass::PermanentSurface))
                    .count(),
            )
            .unwrap_or(u64::MAX);

            let fingerprints = FingerprintsRepo::new(db.conn());
            let mut partial_skipped: std::collections::BTreeMap<String, u64> =
                std::collections::BTreeMap::new();
            let mut partial_skipped_total: u64 = 0;
            for (reason, count) in fingerprints.count_partial_skip_by_reason()? {
                let n = u64::try_from(count).unwrap_or(0);
                partial_skipped_total = partial_skipped_total.saturating_add(n);
                partial_skipped.insert(reason, n);
            }
            let count_active_partial_files = |state: TaskState| -> Result<u64> {
                let mut paths: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                let mut null_payload_count: u64 = 0;
                repo.visit_payload_by_priority_state(
                    crate::indexing::PARTIAL_PRIORITY,
                    state,
                    |payload| {
                        match payload {
                            Some(bytes) => {
                                if let Ok(change) = ChangeTask::from_payload(bytes) {
                                    paths.insert(change.path.as_str().to_owned());
                                }
                            }
                            None => null_payload_count += 1,
                        }
                        Ok(())
                    },
                )?;
                let normalized: Vec<NormalizedPath> = paths
                    .iter()
                    .map(|p| NormalizedPath::new(p.as_str()))
                    .collect();
                let active_hashed = files_repo.active_hashed_paths_in(&normalized)?;
                let active_count = u64::try_from(
                    normalized
                        .iter()
                        .filter(|np| active_hashed.contains(np.as_str()))
                        .count(),
                )
                .unwrap_or(u64::MAX);
                Ok(active_count.saturating_add(null_payload_count))
            };
            let partial_done_raw = count_active_partial_files(TaskState::Done)?;
            let partial_done = partial_done_raw.saturating_sub(partial_skipped_total);

            Ok(ProgressSnapshot {
                pending: repo.count_distinct_files_by_state(TaskState::Pending)?,
                running: repo.count_distinct_files_by_state(TaskState::Running)?,
                done,
                failed,
                cpu_usage_permille: process_metrics.cpu_permille,
                rss_bytes: process_metrics.rss_bytes,
                throughput_bytes_per_sec: throughput,
                pending_bytes,
                current_files,
                dead_workers,
                panic_count,
                partial_pending: count_active_partial_files(TaskState::Pending)?,
                partial_running: count_active_partial_files(TaskState::Running)?,
                partial_done,
                partial_skipped,
                partial_failed,
                folder_scanning,
                scan_discovered,
            })
        });
        match snapshot {
            Ok(snapshot) => Reply::single(Response::Progress(snapshot)),
            Err(err) => internal(&err),
        }
    }

    fn list_groups(&self, trust: Option<IpcTrust>, limit: u32, offset: u32) -> Reply {
        let want = trust.map(ipc_to_db_trust);
        let summaries = self.with_db(|db| {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let edges_repo = SimilarityEdgesRepo::new(db.conn());
            let page = repo.list_page(want, limit, offset)?;
            let ids: Vec<i64> = page.iter().map(|group| group.id).collect();
            let counts = repo.member_counts(&ids)?;
            let intro_outro = edges_repo.all_tagged_intro_outro(&ids)?;
            let mut out = Vec::with_capacity(page.len());
            for group in page {
                let member_count = counts.get(&group.id).copied().unwrap_or(0);
                out.push(GroupSummary {
                    group_id: group.id,
                    trust: db_to_ipc_trust(group.trust_level),
                    best_file_id: group.best_file_id.map(|f| f.0),
                    member_count: u32::try_from(member_count).unwrap_or(u32::MAX),
                    intro_outro: intro_outro.get(&group.id).copied().unwrap_or(false),
                });
            }
            Ok(out)
        });
        match summaries {
            Ok(groups) => Reply::single(Response::Groups(groups)),
            Err(err) => internal(&err),
        }
    }

    fn group_detail(&self, group_id: i64) -> Reply {
        let records = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let files = FilesRepo::new(db.conn());
            let best = groups.get(group_id)?.and_then(|g| g.best_file_id);
            let mut records = Vec::new();
            for file_id in groups.list_members(group_id)? {
                if let Some(record) = files.get(file_id)? {
                    records.push((record, best == Some(file_id)));
                }
            }
            Ok(records)
        });
        match records {
            Ok(records) => {
                let members: Vec<FileDetail> = records
                    .into_iter()
                    .map(|(record, is_best)| {
                        let thumbnail = self.thumbnails.as_ref().and_then(|provider| {
                            provider.data_uri(
                                &record.path.to_native_path(),
                                record.content_hash.as_ref(),
                            )
                        });
                        file_record_to_detail(&record, is_best, thumbnail)
                    })
                    .collect();
                let frames = members
                    .chunks(DETAIL_CHUNK_SIZE)
                    .map(|chunk| Response::GroupDetail(chunk.to_vec()))
                    .collect();
                Reply::stream(frames)
            }
            Err(err) => internal_as_stream(&err),
        }
    }

    fn group_stats(&self, trust: Option<IpcTrust>) -> Reply {
        let want = trust.map(ipc_to_db_trust);
        let stats = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let mut members_by_group: HashMap<i64, Vec<(FileId, i64)>> = HashMap::new();
            for (group_id, file_id, size_bytes) in groups.all_member_sizes()? {
                members_by_group
                    .entry(group_id)
                    .or_default()
                    .push((file_id, size_bytes));
            }
            let mut group_count: u64 = 0;
            let mut reclaimable: u64 = 0;
            for group in groups.list_all()? {
                if want.is_some_and(|t| group.trust_level != t) {
                    continue;
                }
                group_count += 1;
                let mut all_sizes: Vec<i64> = Vec::new();
                let mut best_size: Option<i64> = None;
                if let Some(members) = members_by_group.get(&group.id) {
                    for &(file_id, size_bytes) in members {
                        all_sizes.push(size_bytes);
                        if Some(file_id) == group.best_file_id {
                            best_size = Some(size_bytes);
                        }
                    }
                }
                reclaimable += calculate_reclaimable(&all_sizes, best_size);
            }
            Ok(GroupStats {
                group_count,
                reclaimable_bytes: reclaimable,
            })
        });
        match stats {
            Ok(stats) => Reply::single(Response::GroupStats(stats)),
            Err(err) => internal(&err),
        }
    }

    fn cluster_summaries(&self, trust: Option<IpcTrust>, limit: u32, offset: u32) -> Reply {
        let want = trust.map(ipc_to_db_trust);
        let summaries = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let edges_repo = SimilarityEdgesRepo::new(db.conn());
            let build_started = std::time::Instant::now();
            let clusters = build_clusters(db);
            record_build_clusters_call(build_started.elapsed());
            let page: Vec<Cluster> = clusters?
                .into_iter()
                .filter(|c| want.is_none_or(|t| c.representative_trust == t))
                .skip(offset as usize)
                .take(limit as usize)
                .collect();
            let all_group_ids: Vec<i64> = page
                .iter()
                .flat_map(|c| c.group_ids.iter().copied())
                .collect();
            let intro_outro = edges_repo.all_tagged_intro_outro(&all_group_ids)?;
            let mut out = Vec::new();
            for cluster in page {
                let best_file_id = cluster_best(&groups, &cluster)?;
                let members = cluster_member_details(db, &cluster, best_file_id)?;
                if members.is_empty() {
                    continue;
                }
                out.push(ClusterSummary {
                    cluster_id: cluster_id_of(&cluster),
                    representative_trust: db_to_ipc_trust(cluster.representative_trust),
                    best_file_id,
                    member_count: u32::try_from(cluster.members.len()).unwrap_or(u32::MAX),
                    member_trust_levels: member_trust_levels(&cluster),
                    intro_outro: cluster_all_tagged_intro_outro(&cluster, &intro_outro),
                    members,
                });
            }
            Ok(out)
        });
        match summaries {
            Ok(summaries) => Reply::single(Response::ClusterSummaries(summaries)),
            Err(err) => internal(&err),
        }
    }

    fn cluster_stats(&self, trust: Option<IpcTrust>) -> Reply {
        let want = trust.map(ipc_to_db_trust);
        let stats = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let files = FilesRepo::new(db.conn());
            let mut cluster_count: u64 = 0;
            let mut reclaimable: u64 = 0;
            let build_started = std::time::Instant::now();
            let clusters = build_clusters(db);
            record_build_clusters_call(build_started.elapsed());
            for cluster in clusters? {
                if want.is_some_and(|t| cluster.representative_trust != t) {
                    continue;
                }
                cluster_count += 1;
                let best_file_id = cluster_best(&groups, &cluster)?;
                let mut all_sizes: Vec<i64> = Vec::new();
                let mut best_size: Option<i64> = None;
                for member in &cluster.members {
                    if let Some(record) = files.get(member.file_id)? {
                        all_sizes.push(record.size_bytes);
                        if Some(member.file_id.0) == best_file_id {
                            best_size = Some(record.size_bytes);
                        }
                    }
                }
                reclaimable += calculate_reclaimable(&all_sizes, best_size);
            }
            Ok(ClusterStats {
                cluster_count,
                reclaimable_bytes: reclaimable,
            })
        });
        match stats {
            Ok(stats) => Reply::single(Response::ClusterStats(stats)),
            Err(err) => internal(&err),
        }
    }

    fn cluster_detail(&self, cluster_id: i64) -> Reply {
        let prepared = self.with_db(|db| {
            let build_started = std::time::Instant::now();
            let clusters = build_clusters(db);
            record_build_clusters_call(build_started.elapsed());
            let Some(cluster) = clusters?
                .into_iter()
                .find(|c| cluster_id_of(c) == cluster_id)
            else {
                return Ok(None);
            };
            let groups = DuplicateGroupsRepo::new(db.conn());
            let best = cluster_best(&groups, &cluster)?;
            Ok(Some(cluster_member_details(db, &cluster, best)?))
        });
        match prepared {
            Ok(Some(members)) => {
                let frames = members
                    .chunks(DETAIL_CHUNK_SIZE)
                    .map(|chunk| Response::ClusterDetail(chunk.to_vec()))
                    .collect();
                Reply::stream(frames)
            }
            Ok(None) => Reply::stream(Vec::new()),
            Err(err) => internal_as_stream(&err),
        }
    }

    fn thumbnail(&self, file_id: i64) -> Reply {
        let located = self.with_db(|db| {
            let files = FilesRepo::new(db.conn());
            Ok(files
                .get(FileId(file_id))?
                .map(|record| (record.path.to_native_path(), record.content_hash)))
        });
        match located {
            Ok(Some((path, content_hash))) => {
                let uri = self
                    .thumbnails
                    .as_ref()
                    .and_then(|provider| provider.data_uri(&path, content_hash.as_ref()));
                Reply::single(Response::Thumbnail(uri))
            }
            Ok(None) => Reply::single(Response::Thumbnail(None)),
            Err(err) => internal(&err),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn delete_files(&self, req: &DeleteRequest, mode: DeleteMode) -> Reply {
        let group_id = req.group_id;
        let selected: Vec<FileId> = req.file_ids.iter().map(|&id| FileId(id)).collect();

        let prepared = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let files = FilesRepo::new(db.conn());
            let members = groups.list_members(group_id)?;
            let group = groups.get(group_id)?;
            let best = group.as_ref().and_then(|g| g.best_file_id);
            let to_delete = match plan_deletion(&members, best, &selected, req.confirm_best) {
                Ok(ids) => ids,
                Err(reject) => return Ok(Err(reject)),
            };
            let Some(group) = group else {
                return Ok(Err(DeleteReject::UnknownMember));
            };
            let mut targets: Vec<(FileId, NormalizedPath, i64, i64)> = Vec::new();
            for id in &to_delete {
                if let Some(record) = files.get(*id)? {
                    targets.push((*id, record.path.clone(), record.size_bytes, record.mtime_ns));
                }
            }
            Ok(Ok((targets, group.trust_level, group.non_transitive, best)))
        });
        let (targets, trust, non_transitive, best) = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(reject)) => return delete_rejected(reject),
            Err(err) => return internal(&err),
        };

        let now = unix_now();
        let pending_targets: Vec<(FileId, String)> = targets
            .iter()
            .map(|(id, path, _, _)| (*id, path.as_str().to_owned()))
            .collect();
        let pending = self.with_db_mut(|db| {
            db.transaction(|conn| {
                DeleteJournalRepo::new(conn).record_pending(
                    group_id,
                    trust,
                    non_transitive,
                    best,
                    journal_mode(mode),
                    &pending_targets,
                    now,
                )
            })
        });
        let pending_batch_id = match pending {
            Ok(id) => id,
            Err(err) => return internal(&err),
        };

        let mut removed: Vec<FileId> = Vec::new();
        let mut reclaimed: u64 = 0;
        let mut failures: Vec<String> = Vec::new();
        let mut trashed_count: usize = 0;
        for (id, path, size, mtime_ns) in &targets {
            if path_unsupported_for_delete(path.as_str()) {
                failures.push(format!(
                    "{path}: 지원하지 않는 경로(UNC/확장 경로) — 수동 확인 필요"
                ));
                continue;
            }
            let native = path.to_native_path();
            match std::fs::metadata(&native) {
                Ok(meta) => {
                    let size_now = i64::try_from(meta.len()).unwrap_or(i64::MAX);
                    let mtime_now = fs_mtime_ns(&meta);
                    if size_now != *size || mtime_now != Some(*mtime_ns) {
                        failures.push(format!("{path}: 변경됨 — 재스캔 필요"));
                        continue;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    failures.push(format!("{path}: 변경됨 — 재스캔 필요"));
                    continue;
                }
            }
            match self.remover.remove(&native, mode) {
                Ok(outcome) => {
                    tracing::info!(
                        stage = "delete",
                        file = %crate::redact::redact_path(path.as_str()),
                        mode = ?mode,
                        outcome = ?outcome,
                        size_bytes = *size,
                        group_id = ?group_id,
                        "file removed (audit)",
                    );
                    if outcome == crate::delete::RemoveOutcome::Trashed {
                        trashed_count += 1;
                    }
                    removed.push(*id);
                    reclaimed += u64::try_from(*size).unwrap_or(0);
                }
                Err(err) => failures.push(format!("{path}: {err}")),
            }
        }
        if removed.is_empty() {
            let _ = self.with_db_mut(|db| {
                db.transaction(|conn| DeleteJournalRepo::new(conn).remove(pending_batch_id))
            });
            return delete_failed(format!("삭제 실패: {}", failures.join("; ")));
        }

        let removed_set: std::collections::BTreeSet<FileId> = removed.iter().copied().collect();
        let journaled = self.with_db_mut(|db| {
            db.transaction(|conn| {
                let files = FilesRepo::new(conn);
                let groups = DuplicateGroupsRepo::new(conn);
                let journal_files: Vec<(FileId, String, BatchFileRole)> = {
                    let mut out = Vec::new();
                    for id in groups.list_members(group_id)? {
                        if let Some(record) = files.get(id)? {
                            let role = if removed_set.contains(&id) {
                                BatchFileRole::Deleted
                            } else {
                                BatchFileRole::Survivor
                            };
                            out.push((id, record.path.as_str().to_owned(), role));
                        }
                    }
                    out
                };
                for id in &removed {
                    files.mark_deleted(*id, now)?;
                    groups.remove_member(group_id, *id)?;
                }
                let remaining = groups.list_members(group_id)?.len();
                let survives = if remaining < 2 {
                    groups.delete(group_id)?;
                    false
                } else {
                    true
                };
                DeleteJournalRepo::new(conn)
                    .finalize_committed(pending_batch_id, !survives, &journal_files)
                    .map(|()| pending_batch_id)
            })
        });
        let batch_id = match journaled {
            Ok(batch_id) => batch_id,
            Err(err) => return internal(&err),
        };

        if let Err(err) = self.with_db_mut(|db| assign_best_copies(db, now).map(|_| ())) {
            return internal(&err);
        }

        self.maybe_snapshot(batch_id);

        let n = removed.len();
        let base = match mode {
            DeleteMode::Trash if trashed_count == n => {
                format!("{n}개 파일을 휴지통으로 이동했습니다.")
            }
            DeleteMode::Trash if trashed_count == 0 => {
                format!("{n}개 파일을 영구 삭제했습니다(휴지통 이동 불가).")
            }
            DeleteMode::Trash => {
                let hard = n - trashed_count;
                format!(
                    "{trashed_count}개 파일을 휴지통으로 이동, \
                     {hard}개 파일을 영구 삭제했습니다(휴지통 이동 불가)."
                )
            }
            DeleteMode::Permanent => format!("{n}개 파일을 영구 삭제했습니다."),
        };
        let detail = if failures.is_empty() {
            base
        } else {
            format!("{base} ({}개 실패)", failures.len())
        };
        Reply::single(Response::Delete(DeleteResult {
            ok: true,
            removed_file_ids: removed.iter().map(|f| f.0).collect(),
            reclaimed_bytes: reclaimed,
            detail,
            reject_code: None,
        }))
    }

    fn maybe_snapshot(&self, batch_id: i64) {
        let Some(dir) = &self.backup_dir else {
            return;
        };
        if batch_id % SNAPSHOT_EVERY_BATCHES != 0 {
            return;
        }
        let result = self.with_db(|db| crate::backup::snapshot_into(db, dir, unix_now()));
        match result {
            Ok(path) => {
                tracing::info!(path = %crate::redact::redact_fs_path(&path), batch_id, "post-delete index snapshot written");
            }
            Err(err) => {
                tracing::warn!(error = %err, batch_id, "post-delete index snapshot failed; continuing");
            }
        }
    }

    fn undo_last_delete(&self) -> Reply {
        let now = unix_now();
        let undone = match self.with_db(|db| DeleteJournalRepo::new(db.conn()).last()) {
            Err(err) => Err(err),
            Ok(None) => Ok(None),
            Ok(Some(batch)) => {
                let mut restorable: Vec<FileId> = Vec::new();
                let mut missing: Vec<String> = Vec::new();
                for file in &batch.files {
                    if file.role != BatchFileRole::Deleted {
                        continue;
                    }
                    if NormalizedPath::new(&file.path).to_native_path().exists() {
                        restorable.push(file.file_id);
                    } else {
                        missing.push(file.path.clone());
                    }
                }
                if restorable.is_empty() {
                    Ok(Some((batch, Vec::new(), missing)))
                } else {
                    let restorable_set: std::collections::BTreeSet<FileId> =
                        restorable.iter().copied().collect();
                    self.with_db_mut(|db| {
                        db.transaction(|conn| {
                            let files = FilesRepo::new(conn);
                            let groups = DuplicateGroupsRepo::new(conn);
                            if groups.get(batch.group_id)?.is_none() {
                                groups.create_with_id(
                                    batch.group_id,
                                    batch.trust_level,
                                    batch.non_transitive,
                                    now,
                                )?;
                            }
                            for file in &batch.files {
                                match file.role {
                                    BatchFileRole::Survivor => {
                                        groups
                                            .add_member_if_absent(batch.group_id, file.file_id)?;
                                    }
                                    BatchFileRole::Deleted
                                        if restorable_set.contains(&file.file_id) =>
                                    {
                                        files.clear_deleted(file.file_id)?;
                                        groups
                                            .add_member_if_absent(batch.group_id, file.file_id)?;
                                    }
                                    BatchFileRole::Deleted => {}
                                }
                            }
                            DeleteJournalRepo::new(conn).remove(batch.id)
                        })?;
                        assign_best_copies(db, now)?;
                        Ok(Some((batch, restorable, missing)))
                    })
                }
            }
        };
        match undone {
            Err(err) => internal(&err),
            Ok(None) => Reply::single(Response::Undo(UndoResult {
                ok: false,
                group_id: None,
                restored_file_ids: Vec::new(),
                missing_paths: Vec::new(),
                detail: "되돌릴 삭제 내역이 없습니다.".to_owned(),
            })),
            Ok(Some((_, restored, missing))) if restored.is_empty() => {
                Reply::single(Response::Undo(UndoResult {
                    ok: false,
                    group_id: None,
                    restored_file_ids: Vec::new(),
                    missing_paths: missing,
                    detail: "복원할 파일이 디스크에 없습니다. 휴지통에서 파일을 먼저 복원한 뒤 \
                             다시 시도하세요."
                        .to_owned(),
                }))
            }
            Ok(Some((batch, restored, missing))) => {
                let detail = if missing.is_empty() {
                    format!("{}개 파일을 복원했습니다.", restored.len())
                } else {
                    format!(
                        "{}개 파일을 복원했습니다. ({}개는 디스크에 없어 건너뜀)",
                        restored.len(),
                        missing.len()
                    )
                };
                Reply::single(Response::Undo(UndoResult {
                    ok: true,
                    group_id: Some(batch.group_id),
                    restored_file_ids: restored.iter().map(|f| f.0).collect(),
                    missing_paths: missing,
                    detail,
                }))
            }
        }
    }

    fn partial_overlaps(&self, group_id: i64) -> Reply {
        let overlaps = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let fingerprints = FingerprintsRepo::new(db.conn());
            let edges_repo = SimilarityEdgesRepo::new(db.conn());
            let pmih = PartialMihRepo::new(db.conn());
            let members = groups.list_members(group_id)?;
            let member_set: std::collections::BTreeSet<FileId> = members.iter().copied().collect();
            let is_possible = groups
                .get(group_id)?
                .is_some_and(|g| g.trust_level == DbTrust::Possible);

            let edges: Vec<_> = edges_repo
                .list_for_group(group_id)?
                .into_iter()
                .filter(|e| member_set.contains(&e.file_a) && member_set.contains(&e.file_b))
                .collect();
            let spans_complete = !edges.is_empty()
                && edges
                    .iter()
                    .all(|e| e.partial_span.is_some_and(|s| s.clip_scenes > 0));

            if spans_complete {
                let mut out = Vec::with_capacity(edges.len());
                for edge in &edges {
                    let Some(span) = edge.partial_span else {
                        continue;
                    };
                    let (clip, source) = orient_clip_source(&pmih, edge.file_a, edge.file_b)?;
                    out.push(ClipOverlap {
                        clip_file_id: clip.0,
                        source_file_id: source.0,
                        matched_scenes: u32::try_from(span.matched_scenes).unwrap_or(u32::MAX),
                        clip_scenes: u32::try_from(span.clip_scenes).unwrap_or(u32::MAX),
                        start_ms: span.source_start_ms,
                        end_ms: span.source_end_ms,
                        clip_start_ms: span.clip_start_ms,
                        clip_end_ms: span.clip_end_ms,
                        intro_outro: edge.intro_outro,
                    });
                }
                tracing::debug!(
                    group_id,
                    overlaps = out.len(),
                    "partial_overlaps persisted spans"
                );
                return Ok(out);
            }

            let mut corpus = Vec::new();
            for (id, blob) in fingerprints.list_active_partial()? {
                if is_possible || member_set.contains(&id) {
                    corpus.push((id, decode_tier2(&blob)?));
                }
            }
            let corpus_size = corpus.len();
            let plan = plan_partial_clips(corpus, partial_clip_params());
            tracing::debug!(
                group_id,
                corpus_size,
                matches = plan.matches.len(),
                skipped_short = plan.skipped_short,
                dropped_single_vote = plan.dropped_single_vote,
                "partial_overlaps recompute"
            );
            let files = FilesRepo::new(db.conn());
            let mut out = Vec::new();
            for m in plan.matches {
                if member_set.contains(&m.clip) && member_set.contains(&m.alignment.source) {
                    let clip_dur_ms = files
                        .get(m.clip)?
                        .and_then(|f| f.duration)
                        .map(VideoDuration::as_millis);
                    let source_dur_ms = files
                        .get(m.alignment.source)?
                        .and_then(|f| f.duration)
                        .map(VideoDuration::as_millis);
                    out.push(ClipOverlap {
                        clip_file_id: m.clip.0,
                        source_file_id: m.alignment.source.0,
                        matched_scenes: u32::try_from(m.alignment.matched_scenes)
                            .unwrap_or(u32::MAX),
                        clip_scenes: u32::try_from(m.alignment.clip_scenes).unwrap_or(u32::MAX),
                        start_ms: m.alignment.start_ms,
                        end_ms: m.alignment.end_ms,
                        clip_start_ms: m.alignment.clip_start_ms,
                        clip_end_ms: m.alignment.clip_end_ms,
                        intro_outro: is_intro_outro(&m.alignment, clip_dur_ms, source_dur_ms),
                    });
                }
            }
            Ok(out)
        });
        match overlaps {
            Ok(overlaps) => Reply::single(Response::PartialOverlaps(overlaps)),
            Err(err) => internal(&err),
        }
    }

    fn cross_group_conflicts(&self, group_id: i64) -> Reply {
        let conflicts = self.with_db(|db| {
            let groups = DuplicateGroupsRepo::new(db.conn());
            let files = FilesRepo::new(db.conn());
            let mut out = Vec::new();
            for file_id in groups.list_members(group_id)? {
                let containing = groups.find_groups_containing(file_id)?;
                if containing.len() < 2 {
                    continue;
                }
                let mut any_best = false;
                let mut any_candidate = false;
                let memberships: Vec<GroupRole> = containing
                    .iter()
                    .map(|g| {
                        let is_best = g.best_file_id == Some(file_id);
                        if is_best {
                            any_best = true;
                        } else {
                            any_candidate = true;
                        }
                        GroupRole {
                            group_id: g.id,
                            trust: db_to_ipc_trust(g.trust_level),
                            is_best,
                        }
                    })
                    .collect();
                if any_best && any_candidate {
                    if let Some(record) = files.get(file_id)? {
                        out.push(CrossGroupConflict {
                            file_id: file_id.0,
                            path: record.path.as_str().to_owned(),
                            memberships,
                        });
                    }
                }
            }
            Ok(out)
        });
        match conflicts {
            Ok(conflicts) => Reply::single(Response::CrossGroupConflicts(conflicts)),
            Err(err) => internal(&err),
        }
    }

    fn action(&self, action: Action) -> Reply {
        match action {
            Action::Shutdown => {
                self.shutdown.trigger();
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: "shutdown requested; draining in-flight work".to_owned(),
                }))
            }
            Action::Rescan { path } => self.enqueue_rescan(&path),
            Action::ForceRescan { path } => self.enqueue_force_rescan(&path),
            Action::MoveToTrash(req) => self.delete_files(&req, DeleteMode::Trash),
            Action::DeletePermanent(req) => self.delete_files(&req, DeleteMode::Permanent),
            Action::SetSettings(settings) => self.set_settings(&settings),
            Action::UndoLastDelete => self.undo_last_delete(),
            Action::SetLogLevel(level) => Self::set_log_level(level),
            Action::ExportDiagnostics { dest } => Self::export_diagnostics(&dest),
        }
    }

    fn set_log_level(level: vidcull_ipc::protocol::LogLevel) -> Reply {
        match crate::logctl::set_log_level(level) {
            Ok(()) => {
                tracing::info!(stage = "logctl", level = ?level, "runtime log level changed");
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("log level set to {level:?}"),
                }))
            }
            Err(err) => {
                tracing::warn!(stage = "logctl", error = %err, "runtime log level change failed");
                Reply::single(Response::Action(ActionResult {
                    accepted: false,
                    detail: format!("could not set log level: {err}"),
                }))
            }
        }
    }

    fn export_diagnostics(dest: &str) -> Reply {
        let logs_dir = crate::settings::data_dir().join("logs");
        match crate::diagnostics::collect_diagnostic_bundle(&logs_dir, std::path::Path::new(dest)) {
            Ok(files) => {
                tracing::info!(
                    stage = "diagnostics",
                    count = files.len(),
                    "diagnostic bundle exported",
                );
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("{} log file(s) exported", files.len()),
                }))
            }
            Err(err) => {
                tracing::warn!(stage = "diagnostics", error = %err, "diagnostic bundle export failed");
                Reply::single(Response::Action(ActionResult {
                    accepted: false,
                    detail: format!("export failed: {err}"),
                }))
            }
        }
    }

    fn enqueue_rescan(&self, path: &str) -> Reply {
        let p = std::path::Path::new(path);
        let normalized_path = NormalizedPath::new(p);

        let enqueued = self.with_db_mut(|db| {
            let mut changes = Vec::new();
            if p.is_dir() {
                let options = vidcull_scanner::ScanOptions::default();
                let mut current_map = std::collections::HashMap::new();
                for entry in vidcull_scanner::walk(p, &options).flatten() {
                    current_map.insert(entry.path.clone(), entry);
                }

                let files_repo = FilesRepo::new(db.conn());
                let mut db_entries = std::collections::BTreeMap::new();
                for record in files_repo.list_active_under_root(&normalized_path)? {
                    let fp = vidcull_scanner::FsFingerprint::new(
                        u64::try_from(record.size_bytes).unwrap_or(0),
                        i128::from(record.mtime_ns),
                        record.inode.and_then(|i| u64::try_from(i).ok()),
                    );
                    db_entries.insert(record.path.clone(), fp);
                }

                let diff_result = vidcull_scanner::diff(db_entries, current_map.into_values());

                for entry in diff_result.added {
                    let size_bytes =
                        i64::try_from(entry.fingerprint.size_bytes).unwrap_or(i64::MAX);
                    changes.push(ChangeTask {
                        path: entry.path,
                        change: ChangeKind::Upsert,
                        size_bytes,
                    });
                }
                for entry in diff_result.modified {
                    let size_bytes =
                        i64::try_from(entry.current.fingerprint.size_bytes).unwrap_or(i64::MAX);
                    changes.push(ChangeTask {
                        path: entry.current.path,
                        change: ChangeKind::Upsert,
                        size_bytes,
                    });
                }
                for path in diff_result.removed {
                    changes.push(ChangeTask {
                        path,
                        change: ChangeKind::Remove,
                        size_bytes: 0,
                    });
                }
            } else {
                let size_bytes =
                    crate::watcher::change_size_bytes(&normalized_path, ChangeKind::Upsert);
                changes.push(ChangeTask {
                    path: normalized_path,
                    change: ChangeKind::Upsert,
                    size_bytes,
                });
            }

            crate::watcher::enqueue_changes(db, &changes, &self.task_kind, 0, unix_now())
        });

        match enqueued {
            Ok(count) => Reply::single(Response::Action(ActionResult {
                accepted: true,
                detail: format!("enqueued {count} rescan tasks"),
            })),
            Err(err) => internal(&err),
        }
    }

    fn enqueue_force_rescan(&self, path: &str) -> Reply {
        let p = std::path::Path::new(path);
        let enqueued = self.with_db_mut(|db| {
            let mut changes = Vec::new();
            if p.is_dir() {
                let options = vidcull_scanner::ScanOptions::default();
                for entry in vidcull_scanner::walk(p, &options).flatten() {
                    let size_bytes =
                        i64::try_from(entry.fingerprint.size_bytes).unwrap_or(i64::MAX);
                    changes.push(ChangeTask {
                        path: entry.path,
                        change: ChangeKind::ForceUpsert,
                        size_bytes,
                    });
                }
                if p.is_dir() {
                    let on_disk: std::collections::HashSet<String> =
                        changes.iter().map(|t| t.path.as_str().to_owned()).collect();
                    let root_norm = NormalizedPath::new(p);
                    let mut removes = reconcile_deleted_under_root(db, &root_norm, &on_disk)?;
                    changes.append(&mut removes);
                }
            } else {
                let normalized_path = NormalizedPath::new(p);
                let size_bytes =
                    crate::watcher::change_size_bytes(&normalized_path, ChangeKind::ForceUpsert);
                changes.push(ChangeTask {
                    path: normalized_path,
                    change: ChangeKind::ForceUpsert,
                    size_bytes,
                });
            }
            force_rescan_teardown(db, &changes)?;
            crate::watcher::enqueue_changes(db, &changes, &self.task_kind, 0, unix_now())
        });

        match enqueued {
            Ok(count) => {
                tracing::info!(
                    stage = "force_rescan",
                    path = %crate::redact::redact_path(path),
                    enqueued = %count,
                    "force rescan requested (audit)",
                );
                Reply::single(Response::Action(ActionResult {
                    accepted: true,
                    detail: format!("enqueued {count} force-rescan tasks"),
                }))
            }
            Err(err) => internal(&err),
        }
    }

    fn stream_logs(&self, max_records: u32) -> Reply {
        let records = self.logs.snapshot(max_records as usize);
        Reply::stream(records.into_iter().map(Response::Log).collect())
    }

    fn failed_tasks(&self, limit: u32) -> Reply {
        let tasks = self.with_db(|db| {
            let repo = TaskQueueRepo::new(db.conn());
            let files = FilesRepo::new(db.conn());
            let failed_rows: Vec<(vidcull_db::repo::Task, String)> = repo
                .list_by_state(TaskState::Failed)?
                .into_iter()
                .filter(|task| task.priority >= 0)
                .filter_map(|task| task_path(&task).map(|path| (task, path)))
                .collect();
            let failed_paths: Vec<NormalizedPath> = failed_rows
                .iter()
                .map(|(_, path)| NormalizedPath::new(path.as_str()))
                .collect();
            let active_hashed = files.active_hashed_paths_in(&failed_paths)?;
            let mut by_path: std::collections::HashMap<String, FailedTask> =
                std::collections::HashMap::new();
            for (task, path) in failed_rows {
                if active_hashed.contains(NormalizedPath::new(path.as_str()).as_str()) {
                    continue;
                }
                let attempts = u32::try_from(task.attempts).unwrap_or(0);
                let reason = task.last_error.unwrap_or_else(|| "task failed".to_owned());
                match by_path.get_mut(&path) {
                    Some(entry) => {
                        entry.attempts = entry.attempts.saturating_add(attempts);
                        entry.task_id = task.id;
                        entry.reason = reason;
                    }
                    None => {
                        by_path.insert(
                            path.clone(),
                            FailedTask {
                                task_id: task.id,
                                path,
                                reason,
                                attempts,
                            },
                        );
                    }
                }
            }
            let mut out: Vec<FailedTask> = by_path.into_values().collect();
            out.sort_by(|a, b| b.task_id.cmp(&a.task_id));
            out.truncate(limit as usize);
            Ok(out)
        });
        match tasks {
            Ok(tasks) => Reply::single(Response::FailedTasks(tasks)),
            Err(err) => internal(&err),
        }
    }

    fn get_settings(&self) -> Reply {
        let settings = self
            .with_db(|db| Ok(crate::settings::load(db)))
            .unwrap_or_default();
        Reply::single(Response::Settings(settings))
    }

    fn set_settings(&self, settings: &DaemonSettings) -> Reply {
        let previous = self
            .with_db(|db| Ok(crate::settings::load(db)))
            .unwrap_or_default();
        match self.with_db(|db| crate::settings::save(db, settings)) {
            Ok(()) => {
                self.hide_removed_folders(&previous, settings);
                self.scan_new_folders(&previous, settings);
                self.apply_cpu_throttle(settings);
                self.apply_worker_count(settings);
                self.apply_storage_class(settings);
                self.apply_autostart(settings);
                self.apply_partial_clips(settings);
                self.apply_indexing_enabled(settings);
                self.apply_best_copy_mode(&previous, settings);
                if settings.partial_clips_enabled && !previous.partial_clips_enabled {
                    self.backfill_partial_clips();
                }
                let mut echo = settings.clone();
                echo.cpu_cores = crate::settings::live_cores();
                Reply::single(Response::Settings(echo))
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to persist settings");
                internal(&err)
            }
        }
    }

    fn apply_cpu_throttle(&self, settings: &DaemonSettings) {
        self.throttle_control.set_level(settings.cpu_throttle);
        let priority = if self.throttle_control.is_max_performance() {
            crate::priority::restore_normal_priority()
        } else {
            crate::priority::lower_process_priority()
        };
        if let Err(err) = priority {
            tracing::warn!(error = %err, "could not adjust process priority for CPU throttle");
        }
    }

    fn apply_worker_count(&self, settings: &DaemonSettings) {
        self.throttle_control
            .set_idle_workers(crate::settings::clamp_idle_workers(
                settings.idle_worker_count,
            ));
    }

    fn apply_storage_class(&self, settings: &DaemonSettings) {
        self.throttle_control
            .set_io_budget_cap(crate::storage::detect_io_budget_cap(&settings.scan_folders));
    }

    fn apply_autostart(&self, settings: &DaemonSettings) {
        let Some(command) = &self.autostart_command else {
            return;
        };
        let autostart = crate::Autostart::system();
        match autostart.sync(settings.run_on_boot, AUTOSTART_NAME, command) {
            Ok(()) => tracing::info!(
                run_on_boot = settings.run_on_boot,
                "synced OS autostart registration"
            ),
            Err(err) => {
                tracing::warn!(error = %err, "could not sync OS autostart registration");
            }
        }
    }

    fn apply_partial_clips(&self, settings: &DaemonSettings) {
        self.throttle_control
            .set_partial_clips(settings.partial_clips_enabled);
    }

    fn apply_indexing_enabled(&self, settings: &DaemonSettings) {
        self.throttle_control
            .set_indexing_enabled(settings.indexing_enabled);
    }

    fn apply_best_copy_mode(&self, previous: &DaemonSettings, settings: &DaemonSettings) {
        if settings.best_copy_mode == previous.best_copy_mode {
            return;
        }
        let now = unix_now();
        if let Err(err) = self.with_db_mut(|db| assign_best_copies(db, now).map(|_| ())) {
            tracing::warn!(error = %err, "could not re-pick best copies after best-copy-mode change");
        }
    }

    fn backfill_partial_clips(&self) {
        let result = self.with_db_mut(|db| {
            crate::watcher::enqueue_partial_backfill(db, &self.task_kind, unix_now())
        });
        match result {
            Ok(count) => tracing::info!(
                count,
                "backfilled partial-clip fingerprint pass for files indexed before the toggle"
            ),
            Err(err) => {
                tracing::warn!(error = %err, "failed to backfill partial-clip fingerprints");
            }
        }
    }

    fn scan_new_folders(&self, old: &DaemonSettings, new: &DaemonSettings) {
        let added: Vec<std::path::PathBuf> = new
            .scan_folders
            .iter()
            .filter(|folder| !old.scan_folders.contains(*folder))
            .map(std::path::PathBuf::from)
            .collect();
        if added.is_empty() {
            return;
        }
        for folder in &added {
            self.throttle_control
                .clear_removed_root_overlap(&NormalizedPath::new(folder));
        }
        if let Some(executor) = &self.scan_executor {
            executor.submit(added.clone(), new.exclude_rules.clone());
            tracing::info!(
                folders = added.len(),
                "submitted scan of newly added folders (background)"
            );
            return;
        }
        let kind = self.task_kind.clone();
        let result = self.with_db_mut(|db| {
            crate::watcher::enqueue_initial_scan(db, &added, &kind, unix_now(), &new.exclude_rules)
        });
        match result {
            Ok(count) => {
                tracing::info!(
                    count,
                    folders = added.len(),
                    "enqueued scan of newly added folders"
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to enqueue scan of newly added folders");
            }
        }
    }

    fn hide_removed_folders(&self, old: &DaemonSettings, new: &DaemonSettings) {
        let removed: Vec<NormalizedPath> = old
            .scan_folders
            .iter()
            .filter(|folder| !new.scan_folders.contains(*folder))
            .map(|folder| NormalizedPath::new(folder.clone()))
            .collect();
        if removed.is_empty() {
            return;
        }
        let kind = self.task_kind.clone();
        let now = unix_now();
        let result = self.with_db_mut(|db| {
            db.transaction(|conn| {
                let files = FilesRepo::new(conn);
                let queue = TaskQueueRepo::new(conn);

                let mut changes = Vec::new();
                for root in &removed {
                    for path in files.list_active_paths_under_root(root)? {
                        changes.push(ChangeTask {
                            path,
                            change: ChangeKind::Remove,
                            size_bytes: 0,
                        });
                    }
                }

                let under_removed = |task: &vidcull_db::repo::Task| -> bool {
                    task.payload
                        .as_deref()
                        .and_then(|bytes| ChangeTask::from_payload(bytes).ok())
                        .is_some_and(|change| {
                            change.change != ChangeKind::Remove
                                && removed.iter().any(|root| {
                                    path_under_root(change.path.as_str(), root.as_str())
                                })
                        })
                };

                let mut failed_to_delete = Vec::new();
                for task in queue.list_by_state(TaskState::Failed)? {
                    let hit = task
                        .payload
                        .as_deref()
                        .and_then(|bytes| ChangeTask::from_payload(bytes).ok())
                        .is_some_and(|change| {
                            removed
                                .iter()
                                .any(|root| path_under_root(change.path.as_str(), root.as_str()))
                        });
                    if hit {
                        failed_to_delete.push(task.id);
                    }
                }
                let mut pending_to_cancel = Vec::new();
                for task in queue.list_by_state(TaskState::Pending)? {
                    if under_removed(&task) {
                        pending_to_cancel.push(task.id);
                    }
                }
                let mut running_to_cancel = Vec::new();
                for task in queue.list_by_state(TaskState::Running)? {
                    if under_removed(&task) {
                        running_to_cancel.push(task.id);
                    }
                }

                let enqueued =
                    crate::watcher::enqueue_changes_into(&queue, &changes, &kind, 0, now)?;
                for id in &failed_to_delete {
                    queue.delete(*id)?;
                }
                let mut cancelled_pending = 0usize;
                for id in &pending_to_cancel {
                    if queue.delete_if_pending(*id)? {
                        cancelled_pending += 1;
                    }
                }
                let mut cancelled_running = 0usize;
                for id in &running_to_cancel {
                    queue.delete(*id)?;
                    cancelled_running += 1;
                }
                Ok((
                    enqueued,
                    failed_to_delete.len(),
                    cancelled_pending,
                    cancelled_running,
                ))
            })
        });
        match result {
            Ok((enqueued, cleared, cancelled_pending, cancelled_running)) => {
                self.throttle_control.mark_roots_removed(&removed);
                tracing::info!(
                    enqueued,
                    cleared_failures = cleared,
                    cancelled_pending,
                    cancelled_running,
                    folders = removed.len(),
                    "hid removed folders' files and cancelled their queued/in-flight tasks"
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to hide removed folders");
            }
        }
    }
}

pub(crate) fn reconcile_deleted_under_root(
    db: &Database,
    root: &NormalizedPath,
    on_disk: &std::collections::HashSet<String>,
) -> Result<Vec<ChangeTask>> {
    let db_active = FilesRepo::new(db.conn()).list_active_paths_under_root(root)?;
    let mut removes = Vec::new();
    for db_path in db_active {
        if !on_disk.contains(db_path.as_str()) {
            let native = db_path.to_native_path();
            if let Err(e) = std::fs::symlink_metadata(&native) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    let parent_ok = native.parent().is_some_and(std::path::Path::is_dir);
                    if parent_ok {
                        removes.push(ChangeTask {
                            path: db_path,
                            change: ChangeKind::Remove,
                            size_bytes: 0,
                        });
                    }
                }
            }
        }
    }
    Ok(removes)
}

pub(crate) fn force_rescan_teardown(db: &mut Database, changes: &[ChangeTask]) -> Result<usize> {
    let now = unix_now();
    db.transaction(|conn| {
        let files = FilesRepo::new(conn);
        let groups = DuplicateGroupsRepo::new(conn);
        let fingerprints = FingerprintsRepo::new(conn);
        let regroup = vidcull_db::repo::RegroupQueueRepo::new(conn);
        let mut torn_down = 0usize;
        for change in changes {
            if !matches!(change.change, ChangeKind::ForceUpsert) {
                continue;
            }
            let Some(existing) = files.find_by_path(&change.path)? else {
                continue;
            };
            for group in groups.find_groups_containing(existing.id)? {
                groups.remove_member(group.id, existing.id)?;
                if groups.list_members(group.id)?.len() < 2 {
                    groups.delete(group.id)?;
                }
            }
            fingerprints.delete(existing.id)?;
            files.clear_content_hash(existing.id)?;
            regroup.mark(existing.id, now)?;
            torn_down += 1;
        }
        Ok(torn_down)
    })
}

pub(crate) fn path_under_root(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(windows)]
fn path_unsupported_for_delete(path: &str) -> bool {
    matches!(
        path.strip_prefix("//"),
        Some(rest) if rest.starts_with("?/") || rest == "?" || rest.starts_with("./") || rest == "."
    )
}

#[cfg(not(windows))]
fn path_unsupported_for_delete(_path: &str) -> bool {
    false
}

fn fs_mtime_ns(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok()?;
    Some(crate::indexing::mtime_nanos(meta))
}

const SLOW_CALL_THRESHOLD_MS: u64 = 50;

fn duration_ms(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

static BUILD_CLUSTERS_CALLS: AtomicU64 = AtomicU64::new(0);

fn record_build_clusters_call(elapsed: std::time::Duration) {
    let call_no = BUILD_CLUSTERS_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let elapsed_ms = duration_ms(elapsed);
    if elapsed_ms > SLOW_CALL_THRESHOLD_MS {
        tracing::info!(call_no, elapsed_ms, "build_clusters call");
    } else {
        tracing::debug!(call_no, elapsed_ms, "build_clusters call");
    }
}

fn record_db_access(wait_ms: u64, hold_ms: u64) {
    if wait_ms > SLOW_CALL_THRESHOLD_MS || hold_ms > SLOW_CALL_THRESHOLD_MS {
        tracing::info!(wait_ms, hold_ms, "db lock wait/hold");
    } else {
        tracing::debug!(wait_ms, hold_ms, "db lock wait/hold");
    }
}

fn request_kind(request: &Request) -> &'static str {
    match request {
        Request::Ping => "ping",
        Request::Progress => "progress",
        Request::ListGroups { .. } => "list_groups",
        Request::Action(_) => "action",
        Request::StreamLogs { .. } => "stream_logs",
        Request::GroupDetail { .. } => "group_detail",
        Request::GroupStats { .. } => "group_stats",
        Request::PartialOverlaps { .. } => "partial_overlaps",
        Request::GetSettings => "get_settings",
        Request::ClusterSummaries { .. } => "cluster_summaries",
        Request::ClusterDetail { .. } => "cluster_detail",
        Request::ClusterStats { .. } => "cluster_stats",
        Request::FailedTasks { .. } => "failed_tasks",
        Request::CrossGroupConflicts { .. } => "cross_group_conflicts",
        Request::Thumbnail { .. } => "thumbnail",
    }
}

impl RequestHandler for DaemonRequestHandler {
    fn handle(&self, request: Request) -> Reply {
        let kind = request_kind(&request);
        let started = std::time::Instant::now();
        let reply = match request {
            Request::Ping => Reply::single(Response::Pong {
                protocol_version: PROTOCOL_VERSION,
            }),
            Request::Progress => self.progress(),
            Request::ListGroups {
                trust,
                limit,
                offset,
            } => self.list_groups(trust, limit, offset),
            Request::Action(action) => self.action(action),
            Request::StreamLogs { max_records } => self.stream_logs(max_records),
            Request::GroupDetail { group_id } => self.group_detail(group_id),
            Request::GroupStats { trust } => self.group_stats(trust),
            Request::PartialOverlaps { group_id } => self.partial_overlaps(group_id),
            Request::GetSettings => self.get_settings(),
            Request::ClusterSummaries {
                trust,
                limit,
                offset,
            } => self.cluster_summaries(trust, limit, offset),
            Request::ClusterDetail { cluster_id } => self.cluster_detail(cluster_id),
            Request::ClusterStats { trust } => self.cluster_stats(trust),
            Request::FailedTasks { limit } => self.failed_tasks(limit),
            Request::CrossGroupConflicts { group_id } => self.cross_group_conflicts(group_id),
            Request::Thumbnail { file_id } => self.thumbnail(file_id),
        };
        let elapsed_ms = duration_ms(started.elapsed());
        if elapsed_ms > SLOW_CALL_THRESHOLD_MS {
            tracing::info!(kind, elapsed_ms, "ipc handler duration");
        } else {
            tracing::debug!(kind, elapsed_ms, "ipc handler duration");
        }
        reply
    }
}

fn cluster_id_of(cluster: &Cluster) -> i64 {
    cluster.group_ids.first().copied().unwrap_or(i64::MAX)
}

fn cluster_member_details(
    db: &Database,
    cluster: &Cluster,
    best: Option<i64>,
) -> Result<Vec<ClusterMemberDetail>> {
    let groups = DuplicateGroupsRepo::new(db.conn());
    let files = FilesRepo::new(db.conn());
    let mut group_info: Vec<(i64, DbTrust, std::collections::BTreeSet<FileId>)> = Vec::new();
    for &gid in &cluster.group_ids {
        if let Some(group) = groups.get(gid)? {
            let members = groups.list_members(gid)?.into_iter().collect();
            group_info.push((gid, group.trust_level, members));
        }
    }

    let mut details = Vec::with_capacity(cluster.members.len());
    for member in &cluster.members {
        if let Some(record) = files.get(member.file_id)? {
            let group_id = resolve_member_group(&group_info, member.file_id, member.trust);
            details.push(ClusterMemberDetail {
                file: file_record_to_detail(&record, best == Some(member.file_id.0), None),
                trust: db_to_ipc_trust(member.trust),
                group_id,
            });
        }
    }
    Ok(details)
}

fn member_trust_levels(cluster: &Cluster) -> Vec<IpcTrust> {
    [DbTrust::Exact, DbTrust::VeryLikely, DbTrust::Possible]
        .into_iter()
        .filter(|&t| cluster.members.iter().any(|m| m.trust == t))
        .map(db_to_ipc_trust)
        .collect()
}

fn cluster_all_tagged_intro_outro(cluster: &Cluster, tagged: &HashMap<i64, bool>) -> bool {
    !cluster.group_ids.is_empty()
        && cluster
            .group_ids
            .iter()
            .all(|gid| tagged.get(gid).copied().unwrap_or(false))
}

pub(crate) fn cluster_best(
    groups: &DuplicateGroupsRepo<'_>,
    cluster: &Cluster,
) -> Result<Option<i64>> {
    let mut chosen: Option<(i64, i64)> = None;
    for &gid in &cluster.group_ids {
        if let Some(group) = groups.get(gid)? {
            if group.trust_level == cluster.representative_trust {
                if let Some(best) = group.best_file_id {
                    let keep = chosen.is_none_or(|(prev_gid, _)| gid < prev_gid);
                    if keep {
                        chosen = Some((gid, best.0));
                    }
                }
            }
        }
    }
    Ok(chosen.map(|(_, best)| best))
}

fn orient_clip_source(pmih: &PartialMihRepo<'_>, a: FileId, b: FileId) -> Result<(FileId, FileId)> {
    let ca = pmih.scene_count(a)?;
    let cb = pmih.scene_count(b)?;
    Ok(match (ca, cb) {
        (Some(ca), Some(cb)) if cb < ca => (b, a),
        _ => (a, b),
    })
}

fn resolve_member_group(
    group_info: &[(i64, DbTrust, std::collections::BTreeSet<FileId>)],
    file_id: FileId,
    trust: DbTrust,
) -> i64 {
    let mut at_trust: Option<i64> = None;
    let mut any: Option<i64> = None;
    for (gid, group_trust, members) in group_info {
        if !members.contains(&file_id) {
            continue;
        }
        any = Some(any.map_or(*gid, |g: i64| g.min(*gid)));
        if *group_trust == trust {
            at_trust = Some(at_trust.map_or(*gid, |g: i64| g.min(*gid)));
        }
    }
    at_trust.or(any).unwrap_or(i64::MAX)
}

fn journal_mode(mode: DeleteMode) -> DeleteBatchMode {
    match mode {
        DeleteMode::Trash => DeleteBatchMode::Trash,
        DeleteMode::Permanent => DeleteBatchMode::Permanent,
    }
}

pub fn reconcile_pending_deletes(db: &mut Database, now: i64) -> Result<usize> {
    let pending = DeleteJournalRepo::new(db.conn()).list_pending()?;
    let mut finalized = 0usize;
    for batch in pending {
        let absent: Vec<(FileId, String)> = batch
            .files
            .iter()
            .filter(|f| {
                f.role == BatchFileRole::Deleted
                    && !NormalizedPath::new(&f.path).to_native_path().exists()
            })
            .map(|f| (f.file_id, f.path.clone()))
            .collect();
        if absent.is_empty() {
            db.transaction(|conn| DeleteJournalRepo::new(conn).remove(batch.id))?;
            continue;
        }
        db.transaction(|conn| {
            let files = FilesRepo::new(conn);
            let groups = DuplicateGroupsRepo::new(conn);
            for (id, _) in &absent {
                files.mark_deleted(*id, now)?;
                groups.remove_member(batch.group_id, *id)?;
            }
            let survivors = groups.list_members(batch.group_id)?;
            let mut journal_files: Vec<(FileId, String, BatchFileRole)> = absent
                .iter()
                .map(|(id, path)| (*id, path.clone(), BatchFileRole::Deleted))
                .collect();
            for id in &survivors {
                if let Some(record) = files.get(*id)? {
                    journal_files.push((
                        *id,
                        record.path.as_str().to_owned(),
                        BatchFileRole::Survivor,
                    ));
                }
            }
            let group_dropped = survivors.len() < 2;
            if group_dropped {
                groups.delete(batch.group_id)?;
            }
            DeleteJournalRepo::new(conn).finalize_committed(batch.id, group_dropped, &journal_files)
        })?;
        finalized += 1;
    }
    Ok(finalized)
}

fn delete_rejected(reject: DeleteReject) -> Reply {
    Reply::single(Response::Delete(DeleteResult {
        ok: false,
        removed_file_ids: Vec::new(),
        reclaimed_bytes: 0,
        detail: String::new(),
        reject_code: Some(reject.code_str().to_owned()),
    }))
}

fn task_path(task: &vidcull_db::repo::Task) -> Option<String> {
    task.payload
        .as_deref()
        .and_then(|bytes| ChangeTask::from_payload(bytes).ok())
        .map(|change| change.path.as_str().to_owned())
}

fn delete_failed(detail: String) -> Reply {
    Reply::single(Response::Delete(DeleteResult {
        ok: false,
        removed_file_ids: Vec::new(),
        reclaimed_bytes: 0,
        detail,
        reject_code: None,
    }))
}

fn calculate_reclaimable(all_sizes: &[i64], best_size: Option<i64>) -> u64 {
    if let Some(b_size) = best_size {
        let total: i64 = all_sizes.iter().copied().sum();
        u64::try_from(total - b_size).unwrap_or(0)
    } else {
        reclaimable_within(all_sizes)
    }
}

fn reclaimable_within(sizes: &[i64]) -> u64 {
    let Some(max) = sizes.iter().copied().max() else {
        return 0;
    };
    let total: i64 = sizes.iter().copied().sum();
    u64::try_from(total - max).unwrap_or(0)
}

fn file_record_to_detail(
    record: &FileRecord,
    is_best: bool,
    thumbnail: Option<String>,
) -> FileDetail {
    FileDetail {
        file_id: record.id.0,
        path: record.path.as_str().to_owned(),
        size_bytes: record.size_bytes,
        width: record.resolution.map(|r| r.width),
        height: record.resolution.map(|r| r.height),
        duration_ms: record
            .duration
            .map(vidcull_core::types::VideoDuration::as_millis),
        bitrate_bps: record.bitrate_bps,
        codec: record.codec.as_ref().map(|c| c.short_name().to_owned()),
        container: record.container.clone(),
        is_best,
        thumbnail,
    }
}

fn internal(err: &vidcull_core::Error) -> Reply {
    Reply::single(Response::Error(IpcError::new(
        IpcErrorKind::Internal,
        err.to_string(),
    )))
}

fn internal_as_stream(err: &vidcull_core::Error) -> Reply {
    Reply::stream(vec![Response::Error(IpcError::new(
        IpcErrorKind::Internal,
        err.to_string(),
    ))])
}

fn db_to_ipc_trust(trust: DbTrust) -> IpcTrust {
    match trust {
        DbTrust::Exact => IpcTrust::Exact,
        DbTrust::VeryLikely => IpcTrust::VeryLikely,
        DbTrust::Possible => IpcTrust::Possible,
    }
}

fn ipc_to_db_trust(trust: IpcTrust) -> DbTrust {
    match trust {
        IpcTrust::Exact => DbTrust::Exact,
        IpcTrust::VeryLikely => DbTrust::VeryLikely,
        IpcTrust::Possible => DbTrust::Possible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_round_trips_between_protocol_and_db() {
        for ipc in [IpcTrust::Exact, IpcTrust::VeryLikely, IpcTrust::Possible] {
            assert_eq!(db_to_ipc_trust(ipc_to_db_trust(ipc)), ipc);
        }
    }

    #[test]
    fn reclaimable_within_drops_the_largest_member() {
        assert_eq!(reclaimable_within(&[100, 40, 10]), 50);
    }

    #[test]
    fn reclaimable_within_is_zero_for_trivial_groups() {
        assert_eq!(reclaimable_within(&[]), 0);
        assert_eq!(reclaimable_within(&[999]), 0);
    }

    #[test]
    fn calculate_reclaimable_uses_best_when_known_else_largest() {
        assert_eq!(calculate_reclaimable(&[100, 40, 10], Some(100)), 50);
        assert_eq!(calculate_reclaimable(&[100, 40, 10], Some(40)), 110);
        assert_eq!(calculate_reclaimable(&[100, 40, 10], None), 50);
        assert_eq!(calculate_reclaimable(&[], None), 0);
    }

    #[test]
    fn handler_is_send_sync() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<DaemonRequestHandler>();
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_and_device_paths_are_refused_for_delete() {
        for path in [
            "//?/C:/Users/me/x.mp4",
            "//?/UNC/server/share/x.mp4",
            "//./PhysicalDrive0",
            "//?",
            "//.",
        ] {
            assert!(
                path_unsupported_for_delete(path),
                "verbatim/device path must stay refused: {path}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn plain_unc_is_allowed_for_delete_after_w2b() {
        for path in [
            "//server/share/x.mp4",
            "//host/vol/sub/clip.mkv",
            "//192.168.0.10/media/a.mp4",
        ] {
            assert!(
                !path_unsupported_for_delete(path),
                "plain UNC must be allowed after the W2-B relaxation: {path}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_paths_are_not_refused_for_delete() {
        for path in [
            "C:/Users/me/video.mp4",
            "D:/lib/a.mp4",
            "/foo/bar.mp4",
            "/lib/a.mp4",
        ] {
            assert!(
                !path_unsupported_for_delete(path),
                "ordinary path must not be refused: {path}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn unc_guard_is_inert_off_windows() {
        assert!(!path_unsupported_for_delete("//server/share/x.mp4"));
        assert!(!path_unsupported_for_delete("/lib/a.mp4"));
    }

    #[test]
    fn fs_mtime_ns_matches_the_indexer_derivation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("probe.mp4");
        std::fs::write(&file, b"payload").expect("write file");
        let meta = std::fs::metadata(&file).expect("metadata");
        let got = fs_mtime_ns(&meta).expect("readable mtime");

        let modified = meta.modified().expect("modified");
        let expected = i64::try_from(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .expect("post-epoch")
                .as_nanos(),
        )
        .unwrap_or(i64::MAX);
        assert_eq!(
            got, expected,
            "fs mtime must match the indexer's i64 ns basis"
        );
    }

    #[test]
    fn hide_removed_folders_cancels_pending_nonremove_under_removed_root() {
        use crate::delete::OsFileRemover;

        fn seed(repo: &TaskQueueRepo<'_>, path: &str, change: ChangeKind) {
            let payload = ChangeTask {
                path: NormalizedPath::new(path),
                change,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode payload");
            repo.enqueue(&vidcull_db::repo::NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(payload),
                enqueued_at: 0,
                size_bytes: 0,
            })
            .expect("enqueue");
        }

        let db = vidcull_db::open_in_memory().expect("open db");
        {
            let repo = TaskQueueRepo::new(db.conn());
            seed(&repo, "/removed/a.mp4", ChangeKind::Upsert);
            seed(&repo, "/removed/c.mp4", ChangeKind::Densify);
            seed(&repo, "/removed/b.mp4", ChangeKind::Remove);
            seed(&repo, "/kept/d.mp4", ChangeKind::Upsert);
        }
        let handler = DaemonRequestHandler::new(
            Arc::new(Mutex::new(db)),
            ShutdownToken::new(),
            LogBuffer::new(8),
            "scan".to_owned(),
            Arc::new(OsFileRemover),
        );

        let old = DaemonSettings {
            scan_folders: vec!["/removed".to_owned(), "/kept".to_owned()],
            ..Default::default()
        };
        let new = DaemonSettings {
            scan_folders: vec!["/kept".to_owned()],
            ..Default::default()
        };
        handler.hide_removed_folders(&old, &new);

        let survivors: Vec<(String, ChangeKind)> = handler
            .with_db(|db| {
                let repo = TaskQueueRepo::new(db.conn());
                Ok(repo
                    .list_by_state(TaskState::Pending)?
                    .into_iter()
                    .filter_map(|t| {
                        t.payload
                            .as_deref()
                            .and_then(|b| ChangeTask::from_payload(b).ok())
                            .map(|c| (c.path.as_str().to_owned(), c.change))
                    })
                    .collect())
            })
            .expect("read survivors");

        assert!(
            survivors.contains(&("/removed/b.mp4".to_owned(), ChangeKind::Remove)),
            "the under-removed-root Remove (soft-delete) must survive: {survivors:?}",
        );
        assert!(
            survivors.contains(&("/kept/d.mp4".to_owned(), ChangeKind::Upsert)),
            "the unrelated-root task must survive: {survivors:?}",
        );
        assert!(
            !survivors
                .iter()
                .any(|(p, _)| p == "/removed/a.mp4" || p == "/removed/c.mp4"),
            "non-Remove tasks under the removed root must be cancelled: {survivors:?}",
        );
        assert_eq!(
            survivors.len(),
            2,
            "exactly the two protected tasks remain (2 cancelled): {survivors:?}",
        );
    }

    #[test]
    fn hide_removed_folders_deletes_running_nonremove_under_removed_root() {
        use crate::delete::OsFileRemover;

        fn seed(repo: &TaskQueueRepo<'_>, path: &str, change: ChangeKind) -> i64 {
            let payload = ChangeTask {
                path: NormalizedPath::new(path),
                change,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode payload");
            repo.enqueue(&vidcull_db::repo::NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(payload),
                enqueued_at: 0,
                size_bytes: 0,
            })
            .expect("enqueue")
        }

        let db = vidcull_db::open_in_memory().expect("open db");
        let (running_removed_id, running_kept_id) = {
            let repo = TaskQueueRepo::new(db.conn());
            let removed_id = seed(&repo, "/removed/run.mp4", ChangeKind::Upsert);
            let claimed = repo
                .dequeue_next("scan", 1)
                .expect("dequeue")
                .expect("a pending task to claim");
            assert_eq!(claimed.id, removed_id);
            assert_eq!(claimed.state, TaskState::Running);
            let kept_id = seed(&repo, "/kept/run.mp4", ChangeKind::Upsert);
            let claimed_kept = repo
                .dequeue_next("scan", 1)
                .expect("dequeue")
                .expect("a pending task to claim");
            assert_eq!(claimed_kept.id, kept_id);
            assert_eq!(claimed_kept.state, TaskState::Running);
            (removed_id, kept_id)
        };
        let handler = DaemonRequestHandler::new(
            Arc::new(Mutex::new(db)),
            ShutdownToken::new(),
            LogBuffer::new(8),
            "scan".to_owned(),
            Arc::new(OsFileRemover),
        );

        let old = DaemonSettings {
            scan_folders: vec!["/removed".to_owned(), "/kept".to_owned()],
            ..Default::default()
        };
        let new = DaemonSettings {
            scan_folders: vec!["/kept".to_owned()],
            ..Default::default()
        };
        handler.hide_removed_folders(&old, &new);

        handler
            .with_db(|db| {
                let repo = TaskQueueRepo::new(db.conn());
                assert!(
                    !repo.exists(running_removed_id)?,
                    "the RUNNING decode under the removed root must be deleted",
                );
                assert!(
                    repo.exists(running_kept_id)?,
                    "the RUNNING decode under a kept root must survive",
                );
                Ok(())
            })
            .expect("read post-state");
    }

    #[test]
    fn force_rescan_immediately_ungroups_and_invalidates_cached_artefacts() {
        use crate::delete::OsFileRemover;
        use vidcull_core::types::Blake3Hash;
        use vidcull_db::repo::{
            Fingerprint, FingerprintsRepo, NewFile, RegroupQueueRepo, TrustLevel,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let path_a = dir.path().join("a.mp4");
        let path_b = dir.path().join("b.mp4");
        std::fs::write(&path_a, b"dummy-a").expect("write a");
        std::fs::write(&path_b, b"dummy-b").expect("write b");

        let db = vidcull_db::open_in_memory().expect("open db");
        let (id_a, id_b) = {
            let files = FilesRepo::new(db.conn());
            let id_a = files
                .insert(&NewFile {
                    path: NormalizedPath::new(&path_a),
                    size_bytes: 7,
                    content_hash: Some(Blake3Hash::from_bytes([0xAA; 32])),
                    ..Default::default()
                })
                .expect("insert a");
            let id_b = files
                .insert(&NewFile {
                    path: NormalizedPath::new(&path_b),
                    size_bytes: 7,
                    content_hash: Some(Blake3Hash::from_bytes([0xAA; 32])),
                    ..Default::default()
                })
                .expect("insert b");
            let fps = FingerprintsRepo::new(db.conn());
            for id in [id_a, id_b] {
                fps.upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: vec![0x11],
                    tier2_temporal: Some(vec![0x22]),
                    format_version: 1,
                    created_at: 0,
                })
                .expect("upsert fingerprint");
            }
            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups.create(TrustLevel::Exact, 0).expect("create group");
            groups.add_member(gid, id_a).expect("add a");
            groups.add_member(gid, id_b).expect("add b");
            (id_a, id_b)
        };

        let handler = DaemonRequestHandler::new(
            Arc::new(Mutex::new(db)),
            ShutdownToken::new(),
            LogBuffer::new(8),
            "scan".to_owned(),
            Arc::new(OsFileRemover),
        );

        let dir_str = dir.path().to_str().expect("path to str").to_owned();
        handler.handle(Request::Action(Action::ForceRescan { path: dir_str }));

        handler
            .with_db(|db| {
                let groups = DuplicateGroupsRepo::new(db.conn());
                assert!(
                    groups.list_all()?.is_empty(),
                    "force-rescanned files must drop out of their duplicate \
                     groups the moment the rescan is requested",
                );
                let files = FilesRepo::new(db.conn());
                let fps = FingerprintsRepo::new(db.conn());
                for id in [id_a, id_b] {
                    assert!(
                        files
                            .get(id)?
                            .expect("file row survives")
                            .content_hash
                            .is_none(),
                        "cached content hash must be invalidated — the EXACT \
                         rebuild is content_hash-driven and add-only, so a \
                         stale hash resurrects the group before re-indexing",
                    );
                    assert!(
                        fps.get_active_tier2(id)?.is_none(),
                        "the stale fingerprint row must be gone so the \
                         near/whole-file corpora exclude the file until it is \
                         re-verified",
                    );
                }
                let regroup = RegroupQueueRepo::new(db.conn()).load()?;
                assert!(
                    regroup.contains(&id_a) && regroup.contains(&id_b),
                    "teardown must mark the regroup delta so the durable \
                     partial index drops the files' stale postings",
                );
                Ok(())
            })
            .expect("read post-state");
    }

    #[test]
    fn force_rescan_wires_remove_task_for_deleted_file() {
        use crate::delete::OsFileRemover;
        use vidcull_core::types::Blake3Hash;
        use vidcull_db::repo::{NewFile, TrustLevel};

        let dir = tempfile::tempdir().expect("tempdir");
        let path_a = dir.path().join("a.mp4");
        let path_b = dir.path().join("b.mp4");
        std::fs::write(&path_a, b"dummy-a").expect("write a");
        std::fs::write(&path_b, b"dummy-b").expect("write b");
        let norm_a = NormalizedPath::new(&path_a);
        let norm_b = NormalizedPath::new(&path_b);

        let db = vidcull_db::open_in_memory().expect("open db");
        {
            let files = FilesRepo::new(db.conn());
            let id_a = files
                .insert(&NewFile {
                    path: norm_a.clone(),
                    size_bytes: 7,
                    content_hash: Some(Blake3Hash::from_bytes([0xAA; 32])),
                    ..Default::default()
                })
                .expect("insert a");
            let id_b = files
                .insert(&NewFile {
                    path: norm_b.clone(),
                    size_bytes: 7,
                    content_hash: Some(Blake3Hash::from_bytes([0xBB; 32])),
                    ..Default::default()
                })
                .expect("insert b");
            let groups = DuplicateGroupsRepo::new(db.conn());
            let gid = groups.create(TrustLevel::Exact, 0).expect("create group");
            groups.add_member(gid, id_a).expect("add a");
            groups.add_member(gid, id_b).expect("add b");
        }

        std::fs::remove_file(&path_b).expect("remove b");

        let handler = DaemonRequestHandler::new(
            Arc::new(Mutex::new(db)),
            ShutdownToken::new(),
            LogBuffer::new(8),
            "scan".to_owned(),
            Arc::new(OsFileRemover),
        );

        let dir_str = dir.path().to_str().expect("path to str").to_owned();
        handler.handle(Request::Action(Action::ForceRescan { path: dir_str }));

        let pending: Vec<(String, ChangeKind)> = handler
            .with_db(|db| {
                Ok(TaskQueueRepo::new(db.conn())
                    .list_by_state(TaskState::Pending)?
                    .into_iter()
                    .filter_map(|t| {
                        t.payload
                            .as_deref()
                            .and_then(|raw| ChangeTask::from_payload(raw).ok())
                            .map(|c| (c.path.as_str().to_owned(), c.change))
                    })
                    .collect())
            })
            .expect("read task queue");

        let remove_tasks: Vec<_> = pending
            .iter()
            .filter(|(_, k)| *k == ChangeKind::Remove)
            .collect();
        assert_eq!(
            remove_tasks.len(),
            1,
            "exactly one Remove task must be enqueued for b.mp4: {pending:?}",
        );
        assert_eq!(
            remove_tasks[0].0,
            norm_b.as_str(),
            "Remove task must target b.mp4",
        );
    }

    #[test]
    fn default_handler_has_no_autostart_command_wired() {
        use crate::delete::OsFileRemover;

        let db = vidcull_db::open_in_memory().expect("open db");
        let handler = DaemonRequestHandler::new(
            Arc::new(Mutex::new(db)),
            ShutdownToken::new(),
            LogBuffer::new(8),
            "scan".to_owned(),
            Arc::new(OsFileRemover),
        );
        assert_eq!(
            handler.autostart_command, None,
            "the daemon's handler must not carry an autostart command by \
             default — only the binary's `main` may opt in, and it currently \
             deliberately does not (autostart ownership lives in the app)",
        );

        let settings = DaemonSettings {
            run_on_boot: true,
            ..DaemonSettings::default()
        };
        let reply = handler.set_settings(&settings);
        match reply {
            Reply::Single(Response::Settings(echo)) => {
                assert!(echo.run_on_boot, "the preference itself still persists");
            }
            Reply::Single(Response::Error(err)) => {
                panic!("expected Response::Settings, got Error: {err:?}")
            }
            _ => panic!("expected a single Response::Settings reply"),
        }
    }
}
