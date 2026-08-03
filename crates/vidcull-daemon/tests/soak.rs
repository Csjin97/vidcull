use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use vidcull_core::types::NormalizedPath;
use vidcull_core::{Error, Result};
use vidcull_daemon::metrics::MetricsCollector;
use vidcull_daemon::watcher::{FileWatcher, WatchConfig};
use vidcull_daemon::{
    ChangeKind, ChangeTask, Daemon, DaemonConfig, ShutdownToken, TaskHandler, unix_now,
};
use vidcull_db::Database;
use vidcull_db::repo::{FilesRepo, NewFile, Task, TaskQueueRepo, TaskState};

const PATH_POOL: u64 = 4096;

struct SoakHandler {
    db: Database,
}

impl TaskHandler for SoakHandler {
    fn handle(&mut self, task: &Task) -> Result<()> {
        let payload = task
            .payload
            .as_deref()
            .ok_or_else(|| Error::Unsupported("soak task has no payload".to_owned()))?;
        let change = ChangeTask::from_payload(payload)?;
        let now = unix_now();
        self.db.transaction(|conn| {
            let files = FilesRepo::new(conn);
            match change.change {
                ChangeKind::Upsert => {
                    let new_file = NewFile {
                        path: change.path.clone(),
                        size_bytes: 1,
                        mtime_ns: 0,
                        inode: None,
                        content_hash: Some(hash_from_path(&change.path)),
                        codec: None,
                        container: None,
                        duration: None,
                        fps_x1000: None,
                        bitrate_bps: None,
                        resolution: None,
                        first_seen_at: now,
                        last_seen_at: now,
                        ..Default::default()
                    };
                    match files.find_by_path(&change.path)? {
                        Some(existing) => {
                            files.update_metadata(existing.id, &new_file)?;
                        }
                        None => {
                            files.insert(&new_file)?;
                        }
                    }
                }
                ChangeKind::Remove => {
                    if let Some(existing) = files.find_by_path(&change.path)? {
                        files.mark_deleted(existing.id, now)?;
                    }
                }
                ChangeKind::Densify | ChangeKind::ForceUpsert | ChangeKind::PartialFingerprint => {
                    return Err(Error::Unsupported(
                        "soak queue must only carry watcher-classified changes".to_owned(),
                    ));
                }
            }
            Ok(())
        })
    }
}

fn hash_from_path(path: &NormalizedPath) -> vidcull_core::types::Blake3Hash {
    let mut bytes = [0u8; 32];
    for (i, b) in path.as_str().as_bytes().iter().enumerate() {
        bytes[i % 32] ^= *b;
    }
    vidcull_core::types::Blake3Hash::from_bytes(bytes)
}

fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn slot_path(watch_root: &std::path::Path, slot: u64) -> PathBuf {
    watch_root.join(format!("file_{slot}.mp4"))
}

#[test]
#[ignore = "soak: long-running, run explicitly with --ignored (nightly sets AV_SOAK_SECS)"]
#[allow(clippy::too_many_lines)]
#[allow(clippy::cast_precision_loss)]
fn daemon_survives_a_sustained_random_event_storm() {
    let secs: u64 = std::env::var("AV_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let producer_run = Duration::from_secs(secs);

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("soak.db");
    let watch_root = tmp.path().join("watched");
    std::fs::create_dir(&watch_root).expect("watch root");
    drop(vidcull_db::open_file(&db_path).expect("init db"));

    let worker_shutdown = ShutdownToken::new();
    let watcher_shutdown = ShutdownToken::new();

    let quiet_period = Duration::from_millis(100);
    let watch_config = WatchConfig {
        quiet_period,
        poll_interval: Duration::from_millis(50),
        ..WatchConfig::default()
    };

    let watcher = {
        let db_path = db_path.clone();
        let shutdown = watcher_shutdown.clone();
        let root = watch_root.clone();
        std::thread::spawn(move || {
            let mut db = vidcull_db::open_file(&db_path).expect("watcher db");
            let mut watcher = FileWatcher::new(watch_config).expect("file watcher");
            watcher.watch(&root).expect("watch root");
            watcher.run(&mut db, &shutdown).expect("watcher run")
        })
    };

    let producer = {
        let root = watch_root.clone();
        std::thread::spawn(move || {
            let mut rng = 0x1234_5678_9abc_def0u64;
            let mut ops: u64 = 0;
            let deadline = Instant::now() + producer_run;
            while Instant::now() < deadline {
                let r = next_rand(&mut rng);
                let path = slot_path(&root, r % PATH_POOL);
                if r & 1 == 0 {
                    std::fs::write(&path, r.to_le_bytes()).expect("producer write");
                } else {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => panic!("producer remove: {err}"),
                    }
                }
                ops += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            ops
        })
    };

    let worker = {
        let db_path = db_path.clone();
        let shutdown = worker_shutdown.clone();
        std::thread::spawn(move || {
            let mut db = vidcull_db::open_file(&db_path).expect("worker db");
            let handler_db = vidcull_db::open_file(&db_path).expect("handler db");
            let mut handler = SoakHandler { db: handler_db };
            let daemon = Daemon::new(DaemonConfig::default());
            daemon
                .run(&mut db, &mut handler, &shutdown, unix_now)
                .expect("worker run")
        })
    };

    let collector = MetricsCollector::new();
    std::thread::sleep(Duration::from_secs(1).min(producer_run));
    let rss_baseline = collector.sample().rss_bytes;
    let mut rss_max = rss_baseline;

    while !producer.is_finished() {
        std::thread::sleep(Duration::from_millis(200));
        rss_max = rss_max.max(collector.sample().rss_bytes);
    }
    let produced_ops = producer.join().expect("producer thread");

    std::thread::sleep(quiet_period * 3);
    watcher_shutdown.trigger();
    let watch_stats = watcher.join().expect("watcher thread");

    let drain_db = vidcull_db::open_file(&db_path).expect("drain db");
    let drain_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let repo = TaskQueueRepo::new(drain_db.conn());
        let pending = repo.count_by_state(TaskState::Pending).expect("pending");
        let running = repo.count_by_state(TaskState::Running).expect("running");
        if pending == 0 && running == 0 {
            break;
        }
        assert!(
            Instant::now() < drain_deadline,
            "worker did not drain the queue within the bound (deadlock?): \
             pending={pending} running={running}"
        );
        std::thread::sleep(Duration::from_millis(20));
        rss_max = rss_max.max(collector.sample().rss_bytes);
    }

    worker_shutdown.trigger();
    let stats = worker.join().expect("worker thread");
    rss_max = rss_max.max(collector.sample().rss_bytes);

    let verify = vidcull_db::open_file(&db_path).expect("verify db");

    let integrity: String = verify
        .conn()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check");
    assert_eq!(integrity, "ok", "database integrity after soak");

    assert!(produced_ops > 0, "the producer performed filesystem ops");
    assert!(watch_stats.events_seen > 0, "the watcher saw OS events");
    assert!(
        watch_stats.changes_enqueued > 0,
        "the watcher enqueued change tasks"
    );

    let repo = TaskQueueRepo::new(verify.conn());
    let pending = repo.count_by_state(TaskState::Pending).expect("pending");
    let running = repo.count_by_state(TaskState::Running).expect("running");
    let done = repo.count_by_state(TaskState::Done).expect("done");
    let failed = repo.count_by_state(TaskState::Failed).expect("failed");
    assert_eq!(running, 0, "no task left stranded in RUNNING");
    assert_eq!(pending, 0, "queue fully drained");
    let total = u64::try_from(watch_stats.changes_enqueued).unwrap_or(0);
    assert_eq!(
        done + failed,
        total,
        "every watcher-enqueued task is accounted for (done={done} failed={failed} total={total})"
    );
    assert!(done > 0, "the worker completed real tasks");

    if failed > 0 {
        let rows = repo.list_by_state(TaskState::Failed).expect("failed rows");
        for task in &rows {
            let reason = task.last_error.as_deref().unwrap_or("");
            assert!(
                reason.contains("locked"),
                "the only tolerated soak failure is transient lock contention, got: {reason:?}"
            );
        }
        eprintln!("soak: {failed}/{total} tasks hit transient WAL lock contention (tolerated)");
    }
    assert_eq!(
        u64::try_from(stats.processed).unwrap_or(0),
        done,
        "the worker's processed count matches the DONE rows"
    );

    if failed == 0 {
        let files = FilesRepo::new(verify.conn());
        let active: HashSet<String> = files
            .list_active()
            .expect("list_active")
            .into_iter()
            .map(|record| record.path.as_str().to_owned())
            .collect();
        for slot in 0..PATH_POOL {
            let on_disk = slot_path(&watch_root, slot).exists();
            let path = NormalizedPath::new(slot_path(&watch_root, slot));
            let in_db = active.contains(path.as_str());
            assert_eq!(
                on_disk,
                in_db,
                "disk/DB divergence for {}: on_disk={on_disk} active_row={in_db}",
                path.as_str()
            );
        }
    }

    eprintln!(
        "soak summary: fs_ops={produced_ops} events_seen={} enqueued={total} done={done} failed={failed}",
        watch_stats.events_seen
    );

    if rss_baseline > 0 {
        eprintln!(
            "soak rss: baseline={rss_baseline} max={rss_max} ratio={:.2}",
            rss_max as f64 / rss_baseline as f64
        );
        assert!(
            rss_max <= rss_baseline.saturating_mul(2),
            "RSS grew past 2x baseline: baseline={rss_baseline} max={rss_max}"
        );
    } else {
        eprintln!("soak rss: unmeasured on this platform — ceiling assertion skipped");
    }
}
