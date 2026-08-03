use std::time::{Duration, Instant};

use vidcull_daemon::{ScanExecutor, ShutdownToken};
use vidcull_db::repo::{TaskQueueRepo, TaskState};

#[test]
fn scan_executor_enqueues_submitted_roots_off_thread() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    for name in ["a.mp4", "b.mkv", "nested/c.mov"] {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, b"stub").expect("write video");
    }
    std::fs::write(root.join("notes.txt"), b"stub").expect("write non-video");

    let db_path = root.join("scan.db");
    drop(vidcull_db::open_file(&db_path).expect("init db"));

    let shutdown = ShutdownToken::new();
    let executor = ScanExecutor::spawn(db_path.clone(), "scan".to_owned(), shutdown);
    executor.submit(vec![root.to_path_buf()], Vec::new());

    let deadline = Instant::now() + Duration::from_secs(10);
    let pending = loop {
        let db = vidcull_db::open_file(&db_path).expect("open db");
        let n = TaskQueueRepo::new(db.conn())
            .count_by_state(TaskState::Pending)
            .expect("count");
        drop(db);
        if n >= 3 || Instant::now() >= deadline {
            break n;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(
        pending, 3,
        "the executor enqueued exactly the three video files, off-thread"
    );

    drop(executor);
}
