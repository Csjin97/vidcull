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

fn small_file_count() -> usize {
    std::env::var("VIDCULL_STALL_SMALL_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12)
}

fn large_file_secs() -> u32 {
    std::env::var("VIDCULL_STALL_LARGE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
}

fn large_file_res() -> (u32, u32) {
    if let Ok(v) = std::env::var("VIDCULL_STALL_LARGE_RES") {
        if let Some((w, h)) = v.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                return (w, h);
            }
        }
    }
    (3840, 2160)
}

fn resolve_synth_ffmpeg() -> Option<FfmpegBinaries> {
    if let Some(dir) = std::env::var_os("VIDCULL_SYNTH_FFMPEG_DIR") {
        return Some(FfmpegBinaries::from_dir(Path::new(&dir)));
    }
    FfmpegBinaries::resolve().ok()
}

fn encode_clip(ffmpeg: &Path, out: &Path, duration_secs: u32, width: u32, height: u32) {
    let lavfi = format!("testsrc=duration={duration_secs}:size={width}x{height}:rate=30");
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
            "30",
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
    assert!(out.exists(), "encoded clip missing: {}", out.display());
}

#[derive(Debug, Clone, Copy)]
struct ResourceSample {
    t_ms: i64,
    cpu_permille: u64,
    active_decode_workers: u64,
    base_gate_in_use: u64,
    base_gate_cap: u64,
    decode_conc_in_use: u64,
    decode_conc_cap: u64,
    seq_read_in_use: u64,
}

fn parse_u64_field(line: &str, key: &str) -> Option<u64> {
    let needle = format!("{key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

const POLL_MS: u64 = 200;
const IDLE_GAP_THRESHOLD_MS: i64 = 500;
const HARD_TIMEOUT: Duration = Duration::from_secs(900);

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

    let large_secs = large_file_secs();
    let (large_w, large_h) = large_file_res();
    let small_count = small_file_count();
    eprintln!(
        "[220] encoding synthetic corpus into {}",
        watch_root.display()
    );
    let large = watch_root.join(format!("large_{large_w}x{large_h}_{large_secs}s.mp4"));
    encode_clip(bins.ffmpeg(), &large, large_secs, large_w, large_h);

    for i in 0..small_count {
        let small = watch_root.join(format!("small_{i:02}.mp4"));
        encode_clip(bins.ffmpeg(), &small, 3, 640, 360);
    }
    eprintln!("[220] corpus ready: 1 large + {small_count} small files");

    let log_buffer = LogBuffer::new(65536);
    let subscriber = tracing_subscriber::registry().with(log_buffer.layer());
    let _ = tracing::subscriber::set_global_default(subscriber);

    let db_path = corpus_dir.path().join("stall_220.db");
    let mut worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let bridge_db = Arc::new(Mutex::new(
        vidcull_db::open_file(&db_path).expect("open bridge db"),
    ));

    let task_kind = "scan".to_owned();
    let shutdown = ShutdownToken::new();

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler =
        IndexingHandler::new(handler_db, bins.clone(), unix_now).with_task_kind(task_kind.clone());

    let gate_observer = vidcull_daemon::indexing::DecodeGateObserver::default();
    handler.observe_gates(&gate_observer);
    let sampler_shutdown = shutdown.clone();
    let sampler_observer = gate_observer.clone();
    let sampler_thread = std::thread::spawn(move || {
        let collector = vidcull_daemon::metrics::MetricsCollector::new();
        loop {
            sampler_shutdown.wait_timeout(Duration::from_millis(500));
            if sampler_shutdown.is_triggered() {
                break;
            }
            let m = collector.sample();
            let g = sampler_observer.snapshot().unwrap_or_default();
            tracing::info!(
                stage = "resource",
                rss_bytes = m.rss_bytes,
                cpu_permille = m.cpu_permille,
                decode_conc_in_use = g.decode_conc_in_use,
                decode_conc_cap = g.decode_conc_cap,
                base_gate_in_use = g.base_gate_in_use,
                base_gate_cap = g.base_gate_cap,
                partial_gate_in_use = g.partial_gate_in_use,
                partial_gate_cap = g.partial_gate_cap,
                seq_read_in_use = g.seq_read_in_use,
                seq_read_cap = g.seq_read_cap,
                active_decode_workers = g.active_decode_workers,
                "resource sample",
            );
        }
    });

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
    eprintln!("[220] issuing ForceRescan over {root_str}");
    let _ = request_handler.handle(vidcull_ipc::Request::Action(Action::ForceRescan {
        path: root_str,
    }));

    let total_files = 1 + small_count;
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

    let daemon = Daemon::new(DaemonConfig::default());
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
    let mut last_seen_len = log_buffer.len();
    let mut last_progress_at = Instant::now();
    let mut max_idle_gap_ms: i64 = 0;
    let mut resource_samples: Vec<ResourceSample> = Vec::new();
    let mut busy_log_count: usize = 0;

    loop {
        std::thread::sleep(Duration::from_millis(POLL_MS));
        let repo = TaskQueueRepo::new(poll_db.conn());
        let pending = repo.count_by_state(TaskState::Pending).unwrap_or(0);
        let running = repo.count_by_state(TaskState::Running).unwrap_or(0);

        let cur_len = log_buffer.len();
        if cur_len > last_seen_len {
            let idle_gap =
                i64::try_from(last_progress_at.elapsed().as_millis()).unwrap_or(i64::MAX);
            max_idle_gap_ms = max_idle_gap_ms.max(idle_gap);
            last_progress_at = Instant::now();
        }
        last_seen_len = cur_len;

        if worker_thread.is_finished() || (pending == 0 && running == 0) {
            break;
        }
        assert!(
            start.elapsed() < HARD_TIMEOUT,
            "harness hard-timeout ({HARD_TIMEOUT:?}) exceeded — pending={pending} running={running}"
        );
    }
    max_idle_gap_ms = max_idle_gap_ms
        .max(i64::try_from(last_progress_at.elapsed().as_millis()).unwrap_or(i64::MAX));

    shutdown.trigger();
    let stats = worker_thread
        .join()
        .expect("worker thread panicked")
        .expect("run_throttled");
    sampler_thread.join().expect("sampler thread panicked");
    eprintln!(
        "[220] drained: processed={} failed={} recovered={}",
        stats.processed, stats.failed, stats.recovered
    );

    for record in log_buffer.snapshot(usize::MAX) {
        let line = format!("{} {}", record.target, record.message);
        if line.contains("stage=\"resource\"") || line.contains("stage=resource") {
            if let (Some(cpu), Some(adw), Some(bgi), Some(bgc), Some(dci), Some(dcc), Some(sru)) = (
                parse_u64_field(&line, "cpu_permille"),
                parse_u64_field(&line, "active_decode_workers"),
                parse_u64_field(&line, "base_gate_in_use"),
                parse_u64_field(&line, "base_gate_cap"),
                parse_u64_field(&line, "decode_conc_in_use"),
                parse_u64_field(&line, "decode_conc_cap"),
                parse_u64_field(&line, "seq_read_in_use"),
            ) {
                resource_samples.push(ResourceSample {
                    t_ms: record.timestamp_ms,
                    cpu_permille: cpu,
                    active_decode_workers: adw,
                    base_gate_in_use: bgi,
                    base_gate_cap: bgc,
                    decode_conc_in_use: dci,
                    decode_conc_cap: dcc,
                    seq_read_in_use: sru,
                });
            }
        }
        if line.contains("gate at capacity; requeueing with backoff") {
            busy_log_count += 1;
        }
    }

    println!("\n== GATE harness report ==");
    println!(
        "total wall time        : {:.1}s",
        start.elapsed().as_secs_f64()
    );
    println!(
        "max observed idle gap  : {max_idle_gap_ms} ms  (AC-220.1 threshold: {IDLE_GAP_THRESHOLD_MS} ms)"
    );
    println!("resource samples       : {}", resource_samples.len());
    println!("busy-gate log lines    : {busy_log_count}");
    println!(
        "processed/failed       : {}/{}",
        stats.processed, stats.failed
    );

    if let Some(peak) = resource_samples.iter().max_by_key(|s| s.decode_conc_in_use) {
        println!(
            "peak decode_conc       : {}/{}",
            peak.decode_conc_in_use, peak.decode_conc_cap
        );
    }
    if let Some(peak) = resource_samples.iter().max_by_key(|s| s.base_gate_in_use) {
        println!(
            "peak base_gate         : {}/{}  (active_decode_workers={})",
            peak.base_gate_in_use, peak.base_gate_cap, peak.active_decode_workers
        );
    }
    println!(
        "cpu_permille samples    : {:?}",
        resource_samples
            .iter()
            .map(|s| s.cpu_permille)
            .collect::<Vec<_>>()
    );
    println!(
        "seq_read_in_use samples : {:?}",
        resource_samples
            .iter()
            .map(|s| s.seq_read_in_use)
            .collect::<Vec<_>>()
    );
    if let Some(first_t) = resource_samples.first().map(|s| s.t_ms) {
        println!(
            "-- full DecodeGateSnapshot timeseries (t_rel_ms, cpu‰, active_decode_workers, base_gate, decode_conc, seq_read_in_use) --"
        );
        for s in &resource_samples {
            println!(
                "  t={:>7}  cpu={:>4}  adw={}  base={}/{}  conc={}/{}  seq_read={}",
                s.t_ms - first_t,
                s.cpu_permille,
                s.active_decode_workers,
                s.base_gate_in_use,
                s.base_gate_cap,
                s.decode_conc_in_use,
                s.decode_conc_cap,
                s.seq_read_in_use,
            );
        }
    }

    if max_idle_gap_ms > IDLE_GAP_THRESHOLD_MS {
        let saturated_conc = resource_samples
            .iter()
            .any(|s| s.decode_conc_cap > 0 && s.decode_conc_in_use >= s.decode_conc_cap);
        let holding_worker_active = resource_samples.iter().any(|s| s.active_decode_workers > 0);
        let cpu_active_during_gap = resource_samples
            .iter()
            .any(|s| s.active_decode_workers > 0 && s.cpu_permille > 100);
        let seq_read_active_during_gap = resource_samples.iter().any(|s| s.seq_read_in_use > 0);
        let verdict = if busy_log_count == 0 && saturated_conc {
            "h-permit (decode_conc saturated, busy-log=0)"
        } else if busy_log_count == 0 && holding_worker_active && cpu_active_during_gap {
            "h-longfile (worker holds a decode-gate slot, CPU-active, busy-log=0)"
        } else if busy_log_count == 0 && holding_worker_active && seq_read_active_during_gap {
            "h-io (worker holds a slot but CPU~0 with seq_read in flight — I/O-bound, busy-log=0)"
        } else if busy_log_count == 0 && holding_worker_active {
            "h-io (worker holds a decode-gate slot, CPU~0, busy-log=0 — I/O/seek/demux wait)"
        } else if busy_log_count == 0 {
            "UNCLASSIFIED (idle gap with no worker holding a decode-gate slot and busy-log=0 — \
             investigate outside the 3-way decode-gate model, e.g. DB/queue-claim layer)"
        } else {
            "gate-busy (non-zero busy-gate log lines observed — Error::Busy backoff churn, not a park)"
        };
        println!("VERDICT: idle gap > threshold — {verdict}");
    } else {
        println!(
            "VERDICT: no idle gap > {IDLE_GAP_THRESHOLD_MS}ms observed this run (AC-220.1 holds)"
        );
    }
}

#[test]
#[ignore = "GATE: multi-minute wall-clock (encodes + decodes a synthetic 4K corpus); \
            run explicitly with --ignored --nocapture. Requires a libx264-capable ffmpeg \
            (VIDCULL_SYNTH_FFMPEG_DIR override or PATH)."]
fn force_rescan_mixed_corpus_idle_gap_report() {
    main_test_body();
}
