use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use vidcull_core::types::{BestCopyMode, Codec, NormalizedPath, Resolution, VideoDuration};
use vidcull_core::{FileId, Result};
use vidcull_db::repo::{DaemonSettingsRepo, DuplicateGroupsRepo, FilesRepo, NewFile, TrustLevel};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::ranking::{QualityScore, assign_best_copies, score_quality, select_best};
use vidcull_matcher::whole::{WholeFileParams, scan_whole_file_candidates};
use vidcull_membench::{
    CountingAllocator, current_allocated, peak_allocated, reset_peak, synth_corpus,
};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const T0: i64 = 1_700_000_000;

const SCENES_PER_FILE: usize = 60;

const BANDS: u32 = 8;
const BAND_BITS: u32 = 64 / BANDS;

#[allow(clippy::cast_possible_wrap)]
const VOTE_BUCKET_MS: i64 = vidcull_core::SPARSE_GRID_INTERVAL_MS as i64;

const MATCH_TOLERANCE_MS: i64 = 2 * VOTE_BUCKET_MS;

const MIN_SHARED_POSTINGS: usize = 2;

fn mib(bytes: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / (1024.0 * 1024.0)
    }
}

#[derive(Clone, Copy)]
struct Posting {
    file_id: FileId,
    scene_index: usize,
}

fn for_each_band(phash: u64, mut f: impl FnMut(u8, u64)) {
    let mask = (1u64 << BAND_BITS) - 1;
    for band in 0..BANDS {
        let shift = BAND_BITS * band;
        let value = (phash >> shift) & mask;
        f(u8::try_from(band).unwrap_or(u8::MAX), value);
    }
}

fn offset_bucket(offset_ms: i64) -> i64 {
    offset_ms.div_euclid(VOTE_BUCKET_MS)
}

fn spanned_buckets(offset_ms: i64) -> std::ops::RangeInclusive<i64> {
    offset_bucket(offset_ms - MATCH_TOLERANCE_MS)..=offset_bucket(offset_ms + MATCH_TOLERANCE_MS)
}

struct BandIndexProbe<'a> {
    buckets: BTreeMap<(u8, u64), Vec<Posting>>,
    sources: BTreeMap<FileId, &'a [SceneHash]>,
}

impl<'a> BandIndexProbe<'a> {
    fn build(corpus: &'a [(FileId, Tier2Fingerprint)]) -> Self {
        let mut buckets: BTreeMap<(u8, u64), Vec<Posting>> = BTreeMap::new();
        let mut sources: BTreeMap<FileId, &'a [SceneHash]> = BTreeMap::new();
        for (file_id, fp) in corpus {
            for (scene_index, scene) in fp.scenes.iter().enumerate() {
                if scene.phash == 0 {
                    continue;
                }
                let posting = Posting {
                    file_id: *file_id,
                    scene_index,
                };
                for_each_band(scene.phash, |band, value| {
                    buckets.entry((band, value)).or_default().push(posting);
                });
            }
            sources.insert(*file_id, fp.scenes.as_slice());
        }
        Self { buckets, sources }
    }

    fn partners(&self, clip: &[SceneHash], exclude: FileId) -> BTreeSet<FileId> {
        let mut set = BTreeSet::new();
        for cs in clip {
            if cs.phash == 0 {
                continue;
            }
            for_each_band(cs.phash, |band, value| {
                if let Some(list) = self.buckets.get(&(band, value)) {
                    for posting in list {
                        if posting.file_id != exclude {
                            set.insert(posting.file_id);
                        }
                    }
                }
            });
        }
        set
    }
}

fn dominant_offset_counted(
    index: &BandIndexProbe<'_>,
    clip: &[SceneHash],
    source_id: FileId,
    source_scenes: &[SceneHash],
    visits: &mut u64,
) -> usize {
    let mut total = 0usize;
    for cs in clip {
        if cs.phash == 0 {
            continue;
        }
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        for_each_band(cs.phash, |band, value| {
            if let Some(list) = index.buckets.get(&(band, value)) {
                for posting in list {
                    *visits += 1;
                    if posting.file_id == source_id {
                        seen.insert(posting.scene_index);
                    }
                }
            }
        });
        for j in seen {
            if source_scenes.get(j).is_some() {
                total += 1;
            }
        }
    }
    total
}

#[derive(Debug, Clone, Copy)]
struct GrowthPoint {
    n: usize,
    raw_pairs: usize,
    surviving_pairs: usize,
    dominant_offset_visits: u64,
    verify_alignment_calls: usize,
    wall: Duration,
}

#[derive(Debug, Clone, Copy)]
struct BReductionPoint {
    n: usize,
    raw_pairs_checked: usize,
    survived_ge2_bands: usize,
}

fn band_value_sets(scenes: &[SceneHash]) -> [HashSet<u64>; 8] {
    let mut sets: [HashSet<u64>; 8] = [(); 8].map(|()| HashSet::new());
    for s in scenes {
        if s.phash == 0 {
            continue;
        }
        for_each_band(s.phash, |band, value| {
            sets[band as usize].insert(value);
        });
    }
    sets
}

fn distinct_bands_matched(a: &[HashSet<u64>; 8], b: &[HashSet<u64>; 8]) -> usize {
    (0..8)
        .filter(|&k| a[k].intersection(&b[k]).next().is_some())
        .count()
}

fn run_growth_and_b(n: usize, seed: u64) -> (GrowthPoint, BReductionPoint) {
    let corpus = synth_corpus(n, SCENES_PER_FILE, seed);
    let index = BandIndexProbe::build(&corpus);

    let t = Instant::now();
    let mut raw_pairs: BTreeSet<(FileId, FileId)> = BTreeSet::new();
    for (file_id, fp) in &corpus {
        for partner in index.partners(&fp.scenes, *file_id) {
            let pair = if *file_id < partner {
                (*file_id, partner)
            } else {
                (partner, *file_id)
            };
            raw_pairs.insert(pair);
        }
    }

    let mut visits = 0u64;
    let mut surviving = 0usize;
    let mut verify_calls = 0usize;
    for &(a, b) in &raw_pairs {
        let a_scenes = index.sources[&a];
        let b_scenes = index.sources[&b];
        let votes_ab = dominant_offset_counted(&index, a_scenes, b, b_scenes, &mut visits);
        if votes_ab < MIN_SHARED_POSTINGS {
            continue;
        }
        dominant_offset_counted(&index, b_scenes, a, a_scenes, &mut visits);
        verify_calls += 2;
        surviving += 1;
    }
    let wall = t.elapsed();

    let growth = GrowthPoint {
        n,
        raw_pairs: raw_pairs.len(),
        surviving_pairs: surviving,
        dominant_offset_visits: visits,
        verify_alignment_calls: verify_calls,
        wall,
    };

    let mut band_sets: BTreeMap<FileId, [HashSet<u64>; 8]> = BTreeMap::new();
    for (&file_id, &scenes) in &index.sources {
        band_sets.insert(file_id, band_value_sets(scenes));
    }
    let mut survived = 0usize;
    for &(a, b) in &raw_pairs {
        if distinct_bands_matched(&band_sets[&a], &band_sets[&b]) >= 2 {
            survived += 1;
        }
    }
    let b_point = BReductionPoint {
        n,
        raw_pairs_checked: raw_pairs.len(),
        survived_ge2_bands: survived,
    };

    (growth, b_point)
}

fn growth_slope(n1: usize, v1: u64, n2: usize, v2: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let (n1, n2, v1, v2) = (n1 as f64, n2 as f64, v1.max(1) as f64, v2.max(1) as f64);
    (v2 / v1).ln() / (n2 / n1).ln()
}

fn fit_log_log_slope(points: &[(usize, u64)]) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let xy: Vec<(f64, f64)> = points
        .iter()
        .map(|&(n, v)| ((n as f64).ln(), (v.max(1) as f64).ln()))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let count = xy.len() as f64;
    let mean_x = xy.iter().map(|&(x, _)| x).sum::<f64>() / count;
    let mean_y = xy.iter().map(|&(_, y)| y).sum::<f64>() / count;
    let mut num = 0.0;
    let mut den = 0.0;
    for &(x, y) in &xy {
        num += (x - mean_x) * (y - mean_y);
        den += (x - mean_x) * (x - mean_x);
    }
    if den == 0.0 { 0.0 } else { num / den }
}

fn print_b_point(b: &BReductionPoint) {
    #[allow(clippy::cast_precision_loss)]
    let survival_pct = if b.raw_pairs_checked == 0 {
        0.0
    } else {
        b.survived_ge2_bands as f64 / b.raw_pairs_checked as f64 * 100.0
    };
    #[allow(clippy::cast_precision_loss)]
    let reduction = if b.survived_ge2_bands == 0 {
        f64::INFINITY
    } else {
        b.raw_pairs_checked as f64 / b.survived_ge2_bands as f64
    };
    println!(
        "  [B] n={:>4}  raw_pairs={:>7}  >=2-band survivors={:>7}  survival={:>5.1}%  \
         reduction={:.2}x",
        b.n, b.raw_pairs_checked, b.survived_ge2_bands, survival_pct, reduction
    );
}

#[derive(Debug, Clone, Copy)]
struct FusionStats {
    n: usize,
    peak_bytes_naive: usize,
    pair_keys_naive: usize,
    bucket_entries_naive: usize,
    wall_naive: Duration,
    peak_bytes_compact: usize,
    pair_keys_compact: usize,
    bucket_entries_compact: usize,
    wall_compact: Duration,
}

fn for_each_fused_vote(
    corpus: &[(FileId, Tier2Fingerprint)],
    index: &BandIndexProbe<'_>,
    mut emit: impl FnMut(FileId, FileId, i64),
) {
    for (outer_file, fp) in corpus {
        for cs in &fp.scenes {
            if cs.phash == 0 {
                continue;
            }
            let clip_ts = i64::try_from(cs.timestamp_ms).unwrap_or(i64::MAX);
            let mut seen: BTreeSet<(FileId, usize)> = BTreeSet::new();
            for_each_band(cs.phash, |band, value| {
                if let Some(list) = index.buckets.get(&(band, value)) {
                    for posting in list {
                        if posting.file_id != *outer_file {
                            seen.insert((posting.file_id, posting.scene_index));
                        }
                    }
                }
            });
            for (partner_file, partner_scene_idx) in seen {
                let partner_scenes = index.sources[&partner_file];
                let Some(partner_scene) = partner_scenes.get(partner_scene_idx) else {
                    continue;
                };
                let src_ts = i64::try_from(partner_scene.timestamp_ms).unwrap_or(i64::MAX);
                emit(*outer_file, partner_file, src_ts - clip_ts);
            }
        }
    }
}

fn fused_histogram_naive(
    corpus: &[(FileId, Tier2Fingerprint)],
    index: &BandIndexProbe<'_>,
) -> (usize, usize, usize, Duration) {
    let t = Instant::now();
    let before = current_allocated();
    reset_peak();
    let mut hist: HashMap<(FileId, FileId), BTreeMap<i64, u32>> = HashMap::new();
    for_each_fused_vote(corpus, index, |outer, partner, offset| {
        let entry = hist.entry((outer, partner)).or_default();
        for bucket in spanned_buckets(offset) {
            *entry.entry(bucket).or_default() += 1;
        }
    });
    let peak = peak_allocated().saturating_sub(before);
    let wall = t.elapsed();
    let pair_keys = hist.len();
    let bucket_entries: usize = hist.values().map(BTreeMap::len).sum();
    std::hint::black_box(&hist);
    (peak, pair_keys, bucket_entries, wall)
}

fn fused_histogram_compact(
    corpus: &[(FileId, Tier2Fingerprint)],
    index: &BandIndexProbe<'_>,
) -> (usize, usize, usize, Duration) {
    let t = Instant::now();
    let before = current_allocated();
    reset_peak();
    let mut hist: HashMap<(FileId, FileId), Vec<(i16, u16)>> = HashMap::new();
    for_each_fused_vote(corpus, index, |outer, partner, offset| {
        let entry = hist.entry((outer, partner)).or_default();
        for bucket in spanned_buckets(offset) {
            let b16 = i16::try_from(bucket).unwrap_or(if bucket > 0 { i16::MAX } else { i16::MIN });
            if let Some(existing) = entry.iter_mut().find(|(bb, _)| *bb == b16) {
                existing.1 = existing.1.saturating_add(1);
            } else {
                entry.push((b16, 1));
            }
        }
    });
    let peak = peak_allocated().saturating_sub(before);
    let wall = t.elapsed();
    let pair_keys = hist.len();
    let bucket_entries: usize = hist.values().map(Vec::len).sum();
    std::hint::black_box(&hist);
    (peak, pair_keys, bucket_entries, wall)
}

fn measure_fusion(n: usize, seed: u64) -> FusionStats {
    let corpus = synth_corpus(n, SCENES_PER_FILE, seed);
    let index = BandIndexProbe::build(&corpus);
    let (peak_naive, pk_naive, be_naive, wall_naive) = fused_histogram_naive(&corpus, &index);
    let (peak_compact, pk_compact, be_compact, wall_compact) =
        fused_histogram_compact(&corpus, &index);
    FusionStats {
        n,
        peak_bytes_naive: peak_naive,
        pair_keys_naive: pk_naive,
        bucket_entries_naive: be_naive,
        wall_naive,
        peak_bytes_compact: peak_compact,
        pair_keys_compact: pk_compact,
        bucket_entries_compact: be_compact,
        wall_compact,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct QueryCounts {
    settings_load: usize,
    list_all: usize,
    list_members: usize,
    files_get: usize,
    set_best: usize,
}

impl QueryCounts {
    fn total(self) -> usize {
        self.settings_load + self.list_all + self.list_members + self.files_get + self.set_best
    }
}

fn assign_best_copies_counted(
    db: &mut Database,
    now_unix_s: i64,
) -> Result<(usize, usize, usize, QueryCounts)> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let files = FilesRepo::new(conn);
        let settings_repo = DaemonSettingsRepo::new(conn);
        let mut qc = QueryCounts::default();

        qc.settings_load += 1;
        let _ = settings_repo.load()?;
        let mode = BestCopyMode::default();

        qc.list_all += 1;
        let all_groups = groups.list_all()?;

        let mut updated = 0usize;
        let mut unchanged = 0usize;
        let mut without_active = 0usize;
        for group in all_groups {
            qc.list_members += 1;
            let members = groups.list_members(group.id)?;
            let mut scored: Vec<(FileId, QualityScore)> = Vec::new();
            for member in members {
                qc.files_get += 1;
                let Some(record) = files.get(member)? else {
                    continue;
                };
                if record.deleted_at.is_some() {
                    continue;
                }
                scored.push((
                    member,
                    score_quality(
                        record.resolution,
                        record.bitrate_bps,
                        record.codec.as_ref(),
                        record.container.as_deref(),
                        record.size_bytes,
                        record.laplacian_variance,
                        record.dct_energy,
                        record.bpp,
                        record.encoder_tags.as_deref(),
                        mode,
                    ),
                ));
            }
            let best = select_best(scored);
            if best.is_none() {
                without_active += 1;
            }
            if best == group.best_file_id {
                unchanged += 1;
            } else {
                qc.set_best += 1;
                groups.set_best(group.id, best, now_unix_s)?;
                updated += 1;
            }
        }
        Ok((updated, unchanged, without_active, qc))
    })
}

fn seed_bestcopy_fixture(db: &mut Database, groups: usize) -> Result<()> {
    db.transaction(|conn| {
        let files_repo = FilesRepo::new(conn);
        let groups_repo = DuplicateGroupsRepo::new(conn);
        let mut file_seq = 0i64;
        for g in 0..groups {
            let gid = groups_repo.create(TrustLevel::VeryLikely, T0)?;
            if g % 11 == 0 {
                continue;
            }
            let member_count = 2 + (g % 3);
            for m in 0..member_count {
                file_seq += 1;
                let path = format!("/q/{file_seq:08}.mp4");
                let width = 640 + u32::try_from(m % 3).unwrap_or(0) * 640;
                let new_file = NewFile {
                    path: NormalizedPath::new(&path),
                    size_bytes: 1_000_000 + file_seq,
                    mtime_ns: T0,
                    codec: Some(Codec::H265),
                    duration: Some(VideoDuration::from_millis(60_000)),
                    bitrate_bps: Some(2_000_000 + file_seq),
                    resolution: Some(Resolution::new(width, width * 9 / 16)),
                    first_seen_at: T0,
                    last_seen_at: T0,
                    ..Default::default()
                };
                let file_id = files_repo.insert(&new_file)?;
                groups_repo.add_member(gid, file_id)?;
                if g % 5 == 0 && m == member_count - 1 {
                    files_repo.mark_deleted(file_id, T0)?;
                }
            }
        }
        Ok(())
    })
}

fn run_query_counter_probe() -> Result<()> {
    const GROUPS: usize = 1000;

    let mut db_a = open_in_memory()?;
    seed_bestcopy_fixture(&mut db_a, GROUPS)?;
    let (updated, unchanged, without_active, qc) = assign_best_copies_counted(&mut db_a, T0)?;

    let mut db_b = open_in_memory()?;
    seed_bestcopy_fixture(&mut db_b, GROUPS)?;
    let real = assign_best_copies(&mut db_b, T0)?;

    let matches = real.groups_updated == updated
        && real.groups_unchanged == unchanged
        && real.groups_without_active_members == without_active;
    println!(
        "  fixture: {GROUPS} groups   replica outcome: updated={updated} unchanged={unchanged} \
         without_active={without_active}"
    );
    println!(
        "  real outcome:                updated={} unchanged={} without_active={}  fidelity={}",
        real.groups_updated,
        real.groups_unchanged,
        real.groups_without_active_members,
        if matches { "MATCH" } else { "MISMATCH" }
    );
    println!(
        "  query breakdown (before/N+1): settings={} list_all={} list_members={} files_get={} \
         set_best={}  TOTAL={}",
        qc.settings_load,
        qc.list_all,
        qc.list_members,
        qc.files_get,
        qc.set_best,
        qc.total()
    );
    #[allow(clippy::cast_precision_loss)]
    let reduction = qc.total() as f64 / 2.0;
    println!(
        "  AC5 target (after 211 JOIN fix): exactly 2 queries (settings 1 + JOIN 1) -> \
         {reduction:.0}x reduction"
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    println!("== Phase 0 probe: + combined batch (ralplan v4) ==");
    println!("(structural / deterministic counts; wall time is context only, never a gate)");

    println!();
    println!("-- fidelity check (n=100): replica raw-scan vs real scan_whole_file_candidates --");
    let seed_base = 0xC0FF_EE00_u64;
    let corpus100 = synth_corpus(100, SCENES_PER_FILE, seed_base);
    let real100 = scan_whole_file_candidates(&corpus100, WholeFileParams::default());
    let (g100, b100) = run_growth_and_b(100, seed_base);
    println!(
        "  real API surviving candidates = {}, replica surviving = {}  ({})",
        real100.len(),
        g100.surviving_pairs,
        if real100.len() == g100.surviving_pairs {
            "MATCH"
        } else {
            "MISMATCH"
        }
    );

    println!();
    println!("-- items 1+2: dominant_offset visit counter + growth exponent (n=100/200/400) --");
    let mut points = vec![g100];
    let mut b_points = vec![b100];
    for &n in &[200usize, 400] {
        let seed = seed_base.wrapping_add(n as u64);
        let (g, b) = run_growth_and_b(n, seed);
        points.push(g);
        b_points.push(b);
    }
    for p in &points {
        println!(
            "  n={:>4}  raw_pairs={:>7}  surviving={:>7}  dominant_offset_visits={:>14}  \
             verify_alignment_calls={:>6}  wall={:>8.2?}",
            p.n,
            p.raw_pairs,
            p.surviving_pairs,
            p.dominant_offset_visits,
            p.verify_alignment_calls,
            p.wall
        );
    }
    let slope_1 = growth_slope(
        points[0].n,
        points[0].dominant_offset_visits,
        points[1].n,
        points[1].dominant_offset_visits,
    );
    let slope_2 = growth_slope(
        points[1].n,
        points[1].dominant_offset_visits,
        points[2].n,
        points[2].dominant_offset_visits,
    );
    let fit = fit_log_log_slope(
        &points
            .iter()
            .map(|p| (p.n, p.dominant_offset_visits))
            .collect::<Vec<_>>(),
    );
    println!(
        "  growth exponent: pairwise 100->200 = {slope_1:.2}, 200->400 = {slope_2:.2}, \
         least-squares fit over all 3 points = {fit:.2}  (Theta(n^3) predicts ~3)"
    );

    println!();
    println!("-- item 3: dominant_offset vs verify_alignment cost-weight profile --");
    for p in &points {
        let verify_units = p.verify_alignment_calls * SCENES_PER_FILE;
        #[allow(clippy::cast_precision_loss)]
        let ratio = p.dominant_offset_visits as f64 / verify_units.max(1) as f64;
        println!(
            "  n={:>4}  dominant_offset_visits={:>14}  verify_alignment analytic units \
             (calls x {SCENES_PER_FILE} scenes)={:>9}  ratio={:>8.1}x",
            p.n, p.dominant_offset_visits, verify_units, ratio
        );
    }

    println!();
    println!("-- item 4: Option B (>=2-distinct-band) candidate-acceptance survival --");
    for b in &b_points {
        print_b_point(b);
    }

    println!();
    println!("-- item 5: fusion peak-memory prototype (n=200/400/800) --");
    let mut fusion_points = Vec::new();
    for &n in &[200usize, 400, 800] {
        let seed = 0xFE55_1000_u64.wrapping_add(n as u64);
        let f = measure_fusion(n, seed);
        println!(
            "  n={:>4}  naive:   peak={:>9.2} MiB  pairs={:>7}  bucket_entries={:>8}  \
             wall={:>7.2?}",
            f.n,
            mib(f.peak_bytes_naive),
            f.pair_keys_naive,
            f.bucket_entries_naive,
            f.wall_naive
        );
        println!(
            "         compact: peak={:>9.2} MiB  pairs={:>7}  bucket_entries={:>8}  \
             wall={:>7.2?}",
            mib(f.peak_bytes_compact),
            f.pair_keys_compact,
            f.bucket_entries_compact,
            f.wall_compact
        );
        fusion_points.push(f);
    }
    if let (Some(first), Some(last)) = (fusion_points.first(), fusion_points.last()) {
        #[allow(clippy::cast_precision_loss)]
        let n_ratio_sq = (last.n as f64 / first.n as f64).powi(2);
        #[allow(clippy::cast_precision_loss)]
        let observed_ratio_naive =
            last.peak_bytes_naive as f64 / first.peak_bytes_naive.max(1) as f64;
        let (first_n, last_n) = (first.n, last.n);
        println!(
            "  n={first_n}->{last_n}: peak (naive) grew {observed_ratio_naive:.2}x; O(n^2) \
             predicts {n_ratio_sq:.2}x"
        );
        println!(
            "  precedent budgets: near-dup LSH @100k = 10.3 MiB (48 MiB budget, ~4.6x \
             headroom); partial-clip anchor scoped @100k = 137.9 MiB (512 MiB budget, ~3.7x)"
        );
    }

    println!();
    println!("-- item 6: assign_best_copies 'before' (N+1) query counter --");
    if let Err(e) = run_query_counter_probe() {
        eprintln!("  query counter probe failed: {e}");
    }

    println!();
    println!(
        "-- item 7 (AC1b, post-landing): REAL fused single-pass visit growth (n=100/200/400) --"
    );
    let mut fused_visit_points: Vec<(usize, u64)> = Vec::new();
    for &n in &[100usize, 200, 400] {
        let seed = seed_base.wrapping_add(n as u64).wrapping_add(1);
        let corpus = synth_corpus(n, SCENES_PER_FILE, seed);
        let index = BandIndexProbe::build(&corpus);
        let visits = fused_visit_count(&corpus, &index);
        println!("  n={n:>4}  fused_visits={visits:>14}");
        fused_visit_points.push((n, visits));
    }
    let fslope_1 = growth_slope(
        fused_visit_points[0].0,
        fused_visit_points[0].1,
        fused_visit_points[1].0,
        fused_visit_points[1].1,
    );
    let fslope_2 = growth_slope(
        fused_visit_points[1].0,
        fused_visit_points[1].1,
        fused_visit_points[2].0,
        fused_visit_points[2].1,
    );
    let fused_fit = fit_log_log_slope(&fused_visit_points);
    println!(
        "  fused growth exponent: pairwise 100->200 = {fslope_1:.2}, 200->400 = {fslope_2:.2}, \
         least-squares fit = {fused_fit:.2}  (Theta(n^2) predicts ~2; legacy item 1/2 measured \
         2.99)"
    );

    println!();
    println!(
        "-- item 8 (AC1c, post-landing): REAL scan_whole_file_candidates peak memory \
         (n=200/400/800, default chunk k=8; fused is the sole path since ) --"
    );
    let mut real_peak_points: Vec<(usize, usize)> = Vec::new();
    for &n in &[200usize, 400, 800] {
        let seed = 0xA17C_0000_u64.wrapping_add(n as u64);
        let corpus = synth_corpus(n, SCENES_PER_FILE, seed);
        let before = current_allocated();
        reset_peak();
        let out = scan_whole_file_candidates(&corpus, WholeFileParams::default());
        let peak = peak_allocated().saturating_sub(before);
        #[allow(clippy::cast_precision_loss)]
        let peak_per_n = peak as f64 / n as f64;
        let peak_mib = mib(peak);
        let candidates = out.len();
        println!(
            "  n={n:>4}  peak={peak_mib:>9.2} MiB  candidates={candidates}  \
             peak/n={peak_per_n:>7.1} B"
        );
        std::hint::black_box(&out);
        real_peak_points.push((n, peak));
    }
    if let (Some(&(n0, p0)), Some(&(n1, p1))) = (real_peak_points.first(), real_peak_points.last())
    {
        #[allow(clippy::cast_precision_loss)]
        let observed_ratio = p1 as f64 / p0.max(1) as f64;
        #[allow(clippy::cast_precision_loss)]
        let n_ratio = n1 as f64 / n0 as f64;
        println!(
            "  n={n0}->{n1} ({n_ratio:.1}x): real peak grew {observed_ratio:.2}x -- chunking \
             (k=8) bounds this well below the n^2 the unchunked naive layout hit in item 5"
        );
    }

    println!();
    println!("-- item 8b (AC1c): chunk-size k x n scaling, probe-local chunked replica (n=400) --");
    for &k in &[8usize, 64, 400] {
        let seed = 0xFE55_2000_u64.wrapping_add(k as u64);
        let corpus = synth_corpus(400, SCENES_PER_FILE, seed);
        let index = BandIndexProbe::build(&corpus);
        let (peak, pair_keys, bucket_entries, wall) = fused_histogram_chunked(&corpus, &index, k);
        #[allow(clippy::cast_precision_loss)]
        let bytes_per_pair = if pair_keys == 0 {
            0.0
        } else {
            peak as f64 / pair_keys as f64
        };
        let peak_mib = mib(peak);
        println!(
            "  k={k:>4}  peak={peak_mib:>9.2} MiB  pair_keys(cumulative)={pair_keys:>7}  \
             bucket_entries={bucket_entries:>8}  bytes/pair(peak proxy)={bytes_per_pair:>7.1}  \
             wall={wall:>7.2?}"
        );
    }
    println!(
        "  expectation (Phase 0 item 5 handoff): peak tracks k*n*~196B, so larger k (fewer, \
         bigger chunks) should show a materially larger peak than k=8 at the same n=400 -- \
         confirming the chunk boundary (not just the compact layout) is what bounds memory"
    );

    println!();
    println!("== Phase 0 probe complete ==");
    ExitCode::SUCCESS
}

fn fused_visit_count(corpus: &[(FileId, Tier2Fingerprint)], index: &BandIndexProbe<'_>) -> u64 {
    let mut visits = 0u64;
    for (_outer_file, fp) in corpus {
        for cs in &fp.scenes {
            if cs.phash == 0 {
                continue;
            }
            for_each_band(cs.phash, |band, value| {
                if let Some(list) = index.buckets.get(&(band, value)) {
                    visits += list.len() as u64;
                }
            });
        }
    }
    visits
}

fn fused_histogram_chunked(
    corpus: &[(FileId, Tier2Fingerprint)],
    index: &BandIndexProbe<'_>,
    chunk_size: usize,
) -> (usize, usize, usize, Duration) {
    let t = Instant::now();
    let before = current_allocated();
    reset_peak();
    let mut outer_ids: Vec<FileId> = corpus.iter().map(|(id, _)| *id).collect();
    outer_ids.sort();

    let mut total_pair_keys = 0usize;
    let mut total_bucket_entries = 0usize;
    for chunk in outer_ids.chunks(chunk_size.max(1)) {
        let mut hist: HashMap<(FileId, FileId), Vec<(i16, u16)>> = HashMap::new();
        for &outer_file in chunk {
            let Some(scenes) = index.sources.get(&outer_file) else {
                continue;
            };
            for cs in *scenes {
                if cs.phash == 0 {
                    continue;
                }
                let clip_ts = i64::try_from(cs.timestamp_ms).unwrap_or(i64::MAX);
                let mut seen: BTreeSet<(FileId, usize)> = BTreeSet::new();
                for_each_band(cs.phash, |band, value| {
                    if let Some(list) = index.buckets.get(&(band, value)) {
                        for posting in list {
                            if posting.file_id != outer_file {
                                seen.insert((posting.file_id, posting.scene_index));
                            }
                        }
                    }
                });
                for (partner_file, partner_idx) in seen {
                    if partner_file <= outer_file {
                        continue;
                    }
                    let Some(partner_scenes) = index.sources.get(&partner_file) else {
                        continue;
                    };
                    let Some(partner_scene) = partner_scenes.get(partner_idx) else {
                        continue;
                    };
                    let src_ts = i64::try_from(partner_scene.timestamp_ms).unwrap_or(i64::MAX);
                    let offset = src_ts - clip_ts;
                    let entry = hist.entry((outer_file, partner_file)).or_default();
                    for bucket in spanned_buckets(offset) {
                        let b16 = i16::try_from(bucket).unwrap_or(if bucket > 0 {
                            i16::MAX
                        } else {
                            i16::MIN
                        });
                        if let Some(existing) = entry.iter_mut().find(|(bb, _)| *bb == b16) {
                            existing.1 = existing.1.saturating_add(1);
                        } else {
                            entry.push((b16, 1));
                        }
                    }
                }
            }
        }
        total_pair_keys += hist.len();
        total_bucket_entries += hist.values().map(Vec::len).sum::<usize>();
        std::hint::black_box(&hist);
    }
    let peak = peak_allocated().saturating_sub(before);
    let wall = t.elapsed();
    (peak, total_pair_keys, total_bucket_entries, wall)
}
