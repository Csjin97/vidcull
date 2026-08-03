use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use vidcull_core::Result;
use vidcull_core::types::FileId;
use vidcull_db::Database;
use vidcull_db::repo::{DuplicateGroupsRepo, SimilarityEdge, SimilarityEdgesRepo, TrustLevel};
use vidcull_fingerprint::format::decode_tier2;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

use crate::partial::{
    AnchorParams, ClipAlignment, Posting, VOTE_BUCKET_MS, ms_to_i64, spanned_buckets,
    verify_alignment,
};

const BANDS: u32 = AnchorParams::DEFAULT_BANDS;

const MIN_SHARED_POSTINGS: usize = 2;

const MIN_VERIFY_SCENES: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct WholeFileParams {
    pub scene_ratio_min: f64,
    pub span_coverage_min: f64,
    pub density_floor: f64,
    pub max_distance: u32,
}

impl Default for WholeFileParams {
    fn default() -> Self {
        Self {
            scene_ratio_min: 0.80,
            span_coverage_min: 0.80,
            density_floor: 0.15,
            max_distance: AnchorParams::DEFAULT_MAX_DISTANCE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WholeFileCandidate {
    pub a: FileId,
    pub b: FileId,
    pub scene_count_a: usize,
    pub scene_count_b: usize,
    pub scene_ratio: f64,
    pub span_coverage_a: f64,
    pub span_coverage_b: f64,
    pub coverage_ab: f64,
    pub coverage_ba: f64,
    pub offset_ab_ms: i64,
    pub offset_ba_ms: i64,
    pub offset_consistency_ab: f64,
    pub offset_consistency_ba: f64,
    pub passes_gate: bool,
}

#[must_use]
pub fn scan_whole_file_candidates(
    corpus: &[(FileId, Tier2Fingerprint)],
    params: WholeFileParams,
) -> Vec<WholeFileCandidate> {
    scan_whole_file_candidates_fused_with_chunk(corpus, params, fusion_chunk_size())
}

const DEFAULT_FUSION_CHUNK: usize = 8;

fn fusion_chunk_size() -> usize {
    fusion_chunk_size_from(std::env::var("VIDCULL_WHOLE_FUSION_CHUNK").ok().as_deref())
}

fn fusion_chunk_size_from(raw: Option<&str>) -> usize {
    raw.map(str::trim)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_FUSION_CHUNK)
}

#[cfg(test)]
fn scan_whole_file_candidates_legacy(
    corpus: &[(FileId, Tier2Fingerprint)],
    params: WholeFileParams,
) -> Vec<WholeFileCandidate> {
    let verify_params = build_verify_params(params.max_distance);
    let index = BandIndex::build(corpus, verify_params);

    let mut pairs: BTreeSet<(FileId, FileId)> = BTreeSet::new();
    for (file_id, fp) in corpus {
        for partner in index.partners(&fp.scenes, *file_id) {
            let pair = if *file_id < partner {
                (*file_id, partner)
            } else {
                (partner, *file_id)
            };
            pairs.insert(pair);
        }
    }

    pairs
        .into_iter()
        .filter_map(|(a, b)| analyze_pair(&index, a, b, params, verify_params))
        .collect()
}

fn scan_whole_file_candidates_fused_with_chunk(
    corpus: &[(FileId, Tier2Fingerprint)],
    params: WholeFileParams,
    chunk_size: usize,
) -> Vec<WholeFileCandidate> {
    let verify_params = build_verify_params(params.max_distance);
    let index = BandIndex::build(corpus, verify_params);
    let chunk_size = chunk_size.max(1);

    let outer_order: Vec<FileId> = index.sources.keys().copied().collect();

    // Each chunk owns a disjoint set of pairs (a pair is only ever recorded
    // from the chunk containing its smaller FileId), so chunks are safe to
    // process in parallel. Collecting into a Vec<Vec<_>> before flattening
    // preserves the exact same chunk order as the sequential version, so
    // results stay byte-for-byte deterministic regardless of which worker
    // thread finishes first.
    outer_order
        .par_chunks(chunk_size)
        .map(|chunk| {
            let mut pair_votes: BTreeMap<(FileId, FileId), FusedVotes> = BTreeMap::new();
            accumulate_chunk_votes(&index, chunk, &mut pair_votes);
            pair_votes
                .iter()
                .filter_map(|(&(a, b), votes)| {
                    finalize_fused_pair(&index, a, b, votes, params, verify_params)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

fn build_verify_params(max_distance: u32) -> AnchorParams {
    AnchorParams::new(BANDS, max_distance.min(64), 0, MIN_VERIFY_SCENES)
        .expect("BANDS divides 64, distance <= 64, coverage 0 <= 1000")
}

struct BandIndex<'a> {
    params: AnchorParams,
    buckets: BTreeMap<(u8, u64), Vec<Posting>>,
    sources: BTreeMap<FileId, &'a [SceneHash]>,
}

impl<'a> BandIndex<'a> {
    fn build(corpus: &'a [(FileId, Tier2Fingerprint)], params: AnchorParams) -> Self {
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
                for_each_band(params, scene.phash, |band, value| {
                    buckets.entry((band, value)).or_default().push(posting);
                });
            }
            sources.insert(*file_id, fp.scenes.as_slice());
        }
        Self {
            params,
            buckets,
            sources,
        }
    }

    #[cfg(test)]
    fn partners(&self, clip: &[SceneHash], exclude: FileId) -> BTreeSet<FileId> {
        let mut set = BTreeSet::new();
        for cs in clip {
            if cs.phash == 0 {
                continue;
            }
            for_each_band(self.params, cs.phash, |band, value| {
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

fn for_each_band(params: AnchorParams, phash: u64, mut f: impl FnMut(u8, u64)) {
    let bits = params.band_bits();
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    for band in 0..params.bands() {
        let shift = bits * band;
        let value = (phash >> shift) & mask;
        f(u8::try_from(band).unwrap_or(u8::MAX), value);
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
fn analyze_pair(
    index: &BandIndex<'_>,
    a: FileId,
    b: FileId,
    params: WholeFileParams,
    verify_params: AnchorParams,
) -> Option<WholeFileCandidate> {
    let a_scenes = *index.sources.get(&a)?;
    let b_scenes = *index.sources.get(&b)?;

    let (offset_ab_ms, offset_consistency_ab, votes_ab) =
        dominant_offset(index, a_scenes, b, b_scenes);
    if votes_ab < MIN_SHARED_POSTINGS {
        return None;
    }
    let (offset_ba_ms, offset_consistency_ba, _votes_ba) =
        dominant_offset(index, b_scenes, a, a_scenes);

    Some(build_candidate(
        a,
        a_scenes,
        b,
        b_scenes,
        offset_ab_ms,
        offset_consistency_ab,
        offset_ba_ms,
        offset_consistency_ba,
        params,
        verify_params,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_candidate(
    a: FileId,
    a_scenes: &[SceneHash],
    b: FileId,
    b_scenes: &[SceneHash],
    offset_ab_ms: i64,
    offset_consistency_ab: f64,
    offset_ba_ms: i64,
    offset_consistency_ba: f64,
    params: WholeFileParams,
    verify_params: AnchorParams,
) -> WholeFileCandidate {
    let align_ab = verify_alignment(a_scenes, b, b_scenes, offset_ab_ms, verify_params);
    let align_ba = verify_alignment(b_scenes, a, a_scenes, offset_ba_ms, verify_params);
    let (span_a, matched_ab) = align_ab.map_or((0, 0), |al| (span_ms(&al), al.matched_scenes));
    let (span_b, matched_ba) = align_ba.map_or((0, 0), |al| (span_ms(&al), al.matched_scenes));

    let scene_count_a = informative_count(a_scenes);
    let scene_count_b = informative_count(b_scenes);
    let scene_ratio = ratio_usize(
        scene_count_a.min(scene_count_b),
        scene_count_a.max(scene_count_b),
    );
    let span_coverage_a = ratio_u64(span_a, duration_ms(a_scenes));
    let span_coverage_b = ratio_u64(span_b, duration_ms(b_scenes));
    let coverage_ab = ratio_usize(matched_ab, scene_count_a);
    let coverage_ba = ratio_usize(matched_ba, scene_count_b);

    let passes_gate = whole_file_gate(
        scene_ratio,
        span_coverage_a,
        span_coverage_b,
        coverage_ab,
        coverage_ba,
        params,
    );

    WholeFileCandidate {
        a,
        b,
        scene_count_a,
        scene_count_b,
        scene_ratio,
        span_coverage_a,
        span_coverage_b,
        coverage_ab,
        coverage_ba,
        offset_ab_ms,
        offset_ba_ms,
        offset_consistency_ab,
        offset_consistency_ba,
        passes_gate,
    }
}

#[cfg(test)]
fn dominant_offset(
    index: &BandIndex<'_>,
    clip: &[SceneHash],
    source_id: FileId,
    source_scenes: &[SceneHash],
) -> (i64, f64, usize) {
    let mut votes: BTreeMap<i64, usize> = BTreeMap::new();
    let mut total = 0usize;
    for cs in clip {
        if cs.phash == 0 {
            continue;
        }
        let clip_ts = ms_to_i64(cs.timestamp_ms);
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        for_each_band(index.params, cs.phash, |band, value| {
            if let Some(list) = index.buckets.get(&(band, value)) {
                for posting in list {
                    if posting.file_id == source_id {
                        seen.insert(posting.scene_index);
                    }
                }
            }
        });
        for &j in &seen {
            let Some(src) = source_scenes.get(j) else {
                continue;
            };
            let offset = ms_to_i64(src.timestamp_ms) - clip_ts;
            total += 1;
            for bucket in spanned_buckets(offset) {
                *votes.entry(bucket).or_default() += 1;
            }
        }
    }
    let (bucket, top) = median_max_bucket(&votes);
    (
        bucket.saturating_mul(VOTE_BUCKET_MS),
        consistency(top, total),
        total,
    )
}

#[cfg(test)]
fn median_max_bucket(votes: &BTreeMap<i64, usize>) -> (i64, usize) {
    let Some(max) = votes.values().copied().max() else {
        return (0, 0);
    };
    let tied: Vec<i64> = votes
        .iter()
        .filter(|&(_, &v)| v == max)
        .map(|(&k, _)| k)
        .collect();
    (tied[tied.len() / 2], max)
}

#[derive(Debug, Default)]
struct FusedVotes {
    hist_ab: BTreeMap<i16, u16>,
    hist_ba: BTreeMap<i16, u16>,
    total: usize,
}

fn add_vote(hist: &mut BTreeMap<i16, u16>, bucket: i64) {
    let clamped = i16::try_from(bucket).unwrap_or(if bucket > 0 { i16::MAX } else { i16::MIN });
    hist.entry(clamped)
        .and_modify(|v| *v = v.saturating_add(1))
        .or_insert(1);
}

fn median_max_bucket_compact(votes: &BTreeMap<i16, u16>) -> (i64, usize) {
    let Some(max) = votes.values().copied().max() else {
        return (0, 0);
    };
    let tied: Vec<i64> = votes
        .iter()
        .filter(|&(_, &v)| v == max)
        .map(|(&b, _)| i64::from(b))
        .collect();
    (tied[tied.len() / 2], usize::from(max))
}

fn accumulate_chunk_votes(
    index: &BandIndex<'_>,
    outer_chunk: &[FileId],
    pair_votes: &mut BTreeMap<(FileId, FileId), FusedVotes>,
) {
    for &outer_file in outer_chunk {
        let Some(outer_scenes) = index.sources.get(&outer_file).copied() else {
            continue;
        };
        for cs in outer_scenes {
            if cs.phash == 0 {
                continue;
            }
            let clip_ts = ms_to_i64(cs.timestamp_ms);
            let mut seen: BTreeSet<(FileId, usize)> = BTreeSet::new();
            for_each_band(index.params, cs.phash, |band, value| {
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
                let Some(partner_scenes) = index.sources.get(&partner_file).copied() else {
                    continue;
                };
                let Some(partner_scene) = partner_scenes.get(partner_idx) else {
                    continue;
                };
                let partner_ts = ms_to_i64(partner_scene.timestamp_ms);
                let offset_ab = partner_ts - clip_ts;
                let offset_ba = clip_ts - partner_ts;
                let entry = pair_votes.entry((outer_file, partner_file)).or_default();
                entry.total += 1;
                for bucket in spanned_buckets(offset_ab) {
                    add_vote(&mut entry.hist_ab, bucket);
                }
                for bucket in spanned_buckets(offset_ba) {
                    add_vote(&mut entry.hist_ba, bucket);
                }
            }
        }
    }
}

fn finalize_fused_pair(
    index: &BandIndex<'_>,
    a: FileId,
    b: FileId,
    votes: &FusedVotes,
    params: WholeFileParams,
    verify_params: AnchorParams,
) -> Option<WholeFileCandidate> {
    if votes.total < MIN_SHARED_POSTINGS {
        return None;
    }
    let a_scenes = *index.sources.get(&a)?;
    let b_scenes = *index.sources.get(&b)?;

    let (bucket_ab, top_ab) = median_max_bucket_compact(&votes.hist_ab);
    let (bucket_ba, top_ba) = median_max_bucket_compact(&votes.hist_ba);
    let offset_ab_ms = bucket_ab.saturating_mul(VOTE_BUCKET_MS);
    let offset_ba_ms = bucket_ba.saturating_mul(VOTE_BUCKET_MS);
    let offset_consistency_ab = consistency(top_ab, votes.total);
    let offset_consistency_ba = consistency(top_ba, votes.total);

    Some(build_candidate(
        a,
        a_scenes,
        b,
        b_scenes,
        offset_ab_ms,
        offset_consistency_ab,
        offset_ba_ms,
        offset_consistency_ba,
        params,
        verify_params,
    ))
}

fn whole_file_gate(
    scene_ratio: f64,
    span_coverage_a: f64,
    span_coverage_b: f64,
    coverage_ab: f64,
    coverage_ba: f64,
    params: WholeFileParams,
) -> bool {
    let g1 = scene_ratio >= params.scene_ratio_min;
    let g2 =
        span_coverage_a >= params.span_coverage_min && span_coverage_b >= params.span_coverage_min;
    let g4 = coverage_ab.min(coverage_ba) >= params.density_floor;
    g1 && g2 && g4
}

fn duration_ms(scenes: &[SceneHash]) -> u64 {
    match (scenes.first(), scenes.last()) {
        (Some(first), Some(last)) => last.timestamp_ms.saturating_sub(first.timestamp_ms),
        _ => 0,
    }
}

fn span_ms(align: &ClipAlignment) -> u64 {
    align.clip_end_ms.saturating_sub(align.clip_start_ms)
}

fn informative_count(scenes: &[SceneHash]) -> usize {
    scenes.iter().filter(|s| s.phash != 0).count()
}

#[allow(clippy::cast_precision_loss)]
fn ratio_u64(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio_usize(num: usize, den: usize) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

#[allow(clippy::cast_precision_loss)]
fn consistency(top: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        top as f64 / total as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WholeFileMatch {
    pub a: FileId,
    pub b: FileId,
    pub score_x1000: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WholeFilePlan {
    pub matches: Vec<WholeFileMatch>,
}

#[must_use]
pub fn plan_whole_file_matches(candidates: &[WholeFileCandidate]) -> WholeFilePlan {
    let matches = candidates
        .iter()
        .filter(|c| c.passes_gate)
        .map(|c| WholeFileMatch {
            a: c.a,
            b: c.b,
            score_x1000: density_score_x1000(c.coverage_ab.min(c.coverage_ba)),
        })
        .collect();
    WholeFilePlan { matches }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn density_score_x1000(density: f64) -> i32 {
    (density.clamp(0.0, 1.0) * 1000.0).round() as i32
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WholeFileRebuildOutcome {
    pub groups_cleared: usize,
    pub groups_created: usize,
    pub members_added: usize,
    pub edges_added: usize,
    pub pairs_covered_skipped: usize,
}

pub fn rebuild_whole_file_groups(
    db: &mut Database,
    params: WholeFileParams,
    now_unix_s: i64,
) -> Result<WholeFileRebuildOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let edges_repo = SimilarityEdgesRepo::new(conn);

        let groups_cleared = groups.delete_non_transitive_by_trust(TrustLevel::VeryLikely)?;

        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
        for (file_id, blob) in vidcull_db::repo::FingerprintsRepo::new(conn).list_active_tier2()? {
            corpus.push((file_id, decode_tier2(&blob)?));
        }

        let candidates = scan_whole_file_candidates(&corpus, params);
        let plan = plan_whole_file_matches(&candidates);
        let covered = transitive_component_roots(&groups)?;
        write_whole_file_plan(
            &groups,
            &edges_repo,
            &plan,
            &covered,
            now_unix_s,
            groups_cleared,
        )
    })
}

fn transitive_component_roots(
    groups: &DuplicateGroupsRepo<'_>,
) -> Result<BTreeMap<FileId, FileId>> {
    fn find(parent: &mut BTreeMap<FileId, FileId>, file: FileId) -> FileId {
        let up = parent[&file];
        if up == file {
            return file;
        }
        let root = find(parent, up);
        parent.insert(file, root);
        root
    }

    let mut parent: BTreeMap<FileId, FileId> = BTreeMap::new();
    for (group, members) in groups.list_all_with_members()? {
        if group.non_transitive
            || !matches!(
                group.trust_level,
                TrustLevel::Exact | TrustLevel::VeryLikely
            )
        {
            continue;
        }
        let Some((&first, rest)) = members.split_first() else {
            continue;
        };
        parent.entry(first).or_insert(first);
        for &member in rest {
            parent.entry(member).or_insert(member);
            let (root_a, root_b) = (find(&mut parent, first), find(&mut parent, member));
            if root_a != root_b {
                parent.insert(root_b, root_a);
            }
        }
    }

    let files: Vec<FileId> = parent.keys().copied().collect();
    let mut roots = BTreeMap::new();
    for file in files {
        let root = find(&mut parent, file);
        roots.insert(file, root);
    }
    Ok(roots)
}

fn write_whole_file_plan(
    groups: &DuplicateGroupsRepo<'_>,
    edges_repo: &SimilarityEdgesRepo<'_>,
    plan: &WholeFilePlan,
    covered: &BTreeMap<FileId, FileId>,
    now_unix_s: i64,
    groups_cleared: usize,
) -> Result<WholeFileRebuildOutcome> {
    let mut outcome = WholeFileRebuildOutcome {
        groups_cleared,
        ..WholeFileRebuildOutcome::default()
    };
    for m in &plan.matches {
        if let (Some(root_a), Some(root_b)) = (covered.get(&m.a), covered.get(&m.b)) {
            if root_a == root_b {
                outcome.pairs_covered_skipped += 1;
                continue;
            }
        }
        let gid = groups.create_non_transitive(TrustLevel::VeryLikely, now_unix_s)?;
        groups.add_member(gid, m.a)?;
        groups.add_member(gid, m.b)?;
        edges_repo.insert(&SimilarityEdge {
            group_id: gid,
            file_a: m.a,
            file_b: m.b,
            score_x1000: m.score_x1000,
            partial_span: None,
            intro_outro: false,
        })?;
        outcome.groups_created += 1;
        outcome.members_added += 2;
        outcome.edges_added += 1;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn scene(ts: u64, phash: u64) -> SceneHash {
        SceneHash {
            timestamp_ms: ts,
            phash,
        }
    }

    fn fp(scenes: Vec<SceneHash>) -> Tier2Fingerprint {
        Tier2Fingerprint { scenes }
    }

    fn reencode_pair(n: usize) -> (Vec<SceneHash>, Vec<SceneHash>) {
        reencode_pair_seeded(n, 0x5151_u64)
    }

    fn reencode_pair_seeded(n: usize, seed: u64) -> (Vec<SceneHash>, Vec<SceneHash>) {
        let mut st = seed;
        let a: Vec<SceneHash> = (0..n)
            .map(|i| scene(i as u64 * 1000, splitmix64(&mut st) | 1))
            .collect();
        let b: Vec<SceneHash> = a
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let ph = if i % 5 == 0 {
                    s.phash ^ 0b110
                } else {
                    splitmix64(&mut st) | 1
                };
                scene(s.timestamp_ms + 2500, ph)
            })
            .collect();
        (a, b)
    }

    fn dispersed_pair(n: usize, seed: u64) -> (Vec<SceneHash>, Vec<SceneHash>) {
        let mut st = seed;
        let a: Vec<SceneHash> = (0..n)
            .map(|i| scene(i as u64 * 1000, splitmix64(&mut st) | 1))
            .collect();
        let intro = n / 20;
        let b: Vec<SceneHash> = a
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let shared = i < intro || i >= n - intro;
                let ph = if shared {
                    s.phash ^ 0b110
                } else {
                    splitmix64(&mut st) | 1
                };
                scene(s.timestamp_ms, ph)
            })
            .collect();
        (a, b)
    }

    fn partial_pair(
        n: usize,
        start: usize,
        len: usize,
        seed: u64,
    ) -> (Vec<SceneHash>, Vec<SceneHash>) {
        let mut st = seed;
        let source: Vec<SceneHash> = (0..n)
            .map(|i| scene(i as u64 * 1000, splitmix64(&mut st) | 1))
            .collect();
        let clip: Vec<SceneHash> = source[start..start + len]
            .iter()
            .enumerate()
            .map(|(i, s)| scene(i as u64 * 1000, s.phash ^ 0b110))
            .collect();
        (clip, source)
    }

    #[track_caller]
    fn assert_candidates_eq(x: &WholeFileCandidate, y: &WholeFileCandidate) {
        assert_eq!(x.a, y.a);
        assert_eq!(x.b, y.b);
        assert_eq!(x.scene_count_a, y.scene_count_a);
        assert_eq!(x.scene_count_b, y.scene_count_b);
        assert_eq!(x.scene_ratio.to_bits(), y.scene_ratio.to_bits());
        assert_eq!(x.span_coverage_a.to_bits(), y.span_coverage_a.to_bits());
        assert_eq!(x.span_coverage_b.to_bits(), y.span_coverage_b.to_bits());
        assert_eq!(x.coverage_ab.to_bits(), y.coverage_ab.to_bits());
        assert_eq!(x.coverage_ba.to_bits(), y.coverage_ba.to_bits());
        assert_eq!(x.offset_ab_ms, y.offset_ab_ms);
        assert_eq!(x.offset_ba_ms, y.offset_ba_ms);
        assert_eq!(
            x.offset_consistency_ab.to_bits(),
            y.offset_consistency_ab.to_bits()
        );
        assert_eq!(
            x.offset_consistency_ba.to_bits(),
            y.offset_consistency_ba.to_bits()
        );
        assert_eq!(x.passes_gate, y.passes_gate);
    }

    #[test]
    fn t1_whole_reencode_positive() {
        let (a, b) = reencode_pair(1000);
        let corpus = vec![(FileId(1), fp(a)), (FileId(2), fp(b))];
        let out = scan_whole_file_candidates(&corpus, WholeFileParams::default());
        assert_eq!(out.len(), 1, "one whole-file candidate for the pair");
        let c = &out[0];
        assert_eq!((c.a, c.b), (FileId(1), FileId(2)), "canonical a < b");
        assert!(
            c.scene_ratio > 0.999,
            "near-equal length: {}",
            c.scene_ratio
        );
        assert!(
            c.span_coverage_a > 0.9 && c.span_coverage_b > 0.9,
            "matches span the whole file: {} / {}",
            c.span_coverage_a,
            c.span_coverage_b
        );
        assert!(
            c.coverage_ab > 0.15 && c.coverage_ab < 0.30,
            "~20 % cross-codec density: {}",
            c.coverage_ab
        );
        assert!(c.passes_gate, "whole re-encode clears G1 && G2 && G4");
    }

    #[test]
    fn t2_dispersed_shared_records_honest_gate_outcome() {
        let n = 1000usize;
        let (a, b) = dispersed_pair(n, 0xBEEF_u64);
        let corpus = vec![(FileId(1), fp(a)), (FileId(2), fp(b))];
        let out = scan_whole_file_candidates(&corpus, WholeFileParams::default());
        assert_eq!(out.len(), 1, "the dispersed pair is still measured");
        let c = &out[0];
        assert!(c.scene_ratio > 0.999, "G1 passes (equal length)");
        assert!(
            c.span_coverage_a > 0.9 && c.span_coverage_b > 0.9,
            "F2: G2 span-coverage cannot see dispersion: {} / {}",
            c.span_coverage_a,
            c.span_coverage_b
        );
        assert!(
            c.coverage_ab < 0.15,
            "real shared density is ~10 %: {}",
            c.coverage_ab
        );
        assert!(
            !c.passes_gate,
            "G4 (density floor) is the SOLE defence and rejects at ~10 %"
        );
    }

    #[test]
    fn t3_partial_clip_fails_g1() {
        let n = 1000usize;
        let (clip, source) = partial_pair(n, 400, 200, 0xC0FF_u64);
        let corpus = vec![(FileId(1), fp(clip)), (FileId(2), fp(source))];
        let out = scan_whole_file_candidates(&corpus, WholeFileParams::default());
        assert_eq!(out.len(), 1, "the partial clip is still measured");
        let c = &out[0];
        assert!(
            c.scene_ratio < 0.30,
            "scene_ratio ~0.2 is far below the 0.9 floor: {}",
            c.scene_ratio
        );
        assert!(!c.passes_gate, "G1 rejects a partial clip");
    }

    #[test]
    fn t4_bidirectional_symmetry_canonical() {
        let (a, b) = reencode_pair(1000);
        let fwd = scan_whole_file_candidates(
            &[(FileId(1), fp(a.clone())), (FileId(2), fp(b.clone()))],
            WholeFileParams::default(),
        );
        let rev = scan_whole_file_candidates(
            &[(FileId(2), fp(b)), (FileId(1), fp(a))],
            WholeFileParams::default(),
        );
        assert_eq!(fwd.len(), 1);
        assert_eq!(rev.len(), 1);
        assert_candidates_eq(&fwd[0], &rev[0]);
    }

    #[test]
    fn t5_gate_boundaries() {
        let p = WholeFileParams::default();
        assert!(
            whole_file_gate(0.80, 0.80, 0.80, 0.15, 0.15, p),
            "all three gates exactly at their floors pass"
        );
        assert!(whole_file_gate(0.80, 0.85, 0.85, 0.30, 0.30, p));
        assert!(
            !whole_file_gate(0.7999, 0.85, 0.85, 0.30, 0.30, p),
            "scene_ratio below R"
        );
        assert!(
            !whole_file_gate(0.95, 0.7999, 0.85, 0.30, 0.30, p),
            "span_coverage_a below S"
        );
        assert!(
            !whole_file_gate(0.95, 0.85, 0.7999, 0.30, 0.30, p),
            "span_coverage_b below S"
        );
        assert!(
            whole_file_gate(0.95, 0.85, 0.85, 0.15, 0.90, p),
            "min coverage exactly at the floor"
        );
        assert!(
            !whole_file_gate(0.95, 0.85, 0.85, 0.1499, 0.90, p),
            "min coverage below the floor"
        );
    }

    #[test]
    fn t6_duration_and_span_formulas() {
        let scenes = vec![scene(1000, 1), scene(3000, 2), scene(9000, 3)];
        assert_eq!(duration_ms(&scenes), 8000, "last - first");
        assert_eq!(duration_ms(&[]), 0, "empty is degenerate");
        assert_eq!(duration_ms(&scenes[0..1]), 0, "single scene spans zero");
        let align = ClipAlignment {
            source: FileId(2),
            source_offset: 0,
            matched_scenes: 4,
            clip_scenes: 10,
            coverage_x1000: 400,
            start_ms: 5000,
            end_ms: 15_000,
            clip_start_ms: 2000,
            clip_end_ms: 12_000,
        };
        assert_eq!(span_ms(&align), 10_000, "clip_end - clip_start");
    }

    fn synth_multi_corpus(
        n_pairs: usize,
        n_noise: usize,
        scenes_per_pair: usize,
        seed: u64,
    ) -> Vec<(FileId, Tier2Fingerprint)> {
        let mut corpus = Vec::new();
        let mut next_id = 1i64;
        for i in 0..n_pairs {
            let pair_seed = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let (a, b) = match i % 3 {
                0 => reencode_pair_seeded(scenes_per_pair, pair_seed),
                1 => dispersed_pair(scenes_per_pair, pair_seed),
                _ => {
                    let len = (scenes_per_pair / 5).max(2);
                    let start = scenes_per_pair.saturating_sub(len) / 2;
                    partial_pair(scenes_per_pair, start, len, pair_seed)
                }
            };
            corpus.push((FileId(next_id), fp(a)));
            next_id += 1;
            corpus.push((FileId(next_id), fp(b)));
            next_id += 1;
        }
        let mut noise_state = seed ^ 0xF00D_F00D_u64;
        for _ in 0..n_noise {
            let scenes: Vec<SceneHash> = (0..scenes_per_pair)
                .map(|s| scene(s as u64 * 1000, splitmix64(&mut noise_state) | 1))
                .collect();
            corpus.push((FileId(next_id), fp(scenes)));
            next_id += 1;
        }
        corpus
    }

    #[track_caller]
    fn assert_full_scan_eq(x: &[WholeFileCandidate], y: &[WholeFileCandidate]) {
        assert_eq!(x.len(), y.len(), "surviving candidate count must match");
        for (cx, cy) in x.iter().zip(y) {
            assert_candidates_eq(cx, cy);
        }
    }

    #[test]
    fn fusion_oracle_matches_legacy_across_seeds_and_chunks() {
        const SEEDS: [u64; 8] = [
            0x0000_0001,
            0x1234_5678,
            0x9ABC_DEF0,
            0xDEAD_BEEF,
            0xC0FF_EE00,
            0x5EED_F00D,
            0x7777_7777,
            0xFACE_B00C,
        ];
        const CHUNK_SIZES: [usize; 5] = [1, 2, 3, DEFAULT_FUSION_CHUNK, 64];
        const SHAPES: [(usize, usize, usize); 4] =
            [(1, 0, 40), (3, 2, 30), (6, 4, 25), (10, 6, 20)];

        for &seed in &SEEDS {
            for &(n_pairs, n_noise, scenes) in &SHAPES {
                let corpus = synth_multi_corpus(n_pairs, n_noise, scenes, seed);
                let legacy = scan_whole_file_candidates_legacy(&corpus, WholeFileParams::default());
                for &chunk in &CHUNK_SIZES {
                    let fused = scan_whole_file_candidates_fused_with_chunk(
                        &corpus,
                        WholeFileParams::default(),
                        chunk,
                    );
                    assert_full_scan_eq(&fused, &legacy);
                }
            }
        }
    }

    #[test]
    fn fusion_oracle_matches_legacy_on_t1_t2_t3_shapes_chunk_one() {
        let cases: Vec<(Vec<SceneHash>, Vec<SceneHash>)> = vec![
            reencode_pair(1000),
            dispersed_pair(1000, 0xBEEF_u64),
            partial_pair(1000, 400, 200, 0xC0FF_u64),
        ];
        for (a, b) in cases {
            let corpus = vec![(FileId(1), fp(a)), (FileId(2), fp(b))];
            let legacy = scan_whole_file_candidates_legacy(&corpus, WholeFileParams::default());
            let fused =
                scan_whole_file_candidates_fused_with_chunk(&corpus, WholeFileParams::default(), 1);
            assert_full_scan_eq(&fused, &legacy);
        }
    }

    #[test]
    fn public_entry_uses_fused_default_chunk() {
        let (a, b) = reencode_pair(500);
        let corpus = vec![(FileId(1), fp(a)), (FileId(2), fp(b))];
        let via_public = scan_whole_file_candidates(&corpus, WholeFileParams::default());
        let via_fused = scan_whole_file_candidates_fused_with_chunk(
            &corpus,
            WholeFileParams::default(),
            DEFAULT_FUSION_CHUNK,
        );
        assert_full_scan_eq(&via_public, &via_fused);
    }

    #[test]
    fn fusion_chunk_size_from_parses() {
        assert_eq!(
            fusion_chunk_size_from(None),
            DEFAULT_FUSION_CHUNK,
            "unset -> default"
        );
        assert_eq!(
            fusion_chunk_size_from(Some("bogus")),
            DEFAULT_FUSION_CHUNK,
            "invalid -> default"
        );
        assert_eq!(
            fusion_chunk_size_from(Some("0")),
            DEFAULT_FUSION_CHUNK,
            "zero -> default (chunks() needs >=1)"
        );
        assert_eq!(
            fusion_chunk_size_from(Some("3")),
            3,
            "valid positive int is honoured"
        );
        assert_eq!(
            fusion_chunk_size_from(Some(" 16 ")),
            16,
            "trimmed value is honoured"
        );
    }

    #[test]
    fn median_max_bucket_compact_matches_btreemap_version() {
        let cases: Vec<Vec<(i64, usize)>> = vec![
            vec![],
            vec![(5, 3)],
            vec![(1, 2), (2, 2)],
            vec![(-3, 4), (0, 4), (7, 4)],
            vec![(2, 1), (5, 9), (9, 1)],
        ];
        for case in cases {
            let btree: BTreeMap<i64, usize> = case.iter().copied().collect();
            let compact: BTreeMap<i16, u16> = case
                .iter()
                .map(|&(b, v)| (i16::try_from(b).unwrap(), u16::try_from(v).unwrap()))
                .collect();
            let want = median_max_bucket(&btree);
            let got = median_max_bucket_compact(&compact);
            assert_eq!(got, want, "case {case:?}");
        }
    }
}
