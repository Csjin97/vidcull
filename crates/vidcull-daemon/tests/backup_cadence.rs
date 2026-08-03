#![allow(clippy::too_many_lines)]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vidcull_core::types::{Blake3Hash, NormalizedPath};
use vidcull_daemon::backup::KEEP_SNAPSHOTS;
use vidcull_daemon::delete::{DeleteMode, RemoveOutcome};
use vidcull_daemon::{DaemonRequestHandler, FileRemover, LogBuffer, ShutdownToken};
use vidcull_db::Database;
use vidcull_db::repo::{DuplicateGroupsRepo, FilesRepo, NewFile, TrustLevel as DbTrust};
use vidcull_ipc::{Action, DeleteRequest, IpcClient, IpcServer, Request, Response};

const T0: i64 = 1_700_000_000;
const HASH_LEN: usize = 32;

#[derive(Default)]
struct RecordingRemover {
    removed: Mutex<Vec<(String, DeleteMode)>>,
}

impl RecordingRemover {
    fn path_count(&self) -> usize {
        self.removed.lock().unwrap().len()
    }

    fn all_paths(&self) -> Vec<String> {
        self.removed
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }
}

impl FileRemover for RecordingRemover {
    fn remove(&self, path: &Path, mode: DeleteMode) -> std::io::Result<RemoveOutcome> {
        self.removed
            .lock()
            .unwrap()
            .push((path.to_string_lossy().into_owned(), mode));
        Ok(RemoveOutcome::PermanentlyDeleted)
    }
}

fn unique_endpoint() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\vidcull-cadence-{pid}-{n}")
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir()
            .join(format!("vidcull-cadence-{pid}-{n}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn list_snapshots(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.starts_with("index-")
                            && Path::new(n)
                                .extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn seed_group(db: &Database, idx: u32) -> (i64, i64, i64) {
    let hash_byte = u8::try_from(idx % 256).expect("idx % 256 is always 0..=255");
    let best = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(format!("/testcadence/{idx}/best.mp4")),
            size_bytes: 1024,
            mtime_ns: 0,
            content_hash: Some(Blake3Hash::from_bytes([hash_byte; HASH_LEN])),
            ..Default::default()
        })
        .expect("insert best file");
    let dupe = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(format!("/testcadence/{idx}/dupe.mp4")),
            size_bytes: 512,
            mtime_ns: 0,
            content_hash: Some(Blake3Hash::from_bytes([hash_byte; HASH_LEN])),
            ..Default::default()
        })
        .expect("insert dupe file");
    let groups = DuplicateGroupsRepo::new(db.conn());
    let gid = groups.create(DbTrust::Exact, T0).expect("create group");
    groups.add_member(gid, best).expect("add best member");
    groups.add_member(gid, dupe).expect("add dupe member");
    groups.set_best(gid, Some(best), T0).expect("set best");
    (best.0, dupe.0, gid)
}

struct Harness {
    address: String,
    shutdown: ShutdownToken,
    serve: tokio::task::JoinHandle<vidcull_core::Result<()>>,
    remover: Arc<RecordingRemover>,
    _tmpdir: tempfile::TempDir,
}

fn spawn_with_backup(
    db: Database,
    backup_dir: std::path::PathBuf,
    tmpdir: tempfile::TempDir,
) -> Harness {
    let remover = Arc::new(RecordingRemover::default());
    let db = Arc::new(Mutex::new(db));
    let shutdown = ShutdownToken::new();
    let logs = LogBuffer::default();
    let handler = DaemonRequestHandler::new(
        db,
        shutdown.clone(),
        logs,
        "scan".to_owned(),
        Arc::clone(&remover) as Arc<dyn FileRemover>,
    )
    .with_backup_dir(backup_dir);
    let handler = Arc::new(handler);
    let server = IpcServer::bind(&unique_endpoint()).expect("bind IPC server");
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
        serve,
        remover,
        _tmpdir: tmpdir,
    }
}

#[tokio::test]
async fn snapshots_at_batch_10_and_20() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&tmpdir.path().join("vidcull.db")).expect("open db");
    let backup_dir = tmpdir.path().join("backups");

    let groups: Vec<(i64, i64, i64)> = (0u32..20).map(|i| seed_group(&db, i)).collect();

    let h = spawn_with_backup(db, backup_dir.clone(), tmpdir);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    for (idx, (_best, dupe, gid)) in groups.iter().enumerate() {
        let resp = client
            .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
                group_id: *gid,
                file_ids: vec![*dupe],
                confirm_best: false,
            })))
            .await
            .unwrap_or_else(|e| panic!("delete {} transport error: {e}", idx + 1));
        match resp {
            Response::Delete(r) => {
                assert!(r.ok, "delete {} must succeed (got ok=false)", idx + 1);
            }
            other => panic!("delete {}: unexpected response variant: {other:?}", idx + 1),
        }
    }

    let snapshots = list_snapshots(&backup_dir);
    assert_eq!(
        snapshots.len(),
        2,
        "expected exactly 2 snapshots (batch 10 and 20), got {}; files: {snapshots:?}",
        snapshots.len(),
    );

    for snap in &snapshots {
        let snap_db =
            vidcull_db::open_file(snap).expect("snapshot must open as a readable database");
        snap_db
            .schema_version()
            .expect("snapshot must expose schema_version");
    }

    assert_eq!(
        h.remover.path_count(),
        20,
        "fake remover must receive exactly 20 removal calls",
    );
    for path in h.remover.all_paths() {
        assert!(
            path.contains("dupe.mp4"),
            "remover must only receive dupe paths; got: {path}",
        );
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}

#[tokio::test]
async fn invalid_backup_dir_does_not_abort_delete() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&tmpdir.path().join("vidcull.db")).expect("open db");

    let bad_dir = tmpdir.path().join("not_a_dir");
    std::fs::write(&bad_dir, b"blocker").expect("create blocking file");

    let groups: Vec<(i64, i64, i64)> = (0u32..10).map(|i| seed_group(&db, i + 100)).collect();

    let remover = Arc::new(RecordingRemover::default());
    let db_arc = Arc::new(Mutex::new(db));
    let shutdown = ShutdownToken::new();
    let logs = LogBuffer::default();
    let handler = DaemonRequestHandler::new(
        db_arc,
        shutdown.clone(),
        logs,
        "scan".to_owned(),
        Arc::clone(&remover) as Arc<dyn FileRemover>,
    )
    .with_backup_dir(bad_dir.clone());
    let handler = Arc::new(handler);
    let server = IpcServer::bind(&unique_endpoint()).expect("bind");
    let address = server.address().to_owned();
    let sd = shutdown.clone();
    let serve = tokio::spawn(async move {
        server
            .serve(handler, async move { sd.cancelled().await })
            .await
    });
    let mut client = IpcClient::connect(&address).await.expect("connect");

    for (_best, dupe, gid) in &groups {
        let resp = client
            .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
                group_id: *gid,
                file_ids: vec![*dupe],
                confirm_best: false,
            })))
            .await
            .expect("request must succeed at transport level");
        match resp {
            Response::Delete(r) => {
                assert!(r.ok, "delete must succeed even when snapshot fails");
            }
            other => panic!("unexpected response variant: {other:?}"),
        }
    }

    let snaps = list_snapshots(&bad_dir);
    assert_eq!(
        snaps.len(),
        0,
        "no snapshots must exist when backup_dir is blocked"
    );

    shutdown.trigger();
    let _ = serve.await;
}

#[test]
fn snapshot_eviction_direct() {
    const BASE: i64 = 1_700_000_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = vidcull_db::open_in_memory().expect("in-memory db");
    let mut paths = Vec::new();
    for i in 0..4_i64 {
        let p = vidcull_daemon::backup::snapshot_into(&db, dir.path(), BASE + i)
            .expect("snapshot_into must succeed");
        paths.push(p);
    }

    let remaining = list_snapshots(dir.path());
    assert_eq!(
        remaining.len(),
        KEEP_SNAPSHOTS,
        "prune must retain exactly {KEEP_SNAPSHOTS} snapshots; got {}; files: {remaining:?}",
        remaining.len(),
    );

    assert!(
        !paths[0].exists(),
        "oldest snapshot (now={BASE}+0) must be evicted, but {:?} still exists",
        paths[0],
    );
    assert!(
        paths[1].exists(),
        "second snapshot must survive after prune"
    );
    assert!(paths[2].exists(), "third snapshot must survive after prune");
    assert!(
        paths[3].exists(),
        "fourth (newest) snapshot must survive after prune"
    );

    for p in &paths[1..] {
        vidcull_db::open_file(p).expect("surviving snapshot must be readable");
    }
}

#[tokio::test]
async fn cadence_prune_count_after_forty_batches() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&tmpdir.path().join("vidcull.db")).expect("open db");
    let backup_dir = tmpdir.path().join("backups");

    let groups: Vec<(i64, i64, i64)> = (0u32..40).map(|i| seed_group(&db, i + 200)).collect();

    let h = spawn_with_backup(db, backup_dir.clone(), tmpdir);
    let mut client = IpcClient::connect(&h.address).await.expect("connect");

    for (_best, dupe, gid) in &groups {
        client
            .request(&Request::Action(Action::DeletePermanent(DeleteRequest {
                group_id: *gid,
                file_ids: vec![*dupe],
                confirm_best: false,
            })))
            .await
            .expect("delete request must succeed at transport level");
    }

    let snapshots = list_snapshots(&backup_dir);
    assert_eq!(
        snapshots.len(),
        KEEP_SNAPSHOTS,
        "prune must leave exactly {KEEP_SNAPSHOTS} snapshots after 40 batches; \
         got {}; files: {snapshots:?}",
        snapshots.len(),
    );

    for snap in &snapshots {
        let name = snap.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("index-") && name.ends_with(".db"),
            "surviving file has unexpected name: {name}",
        );
        vidcull_db::open_file(snap).expect("surviving snapshot must be readable");
    }

    h.shutdown.trigger();
    let _ = h.serve.await;
}
