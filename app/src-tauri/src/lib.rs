use std::sync::atomic::{AtomicBool, Ordering};

use tauri::Manager;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};
use tokio::sync::Mutex;
use vidcull_ipc::{
    Action, ClipOverlap, ClusterMemberDetail, ClusterStats, ClusterSummary, CrossGroupConflict,
    DaemonSettings, DeleteRequest, DeleteResult, FailedTask, FileDetail, GroupStats, GroupSummary,
    IpcClient, ProgressSnapshot, Request, Response, TrustLevel, UndoResult,
};

mod autostart;
mod logging;
mod tray;

struct BackgroundEnabled(AtomicBool);

pub(crate) struct SpawnedDaemonPid(std::sync::Mutex<Option<u32>>);

impl SpawnedDaemonPid {
    fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    fn set(&self, pid: u32) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pid);
    }

    #[must_use]
    pub(crate) fn get(&self) -> Option<u32> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn daemon_endpoint() -> String {
    std::env::var("VIDCULL_IPC").unwrap_or_else(|_| vidcull_ipc::default_endpoint())
}

pub(crate) struct DaemonConn {
    client: Mutex<Option<(IpcClient, u32)>>,
    progress_cache: std::sync::Mutex<Option<(ProgressSnapshot, std::time::Instant)>>,
    thumb_client: Mutex<Option<(IpcClient, u32)>>,
}

const PROTOCOL_MISMATCH_PREFIX: &str = "PROTOCOL_MISMATCH:";

fn version_gate(daemon_version: u32, request: &Request) -> Result<(), String> {
    if daemon_version == vidcull_ipc::protocol::PROTOCOL_VERSION || matches!(request, Request::Ping)
    {
        return Ok(());
    }
    Err(format!(
        "{PROTOCOL_MISMATCH_PREFIX} 데몬 프로토콜 v{daemon_version} ≠ 앱 v{} — \
         버전이 일치할 때까지 데이터 요청을 차단합니다. 앱과 데몬을 같은 버전으로 업데이트하세요.",
        vidcull_ipc::protocol::PROTOCOL_VERSION
    ))
}

async fn connect_handshaken(phase: &str) -> Result<(IpcClient, u32), String> {
    IpcClient::connect_negotiated(&daemon_endpoint())
        .await
        .map_err(|err| format!("{phase}: {err}"))
}

async fn probe_daemon(timeout: std::time::Duration) -> Result<(IpcClient, u32), String> {
    IpcClient::connect_negotiated_probe(&daemon_endpoint(), timeout)
        .await
        .map_err(|err| format!("probe: {err}"))
}

const SLOW_IPC_CALL_MS: u64 = 100;

fn duration_ms(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
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

fn record_ipc_call(kind: &'static str, wait_ms: u64, hold_ms: u64) {
    if wait_ms > SLOW_IPC_CALL_MS || hold_ms > SLOW_IPC_CALL_MS {
        tracing::warn!(kind, wait_ms, hold_ms, "daemon IPC mutex wait/hold");
    } else {
        tracing::debug!(kind, wait_ms, hold_ms, "daemon IPC mutex wait/hold");
    }
}

static FAST_LANE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn record_fast_lane_hit(cache_age_ms: u64) {
    let total = FAST_LANE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::debug!(
        kind = "progress",
        cache_age_ms,
        total_hits = total,
        "progress-poll fast-lane cache hit — wire not touched",
    );
}

const FAST_LANE_MAX_CACHE_AGE_MS: u64 = 5_000;

static FAST_LANE_STALE_SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn record_fast_lane_stale_skip(cache_age_ms: u64) {
    let total = FAST_LANE_STALE_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::warn!(
        kind = "progress",
        cache_age_ms,
        max_age_ms = FAST_LANE_MAX_CACHE_AGE_MS,
        total_skips = total,
        "progress-poll fast-lane cache too stale to serve — falling back to \
         blocking path",
    );
}

async fn request_with_guard(
    slot: &mut Option<(IpcClient, u32)>,
    request: &Request,
) -> Result<Response, String> {
    for phase in ["connect", "reconnect"] {
        if slot.is_none() {
            match connect_handshaken(phase).await {
                Ok(conn) => *slot = Some(conn),
                Err(err) => return Err(err),
            }
        }
        let (client, daemon_version) = slot.as_mut().expect("just ensured Some");

        version_gate(*daemon_version, request)?;

        match client.request(request).await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                *slot = None;
                if phase == "reconnect" {
                    return Err(err.to_string());
                }
            }
        }
    }
    unreachable!("the reconnect attempt either returned or errored")
}

impl DaemonConn {
    fn new() -> Self {
        Self {
            client: Mutex::new(None),
            progress_cache: std::sync::Mutex::new(None),
            thumb_client: Mutex::new(None),
        }
    }

    fn update_progress_cache(&self, outcome: &Result<Response, String>) {
        if let Ok(Response::Progress(snapshot)) = outcome {
            *self
                .progress_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((snapshot.clone(), std::time::Instant::now()));
        }
    }

    pub(crate) async fn adopt(&self, client: IpcClient, daemon_version: u32) {
        *self.client.lock().await = Some((client, daemon_version));
    }

    pub(crate) async fn request(&self, request: &Request) -> Result<Response, String> {
        let kind = request_kind(request);
        let is_progress = matches!(request, Request::Progress);

        if is_progress {
            match self.client.try_lock() {
                Ok(mut guard) => {
                    let hold_started = std::time::Instant::now();
                    let outcome = request_with_guard(&mut guard, request).await;
                    drop(guard);
                    self.update_progress_cache(&outcome);
                    record_ipc_call(kind, 0, duration_ms(hold_started.elapsed()));
                    return outcome;
                }
                Err(_) => {
                    let cached = self
                        .progress_cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let Some((snapshot, captured_at)) = cached {
                        let age_ms = duration_ms(captured_at.elapsed());
                        if age_ms <= FAST_LANE_MAX_CACHE_AGE_MS {
                            record_fast_lane_hit(age_ms);
                            return Ok(Response::Progress(snapshot));
                        }
                        record_fast_lane_stale_skip(age_ms);
                    }
                }
            }
        }

        let wait_started = std::time::Instant::now();
        let mut guard: tokio::sync::MutexGuard<'_, Option<(IpcClient, u32)>> =
            self.client.lock().await;
        let wait_ms = duration_ms(wait_started.elapsed());
        let hold_started = std::time::Instant::now();

        let outcome = request_with_guard(&mut guard, request).await;
        drop(guard);
        if is_progress {
            self.update_progress_cache(&outcome);
        }
        record_ipc_call(kind, wait_ms, duration_ms(hold_started.elapsed()));
        outcome
    }

    pub(crate) async fn request_stream(&self, request: &Request) -> Result<Vec<Response>, String> {
        let kind = request_kind(request);
        let wait_started = std::time::Instant::now();
        let mut guard: tokio::sync::MutexGuard<'_, Option<(IpcClient, u32)>> =
            self.client.lock().await;
        let wait_ms = duration_ms(wait_started.elapsed());
        let hold_started = std::time::Instant::now();

        let outcome: Result<Vec<Response>, String> = 'attempt: {
            for phase in ["connect", "reconnect"] {
                if guard.is_none() {
                    match connect_handshaken(phase).await {
                        Ok(conn) => *guard = Some(conn),
                        Err(err) => break 'attempt Err(err),
                    }
                }
                let (client, daemon_version) = guard.as_mut().expect("just ensured Some");
                if let Err(err) = version_gate(*daemon_version, request) {
                    break 'attempt Err(err);
                }
                match client.request_stream(request).await {
                    Ok(frames) => break 'attempt Ok(frames),
                    Err(err) => {
                        *guard = None;
                        if phase == "reconnect" {
                            break 'attempt Err(err.to_string());
                        }
                    }
                }
            }
            unreachable!("the reconnect attempt either returned or errored");
        };

        drop(guard);
        record_ipc_call(kind, wait_ms, duration_ms(hold_started.elapsed()));
        outcome
    }

    pub(crate) async fn request_thumbnail(&self, request: &Request) -> Result<Response, String> {
        let kind = request_kind(request);
        let wait_started = std::time::Instant::now();
        let mut guard = self.thumb_client.lock().await;
        let wait_ms = duration_ms(wait_started.elapsed());
        let hold_started = std::time::Instant::now();
        let outcome = request_with_guard(&mut guard, request).await;
        drop(guard);
        record_ipc_call(kind, wait_ms, duration_ms(hold_started.elapsed()));
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vidcull_ipc::protocol::PROTOCOL_VERSION;

    #[test]
    fn version_gate_passes_everything_on_a_matching_daemon() {
        assert!(version_gate(PROTOCOL_VERSION, &Request::Ping).is_ok());
        assert!(version_gate(PROTOCOL_VERSION, &Request::Progress).is_ok());
        assert!(version_gate(PROTOCOL_VERSION, &Request::GroupDetail { group_id: 1 }).is_ok());
    }

    #[test]
    fn version_gate_blocks_data_rpcs_on_a_mismatched_daemon() {
        for request in [
            Request::Progress,
            Request::GroupDetail { group_id: 1 },
            Request::GetSettings,
            Request::Action(vidcull_ipc::protocol::Action::SetLogLevel(
                vidcull_ipc::protocol::LogLevel::Debug,
            )),
            Request::Action(vidcull_ipc::protocol::Action::ExportDiagnostics {
                dest: "/tmp/bundle".to_owned(),
            }),
        ] {
            let err = version_gate(PROTOCOL_VERSION + 1, &request)
                .expect_err("mismatch must refuse data RPCs");
            assert!(
                err.starts_with(PROTOCOL_MISMATCH_PREFIX),
                "gate errors carry the sentinel prefix, got: {err}"
            );
            assert!(
                err.contains(&format!("v{}", PROTOCOL_VERSION + 1)),
                "the daemon's version is named in: {err}"
            );
        }
    }

    #[test]
    fn version_gate_exempts_ping_so_the_status_pill_keeps_polling() {
        assert!(version_gate(PROTOCOL_VERSION + 1, &Request::Ping).is_ok());
        assert!(version_gate(PROTOCOL_VERSION - 1, &Request::Ping).is_ok());
    }

    #[tokio::test]
    async fn progress_fast_lane_serves_cached_snapshot_when_mutex_is_held() {
        let conn = DaemonConn::new();
        let cached_snapshot = ProgressSnapshot {
            pending: 3,
            running: 1,
            done: 10,
            ..Default::default()
        };
        *conn
            .progress_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((cached_snapshot.clone(), std::time::Instant::now()));

        let _guard = conn.client.lock().await;

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            conn.request(&Request::Progress),
        )
        .await
        .expect("fast-lane must return promptly even while the mutex is held");

        match result {
            Ok(Response::Progress(snap)) => assert_eq!(snap, cached_snapshot),
            other => panic!("expected a cached Progress response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn progress_falls_back_to_blocking_path_when_cache_is_empty() {
        const HOLD_MS: u64 = 80;
        let conn = std::sync::Arc::new(DaemonConn::new());

        let holder_conn = std::sync::Arc::clone(&conn);
        let holder = tokio::spawn(async move {
            let _guard = holder_conn.client.lock().await;
            tokio::time::sleep(std::time::Duration::from_millis(HOLD_MS)).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let started = std::time::Instant::now();
        let _ = conn.request(&Request::Progress).await;
        let elapsed = started.elapsed();

        holder.await.expect("holder task must not panic");
        assert!(
            elapsed >= std::time::Duration::from_millis(HOLD_MS - 5),
            "with no cache, a progress request must wait for the mutex like \
             any other request (elapsed={elapsed:?}, expected >= ~{HOLD_MS}ms)",
        );
    }

    #[tokio::test]
    async fn progress_falls_back_to_blocking_path_when_cache_is_stale() {
        const HOLD_MS: u64 = 80;
        let conn = std::sync::Arc::new(DaemonConn::new());
        let stale_snapshot = ProgressSnapshot {
            pending: 7,
            running: 2,
            done: 5,
            ..Default::default()
        };
        let stale_captured_at = std::time::Instant::now()
            - std::time::Duration::from_millis(FAST_LANE_MAX_CACHE_AGE_MS + 500);
        *conn
            .progress_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((stale_snapshot, stale_captured_at));

        let holder_conn = std::sync::Arc::clone(&conn);
        let holder = tokio::spawn(async move {
            let _guard = holder_conn.client.lock().await;
            tokio::time::sleep(std::time::Duration::from_millis(HOLD_MS)).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let started = std::time::Instant::now();
        let _ = conn.request(&Request::Progress).await;
        let elapsed = started.elapsed();

        holder.await.expect("holder task must not panic");
        assert!(
            elapsed >= std::time::Duration::from_millis(HOLD_MS - 5),
            "a stale cache must not be fast-laned — request() must wait for \
             the mutex like a cache miss (elapsed={elapsed:?}, expected >= \
             ~{HOLD_MS}ms)",
        );
    }

    #[tokio::test]
    async fn thumbnail_connection_does_not_block_primary() {
        let conn = DaemonConn::new();
        let thumb_held = conn.thumb_client.lock().await;
        assert!(
            conn.client.try_lock().is_ok(),
            "primary client mutex must stay free while a thumbnail holds its own connection"
        );
        drop(thumb_held);
        let primary_held = conn.client.lock().await;
        assert!(
            conn.thumb_client.try_lock().is_ok(),
            "thumbnail connection must stay free while the primary is busy"
        );
        drop(primary_held);
    }

    #[test]
    fn spawned_daemon_pid_starts_empty() {
        let pid = SpawnedDaemonPid::new();
        assert_eq!(pid.get(), None);
    }

    #[test]
    fn spawned_daemon_pid_records_and_returns_the_set_pid() {
        let pid = SpawnedDaemonPid::new();
        pid.set(4242);
        assert_eq!(pid.get(), Some(4242));
    }

    #[test]
    fn spawned_daemon_pid_overwrites_on_a_second_set() {
        let pid = SpawnedDaemonPid::new();
        pid.set(1111);
        pid.set(2222);
        assert_eq!(pid.get(), Some(2222));
    }

    #[test]
    fn spawn_backoff_policy_default_is_small_and_bounded() {
        let policy = SpawnBackoffPolicy::DEFAULT;
        assert!(
            policy.max_ping_attempts > 0 && policy.max_ping_attempts <= 5,
            "ping-retry cap must be small and non-zero, got {}",
            policy.max_ping_attempts
        );
        assert!(
            policy.max_spawn_attempts > 0 && policy.max_spawn_attempts <= 5,
            "spawn-retry cap must be small and non-zero, got {}",
            policy.max_spawn_attempts
        );
    }

    #[test]
    fn pre_spawn_probe_timeout_is_a_small_fraction_of_the_post_spawn_deadline() {
        let policy = SpawnBackoffPolicy::DEFAULT;
        assert!(
            policy.pre_spawn_probe_timeout_ms > 0,
            "the probe must attempt at least one real dial, not skip straight to spawning"
        );
        assert!(
            policy.pre_spawn_probe_timeout_ms <= 1000,
            "pre_spawn_probe_timeout_ms should be on the order of hundreds of \
             ms, not seconds — got {}ms; a value this large defeats the \
             c fast-fail fix (3 ping attempts at this cost each is \
             most of the perceived spawn delay)",
            policy.pre_spawn_probe_timeout_ms
        );
    }

    #[tokio::test]
    async fn no_daemon_running_218c_regression_scenarios() {
        let pid = std::process::id();

        unsafe {
            std::env::set_var(
                "VIDCULL_IPC",
                format!(r"\\.\pipe\vidcull-218c-probe-test-{pid}"),
            );
        }
        let probe_conn = DaemonConn::new();
        let policy = SpawnBackoffPolicy::DEFAULT;

        let t0 = std::time::Instant::now();
        let probe_outcome = probe_for_existing_daemon(&probe_conn, policy).await;
        let probe_elapsed = t0.elapsed();

        unsafe {
            std::env::remove_var("VIDCULL_IPC");
        }

        assert_eq!(
            probe_outcome,
            PreSpawnProbeOutcome::NotFound,
            "no daemon exists for this endpoint, so the probe must report NotFound"
        );
        assert!(
            probe_elapsed < std::time::Duration::from_secs(2),
            "probe_for_existing_daemon took {probe_elapsed:?} to report NotFound \
             — expected well under 2s (policy budget is ~{}ms); a multi-second \
             elapsed here means the pre-spawn probe loop is retrying \
             ERROR_FILE_NOT_FOUND for the full post-spawn deadline again \
             (the exact c live regression: a ~2-minute UI-visible \
             spawn delay)",
            policy.max_ping_attempts as u64 * policy.pre_spawn_probe_timeout_ms
        );

        let harmless_non_daemon_exe = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join(r"System32\hostname.exe");
        assert!(
            harmless_non_daemon_exe.is_file(),
            "test precondition: {harmless_non_daemon_exe:?} must exist on the \
             CI/dev Windows box this test runs on"
        );
        unsafe {
            std::env::set_var(
                "VIDCULL_IPC",
                format!(r"\\.\pipe\vidcull-218c-acquire-test-{pid}"),
            );
            std::env::set_var("VIDCULL_DAEMON", &harmless_non_daemon_exe);
        }

        let acquire_conn = DaemonConn::new();
        let spawned_pid = SpawnedDaemonPid::new();

        let t0 = std::time::Instant::now();
        let acquire_outcome = acquire_daemon(&acquire_conn, &spawned_pid).await;
        let acquire_elapsed = t0.elapsed();

        unsafe {
            std::env::remove_var("VIDCULL_IPC");
            std::env::remove_var("VIDCULL_DAEMON");
        }

        assert_eq!(
            acquire_outcome,
            DaemonAcquireOutcome::GaveUp,
            "the spawn target speaks no IPC protocol at all, so acquire_daemon \
             must give up rather than falsely report a daemon"
        );
        assert!(
            acquire_elapsed < std::time::Duration::from_secs(25),
            "acquire_daemon took {acquire_elapsed:?} to give up when no real \
             daemon was reachable — expected well under 25s (live regression \
             was 131s); this means either the pre-spawn probe loop or the \
             spawn loop's retry shape regressed"
        );
    }
}

#[tauri::command]
async fn ping_daemon(conn: tauri::State<'_, DaemonConn>) -> Result<u32, String> {
    match conn.request(&Request::Ping).await? {
        Response::Pong { protocol_version } => Ok(protocol_version),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn force_rescan_directory(
    conn: tauri::State<'_, DaemonConn>,
    path: String,
) -> Result<String, String> {
    match conn
        .request(&Request::Action(Action::ForceRescan { path }))
        .await?
    {
        Response::Action(result) => {
            if result.accepted {
                Ok(result.detail)
            } else {
                Err(result.detail)
            }
        }
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn set_log_level(
    conn: tauri::State<'_, DaemonConn>,
    level: String,
) -> Result<String, String> {
    let parsed = match level.to_ascii_lowercase().as_str() {
        "error" => vidcull_ipc::protocol::LogLevel::Error,
        "warn" => vidcull_ipc::protocol::LogLevel::Warn,
        "info" => vidcull_ipc::protocol::LogLevel::Info,
        "debug" => vidcull_ipc::protocol::LogLevel::Debug,
        "trace" => vidcull_ipc::protocol::LogLevel::Trace,
        other => return Err(format!("알 수 없는 로그 레벨: {other}")),
    };
    tracing::info!(level = %level, "requesting daemon runtime log level change");
    match conn
        .request(&Request::Action(Action::SetLogLevel(parsed)))
        .await?
    {
        Response::Action(result) if result.accepted => Ok(result.detail),
        Response::Action(result) => Err(result.detail),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn export_diagnostics(
    conn: tauri::State<'_, DaemonConn>,
    dest: String,
) -> Result<String, String> {
    tracing::info!("requesting diagnostic bundle export");
    match conn
        .request(&Request::Action(Action::ExportDiagnostics { dest }))
        .await?
    {
        Response::Action(result) if result.accepted => Ok(result.detail),
        Response::Action(result) => Err(result.detail),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn rescan_directory(
    conn: tauri::State<'_, DaemonConn>,
    path: String,
) -> Result<String, String> {
    match conn
        .request(&Request::Action(Action::Rescan { path }))
        .await?
    {
        Response::Action(result) => {
            if result.accepted {
                Ok(result.detail)
            } else {
                Err(result.detail)
            }
        }
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn list_groups(
    conn: tauri::State<'_, DaemonConn>,
    trust: Option<TrustLevel>,
    limit: u32,
    offset: u32,
) -> Result<Vec<GroupSummary>, String> {
    match conn
        .request(&Request::ListGroups {
            trust,
            limit,
            offset,
        })
        .await?
    {
        Response::Groups(groups) => Ok(groups),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn list_group_detail(
    conn: tauri::State<'_, DaemonConn>,
    group_id: i64,
) -> Result<Vec<FileDetail>, String> {
    let frames = conn
        .request_stream(&Request::GroupDetail { group_id })
        .await?;
    let mut members = Vec::new();
    for frame in frames {
        match frame {
            Response::GroupDetail(chunk) => members.extend(chunk),
            Response::Error(err) => return Err(err.message),
            other => return Err(format!("unexpected response from daemon: {other:?}")),
        }
    }
    Ok(members)
}

#[tauri::command]
async fn group_stats(
    conn: tauri::State<'_, DaemonConn>,
    trust: Option<TrustLevel>,
) -> Result<GroupStats, String> {
    match conn.request(&Request::GroupStats { trust }).await? {
        Response::GroupStats(stats) => Ok(stats),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn daemon_progress(conn: tauri::State<'_, DaemonConn>) -> Result<ProgressSnapshot, String> {
    match conn.request(&Request::Progress).await? {
        Response::Progress(snapshot) => Ok(snapshot),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn delete_files(
    conn: tauri::State<'_, DaemonConn>,
    group_id: i64,
    file_ids: Vec<i64>,
    mode: String,
    confirm_best: bool,
) -> Result<DeleteResult, String> {
    let request = DeleteRequest {
        group_id,
        file_ids,
        confirm_best,
    };
    let action = match mode.as_str() {
        "trash" => Action::MoveToTrash(request),
        "permanent" => Action::DeletePermanent(request),
        other => return Err(format!("unknown delete mode: {other}")),
    };
    match conn.request(&Request::Action(action)).await? {
        Response::Delete(result) => Ok(result),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn undo_last_delete(conn: tauri::State<'_, DaemonConn>) -> Result<UndoResult, String> {
    match conn
        .request(&Request::Action(Action::UndoLastDelete))
        .await?
    {
        Response::Undo(result) => Ok(result),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn partial_overlaps(
    conn: tauri::State<'_, DaemonConn>,
    group_id: i64,
) -> Result<Vec<ClipOverlap>, String> {
    match conn.request(&Request::PartialOverlaps { group_id }).await? {
        Response::PartialOverlaps(overlaps) => Ok(overlaps),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn list_clusters(
    conn: tauri::State<'_, DaemonConn>,
    trust: Option<TrustLevel>,
    limit: u32,
    offset: u32,
) -> Result<Vec<ClusterSummary>, String> {
    match conn
        .request(&Request::ClusterSummaries {
            trust,
            limit,
            offset,
        })
        .await?
    {
        Response::ClusterSummaries(clusters) => Ok(clusters),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn cluster_detail(
    conn: tauri::State<'_, DaemonConn>,
    cluster_id: i64,
) -> Result<Vec<ClusterMemberDetail>, String> {
    let frames = conn
        .request_stream(&Request::ClusterDetail { cluster_id })
        .await?;
    let mut members = Vec::new();
    for frame in frames {
        match frame {
            Response::ClusterDetail(chunk) => members.extend(chunk),
            Response::Error(err) => return Err(err.message),
            other => return Err(format!("unexpected response from daemon: {other:?}")),
        }
    }
    Ok(members)
}

#[tauri::command]
async fn thumbnail(
    conn: tauri::State<'_, DaemonConn>,
    file_id: i64,
) -> Result<Option<String>, String> {
    match conn
        .request_thumbnail(&Request::Thumbnail { file_id })
        .await?
    {
        Response::Thumbnail(uri) => Ok(uri),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn cluster_stats(
    conn: tauri::State<'_, DaemonConn>,
    trust: Option<TrustLevel>,
) -> Result<ClusterStats, String> {
    match conn.request(&Request::ClusterStats { trust }).await? {
        Response::ClusterStats(stats) => Ok(stats),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn failed_tasks(
    conn: tauri::State<'_, DaemonConn>,
    limit: u32,
) -> Result<Vec<FailedTask>, String> {
    match conn.request(&Request::FailedTasks { limit }).await? {
        Response::FailedTasks(tasks) => Ok(tasks),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn cross_group_conflicts(
    conn: tauri::State<'_, DaemonConn>,
    group_id: i64,
) -> Result<Vec<CrossGroupConflict>, String> {
    match conn
        .request(&Request::CrossGroupConflicts { group_id })
        .await?
    {
        Response::CrossGroupConflicts(conflicts) => Ok(conflicts),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
fn frontend_trace_enabled() -> bool {
    std::env::var("VIDCULL_FRONTEND_TRACE").is_ok_and(|v| v == "1")
}

#[cfg(target_os = "windows")]
const REVEAL_CREATION_FLAGS: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
fn explorer_reveal_command(path: &str) -> (std::process::Command, u32) {
    use std::os::windows::process::CommandExt;
    let native = path.replace('/', "\\");
    let mut c = std::process::Command::new("explorer");
    // explorer.exe does not use standard argv parsing: it expects `/select,` to be
    // immediately followed by a quoted path with no space in between. If we pass
    // this as a single `.arg()`, Rust's automatic quoting wraps the *entire*
    // "/select,<path>" string in quotes whenever the path contains a space,
    // which explorer fails to parse — it then silently falls back to opening
    // the user's Documents folder instead of the target directory.
    c.raw_arg(format!("/select,\"{native}\""));
    (c, REVEAL_CREATION_FLAGS)
}

#[tauri::command]
async fn reveal_in_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let (mut c, flags) = explorer_reveal_command(&path);
        c.creation_flags(flags);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.args(["-R", &path]);
        c
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let parent = std::path::Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(path);
        let mut c = std::process::Command::new("xdg-open");
        c.arg(parent);
        c
    };

    command
        .spawn()
        .map_err(|err| format!("reveal_in_folder: {err}"))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn explorer_open_command(path: &str) -> (std::process::Command, u32) {
    let native = path.replace('/', "\\");
    let mut c = std::process::Command::new("explorer");
    c.arg(native);
    (c, REVEAL_CREATION_FLAGS)
}

#[tauri::command]
async fn open_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let (mut c, flags) = explorer_open_command(&path);
        c.creation_flags(flags);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(&path);
        c
    };
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(&path);
        c
    };

    command
        .spawn()
        .map_err(|err| format!("open_folder: {err}"))?;
    Ok(())
}

#[cfg(all(test, windows))]
mod reveal_in_folder_tests {
    use super::*;

    #[test]
    fn explorer_reveal_command_sets_create_no_window_flag() {
        let (_cmd, flags) = explorer_reveal_command("C:/Users/test/video.mp4");
        assert_eq!(
            flags, 0x0800_0000,
            "reveal_in_folder must spawn explorer with CREATE_NO_WINDOW"
        );
    }

    #[test]
    fn explorer_reveal_command_has_select_arg_with_native_path() {
        use std::ffi::OsStr;

        let (cmd, _flags) = explorer_reveal_command("C:/Users/test/video.mp4");
        let args: Vec<&OsStr> = cmd.get_args().collect();

        assert_eq!(
            args,
            vec![OsStr::new("/select,\"C:\\Users\\test\\video.mp4\"")],
            "explorer must be invoked with a single /select,\"<native path>\" raw arg"
        );
    }

    #[test]
    fn explorer_reveal_command_quotes_path_with_spaces() {
        use std::ffi::OsStr;

        let (cmd, _flags) = explorer_reveal_command("C:/Users/test/my video.mp4");
        let args: Vec<&OsStr> = cmd.get_args().collect();

        assert_eq!(
            args,
            vec![OsStr::new("/select,\"C:\\Users\\test\\my video.mp4\"")],
            "the path must be individually quoted so explorer doesn't fall back to \
             opening Documents when the path contains a space"
        );
    }

    #[test]
    fn explorer_open_command_opens_folder_without_select() {
        use std::ffi::OsStr;

        let (cmd, flags) = explorer_open_command("C:/Users/test/bundle");
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![OsStr::new("C:\\Users\\test\\bundle")],
            "open_folder must invoke explorer with the bare folder path and no /select"
        );
        assert_eq!(
            flags, 0x0800_0000,
            "open_folder must spawn explorer with CREATE_NO_WINDOW"
        );
    }
}

#[tauri::command]
async fn pick_folder() -> Result<Option<String>, String> {
    let picked = tauri::async_runtime::spawn_blocking(|| rfd::FileDialog::new().pick_folder())
        .await
        .map_err(|err| format!("folder dialog task failed: {err}"))?;
    Ok(picked.map(|path| path.to_string_lossy().replace('\\', "/")))
}

#[tauri::command]
async fn get_settings(conn: tauri::State<'_, DaemonConn>) -> Result<DaemonSettings, String> {
    match conn.request(&Request::GetSettings).await? {
        Response::Settings(settings) => Ok(settings),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

#[tauri::command]
async fn set_settings(
    app: tauri::AppHandle,
    conn: tauri::State<'_, DaemonConn>,
    background: tauri::State<'_, BackgroundEnabled>,
    settings: DaemonSettings,
) -> Result<DaemonSettings, String> {
    match conn
        .request(&Request::Action(Action::SetSettings(settings)))
        .await?
    {
        Response::Settings(stored) => {
            background
                .0
                .store(stored.background_enabled, Ordering::Relaxed);
            tray::sync_indexing_label(&app, stored.indexing_enabled);
            autostart::sync(stored.run_on_boot);
            Ok(stored)
        }
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

async fn load_background_enabled(conn: &DaemonConn) -> Result<bool, String> {
    match conn.request(&Request::GetSettings).await? {
        Response::Settings(settings) => Ok(settings.background_enabled),
        Response::Error(err) => Err(err.message),
        other => Err(format!("unexpected response from daemon: {other:?}")),
    }
}

fn resolve_daemon_exe() -> Option<std::path::PathBuf> {
    let exe_name = if cfg!(windows) {
        "vidcull-daemon.exe"
    } else {
        "vidcull-daemon"
    };

    if let Some(path) = std::env::var_os("VIDCULL_DAEMON") {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();

    let adjacent = exe_dir.join(exe_name);
    if adjacent.is_file() {
        return Some(adjacent);
    }

    let mut cursor: &std::path::Path = exe_dir.as_path();
    for _ in 0..8 {
        for profile in ["release", "debug"] {
            let candidate = cursor.join("target").join(profile).join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    None
}

fn spawn_daemon_detached(daemon_exe: &std::path::Path) -> std::io::Result<u32> {
    let mut command = std::process::Command::new(daemon_exe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map(|child| child.id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpawnBackoffPolicy {
    max_ping_attempts: u32,
    ping_retry_delay_ms: u64,
    max_spawn_attempts: u32,
    pre_spawn_probe_timeout_ms: u64,
}

impl SpawnBackoffPolicy {
    const DEFAULT: Self = Self {
        max_ping_attempts: 3,
        ping_retry_delay_ms: 20,
        max_spawn_attempts: 3,
        pre_spawn_probe_timeout_ms: 250,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonAcquireOutcome {
    AlreadyRunning,
    Spawned,
    GaveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreSpawnProbeOutcome {
    Found,
    NotFound,
}

async fn probe_for_existing_daemon(
    conn: &DaemonConn,
    policy: SpawnBackoffPolicy,
) -> PreSpawnProbeOutcome {
    let probe_timeout = std::time::Duration::from_millis(policy.pre_spawn_probe_timeout_ms);
    for attempt in 0..policy.max_ping_attempts {
        if let Ok((client, daemon_version)) = probe_daemon(probe_timeout).await {
            conn.adopt(client, daemon_version).await;
            return PreSpawnProbeOutcome::Found;
        }
        if attempt + 1 < policy.max_ping_attempts {
            tokio::time::sleep(std::time::Duration::from_millis(policy.ping_retry_delay_ms)).await;
        }
    }
    PreSpawnProbeOutcome::NotFound
}

async fn acquire_daemon(conn: &DaemonConn, spawned_pid: &SpawnedDaemonPid) -> DaemonAcquireOutcome {
    let policy = SpawnBackoffPolicy::DEFAULT;

    if probe_for_existing_daemon(conn, policy).await == PreSpawnProbeOutcome::Found {
        return DaemonAcquireOutcome::AlreadyRunning;
    }

    for spawn_attempt in 1..=policy.max_spawn_attempts {
        match resolve_daemon_exe() {
            Some(exe) => match spawn_daemon_detached(&exe) {
                Ok(pid) => {
                    spawned_pid.set(pid);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        policy.ping_retry_delay_ms,
                    ))
                    .await;
                    if conn.request(&Request::Ping).await.is_ok() {
                        return DaemonAcquireOutcome::Spawned;
                    }
                    tracing::warn!(
                        pid,
                        spawn_attempt,
                        max_spawn_attempts = policy.max_spawn_attempts,
                        "spawned daemon did not answer Ping in time",
                    );
                }
                Err(err) => {
                    tracing::error!(
                        daemon_exe = exe.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                        error = %err,
                        spawn_attempt,
                        max_spawn_attempts = policy.max_spawn_attempts,
                        "failed to spawn daemon sidecar",
                    );
                }
            },
            None => {
                tracing::error!("daemon executable not found; launch vidcull-daemon manually");
                break;
            }
        }
    }

    tracing::error!(
        max_spawn_attempts = policy.max_spawn_attempts,
        "gave up spawning a reachable daemon after exhausting the retry cap",
    );
    DaemonAcquireOutcome::GaveUp
}

const DAEMON_HEALTH_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(8);

const DAEMON_RESPAWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

async fn run_daemon_health_monitor(handle: tauri::AppHandle) {
    loop {
        tokio::time::sleep(DAEMON_HEALTH_CHECK_INTERVAL).await;
        let conn = handle.state::<DaemonConn>();
        if conn.request(&Request::Ping).await.is_ok() {
            continue;
        }
        tracing::warn!("daemon unreachable at health check — attempting runtime respawn");
        let spawned_pid = handle.state::<SpawnedDaemonPid>();
        match acquire_daemon(&conn, &spawned_pid).await {
            DaemonAcquireOutcome::AlreadyRunning | DaemonAcquireOutcome::Spawned => {
                tracing::info!("daemon reattached / respawned after a mid-session loss");
            }
            DaemonAcquireOutcome::GaveUp => {
                tracing::error!(
                    "runtime daemon respawn gave up; retrying on the next health check"
                );
            }
        }
        tokio::time::sleep(DAEMON_RESPAWN_COOLDOWN).await;
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _log_guard = logging::init_file_logging();
    logging::install_panic_hook();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let hidden_relaunch = argv.iter().any(|a| a == autostart::HIDDEN_FLAG);
            tracing::info!(
                hidden_relaunch,
                "second app instance launch redirected to this instance",
            );
            if !hidden_relaunch {
                tray::show_main_window(app);
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(BackgroundEnabled(AtomicBool::new(true)))
        .manage(DaemonConn::new())
        .manage(SpawnedDaemonPid::new())
        .setup(|app| {
            if let Err(err) = tray::build_tray(app.handle()) {
                tracing::warn!(error = %err, "could not create system tray; continuing without it");
            }

            if autostart::launched_hidden() {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let conn = handle.state::<DaemonConn>();
                let spawned_pid = handle.state::<SpawnedDaemonPid>();
                match acquire_daemon(&conn, &spawned_pid).await {
                    DaemonAcquireOutcome::AlreadyRunning | DaemonAcquireOutcome::Spawned => {}
                    DaemonAcquireOutcome::GaveUp => {}
                }
                if let Ok(enabled) = load_background_enabled(&conn).await {
                    handle
                        .state::<BackgroundEnabled>()
                        .0
                        .store(enabled, Ordering::Relaxed);
                }
                if let Ok(Response::Settings(settings)) = conn.request(&Request::GetSettings).await
                {
                    tray::sync_indexing_label(&handle, settings.indexing_enabled);
                    autostart::sync(settings.run_on_boot);
                }
            });
            tauri::async_runtime::spawn(run_daemon_health_monitor(app.handle().clone()));
            Ok(())
        })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_) => {
                let _ = window.app_handle().save_window_state(StateFlags::all());
            }
            tauri::WindowEvent::CloseRequested { api, .. } => {
                let _ = window.app_handle().save_window_state(StateFlags::all());
                let enabled = window
                    .state::<BackgroundEnabled>()
                    .0
                    .load(Ordering::Relaxed);
                if tray::on_close(enabled) == tray::CloseAction::Hide {
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    tray::shutdown_daemon(window.app_handle());
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            ping_daemon,
            list_groups,
            list_group_detail,
            group_stats,
            daemon_progress,
            delete_files,
            undo_last_delete,
            partial_overlaps,
            list_clusters,
            cluster_detail,
            thumbnail,
            cluster_stats,
            failed_tasks,
            cross_group_conflicts,
            get_settings,
            set_settings,
            reveal_in_folder,
            open_folder,
            pick_folder,
            rescan_directory,
            force_rescan_directory,
            set_log_level,
            export_diagnostics,
            frontend_trace_enabled
        ])
        .run(tauri::generate_context!())
        .expect("error while running vidcull UI");
}
