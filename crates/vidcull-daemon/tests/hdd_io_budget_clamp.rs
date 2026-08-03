use std::sync::Arc;
use std::time::{Duration, Instant};

use vidcull_daemon::{
    Activity, Daemon, DaemonConfig, DecodeGateObserver, IndexingHandler, ShutdownToken,
    ThrottleConfig, ThrottleControl,
};
use vidcull_ipc::CpuThrottle;
use vidcull_synth::FfmpegBinaries;

const NOW: i64 = 1_700_000_000;
fn now() -> i64 {
    NOW
}

fn observed_partial_gate_cap(io_cap: usize, idle_workers: usize) -> usize {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("hdd_clamp_238.db");

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
    let mut handler = IndexingHandler::new(handler_db, bins, now);
    let observer = DecodeGateObserver::default();
    handler.observe_gates(&observer);

    let throttle_control = Arc::new(ThrottleControl::default());
    throttle_control.set_level(CpuThrottle::Full);
    throttle_control.set_idle_workers(Some(idle_workers));
    throttle_control.set_io_budget_cap(io_cap);

    let config = DaemonConfig {
        kind: "scan".to_owned(),
        poll_interval: Duration::from_millis(20),
        throttle: ThrottleConfig::default(),
        throttle_control,
    };
    let daemon = Daemon::new(config);

    let shutdown = ShutdownToken::new();
    let run_shutdown = shutdown.clone();
    let mut worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let join = std::thread::spawn(move || {
        daemon
            .run_throttled(&mut worker_db, &mut handler, &run_shutdown, now, || {
                Activity::Idle
            })
            .expect("run_throttled")
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cap = 0usize;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let snap = observer.snapshot().unwrap_or_default();
        if snap.partial_gate_cap > 1 {
            cap = snap.partial_gate_cap;
            break;
        }
    }

    shutdown.trigger();
    let _ = join.join().expect("worker thread panicked");
    assert!(
        cap > 1,
        "the worker loop never published a burst budget within the deadline \
         (io_cap={io_cap}, idle_workers={idle_workers}); partial_gate_cap stayed \
         at its construction default",
    );
    cap
}

#[test]
fn hdd_cap_clamps_decode_budget_ssd_path_unchanged() {
    let unclamped = observed_partial_gate_cap(0, 8);
    assert_eq!(
        unclamped, 7,
        "SSD path (io_budget_cap unset) must keep the full 8-worker budget \
         (partial_gate_cap = budget - 1 = 7)",
    );

    let clamped = observed_partial_gate_cap(4, 8);
    assert_eq!(
        clamped, 3,
        "HDD path (io_budget_cap = 4) must clamp the 8-worker budget to 4 \
         (partial_gate_cap = 4 - 1 = 3)",
    );

    assert!(
        clamped < unclamped,
        "the HDD clamp must reduce concurrent decode workers ({clamped} < {unclamped})",
    );
}

#[test]
fn hdd_cap_at_or_above_budget_is_a_noop() {
    let cap_above = observed_partial_gate_cap(8, 4);
    assert_eq!(
        cap_above, 3,
        "a cap above the budget must not raise concurrency (budget stays 4, \
         partial_gate_cap = 3)",
    );
}
