use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing_subscriber::layer::SubscriberExt;
use vidcull_core::NormalizedPath;
use vidcull_core::Result;
use vidcull_daemon::indexing::IndexingHandler;
use vidcull_daemon::logbuf::LogBuffer;
use vidcull_daemon::{
    ChangeKind, ChangeTask, Daemon, DaemonConfig, ParallelWorkerConfig, ShutdownToken, TaskHandler,
    ThrottleControl, throttle::Activity, unix_now,
};
use vidcull_db::repo::{NewTask, Task, TaskQueueRepo, TaskState};
use vidcull_parser::fallback::FfmpegBinaries;

struct ParallelOnlyHandler {
    config: ParallelWorkerConfig,
}

impl TaskHandler for ParallelOnlyHandler {
    fn handle(&mut self, _task: &Task) -> Result<()> {
        unreachable!(
            "parallel-only test handler: the parallel worker path must never call \
             TaskHandler::handle() — it dispatches through its own IndexingWorker"
        );
    }

    fn as_parallel_worker(&self) -> Option<ParallelWorkerConfig> {
        Some(self.config.clone())
    }
}

#[test]
fn parallel_burst_completes_despite_one_permanently_busy_task() {
    let dir = tempfile::tempdir().expect("tempdir");

    let locked_path = dir.path().join("recording.mp4");
    std::fs::write(&locked_path, b"placeholder").expect("create locked file");
    #[cfg(windows)]
    let _lock_handle = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&locked_path)
            .expect("open exclusive lock handle")
    };

    let db_path = dir.path().join("no_progress.db");
    {
        let setup_db = vidcull_db::open_file(&db_path).expect("open setup db");
        let repo = TaskQueueRepo::new(setup_db.conn());
        const BASE: i64 = 1_700_000_000;

        let locked_change = ChangeTask {
            path: NormalizedPath::new(&locked_path),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        };
        repo.enqueue(&NewTask {
            kind: "scan".to_owned(),
            priority: 0,
            payload: Some(locked_change.to_payload().expect("encode locked change")),
            enqueued_at: BASE,
            size_bytes: 0,
        })
        .expect("enqueue locked task");

        const COMPLETABLE: usize = 5;
        for i in 0..COMPLETABLE {
            let change = ChangeTask {
                path: NormalizedPath::new(format!("Z:/gone/removed_{i}.mp4")),
                change: ChangeKind::Remove,
                size_bytes: 0,
            };
            repo.enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: 0,
                payload: Some(change.to_payload().expect("encode remove change")),
                enqueued_at: BASE,
                size_bytes: 0,
            })
            .expect("enqueue remove task");
        }
    }

    let log_buffer = LogBuffer::new(65536);
    let subscriber = tracing_subscriber::registry().with(log_buffer.layer());
    let _ = tracing::subscriber::set_global_default(subscriber);

    let bins = FfmpegBinaries::new(
        PathBuf::from("nonexistent-ffmpeg"),
        PathBuf::from("nonexistent-ffprobe"),
    );
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let config_source = IndexingHandler::new(handler_db, bins, unix_now);
    let config = config_source
        .as_parallel_worker()
        .expect("IndexingHandler must support the parallel path");

    let mut run_db = vidcull_db::open_file(&db_path).expect("open run db");
    let mut handler = ParallelOnlyHandler { config };

    let shutdown = ShutdownToken::new();
    let throttle_control = Arc::new(ThrottleControl::default());
    throttle_control.set_level(vidcull_ipc::CpuThrottle::Full);
    throttle_control.set_idle_workers(Some(2));
    let daemon_config = DaemonConfig {
        kind: "scan".to_owned(),
        throttle_control,
        ..DaemonConfig::default()
    };
    let daemon = Daemon::new(daemon_config);

    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || {
        daemon.run_throttled(
            &mut run_db,
            &mut handler,
            &worker_shutdown,
            unix_now,
            || Activity::UserActive,
        )
    });

    let poll_db = vidcull_db::open_file(&db_path).expect("open poll db");
    let poll_repo = TaskQueueRepo::new(poll_db.conn());
    let removes_deadline = Instant::now() + Duration::from_secs(10);
    let mut done_count = 0u64;
    while Instant::now() < removes_deadline {
        done_count = poll_repo.count_by_state(TaskState::Done).unwrap_or(0);
        if done_count >= 5 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        done_count, 5,
        "the 5 completable Remove tasks did not all finish within 10s — \
         unrelated to the HIGH-2 fix itself, this would indicate the queue \
         setup/dispatch broke"
    );

    let no_progress_deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_no_progress = false;
    while Instant::now() < no_progress_deadline {
        saw_no_progress = log_buffer
            .snapshot(usize::MAX)
            .iter()
            .any(|r| r.message.contains("no_progress"));
        if saw_no_progress {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    shutdown.trigger();
    let join_result = worker_thread.join();

    assert!(
        saw_no_progress,
        "HIGH-2 regression reproduced: no worker ever logged \
         reason=\"no_progress\" within 20s of the completable tasks \
         draining — a worker permanently parked on the locked file's \
         Busy cycle, blocking `thread::scope`'s join (and therefore the \
         whole burst) indefinitely instead of giving up and returning \
         control to the outer loop"
    );

    join_result
        .expect("worker thread panicked")
        .expect("run_throttled returned an error");
}
