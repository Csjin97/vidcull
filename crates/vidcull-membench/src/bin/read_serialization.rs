use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use vidcull_core::Result;
use vidcull_core::types::{Codec, NormalizedPath, Resolution, VideoDuration};
use vidcull_db::open_file;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, RegroupQueueRepo,
};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

const DEFAULT_CONCURRENCY_LADDER: &[usize] = &[1, 4, 6, 8];
const DEFAULT_ITERATIONS: usize = 5;
const DEFAULT_GROUPS: usize = 200;
const MEMBERS_PER_GROUP: usize = 3;

const INTERACTIVITY_BUDGET_MS: f64 = 100.0;
const MUTEX_WAIT_THRESHOLD_MS: f64 = 50.0;

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

#[derive(Debug, Clone, Copy)]
struct ReadSample {
    total_us: u64,
    mutex_wait_us: u64,
}

fn populate_db(db: &mut vidcull_db::Database, n_groups: usize) -> Result<Vec<i64>> {
    let fp = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H265,
        gop: GopSignature::from_durations(&[]),
        global_phash: 0xDEAD_BEEF_CAFE_F00D,
    };
    let tier1_blob = format::encode_tier1(&fp)?;

    let mut group_ids = Vec::with_capacity(n_groups);
    for g in 0..n_groups {
        let gid = {
            let groups_repo = DuplicateGroupsRepo::new(db.conn());
            groups_repo.create(vidcull_db::repo::TrustLevel::VeryLikely, T0)?
        };
        group_ids.push(gid);
        for m in 0..MEMBERS_PER_GROUP {
            let idx = i64::try_from(g * MEMBERS_PER_GROUP + m).unwrap_or(i64::MAX);
            let path = format!("/bench/{g:05}/{m}.mp4");
            let new_file = NewFile {
                path: NormalizedPath::new(&path),
                size_bytes: 10_000_000 + idx,
                mtime_ns: MTIME,
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
            let file_id = db.transaction(|conn| {
                let files = FilesRepo::new(conn);
                let fid = files.insert(&new_file)?;
                FingerprintsRepo::new(conn).upsert(&Fingerprint {
                    file_id: fid,
                    tier1_global: tier1_blob.clone(),
                    tier2_temporal: None,
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: T0,
                })?;
                RegroupQueueRepo::new(conn).mark(fid, T0)?;
                Ok(fid)
            })?;
            {
                let groups_repo = DuplicateGroupsRepo::new(db.conn());
                groups_repo.add_member(gid, file_id)?;
            }
        }
    }
    Ok(group_ids)
}

fn read_group_detail(db_arc: &Arc<Mutex<vidcull_db::Database>>, group_id: i64) -> ReadSample {
    let t_before_lock = Instant::now();
    let db = db_arc.lock().unwrap_or_else(PoisonError::into_inner);
    let t_after_lock = Instant::now();

    let groups = DuplicateGroupsRepo::new(db.conn());
    let files = FilesRepo::new(db.conn());
    let _best = groups
        .get(group_id)
        .ok()
        .flatten()
        .and_then(|g| g.best_file_id);
    let members = groups.list_members(group_id).unwrap_or_default();
    let mut _count = 0usize;
    for fid in &members {
        if files.get(*fid).unwrap_or(None).is_some() {
            _count += 1;
        }
    }
    let t_after_query = Instant::now();
    drop(db);

    let total_us = u64::try_from(t_before_lock.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mutex_wait_us =
        u64::try_from(t_after_lock.duration_since(t_before_lock).as_micros()).unwrap_or(0);
    let _ = t_after_query;
    ReadSample {
        total_us,
        mutex_wait_us,
    }
}

fn read_list_groups(db_arc: &Arc<Mutex<vidcull_db::Database>>) -> ReadSample {
    let t_before_lock = Instant::now();
    let db = db_arc.lock().unwrap_or_else(PoisonError::into_inner);
    let t_after_lock = Instant::now();

    let groups = DuplicateGroupsRepo::new(db.conn());
    let page = groups.list_page(None, 20, 0).unwrap_or_default();
    let ids: Vec<i64> = page.iter().map(|g| g.id).collect();
    let _counts = groups.member_counts(&ids).unwrap_or_default();
    let t_done = Instant::now();
    drop(db);

    let total_us = u64::try_from(t_before_lock.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mutex_wait_us =
        u64::try_from(t_after_lock.duration_since(t_before_lock).as_micros()).unwrap_or(0);
    let _ = t_done;
    ReadSample {
        total_us,
        mutex_wait_us,
    }
}

fn run_concurrent_reads(
    db_arc: &Arc<Mutex<vidcull_db::Database>>,
    group_ids: &[i64],
    concurrency: usize,
    reads_per_thread: usize,
) -> Vec<ReadSample> {
    let n_groups = group_ids.len();
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(concurrency);
        for t in 0..concurrency {
            let db_clone = Arc::clone(db_arc);
            let ids_slice = group_ids.to_vec();
            handles.push(s.spawn(move || -> Vec<ReadSample> {
                let mut samples = Vec::with_capacity(reads_per_thread * 2);
                for r in 0..reads_per_thread {
                    let gid = ids_slice[(t * reads_per_thread + r) % n_groups];
                    samples.push(read_group_detail(&db_clone, gid));
                    samples.push(read_list_groups(&db_clone));
                }
                samples
            }));
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

#[allow(clippy::cast_precision_loss)]
fn stats(mut values: Vec<u64>) -> (f64, u64, u64, u64, u64) {
    if values.is_empty() {
        return (0.0, 0, 0, 0, 0);
    }
    values.sort_unstable();
    let n = values.len();
    let mean = values.iter().copied().sum::<u64>() as f64 / n as f64;
    let p50 = values[n / 2];
    let p95 = values[(n * 95) / 100];
    let p99 = values[(n * 99) / 100];
    let max = *values.last().unwrap();
    (mean, p50, p95, p99, max)
}

#[allow(clippy::cast_precision_loss)]
fn run(concurrency_ladder: &[usize], iterations: usize, n_groups: usize) -> Result<()> {
    println!("== W2-C read_serialization: DB mutex 직렬화 측정 ==");
    println!(
        "corpus: {} groups × {} members/group = {} files",
        n_groups,
        MEMBERS_PER_GROUP,
        n_groups * MEMBERS_PER_GROUP
    );
    println!("iterations per concurrency level: {iterations}");
    println!(
        "DEFER 부등식: read_p95 < {INTERACTIVITY_BUDGET_MS}ms OR mutex_wait_p95 < {MUTEX_WAIT_THRESHOLD_MS}ms"
    );
    println!(
        "NOTE: 고아 데몬(vidcull-daemon) 프로세스가 실행 중이면 CPU 경합으로 측정값이 높아질 수 있습니다."
    );
    println!();

    for &conc in concurrency_ladder {
        let reads_per_thread = 40usize.max(200 / conc.max(1));

        let mut iter_total_p95s: Vec<f64> = Vec::with_capacity(iterations);
        let mut iter_wait_p95s: Vec<f64> = Vec::with_capacity(iterations);
        let mut all_total: Vec<u64> = Vec::new();
        let mut all_wait: Vec<u64> = Vec::new();

        for iter in 0..iterations {
            let dir = tempfile::tempdir().expect("tempdir");
            let db_path = dir.path().join("read_serialization.db");
            let mut db = open_file(&db_path)?;
            let group_ids = populate_db(&mut db, n_groups)?;
            let db_arc: Arc<Mutex<vidcull_db::Database>> = Arc::new(Mutex::new(db));

            let samples = run_concurrent_reads(&db_arc, &group_ids, conc, reads_per_thread);

            let total_us: Vec<u64> = samples.iter().map(|s| s.total_us).collect();
            let wait_us: Vec<u64> = samples.iter().map(|s| s.mutex_wait_us).collect();

            let (_, _, tp95, _, _) = stats(total_us.clone());
            let (_, _, wp95, _, _) = stats(wait_us.clone());
            iter_total_p95s.push(tp95 as f64);
            iter_wait_p95s.push(wp95 as f64);
            all_total.extend(total_us);
            all_wait.extend(wait_us);

            if iterations > 1 {
                print!(
                    "  iter {}/{}: total_p95={:.2}ms  wait_p95={:.2}ms\r",
                    iter + 1,
                    iterations,
                    tp95 as f64 / 1000.0,
                    wp95 as f64 / 1000.0
                );
            }
        }
        println!();

        let (t_mean, t_p50, t_p95, t_p99, t_max) = stats(all_total);
        let (w_mean, w_p50, w_p95, w_p99, w_max) = stats(all_wait);

        let t_p95_mean = iter_total_p95s.iter().sum::<f64>() / iter_total_p95s.len() as f64;
        let t_p95_stddev = {
            let var = iter_total_p95s
                .iter()
                .map(|v| (v - t_p95_mean).powi(2))
                .sum::<f64>()
                / iter_total_p95s.len() as f64;
            var.sqrt()
        };
        let w_p95_mean = iter_wait_p95s.iter().sum::<f64>() / iter_wait_p95s.len() as f64;
        let w_p95_stddev = {
            let var = iter_wait_p95s
                .iter()
                .map(|v| (v - w_p95_mean).powi(2))
                .sum::<f64>()
                / iter_wait_p95s.len() as f64;
            var.sqrt()
        };

        let q_p95_ms = (t_p95 as f64 - w_p95 as f64).max(0.0) / 1000.0;

        let t_p95_ms = t_p95 as f64 / 1000.0;
        let w_p95_ms = w_p95 as f64 / 1000.0;

        let defer_budget = t_p95_ms < INTERACTIVITY_BUDGET_MS;
        let defer_wait = w_p95_ms < MUTEX_WAIT_THRESHOLD_MS;
        let verdict = if defer_budget || defer_wait {
            "DEFER"
        } else {
            "GO (측정 정당화)"
        };

        println!("─── concurrency = {conc} ───────────────────────────────────────");
        println!(
            "  total (lock+query)  | mean={:.2}ms  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms",
            t_mean / 1000.0,
            t_p50 as f64 / 1000.0,
            t_p95_ms,
            t_p99 as f64 / 1000.0,
            t_max as f64 / 1000.0
        );
        println!(
            "  mutex-wait 성분     | mean={:.2}ms  p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms",
            w_mean / 1000.0,
            w_p50 as f64 / 1000.0,
            w_p95_ms,
            w_p99 as f64 / 1000.0,
            w_max as f64 / 1000.0
        );
        println!("  query 성분(추정)    | p95 ≈ {q_p95_ms:.2}ms  (= total_p95 - wait_p95)");
        println!(
            "  per-iter p95 variance | total: {:.2}±{:.2}ms  wait: {:.2}±{:.2}ms",
            t_p95_mean / 1000.0,
            t_p95_stddev / 1000.0,
            w_p95_mean / 1000.0,
            w_p95_stddev / 1000.0
        );
        println!(
            "  DEFER 부등식        | budget({INTERACTIVITY_BUDGET_MS}ms): {}  wait({MUTEX_WAIT_THRESHOLD_MS}ms): {}",
            if defer_budget {
                "DEFER ✓"
            } else {
                "초과 ✗"
            },
            if defer_wait {
                "DEFER ✓"
            } else {
                "초과 ✗"
            }
        );
        println!("  판정                | {verdict}");
        println!();
    }

    println!("== 요약 ==");
    println!(
        "임계값(사용자/architect 합의 필요): interactivity_budget={INTERACTIVITY_BUDGET_MS}ms, mutex_wait_threshold={MUTEX_WAIT_THRESHOLD_MS}ms"
    );
    println!("§6.3 DEFER 부등식: read_p95 < budget OR mutex_wait_p95 < threshold → DEFER");
    println!("GO 는 두 조건 모두 초과 시에만. pool 구현은 GO + 사용자 승인 후.");

    Ok(())
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let concurrency: Option<usize> = args.get(1).and_then(|s| s.parse().ok());
    let iterations: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let n_groups: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_GROUPS);

    let ladder: Vec<usize> = if let Some(c) = concurrency {
        vec![1, c]
    } else {
        DEFAULT_CONCURRENCY_LADDER.to_vec()
    };

    match run(&ladder, iterations, n_groups) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("read_serialization failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
