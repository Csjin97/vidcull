use std::path::Path;
use std::sync::{Arc, Mutex};

use std::sync::atomic::{AtomicU64, Ordering};
use vidcull_core::types::{Blake3Hash, Codec, FileId, NormalizedPath, Resolution, VideoDuration};
use vidcull_daemon::delete::RemoveOutcome;
use vidcull_daemon::{
    ChangeKind, ChangeTask, DaemonRequestHandler, DeleteMode, FileRemover, LogBuffer,
    OsFileRemover, ShutdownToken, ThumbnailProvider,
};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, NewTask,
    PartialEdgeSpan, PartialMihRepo, PartialSkipMarker, SimilarityEdge, SimilarityEdgesRepo,
    TaskQueueRepo, TaskState, TrustLevel as DbTrust,
};
use vidcull_fingerprint::format as fp_format;
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_ipc::protocol::PROTOCOL_VERSION;
use vidcull_ipc::{
    Action, ClipOverlap, ClusterMemberDetail, DeleteRequest, FileDetail, IpcClient, IpcServer,
    LogLevel, LogRecord, Request, Response, TrustLevel,
};
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
};
use vidcull_matcher::partial::partial_clip_params;

#[derive(Default)]
struct RecordingRemover {
    removed: Mutex<Vec<(String, DeleteMode)>>,
    fail: bool,
}

impl FileRemover for RecordingRemover {
    fn remove(&self, path: &Path, mode: DeleteMode) -> std::io::Result<RemoveOutcome> {
        if self.fail {
            return Err(std::io::Error::other("fake remover failure"));
        }
        self.removed
            .lock()
            .unwrap()
            .push((path.to_string_lossy().into_owned(), mode));
        Ok(match mode {
            DeleteMode::Trash => RemoveOutcome::Trashed,
            DeleteMode::Permanent => RemoveOutcome::PermanentlyDeleted,
        })
    }
}

struct HardDeleteFallbackRemover;

impl FileRemover for HardDeleteFallbackRemover {
    fn remove(&self, _path: &Path, _mode: DeleteMode) -> std::io::Result<RemoveOutcome> {
        Ok(RemoveOutcome::PermanentlyDeleted)
    }
}

const HASH_LEN: usize = 32;
const T0: i64 = 1_700_000_000;

fn unique_endpoint() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\vidcull-bridge-{pid}-{n}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("vidcull-bridge-{pid}-{n}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn seed_file(db: &Database, path: &str, hash_byte: u8) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: 0,
        inode: None,
        content_hash: Some(Blake3Hash::from_bytes([hash_byte; HASH_LEN])),
        codec: None,
        container: None,
        duration: None,
        fps_x1000: None,
        bitrate_bps: None,
        resolution: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn on_disk_mtime_ns(path: &std::path::Path) -> i64 {
    let modified = std::fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("modified");
    let nanos = modified
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post-epoch mtime in tests")
        .as_nanos();
    i64::try_from(nanos).unwrap_or(i64::MAX)
}

fn seed_rich(db: &Database, path: &str, size_bytes: i64, hash_byte: u8) -> FileId {
    let (size_bytes, mtime_ns) = match std::fs::metadata(path) {
        Ok(meta) => (
            i64::try_from(meta.len()).unwrap_or(i64::MAX),
            on_disk_mtime_ns(std::path::Path::new(path)),
        ),
        Err(_) => (size_bytes, 0),
    };
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes,
        mtime_ns,
        inode: None,
        content_hash: Some(Blake3Hash::from_bytes([hash_byte; HASH_LEN])),
        codec: Some(Codec::H264),
        container: Some("mp4".to_owned()),
        duration: Some(VideoDuration::from_millis(60_000)),
        fps_x1000: Some(30_000),
        bitrate_bps: Some(5_000_000),
        resolution: Some(Resolution::new(1920, 1080)),
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert rich file")
}

fn enqueue(db: &Database, kind: &str) -> i64 {
    TaskQueueRepo::new(db.conn())
        .enqueue(&NewTask {
            kind: kind.to_owned(),
            priority: 0,
            payload: None,
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue")
}

struct Harness {
    address: String,
    shutdown: ShutdownToken,
    logs: LogBuffer,
    serve: tokio::task::JoinHandle<vidcull_core::Result<()>>,
    _dir: tempfile::TempDir,
}

fn spawn_bridge() -> Harness {
    spawn_seeded(|db| {
        enqueue(db, "scan");
        enqueue(db, "scan");
        let running = enqueue(db, "scan");
        let done = enqueue(db, "scan");
        {
            let repo = TaskQueueRepo::new(db.conn());
            let _first = repo.dequeue_next("scan", T0).expect("dq1").expect("first");
            let second = repo.dequeue_next("scan", T0).expect("dq2").expect("second");
            repo.mark_done(second.id, T0 + 1).expect("mark done");
            let _ = (running, done);
        }
        let f1 = seed_file(db, "/lib/a.mp4", 0xaa);
        let f2 = seed_file(db, "/lib/b.mp4", 0xaa);
        {
            let repo = DuplicateGroupsRepo::new(db.conn());
            let gid = repo.create(DbTrust::Exact, T0).expect("create group");
            repo.add_member(gid, f1).expect("add f1");
            repo.add_member(gid, f2).expect("add f2");
            repo.set_best(gid, Some(f1), T0 + 1).expect("set best");
        }
    })
}

fn spawn_seeded(seed: impl FnOnce(&Database)) -> Harness {
    spawn_seeded_with(Arc::new(RecordingRemover::default()), seed)
}

fn spawn_seeded_with(remover: Arc<dyn FileRemover>, seed: impl FnOnce(&Database)) -> Harness {
    spawn_seeded_full(remover, None, seed)
}

fn spawn_seeded_with_thumbnails(
    provider: Arc<ThumbnailProvider>,
    seed: impl FnOnce(&Database),
) -> Harness {
    spawn_seeded_full(Arc::new(RecordingRemover::default()), Some(provider), seed)
}

fn spawn_seeded_full(
    remover: Arc<dyn FileRemover>,
    thumbnails: Option<Arc<ThumbnailProvider>>,
    seed: impl FnOnce(&Database),
) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("vidcull.db");
    let db = vidcull_db::open_file(&db_path).expect("open db");

    seed(&db);

    let db = Arc::new(Mutex::new(db));
    let shutdown = ShutdownToken::new();
    let logs = LogBuffer::default();
    let mut handler = DaemonRequestHandler::new(
        db,
        shutdown.clone(),
        logs.clone(),
        "scan".to_owned(),
        remover,
    );
    if let Some(provider) = thumbnails {
        handler = handler.with_thumbnails(provider);
    }
    let handler = Arc::new(handler);

    let server = IpcServer::bind(&unique_endpoint()).expect("bind");
    let address = server.address().to_owned();
    let sd = shutdown.clone();
    let serve = tokio::spawn(async move {
        server
            .serve(handler, async move { sd.cancelled().await })
            .await
    });

    Harness {
        address,
        shutdown,
        logs,
        serve,
        _dir: dir,
    }
}

fn spawn_seeded_mut<T>(seed: impl FnOnce(&mut Database) -> T) -> (Harness, T) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("vidcull.db");
    let mut db = vidcull_db::open_file(&db_path).expect("open db");

    let out = seed(&mut db);

    let db = Arc::new(Mutex::new(db));
    let shutdown = ShutdownToken::new();
    let logs = LogBuffer::default();
    let handler = Arc::new(DaemonRequestHandler::new(
        db,
        shutdown.clone(),
        logs.clone(),
        "scan".to_owned(),
        Arc::new(RecordingRemover::default()),
    ));

    let server = IpcServer::bind(&unique_endpoint()).expect("bind");
    let address = server.address().to_owned();
    let sd = shutdown.clone();
    let serve = tokio::spawn(async move {
        server
            .serve(handler, async move { sd.cancelled().await })
            .await
    });

    (
        Harness {
            address,
            shutdown,
            logs,
            serve,
            _dir: dir,
        },
        out,
    )
}

async fn fetch_group_detail(client: &mut IpcClient, group_id: i64) -> Vec<FileDetail> {
    let chunks = client
        .request_stream(&Request::GroupDetail { group_id })
        .await
        .expect("request_stream GroupDetail");
    let mut members: Vec<FileDetail> = Vec::new();
    for chunk in chunks {
        match chunk {
            Response::GroupDetail(m) => members.extend(m),
            other => panic!("unexpected frame in GroupDetail stream: {other:?}"),
        }
    }
    members
}

async fn fetch_cluster_detail(client: &mut IpcClient, cluster_id: i64) -> Vec<ClusterMemberDetail> {
    let chunks = client
        .request_stream(&Request::ClusterDetail { cluster_id })
        .await
        .expect("request_stream ClusterDetail");
    let mut members: Vec<ClusterMemberDetail> = Vec::new();
    for chunk in chunks {
        match chunk {
            Response::ClusterDetail(m) => members.extend(m),
            other => panic!("unexpected frame in ClusterDetail stream: {other:?}"),
        }
    }
    members
}

#[tokio::test]
async fn ping_round_trips_over_real_socket() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client.request(&Request::Ping).await.expect("ping");
    assert_eq!(
        response,
        Response::Pong {
            protocol_version: PROTOCOL_VERSION
        }
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn enqueue_failed(db: &Database, kind: &str, path: &str, error: &str) -> i64 {
    let payload = ChangeTask {
        path: NormalizedPath::new(path),
        change: ChangeKind::Upsert,
        size_bytes: 0,
    }
    .to_payload()
    .expect("encode payload");
    let repo = TaskQueueRepo::new(db.conn());
    let id = repo
        .enqueue(&NewTask {
            kind: kind.to_owned(),
            priority: 0,
            payload: Some(payload),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue failed task");
    repo.dequeue_next(kind, T0)
        .expect("dequeue")
        .expect("claim");
    repo.mark_failed(id, T0 + 1, error).expect("mark failed");
    id
}

fn enqueue_failed_kind(db: &Database, path: &str, change: ChangeKind, error: &str) -> i64 {
    let payload = ChangeTask {
        path: NormalizedPath::new(path),
        change,
        size_bytes: 0,
    }
    .to_payload()
    .expect("encode payload");
    let repo = TaskQueueRepo::new(db.conn());
    let id = repo
        .enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: 0,
            payload: Some(payload),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue failed task");
    repo.dequeue_next("scan", T0)
        .expect("dequeue")
        .expect("claim");
    repo.mark_failed(id, T0 + 1, error).expect("mark failed");
    id
}

#[tokio::test]
async fn failed_tasks_and_count_dedup_one_file_across_change_kinds() {
    let h = spawn_seeded(|db| {
        enqueue_failed_kind(db, "/lib/timeline.mp4", ChangeKind::Upsert, "timed out");
        enqueue_failed_kind(
            db,
            "/lib/timeline.mp4",
            ChangeKind::ForceUpsert,
            "timed out again",
        );
        enqueue_failed_kind(
            db,
            "/lib/timeline.mp4",
            ChangeKind::ForceUpsert,
            "decode failed",
        );
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    match client
        .request(&Request::FailedTasks { limit: 50 })
        .await
        .expect("failed tasks")
    {
        Response::FailedTasks(tasks) => {
            assert_eq!(
                tasks.len(),
                1,
                "three rows for one file collapse to one entry"
            );
            assert_eq!(tasks[0].path, "/lib/timeline.mp4");
            assert_eq!(
                tasks[0].attempts, 3,
                "1 Upsert + 2 ForceUpsert tries summed"
            );
            assert_eq!(
                tasks[0].reason, "decode failed",
                "the most-recent row's reason"
            );
        }
        other => panic!("expected FailedTasks, got {other:?}"),
    }
    match client.request(&Request::Progress).await.expect("progress") {
        Response::Progress(p) => {
            assert_eq!(p.failed, 1, "one distinct failed file, not two payloads");
        }
        other => panic!("expected Progress, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn failed_excludes_files_that_are_actually_indexed() {
    let h = spawn_seeded(|db| {
        seed_file(db, "/lib/h265.mp4", 0x44);
        enqueue_failed_kind(
            db,
            "/lib/h265.mp4",
            ChangeKind::ForceUpsert,
            "timed out after 300s",
        );
        enqueue_failed_kind(
            db,
            "/lib/timeline.mp4",
            ChangeKind::Upsert,
            "corrupt: no sequence header",
        );
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    match client
        .request(&Request::FailedTasks { limit: 50 })
        .await
        .expect("failed tasks")
    {
        Response::FailedTasks(tasks) => {
            assert_eq!(
                tasks.len(),
                1,
                "only the unindexed corrupt file is a failure"
            );
            assert_eq!(tasks[0].path, "/lib/timeline.mp4");
        }
        other => panic!("expected FailedTasks, got {other:?}"),
    }
    match client.request(&Request::Progress).await.expect("progress") {
        Response::Progress(p) => {
            assert_eq!(p.failed, 1, "indexed-but-rescan-failed file excluded");
            assert_eq!(
                p.partial_failed, 0,
                "a transient rescan timeout on an indexed file must stay suppressed",
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_surfaces_permanent_reindex_failure_as_partial_failed() {
    let h = spawn_seeded(|db| {
        seed_file(db, "/lib/corrupted.mp4", 0x55);
        enqueue_failed_kind(
            db,
            "/lib/corrupted.mp4",
            ChangeKind::ForceUpsert,
            "decode error: invalid NAL unit (re-encoded into corrupt content)",
        );
        enqueue_failed_kind(
            db,
            "/lib/never_indexed.mp4",
            ChangeKind::Upsert,
            "decode error: no sequence header",
        );
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    match client.request(&Request::Progress).await.expect("progress") {
        Response::Progress(p) => {
            assert_eq!(
                p.partial_failed, 1,
                "the indexed-but-permanently-undecodable file must surface as a genuine failure",
            );
            assert_eq!(
                p.failed, 1,
                "only the never-indexed corrupt file is an ordinary `failed` (channels distinct)",
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn failed_tasks_projects_real_failed_queue_rows() {
    let h = spawn_seeded(|db| {
        enqueue_failed(db, "scan", "/lib/one.mp4", "decode error: bad stream");
        enqueue_failed(db, "scan", "/lib/two.mkv", "ffprobe timed out");
        enqueue_failed(db, "scan", "/lib/three.mp4", "io error: permission denied");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client
        .request(&Request::FailedTasks { limit: 50 })
        .await
        .expect("failed tasks");
    match response {
        Response::FailedTasks(tasks) => {
            assert_eq!(tasks.len(), 3, "all three failures returned");
            assert_eq!(tasks[0].path, "/lib/three.mp4");
            assert_eq!(tasks[0].reason, "io error: permission denied");
            assert_eq!(tasks[0].attempts, 1, "claimed once before failing");
            assert_eq!(tasks[2].path, "/lib/one.mp4");
        }
        other => panic!("expected FailedTasks, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn failed_tasks_honours_limit_and_is_empty_when_none_failed() {
    let empty = spawn_bridge();
    let mut client = IpcClient::connect(&empty.address).await.expect("connect");
    match client
        .request(&Request::FailedTasks { limit: 10 })
        .await
        .expect("failed tasks")
    {
        Response::FailedTasks(tasks) => assert!(tasks.is_empty(), "nothing failed"),
        other => panic!("expected FailedTasks, got {other:?}"),
    }
    empty.shutdown.trigger();
    let _ = empty.serve.await;

    let h = spawn_seeded(|db| {
        for i in 0..5 {
            enqueue_failed(db, "scan", &format!("/lib/f{i}.mp4"), "boom");
        }
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    match client
        .request(&Request::FailedTasks { limit: 2 })
        .await
        .expect("failed tasks")
    {
        Response::FailedTasks(tasks) => {
            assert_eq!(tasks.len(), 2, "capped at the limit");
            assert_eq!(tasks[0].path, "/lib/f4.mp4", "most recent first");
            assert_eq!(tasks[1].path, "/lib/f3.mp4");
        }
        other => panic!("expected FailedTasks, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_reflects_real_queue_state() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client.request(&Request::Progress).await.expect("progress");
    match response {
        Response::Progress(p) => {
            assert_eq!(p.pending, 2, "two tasks left PENDING");
            assert_eq!(p.running, 1, "one task claimed and not finished");
            assert_eq!(p.done, 2, "two active indexed files");
            assert_eq!(p.failed, 0);
            assert!(p.rss_bytes > 0, "rss_bytes must be measured, not stubbed");
            assert!(p.cpu_usage_permille <= 1000, "cpu permille is bounded");
            let _ = p.throughput_bytes_per_sec;
            assert!(
                p.current_files.is_empty(),
                "no payload on the running task → no current file"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_reports_partial_clip_pass_done_count() {
    const PARTIAL: i32 = -200;
    let h = spawn_seeded(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        let payload = |tag: u8| {
            ChangeTask {
                path: NormalizedPath::new(format!("/lib/partial_{tag}.mp4")),
                change: ChangeKind::Densify,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode partial payload")
        };
        seed_file(db, "/lib/partial_1.mp4", 0x01);
        seed_file(db, "/lib/partial_2.mp4", 0x02);
        seed_file(db, "/lib/partial_3.mp4", 0x03);
        let done_id = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload(1)),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue done partial");
        let done_dup_id = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload(1)),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue done partial dup");
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: PARTIAL,
            payload: Some(payload(2)),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue running partial");
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: PARTIAL,
            payload: Some(payload(3)),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue pending partial");

        repo.mark_done(done_id, T0 + 1).expect("mark partial done");
        repo.mark_done(done_dup_id, T0 + 1)
            .expect("mark partial done dup");
        repo.dequeue_next("scan", T0)
            .expect("dequeue partial")
            .expect("a partial task to run");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client.request(&Request::Progress).await.expect("progress");
    match response {
        Response::Progress(p) => {
            assert_eq!(p.partial_done, 1, "one distinct partial file finished");
            assert_eq!(p.partial_running, 1, "one partial file in flight");
            assert_eq!(p.partial_pending, 1, "one partial file still queued");
            assert!(
                p.partial_done <= p.partial_done + p.partial_pending + p.partial_running,
                "done is the numerator of the N/M total"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_excludes_partial_skips_from_done_numerator() {
    const PARTIAL: i32 = -200;
    let h = spawn_seeded(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        let payload = |tag: u8| {
            ChangeTask {
                path: NormalizedPath::new(format!("/lib/partial_{tag}.mp4")),
                change: ChangeKind::Densify,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode partial payload")
        };
        seed_file(db, "/lib/partial_1.mp4", 0x01);
        seed_file(db, "/lib/partial_2.mp4", 0x02);
        for tag in [1u8, 2] {
            let id = repo
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL,
                    payload: Some(payload(tag)),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue partial");
            repo.mark_done(id, T0 + 1).expect("mark partial done");
        }
        let skipped = seed_file(db, "/lib/av1_skip.mp4", 0xAB);
        let fps = FingerprintsRepo::new(db.conn());
        fps.upsert(&Fingerprint {
            file_id: skipped,
            tier1_global: vec![0u8; 8],
            tier2_temporal: None,
            format_version: 1,
            created_at: T0,
        })
        .expect("upsert fingerprint");
        fps.set_partial_skip(
            skipped,
            &PartialSkipMarker {
                reason: "unsupported-codec".to_owned(),
                size_bytes: 1024,
                mtime_ns: 0,
            },
        )
        .expect("stamp skip marker");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client.request(&Request::Progress).await.expect("progress");
    match response {
        Response::Progress(p) => {
            assert_eq!(
                p.partial_done, 1,
                "skip-marked file excluded from the N/M numerator (2 raw DONE − 1 skip)",
            );
            assert_eq!(
                p.partial_skipped.get("unsupported-codec").copied(),
                Some(1),
                "the skip is surfaced under its reason",
            );
            assert_eq!(
                p.partial_skipped.len(),
                1,
                "only the one stamped reason appears",
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_partial_total_drops_when_folder_soft_deleted() {
    const PARTIAL: i32 = -200;
    let (h, removed_ids) = spawn_seeded_mut(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        let payload = |tag: u8| {
            ChangeTask {
                path: NormalizedPath::new(format!("/lib/keep_{tag}.mp4")),
                change: ChangeKind::Densify,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode keep payload")
        };
        let removed_payload = |tag: u8| {
            ChangeTask {
                path: NormalizedPath::new(format!("/removed/gone_{tag}.mp4")),
                change: ChangeKind::Densify,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode removed payload")
        };
        seed_file(db, "/lib/keep_1.mp4", 0x01);
        seed_file(db, "/lib/keep_2.mp4", 0x02);
        let removed_1 = seed_file(db, "/removed/gone_1.mp4", 0x03);
        let removed_2 = seed_file(db, "/removed/gone_2.mp4", 0x04);
        for tag in [1u8, 2] {
            let id = repo
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL,
                    payload: Some(payload(tag)),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue keep partial");
            repo.mark_done(id, T0 + 1).expect("mark keep done");
        }
        for tag in [1u8, 2] {
            let id = repo
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL,
                    payload: Some(removed_payload(tag)),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue removed partial");
            repo.mark_done(id, T0 + 1).expect("mark removed done");
        }
        (removed_1, removed_2)
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    match client
        .request(&Request::Progress)
        .await
        .expect("progress before removal")
    {
        Response::Progress(p) => {
            assert_eq!(p.partial_done, 4, "all four partial files active + done");
            assert_eq!(
                p.partial_done + p.partial_pending + p.partial_running,
                4,
                "M = 4 before the folder is removed"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }

    {
        let db2 = vidcull_db::open_file(&h._dir.path().join("vidcull.db")).expect("reopen db");
        let files_repo = FilesRepo::new(db2.conn());
        files_repo
            .mark_deleted(removed_ids.0, T0 + 2)
            .expect("soft-delete gone_1");
        files_repo
            .mark_deleted(removed_ids.1, T0 + 2)
            .expect("soft-delete gone_2");
    }

    match client
        .request(&Request::Progress)
        .await
        .expect("progress after removal")
    {
        Response::Progress(p) => {
            assert_eq!(
                p.partial_done, 2,
                "the two soft-deleted files drop out of the numerator"
            );
            assert_eq!(
                p.partial_done + p.partial_pending + p.partial_running,
                2,
                "M falls by exactly the two removed active files (was 4, now 2)"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_partial_n_never_exceeds_m() {
    const PARTIAL: i32 = -200;
    let h = spawn_seeded(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        let payload = |tag: &str| {
            ChangeTask {
                path: NormalizedPath::new(format!("/lib/{tag}.mp4")),
                change: ChangeKind::Densify,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode payload")
        };
        seed_file(db, "/lib/done_active.mp4", 0x01);
        let done_id = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload("done_active")),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue done_active");
        repo.mark_done(done_id, T0 + 1).expect("mark done_active");

        let deleted_id = seed_file(db, "/lib/done_deleted.mp4", 0x02);
        let deleted_task = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload("done_deleted")),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue done_deleted");
        repo.mark_done(deleted_task, T0 + 1)
            .expect("mark done_deleted");
        FilesRepo::new(db.conn())
            .mark_deleted(deleted_id, T0 + 2)
            .expect("soft-delete done_deleted");

        seed_file(db, "/lib/pending_active.mp4", 0x03);
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: PARTIAL,
            payload: Some(payload("pending_active")),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue pending_active");

        seed_file(db, "/lib/running_active.mp4", 0x04);
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: PARTIAL,
            payload: Some(payload("running_active")),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue running_active");
        repo.dequeue_next("scan", T0).expect("claim 1");
        repo.dequeue_next("scan", T0).expect("claim 2");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    match client.request(&Request::Progress).await.expect("progress") {
        Response::Progress(p) => {
            let m = p.partial_done + p.partial_pending + p.partial_running;
            assert!(
                p.partial_done <= m,
                "N ({}) must never exceed M ({m})",
                p.partial_done
            );
            assert_eq!(p.partial_done, 1, "only the active DONE file counts");
            assert_eq!(m, 3, "soft-deleted file excluded from M too");
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_skip_marked_deleted_file_subtracted_once() {
    const PARTIAL: i32 = -200;
    let h = spawn_seeded(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        let payload = |tag: &str| {
            ChangeTask {
                path: NormalizedPath::new(format!("/lib/{tag}.mp4")),
                change: ChangeKind::Densify,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode payload")
        };
        let fps = FingerprintsRepo::new(db.conn());

        seed_file(db, "/lib/real_done.mp4", 0x01);
        let real_done_task = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload("real_done")),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue real_done");
        repo.mark_done(real_done_task, T0 + 1)
            .expect("mark real_done");

        let skipped_active = seed_file(db, "/lib/skip_active.mp4", 0x02);
        fps.upsert(&Fingerprint {
            file_id: skipped_active,
            tier1_global: vec![0u8; 8],
            tier2_temporal: None,
            format_version: 1,
            created_at: T0,
        })
        .expect("upsert skip_active fingerprint");
        fps.set_partial_skip(
            skipped_active,
            &PartialSkipMarker {
                reason: "unsupported-codec".to_owned(),
                size_bytes: 1024,
                mtime_ns: 0,
            },
        )
        .expect("stamp skip_active marker");
        let skip_active_task = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload("skip_active")),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue skip_active");
        repo.mark_done(skip_active_task, T0 + 1)
            .expect("mark skip_active done");

        let skipped_deleted = seed_file(db, "/lib/skip_deleted.mp4", 0x03);
        fps.upsert(&Fingerprint {
            file_id: skipped_deleted,
            tier1_global: vec![0u8; 8],
            tier2_temporal: None,
            format_version: 1,
            created_at: T0,
        })
        .expect("upsert skip_deleted fingerprint");
        fps.set_partial_skip(
            skipped_deleted,
            &PartialSkipMarker {
                reason: "decode-failed".to_owned(),
                size_bytes: 1024,
                mtime_ns: 0,
            },
        )
        .expect("stamp skip_deleted marker");
        let skip_deleted_task = repo
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL,
                payload: Some(payload("skip_deleted")),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue skip_deleted");
        repo.mark_done(skip_deleted_task, T0 + 1)
            .expect("mark skip_deleted done");
        FilesRepo::new(db.conn())
            .mark_deleted(skipped_deleted, T0 + 2)
            .expect("soft-delete skip_deleted");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    match client.request(&Request::Progress).await.expect("progress") {
        Response::Progress(p) => {
            assert_eq!(
                p.partial_done, 1,
                "skip-marked-and-deleted file subtracted exactly once (no under-count)"
            );
            assert_eq!(
                p.partial_skipped.get("unsupported-codec").copied(),
                Some(1),
                "the active skip is still surfaced"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_partial_done_active_files_are_always_hashed() {
    const PARTIAL: i32 = -200;
    let h = spawn_seeded(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        seed_file(db, "/lib/premise_a.mp4", 0x11);
        seed_file(db, "/lib/premise_b.mp4", 0x12);
        for tag in ["premise_a", "premise_b"] {
            let payload = ChangeTask {
                path: NormalizedPath::new(format!("/lib/{tag}.mp4")),
                change: ChangeKind::PartialFingerprint,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode payload");
            let id = repo
                .enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: PARTIAL,
                    payload: Some(payload),
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue partial");
            repo.mark_done(id, T0 + 1).expect("mark partial done");
        }
    });

    let db2 = vidcull_db::open_file(&h._dir.path().join("vidcull.db")).expect("reopen db");
    let repo = TaskQueueRepo::new(db2.conn());
    let files_repo = FilesRepo::new(db2.conn());
    let done_tasks = repo
        .list_by_priority_state(PARTIAL, TaskState::Done)
        .expect("list partial done tasks");
    assert_eq!(done_tasks.len(), 2, "both partial tasks reached DONE");
    let paths: Vec<NormalizedPath> = done_tasks
        .iter()
        .filter_map(|t| t.payload.as_deref())
        .filter_map(|bytes| ChangeTask::from_payload(bytes).ok())
        .map(|change| change.path)
        .collect();
    assert_eq!(paths.len(), 2, "every DONE row decodes to a path");
    let active_hashed = files_repo
        .active_hashed_paths_in(&paths)
        .expect("active+hashed lookup");
    for path in &paths {
        assert!(
            active_hashed.contains(path.as_str()),
            "premise violated: {path:?} has a partial-DONE row but is not an \
             active, hashed file — a partial task outran indexing",
        );
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn progress_reports_current_files_excluding_densify() {
    let h = spawn_seeded(|db| {
        let repo = TaskQueueRepo::new(db.conn());
        let fg = ChangeTask {
            path: NormalizedPath::new("/lib/현재 처리.mp4"),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        }
        .to_payload()
        .expect("encode fg");
        let bg = ChangeTask {
            path: NormalizedPath::new("/lib/배경 densify.mp4"),
            change: ChangeKind::Densify,
            size_bytes: 0,
        }
        .to_payload()
        .expect("encode bg");
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: 0,
            payload: Some(fg),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue fg");
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: -1,
            payload: Some(bg),
            enqueued_at: T0,
            size_bytes: 0,
        })
        .expect("enqueue bg");
        repo.dequeue_next("scan", T0)
            .expect("dq1")
            .expect("fg running");
        repo.dequeue_next("scan", T0)
            .expect("dq2")
            .expect("bg running");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client.request(&Request::Progress).await.expect("progress");
    match response {
        Response::Progress(p) => {
            assert_eq!(
                p.running, 1,
                "only the foreground task counts as running (densify excluded)"
            );
            assert_eq!(
                p.current_files,
                vec!["/lib/현재 처리.mp4".to_owned()],
                "only the foreground RUNNING task's path; densify excluded"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn force_rescan_in_flight_drops_the_done_count() {
    let h = spawn_seeded(|db| {
        let _f1 = seed_file(db, "/lib/a.mp4", 0xaa);
        let _f2 = seed_file(db, "/lib/b.mp4", 0xbb);
        let repo = TaskQueueRepo::new(db.conn());
        for path in ["/lib/a.mp4", "/lib/b.mp4"] {
            let payload = ChangeTask {
                path: NormalizedPath::new(path),
                change: ChangeKind::ForceUpsert,
                size_bytes: 0,
            }
            .to_payload()
            .expect("encode force payload");
            repo.enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(payload),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue force task");
        }
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client.request(&Request::Progress).await.expect("progress");
    match response {
        Response::Progress(p) => {
            assert_eq!(p.pending, 2, "both forced files are pending");
            assert_eq!(
                p.done, 0,
                "in-flight force-reprocessed files are not counted as 완료"
            );
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn list_groups_pages_real_duplicate_groups() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client
        .request(&Request::ListGroups {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list");
    match response {
        Response::Groups(groups) => {
            assert_eq!(groups.len(), 1, "one seeded group");
            let g = groups[0];
            assert_eq!(g.trust, TrustLevel::Exact);
            assert_eq!(g.member_count, 2);
            assert!(g.best_file_id.is_some(), "best copy was set");
        }
        other => panic!("expected Groups, got {other:?}"),
    }

    let none = client
        .request(&Request::ListGroups {
            trust: Some(TrustLevel::Possible),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list possible");
    assert_eq!(none, Response::Groups(vec![]));

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn group_detail_projects_member_metadata_and_best_flag() {
    let h = spawn_seeded(|db| {
        let f1 = seed_rich(db, "/lib/a.mp4", 1_000_000, 0xaa);
        let f2 = seed_rich(db, "/lib/b.mp4", 400_000, 0xaa);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
        repo.set_best(gid, Some(f1), T0 + 1).expect("set best");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let members = fetch_group_detail(&mut client, 1).await;
    {
        assert_eq!(members.len(), 2, "both members projected");
        let a = &members[0];
        assert_eq!(a.file_id, 1);
        assert_eq!(a.path, "/lib/a.mp4");
        assert_eq!(a.size_bytes, 1_000_000);
        assert_eq!(a.width, Some(1920));
        assert_eq!(a.height, Some(1080));
        assert_eq!(a.duration_ms, Some(60_000));
        assert_eq!(a.bitrate_bps, Some(5_000_000));
        assert_eq!(a.codec.as_deref(), Some("h264"));
        assert_eq!(a.container.as_deref(), Some("mp4"));
        assert!(a.is_best, "f1 is the best copy");
        assert!(!members[1].is_best, "f2 is not the best copy");
        assert!(a.thumbnail.is_none(), "no provider ⇒ no thumbnail");
    }

    let unknown = fetch_group_detail(&mut client, 999).await;
    assert!(unknown.is_empty(), "unknown group_id returns an empty list");

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn group_detail_serves_a_cached_thumbnail_as_a_data_uri() {
    let cache = tempfile::tempdir().expect("cache dir");
    let hash_hex = Blake3Hash::from_bytes([0xaa; HASH_LEN]).to_hex();
    let pixels = vec![128u8; 32 * 18];
    let jpeg = vidcull_thumb::encode_thumbnail(
        vidcull_thumb::GrayView {
            width: 32,
            height: 18,
            pixels: &pixels,
        },
        vidcull_thumb::ThumbnailOptions::default(),
    )
    .expect("encode fixture jpeg");
    std::fs::write(cache.path().join(format!("{hash_hex}_0_v2.jpg")), &jpeg).expect("seed cache");

    let provider = Arc::new(ThumbnailProvider::new(cache.path().to_path_buf(), None));
    let h = spawn_seeded_with_thumbnails(provider, |db| {
        let f1 = seed_rich(db, "/lib/a.mp4", 1_000_000, 0xaa);
        let f2 = seed_rich(db, "/lib/b.mp4", 400_000, 0xbb);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
        repo.set_best(gid, Some(f1), T0 + 1).expect("set best");
    });

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let members = fetch_group_detail(&mut client, 1).await;
    {
        assert_eq!(members.len(), 2);
        let a = &members[0];
        let uri = a
            .thumbnail
            .as_deref()
            .expect("f1's cached preview is delivered");
        assert!(
            uri.starts_with("data:image/jpeg;base64,"),
            "expected a JPEG data URI, got {uri}"
        );
        assert!(
            uri.len() > "data:image/jpeg;base64,".len(),
            "URI carries bytes"
        );
        assert!(
            members[1].thumbnail.is_none(),
            "a cache miss without ffmpeg yields no preview"
        );
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn group_stats_counts_groups_and_reclaimable_bytes() {
    let h = spawn_seeded(|db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let a1 = seed_rich(db, "/x/a1.mp4", 1_000_000, 0x01);
        let a2 = seed_rich(db, "/x/a2.mp4", 400_000, 0x01);
        let g1 = repo.create(DbTrust::Exact, T0).expect("g1");
        repo.add_member(g1, a1).expect("a1");
        repo.add_member(g1, a2).expect("a2");
        repo.set_best(g1, Some(a1), T0).expect("best g1");

        let b1 = seed_rich(db, "/x/b1.mp4", 500, 0x02);
        let b2 = seed_rich(db, "/x/b2.mp4", 200, 0x02);
        let b3 = seed_rich(db, "/x/b3.mp4", 100, 0x02);
        let g2 = repo.create(DbTrust::VeryLikely, T0).expect("g2");
        repo.add_member(g2, b1).expect("b1");
        repo.add_member(g2, b2).expect("b2");
        repo.add_member(g2, b3).expect("b3");
        repo.set_best(g2, Some(b1), T0).expect("best g2");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let all = client
        .request(&Request::GroupStats { trust: None })
        .await
        .expect("stats all");
    assert_eq!(
        all,
        Response::GroupStats(vidcull_ipc::GroupStats {
            group_count: 2,
            reclaimable_bytes: 400_300,
        })
    );

    let exact = client
        .request(&Request::GroupStats {
            trust: Some(TrustLevel::Exact),
        })
        .await
        .expect("stats exact");
    assert_eq!(
        exact,
        Response::GroupStats(vidcull_ipc::GroupStats {
            group_count: 1,
            reclaimable_bytes: 400_000,
        })
    );

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn rescan_action_enqueues_a_task() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client
        .request(&Request::Action(Action::Rescan {
            path: "/lib/new".to_owned(),
        }))
        .await
        .expect("rescan");
    match response {
        Response::Action(result) => assert!(result.accepted, "rescan accepted: {}", result.detail),
        other => panic!("expected Action, got {other:?}"),
    }
    let progress = client.request(&Request::Progress).await.expect("progress");
    match progress {
        Response::Progress(p) => {
            assert_eq!(p.pending, 3);
            assert_eq!(p.running, 1);
            assert_eq!(p.done, 2);
            assert_eq!(p.failed, 0);
            assert!(p.rss_bytes > 0);
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn rescan_action_directory_diff() {
    let base = tempfile::tempdir().expect("tempdir");
    let base_path = base.path();
    let base_str = base_path.display().to_string().replace('\\', "/");

    let h = spawn_seeded(|db| {
        let _f1 = seed_file(db, &format!("{base_str}/existing.mp4"), 0x11);
        let _f2 = seed_file(db, &format!("{base_str}/removed.mp4"), 0x22);
    });

    let existing_path = base_path.join("existing.mp4");
    std::fs::write(&existing_path, b"video data").expect("write existing");
    let new_path = base_path.join("new.mp4");
    std::fs::write(&new_path, b"video data").expect("write new");

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client
        .request(&Request::Action(Action::Rescan {
            path: base_str.clone(),
        }))
        .await
        .expect("rescan");

    match response {
        Response::Action(result) => {
            assert!(result.accepted);
            assert!(
                result.detail.contains("enqueued 3 rescan tasks"),
                "got detail: {}",
                result.detail
            );
        }
        other => panic!("expected Action, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn force_rescan_action_reenqueues_every_present_file() {
    let base = tempfile::tempdir().expect("tempdir");
    let base_path = base.path();
    let base_str = base_path.display().to_string().replace('\\', "/");

    let h = spawn_seeded(|db| {
        let _f1 = seed_file(db, &format!("{base_str}/a.mp4"), 0x11);
        let _f2 = seed_file(db, &format!("{base_str}/b.mp4"), 0x22);
    });
    std::fs::write(base_path.join("a.mp4"), b"video a").expect("write a");
    std::fs::write(base_path.join("b.mp4"), b"video b").expect("write b");

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client
        .request(&Request::Action(Action::ForceRescan {
            path: base_str.clone(),
        }))
        .await
        .expect("force rescan");

    match response {
        Response::Action(result) => {
            assert!(result.accepted);
            assert!(
                result.detail.contains("enqueued 2 force-rescan tasks"),
                "force rescan must re-enqueue every present file; got: {}",
                result.detail
            );
        }
        other => panic!("expected Action, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn stream_logs_drains_the_ring_buffer() {
    let h = spawn_bridge();
    for i in 0..3 {
        h.logs.push(LogRecord {
            timestamp_ms: i,
            level: LogLevel::Info,
            target: "test".to_owned(),
            message: format!("line {i}"),
        });
    }
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let frames = client
        .request_stream(&Request::StreamLogs { max_records: 10 })
        .await
        .expect("stream");
    assert_eq!(
        frames.len(),
        3,
        "three buffered records, StreamEnd excluded"
    );
    let messages: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            Response::Log(r) => Some(r.message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, ["line 0", "line 1", "line 2"]);
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn empty_log_buffer_streams_nothing() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let frames = client
        .request_stream(&Request::StreamLogs { max_records: 10 })
        .await
        .expect("stream");
    assert!(frames.is_empty());
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn shutdown_action_triggers_the_token_and_stops_serving() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let response = client
        .request(&Request::Action(Action::Shutdown))
        .await
        .expect("shutdown");
    match response {
        Response::Action(result) => assert!(result.accepted),
        other => panic!("expected Action, got {other:?}"),
    }
    assert!(h.shutdown.is_triggered(), "handler triggered shutdown");
    let served = tokio::time::timeout(std::time::Duration::from_secs(5), h.serve).await;
    assert!(served.is_ok(), "serve loop stopped after shutdown");
}

fn seed_trio(db: &Database) {
    let f1 = seed_rich(db, "/lib/best.mp4", 1_000_000, 0xaa);
    let f2 = seed_rich(db, "/lib/dupe1.mp4", 400_000, 0xaa);
    let f3 = seed_rich(db, "/lib/dupe2.mp4", 300_000, 0xaa);
    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo.create(DbTrust::Exact, T0).expect("group");
    repo.add_member(gid, f1).expect("f1");
    repo.add_member(gid, f2).expect("f2");
    repo.add_member(gid, f3).expect("f3");
    repo.set_best(gid, Some(f1), T0).expect("best");
}

#[tokio::test]
async fn move_to_trash_removes_member_and_keeps_group_alive() {
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover.clone(), seed_trio);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    match resp {
        Response::Delete(r) => {
            assert!(r.ok, "delete accepted: {}", r.detail);
            assert_eq!(r.removed_file_ids, vec![2]);
            assert_eq!(r.reclaimed_bytes, 400_000, "f2's on-disk size reclaimed");
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    let recorded = remover.removed.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].0,
        NormalizedPath::new("/lib/dupe1.mp4")
            .to_native_path()
            .to_string_lossy()
    );
    assert_eq!(recorded[0].1, DeleteMode::Trash);

    let members = fetch_group_detail(&mut client, 1).await;
    {
        let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
        assert_eq!(ids, vec![1, 3], "f2 dropped; best (1) + f3 remain");
        assert!(
            members.iter().find(|m| m.file_id == 1).unwrap().is_best,
            "best copy unchanged after deleting a non-best member"
        );
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn delete_guards_reject_unsafe_requests_server_side() {
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover.clone(), seed_trio);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let all = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![1, 2, 3],
            confirm_best: true,
        })))
        .await
        .expect("delete all");
    assert!(matches!(all, Response::Delete(ref r) if !r.ok && r.removed_file_ids.is_empty()));

    let best = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![1],
            confirm_best: false,
        })))
        .await
        .expect("delete best");
    match best {
        Response::Delete(r) => {
            assert!(!r.ok);
            assert_eq!(
                r.reject_code.as_deref(),
                Some("BEST_UNCONFIRMED"),
                "best-copy guard sends the stable reject code"
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }

    let unknown = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![999],
            confirm_best: false,
        })))
        .await
        .expect("delete unknown");
    assert!(matches!(unknown, Response::Delete(ref r) if !r.ok));

    assert!(
        remover.removed.lock().unwrap().is_empty(),
        "guards block before any file is removed"
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn deleting_down_to_one_member_drops_the_group() {
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover.clone(), |db| {
        let f1 = seed_rich(db, "/lib/keep.mp4", 1_000, 0xcc);
        let f2 = seed_rich(db, "/lib/gone.mp4", 500, 0xcc);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("group");
        repo.add_member(gid, f1).expect("f1");
        repo.add_member(gid, f2).expect("f2");
        repo.set_best(gid, Some(f1), T0).expect("best");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(matches!(&resp, Response::Delete(r) if r.ok && r.removed_file_ids == vec![2]));
    assert_eq!(
        remover.removed.lock().unwrap()[0].1,
        DeleteMode::Permanent,
        "permanent delete uses the permanent mode"
    );

    let stats = client
        .request(&Request::GroupStats { trust: None })
        .await
        .expect("stats");
    assert_eq!(
        stats,
        Response::GroupStats(vidcull_ipc::GroupStats {
            group_count: 0,
            reclaimable_bytes: 0,
        })
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn filesystem_failure_leaves_the_database_untouched() {
    let remover = Arc::new(RecordingRemover {
        fail: true,
        ..Default::default()
    });
    let h = spawn_seeded_with(remover, seed_trio);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(
        matches!(resp, Response::Delete(ref r) if !r.ok && r.removed_file_ids.is_empty()),
        "a failed filesystem op reports failure and removes nothing"
    );

    let members = fetch_group_detail(&mut client, 1).await;
    assert_eq!(members.len(), 3, "all three members present; db untouched");
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[derive(Default)]
struct NativePathRecordingRemover {
    received: Mutex<Vec<std::path::PathBuf>>,
}

impl FileRemover for NativePathRecordingRemover {
    fn remove(&self, path: &Path, mode: DeleteMode) -> std::io::Result<RemoveOutcome> {
        self.received.lock().unwrap().push(path.to_path_buf());
        if std::fs::symlink_metadata(path)
            .err()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(RemoveOutcome::AlreadyAbsent);
        }
        Ok(match mode {
            DeleteMode::Trash => RemoveOutcome::Trashed,
            DeleteMode::Permanent => RemoveOutcome::PermanentlyDeleted,
        })
    }
}

#[cfg(windows)]
#[tokio::test]
async fn unc_delete_routes_native_pathbuf() {
    let files_dir = tempfile::tempdir().expect("files dir");
    let best = real_file(files_dir.path(), "best.mp4");
    let unc = "//server/share/dupe.mp4";
    let remover = Arc::new(NativePathRecordingRemover::default());
    let h = spawn_seeded_with(remover.clone(), {
        let best = best.clone();
        move |db| {
            let f1 = seed_rich(db, &best, 1_000, 0xaa);
            let f2 = seed_file(db, unc, 0xaa);
            let repo = DuplicateGroupsRepo::new(db.conn());
            let gid = repo.create(DbTrust::Exact, T0).expect("group");
            repo.add_member(gid, f1).expect("f1");
            repo.add_member(gid, f2).expect("f2");
            repo.set_best(gid, Some(f1), T0).expect("best");
        }
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");

    let received = remover.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1, "the UNC dupe reached the remover");
    let received_str = received[0].to_string_lossy();
    assert_eq!(
        received_str, r"\\server\share\dupe.mp4",
        "delete I/O routes the native UNC path (backslashes), not the // mangle"
    );
    assert!(
        !received_str.contains('/'),
        "no forward slash survives into the native delete target: {received_str}"
    );

    match resp {
        Response::Delete(r) => {
            assert!(r.ok, "the absent stale row is cleaned up: {}", r.detail);
            assert_eq!(r.removed_file_ids, vec![2]);
            assert!(
                !r.detail.contains("휴지통으로 이동"),
                "an absent UNC path must not be reported as moved to trash: {}",
                r.detail
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[cfg(windows)]
#[tokio::test]
async fn ordinary_delete_routes_native_pathbuf_and_trashes() {
    let files_dir = tempfile::tempdir().expect("files dir");
    let best = real_file(files_dir.path(), "best.mp4");
    let dupe = real_file(files_dir.path(), "dupe.mp4");
    let remover = Arc::new(NativePathRecordingRemover::default());
    let h = spawn_seeded_with(remover.clone(), {
        let best = best.clone();
        let dupe = dupe.clone();
        move |db| {
            let f1 = seed_rich(db, &best, 1_000, 0xbb);
            let f2 = seed_rich(db, &dupe, 1_000, 0xbb);
            let repo = DuplicateGroupsRepo::new(db.conn());
            let gid = repo.create(DbTrust::Exact, T0).expect("group");
            repo.add_member(gid, f1).expect("f1");
            repo.add_member(gid, f2).expect("f2");
            repo.set_best(gid, Some(f1), T0).expect("best");
        }
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");

    let received = remover.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    let received_str = received[0].to_string_lossy();
    assert_eq!(
        received_str,
        dupe.replace('/', "\\"),
        "ordinary delete routes the native (backslash) path"
    );
    assert!(
        received[0].exists(),
        "the native path resolves the real on-disk file"
    );
    match resp {
        Response::Delete(r) => {
            assert!(r.ok, "ordinary delete succeeds: {}", r.detail);
            assert_eq!(r.removed_file_ids, vec![2]);
            assert!(
                r.detail.contains("휴지통으로 이동"),
                "a present file is honestly reported as trashed: {}",
                r.detail
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn real_file(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, b"video payload").expect("write real file");
    path.to_string_lossy().replace('\\', "/")
}

#[tokio::test]
async fn undo_restores_a_trashed_member_when_the_file_is_back_on_disk() {
    let files_dir = tempfile::tempdir().expect("files dir");
    let p1 = real_file(files_dir.path(), "best.mp4");
    let p2 = real_file(files_dir.path(), "dupe1.mp4");
    let p3 = real_file(files_dir.path(), "dupe2.mp4");
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover, move |db| {
        let f1 = seed_rich(db, &p1, 1_000_000, 0xaa);
        let f2 = seed_rich(db, &p2, 400_000, 0xaa);
        let f3 = seed_rich(db, &p3, 300_000, 0xaa);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("group");
        repo.add_member(gid, f1).expect("f1");
        repo.add_member(gid, f2).expect("f2");
        repo.add_member(gid, f3).expect("f3");
        repo.set_best(gid, Some(f1), T0).expect("best");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let deleted = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(matches!(&deleted, Response::Delete(r) if r.ok));

    let undone = client
        .request(&Request::Action(Action::UndoLastDelete))
        .await
        .expect("undo");
    match undone {
        Response::Undo(r) => {
            assert!(r.ok, "undo accepted: {}", r.detail);
            assert_eq!(r.group_id, Some(1));
            assert_eq!(r.restored_file_ids, vec![2]);
            assert!(r.missing_paths.is_empty());
        }
        other => panic!("expected Undo, got {other:?}"),
    }

    let members = fetch_group_detail(&mut client, 1).await;
    {
        let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
        assert_eq!(ids, vec![1, 2, 3], "f2 restored into the group");
    }

    let again = client
        .request(&Request::Action(Action::UndoLastDelete))
        .await
        .expect("undo again");
    assert!(
        matches!(again, Response::Undo(ref r) if !r.ok && r.missing_paths.is_empty()),
        "the consumed journal leaves nothing to undo"
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn undo_recreates_a_group_the_delete_batch_dropped() {
    let files_dir = tempfile::tempdir().expect("files dir");
    let p1 = real_file(files_dir.path(), "keep.mp4");
    let p2 = real_file(files_dir.path(), "gone.mp4");
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover, move |db| {
        let f1 = seed_rich(db, &p1, 1_000, 0xcc);
        let f2 = seed_rich(db, &p2, 500, 0xcc);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("group");
        repo.add_member(gid, f1).expect("f1");
        repo.add_member(gid, f2).expect("f2");
        repo.set_best(gid, Some(f1), T0).expect("best");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let deleted = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(matches!(&deleted, Response::Delete(r) if r.ok));
    let stats = client
        .request(&Request::GroupStats { trust: None })
        .await
        .expect("stats");
    assert!(matches!(stats, Response::GroupStats(s) if s.group_count == 0));

    let undone = client
        .request(&Request::Action(Action::UndoLastDelete))
        .await
        .expect("undo");
    assert!(
        matches!(&undone, Response::Undo(r) if r.ok && r.group_id == Some(1)),
        "undo recreates the dropped group: {undone:?}"
    );
    let members = fetch_group_detail(&mut client, 1).await;
    {
        let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
        assert_eq!(ids, vec![1, 2], "both members back in the restored group");
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
#[allow(clippy::used_underscore_binding)]
async fn undo_recreates_a_non_transitive_group_with_the_flag_still_set() {
    let files_dir = tempfile::tempdir().expect("files dir");
    let p1 = real_file(files_dir.path(), "keep.mp4");
    let p2 = real_file(files_dir.path(), "gone.mp4");
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover, move |db| {
        let f1 = seed_rich(db, &p1, 1_000, 0xcc);
        let f2 = seed_rich(db, &p2, 500, 0xcc);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo
            .create_non_transitive(DbTrust::VeryLikely, T0)
            .expect("non-transitive group");
        repo.add_member(gid, f1).expect("f1");
        repo.add_member(gid, f2).expect("f2");
        repo.set_best(gid, Some(f1), T0).expect("best");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let deleted = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(matches!(&deleted, Response::Delete(r) if r.ok));

    let undone = client
        .request(&Request::Action(Action::UndoLastDelete))
        .await
        .expect("undo");
    assert!(
        matches!(&undone, Response::Undo(r) if r.ok && r.group_id == Some(1)),
        "undo recreates the dropped group: {undone:?}"
    );

    let db2 = vidcull_db::open_file(&h._dir.path().join("vidcull.db")).expect("reopen db");
    let restored = DuplicateGroupsRepo::new(db2.conn())
        .get(1)
        .expect("get")
        .expect("group restored by undo");
    assert!(
        restored.non_transitive,
        "restored group must keep non_transitive=1 after undo, not revert to transitive"
    );

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn undo_refuses_while_the_files_are_still_gone_and_keeps_the_journal() {
    let remover = Arc::new(RecordingRemover::default());
    let h = spawn_seeded_with(remover, seed_trio);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let deleted = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(matches!(&deleted, Response::Delete(r) if r.ok));

    for attempt in 0..2 {
        let undone = client
            .request(&Request::Action(Action::UndoLastDelete))
            .await
            .expect("undo");
        match undone {
            Response::Undo(r) => {
                assert!(!r.ok, "no on-disk file → refuse (attempt {attempt})");
                assert_eq!(
                    r.missing_paths,
                    vec!["/lib/dupe1.mp4".to_owned()],
                    "the still-missing file is named (attempt {attempt})"
                );
                assert!(r.restored_file_ids.is_empty());
            }
            other => panic!("expected Undo, got {other:?}"),
        }
    }

    let members = fetch_group_detail(&mut client, 1).await;
    assert_eq!(
        members.len(),
        2,
        "a refused undo leaves the post-delete state untouched"
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn undo_with_an_empty_journal_reports_nothing_to_undo() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let undone = client
        .request(&Request::Action(Action::UndoLastDelete))
        .await
        .expect("undo");
    match undone {
        Response::Undo(r) => {
            assert!(!r.ok);
            assert!(r.restored_file_ids.is_empty());
            assert!(r.missing_paths.is_empty());
            assert!(
                r.detail.contains("되돌릴"),
                "empty-journal message names the cause: {}",
                r.detail
            );
        }
        other => panic!("expected Undo, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn partial_overlaps_is_empty_for_a_non_partial_group() {
    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::PartialOverlaps { group_id: 1 })
        .await
        .expect("overlaps");
    assert_eq!(resp, Response::PartialOverlaps(vec![]));
    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn rt_splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn rt_source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
    let mut state = seed;
    let scenes = (0..n)
        .map(|i| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: rt_splitmix64(&mut state) | 1,
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn rt_clip_of(source: &Tier2Fingerprint, start: usize, len: usize) -> Tier2Fingerprint {
    let scenes = source.scenes[start..start + len]
        .iter()
        .enumerate()
        .map(|(i, s)| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: s.phash ^ 0b1111,
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn seed_partial_fp(db: &Database, path: &str, fp: &Tier2Fingerprint) -> FileId {
    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: fp.scenes.first().map_or(0, |s| s.phash),
    };
    let tier2_blob = fp_format::encode_tier2(fp).expect("encode tier2");
    let tier1_blob = fp_format::encode_tier1(&t1).expect("encode tier1");
    let id = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(path),
            size_bytes: 1024,
            mtime_ns: 1,
            inode: None,
            content_hash: None,
            codec: Some(Codec::H264),
            container: None,
            duration: None,
            fps_x1000: None,
            bitrate_bps: None,
            resolution: None,
            first_seen_at: T0,
            last_seen_at: T0,
            ..Default::default()
        })
        .expect("insert file");
    let fps = FingerprintsRepo::new(db.conn());
    fps.upsert(&Fingerprint {
        file_id: id,
        tier1_global: tier1_blob,
        tier2_temporal: Some(tier2_blob.clone()),
        format_version: u32::from(fp_format::FORMAT_VERSION),
        created_at: T0,
    })
    .expect("upsert fp");
    fps.set_partial(id, &tier2_blob).expect("set_partial");
    id
}

#[tokio::test]
async fn partial_overlaps_reads_persisted_span_without_recompute() {
    let h = spawn_seeded(|db| {
        let source = seed_file(db, "/lib/src.mp4", 0x10);
        let clip = seed_file(db, "/lib/clip.mp4", 0x11);
        let groups = DuplicateGroupsRepo::new(db.conn());
        let gid = groups
            .create(DbTrust::Possible, T0)
            .expect("create possible group");
        groups.add_member(gid, source).expect("add source");
        groups.add_member(gid, clip).expect("add clip");
        let pmih = PartialMihRepo::new(db.conn());
        pmih.set_scene_count(source, 40)
            .expect("source scene count");
        pmih.set_scene_count(clip, 6).expect("clip scene count");
        SimilarityEdgesRepo::new(db.conn())
            .insert(&SimilarityEdge {
                group_id: gid,
                file_a: clip,
                file_b: source,
                score_x1000: 600,
                partial_span: Some(PartialEdgeSpan {
                    clip_start_ms: 0,
                    clip_end_ms: 5_000,
                    source_start_ms: 10_000,
                    source_end_ms: 15_000,
                    matched_scenes: 6,
                    clip_scenes: 6,
                }),
                intro_outro: false,
            })
            .expect("insert spanned edge");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::PartialOverlaps { group_id: 1 })
        .await
        .expect("overlaps");
    assert_eq!(
        resp,
        Response::PartialOverlaps(vec![ClipOverlap {
            clip_file_id: 2,
            source_file_id: 1,
            matched_scenes: 6,
            clip_scenes: 6,
            start_ms: 10_000,
            end_ms: 15_000,
            clip_start_ms: 0,
            clip_end_ms: 5_000,
            intro_outro: false,
        }]),
        "persisted span must be read back with correct orientation, no recompute",
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn partial_overlaps_legacy_null_span_recomputes_full_corpus() {
    let h = spawn_seeded(|db| {
        let source = rt_source_seq(0xABCD_1234_5678_9F01, 40);
        let clip = rt_clip_of(&source, 10, 6);
        let source_id = seed_partial_fp(db, "/lib/legacy_src.mp4", &source);
        let clip_id = seed_partial_fp(db, "/lib/legacy_clip.mp4", &clip);
        let groups = DuplicateGroupsRepo::new(db.conn());
        let gid = groups
            .create(DbTrust::Possible, T0)
            .expect("create possible group");
        groups.add_member(gid, source_id).expect("add source");
        groups.add_member(gid, clip_id).expect("add clip");
        SimilarityEdgesRepo::new(db.conn())
            .insert(&SimilarityEdge {
                group_id: gid,
                file_a: clip_id,
                file_b: source_id,
                score_x1000: 600,
                partial_span: None,
                intro_outro: false,
            })
            .expect("insert legacy edge");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::PartialOverlaps { group_id: 1 })
        .await
        .expect("overlaps");
    match resp {
        Response::PartialOverlaps(overlaps) => {
            assert_eq!(
                overlaps.len(),
                1,
                "legacy fallback recompute finds the overlap"
            );
            assert_eq!(overlaps[0].clip_file_id, 2, "clip is the shorter video");
            assert_eq!(overlaps[0].source_file_id, 1, "source is the longer video");
            assert!(
                overlaps[0].clip_scenes > 0,
                "recompute produced a real alignment"
            );
        }
        other => panic!("expected PartialOverlaps, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn legacy_null_span_stays_null_across_durable_rebuild_and_recomputes() {
    let (h, group_id) = spawn_seeded_mut(|db| {
        let source = rt_source_seq(0xABCD_1234_5678_9F01, 40);
        let clip = rt_clip_of(&source, 10, 6);
        seed_partial_fp(db, "/lib/legacy_src.mp4", &source);
        seed_partial_fp(db, "/lib/legacy_clip.mp4", &clip);

        let params = partial_clip_params();
        let empty = std::collections::BTreeSet::new();

        let mut index = PartialClipIndex::new_with_source(params, BlobSource::Partial);
        rebuild_partial_clip_groups_durable(&mut index, db, T0, &empty)
            .expect("cold build groups the pair + sets the reconciled marker");

        db.conn()
            .execute(
                "UPDATE similarity_edges SET clip_start_ms = NULL, clip_end_ms = NULL, \
                 source_start_ms = NULL, source_end_ms = NULL, matched_scenes = NULL, \
                 clip_scenes = NULL",
                [],
            )
            .expect("null out the span (legacy pre-V013 row)");

        let mut restarted = PartialClipIndex::new_with_source(params, BlobSource::Partial);
        rebuild_partial_clip_groups_durable(&mut restarted, db, T0 + 1, &empty)
            .expect("restart reloads + re-persists the legacy edge");

        let edges = SimilarityEdgesRepo::new(db.conn())
            .list_by_trust(DbTrust::Possible)
            .expect("list possible edges");
        assert_eq!(edges.len(), 1, "exactly one carried-forward POSSIBLE edge");
        assert!(
            edges[0].partial_span.is_none(),
            "legacy NULL span must stay NULL across the rebuild, not become Some(zeros)",
        );
        edges[0].group_id
    });

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::PartialOverlaps { group_id })
        .await
        .expect("overlaps");
    match resp {
        Response::PartialOverlaps(overlaps) => {
            assert_eq!(
                overlaps.len(),
                1,
                "recompute fallback finds the legacy overlap"
            );
            assert_eq!(overlaps[0].clip_file_id, 2, "clip is the shorter video");
            assert_eq!(overlaps[0].source_file_id, 1, "source is the longer video");
            assert!(
                overlaps[0].clip_scenes > 0,
                "recompute produced a real alignment, not a degenerate zero span",
            );
        }
        other => panic!("expected PartialOverlaps, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn seed_merged_clusters(db: &Database) {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let f1 = seed_rich(db, "/lib/a.mp4", 1_000_000, 0x01);
    let f2 = seed_rich(db, "/lib/b.mp4", 400_000, 0x01);
    let f3 = seed_rich(db, "/lib/c.mp4", 300_000, 0x02);
    let g1 = repo.create(DbTrust::Exact, T0).expect("g1");
    repo.add_member(g1, f1).expect("f1");
    repo.add_member(g1, f2).expect("f2");
    repo.set_best(g1, Some(f1), T0).expect("best g1");
    let g2 = repo.create(DbTrust::VeryLikely, T0).expect("g2");
    repo.add_member(g2, f2).expect("f2 in g2");
    repo.add_member(g2, f3).expect("f3");
    repo.set_best(g2, Some(f3), T0).expect("best g2");

    let f4 = seed_rich(db, "/lib/d.mp4", 500, 0x03);
    let f5 = seed_rich(db, "/lib/e.mp4", 200, 0x03);
    let g3 = repo.create(DbTrust::Exact, T0).expect("g3");
    repo.add_member(g3, f4).expect("f4");
    repo.add_member(g3, f5).expect("f5");
    repo.set_best(g3, Some(f4), T0).expect("best g3");
}

#[tokio::test]
async fn cluster_summaries_merge_trust_levels_and_keep_disjoint_separate() {
    let h = spawn_seeded(seed_merged_clusters);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("clusters");
    match resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 2, "merged {{1,2,3}} + disjoint {{4,5}}");
            let merged = &clusters[0];
            assert_eq!(merged.cluster_id, 1);
            assert_eq!(merged.member_count, 3, "f2 shared → one cluster of 3");
            assert_eq!(merged.representative_trust, TrustLevel::Exact);
            assert_eq!(
                merged.member_trust_levels,
                vec![TrustLevel::Exact, TrustLevel::VeryLikely],
                "card shows EXACT and VERY_LIKELY badges simultaneously"
            );
            assert_eq!(merged.best_file_id, Some(1), "representative group's best");

            let disjoint = &clusters[1];
            assert_eq!(disjoint.cluster_id, 3);
            assert_eq!(disjoint.member_count, 2);
            assert_eq!(disjoint.member_trust_levels, vec![TrustLevel::Exact]);
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    let very_likely = client
        .request(&Request::ClusterSummaries {
            trust: Some(TrustLevel::VeryLikely),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("filter");
    assert_eq!(very_likely, Response::ClusterSummaries(vec![]));

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn cluster_detail_projects_per_member_trust_and_routing_group() {
    let h = spawn_seeded(seed_merged_clusters);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let members = fetch_cluster_detail(&mut client, 1).await;
    {
        assert_eq!(members.len(), 3, "all three cluster members");
        let by_id = |id: i64| members.iter().find(|m| m.file.file_id == id).unwrap();

        let m1 = by_id(1);
        assert_eq!(m1.trust, TrustLevel::Exact);
        assert!(m1.file.is_best, "cluster best is f1");
        assert_eq!(m1.group_id, 1, "routed through the EXACT group g1");

        let m2 = by_id(2);
        assert_eq!(m2.trust, TrustLevel::Exact, "strongest whole-file trust");
        assert!(!m2.file.is_best);
        assert_eq!(m2.group_id, 1);

        let m3 = by_id(3);
        assert_eq!(m3.trust, TrustLevel::VeryLikely);
        assert_eq!(m3.group_id, 2, "routed through the VERY_LIKELY group g2");
    }

    let unknown = fetch_cluster_detail(&mut client, 9_999).await;
    assert!(
        unknown.is_empty(),
        "unknown cluster_id returns an empty list"
    );

    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn prime_thumb_cache(cache_dir: &std::path::Path, hash: Blake3Hash) {
    let pixels = vec![128u8; 32 * 18];
    let jpeg = vidcull_thumb::encode_thumbnail(
        vidcull_thumb::GrayView {
            width: 32,
            height: 18,
            pixels: &pixels,
        },
        vidcull_thumb::ThumbnailOptions::default(),
    )
    .expect("encode fixture jpeg");
    std::fs::write(cache_dir.join(format!("{}_0_v2.jpg", hash.to_hex())), &jpeg)
        .expect("seed thumbnail cache");
}

#[tokio::test]
async fn cluster_detail_omits_thumbnails_for_fast_first_paint() {
    let cache = tempfile::tempdir().expect("cache dir");
    prime_thumb_cache(cache.path(), Blake3Hash::from_bytes([0x01; HASH_LEN]));

    let provider = Arc::new(ThumbnailProvider::new(cache.path().to_path_buf(), None));
    let h = spawn_seeded_with_thumbnails(provider, seed_merged_clusters);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let members = fetch_cluster_detail(&mut client, 1).await;
    assert_eq!(members.len(), 3, "all three cluster members still render");
    for m in &members {
        assert!(
            m.file.thumbnail.is_none(),
            "cluster_detail must not carry a thumbnail; file {} had one",
            m.file.file_id
        );
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn thumbnail_rpc_serves_a_cached_member_preview_lazily() {
    let cache = tempfile::tempdir().expect("cache dir");
    prime_thumb_cache(cache.path(), Blake3Hash::from_bytes([0x01; HASH_LEN]));

    let provider = Arc::new(ThumbnailProvider::new(cache.path().to_path_buf(), None));
    let h = spawn_seeded_with_thumbnails(provider, seed_merged_clusters);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    match client
        .request(&Request::Thumbnail { file_id: 1 })
        .await
        .expect("thumbnail rpc")
    {
        Response::Thumbnail(Some(uri)) => assert!(
            uri.starts_with("data:image/jpeg;base64,"),
            "expected a JPEG data URI, got {uri}"
        ),
        other => panic!("expected a cached preview, got {other:?}"),
    }

    match client
        .request(&Request::Thumbnail { file_id: 9_999 })
        .await
        .expect("thumbnail rpc")
    {
        Response::Thumbnail(None) => {}
        other => panic!("unknown file id must yield Thumbnail(None), got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn group_detail_chunks_when_single_frame_would_exceed_max_frame_len() {
    use vidcull_ipc::protocol::MAX_FRAME_LEN;

    const MEMBER_COUNT: usize = 600;
    const THUMB_RAW_BYTES: usize = 28_000;

    let cache_dir = tempfile::tempdir().expect("cache dir");
    let shared_hash = Blake3Hash::from_bytes([0xeeu8; HASH_LEN]);
    let hash_hex = shared_hash.to_hex();
    let fake_thumb = vec![0xffu8; THUMB_RAW_BYTES];
    std::fs::write(
        cache_dir.path().join(format!("{hash_hex}_0_v2.jpg")),
        &fake_thumb,
    )
    .expect("seed thumbnail cache");

    let provider = Arc::new(ThumbnailProvider::new(cache_dir.path().to_path_buf(), None));
    let h = spawn_seeded_with_thumbnails(provider, |db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("create group");
        for i in 0..MEMBER_COUNT {
            let fid = seed_file(db, &format!("/lib/large/f{i:04}.mp4"), 0xee);
            repo.add_member(gid, fid).expect("add member");
        }
    });

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let chunks = client
        .request_stream(&Request::GroupDetail { group_id: 1 })
        .await
        .expect("request_stream");

    let mut chunk_count = 0usize;
    let mut members: Vec<FileDetail> = Vec::new();
    for chunk in chunks {
        match chunk {
            Response::GroupDetail(m) => {
                chunk_count += 1;
                members.extend(m);
            }
            other => panic!("unexpected frame in GroupDetail stream: {other:?}"),
        }
    }

    assert_eq!(
        members.len(),
        MEMBER_COUNT,
        "all {MEMBER_COUNT} members returned after chunked reassembly"
    );
    assert!(
        chunk_count > 1,
        "large group must be split into >1 frames (got {chunk_count})"
    );

    let full_encoded_len = vidcull_core::encode(&Response::GroupDetail(members))
        .expect("encode full payload")
        .len();
    assert!(
        full_encoded_len > MAX_FRAME_LEN as usize,
        "red-baseline violated: full payload {full_encoded_len} B must exceed \
         MAX_FRAME_LEN {MAX_FRAME_LEN} B — increase MEMBER_COUNT or THUMB_RAW_BYTES"
    );

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn cluster_stats_counts_clusters_and_reclaimable_bytes() {
    let h = spawn_seeded(seed_merged_clusters);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let stats = client
        .request(&Request::ClusterStats { trust: None })
        .await
        .expect("stats");
    assert_eq!(
        stats,
        Response::ClusterStats(vidcull_ipc::ClusterStats {
            cluster_count: 2,
            reclaimable_bytes: 700_200,
        })
    );

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn cluster_summaries_keep_possible_clips_as_their_own_cluster() {
    let h = spawn_seeded(|db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let f1 = seed_rich(db, "/lib/whole-a.mp4", 1_000, 0x11);
        let f2 = seed_rich(db, "/lib/whole-b.mp4", 800, 0x11);
        let g1 = repo.create(DbTrust::Exact, T0).expect("g1");
        repo.add_member(g1, f1).expect("f1");
        repo.add_member(g1, f2).expect("f2");
        let f3 = seed_rich(db, "/lib/source.mp4", 5_000, 0x22);
        let f4 = seed_rich(db, "/lib/clip.mp4", 900, 0x33);
        let g2 = repo.create(DbTrust::Possible, T0).expect("g2");
        repo.add_member(g2, f3).expect("f3");
        repo.add_member(g2, f4).expect("f4");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::ClusterSummaries {
            trust: Some(TrustLevel::Possible),
            limit: 10,
            offset: 0,
        })
        .await
        .expect("possible clusters");
    match resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 1, "the POSSIBLE group is its own cluster");
            assert_eq!(clusters[0].representative_trust, TrustLevel::Possible);
            assert_eq!(clusters[0].member_trust_levels, vec![TrustLevel::Possible]);
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn seed_colliding_clusters(db: &Database) {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let c1 = seed_rich(db, "/lib/copy1.mp4", 1_000_000, 0x01);
    let c2 = seed_rich(db, "/lib/copy2.mp4", 1_000_000, 0x01);
    let c3 = seed_rich(db, "/lib/copy3.mp4", 1_000_000, 0x01);
    let g_exact = repo.create(DbTrust::Exact, T0).expect("g_exact");
    repo.add_member(g_exact, c1).expect("c1");
    repo.add_member(g_exact, c2).expect("c2");
    repo.add_member(g_exact, c3).expect("c3");
    repo.set_best(g_exact, Some(c1), T0).expect("best");
    let clip = seed_rich(db, "/lib/clip.mp4", 300_000, 0x02);
    for copy in [c1, c2, c3] {
        let gp = repo.create(DbTrust::Possible, T0).expect("possible pair");
        repo.add_member(gp, clip).expect("clip");
        repo.add_member(gp, copy).expect("copy");
    }
}

#[tokio::test]
async fn cluster_ids_disambiguate_possible_fanout_from_transitive_sharing_a_member() {
    let h = spawn_seeded(seed_colliding_clusters);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("clusters");
    let (transitive_id, possible_id) = match resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(
                clusters.len(),
                2,
                "transitive {{1,2,3}} + POSSIBLE fan-out {{1,2,3,4}}"
            );
            let transitive = clusters
                .iter()
                .find(|c| c.representative_trust == TrustLevel::Exact)
                .expect("transitive cluster");
            let possible = clusters
                .iter()
                .find(|c| c.representative_trust == TrustLevel::Possible)
                .expect("POSSIBLE fan-out cluster");
            assert_ne!(
                transitive.cluster_id, possible.cluster_id,
                "the two clusters share a smallest member id but must NOT collide \
                 on one cluster id"
            );
            assert_eq!(transitive.member_count, 3, "the three exact copies");
            assert_eq!(
                possible.member_count, 4,
                "fan-out card = clip + all three matched copies"
            );
            (transitive.cluster_id, possible.cluster_id)
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    };

    let possible_members = fetch_cluster_detail(&mut client, possible_id).await;
    let mut ids: Vec<i64> = possible_members.iter().map(|m| m.file.file_id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![1, 2, 3, 4],
        "cluster_detail(possible_id) returns the fan-out incl. the clip"
    );

    let transitive_members = fetch_cluster_detail(&mut client, transitive_id).await;
    let mut t_ids: Vec<i64> = transitive_members.iter().map(|m| m.file.file_id).collect();
    t_ids.sort_unstable();
    assert_eq!(t_ids, vec![1, 2, 3], "transitive detail excludes the clip");

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn settings_get_set_round_trip_over_real_socket() {
    use vidcull_ipc::DaemonSettings;

    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let live_cores = vidcull_daemon::settings::live_cores();
    let initial = client
        .request(&Request::GetSettings)
        .await
        .expect("get settings");
    assert_eq!(
        initial,
        Response::Settings(DaemonSettings {
            cpu_cores: live_cores,
            ..DaemonSettings::default()
        })
    );

    let want = DaemonSettings {
        scan_folders: vec!["C:/clips".to_owned(), "D:/raw".to_owned()],
        background_enabled: true,
        auto_index: false,
        exclude_rules: vec![".trash".to_owned()],
        run_on_boot: true,
        cpu_throttle: vidcull_ipc::CpuThrottle::Eco,
        best_copy_mode: vidcull_ipc::BestCopyMode::SpaceSaving,
        idle_worker_count: Some(2),
        cpu_cores: live_cores,
        partial_clips_enabled: true,
        indexing_enabled: true,
    };
    let stored = client
        .request(&Request::Action(Action::SetSettings(want.clone())))
        .await
        .expect("set settings");
    assert_eq!(stored, Response::Settings(want.clone()));

    let reread = client
        .request(&Request::GetSettings)
        .await
        .expect("re-read settings");
    assert_eq!(reread, Response::Settings(want));

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn set_settings_persist_failure_surfaces_as_error_not_false_echo() {
    use vidcull_ipc::DaemonSettings;

    let h = spawn_bridge();
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let bad = DaemonSettings {
        run_on_boot: true,
        scan_folders: vec!["relative/path".to_owned()],
        ..DaemonSettings::default()
    };
    let resp = client
        .request(&Request::Action(Action::SetSettings(bad)))
        .await
        .expect("set settings");
    match resp {
        Response::Error(err) => {
            assert!(!err.message.is_empty(), "error reply should carry a reason");
        }
        other => panic!(
            "expected Response::Error on a persist failure, got {other:?} \
             (false echo of the unchanged persisted value)"
        ),
    }

    let reread = client
        .request(&Request::GetSettings)
        .await
        .expect("re-read settings");
    match reread {
        Response::Settings(settings) => assert!(
            !settings.run_on_boot,
            "a rejected write must not persist any part of its payload"
        ),
        other => panic!("expected Response::Settings, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn changing_best_copy_mode_repicks_best_without_reindex() {
    use vidcull_ipc::{BestCopyMode, DaemonSettings};

    let h = spawn_seeded(|db| {
        let big = seed_rich(db, "/lib/big.mp4", 100_000_000, 0xaa);
        let small = seed_rich(db, "/lib/small.mp4", 10_000_000, 0xaa);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("create group");
        repo.add_member(gid, big).expect("add big");
        repo.add_member(gid, small).expect("add small");
        repo.set_best(gid, Some(big), T0 + 1)
            .expect("seed best = big");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let settings = DaemonSettings {
        best_copy_mode: BestCopyMode::MinSize,
        ..DaemonSettings::default()
    };
    client
        .request(&Request::Action(Action::SetSettings(settings)))
        .await
        .expect("set settings");

    let resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("clusters");
    match resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 1, "one EXACT group → one cluster");
            assert_eq!(
                clusters[0].best_file_id,
                Some(2),
                "SetSettings(MinSize) must re-pick the smaller file (id 2) as best \
                 live, with no reindex"
            );
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn adding_a_scan_folder_enqueues_an_initial_scan() {
    use vidcull_ipc::DaemonSettings;

    let lib = tempfile::tempdir().expect("lib dir");
    std::fs::write(lib.path().join("clip.mp4"), b"not a real video").expect("write video");
    std::fs::write(lib.path().join("notes.txt"), b"ignored").expect("write txt");
    let folder = lib.path().to_string_lossy().replace('\\', "/");

    let h = spawn_seeded(|_db| {});
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let before = client.request(&Request::Progress).await.expect("progress");
    match before {
        Response::Progress(p) => {
            assert_eq!(p.pending, 0);
            assert_eq!(p.running, 0);
            assert_eq!(p.done, 0);
            assert_eq!(p.failed, 0);
            assert!(p.rss_bytes > 0);
        }
        other => panic!("expected Progress, got {other:?}"),
    }

    let settings = DaemonSettings {
        scan_folders: vec![folder],
        ..DaemonSettings::default()
    };
    let stored = client
        .request(&Request::Action(Action::SetSettings(settings.clone())))
        .await
        .expect("set settings");
    assert_eq!(
        stored,
        Response::Settings(DaemonSettings {
            cpu_cores: vidcull_daemon::settings::live_cores(),
            ..settings
        })
    );

    let after = client.request(&Request::Progress).await.expect("progress");
    match after {
        Response::Progress(p) => assert_eq!(
            p.pending, 1,
            "the one video file is queued for indexing; the .txt is filtered out",
        ),
        other => panic!("expected Progress, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn re_saving_settings_without_new_folders_enqueues_nothing() {
    use vidcull_ipc::DaemonSettings;

    let lib = tempfile::tempdir().expect("lib dir");
    std::fs::write(lib.path().join("clip.mp4"), b"x").expect("write video");
    let folder = lib.path().to_string_lossy().replace('\\', "/");

    let h = spawn_seeded(|_db| {});
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let settings = DaemonSettings {
        scan_folders: vec![folder],
        ..DaemonSettings::default()
    };
    client
        .request(&Request::Action(Action::SetSettings(settings.clone())))
        .await
        .expect("first save");
    let unchanged_folders = DaemonSettings {
        auto_index: false,
        ..settings
    };
    client
        .request(&Request::Action(Action::SetSettings(unchanged_folders)))
        .await
        .expect("second save");

    let after = client.request(&Request::Progress).await.expect("progress");
    match after {
        Response::Progress(p) => assert_eq!(
            p.pending, 1,
            "the folder was already known; re-saving must not re-enqueue it",
        ),
        other => panic!("expected Progress, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn removing_a_scan_folder_queues_removal_of_its_indexed_files() {
    use vidcull_ipc::DaemonSettings;

    let h = spawn_seeded(|db| {
        seed_file(db, "C:/lib/a.mp4", 0x11);
        seed_file(db, "C:/lib/b.mp4", 0x22);
        seed_file(db, "C:/keep/c.mp4", 0x33);
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let before = client.request(&Request::Progress).await.expect("progress");
    match before {
        Response::Progress(p) => {
            assert_eq!(p.done, 3, "three indexed files before any removal");
            assert_eq!(p.pending, 0, "nothing queued yet");
        }
        other => panic!("expected Progress, got {other:?}"),
    }

    let both = DaemonSettings {
        scan_folders: vec!["C:/lib".to_owned(), "C:/keep".to_owned()],
        ..DaemonSettings::default()
    };
    client
        .request(&Request::Action(Action::SetSettings(both.clone())))
        .await
        .expect("register folders");
    let registered = client.request(&Request::Progress).await.expect("progress");
    match registered {
        Response::Progress(p) => assert_eq!(p.pending, 0, "registering folders queues no work"),
        other => panic!("expected Progress, got {other:?}"),
    }

    let keep_only = DaemonSettings {
        scan_folders: vec!["C:/keep".to_owned()],
        ..both
    };
    client
        .request(&Request::Action(Action::SetSettings(keep_only)))
        .await
        .expect("remove a folder");

    let after = client.request(&Request::Progress).await.expect("progress");
    match after {
        Response::Progress(p) => assert_eq!(
            p.pending, 2,
            "the two files under the removed folder are queued for removal; \
             the kept folder's file is not",
        ),
        other => panic!("expected Progress, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn removing_a_scan_folder_clears_its_failed_tasks() {
    use vidcull_ipc::DaemonSettings;

    let h = spawn_seeded(|db| {
        enqueue_failed(db, "scan", "C:/lib/broken.mp4", "decode error");
        enqueue_failed(db, "scan", "C:/keep/broken.mp4", "decode error");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let before = client.request(&Request::Progress).await.expect("progress");
    match before {
        Response::Progress(p) => assert_eq!(p.failed, 2, "two failed tasks to start"),
        other => panic!("expected Progress, got {other:?}"),
    }

    let both = DaemonSettings {
        scan_folders: vec!["C:/lib".to_owned(), "C:/keep".to_owned()],
        ..DaemonSettings::default()
    };
    client
        .request(&Request::Action(Action::SetSettings(both.clone())))
        .await
        .expect("register folders");
    let keep_only = DaemonSettings {
        scan_folders: vec!["C:/keep".to_owned()],
        ..both
    };
    client
        .request(&Request::Action(Action::SetSettings(keep_only)))
        .await
        .expect("remove a folder");

    let after = client.request(&Request::Progress).await.expect("progress");
    match after {
        Response::Progress(p) => assert_eq!(
            p.failed, 1,
            "only the removed folder's failed task is cleared",
        ),
        other => panic!("expected Progress, got {other:?}"),
    }
    match client
        .request(&Request::FailedTasks { limit: 10 })
        .await
        .expect("failed tasks")
    {
        Response::FailedTasks(tasks) => {
            assert_eq!(tasks.len(), 1, "one failure remains");
            assert_eq!(
                tasks[0].path, "C:/keep/broken.mp4",
                "the kept folder's file"
            );
        }
        other => panic!("expected FailedTasks, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[test]
fn setting_cpu_throttle_updates_the_live_throttle_control() {
    use std::time::Duration;
    use vidcull_daemon::ThrottleControl;
    use vidcull_ipc::{CpuThrottle, DaemonSettings, RequestHandler};

    let control = Arc::new(ThrottleControl::default());
    let db = Arc::new(Mutex::new(
        vidcull_db::open_in_memory().expect("in-memory db"),
    ));
    let handler = DaemonRequestHandler::new(
        db,
        ShutdownToken::new(),
        LogBuffer::default(),
        "scan".to_owned(),
        Arc::new(RecordingRemover::default()),
    )
    .with_throttle_control(Arc::clone(&control));

    assert!(!control.is_max_performance());
    assert_eq!(control.effective_cooldown(Duration::ZERO), Duration::ZERO);

    let eco = DaemonSettings {
        cpu_throttle: CpuThrottle::Eco,
        ..DaemonSettings::default()
    };
    let _ = handler.handle(Request::Action(Action::SetSettings(eco)));
    let eco_floor = vidcull_daemon::cpu_throttle_cooldown(CpuThrottle::Eco);
    assert!(eco_floor > Duration::ZERO);
    assert!(!control.is_max_performance(), "Eco is not max performance");
    assert_eq!(
        control.effective_cooldown(Duration::ZERO),
        eco_floor,
        "Eco paces between tasks even on an idle machine",
    );

    let _ = handler.handle(Request::Action(Action::SetSettings(
        DaemonSettings::default(),
    )));
    assert!(control.is_max_performance(), "Full enables max performance");
    assert_eq!(
        control.effective_cooldown(Duration::from_millis(25)),
        Duration::ZERO,
        "max performance drops even the activity cooldown to zero",
    );
}

#[test]
fn setting_idle_worker_count_updates_the_live_throttle_control() {
    use vidcull_daemon::ThrottleControl;
    use vidcull_ipc::{DaemonSettings, RequestHandler};

    let control = Arc::new(ThrottleControl::default());
    let db = Arc::new(Mutex::new(
        vidcull_db::open_in_memory().expect("in-memory db"),
    ));
    let handler = DaemonRequestHandler::new(
        db,
        ShutdownToken::new(),
        LogBuffer::default(),
        "scan".to_owned(),
        Arc::new(RecordingRemover::default()),
    )
    .with_throttle_control(Arc::clone(&control));

    assert_eq!(control.idle_workers_override(), None);

    let two = DaemonSettings {
        idle_worker_count: Some(2),
        ..DaemonSettings::default()
    };
    let _ = handler.handle(Request::Action(Action::SetSettings(two)));
    assert_eq!(control.idle_workers_override(), Some(2));

    let cores = usize::try_from(vidcull_daemon::settings::live_cores())
        .unwrap_or(usize::MAX)
        .max(1);
    let huge = DaemonSettings {
        idle_worker_count: Some(u32::MAX),
        ..DaemonSettings::default()
    };
    let _ = handler.handle(Request::Action(Action::SetSettings(huge)));
    assert_eq!(control.idle_workers_override(), Some(cores));

    let auto = DaemonSettings {
        idle_worker_count: None,
        ..DaemonSettings::default()
    };
    let _ = handler.handle(Request::Action(Action::SetSettings(auto)));
    assert_eq!(control.idle_workers_override(), None);
}

fn seed_cross_group_conflict(db: &Database) -> (i64, i64, FileId) {
    let shared = seed_file(db, "/lib/shared.mp4", 0x11);
    let b = seed_file(db, "/lib/exact_other.mp4", 0x11);
    let c = seed_file(db, "/lib/clip.mp4", 0x22);
    let repo = DuplicateGroupsRepo::new(db.conn());
    let exact = repo.create(DbTrust::Exact, T0).expect("create exact");
    repo.add_member(exact, shared).expect("exact + shared");
    repo.add_member(exact, b).expect("exact + b");
    repo.set_best(exact, Some(shared), T0 + 1)
        .expect("set best");
    let possible = repo.create(DbTrust::Possible, T0).expect("create possible");
    repo.add_member(possible, shared)
        .expect("possible + shared");
    repo.add_member(possible, c).expect("possible + c");
    (exact, possible, shared)
}

#[tokio::test]
async fn cross_group_conflicts_surfaces_a_file_kept_here_but_a_candidate_elsewhere() {
    let mut shared_id = FileId(0);
    let mut exact_gid = 0;
    let mut possible_gid = 0;
    let h = spawn_seeded(|db| {
        let (exact, possible, shared) = seed_cross_group_conflict(db);
        exact_gid = exact;
        possible_gid = possible;
        shared_id = shared;
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    match client
        .request(&Request::CrossGroupConflicts {
            group_id: exact_gid,
        })
        .await
        .expect("conflicts")
    {
        Response::CrossGroupConflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1, "only `shared` is conflicted");
            let conflict = &conflicts[0];
            assert_eq!(conflict.file_id, shared_id.0);
            assert_eq!(conflict.path, "/lib/shared.mp4");
            assert_eq!(conflict.memberships.len(), 2);
            let exact_role = conflict
                .memberships
                .iter()
                .find(|r| r.group_id == exact_gid)
                .expect("exact role");
            let possible_role = conflict
                .memberships
                .iter()
                .find(|r| r.group_id == possible_gid)
                .expect("possible role");
            assert!(exact_role.is_best, "kept in the EXACT group");
            assert_eq!(exact_role.trust, TrustLevel::Exact);
            assert!(
                !possible_role.is_best,
                "deletion candidate in the POSSIBLE group",
            );
            assert_eq!(possible_role.trust, TrustLevel::Possible);
        }
        other => panic!("expected CrossGroupConflicts, got {other:?}"),
    }

    match client
        .request(&Request::CrossGroupConflicts {
            group_id: possible_gid,
        })
        .await
        .expect("conflicts")
    {
        Response::CrossGroupConflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].file_id, shared_id.0);
        }
        other => panic!("expected CrossGroupConflicts, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn cross_group_conflicts_is_empty_when_no_member_spans_groups() {
    let mut gid = 0;
    let h = spawn_seeded(|db| {
        let f1 = seed_file(db, "/lib/a.mp4", 0xaa);
        let f2 = seed_file(db, "/lib/b.mp4", 0xaa);
        let repo = DuplicateGroupsRepo::new(db.conn());
        gid = repo.create(DbTrust::Exact, T0).expect("create");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
        repo.set_best(gid, Some(f1), T0 + 1).expect("set best");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    match client
        .request(&Request::CrossGroupConflicts { group_id: gid })
        .await
        .expect("conflicts")
    {
        Response::CrossGroupConflicts(conflicts) => assert!(conflicts.is_empty()),
        other => panic!("expected CrossGroupConflicts, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn cross_group_conflicts_ignores_benign_multi_membership() {
    let mut gid_a = 0;
    let h = spawn_seeded(|db| {
        let shared = seed_file(db, "/lib/shared.mp4", 0x33);
        let x = seed_file(db, "/lib/x.mp4", 0x33);
        let y = seed_file(db, "/lib/y.mp4", 0x44);
        let repo = DuplicateGroupsRepo::new(db.conn());
        gid_a = repo.create(DbTrust::VeryLikely, T0).expect("create a");
        repo.add_member(gid_a, shared).expect("a + shared");
        repo.add_member(gid_a, x).expect("a + x");
        let gid_b = repo.create(DbTrust::Possible, T0).expect("create b");
        repo.add_member(gid_b, shared).expect("b + shared");
        repo.add_member(gid_b, y).expect("b + y");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    match client
        .request(&Request::CrossGroupConflicts { group_id: gid_a })
        .await
        .expect("conflicts")
    {
        Response::CrossGroupConflicts(conflicts) => assert!(
            conflicts.is_empty(),
            "candidate-in-both is not a conflict, got {conflicts:?}",
        ),
        other => panic!("expected CrossGroupConflicts, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn seed_real_group(db: &Database, specs: &[(&std::path::Path, i64, bool)]) {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo.create(DbTrust::Exact, T0).expect("create group");
    let mut best: Option<FileId> = None;
    for (path, size, is_best) in specs {
        let id = seed_rich(db, &path.to_string_lossy(), *size, 0xaa);
        repo.add_member(gid, id).expect("add member");
        if *is_best {
            best = Some(id);
        }
    }
    repo.set_best(gid, best, T0 + 1).expect("set best");
}

#[tokio::test]
async fn os_remover_permanently_deletes_a_real_file_and_updates_the_db() {
    let files = tempfile::tempdir().expect("files dir");
    let keep = files.path().join("best.mp4");
    let dupe1 = files.path().join("dupe1.mp4");
    let dupe2 = files.path().join("dupe2.mp4");
    for p in [&keep, &dupe1, &dupe2] {
        std::fs::write(p, b"payload").expect("write file");
    }

    let h = spawn_seeded_with(Arc::new(OsFileRemover), |db| {
        seed_real_group(
            db,
            &[
                (&keep, 1_000_000, true),
                (&dupe1, 400_000, false),
                (&dupe2, 300_000, false),
            ],
        );
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    match resp {
        Response::Delete(r) => {
            assert!(r.ok, "real delete succeeded: {}", r.detail);
            assert_eq!(r.removed_file_ids, vec![2]);
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    assert!(!dupe1.exists(), "the file is gone from disk");
    assert!(keep.exists() && dupe2.exists(), "the others are untouched");

    match client
        .request(&Request::GroupDetail { group_id: 1 })
        .await
        .expect("detail")
    {
        Response::GroupDetail(members) => {
            let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
            assert_eq!(ids, vec![1, 3], "removed member soft-deleted + un-grouped");
        }
        other => panic!("expected GroupDetail, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn os_remover_is_idempotent_when_the_file_already_vanished() {
    let files = tempfile::tempdir().expect("files dir");
    let keep = files.path().join("best.mp4");
    let vanished = files.path().join("gone.mp4");
    std::fs::write(&keep, b"payload").expect("write best");

    let h = spawn_seeded_with(Arc::new(OsFileRemover), |db| {
        seed_real_group(db, &[(&keep, 1_000, true), (&vanished, 500, false)]);
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    assert!(
        matches!(&resp, Response::Delete(r) if r.ok && r.removed_file_ids == vec![2]),
        "deleting an already-vanished file is idempotent success, got {resp:?}"
    );

    let stats = client
        .request(&Request::GroupStats { trust: None })
        .await
        .expect("stats");
    assert_eq!(
        stats,
        Response::GroupStats(vidcull_ipc::GroupStats {
            group_count: 0,
            reclaimable_bytes: 0,
        })
    );
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[cfg(windows)]
#[tokio::test]
async fn os_remover_continues_past_a_locked_file() {
    use std::os::windows::fs::OpenOptionsExt;

    let files = tempfile::tempdir().expect("files dir");
    let keep = files.path().join("best.mp4");
    let deletable = files.path().join("deletable.mp4");
    let locked = files.path().join("locked.mp4");
    for p in [&keep, &deletable, &locked] {
        std::fs::write(p, b"payload").expect("write file");
    }
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked)
        .expect("hold exclusive handle");

    let h = spawn_seeded_with(Arc::new(OsFileRemover), |db| {
        seed_real_group(
            db,
            &[
                (&keep, 1_000_000, true),
                (&deletable, 400_000, false),
                (&locked, 300_000, false),
            ],
        );
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let resp = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![2, 3],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    match resp {
        Response::Delete(r) => {
            assert!(r.ok, "the batch made progress: {}", r.detail);
            assert_eq!(r.removed_file_ids, vec![2], "only the unlocked file went");
            assert!(
                r.detail.contains("1개 실패"),
                "the locked file is reported as a failure: {}",
                r.detail
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    assert!(!deletable.exists(), "the unlocked file is gone");
    assert!(locked.exists(), "the locked file stays on disk");

    match client
        .request(&Request::GroupDetail { group_id: 1 })
        .await
        .expect("detail")
    {
        Response::GroupDetail(members) => {
            let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
            assert_eq!(ids, vec![1, 3], "best + locked remain; deletable dropped");
        }
        other => panic!("expected GroupDetail, got {other:?}"),
    }

    drop(handle);
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn toctou_guard_refuses_a_changed_file_and_leaves_it_on_disk() {
    let files = tempfile::tempdir().expect("files dir");
    let keep = files.path().join("best.mp4");
    let dupe = files.path().join("dupe.mp4");
    std::fs::write(&keep, b"payload").expect("write best");
    std::fs::write(&dupe, b"payload").expect("write dupe");

    let h = spawn_seeded_with(Arc::new(OsFileRemover), |db| {
        seed_real_group(db, &[(&keep, 7, true), (&dupe, 7, false)]);
    });

    std::fs::write(&dupe, b"a much larger payload than before").expect("grow dupe");

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    match resp {
        Response::Delete(r) => {
            assert!(!r.ok, "a changed-file batch removed nothing: {}", r.detail);
            assert!(r.removed_file_ids.is_empty(), "nothing was removed");
            assert!(
                r.detail.contains("변경됨"),
                "the changed file is reported, not silently deleted: {}",
                r.detail
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    assert!(dupe.exists(), "the changed file must remain on disk");
    assert!(keep.exists(), "the best copy is untouched");

    match client
        .request(&Request::GroupDetail { group_id: 1 })
        .await
        .expect("detail")
    {
        Response::GroupDetail(members) => {
            let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
            assert_eq!(ids, vec![1, 2], "both members survive a refused delete");
        }
        other => panic!("expected GroupDetail, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[cfg(windows)]
#[tokio::test]
async fn verbatim_path_is_refused_and_never_ghost_deleted() {
    let files = tempfile::tempdir().expect("files dir");
    let keep = files.path().join("best.mp4");
    std::fs::write(&keep, b"payload").expect("write best");
    let verbatim = std::path::PathBuf::from("//?/C:/never/remote-dupe.mp4");

    let h = spawn_seeded_with(Arc::new(OsFileRemover), |db| {
        seed_real_group(db, &[(&keep, 7, true), (&verbatim, 500, false)]);
    });

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    match resp {
        Response::Delete(r) => {
            assert!(
                !r.ok,
                "a refused verbatim batch removed nothing: {}",
                r.detail
            );
            assert!(r.removed_file_ids.is_empty(), "nothing was removed");
            assert!(
                r.detail.contains("지원하지 않는 경로"),
                "the verbatim path is reported as unsupported: {}",
                r.detail
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    assert!(keep.exists(), "the real best copy is untouched");

    match client
        .request(&Request::GroupDetail { group_id: 1 })
        .await
        .expect("detail")
    {
        Response::GroupDetail(members) => {
            let ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
            assert_eq!(
                ids,
                vec![1, 2],
                "the verbatim member stays grouped, not ghost-deleted"
            );
        }
        other => panic!("expected GroupDetail, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn trash_that_hard_deletes_is_reported_as_permanent() {
    let h = spawn_seeded_with(Arc::new(HardDeleteFallbackRemover), |db| {
        let f1 = seed_file(db, "/nas/best.mp4", 0xaa);
        let f2 = seed_file(db, "/nas/dupe.mp4", 0xaa);
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo.create(DbTrust::Exact, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
        repo.set_best(gid, Some(f1), T0 + 1).expect("set best");
    });

    let mut client = IpcClient::connect(&h.address).await.expect("connect");
    let resp = client
        .request(&Request::Action(Action::MoveToTrash(DeleteRequest {
            group_id: 1,
            file_ids: vec![2],
            confirm_best: false,
        })))
        .await
        .expect("delete");
    match resp {
        Response::Delete(r) => {
            assert!(r.ok, "the delete made progress: {}", r.detail);
            assert_eq!(r.removed_file_ids, vec![2]);
            assert!(
                r.detail.contains("영구 삭제") && r.detail.contains("휴지통 이동 불가"),
                "a trash that hard-deleted is reported as permanent: {}",
                r.detail
            );
            assert!(
                !r.detail.contains("휴지통으로 이동했습니다"),
                "must not claim a recoverable trash that did not happen: {}",
                r.detail
            );
        }
        other => panic!("expected Delete, got {other:?}"),
    }
    h.shutdown.trigger();
    let _ = h.serve.await;
}

fn seed_partial_edge(db: &Database, gid: i64, a: FileId, b: FileId, intro_outro: bool) {
    SimilarityEdgesRepo::new(db.conn())
        .insert(&SimilarityEdge {
            group_id: gid,
            file_a: a,
            file_b: b,
            score_x1000: 700,
            partial_span: None,
            intro_outro,
        })
        .expect("insert edge");
}

#[tokio::test]
async fn list_groups_and_clusters_flag_possible_group_when_every_edge_is_tagged() {
    let h = spawn_seeded(|db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let f1 = seed_rich(db, "/lib/tagged-a.mp4", 5_000, 0x41);
        let f2 = seed_rich(db, "/lib/tagged-b.mp4", 900, 0x42);
        let gid = repo.create(DbTrust::Possible, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
        seed_partial_edge(db, gid, f1, f2, true);
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let groups_resp = client
        .request(&Request::ListGroups {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list groups");
    match groups_resp {
        Response::Groups(groups) => {
            assert_eq!(groups.len(), 1);
            assert!(
                groups[0].intro_outro,
                "single tagged edge ⇒ group is all-tagged"
            );
        }
        other => panic!("expected Groups, got {other:?}"),
    }

    let clusters_resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list clusters");
    match clusters_resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 1);
            assert!(
                clusters[0].intro_outro,
                "POSSIBLE cluster mirrors its one group"
            );
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn list_groups_and_clusters_do_not_flag_a_partially_tagged_group() {
    let h = spawn_seeded(|db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let f1 = seed_rich(db, "/lib/mixed-a.mp4", 5_000, 0x51);
        let f2 = seed_rich(db, "/lib/mixed-b.mp4", 900, 0x52);
        let f3 = seed_rich(db, "/lib/mixed-c.mp4", 800, 0x53);
        let gid = repo.create(DbTrust::Possible, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
        repo.add_member(gid, f3).expect("add f3");
        seed_partial_edge(db, gid, f1, f2, true);
        seed_partial_edge(db, gid, f1, f3, false);
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let groups_resp = client
        .request(&Request::ListGroups {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list groups");
    match groups_resp {
        Response::Groups(groups) => {
            assert_eq!(groups.len(), 1);
            assert!(
                !groups[0].intro_outro,
                "one untagged edge ⇒ group is not all-tagged"
            );
        }
        other => panic!("expected Groups, got {other:?}"),
    }

    let clusters_resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list clusters");
    match clusters_resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 1);
            assert!(!clusters[0].intro_outro);
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn list_groups_and_clusters_never_flag_a_non_partial_group() {
    let h = spawn_seeded(|db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let f1 = seed_rich(db, "/lib/exact-a.mp4", 1_000_000, 0x61);
        let f2 = seed_rich(db, "/lib/exact-b.mp4", 1_000_000, 0x61);
        let gid = repo.create(DbTrust::Exact, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let groups_resp = client
        .request(&Request::ListGroups {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list groups");
    match groups_resp {
        Response::Groups(groups) => {
            assert_eq!(groups.len(), 1);
            assert!(
                !groups[0].intro_outro,
                "EXACT group is never intro/outro-tagged"
            );
        }
        other => panic!("expected Groups, got {other:?}"),
    }

    let clusters_resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list clusters");
    match clusters_resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 1);
            assert!(!clusters[0].intro_outro);
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn list_groups_and_clusters_do_not_flag_a_possible_group_with_no_edges() {
    let h = spawn_seeded(|db| {
        let repo = DuplicateGroupsRepo::new(db.conn());
        let f1 = seed_rich(db, "/lib/noedge-a.mp4", 5_000, 0x71);
        let f2 = seed_rich(db, "/lib/noedge-b.mp4", 900, 0x72);
        let gid = repo.create(DbTrust::Possible, T0).expect("create group");
        repo.add_member(gid, f1).expect("add f1");
        repo.add_member(gid, f2).expect("add f2");
    });
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    let groups_resp = client
        .request(&Request::ListGroups {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list groups");
    match groups_resp {
        Response::Groups(groups) => {
            assert_eq!(groups.len(), 1);
            assert!(!groups[0].intro_outro, "no edges ⇒ untagged, not a guess");
        }
        other => panic!("expected Groups, got {other:?}"),
    }

    let clusters_resp = client
        .request(&Request::ClusterSummaries {
            trust: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("list clusters");
    match clusters_resp {
        Response::ClusterSummaries(clusters) => {
            assert_eq!(clusters.len(), 1);
            assert!(!clusters[0].intro_outro);
        }
        other => panic!("expected ClusterSummaries, got {other:?}"),
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}
