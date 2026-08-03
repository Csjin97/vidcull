/**
 * @file    `protocol.rs`
 * @brief   UI와 데몬 사이의 IPC 요청 및 응답 형식
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 클러스터 목록에 멤버 상세를 포함해 N+1 IPC 조회 제거
 */
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u32 = 28;

pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Progress,
    ListGroups {
        trust: Option<TrustLevel>,
        limit: u32,
        offset: u32,
    },
    Action(Action),
    StreamLogs {
        max_records: u32,
    },
    GroupDetail {
        group_id: i64,
    },
    GroupStats {
        trust: Option<TrustLevel>,
    },
    PartialOverlaps {
        group_id: i64,
    },
    GetSettings,
    ClusterSummaries {
        trust: Option<TrustLevel>,
        limit: u32,
        offset: u32,
    },
    ClusterDetail {
        cluster_id: i64,
    },
    ClusterStats {
        trust: Option<TrustLevel>,
    },
    FailedTasks {
        limit: u32,
    },
    CrossGroupConflicts {
        group_id: i64,
    },
    Thumbnail {
        file_id: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Pong { protocol_version: u32 },
    Progress(ProgressSnapshot),
    Groups(Vec<GroupSummary>),
    Action(ActionResult),
    Log(LogRecord),
    StreamEnd,
    Error(IpcError),
    GroupDetail(Vec<FileDetail>),
    GroupStats(GroupStats),
    Delete(DeleteResult),
    PartialOverlaps(Vec<ClipOverlap>),
    Settings(DaemonSettings),
    ClusterSummaries(Vec<ClusterSummary>),
    ClusterDetail(Vec<ClusterMemberDetail>),
    ClusterStats(ClusterStats),
    FailedTasks(Vec<FailedTask>),
    CrossGroupConflicts(Vec<CrossGroupConflict>),
    Undo(UndoResult),
    Thumbnail(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum TrustLevel {
    Exact = 1,
    VeryLikely = 2,
    Possible = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    pub pending: u64,
    pub running: u64,
    pub done: u64,
    pub failed: u64,
    #[serde(default)]
    pub cpu_usage_permille: u32,
    #[serde(default)]
    pub rss_bytes: u64,
    #[serde(default)]
    pub throughput_bytes_per_sec: u64,
    #[serde(default)]
    pub pending_bytes: u64,
    #[serde(default)]
    pub current_files: Vec<String>,
    #[serde(default)]
    pub dead_workers: u32,
    #[serde(default)]
    pub panic_count: u32,
    #[serde(default)]
    pub partial_pending: u64,
    #[serde(default)]
    pub partial_running: u64,
    #[serde(default)]
    pub partial_done: u64,
    #[serde(default)]
    pub partial_skipped: BTreeMap<String, u64>,
    #[serde(default)]
    pub partial_failed: u64,
    #[serde(default)]
    pub folder_scanning: bool,
    #[serde(default)]
    pub scan_discovered: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSummary {
    pub group_id: i64,
    pub trust: TrustLevel,
    pub best_file_id: Option<i64>,
    pub member_count: u32,
    pub intro_outro: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDetail {
    pub file_id: i64,
    pub path: String,
    pub size_bytes: i64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub bitrate_bps: Option<i64>,
    pub codec: Option<String>,
    pub container: Option<String>,
    pub is_best: bool,
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GroupStats {
    pub group_count: u64,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSummary {
    pub cluster_id: i64,
    pub representative_trust: TrustLevel,
    pub best_file_id: Option<i64>,
    pub member_count: u32,
    pub member_trust_levels: Vec<TrustLevel>,
    pub intro_outro: bool,
    pub members: Vec<ClusterMemberDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterMemberDetail {
    pub file: FileDetail,
    pub trust: TrustLevel,
    pub group_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClusterStats {
    pub cluster_count: u64,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedTask {
    pub task_id: i64,
    pub path: String,
    pub reason: String,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRole {
    pub group_id: i64,
    pub trust: TrustLevel,
    pub is_best: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossGroupConflict {
    pub file_id: i64,
    pub path: String,
    pub memberships: Vec<GroupRole>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Rescan { path: String },
    Shutdown,
    MoveToTrash(DeleteRequest),
    DeletePermanent(DeleteRequest),
    SetSettings(DaemonSettings),
    UndoLastDelete,
    ForceRescan { path: String },
    SetLogLevel(LogLevel),
    ExportDiagnostics { dest: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteRequest {
    pub group_id: i64,
    pub file_ids: Vec<i64>,
    pub confirm_best: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteResult {
    pub ok: bool,
    pub removed_file_ids: Vec<i64>,
    pub reclaimed_bytes: u64,
    pub detail: String,
    #[serde(default)]
    pub reject_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoResult {
    pub ok: bool,
    pub group_id: Option<i64>,
    pub restored_file_ids: Vec<i64>,
    pub missing_paths: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipOverlap {
    pub clip_file_id: i64,
    pub source_file_id: i64,
    pub matched_scenes: u32,
    pub clip_scenes: u32,
    pub start_ms: u64,
    pub end_ms: u64,
    pub clip_start_ms: u64,
    pub clip_end_ms: u64,
    pub intro_outro: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub accepted: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp_ms: i64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum IpcErrorKind {
    BadRequest = 1,
    NotFound = 2,
    Internal = 3,
    Unsupported = 4,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub kind: IpcErrorKind,
    pub message: String,
}

impl IpcError {
    pub fn new(kind: IpcErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CpuThrottle {
    #[default]
    Full,
    Balanced,
    Eco,
}

pub use vidcull_core::types::BestCopyMode;

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonSettings {
    pub scan_folders: Vec<String>,
    pub background_enabled: bool,
    pub auto_index: bool,
    pub exclude_rules: Vec<String>,
    pub run_on_boot: bool,
    pub cpu_throttle: CpuThrottle,
    pub best_copy_mode: BestCopyMode,
    pub idle_worker_count: Option<u32>,
    pub cpu_cores: u32,
    pub partial_clips_enabled: bool,
    pub indexing_enabled: bool,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            scan_folders: Vec::new(),
            background_enabled: true,
            auto_index: true,
            exclude_rules: vec![
                "$RECYCLE.BIN".to_owned(),
                "System Volume Information".to_owned(),
            ],
            run_on_boot: false,
            cpu_throttle: CpuThrottle::Full,
            best_copy_mode: BestCopyMode::Archival,
            idle_worker_count: None,
            cpu_cores: 1,
            partial_clips_enabled: true,
            indexing_enabled: true,
        }
    }
}
