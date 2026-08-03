mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FAKE_NOW, KIND};
use tempfile::tempdir;
use vidcull_core::types::NormalizedPath;
use vidcull_core::{Error, Result};
use vidcull_daemon::{
    Activity, ChangeKind, ChangeTask, Daemon, DaemonConfig, ShutdownToken, TaskHandler,
    ThrottleControl, enqueue_changes,
};
use vidcull_db::repo::{Task, TaskQueueRepo, TaskState};

struct CancellingHandler {
    token: ShutdownToken,
    seen: usize,
}

impl CancellingHandler {
    fn new(token: ShutdownToken) -> Self {
        Self { token, seen: 0 }
    }
}

impl TaskHandler for CancellingHandler {
    fn handle(&mut self, _task: &Task) -> Result<()> {
        self.seen += 1;
        self.token.trigger();
        Err(Error::Cancelled)
    }
}

fn config_with_control(control: Arc<ThrottleControl>) -> DaemonConfig {
    DaemonConfig {
        kind: KIND.to_owned(),
        poll_interval: Duration::from_millis(5),
        throttle_control: control,
        ..DaemonConfig::default()
    }
}

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

fn enqueue_change_under(db: &mut vidcull_db::Database, path: &str) {
    let changes = [ChangeTask {
        path: NormalizedPath::new(path),
        change: ChangeKind::Upsert,
        size_bytes: 0,
    }];
    let n = enqueue_changes(db, &changes, KIND, 0, FAKE_NOW).expect("enqueue change");
    assert_eq!(n, 1, "one change enqueued");
}

#[test]
fn folder_removal_skips_decode_and_drops_the_task() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_change_under(&mut db, "C:/videos/clip.mp4");

    let control = Arc::new(ThrottleControl::default());
    control.mark_roots_removed(&[NormalizedPath::new("C:/videos")]);

    let token = ShutdownToken::new();
    let mut handler = CancellingHandler::new(token.clone());
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let stop = token.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        stop.trigger();
    });

    let stats = daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");
    stopper.join().expect("stopper");

    assert_eq!(
        handler.seen, 0,
        "a removed-folder task is dropped before the handler decodes it"
    );
    assert_eq!(
        pending_count(&db),
        0,
        "the removed-folder task is dropped, not left pending (no churn)"
    );
    assert_eq!(running_count(&db), 0, "no task left RUNNING");
    assert_eq!(stats.processed, 0);
    assert_eq!(stats.failed, 0);
}

#[test]
fn pause_cancel_requeues_the_task_for_resume() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_change_under(&mut db, "C:/videos/clip.mp4");

    let control = Arc::new(ThrottleControl::default());

    let token = ShutdownToken::new();
    let mut handler = CancellingHandler::new(token.clone());
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");

    assert_eq!(handler.seen, 1, "the task was claimed exactly once");
    assert_eq!(
        pending_count(&db),
        1,
        "a pause cancel must requeue the task so it re-decodes on resume"
    );
    assert_eq!(running_count(&db), 0, "no task left RUNNING");
}
