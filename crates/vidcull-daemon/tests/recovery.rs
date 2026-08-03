mod common;

use common::{FAKE_NOW, KIND, RecordingHandler, config, enqueue_scan};
use tempfile::tempdir;
use vidcull_daemon::{Daemon, Outcome, ShutdownToken};
use vidcull_db::repo::{TaskQueueRepo, TaskState};

fn pending_count(db: &vidcull_db::Database) -> usize {
    TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Pending)
        .expect("list pending")
        .len()
}

fn running_count(db: &vidcull_db::Database) -> usize {
    TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Running)
        .expect("list running")
        .len()
}

fn done_ids(db: &vidcull_db::Database) -> Vec<i64> {
    let mut ids: Vec<i64> = TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Done)
        .expect("list done")
        .into_iter()
        .map(|t| t.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn step_returns_none_when_queue_is_empty() {
    let dir = tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    let daemon = Daemon::new(config());
    let mut handler = RecordingHandler::new(ShutdownToken::new());

    assert!(
        daemon
            .step(&db, &mut handler, FAKE_NOW)
            .expect("step")
            .is_none()
    );
    assert!(handler.seen.is_empty());
}

#[test]
fn failing_handler_marks_task_failed_records_error_and_continues() {
    let dir = tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    let ids = enqueue_scan(&db, 3);
    let daemon = Daemon::new(config());
    let mut handler = RecordingHandler::new(ShutdownToken::new()).failing(vec![ids[1]]);

    let mut outcomes = Vec::new();
    while let Some(result) = daemon.step(&db, &mut handler, FAKE_NOW).expect("step") {
        outcomes.push((result.id, result.outcome));
    }

    assert_eq!(
        outcomes,
        vec![
            (ids[0], Outcome::Done),
            (ids[1], Outcome::Failed),
            (ids[2], Outcome::Done),
        ]
    );

    let repo = TaskQueueRepo::new(db.conn());
    let failed = repo.list_by_state(TaskState::Failed).expect("failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].id, ids[1]);
    assert!(
        failed[0]
            .last_error
            .as_deref()
            .expect("error recorded")
            .contains("synthetic failure"),
    );
    assert_eq!(repo.list_by_state(TaskState::Done).expect("done").len(), 2);
}

#[test]
fn crash_recovery_reprocesses_inflight_task_after_restart() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("av.db");

    let ids = {
        let db = vidcull_db::open_file(&db_path).expect("open run1");
        let ids = enqueue_scan(&db, 3);
        let claimed = TaskQueueRepo::new(db.conn())
            .dequeue_next(KIND, FAKE_NOW)
            .expect("dequeue")
            .expect("claimed");
        assert_eq!(claimed.state, TaskState::Running);
        ids
    };

    let mut db = vidcull_db::open_file(&db_path).expect("open run2");
    assert_eq!(running_count(&db), 1, "stale RUNNING survived the crash");

    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(3);
    let daemon = Daemon::new(config());
    let stats = daemon
        .run(&mut db, &mut handler, &token, || FAKE_NOW)
        .expect("run");

    assert_eq!(stats.recovered, 1, "the in-flight task was requeued");
    assert_eq!(stats.processed, 3);
    assert_eq!(stats.failed, 0);

    let mut unique = handler.seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 3, "each task processed exactly once");
    assert_eq!(done_ids(&db), {
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted
    });
    assert_eq!(pending_count(&db), 0);
    assert_eq!(running_count(&db), 0);
}

#[test]
fn graceful_shutdown_preserves_pending_tasks_across_restart() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("av.db");

    let ids = {
        let db = vidcull_db::open_file(&db_path).expect("open seed");
        enqueue_scan(&db, 5)
    };

    let run1_seen = {
        let mut db = vidcull_db::open_file(&db_path).expect("open run1");
        let token = ShutdownToken::new();
        let mut handler = RecordingHandler::new(token.clone()).stop_after(2);
        let daemon = Daemon::new(config());
        let stats = daemon
            .run(&mut db, &mut handler, &token, || FAKE_NOW)
            .expect("run1");

        assert_eq!(stats.recovered, 0);
        assert_eq!(stats.processed, 2);
        assert_eq!(pending_count(&db), 3);
        assert_eq!(running_count(&db), 0);
        handler.seen.clone()
    };
    assert_eq!(run1_seen.len(), 2);

    let db = vidcull_db::open_file(&db_path).expect("open run2");
    assert_eq!(
        Daemon::recover(&db).expect("recover"),
        0,
        "clean shutdown left nothing stale"
    );

    let daemon = Daemon::new(config());
    let mut handler = RecordingHandler::new(ShutdownToken::new());
    while daemon
        .step(&db, &mut handler, FAKE_NOW)
        .expect("step")
        .is_some()
    {}

    let mut all = run1_seen;
    all.extend(handler.seen.iter().copied());
    all.sort_unstable();
    let unique_len = {
        let mut dedup = all.clone();
        dedup.dedup();
        dedup.len()
    };
    assert_eq!(unique_len, 5, "no task processed twice");
    assert_eq!(all, {
        let mut sorted = ids;
        sorted.sort_unstable();
        sorted
    });
    assert_eq!(done_ids(&db).len(), 5);
}

#[test]
fn busy_task_requeue_and_backoff_retry() {
    use vidcull_core::Error;

    struct BusyHandler;
    impl vidcull_daemon::TaskHandler for BusyHandler {
        fn handle(&mut self, _task: &vidcull_db::repo::Task) -> vidcull_core::Result<()> {
            Err(Error::Busy("synthetic busy".to_owned()))
        }
    }

    let dir = tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    let ids = enqueue_scan(&db, 1);
    let daemon = Daemon::new(config());

    let mut handler = BusyHandler;

    let res = daemon.step(&db, &mut handler, FAKE_NOW).expect("step");
    assert!(res.is_none(), "Busy task step returns None");

    let repo = TaskQueueRepo::new(db.conn());
    let tasks = repo.list_by_state(TaskState::Pending).expect("pending");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, ids[0]);
    assert_eq!(tasks[0].enqueued_at, FAKE_NOW + 30);
    assert_eq!(tasks[0].attempts, 0);

    let res2 = daemon.step(&db, &mut handler, FAKE_NOW + 10).expect("step");
    assert!(res2.is_none(), "Future task step returns None");

    let tasks = repo.list_by_state(TaskState::Pending).expect("pending");
    assert_eq!(
        tasks[0].attempts, 0,
        "attempts should remain 0 when skipped"
    );

    let res3 = daemon.step(&db, &mut handler, FAKE_NOW + 35).expect("step");
    assert!(res3.is_none());

    let tasks = repo.list_by_state(TaskState::Pending).expect("pending");
    assert_eq!(tasks[0].enqueued_at, FAKE_NOW + 35 + 30);
}
