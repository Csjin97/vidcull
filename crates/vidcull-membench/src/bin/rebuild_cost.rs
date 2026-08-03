use std::collections::BTreeSet;
use std::process::ExitCode;
use std::time::Instant;

use vidcull_core::FileId;
use vidcull_core::types::{Codec, NormalizedPath, Resolution, VideoDuration};
use vidcull_db::open_file;
use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile};
use vidcull_fingerprint::format::{self, FORMAT_VERSION, decode_tier2};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::near::{
    LshParams, NearEdge, plan_near_duplicates, plan_near_duplicates_incremental,
    rebuild_near_duplicate_groups, rebuild_near_duplicate_groups_incremental,
};
use vidcull_matcher::ranking::assign_best_copies;
use vidcull_matcher::whole::{
    WholeFileParams, plan_whole_file_matches, rebuild_whole_file_groups, scan_whole_file_candidates,
};
use vidcull_membench::{
    CountingAllocator, current_allocated, peak_allocated, reset_peak, splitmix64,
};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

const DELTA_BURST: usize = 16;

const WHOLE_SCENES: usize = 60;

const WHOLE_PERTURB: u32 = 3;

const RESCAN_TICK_DELTA: usize = 16;

const WHOLE_BLOCK_DEFAULT: usize = 10;
const WHOLE_BLOCK_SPARSE: usize = 100;

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn block_base(block: u64) -> u64 {
    let mut s = block.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    splitmix64(&mut s) | 1
}

fn mib(bytes: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / (1024.0 * 1024.0)
    }
}

fn seed(db: &mut vidcull_db::Database, n: usize) -> vidcull_core::Result<Vec<(FileId, u64)>> {
    let mut state = 0xB17E_0001u64;
    let mut items = Vec::with_capacity(n);
    db.transaction(|conn| {
        let files = FilesRepo::new(conn);
        let fps = FingerprintsRepo::new(conn);
        for i in 0..n {
            let id_i = i64::try_from(i).unwrap_or(i64::MAX);
            let path = format!("/m/{i:08}.mp4");
            let width = 640 + u32::try_from(i % 4).unwrap_or(0) * 640;
            let new_file = NewFile {
                path: NormalizedPath::new(&path),
                size_bytes: 1_000_000 + id_i,
                mtime_ns: MTIME,
                inode: None,
                content_hash: None,
                codec: Some(Codec::H265),
                container: None,
                duration: Some(VideoDuration::from_millis(60_000)),
                fps_x1000: None,
                bitrate_bps: Some(2_000_000 + id_i),
                resolution: Some(Resolution::new(width, width * 9 / 16)),
                first_seen_at: T0,
                last_seen_at: T0,
                ..Default::default()
            };
            let file_id = files.insert(&new_file)?;

            let slot = i % 10;
            let phash = if slot < 3 {
                let base = block_base(u64::try_from(i / 10).unwrap_or(0));
                flip_low_bits(base, u32::try_from(slot).unwrap_or(0) * 2)
            } else {
                splitmix64(&mut state) | 1
            };
            let fp = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H265,
                gop: GopSignature::from_durations(&[]),
                global_phash: phash,
            };
            let blob = format::encode_tier1(&fp)?;
            fps.upsert(&Fingerprint {
                file_id,
                tier1_global: blob,
                tier2_temporal: None,
                format_version: u32::from(FORMAT_VERSION),
                created_at: T0,
            })?;
            items.push((file_id, phash));
        }
        Ok(())
    })?;
    Ok(items)
}

fn gen_whole_scenes(i: usize) -> Vec<SceneHash> {
    let mut state = block_base(u64::try_from(i).unwrap_or(0));
    (0..WHOLE_SCENES)
        .map(|s| SceneHash {
            timestamp_ms: u64::try_from(s).unwrap_or(0) * 1000,
            phash: splitmix64(&mut state) | 1,
        })
        .collect()
}

fn seed_tier2_whole(
    db: &mut vidcull_db::Database,
    n: usize,
    block_size: usize,
) -> vidcull_core::Result<()> {
    let block_size = block_size.max(2);
    db.transaction(|conn| {
        let files = FilesRepo::new(conn);
        let fps = FingerprintsRepo::new(conn);
        let mut last_base_scenes: Vec<SceneHash> = Vec::new();
        for i in 0..n {
            let id_i = i64::try_from(i).unwrap_or(i64::MAX);
            let slot = i % block_size;
            let scenes = if slot == 1 && !last_base_scenes.is_empty() {
                last_base_scenes
                    .iter()
                    .map(|s| SceneHash {
                        timestamp_ms: s.timestamp_ms,
                        phash: flip_low_bits(s.phash, WHOLE_PERTURB),
                    })
                    .collect()
            } else {
                let sc = gen_whole_scenes(i / block_size * block_size + slot);
                if slot == 0 {
                    last_base_scenes.clone_from(&sc);
                }
                sc
            };

            let path = format!("/w/{i:08}.mp4");
            let new_file = NewFile {
                path: NormalizedPath::new(&path),
                size_bytes: 1_000_000 + id_i,
                mtime_ns: MTIME,
                inode: None,
                content_hash: None,
                codec: Some(Codec::H265),
                container: None,
                duration: Some(VideoDuration::from_millis(60_000)),
                fps_x1000: None,
                bitrate_bps: Some(2_000_000 + id_i),
                resolution: Some(Resolution::new(1280, 720)),
                first_seen_at: T0,
                last_seen_at: T0,
                ..Default::default()
            };
            let file_id = files.insert(&new_file)?;

            let tier1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H265,
                gop: GopSignature::from_durations(&[]),
                global_phash: 0xABCD_1234_5678_9F01,
            };
            let tier1_blob = format::encode_tier1(&tier1)?;
            let tier2_blob = format::encode_tier2(&Tier2Fingerprint { scenes })?;
            fps.upsert(&Fingerprint {
                file_id,
                tier1_global: tier1_blob,
                tier2_temporal: Some(tier2_blob),
                format_version: u32::from(FORMAT_VERSION),
                created_at: T0,
            })?;
        }
        Ok(())
    })
}

fn touch_tier2(
    db: &mut vidcull_db::Database,
    file_id: FileId,
    tick: usize,
) -> vidcull_core::Result<()> {
    db.transaction(|conn| {
        let fps = FingerprintsRepo::new(conn);
        let scenes = gen_whole_scenes(usize::try_from(file_id.0).unwrap_or(0) + tick * 97);
        let tier1 = Tier1Fingerprint {
            duration_ms: 60_000,
            codec: Codec::H265,
            gop: GopSignature::from_durations(&[]),
            global_phash: 0xABCD_1234_5678_9F01,
        };
        let tier1_blob = format::encode_tier1(&tier1)?;
        let tier2_blob = format::encode_tier2(&Tier2Fingerprint { scenes })?;
        fps.upsert(&Fingerprint {
            file_id,
            tier1_global: tier1_blob,
            tier2_temporal: Some(tier2_blob),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0 + i64::try_from(tick).unwrap_or(0) + 1,
        })
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct TickBreakdown {
    fetch_decode: std::time::Duration,
    scan_candidates: std::time::Duration,
    plan: std::time::Duration,
    candidate_pairs: usize,
    plan_matches: usize,
}

fn measure_tick_breakdown(
    db: &mut vidcull_db::Database,
    params: WholeFileParams,
) -> vidcull_core::Result<TickBreakdown> {
    use vidcull_db::repo::FingerprintsRepo as FpRepo;

    let t = Instant::now();
    let raw = FpRepo::new(db.conn()).list_active_tier2()?;
    let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::with_capacity(raw.len());
    for (file_id, blob) in raw {
        corpus.push((file_id, decode_tier2(&blob)?));
    }
    let fetch_decode = t.elapsed();

    let t = Instant::now();
    let candidates = scan_whole_file_candidates(&corpus, params);
    let scan_candidates = t.elapsed();
    let candidate_pairs = candidates.len();

    let t = Instant::now();
    let plan = plan_whole_file_matches(&candidates);
    let plan_elapsed = t.elapsed();
    let plan_matches = plan.matches.len();

    Ok(TickBreakdown {
        fetch_decode,
        scan_candidates,
        plan: plan_elapsed,
        candidate_pairs,
        plan_matches,
    })
}

#[allow(clippy::too_many_lines)]
fn rescan_wall_probe(
    db: &mut vidcull_db::Database,
    n: usize,
    block_size: usize,
    ticks: usize,
) -> vidcull_core::Result<()> {
    use vidcull_db::repo::FingerprintsRepo as FpRepo;

    println!();
    #[allow(clippy::cast_precision_loss)]
    let density_pct = 100.0 / block_size.max(1) as f64;
    println!(
        "== whole-file rescan-wall probe (n={n}, block_size={block_size} [density {density_pct:.1}%], ticks={ticks}, delta/tick={RESCAN_TICK_DELTA}) =="
    );
    let seed_start = Instant::now();
    seed_tier2_whole(db, n, block_size)?;
    println!(
        "seeded {n} files + tier2 fingerprints ({WHOLE_SCENES} scenes each, ~{} planted whole-file pairs) in {:.2?}",
        n / block_size.max(1),
        seed_start.elapsed()
    );

    let params = WholeFileParams::default();
    let mut prev_ids: Option<Vec<FileId>> = None;
    let mut delta0_ticks = 0usize;
    let mut rescan_wall_total = std::time::Duration::ZERO;
    let mut assign_wall_total = std::time::Duration::ZERO;
    let mut last_outcome = None;
    let mut breakdown_total = TickBreakdown::default();
    let mut max_candidate_pairs = 0usize;

    for tick in 0..ticks {
        let start = (tick * RESCAN_TICK_DELTA) % n.max(1);
        for j in 0..RESCAN_TICK_DELTA.min(n) {
            let idx = (start + j) % n;
            touch_tier2(db, FileId(i64::try_from(idx).unwrap_or(0) + 1), tick)?;
        }

        let ids_now = FpRepo::new(db.conn()).list_active_tier2_ids()?;
        if let Some(prev) = &prev_ids {
            if *prev == ids_now {
                delta0_ticks += 1;
            }
        }
        prev_ids = Some(ids_now);

        let bd = measure_tick_breakdown(db, params)?;
        breakdown_total.fetch_decode += bd.fetch_decode;
        breakdown_total.scan_candidates += bd.scan_candidates;
        breakdown_total.plan += bd.plan;
        max_candidate_pairs = max_candidate_pairs.max(bd.candidate_pairs);

        let t = Instant::now();
        let outcome = rebuild_whole_file_groups(db, params, T0 + i64::try_from(tick).unwrap_or(0))?;
        rescan_wall_total += t.elapsed();

        let t = Instant::now();
        assign_best_copies(db, T0 + i64::try_from(tick).unwrap_or(0))?;
        assign_wall_total += t.elapsed();

        debug_assert_eq!(
            outcome.groups_created, bd.plan_matches,
            "replica plan and persisted rebuild must agree on match count"
        );
        last_outcome = Some(outcome);
    }

    let ticks_u32 = u32::try_from(ticks).unwrap_or(1).max(1);
    #[allow(clippy::cast_precision_loss)]
    let delta0_ratio = delta0_ticks as f64 / ticks.max(1) as f64;
    let rescan_avg = rescan_wall_total / ticks_u32;
    let assign_avg = assign_wall_total / ticks_u32;
    let fetch_decode_avg = breakdown_total.fetch_decode / ticks_u32;
    let scan_avg = breakdown_total.scan_candidates / ticks_u32;
    let plan_avg = breakdown_total.plan / ticks_u32;

    println!(
        "rescan wall (rebuild_whole_file_groups, fused persisting call): total {rescan_wall_total:.2?} over {ticks} ticks, avg/tick {rescan_avg:.2?}"
    );
    println!(
        "  decomposed replica (read-only, same tick's data): fetch+decode avg {fetch_decode_avg:.2?} | scan_whole_file_candidates (LSH scan + verify_alignment, FUSED) avg {scan_avg:.2?} | plan avg {plan_avg:.2?}"
    );
    println!(
        "  candidate pairs (scan_whole_file_candidates output len): max over ticks = {max_candidate_pairs}, planted pairs ≈ {}",
        n / block_size.max(1)
    );
    if max_candidate_pairs > 0 {
        #[allow(clippy::cast_precision_loss)]
        let inflation = max_candidate_pairs as f64 / (n / block_size.max(1)).max(1) as f64;
        if inflation > 3.0 {
            println!(
                "  → candidate pairs are {inflation:.1}× the planted-pair count — LIKELY a synthetic-corpus\n    LSH band-collision artifact (candidate discovery finds far more pairs than were planted),\n    not evidence that verify_alignment's real per-pair cost is high. Diversify the bench corpus\n    before trusting the fused rescan-wall figure as a genuine per-file cost."
            );
        } else {
            println!(
                "  → candidate pairs track the planted-pair count ({inflation:.1}×) — verify_alignment's\n    per-pair cost is plausibly genuine (not an LSH collision-density artifact)."
            );
        }
    }
    println!(
        "assign_best_copies wall:                 total {assign_wall_total:.2?} over {ticks} ticks, avg/tick {assign_avg:.2?}"
    );
    if let Some(o) = last_outcome {
        println!(
            "last tick outcome: {} groups cleared, {} created, {} members, {} edges",
            o.groups_cleared, o.groups_created, o.members_added, o.edges_added
        );
    }
    println!(
        "tier2-delta-0 tick ratio (active id-SET unchanged tick-over-tick): {delta0_ratio:.2} ({delta0_ticks}/{ticks})",
    );
    println!(
        "  → this corpus only re-upserts existing ids (no insert/delete/soft-delete), so the\n    id-SET is invariant every tick by construction; the ratio above reports that honestly\n    (it is a structural ceiling on 'nothing changed', not a content-unchanged signal — every\n    tick here rewrites {RESCAN_TICK_DELTA} files' tier2 CONTENT via touch_tier2, which an\n    id-set-only skip signal would silently miss, exactly the §J gap the plan's B2 guard warns\n    about). A true content-aware delta-0 ratio requires the created_at/generation signal\n    (Option A/B in the plan), not measured by this probe."
    );
    if rescan_avg > assign_avg {
        let ratio = rescan_avg.as_secs_f64() / assign_avg.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "  → rescan wall dominates assign_best_copies wall by {ratio:.1}× at this scale (Q2 stage-share signal for B2 promotion) — but see the candidate-pair verdict above before trusting this as a real-corpus signal."
        );
    } else {
        let ratio = assign_avg.as_secs_f64() / rescan_avg.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "  → assign_best_copies wall dominates rescan wall by {ratio:.1}× at this scale (B2 promotion signal weak here)."
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn run(n: usize) -> vidcull_core::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("membench.db");
    let mut db = open_file(&db_path)?;

    println!("== §B full-rebuild cost probe (n={n}) ==");
    let seed_start = Instant::now();
    let items = seed(&mut db, n)?;
    println!(
        "seeded {n} files + tier1 fingerprints in {:.2?} (WAL file db at {})",
        seed_start.elapsed(),
        db_path.display()
    );

    let t = Instant::now();
    let plan = plan_near_duplicates(items.iter().copied(), LshParams::default());
    let plan_elapsed = t.elapsed();
    println!(
        "pure planner (no DB):          {plan_elapsed:.2?}  → {} groups, {} candidate pairs examined",
        plan.groups.len(),
        plan.candidate_pairs_examined,
    );

    let before = current_allocated();
    reset_peak();
    let t = Instant::now();
    let near = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    let near_elapsed = t.elapsed();

    let t = Instant::now();
    let best = assign_best_copies(&mut db, T0)?;
    let best_elapsed = t.elapsed();

    let peak = peak_allocated().saturating_sub(before);

    println!(
        "rebuild_near_duplicate_groups: {:.2?}  → {} groups, {} members, {} edges, {} skipped",
        near_elapsed,
        near.groups_created,
        near.members_added,
        near.edges_added,
        near.skipped_uninformative,
    );
    println!(
        "assign_best_copies:            {best_elapsed:.2?}  → {} updated, {} unchanged, {} without active members",
        best.groups_updated, best.groups_unchanged, best.groups_without_active_members,
    );
    println!(
        "total rebuild wall: {:.2?}   peak allocated during rebuild+assign: {:.1} MiB",
        near_elapsed + best_elapsed,
        mib(peak),
    );
    println!(
        "note: wall time is machine-dependent (reported, not goldened); peak bytes are deterministic"
    );

    let prev_edges: Vec<NearEdge> = plan
        .groups
        .iter()
        .flat_map(|g| g.edges.iter().copied())
        .collect();
    let delta = DELTA_BURST.min(items.len());
    let changed: BTreeSet<FileId> = items.iter().take(delta).map(|(id, _)| *id).collect();

    let t = Instant::now();
    let inc_plan = plan_near_duplicates_incremental(
        items.iter().copied(),
        &prev_edges,
        &changed,
        LshParams::default(),
    );
    let inc_plan_elapsed = t.elapsed();

    let before_inc = current_allocated();
    reset_peak();
    let t = Instant::now();
    let inc =
        rebuild_near_duplicate_groups_incremental(&mut db, LshParams::default(), T0, &changed)?;
    let inc_elapsed = t.elapsed();
    let inc_peak = peak_allocated().saturating_sub(before_inc);

    println!();
    println!("== incremental rebuild (delta = {delta} changed files) ==");
    println!(
        "pure incremental planner:      {inc_plan_elapsed:.2?}  → {} groups, {} candidate pairs examined",
        inc_plan.groups.len(),
        inc_plan.candidate_pairs_examined,
    );
    #[allow(clippy::cast_precision_loss)]
    let pair_reduction = if inc_plan.candidate_pairs_examined == 0 {
        f64::INFINITY
    } else {
        plan.candidate_pairs_examined as f64 / inc_plan.candidate_pairs_examined as f64
    };
    println!(
        "  candidate pairs: full {} → incremental {}  ({pair_reduction:.0}× fewer)",
        plan.candidate_pairs_examined, inc_plan.candidate_pairs_examined,
    );
    println!(
        "incremental DB rebuild:        {:.2?}  → {} groups, {} members, {} edges   peak {:.1} MiB",
        inc_elapsed,
        inc.groups_created,
        inc.members_added,
        inc.edges_added,
        mib(inc_peak),
    );
    println!(
        "note: the incremental grouping is identical to the full rebuild (proven by\n      vidcull-matcher near::tests + tests/near_incremental.rs); only the work differs"
    );

    if std::env::var("VIDCULL_MEMBENCH_SKIP_RESCAN").as_deref() == Ok("1") {
        println!();
        println!(
            "== rescan-wall scale series SKIPPED (VIDCULL_MEMBENCH_SKIP_RESCAN=1) ==\n   superseded by (H2: structural all-pairs candidate saturation, not a bench\n   artifact) — these numbers are not used for the B2 gate."
        );
    } else {
        run_rescan_series(n)?;
    }

    Ok(())
}

const RESCAN_SERIES_TICKS: usize = 5;

const RESCAN_SERIES_BUDGET_SECS: u64 = 60;

fn run_rescan_series(n: usize) -> vidcull_core::Result<()> {
    println!();
    println!(
        "== rescan-wall scale series (ticks/scale={RESCAN_SERIES_TICKS}, budget/scale={RESCAN_SERIES_BUDGET_SECS}s, stop-on-budget) =="
    );

    let candidate_scales = [250usize, 500, 1000];
    let mut last_completed_scale = None;
    let mut prev_wall_secs: Option<f64> = None;

    for &scale in &candidate_scales {
        let scale = scale.min(n.max(1));
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("rescan_series.db");
        let mut db = open_file(&db_path)?;

        let t = Instant::now();
        rescan_wall_probe(&mut db, scale, WHOLE_BLOCK_DEFAULT, RESCAN_SERIES_TICKS)?;
        let scale_wall = t.elapsed();
        let scale_wall_secs = scale_wall.as_secs_f64();

        if let Some(prev) = prev_wall_secs {
            let growth = scale_wall_secs / prev.max(f64::MIN_POSITIVE);
            println!(
                "  [scale series] n={scale}: total {scale_wall:.2?} for this scale point ({growth:.2}× the previous scale point's wall)"
            );
        } else {
            println!("  [scale series] n={scale}: total {scale_wall:.2?} for this scale point");
        }
        last_completed_scale = Some(scale);
        prev_wall_secs = Some(scale_wall_secs);

        if scale_wall.as_secs() > RESCAN_SERIES_BUDGET_SECS {
            println!(
                "  [scale series] n={scale} exceeded the {RESCAN_SERIES_BUDGET_SECS}s/scale budget — stopping the series here (not advancing to a larger scale). Scaling exponent above is the evidence to extrapolate from, not a further live measurement."
            );
            break;
        }
        if scale >= n {
            break;
        }
    }

    if let Some(scale) = last_completed_scale {
        println!();
        #[allow(clippy::cast_precision_loss)]
        let sparse_pct = 100.0 / WHOLE_BLOCK_SPARSE as f64;
        #[allow(clippy::cast_precision_loss)]
        let default_pct = 100.0 / WHOLE_BLOCK_DEFAULT as f64;
        println!(
            "== pair-density sensitivity: rerunning n={scale} at {sparse_pct:.1}% planted-pair density (sparse, was {default_pct:.1}%) =="
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("rescan_sparse.db");
        let mut db = open_file(&db_path)?;
        rescan_wall_probe(&mut db, scale, WHOLE_BLOCK_SPARSE, RESCAN_SERIES_TICKS)?;
        println!(
            "  → compare this run's 'candidate pairs' + rescan-wall avg/tick against the default-density\n    run at the same n={scale} above: if candidate pairs and wall both drop roughly in step with\n    planted-pair density, the cost is density-driven (synthetic-corpus artifact risk); if wall stays\n    similar despite far fewer planted pairs, the LSH scan itself (not verify_alignment call count)\n    dominates and the artifact risk is lower."
        );
    }

    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let n = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000usize);
    match run(n) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rebuild_cost failed: {e}");
            ExitCode::FAILURE
        }
    }
}
