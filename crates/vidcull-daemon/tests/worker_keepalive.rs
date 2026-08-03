use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::layer::SubscriberExt;
use vidcull_daemon::indexing::IndexingHandler;
use vidcull_daemon::logbuf::LogBuffer;
use vidcull_daemon::{Daemon, DaemonConfig, ShutdownToken, unix_now};
use vidcull_db::repo::{TaskQueueRepo, TaskState};
use vidcull_ipc::{Action, RequestHandler};
use vidcull_parser::fallback::FfmpegBinaries;

fn resolve_synth_ffmpeg() -> Option<FfmpegBinaries> {
    if let Some(dir) = std::env::var_os("VIDCULL_SYNTH_FFMPEG_DIR") {
        return Some(FfmpegBinaries::from_dir(Path::new(&dir)));
    }
    FfmpegBinaries::resolve().ok()
}

fn encode_clip(ffmpeg: &Path, out: &Path, duration_secs: u32, size: u32) {
    let lavfi = format!("testsrc=duration={duration_secs}:size={size}x{size}:rate=15");
    let status = Command::new(ffmpeg)
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &lavfi,
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-an",
        ])
        .arg(out)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn ffmpeg for {}: {e}", out.display()));
    assert!(
        status.success(),
        "ffmpeg encode failed ({status}) for {}",
        out.display()
    );
}

#[allow(clippy::too_many_lines)]
fn main_test_body() {
    let Some(bins) = resolve_synth_ffmpeg() else {
        eprintln!(
            "SKIP: no ffmpeg resolvable for corpus encoding \
             (set VIDCULL_SYNTH_FFMPEG_DIR or put a libx264 ffmpeg on PATH)"
        );
        return;
    };
    let corpus_dir = tempfile::tempdir().expect("corpus tempdir");
    let watch_root = corpus_dir.path().to_path_buf();

    let slow = watch_root.join("slow.mp4");
    encode_clip(bins.ffmpeg(), &slow, 20, 96);
    let small_count = 8usize;
    for i in 0..small_count {
        let small = watch_root.join(format!("small_{i:02}.mp4"));
        encode_clip(bins.ffmpeg(), &small, 1, 48);
    }
    let total_files = 1 + small_count;
    eprintln!("[224-1c] corpus ready: 1 slow + {small_count} small files");

    let log_buffer = LogBuffer::new(65536);
    let subscriber = tracing_subscriber::registry().with(log_buffer.layer());
    let _ = tracing::subscriber::set_global_default(subscriber);

    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("VIDCULL_SEQ_READ_MAX", "1");
    }

    let db_path = corpus_dir.path().join("keepalive.db");
    let mut worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let bridge_db = Arc::new(Mutex::new(
        vidcull_db::open_file(&db_path).expect("open bridge db"),
    ));

    let task_kind = "scan".to_owned();
    let shutdown = ShutdownToken::new();

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler =
        IndexingHandler::new(handler_db, bins.clone(), unix_now).with_task_kind(task_kind.clone());

    let remover: Arc<dyn vidcull_daemon::delete::FileRemover> =
        Arc::new(vidcull_daemon::delete::OsFileRemover);
    let request_handler = vidcull_daemon::bridge::DaemonRequestHandler::new(
        Arc::clone(&bridge_db),
        shutdown.clone(),
        LogBuffer::new(8),
        task_kind,
        remover,
    );
    let root_str = watch_root.to_str().expect("utf8 path").to_owned();
    let _ = request_handler.handle(vidcull_ipc::Request::Action(Action::ForceRescan {
        path: root_str,
    }));

    {
        let pending = TaskQueueRepo::new(bridge_db.lock().unwrap().conn())
            .list_by_state(TaskState::Pending)
            .expect("list pending after enqueue");
        assert!(
            pending.len() >= total_files,
            "force-rescan enqueued {} tasks, expected >= {total_files}",
            pending.len()
        );
    }

    let throttle_control = Arc::new(vidcull_daemon::ThrottleControl::default());
    throttle_control.set_level(vidcull_ipc::CpuThrottle::Full);
    let daemon_config = DaemonConfig {
        throttle_control,
        ..DaemonConfig::default()
    };
    let daemon = Daemon::new(daemon_config);
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || {
        daemon.run_throttled(
            &mut worker_db,
            &mut handler,
            &worker_shutdown,
            unix_now,
            || vidcull_daemon::throttle::Activity::UserActive,
        )
    });

    let poll_db = vidcull_db::open_file(&db_path).expect("open poll db");
    let start = Instant::now();
    const HARD_TIMEOUT: Duration = Duration::from_secs(45);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let repo = TaskQueueRepo::new(poll_db.conn());
        let pending = repo.count_by_state(TaskState::Pending).unwrap_or(0);
        let running = repo.count_by_state(TaskState::Running).unwrap_or(0);
        if pending == 0 && running == 0 {
            break;
        }
        if worker_thread.is_finished() {
            break;
        }
        if start.elapsed() > HARD_TIMEOUT {
            break;
        }
    }
    let elapsed = start.elapsed();

    shutdown.trigger();
    let stats = worker_thread
        .join()
        .expect("worker thread panicked")
        .expect("run_throttled");

    let repo = TaskQueueRepo::new(poll_db.conn());
    let done = repo
        .count_distinct_files_by_state(TaskState::Done)
        .unwrap_or(0);
    let pending_left = repo.count_by_state(TaskState::Pending).unwrap_or(0);
    let running_left = repo.count_by_state(TaskState::Running).unwrap_or(0);

    eprintln!(
        "[224-1c] elapsed={:.1}s processed={} failed={} done_files={} pending_left={} running_left={}",
        elapsed.as_secs_f64(),
        stats.processed,
        stats.failed,
        done,
        pending_left,
        running_left,
    );

    assert_eq!(
        pending_left + running_left,
        0,
        "not fully drained within {HARD_TIMEOUT:?} (elapsed {elapsed:?}) — a file's task was \
         orphaned behind a busy-backoff with no worker left alive to retry it \
         (the exact GATE-224 #10 failure mode this fix addresses)"
    );

    assert!(elapsed < HARD_TIMEOUT, "drain exceeded the hard timeout");

    let lifecycle_lines: Vec<String> = log_buffer
        .snapshot(usize::MAX)
        .into_iter()
        .filter(|r| r.message.contains("worker_lifecycle") || r.target.contains("worker_lifecycle"))
        .map(|r| format!("{} {}", r.target, r.message))
        .collect();
    eprintln!(
        "[224-1c] worker_lifecycle log lines: {}",
        lifecycle_lines.len()
    );
    assert!(
        !lifecycle_lines.is_empty(),
        "expected worker_lifecycle debug lines (Phase 0 instrumentation) — none captured"
    );
    assert!(
        lifecycle_lines.iter().any(|l| l.contains("kind_drained")),
        "expected at least one worker to exit via kind_drained (genuine empty-queue exit)"
    );
}

#[test]
#[ignore = "-1c regression: needs a real ffmpeg (libx264) and several wall-clock seconds; \
            run explicitly with --ignored --nocapture."]
fn workers_survive_busy_backoff_and_fully_drain_the_corpus() {
    main_test_body();
}
