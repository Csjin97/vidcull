use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vidcull_core::types::{Blake3Hash, NormalizedPath};
use vidcull_daemon::metrics::MetricsCollector;
use vidcull_daemon::{
    Activity, ChangeKind, ChangeTask, Daemon, DaemonConfig, IndexingHandler, ShutdownToken,
    ThrottleConfig, ThrottleControl, enqueue_changes,
};
use vidcull_db::repo::{FilesRepo, NewFile, TaskQueueRepo, TaskState};
use vidcull_ipc::CpuThrottle;
use vidcull_synth::FfmpegBinaries;

const NOW: i64 = 1_700_000_000;
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_DECODE_BUDGET: usize = 150;
const DEFAULT_DRAIN_SECS: u64 = 2 * 60 * 60;

fn now() -> i64 {
    NOW
}

fn mib(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / (1024.0 * 1024.0)
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn collect_fixtures(dir: &Path, max_bytes: u64) -> Vec<PathBuf> {
    fn is_video(p: &Path) -> bool {
        matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("mp4" | "mkv" | "mov" | "webm" | "avi" | "ts" | "m4v" | "wmv")
        )
    }
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && is_video(&path) && meta.len() <= max_bytes {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

struct RunResult {
    wall: Duration,
    baseline_rss: u64,
    peak_rss: u64,
    done: usize,
    failed: usize,
}

fn run_partial_backfill(
    fixtures: &[PathBuf],
    workers: usize,
    decode_budget: usize,
    bins: &FfmpegBinaries,
) -> RunResult {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("bench.db");

    {
        let mut db = vidcull_db::open_file(&db_path).expect("open seed db");
        {
            let files = FilesRepo::new(db.conn());
            for (i, path) in fixtures.iter().enumerate() {
                let mut hash = [0u8; 32];
                hash[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                files
                    .insert(&NewFile {
                        path: NormalizedPath::new(path),
                        size_bytes: i64::try_from(size).unwrap_or(i64::MAX),
                        content_hash: Some(Blake3Hash::from_bytes(hash)),
                        first_seen_at: NOW,
                        last_seen_at: NOW,
                        ..Default::default()
                    })
                    .expect("seed file row");
            }
        }
        let changes: Vec<ChangeTask> = fixtures
            .iter()
            .map(|p| ChangeTask {
                path: NormalizedPath::new(p),
                change: ChangeKind::PartialFingerprint,
                size_bytes: 0,
            })
            .collect();
        enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue partial tasks");
    }

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let handler = IndexingHandler::new(handler_db, bins.clone(), now)
        .with_partial_clips(true)
        .with_budget(decode_budget);

    let throttle_control = Arc::new(ThrottleControl::default());
    throttle_control.set_level(CpuThrottle::Full);
    let config = DaemonConfig {
        kind: "scan".to_owned(),
        poll_interval: Duration::from_millis(50),
        throttle: ThrottleConfig {
            idle_workers: workers,
            ..Default::default()
        },
        throttle_control,
    };
    let daemon = Daemon::new(config);

    let token = ShutdownToken::new();
    let run_token = token.clone();
    let worker_db_path = db_path.clone();
    let start = Instant::now();
    let join = std::thread::spawn(move || {
        let mut worker_db = vidcull_db::open_file(&worker_db_path).expect("open worker db");
        let mut handler = handler;
        daemon
            .run_throttled(&mut worker_db, &mut handler, &run_token, now, || {
                Activity::Idle
            })
            .expect("run_throttled")
    });

    let collector = MetricsCollector::new();
    let baseline_rss = collector.sample().rss_bytes;
    let mut peak_rss = baseline_rss;
    let verify = vidcull_db::open_file(&db_path).expect("open verify db");
    let drain_timeout = Duration::from_secs(env_u64("AV_PARTIAL_DRAIN_SECS", DEFAULT_DRAIN_SECS));
    let deadline = Instant::now() + drain_timeout;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        peak_rss = peak_rss.max(collector.sample().rss_bytes);
        let repo = TaskQueueRepo::new(verify.conn());
        let pending = repo.count_by_state(TaskState::Pending).expect("pending");
        let running = repo.count_by_state(TaskState::Running).expect("running");
        if pending == 0 && running == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "partial backfill did not drain within {drain_timeout:?} (pending={pending} running={running})"
        );
    }
    let wall = start.elapsed();
    token.trigger();
    let _stats = join.join().expect("join worker");
    peak_rss = peak_rss.max(collector.sample().rss_bytes);

    let repo = TaskQueueRepo::new(verify.conn());
    let done = repo.count_by_state(TaskState::Done).expect("done");
    let failed = repo.count_by_state(TaskState::Failed).expect("failed");
    RunResult {
        wall,
        baseline_rss,
        peak_rss,
        done: usize::try_from(done).unwrap_or(0),
        failed: usize::try_from(failed).unwrap_or(0),
    }
}

#[test]
#[ignore = "measurement: needs ffmpeg + AV_PARTIAL_FIXTURES (real videos); minutes-long"]
#[allow(clippy::cast_precision_loss)]
fn partial_backfill_concurrency_measurement() {
    vidcull_core::init_tracing();
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP partial_backfill_concurrency_measurement: ffmpeg not resolvable");
        return;
    };
    let Some(dir) = std::env::var_os("AV_PARTIAL_FIXTURES") else {
        eprintln!("SKIP partial_backfill_concurrency_measurement: set AV_PARTIAL_FIXTURES=<dir>");
        return;
    };
    let max_bytes = env_u64("AV_PARTIAL_MAX_BYTES", DEFAULT_MAX_BYTES);
    let decode_budget =
        usize::try_from(env_u64("AV_PARTIAL_BUDGET", DEFAULT_DECODE_BUDGET as u64)).unwrap_or(150);

    let fixtures = collect_fixtures(Path::new(&dir), max_bytes);
    if fixtures.is_empty() {
        eprintln!(
            "SKIP partial_backfill_concurrency_measurement: no video files <= {} MiB under {}",
            mib(max_bytes),
            Path::new(&dir).display()
        );
        return;
    }
    let n = fixtures.len();
    eprintln!(
        "partial backfill measurement: {n} files (<= {} MiB each), decode_budget={decode_budget}",
        mib(max_bytes),
    );
    for p in &fixtures {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "  - {} ({:.0} MiB)",
            p.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            mib(size)
        );
    }

    eprintln!("\n[serial] 1 worker (gate capacity 1) …");
    let serial = run_partial_backfill(&fixtures, 1, decode_budget, &bins);
    eprintln!("[concurrent] {} workers (gate capacity {n}) …", n + 1);
    let concurrent = run_partial_backfill(&fixtures, n + 1, decode_budget, &bins);

    let speedup = serial.wall.as_secs_f64() / concurrent.wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let serial_growth = serial.peak_rss.saturating_sub(serial.baseline_rss);
    let conc_growth = concurrent.peak_rss.saturating_sub(concurrent.baseline_rss);

    eprintln!("\n=== partial backfill A/B (files={n}, decode_budget={decode_budget}) ===");
    eprintln!(
        "serial     (gate cap 1):  wall={:>7.1}s  done={}/{n} (failed {})  peak_rss={:>7.1} MiB  (baseline {:.1} MiB, +{:.1} MiB)",
        serial.wall.as_secs_f64(),
        serial.done,
        serial.failed,
        mib(serial.peak_rss),
        mib(serial.baseline_rss),
        mib(serial_growth),
    );
    eprintln!(
        "concurrent (gate cap {n}):  wall={:>7.1}s  done={}/{n} (failed {})  peak_rss={:>7.1} MiB  (baseline {:.1} MiB, +{:.1} MiB)",
        concurrent.wall.as_secs_f64(),
        concurrent.done,
        concurrent.failed,
        mib(concurrent.peak_rss),
        mib(concurrent.baseline_rss),
        mib(conc_growth),
    );
    eprintln!(
        "→ drain speedup = {speedup:.2}×   peak-RSS-growth ratio (conc/serial) = {:.2}×",
        conc_growth as f64 / (serial_growth.max(1)) as f64,
    );

    assert_eq!(
        serial.done + serial.failed,
        n,
        "serial run resolved every partial (done {} + failed {} != {n})",
        serial.done,
        serial.failed,
    );
    assert_eq!(
        concurrent.done + concurrent.failed,
        n,
        "concurrent run resolved every partial (done {} + failed {} != {n})",
        concurrent.done,
        concurrent.failed,
    );
    assert!(
        serial.done > 0 && concurrent.done > 0,
        "at least one partial decoded successfully in each run"
    );
}
