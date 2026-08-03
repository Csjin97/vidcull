mod common;

use std::time::Duration;

use common::{FAKE_NOW, RecordingHandler, config, enqueue_scan};
use tempfile::tempdir;
use vidcull_daemon::{Daemon, ShutdownToken};
use vidcull_db::repo::{TaskQueueRepo, TaskState};

#[tokio::test]
async fn async_run_idles_until_external_shutdown() {
    let dir = tempdir().expect("tempdir");
    let db = vidcull_db::open_file(&dir.path().join("av.db")).expect("open");
    let token = ShutdownToken::new();
    let handler = RecordingHandler::new(token.clone());
    let daemon = Daemon::new(config());

    let trigger = token.clone();
    let signal = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.trigger();
    });

    let stats = daemon
        .run_async(db, handler, token, || FAKE_NOW)
        .await
        .expect("run_async");
    signal.await.expect("signal task");

    assert_eq!(stats.recovered, 0);
    assert_eq!(stats.processed, 0);
    assert_eq!(stats.failed, 0);
}

#[tokio::test]
async fn async_run_drains_queue_on_blocking_worker() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("av.db");
    let ids = {
        let db = vidcull_db::open_file(&db_path).expect("seed");
        enqueue_scan(&db, 3)
    };

    let db = vidcull_db::open_file(&db_path).expect("reopen");
    let token = ShutdownToken::new();
    let handler = RecordingHandler::new(token.clone()).stop_after(3);
    let daemon = Daemon::new(config());

    let stats = daemon
        .run_async(db, handler, token, || FAKE_NOW)
        .await
        .expect("run_async");
    assert_eq!(stats.processed, 3);

    let db = vidcull_db::open_file(&db_path).expect("verify");
    let repo = TaskQueueRepo::new(db.conn());
    let done: Vec<i64> = repo
        .list_by_state(TaskState::Done)
        .expect("done")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(done.len(), 3);
    for id in ids {
        assert!(done.contains(&id));
    }
}
