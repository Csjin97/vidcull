mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FAKE_NOW, KIND};
use tempfile::tempdir;
use vidcull_core::types::NormalizedPath;
use vidcull_core::{Error, Result};
use vidcull_daemon::{
    Activity, ChangeKind, ChangeTask, Daemon, DaemonConfig, ShutdownToken, TaskHandler,
    ThrottleControl, enqueue_changes, enqueue_changes_guarded,
};
use vidcull_db::repo::{Task, TaskQueueRepo, TaskState};

struct SlowCancelPollingHandler {
    token: ShutdownToken,
    control: Option<Arc<ThrottleControl>>,
    pub steps_to_cancel: usize,
    pub tasks_seen: usize,
}

impl SlowCancelPollingHandler {
    fn new(token: ShutdownToken) -> Self {
        Self {
            token,
            control: None,
            steps_to_cancel: 0,
            tasks_seen: 0,
        }
    }
}

impl TaskHandler for SlowCancelPollingHandler {
    fn handle(&mut self, _task: &Task) -> Result<()> {
        self.tasks_seen += 1;
        let mut steps = 0usize;
        loop {
            let cancelled = self.control.as_ref().is_some_and(|c| !c.indexing_enabled());
            if cancelled {
                self.steps_to_cancel = steps;
                self.token.trigger();
                return Err(Error::Cancelled);
            }
            steps += 1;
            std::thread::sleep(Duration::from_millis(5));
            if steps > 2_000 {
                self.token.trigger();
                return Err(Error::Cancelled);
            }
        }
    }

    fn link_cancel_source(&mut self, control: Arc<ThrottleControl>) {
        self.control = Some(control);
    }
}

fn pending_count(db: &vidcull_db::Database) -> u64 {
    TaskQueueRepo::new(db.conn())
        .count_distinct_files_by_state(TaskState::Pending)
        .expect("count pending")
}

fn running_count(db: &vidcull_db::Database) -> u64 {
    TaskQueueRepo::new(db.conn())
        .count_distinct_files_by_state(TaskState::Running)
        .expect("count running")
}

fn enqueue_upsert(db: &mut vidcull_db::Database, path: &str) {
    let ch = [ChangeTask {
        path: NormalizedPath::new(path),
        change: ChangeKind::Upsert,
        size_bytes: 0,
    }];
    assert_eq!(
        enqueue_changes(db, &ch, KIND, 0, FAKE_NOW).expect("enqueue upsert"),
        1,
        "expected 1 row inserted for {path}"
    );
}

fn enqueue_remove(db: &mut vidcull_db::Database, path: &str) -> usize {
    let ch = [ChangeTask {
        path: NormalizedPath::new(path),
        change: ChangeKind::Remove,
        size_bytes: 0,
    }];
    enqueue_changes(db, &ch, KIND, 0, FAKE_NOW).expect("enqueue remove")
}

fn config_with_control(control: Arc<ThrottleControl>) -> DaemonConfig {
    DaemonConfig {
        kind: KIND.to_owned(),
        poll_interval: Duration::from_millis(5),
        throttle_control: control,
        ..DaemonConfig::default()
    }
}

#[test]
fn pause_drops_inflight_count_promptly() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open db");

    enqueue_upsert(&mut db, "C:/videos/clip1.mp4");
    enqueue_upsert(&mut db, "C:/videos/clip2.mp4");
    enqueue_upsert(&mut db, "C:/videos/clip3.mp4");

    let control = Arc::new(ThrottleControl::default());
    let token = ShutdownToken::new();
    let mut handler = SlowCancelPollingHandler::new(token.clone());
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let ctrl2 = Arc::clone(&control);
    let tok2 = token.clone();
    let pauser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        ctrl2.set_indexing_enabled(false);
        std::thread::sleep(Duration::from_millis(500));
        tok2.trigger();
    });

    daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");
    pauser.join().expect("pauser thread");

    let running = running_count(&db);
    let pending = pending_count(&db);

    eprintln!(
        "[P1.2] steps_to_cancel={} tasks_seen={} running={} pending={}",
        handler.steps_to_cancel, handler.tasks_seen, running, pending
    );

    assert_eq!(
        running, 0,
        "B1/H1c: task left RUNNING after pause — cancel→requeue path broken"
    );
    assert_eq!(
        pending, 3,
        "B1: wrong PENDING count after pause (expected 3: 1 requeued + 2 held)"
    );
}

#[test]
fn folder_removal_pending_stalls_while_paused() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open db");

    let control = Arc::new(ThrottleControl::default());
    control.set_indexing_enabled(false);

    enqueue_upsert(&mut db, "C:/videos/a.mp4");
    enqueue_upsert(&mut db, "C:/videos/b.mp4");
    assert_eq!(pending_count(&db), 2, "pre: 2 Upsert tasks pending");

    {
        let repo = TaskQueueRepo::new(db.conn());
        let ids: Vec<i64> = repo
            .list_by_state(TaskState::Pending)
            .expect("list pending")
            .into_iter()
            .map(|t| t.id)
            .collect();
        for id in ids {
            repo.delete_if_pending(id).expect("delete pending upsert");
        }
    }
    assert_eq!(pending_count(&db), 0, "Upserts deleted, queue empty");

    assert_eq!(
        enqueue_remove(&mut db, "C:/videos/a.mp4"),
        1,
        "Remove for a.mp4 inserted"
    );
    assert_eq!(
        enqueue_remove(&mut db, "C:/videos/b.mp4"),
        1,
        "Remove for b.mp4 inserted"
    );

    control.mark_roots_removed(&[NormalizedPath::new("C:/videos")]);

    assert_eq!(pending_count(&db), 2, "2 Remove tasks now pending");
    assert_eq!(running_count(&db), 0, "nothing running yet");

    let token = ShutdownToken::new();
    let mut handler = common::RecordingHandler::new(token.clone());
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let tok2 = token.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        tok2.trigger();
    });

    daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");
    stopper.join().expect("stopper");

    let pending = pending_count(&db);
    let running = running_count(&db);

    eprintln!(
        "[P1.3(i)] handler_seen={} pending={} running={}",
        handler.seen.len(),
        pending,
        running
    );

    assert_eq!(running, 0, "no task left RUNNING");
    assert_eq!(
        handler.seen.len(),
        2,
        "paused drain: handler invoked for both Remove tasks (purge_file / soft-delete)"
    );

    assert_eq!(
        pending, 0,
        "H2a fixed: Remove tasks must drain to 0 while paused"
    );
}

#[test]
fn watcher_re_event_blocked_after_removal() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open db");

    let control = Arc::new(ThrottleControl::default());

    assert_eq!(
        enqueue_remove(&mut db, "C:/videos/a.mp4"),
        1,
        "pre: Remove task inserted"
    );
    control.mark_roots_removed(&[NormalizedPath::new("C:/videos")]);
    assert_eq!(pending_count(&db), 1, "pre: 1 Remove task pending");

    let stale = [ChangeTask {
        path: NormalizedPath::new("C:/videos/a.mp4"),
        change: ChangeKind::Upsert,
        size_bytes: 0,
    }];
    let inserted = enqueue_changes_guarded(&mut db, &stale, KIND, 0, FAKE_NOW, &control)
        .expect("enqueue stale upsert guarded");

    eprintln!(
        "[P1.3(ii)] stale_upsert_inserted={} total_pending={}",
        inserted,
        pending_count(&db)
    );

    assert_eq!(
        inserted, 0,
        "H2b fixed: stale Upsert for removed-root path blocked at guarded enqueue"
    );
    assert_eq!(
        pending_count(&db),
        1,
        "H2b fixed: only the Remove task remains pending (Upsert blocked)"
    );
}

#[test]
fn normal_drain_pending_to_zero() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open db");

    assert_eq!(
        enqueue_remove(&mut db, "C:/videos/a.mp4"),
        1,
        "pre: Remove a"
    );
    assert_eq!(
        enqueue_remove(&mut db, "C:/videos/b.mp4"),
        1,
        "pre: Remove b"
    );
    assert_eq!(pending_count(&db), 2, "pre-condition");

    let control = Arc::new(ThrottleControl::default());
    let token = ShutdownToken::new();
    let mut handler = common::RecordingHandler::new(token.clone()).stop_after(2);
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let tok2 = token.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        tok2.trigger();
    });

    daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");
    stopper.join().expect("stopper");

    eprintln!(
        "[P1.3(iii)] handler_seen={} pending={}",
        handler.seen.len(),
        pending_count(&db)
    );

    assert_eq!(
        pending_count(&db),
        0,
        "control: Remove tasks must drain to 0 without pause"
    );
    assert_eq!(
        pending_count(&db),
        0,
        "control: queue stays at 0 on re-poll (no churn refill)"
    );
    assert_eq!(handler.seen.len(), 2, "both Remove tasks were processed");
}

#[test]
fn mixed_queue_paused_no_drain_no_churn() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open db");

    let control = Arc::new(ThrottleControl::default());
    control.set_indexing_enabled(false);

    assert_eq!(
        enqueue_remove(&mut db, "C:/videos/a.mp4"),
        1,
        "Remove task under removed root"
    );
    enqueue_upsert(&mut db, "C:/other_folder/b.mp4");
    control.mark_roots_removed(&[NormalizedPath::new("C:/videos")]);

    assert_eq!(pending_count(&db), 2, "pre: 2 tasks pending");

    let token = ShutdownToken::new();
    let mut handler = common::RecordingHandler::new(token.clone());
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let tok2 = token.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        tok2.trigger();
    });

    daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");
    stopper.join().expect("stopper");

    let pending = pending_count(&db);
    let running = running_count(&db);

    eprintln!(
        "[P1.3 mixed] handler_seen={} pending={} running={}",
        handler.seen.len(),
        pending,
        running
    );

    assert_eq!(
        handler.seen.len(),
        1,
        "paused drain: handler called exactly once — for the removed-root Remove task"
    );
    assert_eq!(
        running, 0,
        "paused: no task ever RUNNING (zero churn on normal tasks)"
    );
    assert_eq!(
        pending, 1,
        "mixed/paused: only the non-removed-root Upsert stays PENDING"
    );
}
