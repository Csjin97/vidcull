use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use vidcull_core::types::{Codec, NormalizedPath, Resolution, VideoDuration};
use vidcull_db::open_file;
use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile, RegroupQueueRepo};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::hash_reader;
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

const DEFAULT_TOTAL: usize = 30_000;

const DEFAULT_CPU_KIB: usize = 512;

const TIER2_BYTES: usize = 4 * 1024;

struct Payload {
    tier1: Vec<u8>,
    tier2: Vec<u8>,
}

impl Payload {
    fn build() -> vidcull_core::Result<Self> {
        let fp = Tier1Fingerprint {
            duration_ms: 60_000,
            codec: Codec::H265,
            gop: GopSignature::from_durations(&[]),
            global_phash: 0xDEAD_BEEF_CAFE_F00D,
        };
        Ok(Self {
            tier1: format::encode_tier1(&fp)?,
            tier2: vec![0x5Au8; TIER2_BYTES],
        })
    }
}

fn write_task(
    db: &mut vidcull_db::Database,
    idx: i64,
    payload: &Payload,
) -> vidcull_core::Result<()> {
    let path = format!("/m/{idx:09}.mp4");
    let new_file = NewFile {
        path: NormalizedPath::new(&path),
        size_bytes: 1_000_000 + idx,
        mtime_ns: MTIME,
        inode: None,
        content_hash: None,
        codec: Some(Codec::H265),
        container: Some("mp4".to_owned()),
        duration: Some(VideoDuration::from_millis(60_000)),
        fps_x1000: Some(30_000),
        bitrate_bps: Some(2_000_000 + idx),
        resolution: Some(Resolution::new(1920, 1080)),
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    db.transaction(|conn| {
        let file_id = FilesRepo::new(conn).insert(&new_file)?;
        FingerprintsRepo::new(conn).upsert(&Fingerprint {
            file_id,
            tier1_global: payload.tier1.clone(),
            tier2_temporal: Some(payload.tier2.clone()),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })?;
        RegroupQueueRepo::new(conn).mark(file_id, T0)?;
        Ok(())
    })
}

fn run_workers(
    db_path: &std::path::Path,
    workers: usize,
    total: usize,
    cpu_kib: usize,
    payload: &Payload,
) -> vidcull_core::Result<std::time::Duration> {
    let next = Arc::new(AtomicI64::new(0));
    let total_i = i64::try_from(total).unwrap_or(i64::MAX);
    let cpu_buf = vec![0xA5u8; cpu_kib * 1024];

    let start = Instant::now();
    std::thread::scope(|s| -> vidcull_core::Result<()> {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let next = Arc::clone(&next);
            let cpu_buf = &cpu_buf;
            handles.push(s.spawn(move || -> vidcull_core::Result<()> {
                let mut db = open_file(db_path)?;
                db.conn()
                    .busy_timeout(std::time::Duration::from_secs(120))
                    .map_err(|e| vidcull_core::Error::Database(e.to_string()))?;
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= total_i {
                        break;
                    }
                    if cpu_kib > 0 {
                        let h = hash_reader(&cpu_buf[..])?;
                        std::hint::black_box(h.as_bytes()[0]);
                    }
                    write_task(&mut db, idx, payload)?;
                }
                Ok(())
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked")?;
        }
        Ok(())
    })?;
    Ok(start.elapsed())
}

fn worker_ladder(cores: usize) -> Vec<usize> {
    let mut ns = vec![1usize];
    let mut p = 2;
    while p < cores {
        ns.push(p);
        p *= 2;
    }
    ns.push(cores);
    ns.sort_unstable();
    ns.dedup();
    ns
}

#[allow(clippy::cast_precision_loss)]
fn sweep(label: &str, total: usize, cpu_kib: usize, ladder: &[usize], payload: &Payload) {
    println!("\n== {label} (total tasks = {total}, cpu_kib = {cpu_kib}) ==");
    println!("  N  |    wall    |   tasks/sec  | speedup | efficiency");
    println!("-----+------------+--------------+---------+-----------");
    let mut base_rate = 0.0f64;
    for (i, &n) in ladder.iter().enumerate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("worker_scaling.db");
        drop(open_file(&db_path).expect("migrate db"));
        let elapsed = run_workers(&db_path, n, total, cpu_kib, payload).expect("run workers");
        let rate = total as f64 / elapsed.as_secs_f64();
        if i == 0 {
            base_rate = rate;
        }
        let speedup = rate / base_rate;
        let efficiency = speedup / n as f64;
        println!(
            "{n:>4} | {:>9.3?} | {rate:>10.0}/s | {speedup:>6.2}× | {:>8.0}%",
            elapsed,
            efficiency * 100.0,
        );
    }
}

fn run(total: usize, cpu_kib: usize) -> vidcull_core::Result<()> {
    let cores = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    let ladder = worker_ladder(cores);
    let payload = Payload::build()?;

    println!("== worker-scale indexing throughput probe ==");
    println!(
        "machine cores (available_parallelism) = {cores}; worker ladder = {ladder:?}\n\
         per-task write = files.insert + fingerprints.upsert(tier1 {}B + tier2 {}B) + regroup.mark\n\
         WAL pragmas: journal_mode=WAL, synchronous=NORMAL, busy_timeout=5000, IMMEDIATE tx\n\
         note: tasks/sec is machine-dependent (reported, not goldened); DB content is deterministic",
        payload.tier1.len(),
        payload.tier2.len(),
    );

    sweep("db-write ceiling (write only)", total, 0, &ladder, &payload);
    sweep(
        "tiny CPU (512 KiB hash) + write",
        total,
        cpu_kib,
        &ladder,
        &payload,
    );
    let decode_like_total = (total / 8).max(2_000);
    let decode_like_kib = 8 * 1024;
    sweep(
        "decode-like CPU (8 MiB hash) + write",
        decode_like_total,
        decode_like_kib,
        &ladder,
        &payload,
    );

    println!(
        "\nread the gate: the db-write ceiling row that stops rising is the most tasks/sec the\n\
         single WAL writer can pass, regardless of worker count. Compare it to the per-worker\n\
         indexing rate implied by the §S1/§A decode costs (docs/benchmarks.md): if N·rate stays\n\
         below the ceiling across 1..=cores, the writer never binds and the worker slider gives\n\
         real gains; if the ceiling sits at/under a few workers' worth, the slider misleads.\n\
         The decode-like curve is the realistic regime: per-task work in the ms range, where the\n\
         <0.15 ms serial write is negligible and throughput tracks worker count up to cores."
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let total = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TOTAL);
    let cpu_kib = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CPU_KIB);
    match run(total, cpu_kib) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("worker_scaling failed: {e}");
            ExitCode::FAILURE
        }
    }
}
