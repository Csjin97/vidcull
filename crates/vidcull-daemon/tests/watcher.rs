mod common;

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use common::FAKE_NOW;
use notify::Event;
use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};
use tempfile::tempdir;
use vidcull_daemon::{
    ChangeKind, ChangeTask, ShutdownToken, WatchConfig, enqueue_changes, enqueue_partial_backfill,
    run_event_loop,
};
use vidcull_db::Database;
use vidcull_db::repo::{TaskQueueRepo, TaskState};
use vidcull_ipc::{BestCopyMode, CpuThrottle, DaemonSettings};
use vidcull_scanner::ScanOptions;

const KIND: &str = "scan";

fn held_config() -> WatchConfig {
    WatchConfig {
        quiet_period: Duration::from_secs(3_600),
        poll_interval: Duration::from_millis(5),
        task_kind: KIND.to_owned(),
        ..WatchConfig::default()
    }
}

fn pending_tasks(db: &Database) -> Vec<vidcull_db::repo::Task> {
    TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Pending)
        .expect("list pending")
}

fn create_event(path: &str) -> Event {
    Event::new(EventKind::Create(CreateKind::File)).add_path(PathBuf::from(path))
}

fn remove_event(path: &str) -> Event {
    Event::new(EventKind::Remove(RemoveKind::File)).add_path(PathBuf::from(path))
}

fn run_with_events(db: &mut Database, events: Vec<Event>) -> usize {
    let (tx, rx) = channel::<notify::Result<Event>>();
    for event in events {
        tx.send(Ok(event)).expect("send event");
    }
    drop(tx);
    let shutdown = ShutdownToken::new();
    let config = held_config();
    let stats = run_event_loop(&rx, &config, db, &shutdown, None, &[], || 0, || FAKE_NOW)
        .expect("run loop");
    stats.changes_enqueued
}

#[test]
fn enqueue_changes_writes_one_decodable_task_per_change() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");

    let changes = vec![
        ChangeTask {
            path: vidcull_core::NormalizedPath::new("/lib/a.mp4"),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        },
        ChangeTask {
            path: vidcull_core::NormalizedPath::new("/lib/b.mkv"),
            change: ChangeKind::Remove,
            size_bytes: 0,
        },
    ];
    let n = enqueue_changes(&mut db, &changes, KIND, 0, FAKE_NOW).expect("enqueue");
    assert_eq!(n, 2);

    let tasks = pending_tasks(&db);
    assert_eq!(tasks.len(), 2);
    let decoded: Vec<ChangeTask> = tasks
        .iter()
        .map(|t| ChangeTask::from_payload(t.payload.as_ref().expect("payload")).expect("decode"))
        .collect();
    assert!(decoded.contains(&changes[0]));
    assert!(decoded.contains(&changes[1]));
    assert!(tasks.iter().all(|t| t.enqueued_at == FAKE_NOW));
}

#[test]
fn enqueue_changes_quarantines_a_file_that_failed_at_the_same_size() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("q.db")).expect("open");

    let change = ChangeTask {
        path: vidcull_core::NormalizedPath::new("/lib/corrupt.mp4"),
        change: ChangeKind::Upsert,
        size_bytes: 90_000,
    };
    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&change), KIND, 0, FAKE_NOW).expect("e1"),
        1
    );
    {
        let repo = TaskQueueRepo::new(db.conn());
        let task = repo
            .dequeue_next(KIND, FAKE_NOW)
            .expect("dq")
            .expect("task");
        repo.mark_failed(task.id, FAKE_NOW, "decode error")
            .expect("fail");
    }
    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&change), KIND, 0, FAKE_NOW).expect("e2"),
        0,
        "a file that already failed at this size must not be re-queued"
    );
    let replaced = ChangeTask {
        size_bytes: 91_000,
        ..change.clone()
    };
    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&replaced), KIND, 0, FAKE_NOW).expect("e3"),
        1,
        "a replaced file (different size) retries"
    );
}

#[test]
fn enqueue_changes_densify_failure_does_not_quarantine_the_file() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("densify_q.db")).expect("open");

    let densify = ChangeTask {
        path: vidcull_core::NormalizedPath::new("/lib/fallback.mp4"),
        change: ChangeKind::Densify,
        size_bytes: 0,
    };

    assert_eq!(
        enqueue_changes(
            &mut db,
            std::slice::from_ref(&densify),
            KIND,
            -100,
            FAKE_NOW
        )
        .expect("enqueue densify 1"),
        1,
        "first densify must be enqueued"
    );

    {
        let repo = TaskQueueRepo::new(db.conn());
        let task = repo
            .dequeue_next(KIND, FAKE_NOW)
            .expect("dq")
            .expect("task");
        repo.mark_failed(task.id, FAKE_NOW, "decode error on densify")
            .expect("fail");
    }

    assert_eq!(
        enqueue_changes(
            &mut db,
            std::slice::from_ref(&densify),
            KIND,
            -100,
            FAKE_NOW
        )
        .expect("enqueue densify 2"),
        1,
        "a failed Densify must not quarantine the file; the revisit must be re-enqueued"
    );

    let upsert = ChangeTask {
        path: vidcull_core::NormalizedPath::new("/lib/fallback.mp4"),
        change: ChangeKind::Upsert,
        size_bytes: 50_000,
    };
    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&upsert), KIND, 0, FAKE_NOW)
            .expect("enqueue upsert 1"),
        1,
        "first upsert lands normally"
    );
    {
        let repo = TaskQueueRepo::new(db.conn());
        while let Some(task) = repo.dequeue_next(KIND, FAKE_NOW).expect("dq") {
            let change =
                ChangeTask::from_payload(task.payload.as_deref().expect("payload")).expect("dec");
            if change.change == ChangeKind::Upsert {
                repo.mark_failed(task.id, FAKE_NOW, "corrupted file")
                    .expect("fail upsert");
                break;
            }
            repo.mark_done(task.id, FAKE_NOW).expect("done other");
        }
    }
    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&upsert), KIND, 0, FAKE_NOW)
            .expect("enqueue upsert 2"),
        0,
        "a failed Upsert at the same size is still quarantined"
    );
}

#[test]
fn enqueue_changes_force_supersedes_active_upsert_for_same_path() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("supersede.db")).expect("open");

    let path = vidcull_core::NormalizedPath::new("/lib/clip.mp4");
    let upsert = ChangeTask {
        path: path.clone(),
        change: ChangeKind::Upsert,
        size_bytes: 90_000,
    };
    let force = ChangeTask {
        path: path.clone(),
        change: ChangeKind::ForceUpsert,
        size_bytes: 90_000,
    };

    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&upsert), KIND, 0, FAKE_NOW).expect("e1"),
        1
    );
    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&force), KIND, 0, FAKE_NOW).expect("e2"),
        1,
        "the force-rescan lands"
    );

    let upsert_class: Vec<ChangeKind> = pending_tasks(&db)
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref()?).ok())
        .filter(|c| c.path == path)
        .map(|c| c.change)
        .collect();
    assert_eq!(
        upsert_class,
        vec![ChangeKind::ForceUpsert],
        "exactly one active upsert-class row survives; the force wins (was 2 rows pre-M1)"
    );

    assert_eq!(
        enqueue_changes(&mut db, std::slice::from_ref(&upsert), KIND, 0, FAKE_NOW).expect("e3"),
        0,
        "a plain Upsert is skipped while a ForceUpsert is active for the same path"
    );
    let still_one: Vec<ChangeKind> = pending_tasks(&db)
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref()?).ok())
        .filter(|c| c.path == path)
        .map(|c| c.change)
        .collect();
    assert_eq!(still_one, vec![ChangeKind::ForceUpsert]);
}

#[test]
fn watcher_live_reloads_exclude_rules() {
    use std::sync::atomic::{AtomicI64, Ordering};

    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("excl.db")).expect("open");

    let settings = DaemonSettings {
        scan_folders: Vec::new(),
        background_enabled: true,
        auto_index: true,
        exclude_rules: vec![".trash".to_owned()],
        run_on_boot: false,
        cpu_throttle: CpuThrottle::Full,
        best_copy_mode: BestCopyMode::Archival,
        idle_worker_count: None,
        cpu_cores: 1,
        partial_clips_enabled: false,
        indexing_enabled: true,
    };
    vidcull_daemon::settings::save(&db, &settings).expect("save settings");

    let (tx, rx) = channel::<notify::Result<Event>>();
    tx.send(Ok(create_event("/lib/.trash/excluded.mp4")))
        .expect("send excluded");
    tx.send(Ok(create_event("/lib/keep/wanted.mp4")))
        .expect("send wanted");
    drop(tx);

    let config = WatchConfig {
        quiet_period: Duration::from_secs(3_600),
        poll_interval: Duration::from_millis(5),
        task_kind: KIND.to_owned(),
        ..WatchConfig::default()
    };

    let clock = AtomicI64::new(0);
    let monotonic = || clock.fetch_add(2_000_000_000, Ordering::SeqCst);

    let shutdown = ShutdownToken::new();
    run_event_loop(
        &rx,
        &config,
        &mut db,
        &shutdown,
        None,
        &[],
        monotonic,
        || FAKE_NOW,
    )
    .expect("run loop");

    let paths: Vec<String> = pending_tasks(&db)
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref()?).ok())
        .map(|c| c.path.as_str().to_owned())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("keep/wanted.mp4")),
        "the non-excluded video is still enqueued: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains("/.trash/")),
        "a video under a live-reloaded `.trash` exclude must NOT be enqueued: {paths:?}"
    );
}

#[test]
fn enqueue_changes_empty_batch_is_a_noop() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    assert_eq!(
        enqueue_changes(&mut db, &[], KIND, 0, FAKE_NOW).expect("enqueue"),
        0
    );
    assert!(pending_tasks(&db).is_empty());
}

#[test]
fn loop_turns_video_events_into_tasks() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");

    let enqueued = run_with_events(
        &mut db,
        vec![
            create_event("/lib/one.mp4"),
            create_event("/lib/two.mkv"),
            remove_event("/lib/three.mov"),
        ],
    );
    assert_eq!(enqueued, 3);

    let tasks = pending_tasks(&db);
    assert_eq!(tasks.len(), 3);
    let removes = tasks
        .iter()
        .filter(|t| {
            ChangeTask::from_payload(t.payload.as_ref().unwrap())
                .unwrap()
                .change
                == ChangeKind::Remove
        })
        .count();
    assert_eq!(removes, 1, "the one remove event maps to a Remove task");
}

#[test]
fn loop_ignores_non_video_events() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");

    let enqueued = run_with_events(
        &mut db,
        vec![
            create_event("/lib/notes.txt"),
            create_event("/lib/cover.jpg"),
            create_event("/lib/subdir"),
        ],
    );
    assert_eq!(enqueued, 0);
    assert!(pending_tasks(&db).is_empty());
}

#[test]
fn loop_coalesces_a_thousand_event_burst_per_path() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");

    let paths = ["/lib/a.mp4", "/lib/b.mkv", "/lib/c.mov", "/lib/d.webm"];
    let events: Vec<Event> = (0..1000)
        .map(|i| create_event(paths[i % paths.len()]))
        .collect();
    let enqueued = run_with_events(&mut db, events);

    assert_eq!(enqueued, paths.len());
    assert_eq!(pending_tasks(&db).len(), paths.len());
}

#[test]
fn loop_rename_both_purges_old_and_indexes_new() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");

    let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
        .add_path(PathBuf::from("/lib/old.mp4"))
        .add_path(PathBuf::from("/lib/new.mp4"));
    let enqueued = run_with_events(&mut db, vec![rename]);
    assert_eq!(enqueued, 2);

    let mut by_path: Vec<(String, ChangeKind)> = pending_tasks(&db)
        .iter()
        .map(|t| {
            let c = ChangeTask::from_payload(t.payload.as_ref().unwrap()).unwrap();
            (c.path.as_str().to_owned(), c.change)
        })
        .collect();
    by_path.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        by_path,
        vec![
            ("/lib/new.mp4".to_owned(), ChangeKind::Upsert),
            ("/lib/old.mp4".to_owned(), ChangeKind::Remove),
        ]
    );
}

#[test]
fn loop_stops_when_shutdown_is_triggered() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");

    let (_tx, rx): (Sender<notify::Result<Event>>, _) = channel();
    let shutdown = ShutdownToken::new();
    shutdown.trigger();
    let config = held_config();

    let stats = run_event_loop(
        &rx,
        &config,
        &mut db,
        &shutdown,
        None,
        &[],
        || 0,
        || FAKE_NOW,
    )
    .expect("run");
    assert_eq!(stats.changes_enqueued, 0);
    assert!(pending_tasks(&db).is_empty());
}

#[test]
fn real_watcher_enqueues_a_created_video() {
    use vidcull_daemon::FileWatcher;

    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("av.db");
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let poll_db = vidcull_db::open_file(&db_path).expect("open poll db");

    let config = WatchConfig {
        quiet_period: Duration::from_millis(150),
        poll_interval: Duration::from_millis(20),
        task_kind: KIND.to_owned(),
        options: ScanOptions::default(),
        priority: 0,
        throttle_control: None,
    };
    let mut watcher = FileWatcher::new(config).expect("watcher");
    watcher.watch(dir.path()).expect("watch");

    let shutdown = ShutdownToken::new();
    let worker_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        let mut db = worker_db;
        watcher.run(&mut db, &worker_shutdown).expect("watcher run")
    });

    let video = dir.path().join("clip.mp4");
    std::fs::write(&video, b"not a real video, just bytes").expect("write video");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        if !pending_tasks(&poll_db).is_empty() {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    shutdown.trigger();
    let _stats = handle.join().expect("join worker");

    assert!(
        found,
        "the created .mp4 should have produced a pending task"
    );
    let tasks = pending_tasks(&poll_db);
    let change = ChangeTask::from_payload(tasks[0].payload.as_ref().expect("payload"))
        .expect("decode payload");
    assert_eq!(change.change, ChangeKind::Upsert);
    assert!(change.path.as_str().ends_with("clip.mp4"));
}

#[test]
fn dynamic_watcher_and_auto_index_control() {
    use vidcull_daemon::FileWatcher;

    let dir1 = tempdir().expect("tempdir 1");
    let dir2 = tempdir().expect("tempdir 2");
    let db_path = dir1.path().join("av.db");

    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let poll_db = vidcull_db::open_file(&db_path).expect("open poll db");

    let initial_settings = DaemonSettings {
        scan_folders: Vec::new(),
        background_enabled: true,
        auto_index: true,
        exclude_rules: Vec::new(),
        run_on_boot: false,
        cpu_throttle: CpuThrottle::Full,
        best_copy_mode: BestCopyMode::Archival,
        idle_worker_count: None,
        cpu_cores: 1,
        partial_clips_enabled: false,
        indexing_enabled: true,
    };
    vidcull_daemon::settings::save(&worker_db, &initial_settings).expect("save settings");

    let config = WatchConfig {
        quiet_period: Duration::from_millis(150),
        poll_interval: Duration::from_millis(20),
        task_kind: KIND.to_owned(),
        options: ScanOptions::default(),
        priority: 0,
        throttle_control: None,
    };

    let mut watcher = FileWatcher::new(config).expect("watcher");
    let shutdown = ShutdownToken::new();
    let worker_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        let mut db = worker_db;
        watcher.run(&mut db, &worker_shutdown).expect("watcher run")
    });

    let video1 = dir2.path().join("clip1.mp4");
    std::fs::write(&video1, b"bytes").expect("write video1");

    thread::sleep(Duration::from_millis(300));
    assert!(pending_tasks(&poll_db).is_empty());

    let updated_settings = DaemonSettings {
        scan_folders: vec![dir2.path().to_string_lossy().replace('\\', "/")],
        ..initial_settings.clone()
    };
    let update_db = vidcull_db::open_file(&db_path).expect("open update db");
    vidcull_daemon::settings::save(&update_db, &updated_settings).expect("save settings");

    thread::sleep(Duration::from_millis(1500));

    let video2 = dir2.path().join("clip2.mp4");
    std::fs::write(&video2, b"bytes").expect("write video2");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found_clip2 = false;
    while Instant::now() < deadline {
        let tasks = pending_tasks(&poll_db);
        if tasks.iter().any(|t| {
            let change = ChangeTask::from_payload(t.payload.as_ref().unwrap()).unwrap();
            change.path.as_str().ends_with("clip2.mp4")
        }) {
            found_clip2 = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        found_clip2,
        "dynamically watched folder should have detected clip2.mp4"
    );

    let disabled_settings = DaemonSettings {
        auto_index: false,
        ..updated_settings
    };
    vidcull_daemon::settings::save(&update_db, &disabled_settings).expect("save settings");

    thread::sleep(Duration::from_millis(1500));

    let video3 = dir2.path().join("clip3.mp4");
    std::fs::write(&video3, b"bytes").expect("write video3");

    thread::sleep(Duration::from_millis(1000));
    let final_tasks = pending_tasks(&poll_db);
    let found_clip3 = final_tasks.iter().any(|t| {
        let change = ChangeTask::from_payload(t.payload.as_ref().unwrap()).unwrap();
        change.path.as_str().ends_with("clip3.mp4")
    });
    assert!(
        !found_clip3,
        "clip3.mp4 should not be detected when auto_index is disabled"
    );

    shutdown.trigger();
    let _stats = handle.join().expect("join worker");
}

#[test]
fn reconcile_sweep_enqueues_remove_for_a_vanished_file_via_the_loop() {
    use std::sync::mpsc::channel;
    use vidcull_core::types::NormalizedPath;
    use vidcull_db::repo::{FilesRepo, NewFile};

    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("av.db");
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let poll_db = vidcull_db::open_file(&db_path).expect("open poll db");

    let settings = DaemonSettings {
        scan_folders: vec![dir.path().to_string_lossy().replace('\\', "/")],
        background_enabled: true,
        auto_index: true,
        exclude_rules: Vec::new(),
        run_on_boot: false,
        cpu_throttle: CpuThrottle::Full,
        best_copy_mode: BestCopyMode::Archival,
        idle_worker_count: None,
        cpu_cores: 1,
        partial_clips_enabled: false,
        indexing_enabled: true,
    };
    vidcull_daemon::settings::save(&worker_db, &settings).expect("save settings");

    let gone = dir.path().join("clips").join("gone.mp4");
    let gone_norm = NormalizedPath::new(&gone);
    FilesRepo::new(worker_db.conn())
        .insert(&NewFile {
            path: gone_norm.clone(),
            size_bytes: 1,
            ..Default::default()
        })
        .expect("seed gone file");

    let (tx, rx) = channel::<notify::Result<Event>>();
    let shutdown = ShutdownToken::new();
    let worker_shutdown = shutdown.clone();
    let roots = vec![dir.path().to_path_buf()];
    let handle = thread::spawn(move || {
        let mut db = worker_db;
        let config = WatchConfig {
            quiet_period: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            task_kind: KIND.to_owned(),
            ..WatchConfig::default()
        };
        run_event_loop(
            &rx,
            &config,
            &mut db,
            &worker_shutdown,
            None,
            &roots,
            || 0,
            || FAKE_NOW,
        )
        .expect("run")
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        let has_remove = pending_tasks(&poll_db).iter().any(|t| {
            ChangeTask::from_payload(t.payload.as_ref().expect("payload"))
                .map(|c| c.change == ChangeKind::Remove && c.path == gone_norm)
                .unwrap_or(false)
        });
        if has_remove {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    shutdown.trigger();
    drop(tx);
    let _ = handle.join().expect("join worker");

    assert!(
        found,
        "the reconcile sweep must enqueue a Remove for the file missing from disk",
    );
}

#[test]
fn reconcile_scopes_configured_scan_folders_even_when_unwatched() {
    use std::sync::mpsc::channel;
    use vidcull_core::types::NormalizedPath;
    use vidcull_db::repo::{FilesRepo, NewFile};

    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("av.db");
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let poll_db = vidcull_db::open_file(&db_path).expect("open poll db");

    let moved_root = dir.path().join("iuueas");
    let settings = DaemonSettings {
        scan_folders: vec![moved_root.to_string_lossy().replace('\\', "/")],
        background_enabled: true,
        auto_index: true,
        exclude_rules: Vec::new(),
        run_on_boot: false,
        cpu_throttle: CpuThrottle::Full,
        best_copy_mode: BestCopyMode::Archival,
        idle_worker_count: None,
        cpu_cores: 1,
        partial_clips_enabled: false,
        indexing_enabled: true,
    };
    vidcull_daemon::settings::save(&worker_db, &settings).expect("save settings");

    let gone = moved_root.join("clip.mp4");
    let gone_norm = NormalizedPath::new(&gone);
    FilesRepo::new(worker_db.conn())
        .insert(&NewFile {
            path: gone_norm.clone(),
            size_bytes: 1,
            ..Default::default()
        })
        .expect("seed file under moved root");

    let (tx, rx) = channel::<notify::Result<Event>>();
    let shutdown = ShutdownToken::new();
    let worker_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        let mut db = worker_db;
        let config = WatchConfig {
            quiet_period: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            task_kind: KIND.to_owned(),
            ..WatchConfig::default()
        };
        run_event_loop(
            &rx,
            &config,
            &mut db,
            &worker_shutdown,
            None,
            &[],
            || 0,
            || FAKE_NOW,
        )
        .expect("run")
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        let has_remove = pending_tasks(&poll_db).iter().any(|t| {
            ChangeTask::from_payload(t.payload.as_ref().expect("payload"))
                .map(|c| c.change == ChangeKind::Remove && c.path == gone_norm)
                .unwrap_or(false)
        });
        if has_remove {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    shutdown.trigger();
    drop(tx);
    let _ = handle.join().expect("join worker");
    assert!(
        found,
        "reconcile must purge a moved-away CONFIGURED root's files even though it was never watched",
    );
}

#[test]
fn backfill_excludes_skip_marked_but_enqueues_toggle_window_file() {
    use vidcull_core::types::{Codec, NormalizedPath};
    use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile, PartialSkipMarker};
    use vidcull_fingerprint::format::{self, FORMAT_VERSION};
    use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

    const T0: i64 = 1_700_000_000;
    const MTIME: i64 = 1_700_000_000_000_000_000;

    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("backfill.db")).expect("open");

    let seed = |db: &Database, path: &str| -> vidcull_core::types::FileId {
        let id = FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: NormalizedPath::new(path),
                size_bytes: 1024,
                mtime_ns: MTIME,
                codec: Some(Codec::Av1),
                first_seen_at: T0,
                last_seen_at: T0,
                ..Default::default()
            })
            .expect("insert file");
        let t1 = Tier1Fingerprint {
            duration_ms: 60_000,
            codec: Codec::Av1,
            gop: GopSignature::from_durations(&[]),
            global_phash: 1,
        };
        FingerprintsRepo::new(db.conn())
            .upsert(&Fingerprint {
                file_id: id,
                tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: T0,
            })
            .expect("upsert fingerprint");
        id
    };

    let marked = seed(&db, "/lib/undecodable_av1.mp4");
    FingerprintsRepo::new(db.conn())
        .set_partial_skip(
            marked,
            &PartialSkipMarker {
                reason: "unsupported-codec".to_owned(),
                size_bytes: 1024,
                mtime_ns: MTIME,
            },
        )
        .expect("set partial skip marker");

    let _toggle = seed(&db, "/lib/toggle_window.mp4");

    let first = enqueue_partial_backfill(&mut db, KIND, T0).expect("first backfill");
    assert_eq!(
        first, 1,
        "exactly the toggle-window file is enqueued; the skip-marked AV1 is excluded",
    );
    let queued: Vec<String> = pending_tasks(&db)
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref().unwrap()).ok())
        .filter(|c| c.change == ChangeKind::PartialFingerprint)
        .map(|c| c.path.as_str().to_owned())
        .collect();
    assert_eq!(queued, vec!["/lib/toggle_window.mp4".to_owned()]);

    let second = enqueue_partial_backfill(&mut db, KIND, T0).expect("second backfill");
    assert_eq!(
        second, 0,
        "#2: a re-drain enqueues nothing — the marker stops re-enqueue churn",
    );
}
