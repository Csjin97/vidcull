use std::collections::BTreeSet;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use vidcull_core::FileId;
use vidcull_core::types::{Codec, NormalizedPath, Resolution, VideoDuration};
use vidcull_db::open_file;
use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
    rebuild_partial_clip_groups_from_fingerprints,
};
use vidcull_matcher::partial::{
    AnchorParams, ClipMatch, DEFAULT_SHARD_SOURCES, PartialClipPlan, partial_clip_params,
    plan_partial_clips_incremental, plan_partial_clips_scoped, rebuild_partial_clip_groups,
    rebuild_partial_clip_groups_incremental,
};
use vidcull_membench::{
    CountingAllocator, current_allocated, peak_allocated, reset_peak, splitmix64,
};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

const SCENES: usize = 60;
const CLIP_START: usize = 20;
const CLIP_LEN: usize = 6;
const CLIP_PERTURB: u32 = 4;

const DELTA_BURST: usize = 16;

const DEFAULT_FULL_CAP: usize = 10_000;

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn mib(bytes: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / (1024.0 * 1024.0)
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn gen_source_scenes(seed: u64, n: usize) -> Vec<SceneHash> {
    let mut state = seed;
    (0..n)
        .map(|i| SceneHash {
            timestamp_ms: u64::try_from(i).unwrap_or(0) * 1000,
            phash: splitmix64(&mut state) | 1,
        })
        .collect()
}

fn gen_clip_scenes(source: &[SceneHash], start: usize, len: usize, perturb: u32) -> Vec<SceneHash> {
    source[start..start + len]
        .iter()
        .enumerate()
        .map(|(i, s)| SceneHash {
            timestamp_ms: u64::try_from(i).unwrap_or(0) * 1000,
            phash: flip_low_bits(s.phash, perturb),
        })
        .collect()
}

fn video_seed(i: usize) -> u64 {
    let mut s = u64::try_from(i)
        .unwrap_or(u64::MAX)
        .wrapping_add(1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    splitmix64(&mut s) | 1
}

struct Seeded {
    corpus: Vec<(FileId, Tier2Fingerprint)>,
    clip_ids: Vec<FileId>,
    source_ids: Vec<FileId>,
}

fn seed(
    db: &mut vidcull_db::Database,
    n: usize,
    clips_per_source: usize,
) -> vidcull_core::Result<Seeded> {
    let tier1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H265,
        gop: GopSignature::from_durations(&[]),
        global_phash: 0xABCD_1234_5678_9F01,
    };
    let tier1_blob = format::encode_tier1(&tier1)?;

    let clips_per_source = clips_per_source.max(1);
    let block = clips_per_source + 1;

    let mut corpus = Vec::with_capacity(n);
    let mut clip_ids = Vec::new();
    let mut source_ids = Vec::new();

    db.transaction(|conn| {
        let files = FilesRepo::new(conn);
        let fps = FingerprintsRepo::new(conn);
        let mut last_source0: Vec<SceneHash> = Vec::new();
        for i in 0..n {
            let id_i = i64::try_from(i).unwrap_or(i64::MAX);
            let slot = i % block;

            let (scenes, is_clip, is_source) = if slot >= 1
                && last_source0.len() > CLIP_START + CLIP_LEN
            {
                let start = (CLIP_START + slot - 1) % last_source0.len().saturating_sub(CLIP_LEN);
                (
                    gen_clip_scenes(&last_source0, start, CLIP_LEN, CLIP_PERTURB),
                    true,
                    false,
                )
            } else {
                let sc = gen_source_scenes(video_seed(i), SCENES);
                let is_source = slot == 0;
                if is_source {
                    last_source0.clone_from(&sc);
                }
                (sc, false, is_source)
            };

            let path = format!("/m/{i:08}.mp4");
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

            let fp = Tier2Fingerprint { scenes };
            let blob = format::encode_tier2(&fp)?;
            fps.upsert(&Fingerprint {
                file_id,
                tier1_global: tier1_blob.clone(),
                tier2_temporal: Some(blob),
                format_version: u32::from(FORMAT_VERSION),
                created_at: T0,
            })?;

            if is_clip {
                clip_ids.push(file_id);
            }
            if is_source {
                source_ids.push(file_id);
            }
            corpus.push((file_id, fp));
        }
        Ok(())
    })?;

    Ok(Seeded {
        corpus,
        clip_ids,
        source_ids,
    })
}

fn timed_incremental(
    corpus: &[(FileId, Tier2Fingerprint)],
    prev: &[ClipMatch],
    changed: &BTreeSet<FileId>,
) -> (PartialClipPlan, Duration) {
    let t = Instant::now();
    let plan = plan_partial_clips_incremental(
        corpus,
        prev,
        changed,
        AnchorParams::default(),
        DEFAULT_SHARD_SOURCES,
    );
    (plan, t.elapsed())
}

fn measure_full_rebuild(
    db: &mut vidcull_db::Database,
    params: AnchorParams,
    corpus: &[(FileId, Tier2Fingerprint)],
    n: usize,
    full_cap: usize,
) -> vidcull_core::Result<Vec<ClipMatch>> {
    if n > full_cap {
        println!();
        println!(
            "== full rebuild SKIPPED (n={n} > full_cap={full_cap}) ==\n   full is O(N²·scenes) — intractable at this scale; that cost is *why* the\n   rebuild is incremental. Incremental runs cold (no prior POSSIBLE edges)."
        );
        return Ok(Vec::new());
    }

    let t = Instant::now();
    let plan = plan_partial_clips_scoped(corpus, params, DEFAULT_SHARD_SOURCES);
    let plan_elapsed = t.elapsed();
    println!();
    println!("== full rebuild (every clip × every source) ==");
    println!(
        "pure scoped planner:           {plan_elapsed:.2?}  → {} matches, {} candidate offsets examined, {} short",
        plan.matches.len(),
        plan.candidate_offsets_examined,
        plan.skipped_short,
    );

    let before = current_allocated();
    reset_peak();
    let t = Instant::now();
    let full = rebuild_partial_clip_groups(db, params, T0)?;
    let full_elapsed = t.elapsed();
    let peak = peak_allocated().saturating_sub(before);
    println!(
        "rebuild_partial_clip_groups:   {full_elapsed:.2?}  → {} groups, {} members, {} edges   peak {:.1} MiB",
        full.groups_created,
        full.members_added,
        full.edges_added,
        mib(peak),
    );
    Ok(plan.matches)
}

const N_CAND_SAMPLE: usize = 64;

fn measure_durable(
    corpus: &[(FileId, Tier2Fingerprint)],
    params: AnchorParams,
    source_changed: &BTreeSet<FileId>,
    clip_sweep: Duration,
) -> usize {
    let before = current_allocated();
    reset_peak();
    let t = Instant::now();
    let mut durable = PartialClipIndex::new(params);
    durable.bootstrap(corpus.iter().cloned());
    let boot_elapsed = t.elapsed();
    let boot_peak = peak_allocated().saturating_sub(before);
    let boot_retained = current_allocated().saturating_sub(before);

    {
        let sample = corpus.iter().take(N_CAND_SAMPLE);
        let mut total_cands: u64 = 0;
        let mut total_queries: u64 = 0;
        for (_id, fp) in sample {
            for scene in &fp.scenes {
                if scene.phash != 0 {
                    total_cands +=
                        u64::try_from(durable.mih_candidates_count(scene.phash)).unwrap_or(0);
                    total_queries += 1;
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let avg = if total_queries > 0 {
            total_cands as f64 / total_queries as f64
        } else {
            0.0
        };
        println!();
        println!(
            "== MIH candidate-count sweep (sample={} files, {} scene queries) ==",
            corpus.len().min(N_CAND_SAMPLE),
            total_queries,
        );
        let corpus_n = corpus.len();
        println!("total candidates: {total_cands}  avg per scene query: {avg:.1}  (n={corpus_n})");
    }

    let by_id: std::collections::BTreeMap<FileId, &Tier2Fingerprint> =
        corpus.iter().map(|(id, fp)| (*id, fp)).collect();
    let t = Instant::now();
    for id in source_changed {
        if let Some(fp) = by_id.get(id) {
            durable.upsert(*id, fp.scenes.clone());
        }
    }
    durable.rediscover(source_changed);
    let durable_burst = t.elapsed();
    let durable_examined = durable.plan().candidate_offsets_examined;

    println!();
    println!("== durable index (B-fix-3 ② fix: query only the changed files) ==");
    println!(
        "bootstrap (full reconcile):    {boot_elapsed:.2?}  → {} matches   peak {:.1} MiB / retained {:.1} MiB",
        durable.matches().len(),
        mib(boot_peak),
        mib(boot_retained),
    );
    println!(
        "source-changed burst:          {durable_burst:.2?}  → {durable_examined} offsets examined  [(C) replaced: {} changed-source queries, not {} clip queries]",
        source_changed.len(),
        corpus.len(),
    );
    let sweep_secs = secs(clip_sweep).max(f64::MIN_POSITIVE);
    let speedup = sweep_secs / secs(durable_burst).max(f64::MIN_POSITIVE);
    println!(
        "  → durable burst vs incremental (C) sweep: {speedup:.0}× faster (sweep {clip_sweep:.2?} → durable {durable_burst:.2?})",
    );
    boot_retained
}

fn measure_cold_db_build(
    db: &mut vidcull_db::Database,
    params: AnchorParams,
    n: usize,
    full_cap: usize,
    pure_retained: usize,
) -> vidcull_core::Result<()> {
    println!();
    if n > full_cap {
        println!(
            "== paged cold DB build SKIPPED (n={n} > full_cap={full_cap}) ==\n   genuine first-plan discovery is O(N²·scenes) — intractable at this scale; paging\n   bounds the *memory*, not the one-time discovery cost. Run at a tractable n to read\n   the paged peak."
        );
        return Ok(());
    }

    let empty = BTreeSet::new();
    let before = current_allocated();
    reset_peak();
    let t = Instant::now();
    let mut index = PartialClipIndex::new(params);
    let outcome = rebuild_partial_clip_groups_durable(&mut index, db, T0, &empty)?;
    let elapsed = t.elapsed();
    let peak = peak_allocated().saturating_sub(before);
    let retained = current_allocated().saturating_sub(before);

    println!("== paged cold DB build (B6: bounded-memory genuine first plan) ==");
    println!(
        "rebuild_..._durable (cold):    {elapsed:.2?}  → {} groups, {} members   peak {:.1} MiB / retained {:.1} MiB",
        outcome.groups_created,
        outcome.members_added,
        mib(peak),
        mib(retained),
    );
    println!(
        "  → resident (retained) after build: paged {:.1} MiB vs pure in-memory bootstrap {:.1} MiB.\n    This is the B6 target (\"상주 메모리\"): the pure bootstrap keeps the whole-corpus scene\n    map + MIH resident — O(N), ~700 MiB @ 100k×60 (docs/benchmarks.md §B-fix-4) — while the\n    paged build keeps NONE of it (the corpus lives in the DB; the only resident state is the\n    match set). Peak is comparable at this small n because the whole corpus is ~one page's\n    worth; the paged peak stays bounded by page + candidates + matches as N grows, whereas the\n    pure resident set grows linearly with the corpus.",
        mib(retained),
        mib(pure_retained),
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn run(n: usize, full_cap: usize, clips_per_source: usize) -> vidcull_core::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("partial_membench.db");
    let mut db = open_file(&db_path)?;
    let params = AnchorParams::default();

    println!(
        "== §B partial-clip rebuild cost probe (n={n}, full_cap={full_cap}, clips_per_source={clips_per_source}) =="
    );
    let seed_start = Instant::now();
    let seeded = seed(&mut db, n, clips_per_source)?;
    let corpus = &seeded.corpus;
    println!(
        "seeded {n} videos + tier2 fingerprints ({SCENES} scenes each) in {:.2?} (WAL file db)",
        seed_start.elapsed(),
    );

    let delta = DELTA_BURST
        .min(seeded.clip_ids.len())
        .min(seeded.source_ids.len());
    let clip_changed: BTreeSet<FileId> = seeded.clip_ids.iter().take(delta).copied().collect();
    let source_changed: BTreeSet<FileId> = seeded.source_ids.iter().take(delta).copied().collect();
    let empty_changed: BTreeSet<FileId> = BTreeSet::new();

    let prev_matches = measure_full_rebuild(&mut db, params, corpus, n, full_cap)?;

    let (empty_plan, empty_elapsed) = timed_incremental(corpus, &prev_matches, &empty_changed);
    let (clip_plan, clip_elapsed) = timed_incremental(corpus, &prev_matches, &clip_changed);
    let (source_plan, source_elapsed) = timed_incremental(corpus, &prev_matches, &source_changed);

    let clip_sweep = source_elapsed.saturating_sub(empty_elapsed);
    let changed_clip_search = clip_elapsed.saturating_sub(empty_elapsed);

    println!();
    println!("== incremental plan decomposition (Δ = {delta} changed files) ==");
    println!(
        "empty Δ (carry only):          {empty_elapsed:.2?}  → {} offsets examined  [(B) guarded off, (C) empty]",
        empty_plan.candidate_offsets_examined,
    );
    println!(
        "clip-changed Δ:                {clip_elapsed:.2?}  → {} offsets examined  [(B) build + search {} clips]",
        clip_plan.candidate_offsets_examined, delta,
    );
    println!(
        "source-changed Δ:              {source_elapsed:.2?}  → {} offsets examined  [(C) sweep {} clips]",
        source_plan.candidate_offsets_examined,
        corpus.len(),
    );
    println!();
    println!("  → carry-forward baseline (no build, no search):  {empty_elapsed:.2?}");
    println!("  → clip-changed (B) build + Δ search:             {changed_clip_search:.2?}");
    println!("  → residual (C) O(N) clip sweep (② candidate):    {clip_sweep:.2?}");

    let before = current_allocated();
    reset_peak();
    let t = Instant::now();
    let inc = rebuild_partial_clip_groups_incremental(&mut db, params, T0, &source_changed)?;
    let inc_elapsed = t.elapsed();
    let inc_peak = peak_allocated().saturating_sub(before);
    println!();
    println!("== DB incremental rebuild (source-changed Δ, daemon path) ==");
    println!(
        "rebuild_..._incremental:       {inc_elapsed:.2?}  → {} groups, {} members, {} edges   peak {:.1} MiB",
        inc.groups_created,
        inc.members_added,
        inc.edges_added,
        mib(inc_peak),
    );

    let pure_retained = measure_durable(corpus, params, &source_changed, clip_sweep);

    measure_cold_db_build(&mut db, params, n, full_cap, pure_retained)?;

    for (id, fp) in corpus {
        FingerprintsRepo::new(db.conn())
            .set_partial(*id, &format::encode_tier2(fp).expect("encode partial"))
            .expect("set partial");
    }
    let pp = partial_clip_params();
    println!();
    println!("== partial-ON rebuild: from-scratch vs durable incremental Δ ==");
    let mut pon_index = PartialClipIndex::new_with_source(pp, BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut pon_index, &mut db, T0, &empty_changed)?;
    let t = Instant::now();
    let pon_inc =
        rebuild_partial_clip_groups_durable(&mut pon_index, &mut db, T0, &source_changed)?;
    let pon_inc_elapsed = t.elapsed();
    println!(
        "durable incremental (source Δ):  {pon_inc_elapsed:.2?}  → {} groups, {} members",
        pon_inc.groups_created, pon_inc.members_added,
    );
    if n <= full_cap {
        let t = Instant::now();
        let pon_full = rebuild_partial_clip_groups_from_fingerprints(&mut db, pp, T0)?;
        let pon_full_elapsed = t.elapsed();
        let speedup = secs(pon_full_elapsed) / secs(pon_inc_elapsed).max(f64::MIN_POSITIVE);
        println!(
            "from_fingerprints (full O(N²)):  {pon_full_elapsed:.2?}  → {} groups",
            pon_full.groups_created,
        );
        println!(
            "  → partial-ON incremental speedup: {speedup:.1}× (target ≥10×; 29–44× expected at scale)"
        );
    } else {
        println!(
            "from_fingerprints (full O(N²)):  SKIPPED (n={n} > full_cap={full_cap}; intractable)"
        );
    }

    println!();
    let total_src = secs(source_elapsed).max(f64::MIN_POSITIVE);
    let sweep_frac = secs(clip_sweep) / total_src;
    let build_frac = secs(empty_elapsed) / total_src;
    println!("== ② residual-sweep verdict ==");
    println!(
        "  (C) clip sweep is {:.0}% of the source-changed plan; per-burst (B) index build is {:.0}%.",
        sweep_frac * 100.0,
        build_frac * 100.0,
    );
    if sweep_frac >= 0.5 {
        println!(
            "  → (C) O(N) clip sweep DOMINATES the stateless incremental source-changed burst — exactly the\n    bottleneck the durable index above removes. A clip-side *band* index did NOT fix it (the\n    band-collision work is direction-invariant and O(N) at the recall-mandated ≤8-bit width;\n    both the inversion and a membership pre-filter were measured, no speedup). The landed fix is\n    the durable cross-burst index (query only the changed files against a persisted multi-index\n    hash) — see the durable section above and docs/benchmarks.md §B-fix-3."
        );
    } else {
        println!(
            "  → (C) sweep does NOT dominate the stateless incremental burst here (the per-burst (B) index\n    build does); the durable index above still removes both per-burst index builds entirely."
        );
    }
    println!(
        "note: wall time is machine-dependent (reported, never goldened); candidate_offsets_examined\n      is deterministic. Grouping equivalence (durable == incremental == full) is proven by\n      vidcull-matcher partial::{{durable,tests}} + tests/partial_durable.rs, not re-asserted here."
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let n = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000usize);
    let full_cap = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_FULL_CAP);
    let clips_per_source = args
        .get(3)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    match run(n, full_cap, clips_per_source) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("partial_rebuild_cost failed: {e}");
            ExitCode::FAILURE
        }
    }
}
