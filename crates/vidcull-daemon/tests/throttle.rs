mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{FAKE_NOW, KIND, RecordingHandler, enqueue_scan};
use tempfile::tempdir;
use vidcull_daemon::{
    Activity, Daemon, DaemonConfig, ShutdownToken, ThrottleConfig, ThrottleControl,
};
use vidcull_db::repo::{NewTask, TaskQueueRepo, TaskState};

fn config_with_huge_cooldown() -> DaemonConfig {
    DaemonConfig {
        kind: KIND.to_owned(),
        poll_interval: Duration::from_millis(5),
        throttle: ThrottleConfig {
            active_cooldown: Duration::from_secs(3_600),
            ..ThrottleConfig::default()
        },
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

#[test]
fn graceful_shutdown_wins_over_active_cooldown() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_scan(&db, 3);

    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(1);
    let daemon = Daemon::new(config_with_huge_cooldown());

    let stats = daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::UserActive,
        )
        .expect("run_throttled");

    assert_eq!(stats.processed, 1);
    assert_eq!(stats.failed, 0);
    assert_eq!(pending_count(&db), 2);
    assert_eq!(running_count(&db), 0);
}

#[test]
fn idle_activity_inserts_no_cooldown_and_drains() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_scan(&db, 3);

    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(3);
    let daemon = Daemon::new(config_with_huge_cooldown());

    let stats = daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");

    assert_eq!(stats.processed, 3);
    assert_eq!(pending_count(&db), 0);
    assert_eq!(running_count(&db), 0);
}

#[test]
fn activity_source_is_polled_once_per_task() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_scan(&db, 3);

    let polls = Arc::new(AtomicUsize::new(0));
    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(3);
    let daemon = Daemon::new(config_with_huge_cooldown());

    let probe = Arc::clone(&polls);
    let stats = daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            move || {
                probe.fetch_add(1, Ordering::SeqCst);
                Activity::Idle
            },
        )
        .expect("run_throttled");

    assert_eq!(stats.processed, 3);
    assert_eq!(polls.load(Ordering::SeqCst), 3);
}

#[test]
fn run_throttled_interleaves_the_starved_partial_band() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    let fresh = enqueue_scan(&db, 10);
    let partial: Vec<i64> = {
        let repo = TaskQueueRepo::new(db.conn());
        (0..2)
            .map(|_| {
                repo.enqueue(&NewTask {
                    kind: KIND.to_owned(),
                    priority: -200,
                    payload: None,
                    enqueued_at: FAKE_NOW,
                    size_bytes: 0,
                })
                .expect("enqueue partial")
            })
            .collect()
    };

    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(fresh.len() + partial.len());
    let daemon = Daemon::new(config_with_huge_cooldown());

    let stats = daemon
        .run_throttled(
            &mut db,
            &mut handler,
            &token,
            || FAKE_NOW,
            || Activity::Idle,
        )
        .expect("run_throttled");

    assert_eq!(stats.processed, 12);
    assert_eq!(pending_count(&db), 0);

    let first_partial = handler
        .seen
        .iter()
        .position(|id| partial.contains(id))
        .expect("a partial task ran");
    assert!(
        first_partial < fresh.len(),
        "partial band interleaved before foreground drained (first partial at {first_partial}, fresh count {})",
        fresh.len()
    );
    assert!(
        partial.iter().all(|id| handler.seen.contains(id)),
        "both partial-band tasks completed"
    );
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
fn paused_indexing_claims_nothing() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_scan(&db, 3);

    let control = Arc::new(ThrottleControl::default());
    control.set_indexing_enabled(false);

    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(3);
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let stop = token.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
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
    assert_eq!(stats.processed, 0, "a paused daemon processes nothing");
    assert_eq!(pending_count(&db), 3, "all tasks stay PENDING while paused");
    assert_eq!(running_count(&db), 0, "no task is left RUNNING");
}

#[test]
fn resuming_indexing_drains_the_queue() {
    let dir = tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    enqueue_scan(&db, 3);

    let control = Arc::new(ThrottleControl::default());
    control.set_indexing_enabled(false);

    let token = ShutdownToken::new();
    let mut handler = RecordingHandler::new(token.clone()).stop_after(3);
    let daemon = Daemon::new(config_with_control(Arc::clone(&control)));

    let resume = Arc::clone(&control);
    let resumer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        resume.set_indexing_enabled(true);
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

    resumer.join().expect("resumer");
    assert_eq!(stats.processed, 3, "the queue drains once indexing resumes");
    assert_eq!(pending_count(&db), 0);
    assert_eq!(running_count(&db), 0);
}
