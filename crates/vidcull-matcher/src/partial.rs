use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use vidcull_core::types::FileId;
use vidcull_core::{Error, Result};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, FingerprintsRepo, PartialEdgeSpan, SimilarityEdge,
    SimilarityEdgesRepo, TrustLevel,
};
use vidcull_fingerprint::format;
use vidcull_fingerprint::hamming_distance;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

pub mod durable;
pub(crate) mod mih;

const PHASH_BITS: u32 = 64;

const VERIFY_MIN_VOTES: usize = 2;

#[allow(clippy::cast_possible_wrap)]
pub(crate) const VOTE_BUCKET_MS: i64 = vidcull_core::SPARSE_GRID_INTERVAL_MS as i64;

const MATCH_TOLERANCE_CAP_MS: i64 = 2 * VOTE_BUCKET_MS;

const MATCH_TOLERANCE_MS: i64 = {
    let chosen = 2 * VOTE_BUCKET_MS;
    if chosen > MATCH_TOLERANCE_CAP_MS {
        MATCH_TOLERANCE_CAP_MS
    } else {
        chosen
    }
};

fn offset_bucket(offset_ms: i64) -> i64 {
    offset_ms.div_euclid(VOTE_BUCKET_MS)
}

pub(crate) fn spanned_buckets(offset_ms: i64) -> std::ops::RangeInclusive<i64> {
    offset_bucket(offset_ms - MATCH_TOLERANCE_MS)..=offset_bucket(offset_ms + MATCH_TOLERANCE_MS)
}

pub const DEFAULT_SHARD_SOURCES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorParams {
    bands: u32,
    max_distance: u32,
    min_coverage_x1000: u32,
    min_scenes: usize,
    min_matched: usize,
}

impl AnchorParams {
    pub const DEFAULT_BANDS: u32 = 8;
    pub const DEFAULT_MAX_DISTANCE: u32 = 6;
    pub const DEFAULT_MIN_COVERAGE_X1000: u32 = 600;
    pub const DEFAULT_MIN_SCENES: usize = 3;
    pub const DEFAULT_MIN_MATCHED: usize = usize::MAX;

    pub fn new(
        bands: u32,
        max_distance: u32,
        min_coverage_x1000: u32,
        min_scenes: usize,
    ) -> Result<Self> {
        if bands == 0 || bands > PHASH_BITS || PHASH_BITS % bands != 0 {
            return Err(Error::Unsupported(format!(
                "anchor band count {bands} must be a divisor of {PHASH_BITS} (1, 2, 4, 8, 16, 32, 64)"
            )));
        }
        if max_distance > PHASH_BITS {
            return Err(Error::Unsupported(format!(
                "anchor max_distance {max_distance} cannot exceed {PHASH_BITS} bits"
            )));
        }
        if min_coverage_x1000 > 1000 {
            return Err(Error::Unsupported(format!(
                "anchor min_coverage_x1000 {min_coverage_x1000} cannot exceed 1000"
            )));
        }
        Ok(Self {
            bands,
            max_distance,
            min_coverage_x1000,
            min_scenes,
            min_matched: Self::DEFAULT_MIN_MATCHED,
        })
    }

    #[must_use]
    pub fn with_min_matched(mut self, min_matched: usize) -> Self {
        self.min_matched = min_matched;
        self
    }

    #[must_use]
    pub fn min_matched(&self) -> usize {
        self.min_matched
    }

    #[must_use]
    pub fn bands(&self) -> u32 {
        self.bands
    }

    #[must_use]
    pub fn max_distance(&self) -> u32 {
        self.max_distance
    }

    #[must_use]
    pub fn min_coverage_x1000(&self) -> u32 {
        self.min_coverage_x1000
    }

    #[must_use]
    pub fn min_scenes(&self) -> usize {
        self.min_scenes
    }

    #[must_use]
    pub fn band_bits(&self) -> u32 {
        PHASH_BITS / self.bands
    }

    #[must_use]
    fn required_matches(&self, clip_len: usize) -> usize {
        let by_coverage = (clip_len * self.min_coverage_x1000 as usize).div_ceil(1000);
        by_coverage.max(2)
    }

    fn for_each_band(&self, phash: u64, mut f: impl FnMut(u8, u64)) {
        let bits = self.band_bits();
        let mask = if bits >= PHASH_BITS {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        for band in 0..self.bands {
            let shift = bits * band;
            let value = (phash >> shift) & mask;
            f(u8::try_from(band).unwrap_or(u8::MAX), value);
        }
    }
}

impl Default for AnchorParams {
    fn default() -> Self {
        Self {
            bands: Self::DEFAULT_BANDS,
            max_distance: Self::DEFAULT_MAX_DISTANCE,
            min_coverage_x1000: Self::DEFAULT_MIN_COVERAGE_X1000,
            min_scenes: Self::DEFAULT_MIN_SCENES,
            min_matched: Self::DEFAULT_MIN_MATCHED,
        }
    }
}

#[must_use]
pub fn partial_clip_params() -> AnchorParams {
    AnchorParams::new(AnchorParams::DEFAULT_BANDS, 6, 1000, 3)
        .expect("valid partial-clip params")
        .with_min_matched(3)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Posting {
    pub(crate) file_id: FileId,
    pub(crate) scene_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipAlignment {
    pub source: FileId,
    pub source_offset: i64,
    pub matched_scenes: usize,
    pub clip_scenes: usize,
    pub coverage_x1000: u32,
    pub start_ms: u64,
    pub end_ms: u64,
    pub clip_start_ms: u64,
    pub clip_end_ms: u64,
}

const INTRO_OUTRO_SPAN_PCT_X1000: u64 = 370;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanPosition {
    Head,
    Mid,
    Tail,
}

fn span_position(mid_ms: u64, dur_ms: u64) -> SpanPosition {
    if dur_ms == 0 {
        return SpanPosition::Mid;
    }
    let third = dur_ms / 3;
    if mid_ms <= third {
        SpanPosition::Head
    } else if mid_ms >= dur_ms - third {
        SpanPosition::Tail
    } else {
        SpanPosition::Mid
    }
}

#[must_use]
pub fn is_intro_outro(
    alignment: &ClipAlignment,
    clip_dur_ms: Option<u64>,
    source_dur_ms: Option<u64>,
) -> bool {
    if alignment.clip_scenes == 0 {
        return false;
    }
    let (Some(clip_dur_ms), Some(source_dur_ms)) = (clip_dur_ms, source_dur_ms) else {
        return false;
    };
    if clip_dur_ms == 0 || source_dur_ms == 0 {
        return false;
    }

    let clip_span = alignment
        .clip_end_ms
        .saturating_sub(alignment.clip_start_ms);
    let source_span = alignment.end_ms.saturating_sub(alignment.start_ms);
    let clip_span_ratio_x1000 = clip_span.saturating_mul(1000) / clip_dur_ms;
    let source_span_ratio_x1000 = source_span.saturating_mul(1000) / source_dur_ms;
    let short_both = clip_span_ratio_x1000 <= INTRO_OUTRO_SPAN_PCT_X1000
        && source_span_ratio_x1000 <= INTRO_OUTRO_SPAN_PCT_X1000;

    let clip_mid = alignment.clip_start_ms + clip_span / 2;
    let source_mid = alignment.start_ms + source_span / 2;
    let clip_pos = span_position(clip_mid, clip_dur_ms);
    let source_pos = span_position(source_mid, source_dur_ms);
    let localized_both = clip_pos != SpanPosition::Mid && source_pos != SpanPosition::Mid;

    short_both && localized_both
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipMatch {
    pub clip: FileId,
    pub alignment: ClipAlignment,
}

#[derive(Debug, Clone)]
pub struct AnchorIndex {
    params: AnchorParams,
    buckets: BTreeMap<(u8, u64), Vec<Posting>>,
    sources: BTreeMap<FileId, Vec<SceneHash>>,
}

impl AnchorIndex {
    #[must_use]
    pub fn build<I>(corpus: I, params: AnchorParams) -> Self
    where
        I: IntoIterator<Item = (FileId, Tier2Fingerprint)>,
    {
        let mut index = Self {
            params,
            buckets: BTreeMap::new(),
            sources: BTreeMap::new(),
        };
        for (file_id, fp) in corpus {
            if index.sources.contains_key(&file_id) {
                index.retract(file_id);
            }
            for (scene_index, scene) in fp.scenes.iter().enumerate() {
                if scene.phash == 0 {
                    continue;
                }
                let posting = Posting {
                    file_id,
                    scene_index,
                };
                params.for_each_band(scene.phash, |band, value| {
                    index
                        .buckets
                        .entry((band, value))
                        .or_default()
                        .push(posting);
                });
            }
            index.sources.insert(file_id, fp.scenes);
        }
        index
    }

    fn retract(&mut self, file_id: FileId) {
        if let Some(scenes) = self.sources.get(&file_id) {
            for scene in scenes {
                if scene.phash == 0 {
                    continue;
                }
                self.params.for_each_band(scene.phash, |band, value| {
                    if let Some(list) = self.buckets.get_mut(&(band, value)) {
                        list.retain(|p| p.file_id != file_id);
                    }
                });
            }
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    #[must_use]
    pub fn params(&self) -> AnchorParams {
        self.params
    }

    fn candidates(&self, phash: u64) -> Vec<Posting> {
        let mut set: BTreeSet<Posting> = BTreeSet::new();
        self.params.for_each_band(phash, |band, value| {
            if let Some(list) = self.buckets.get(&(band, value)) {
                set.extend(list.iter().copied());
            }
        });
        set.into_iter().collect()
    }

    #[must_use]
    pub fn search(&self, clip: &[SceneHash], exclude: Option<FileId>) -> Vec<ClipAlignment> {
        let mut examined = 0;
        let mut near_miss = NearMissCounts::default();
        self.search_inner(clip, exclude, &mut examined, &mut near_miss)
    }

    fn search_inner(
        &self,
        clip: &[SceneHash],
        exclude: Option<FileId>,
        examined: &mut usize,
        near_miss: &mut NearMissCounts,
    ) -> Vec<ClipAlignment> {
        if clip.len() < self.params.min_scenes {
            return Vec::new();
        }

        let mut votes: BTreeMap<(FileId, i64), usize> = BTreeMap::new();
        for scene in clip {
            if scene.phash == 0 {
                continue;
            }
            let clip_ts = ms_to_i64(scene.timestamp_ms);
            for posting in self.candidates(scene.phash) {
                if Some(posting.file_id) == exclude {
                    continue;
                }
                let Some(src_scene) = self
                    .sources
                    .get(&posting.file_id)
                    .and_then(|scenes| scenes.get(posting.scene_index))
                else {
                    continue;
                };
                let offset_ms = ms_to_i64(src_scene.timestamp_ms) - clip_ts;
                for bucket in spanned_buckets(offset_ms) {
                    *votes.entry((posting.file_id, bucket)).or_default() += 1;
                }
            }
        }

        let mut best: BTreeMap<FileId, ClipAlignment> = BTreeMap::new();
        for (&(source, bucket), &count) in &votes {
            if count < VERIFY_MIN_VOTES {
                near_miss.single_vote += 1;
                continue;
            }
            let Some(scenes) = self.sources.get(&source) else {
                continue;
            };
            if scenes.len() <= clip.len() {
                continue;
            }
            *examined += 1;
            match self.verify(clip, source, scenes, bucket * VOTE_BUCKET_MS) {
                Some(alignment) => {
                    let keep = match best.get(&source) {
                        Some(prev) => is_better(&alignment, prev),
                        None => true,
                    };
                    if keep {
                        best.insert(source, alignment);
                    }
                }
                None => near_miss.below_coverage += 1,
            }
        }
        best.into_values().collect()
    }

    fn verify(
        &self,
        clip: &[SceneHash],
        source: FileId,
        scenes: &[SceneHash],
        offset: i64,
    ) -> Option<ClipAlignment> {
        verify_alignment(clip, source, scenes, offset, self.params)
    }
}

pub(crate) fn verify_alignment(
    clip: &[SceneHash],
    source: FileId,
    scenes: &[SceneHash],
    offset: i64,
    params: AnchorParams,
) -> Option<ClipAlignment> {
    if clip.is_empty() {
        return None;
    }
    let mut matched = 0usize;
    let mut start_ms: Option<u64> = None;
    let mut end_ms = 0u64;
    let mut clip_start_ms: Option<u64> = None;
    let mut clip_end_ms = 0u64;
    let mut src_ptr = 0usize;
    for clip_scene in clip {
        if clip_scene.phash == 0 {
            continue;
        }
        let center = ms_to_i64(clip_scene.timestamp_ms) + offset;
        let lo = center - MATCH_TOLERANCE_MS;
        let hi = center + MATCH_TOLERANCE_MS;
        while src_ptr < scenes.len() && ms_to_i64(scenes[src_ptr].timestamp_ms) < lo {
            src_ptr += 1;
        }
        let mut probe = src_ptr;
        while probe < scenes.len() && ms_to_i64(scenes[probe].timestamp_ms) <= hi {
            let src_scene = &scenes[probe];
            if src_scene.phash != 0
                && hamming_distance(clip_scene.phash, src_scene.phash) <= params.max_distance
            {
                matched += 1;
                start_ms.get_or_insert(src_scene.timestamp_ms);
                end_ms = src_scene.timestamp_ms;
                clip_start_ms.get_or_insert(clip_scene.timestamp_ms);
                clip_end_ms = clip_scene.timestamp_ms;
                src_ptr = probe + 1;
                break;
            }
            probe += 1;
        }
    }
    if matched < params.required_matches(clip.len()) && matched < params.min_matched {
        return None;
    }
    let coverage_x1000 = u32::try_from(matched * 1000 / clip.len()).unwrap_or(1000);
    let start = start_ms.unwrap_or(0);
    let clip_start = clip_start_ms.unwrap_or(0);
    let source_offset = ms_to_i64(start) - ms_to_i64(clip_start);
    Some(ClipAlignment {
        source,
        source_offset,
        matched_scenes: matched,
        clip_scenes: clip.len(),
        coverage_x1000,
        start_ms: start,
        end_ms,
        clip_start_ms: clip_start,
        clip_end_ms,
    })
}

pub(crate) fn ms_to_i64(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

fn is_better(candidate: &ClipAlignment, incumbent: &ClipAlignment) -> bool {
    candidate
        .matched_scenes
        .cmp(&incumbent.matched_scenes)
        .then_with(|| {
            incumbent
                .source_offset
                .abs()
                .cmp(&candidate.source_offset.abs())
        })
        .then_with(|| incumbent.source_offset.cmp(&candidate.source_offset))
        == std::cmp::Ordering::Greater
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NearMissCounts {
    below_coverage: usize,
    single_vote: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialClipPlan {
    pub matches: Vec<ClipMatch>,
    pub candidate_offsets_examined: usize,
    pub skipped_short: usize,
    pub dropped_below_coverage: usize,
    pub dropped_single_vote: usize,
}

#[must_use]
pub fn plan_partial_clips<I>(corpus: I, params: AnchorParams) -> PartialClipPlan
where
    I: IntoIterator<Item = (FileId, Tier2Fingerprint)>,
{
    let index = AnchorIndex::build(corpus, params);

    // Each clip's search only reads the shared, already-built `index` and
    // writes to its own local accumulators, so per-clip searches are
    // embarrassingly parallel. The final sort below already makes the merge
    // order-independent, so there's no determinism cost to searching clips
    // out of order across threads.
    let entries: Vec<(FileId, &[SceneHash])> = index
        .sources
        .iter()
        .map(|(&id, scenes)| (id, scenes.as_slice()))
        .collect();

    let per_clip: Vec<(Vec<ClipMatch>, usize, usize, NearMissCounts)> = entries
        .par_iter()
        .map(|&(clip_id, scenes)| {
            if scenes.len() < params.min_scenes {
                return (Vec::new(), 0, 1, NearMissCounts::default());
            }
            let mut examined = 0usize;
            let mut near_miss = NearMissCounts::default();
            let alignments = index.search_inner(scenes, Some(clip_id), &mut examined, &mut near_miss);
            let matches = alignments
                .into_iter()
                .map(|alignment| ClipMatch {
                    clip: clip_id,
                    alignment,
                })
                .collect();
            (matches, examined, 0, near_miss)
        })
        .collect();

    let mut matches: Vec<ClipMatch> = Vec::new();
    let mut candidate_offsets_examined = 0usize;
    let mut skipped_short = 0usize;
    let mut near_miss = NearMissCounts::default();
    for (m, examined, skipped, nm) in per_clip {
        matches.extend(m);
        candidate_offsets_examined += examined;
        skipped_short += skipped;
        near_miss.below_coverage += nm.below_coverage;
        near_miss.single_vote += nm.single_vote;
    }

    matches.sort_by(|x, y| {
        x.clip
            .cmp(&y.clip)
            .then_with(|| x.alignment.source.cmp(&y.alignment.source))
    });
    PartialClipPlan {
        matches,
        candidate_offsets_examined,
        skipped_short,
        dropped_below_coverage: near_miss.below_coverage,
        dropped_single_vote: near_miss.single_vote,
    }
}

#[must_use]
pub fn plan_partial_clips_scoped(
    corpus: &[(FileId, Tier2Fingerprint)],
    params: AnchorParams,
    _shard_sources: usize,
) -> PartialClipPlan {
    // Sharding used to rebuild a fresh AnchorIndex per shard and rescan the
    // *entire* corpus against each one (O(N^2/shard_sources) search_inner
    // calls), trying to bound the posting-index's peak size. That trade-off
    // doesn't pay for itself here: the only caller already holds every
    // Tier2Fingerprint in `corpus` resident in memory before this function is
    // even entered, so the index built over the whole corpus at once is no
    // heavier than the per-shard version — it just isn't rebuilt from scratch
    // N/shard_sources times. `_shard_sources` is kept for API/call-site
    // compatibility only. A genuinely memory-bounded large-corpus path
    // belongs in durable.rs's DB-paged `cold_build_paged`, not here.
    plan_partial_clips(corpus.iter().cloned(), params)
}

#[must_use]
pub fn plan_partial_clips_incremental(
    corpus: &[(FileId, Tier2Fingerprint)],
    prev_matches: &[ClipMatch],
    changed: &BTreeSet<FileId>,
    params: AnchorParams,
    shard_sources: usize,
) -> PartialClipPlan {
    let shard_sources = shard_sources.max(1);
    let present: BTreeSet<FileId> = corpus.iter().map(|(id, _)| *id).collect();

    let skipped_short = corpus
        .iter()
        .filter(|(_, fp)| fp.scenes.len() < params.min_scenes)
        .count();

    let mut matches: Vec<ClipMatch> = Vec::new();
    let mut candidate_offsets_examined = 0usize;
    let mut near_miss = NearMissCounts::default();

    for m in prev_matches {
        let both_present = present.contains(&m.clip) && present.contains(&m.alignment.source);
        let unchanged = !changed.contains(&m.clip) && !changed.contains(&m.alignment.source);
        if both_present && unchanged {
            matches.push(*m);
        }
    }

    let has_changed_clip = corpus
        .iter()
        .any(|(id, fp)| changed.contains(id) && fp.scenes.len() >= params.min_scenes);
    if has_changed_clip {
        for shard in corpus.chunks(shard_sources) {
            let index = AnchorIndex::build(shard.iter().cloned(), params);
            for (clip_id, fp) in corpus {
                if !changed.contains(clip_id) || fp.scenes.len() < params.min_scenes {
                    continue;
                }
                let alignments = index.search_inner(
                    &fp.scenes,
                    Some(*clip_id),
                    &mut candidate_offsets_examined,
                    &mut near_miss,
                );
                for alignment in alignments {
                    matches.push(ClipMatch {
                        clip: *clip_id,
                        alignment,
                    });
                }
            }
        }
    }

    let changed_sources: Vec<(FileId, Tier2Fingerprint)> = corpus
        .iter()
        .filter(|(id, _)| changed.contains(id))
        .cloned()
        .collect();
    for shard in changed_sources.chunks(shard_sources) {
        let index = AnchorIndex::build(shard.iter().cloned(), params);
        for (clip_id, fp) in corpus {
            if changed.contains(clip_id) || fp.scenes.len() < params.min_scenes {
                continue;
            }
            let alignments = index.search_inner(
                &fp.scenes,
                Some(*clip_id),
                &mut candidate_offsets_examined,
                &mut near_miss,
            );
            for alignment in alignments {
                matches.push(ClipMatch {
                    clip: *clip_id,
                    alignment,
                });
            }
        }
    }

    matches.sort_by(|x, y| {
        x.clip
            .cmp(&y.clip)
            .then_with(|| x.alignment.source.cmp(&y.alignment.source))
    });
    PartialClipPlan {
        matches,
        candidate_offsets_examined,
        skipped_short,
        dropped_below_coverage: near_miss.below_coverage,
        dropped_single_vote: near_miss.single_vote,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PartialRebuildOutcome {
    pub groups_cleared: usize,
    pub groups_created: usize,
    pub members_added: usize,
    pub edges_added: usize,
    pub skipped_short: usize,
    pub dropped_below_coverage: usize,
    pub dropped_single_vote: usize,
    pub tagged_intro_outro: usize,
}

pub(crate) fn load_active_durations(files: &FilesRepo<'_>) -> Result<BTreeMap<FileId, u64>> {
    Ok(files
        .list_active()?
        .into_iter()
        .filter_map(|f| f.duration.map(|d| (f.id, d.as_millis())))
        .collect())
}

pub fn rebuild_partial_clip_groups(
    db: &mut Database,
    params: AnchorParams,
    now_unix_s: i64,
) -> Result<PartialRebuildOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let fingerprints = FingerprintsRepo::new(conn);
        let edges_repo = SimilarityEdgesRepo::new(conn);
        let files = FilesRepo::new(conn);

        let groups_cleared = groups.delete_by_trust(TrustLevel::Possible)?;

        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
        for (file_id, blob) in fingerprints.list_active_tier2()? {
            corpus.push((file_id, format::decode_tier2(&blob)?));
        }

        let plan = plan_partial_clips_scoped(&corpus, params, DEFAULT_SHARD_SOURCES);
        let durations = load_active_durations(&files)?;
        write_partial_plan(
            &groups,
            &edges_repo,
            &plan,
            now_unix_s,
            groups_cleared,
            &durations,
        )
    })
}

pub fn rebuild_partial_clip_groups_incremental(
    db: &mut Database,
    params: AnchorParams,
    now_unix_s: i64,
    changed: &BTreeSet<FileId>,
) -> Result<PartialRebuildOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let fingerprints = FingerprintsRepo::new(conn);
        let edges_repo = SimilarityEdgesRepo::new(conn);
        let files = FilesRepo::new(conn);

        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
        for (file_id, blob) in fingerprints.list_active_tier2()? {
            corpus.push((file_id, format::decode_tier2(&blob)?));
        }
        let scene_count: BTreeMap<FileId, usize> = corpus
            .iter()
            .map(|(id, fp)| (*id, fp.scenes.len()))
            .collect();

        let prev_matches: Vec<ClipMatch> = edges_repo
            .list_by_trust(TrustLevel::Possible)?
            .iter()
            .map(|e| reconstruct_prev_match(e, &scene_count))
            .collect();

        let groups_cleared = groups.delete_by_trust(TrustLevel::Possible)?;
        let plan = plan_partial_clips_incremental(
            &corpus,
            &prev_matches,
            changed,
            params,
            DEFAULT_SHARD_SOURCES,
        );
        let durations = load_active_durations(&files)?;
        write_partial_plan(
            &groups,
            &edges_repo,
            &plan,
            now_unix_s,
            groups_cleared,
            &durations,
        )
    })
}

pub(crate) fn reconstruct_prev_match(
    edge: &SimilarityEdge,
    scene_count: &BTreeMap<FileId, usize>,
) -> ClipMatch {
    let (a, b) = (edge.file_a, edge.file_b);
    let (clip, source) = match (scene_count.get(&a), scene_count.get(&b)) {
        (Some(ca), Some(cb)) if cb < ca => (b, a),
        _ => (a, b),
    };
    let span = edge.partial_span;
    ClipMatch {
        clip,
        alignment: ClipAlignment {
            source,
            source_offset: 0,
            matched_scenes: span.map_or(0, |s| s.matched_scenes),
            clip_scenes: span.map_or(0, |s| s.clip_scenes),
            coverage_x1000: u32::try_from(edge.score_x1000.clamp(0, 1000)).unwrap_or(0),
            start_ms: span.map_or(0, |s| s.source_start_ms),
            end_ms: span.map_or(0, |s| s.source_end_ms),
            clip_start_ms: span.map_or(0, |s| s.clip_start_ms),
            clip_end_ms: span.map_or(0, |s| s.clip_end_ms),
        },
    }
}

pub(crate) fn write_partial_plan(
    groups: &DuplicateGroupsRepo<'_>,
    edges_repo: &SimilarityEdgesRepo<'_>,
    plan: &PartialClipPlan,
    now_unix_s: i64,
    groups_cleared: usize,
    durations: &BTreeMap<FileId, u64>,
) -> Result<PartialRebuildOutcome> {
    let mut outcome = PartialRebuildOutcome {
        groups_cleared,
        skipped_short: plan.skipped_short,
        dropped_below_coverage: plan.dropped_below_coverage,
        dropped_single_vote: plan.dropped_single_vote,
        ..PartialRebuildOutcome::default()
    };
    for m in &plan.matches {
        let gid = groups.create(TrustLevel::Possible, now_unix_s)?;
        groups.add_member(gid, m.clip)?;
        groups.add_member(gid, m.alignment.source)?;
        let tagged = is_intro_outro(
            &m.alignment,
            durations.get(&m.clip).copied(),
            durations.get(&m.alignment.source).copied(),
        );
        edges_repo.insert(&SimilarityEdge {
            group_id: gid,
            file_a: m.clip,
            file_b: m.alignment.source,
            score_x1000: coverage_score_x1000(m.alignment.coverage_x1000),
            partial_span: (m.alignment.clip_scenes > 0).then_some(PartialEdgeSpan {
                clip_start_ms: m.alignment.clip_start_ms,
                clip_end_ms: m.alignment.clip_end_ms,
                source_start_ms: m.alignment.start_ms,
                source_end_ms: m.alignment.end_ms,
                matched_scenes: m.alignment.matched_scenes,
                clip_scenes: m.alignment.clip_scenes,
            }),
            intro_outro: tagged,
        })?;
        outcome.groups_created += 1;
        outcome.members_added += 2;
        outcome.edges_added += 1;
        if tagged {
            outcome.tagged_intro_outro += 1;
        }
    }
    Ok(outcome)
}

fn coverage_score_x1000(coverage_x1000: u32) -> i32 {
    i32::try_from(coverage_x1000.min(1000)).unwrap_or(1000)
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

    fn flip_low_bits(h: u64, n: u32) -> u64 {
        if n == 0 {
            return h;
        }
        let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        h ^ mask
    }

    fn scene(ts: u64, phash: u64) -> SceneHash {
        SceneHash {
            timestamp_ms: ts,
            phash,
        }
    }

    fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
        let mut state = seed;
        let scenes = (0..n)
            .map(|i| scene(i as u64 * 1000, splitmix64(&mut state) | 1))
            .collect();
        Tier2Fingerprint { scenes }
    }

    fn clip_of(
        source: &Tier2Fingerprint,
        start: usize,
        len: usize,
        perturb: u32,
    ) -> Tier2Fingerprint {
        let scenes = source.scenes[start..start + len]
            .iter()
            .enumerate()
            .map(|(i, s)| scene(i as u64 * 1000, flip_low_bits(s.phash, perturb)))
            .collect();
        Tier2Fingerprint { scenes }
    }

    #[test]
    fn verify_alignment_empty_clip_returns_none() {
        let source = source_seq(1, 8);
        let result = verify_alignment(&[], FileId(1), &source.scenes, 0, partial_clip_params());
        assert!(
            result.is_none(),
            "empty clip yields no alignment, not a panic"
        );
    }

    #[test]
    fn reconstruct_prev_match_reads_persisted_span_with_orientation() {
        let edge = SimilarityEdge {
            group_id: 1,
            file_a: FileId(1),
            file_b: FileId(2),
            score_x1000: 600,
            partial_span: Some(PartialEdgeSpan {
                clip_start_ms: 0,
                clip_end_ms: 5_000,
                source_start_ms: 10_000,
                source_end_ms: 15_000,
                matched_scenes: 6,
                clip_scenes: 6,
            }),
            intro_outro: false,
        };
        let scene_count: BTreeMap<FileId, usize> =
            [(FileId(1), 40), (FileId(2), 6)].into_iter().collect();
        let m = reconstruct_prev_match(&edge, &scene_count);
        assert_eq!(m.clip, FileId(2), "clip is the strictly shorter video");
        assert_eq!(m.alignment.source, FileId(1), "source is the longer video");
        assert_eq!(m.alignment.clip_start_ms, 0);
        assert_eq!(m.alignment.clip_end_ms, 5_000);
        assert_eq!(
            m.alignment.start_ms, 10_000,
            "source-side span maps to start_ms"
        );
        assert_eq!(m.alignment.end_ms, 15_000);
        assert_eq!(m.alignment.matched_scenes, 6);
        assert_eq!(m.alignment.clip_scenes, 6);
        assert_eq!(m.alignment.coverage_x1000, 600);
    }

    #[test]
    fn reconstruct_prev_match_legacy_null_span_yields_zero_offsets() {
        let edge = SimilarityEdge {
            group_id: 1,
            file_a: FileId(1),
            file_b: FileId(2),
            score_x1000: 700,
            partial_span: None,
            intro_outro: false,
        };
        let scene_count: BTreeMap<FileId, usize> =
            [(FileId(1), 40), (FileId(2), 6)].into_iter().collect();
        let m = reconstruct_prev_match(&edge, &scene_count);
        assert_eq!(m.clip, FileId(2));
        assert_eq!(m.alignment.source, FileId(1));
        assert_eq!(m.alignment.clip_start_ms, 0);
        assert_eq!(m.alignment.start_ms, 0);
        assert_eq!(m.alignment.matched_scenes, 0);
        assert_eq!(m.alignment.clip_scenes, 0);
        assert_eq!(m.alignment.coverage_x1000, 700);
    }

    #[test]
    fn params_reject_invalid_configs() {
        assert!(
            AnchorParams::new(3, 6, 600, 3).is_err(),
            "non-divisor bands"
        );
        assert!(
            AnchorParams::new(8, 65, 600, 3).is_err(),
            "oversized distance"
        );
        assert!(AnchorParams::new(8, 6, 1001, 3).is_err(), "coverage > 1.0");
        assert!(AnchorParams::new(8, 6, 600, 3).is_ok());
    }

    #[test]
    fn default_params_match_6_1_band_layout() {
        let p = AnchorParams::default();
        assert_eq!(p.bands(), 8);
        assert_eq!(p.band_bits(), 8);
        assert_eq!(p.max_distance(), 6);
        assert_eq!(p.min_coverage_x1000(), 600);
        assert_eq!(p.min_scenes(), 3);
    }

    #[test]
    fn required_matches_is_ceil_coverage_but_at_least_two() {
        let p = AnchorParams::default();
        assert_eq!(p.required_matches(3), 2);
        assert_eq!(p.required_matches(10), 6);
        assert_eq!(p.required_matches(2), 2);
    }

    #[test]
    fn min_matched_accepts_short_overlap_in_a_long_clip() {
        let source = source_seq(0x5151, 30);
        let mut state = 0x9999u64;
        let mut scenes: Vec<SceneHash> = source.scenes[12..16]
            .iter()
            .enumerate()
            .map(|(i, s)| scene(i as u64 * 1000, s.phash))
            .collect();
        for k in 0..6usize {
            scenes.push(scene((4 + k) as u64 * 1000, splitmix64(&mut state) | 1));
        }
        let clip = Tier2Fingerprint { scenes };
        let corpus = [(FileId(1), clip), (FileId(2), source)];

        let plan = plan_partial_clips(corpus.clone(), AnchorParams::default());
        assert!(
            plan.matches.is_empty(),
            "the coverage gate alone rejects a 40 % overlap"
        );

        let plan = plan_partial_clips(corpus, AnchorParams::default().with_min_matched(4));
        assert_eq!(
            plan.matches.len(),
            1,
            "min_matched=4 accepts the 4-scene overlap on its own (OR gate): {:?}",
            plan.matches,
        );
    }

    #[test]
    fn min_matched_default_is_disabled_and_with_min_matched_sets_it() {
        assert_eq!(AnchorParams::default().min_matched(), usize::MAX);
        assert_eq!(AnchorParams::default().with_min_matched(4).min_matched(), 4);
    }

    fn adjacent_segment_pair(
        seed_a: u64,
        seed_b: u64,
        a_scenes: usize,
        b_scenes: usize,
        shared: usize,
    ) -> (Tier2Fingerprint, Tier2Fingerprint) {
        assert!(
            shared <= a_scenes && shared <= b_scenes,
            "shared > either video length"
        );

        let mut state_a = seed_a;
        let mut a_unique_scenes: Vec<SceneHash> = Vec::new();
        for i in 0..(a_scenes - shared) {
            a_unique_scenes.push(scene(i as u64 * 1000, splitmix64(&mut state_a) | 1));
        }
        let mut shared_scenes: Vec<SceneHash> = Vec::new();
        for k in 0..shared {
            let ts = (a_scenes - shared + k) as u64 * 1000;
            shared_scenes.push(scene(ts, splitmix64(&mut state_a) | 1));
        }

        let mut a_full = a_unique_scenes;
        a_full.extend(shared_scenes.iter().copied());
        let fp_a = Tier2Fingerprint { scenes: a_full };

        let mut state_b = seed_b;
        let mut b_scenes_vec: Vec<SceneHash> = Vec::new();
        for (k, s) in shared_scenes.iter().enumerate() {
            b_scenes_vec.push(scene(k as u64 * 1000, s.phash));
        }
        for k in 0..(b_scenes - shared) {
            b_scenes_vec.push(scene(
                (shared + k) as u64 * 1000,
                splitmix64(&mut state_b) | 1,
            ));
        }
        let fp_b = Tier2Fingerprint {
            scenes: b_scenes_vec,
        };

        (fp_a, fp_b)
    }

    #[test]
    fn adjacent_segment_accepted_with_min_matched_rejected_coverage_only() {
        let (fp_a, fp_b) = adjacent_segment_pair(0xA_FEED, 0xB_FEED, 15, 10, 3);

        let a_tail = &fp_a.scenes[(15 - 3)..];
        let b_head = &fp_b.scenes[..3];
        for (a, b) in a_tail.iter().zip(b_head.iter()) {
            assert_eq!(a.phash, b.phash, "shared scenes must be identical");
        }

        let corpus = vec![(FileId(1), fp_a), (FileId(2), fp_b)];

        let plan_default = plan_partial_clips(corpus.clone(), AnchorParams::default());
        assert!(
            plan_default.matches.is_empty(),
            "default coverage gate must reject the adjacent-segment pair (30 % < 60 %): {:?}",
            plan_default.matches,
        );

        let params_with_min_matched = AnchorParams::new(8, 6, 600, 3)
            .expect("valid params")
            .with_min_matched(3);
        let plan_min_matched = plan_partial_clips(corpus.clone(), params_with_min_matched);
        assert_eq!(
            plan_min_matched.matches.len(),
            1,
            "min_matched=3 accepts the 3-scene adjacent-segment overlap (OR gate, \
             accepted precision tradeoff — POSSIBLE/review-only): {:?}",
            plan_min_matched.matches,
        );
    }

    #[test]
    fn adjacent_segment_five_shared_scenes_accepted_with_min_matched() {
        let (fp_a, fp_b) = adjacent_segment_pair(0xDECA, 0xFBEE, 25, 20, 5);

        let corpus = vec![(FileId(1), fp_a), (FileId(2), fp_b)];

        let plan_default = plan_partial_clips(corpus.clone(), AnchorParams::default());
        assert!(
            plan_default.matches.is_empty(),
            "coverage-only default must reject the 25 % adjacent-segment pair: {:?}",
            plan_default.matches,
        );

        let params = AnchorParams::new(8, 6, 600, 3)
            .expect("valid params")
            .with_min_matched(5);
        let plan = plan_partial_clips(corpus, params);
        assert_eq!(
            plan.matches.len(),
            1,
            "min_matched=5 accepts the 5-scene adjacent-segment overlap (OR gate, \
             accepted precision tradeoff): {:?}",
            plan.matches,
        );
    }

    #[test]
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    fn min_matched_floor_no_false_positives_on_random_corpus() {
        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
        for i in 0..8u64 {
            corpus.push((
                FileId(i as i64 + 1),
                source_seq(0x1000 + i, 20 + i as usize * 3),
            ));
        }
        let source = source_seq(0xBEEF, 40);
        let clip = clip_of(&source, 10, 8, 3);
        corpus.push((FileId(9), clip));
        corpus.push((FileId(10), source));

        let params = AnchorParams::new(8, 6, 600, 3)
            .expect("params")
            .with_min_matched(3);
        let plan = plan_partial_clips(corpus, params);
        assert_eq!(
            plan.matches.len(),
            1,
            "exactly the real clip pair, no false positives: {:?}",
            plan.matches
        );
        assert_eq!(plan.matches[0].clip, FileId(9));
        assert_eq!(plan.matches[0].alignment.source, FileId(10));
        assert_eq!(plan.matches[0].alignment.matched_scenes, 8);
    }

    #[test]
    fn finds_a_thirty_second_clip_inside_a_long_source() {
        let source = source_seq(0x1111, 60);
        let clip = clip_of(&source, 20, 6, 4);
        let index = AnchorIndex::build([(FileId(1), source.clone())], AnchorParams::default());

        let hits = index.search(&clip.scenes, None);
        assert_eq!(hits.len(), 1, "clip located in exactly the one source");
        let a = hits[0];
        assert_eq!(a.source, FileId(1));
        assert_eq!(
            a.source_offset, 20_000,
            "clip scene 0 (ts 0) aligns at source scene 20 (ts 20_000 ms) — \
             source_offset is now the exact ms gap"
        );
        assert_eq!(a.matched_scenes, 6);
        assert_eq!(a.clip_scenes, 6);
        assert_eq!(a.coverage_x1000, 1000);
        assert_eq!(a.start_ms, 20_000);
        assert_eq!(a.end_ms, 25_000);
        assert!(a.start_ms < a.end_ms, "matched span is timestamp-monotonic");
        assert_eq!(a.clip_start_ms, 0);
        assert_eq!(a.clip_end_ms, 5_000);
        assert!(
            a.clip_end_ms < a.start_ms,
            "clip-side range differs from source-side when offset != 0"
        );
    }

    #[test]
    fn unrelated_clip_does_not_match() {
        let source = source_seq(0x2222, 40);
        let other = source_seq(0xDEAD, 8);
        let index = AnchorIndex::build([(FileId(1), source)], AnchorParams::default());
        assert!(
            index.search(&other.scenes, None).is_empty(),
            "an unrelated sequence must not align anywhere",
        );
    }

    #[test]
    fn equal_length_match_is_not_a_partial_clip() {
        let source = source_seq(0x3333, 12);
        let whole = clip_of(&source, 0, 12, 3);
        let index = AnchorIndex::build([(FileId(1), source)], AnchorParams::default());
        assert!(
            index.search(&whole.scenes, None).is_empty(),
            "equal-length sequence is not a partial clip",
        );
    }

    #[test]
    fn clip_below_min_scenes_is_skipped() {
        let source = source_seq(0x4444, 30);
        let tiny = clip_of(&source, 5, 2, 0);
        let index = AnchorIndex::build([(FileId(1), source)], AnchorParams::default());
        assert!(
            index.search(&tiny.scenes, None).is_empty(),
            "clips shorter than min_scenes are not attempted",
        );
    }

    #[test]
    fn partial_coverage_below_floor_is_rejected() {
        let source = source_seq(0x5555, 40);
        let mut clip = clip_of(&source, 10, 10, 0);
        for s in clip.scenes.iter_mut().skip(4) {
            s.phash = flip_low_bits(s.phash, 40);
        }
        let index = AnchorIndex::build([(FileId(1), source)], AnchorParams::default());
        assert!(
            index.search(&clip.scenes, None).is_empty(),
            "40 % coverage must not clear the 60 % floor",
        );
    }

    #[test]
    fn below_coverage_near_miss_is_counted_not_matched() {
        let source = source_seq(0x5555, 40);
        let mut clip = clip_of(&source, 10, 10, 0);
        for s in clip.scenes.iter_mut().skip(4) {
            s.phash = flip_low_bits(s.phash, 40);
        }
        let corpus = [(FileId(1), clip), (FileId(2), source)];

        let plan = plan_partial_clips(corpus, AnchorParams::default());
        assert!(
            plan.matches.is_empty(),
            "the 40 % near-miss must NOT be force-matched (recall ceiling unchanged): {:?}",
            plan.matches,
        );
        assert!(
            plan.dropped_below_coverage > 0,
            "the below-coverage near-miss must be tallied for observability",
        );
    }

    #[test]
    fn single_vote_near_miss_is_counted_not_matched() {
        let source = source_seq(0x6363, 30);
        let mut state = 0x00C0_FFEEu64;
        let clip = Tier2Fingerprint {
            scenes: vec![
                scene(0, source.scenes[5].phash),
                scene(1000, source.scenes[20].phash),
                scene(2000, splitmix64(&mut state) | 1),
            ],
        };
        let corpus = [(FileId(1), clip), (FileId(2), source)];

        let plan = plan_partial_clips(corpus, AnchorParams::default());
        assert!(
            plan.matches.is_empty(),
            "single-vote offsets must NOT be matched (vote threshold unchanged): {:?}",
            plan.matches,
        );
        assert!(
            plan.dropped_single_vote > 0,
            "single-vote Hough drops must be tallied for observability",
        );
    }

    #[test]
    fn near_miss_counts_are_shard_invariant() {
        let source = source_seq(0x5555, 40);
        let mut clip = clip_of(&source, 10, 10, 0);
        for s in clip.scenes.iter_mut().skip(4) {
            s.phash = flip_low_bits(s.phash, 40);
        }
        let corpus = vec![(FileId(1), clip), (FileId(2), source)];

        let full = plan_partial_clips(corpus.iter().cloned(), AnchorParams::default());
        assert!(
            full.dropped_below_coverage > 0,
            "fixture must drop a near-miss"
        );
        for shard in [1usize, 2, DEFAULT_SHARD_SOURCES] {
            let scoped = plan_partial_clips_scoped(&corpus, AnchorParams::default(), shard);
            assert_eq!(
                scoped.dropped_below_coverage, full.dropped_below_coverage,
                "shard {shard}: below-coverage tally must match the single-index plan",
            );
            assert_eq!(
                scoped.dropped_single_vote, full.dropped_single_vote,
                "shard {shard}: single-vote tally must match the single-index plan",
            );
        }
    }

    #[test]
    fn compilation_matches_each_component_clip() {
        let a = source_seq(0xAAAA, 8);
        let b = source_seq(0xBBBB, 9);
        let mut comp_scenes: Vec<SceneHash> = Vec::new();
        let mut t = 0u64;
        for s in a.scenes.iter().chain(b.scenes.iter()) {
            comp_scenes.push(scene(t, flip_low_bits(s.phash, 3)));
            t += 1000;
        }
        let compilation = Tier2Fingerprint {
            scenes: comp_scenes,
        };

        let plan = plan_partial_clips(
            [(FileId(1), a), (FileId(2), b), (FileId(3), compilation)],
            AnchorParams::default(),
        );
        let pairs: Vec<(FileId, FileId)> = plan
            .matches
            .iter()
            .map(|m| (m.clip, m.alignment.source))
            .collect();
        assert!(
            pairs.contains(&(FileId(1), FileId(3))),
            "A is a clip of the compilation"
        );
        assert!(
            pairs.contains(&(FileId(2), FileId(3))),
            "B is a clip of the compilation"
        );
        assert_eq!(pairs.len(), 2, "exactly the two component matches");
        let b_match = plan
            .matches
            .iter()
            .find(|m| m.clip == FileId(2))
            .expect("B match");
        assert_eq!(b_match.alignment.source_offset, 8_000);
    }

    #[test]
    fn plan_is_deterministic() {
        let source = source_seq(0x9999, 50);
        let c1 = clip_of(&source, 0, 6, 2);
        let c2 = clip_of(&source, 30, 5, 3);
        let corpus = || {
            [
                (FileId(10), source.clone()),
                (FileId(3), c1.clone()),
                (FileId(7), c2.clone()),
            ]
        };
        let first = plan_partial_clips(corpus(), AnchorParams::default());
        let second = plan_partial_clips(corpus(), AnchorParams::default());
        assert_eq!(first, second);
        assert_eq!(
            first
                .matches
                .iter()
                .map(|m| (m.clip, m.alignment.source))
                .collect::<Vec<_>>(),
            vec![(FileId(3), FileId(10)), (FileId(7), FileId(10))],
        );
    }

    fn planted_corpus(sources: usize) -> Vec<(FileId, Tier2Fingerprint)> {
        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
        for s in 0..sources {
            let seed = 0xC0DE_0000 + u64::try_from(s).unwrap();
            let source = source_seq(seed, 40);
            let source_id = FileId(i64::try_from(s).unwrap() + 1);
            let clip = clip_of(&source, 10, 6, 3);
            let clip_id = FileId(10_000 + i64::try_from(s).unwrap());
            corpus.push((source_id, source));
            corpus.push((clip_id, clip));
        }
        corpus
    }

    #[test]
    fn scoped_matches_full_plan_across_shard_sizes() {
        let corpus = planted_corpus(20);
        let full = plan_partial_clips(corpus.clone(), AnchorParams::default());
        assert!(!full.matches.is_empty(), "fixture must plant real matches");

        for shard in [1usize, 2, 3, 7, 20, 100, DEFAULT_SHARD_SOURCES] {
            let scoped = plan_partial_clips_scoped(&corpus, AnchorParams::default(), shard);
            assert_eq!(
                scoped, full,
                "shard size {shard} must reproduce the full single-index plan",
            );
        }
    }

    #[test]
    fn scoped_one_shard_equals_full_and_huge_shard_is_one_pass() {
        let corpus = planted_corpus(8);
        let full = plan_partial_clips(corpus.clone(), AnchorParams::default());
        let one_pass = plan_partial_clips_scoped(&corpus, AnchorParams::default(), corpus.len());
        let huge = plan_partial_clips_scoped(&corpus, AnchorParams::default(), usize::MAX);
        assert_eq!(one_pass, full);
        assert_eq!(huge, full);
    }

    #[test]
    fn scoped_zero_shard_is_treated_as_one() {
        let corpus = planted_corpus(5);
        let full = plan_partial_clips(corpus.clone(), AnchorParams::default());
        let scoped = plan_partial_clips_scoped(&corpus, AnchorParams::default(), 0);
        assert_eq!(scoped, full);
    }

    #[test]
    fn scoped_counts_short_clips_once_not_per_shard() {
        let mut corpus = planted_corpus(6);
        for k in 0..3i64 {
            corpus.push((
                FileId(50_000 + k),
                source_seq(0xBEEF + u64::try_from(k).unwrap(), 2),
            ));
        }
        let full = plan_partial_clips(corpus.clone(), AnchorParams::default());
        let scoped = plan_partial_clips_scoped(&corpus, AnchorParams::default(), 2);
        assert_eq!(scoped.skipped_short, full.skipped_short);
        assert_eq!(scoped.skipped_short, 3, "exactly the three short videos");
    }

    #[test]
    fn zero_scene_does_not_anchor_or_count() {
        let source = source_seq(0x7777, 30);
        let mut clip = clip_of(&source, 5, 6, 2);
        clip.scenes[2].phash = 0;
        let index = AnchorIndex::build([(FileId(1), source)], AnchorParams::default());
        let hits = index.search(&clip.scenes, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched_scenes, 5, "the zero scene does not match");
        assert_eq!(
            hits[0].clip_scenes, 6,
            "but still counts in the denominator"
        );
    }

    #[test]
    fn candidate_offsets_stay_well_below_all_pairs() {
        const VIDEOS: i64 = 200;
        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = (0..VIDEOS)
            .map(|i| {
                (
                    FileId(i + 1),
                    source_seq(0x100 + u64::try_from(i).unwrap(), 12),
                )
            })
            .collect();
        let base = corpus[0].1.clone();
        for k in 0..5usize {
            corpus.push((
                FileId(1000 + i64::try_from(k).unwrap()),
                clip_of(&base, k, 5, 2),
            ));
        }

        let plan = plan_partial_clips(corpus, AnchorParams::default());
        let total = usize::try_from(VIDEOS).unwrap() + 5;
        let naive = total * total;
        assert!(
            plan.candidate_offsets_examined < naive / 10,
            "examined {} of naive {} — anchor voting is not pruning",
            plan.candidate_offsets_examined,
            naive,
        );
        let found = plan.matches.iter().filter(|m| m.clip.0 >= 1000).count();
        assert_eq!(found, 5, "all five planted clips located");
    }

    #[test]
    fn coverage_score_widens_losslessly() {
        assert_eq!(coverage_score_x1000(1000), 1000);
        assert_eq!(coverage_score_x1000(667), 667);
        assert_eq!(coverage_score_x1000(0), 0);
    }

    #[test]
    fn incremental_with_all_changed_equals_full() {
        let corpus = planted_corpus(20);
        let full = plan_partial_clips(corpus.clone(), AnchorParams::default());
        assert!(!full.matches.is_empty(), "fixture must plant real matches");
        let changed: BTreeSet<FileId> = corpus.iter().map(|(id, _)| *id).collect();

        let incr = plan_partial_clips_incremental(
            &corpus,
            &[],
            &changed,
            AnchorParams::default(),
            DEFAULT_SHARD_SOURCES,
        );
        assert_eq!(
            incr.matches, full.matches,
            "matches must match the full plan"
        );
        assert_eq!(
            incr.candidate_offsets_examined, full.candidate_offsets_examined,
            "all-changed must examine the same offsets as the full pass",
        );
        assert_eq!(incr.skipped_short, full.skipped_short);
    }

    #[test]
    fn incremental_delta_matches_full_and_examines_far_fewer() {
        let params = AnchorParams::default();
        let old = planted_corpus(60);
        let prev = plan_partial_clips(old.clone(), params).matches;

        let mut new_corpus = old.clone();
        let mut changed: BTreeSet<FileId> = BTreeSet::new();

        let source0 = old[0].1.clone();
        let new_clip = clip_of(&source0, 25, 6, 3);
        let new_clip_id = FileId(90_001);
        new_corpus.push((new_clip_id, new_clip));
        changed.insert(new_clip_id);

        let mutate_clip_idx = 5;
        for s in &mut new_corpus[mutate_clip_idx].1.scenes {
            s.phash = flip_low_bits(s.phash, 40);
        }
        changed.insert(new_corpus[mutate_clip_idx].0);

        let existing_clip = old[3].1.clone();
        let mut embed = source_seq(0xFEED_BEEF, 40);
        for (k, s) in existing_clip.scenes.iter().enumerate() {
            embed.scenes[12 + k] = scene((12 + k) as u64 * 1000, s.phash);
        }
        let new_source_id = FileId(80_001);
        new_corpus.push((new_source_id, embed));
        changed.insert(new_source_id);

        let full_new = plan_partial_clips(new_corpus.clone(), params);
        let incr = plan_partial_clips_incremental(
            &new_corpus,
            &prev,
            &changed,
            params,
            DEFAULT_SHARD_SOURCES,
        );

        assert_eq!(
            incr.matches, full_new.matches,
            "incremental matches must equal a full rebuild of the new corpus",
        );
        assert!(
            incr.candidate_offsets_examined < full_new.candidate_offsets_examined / 3,
            "incremental examined {} offsets vs full {} — delta work is not bounded",
            incr.candidate_offsets_examined,
            full_new.candidate_offsets_examined,
        );
    }

    #[test]
    fn incremental_drops_matches_of_removed_files() {
        let params = AnchorParams::default();
        let old = planted_corpus(10);
        let prev = plan_partial_clips(old.clone(), params).matches;

        let new_corpus: Vec<(FileId, Tier2Fingerprint)> = old
            .iter()
            .filter(|(id, _)| *id != FileId(1))
            .cloned()
            .collect();
        let changed: BTreeSet<FileId> = BTreeSet::new();

        let full_new = plan_partial_clips(new_corpus.clone(), params);
        let incr = plan_partial_clips_incremental(
            &new_corpus,
            &prev,
            &changed,
            params,
            DEFAULT_SHARD_SOURCES,
        );
        assert_eq!(incr.matches, full_new.matches);
        assert!(
            !incr.matches.iter().any(|m| m.alignment.source == FileId(1)),
            "the removed source must not appear in any carried match",
        );
    }

    #[test]
    fn incremental_source_change_rediscovers_unchanged_clip() {
        let params = AnchorParams::default();
        let clip = source_seq(0xC11D, 6);
        let mut corpus = vec![(FileId(1), clip.clone())];
        let prev = plan_partial_clips(corpus.clone(), params).matches;
        assert!(prev.is_empty());

        let mut source = source_seq(0x5005, 40);
        for (k, s) in clip.scenes.iter().enumerate() {
            source.scenes[20 + k] = scene((20 + k) as u64 * 1000, s.phash);
        }
        corpus.push((FileId(2), source));
        let changed: BTreeSet<FileId> = [FileId(2)].into_iter().collect();

        let full = plan_partial_clips(corpus.clone(), params);
        let incr =
            plan_partial_clips_incremental(&corpus, &prev, &changed, params, DEFAULT_SHARD_SOURCES);
        assert_eq!(incr.matches, full.matches);
        assert_eq!(
            incr.matches.len(),
            1,
            "the clip is found inside the new source"
        );
        assert_eq!(incr.matches[0].clip, FileId(1));
        assert_eq!(incr.matches[0].alignment.source, FileId(2));
    }

    #[test]
    fn incremental_source_change_with_many_clips_matches_full() {
        let params = AnchorParams::default();
        let mut corpus = planted_corpus(40);
        let prev = plan_partial_clips(corpus.clone(), params).matches;

        let target_clip = corpus
            .iter()
            .find(|(id, _)| *id == FileId(10_000))
            .expect("planted clip of source 0")
            .1
            .clone();
        let mut embed = source_seq(0xD00D_F00D, 50);
        for (k, s) in target_clip.scenes.iter().enumerate() {
            embed.scenes[15 + k] = scene((15 + k) as u64 * 1000, s.phash);
        }
        let new_source_id = FileId(70_001);
        corpus.push((new_source_id, embed));
        let changed: BTreeSet<FileId> = [new_source_id].into_iter().collect();

        let full = plan_partial_clips(corpus.clone(), params);
        let incr =
            plan_partial_clips_incremental(&corpus, &prev, &changed, params, DEFAULT_SHARD_SOURCES);

        assert_eq!(
            incr.matches, full.matches,
            "the candidate filter must not change the match set",
        );
        assert!(
            incr.matches
                .iter()
                .any(|m| m.clip == FileId(10_000) && m.alignment.source == new_source_id),
            "the embedded clip must be rediscovered inside the changed source",
        );
        assert!(
            incr.candidate_offsets_examined < full.candidate_offsets_examined,
            "incremental examined {} offsets, full {} — the filter is not pruning",
            incr.candidate_offsets_examined,
            full.candidate_offsets_examined,
        );
    }

    #[test]
    fn incremental_is_deterministic() {
        let params = AnchorParams::default();
        let corpus = planted_corpus(12);
        let changed: BTreeSet<FileId> = [FileId(1), FileId(10_003)].into_iter().collect();
        let first =
            plan_partial_clips_incremental(&corpus, &[], &changed, params, DEFAULT_SHARD_SOURCES);
        let second =
            plan_partial_clips_incremental(&corpus, &[], &changed, params, DEFAULT_SHARD_SOURCES);
        assert_eq!(first, second);
    }

    #[test]
    fn verify_binds_exact_ts_scene_with_neighbors_in_window() {
        let mut state = 0xA11Cu64;
        let src_hashes: Vec<u64> = (0..6).map(|_| splitmix64(&mut state) | 1).collect();
        let source: Vec<SceneHash> = src_hashes
            .iter()
            .enumerate()
            .map(|(i, &h)| scene(i as u64 * 2500, h))
            .collect();
        let clip: Vec<SceneHash> = (0..3)
            .map(|i| scene(i as u64 * 2500, src_hashes[2 + i]))
            .collect();

        for n in [0usize, 1, 3] {
            assert!(
                hamming_distance(src_hashes[2], src_hashes[n])
                    > partial_clip_params().max_distance(),
                "neighbour source scene {n} must be pHash-ineligible for the test to be meaningful",
            );
        }

        for off in [5_000i64, 2_500, 7_500] {
            let a = verify_alignment(&clip, FileId(9), &source, off, partial_clip_params())
                .unwrap_or_else(|| panic!("offset seed {off} must align via the tolerance window"));
            assert_eq!(
                a.matched_scenes, 3,
                "all three exact-ts scenes bind (seed {off})"
            );
            assert_eq!(
                a.start_ms, 5_000,
                "first bound is the exact-ts source scene 2 (ts 5000), not a neighbour (seed {off})",
            );
            assert_eq!(
                a.end_ms, 10_000,
                "last bound is source scene 4 (ts 10000) (seed {off})"
            );
            assert_eq!(a.clip_start_ms, 0);
            assert_eq!(a.clip_end_ms, 5_000);
            assert_eq!(
                a.source_offset, 5_000,
                "canonical measured offset = 5000 ms regardless of the seed centre (seed {off})",
            );
        }
    }
}
