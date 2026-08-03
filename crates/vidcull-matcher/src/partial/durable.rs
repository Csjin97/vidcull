use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use vidcull_core::Result;
use vidcull_core::types::FileId;
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, FingerprintsRepo, MihPosting, PartialMihRepo,
    SimilarityEdgesRepo, SystemMetadataRepo, TrustLevel,
};
use vidcull_fingerprint::format;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

use super::mih::MultiIndexHash;
use super::{
    AnchorParams, ClipMatch, NearMissCounts, PartialClipPlan, PartialRebuildOutcome, Posting,
    VERIFY_MIN_VOTES, VOTE_BUCKET_MS, is_better, load_active_durations, ms_to_i64,
    plan_partial_clips, spanned_buckets, verify_alignment, write_partial_plan,
};

pub const DEFAULT_MIH_CHUNKS: u32 = 4;

pub const COLD_BUILD_PAGE: usize = 2048;

const RECONCILED_KEY: &str = "partial_index_reconciled";

const COLD_CHECKPOINT_KEY: &str = "partial_cold_checkpoint";

const PARTIAL_INDEX_PARAMS_FP_KEY: &str = "partial_index_params_fp";

fn format_cold_checkpoint(last_completed_page: usize, active_id_count: usize) -> String {
    format!("{last_completed_page}:{active_id_count}")
}

fn parse_cold_checkpoint(value: &str) -> Option<(usize, usize)> {
    let (page, count) = value.split_once(':')?;
    Some((page.parse().ok()?, count.parse().ok()?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobSource {
    Tier2,
    Partial,
}

#[derive(Debug, Clone)]
pub struct PartialClipIndex {
    params: AnchorParams,
    blob_source: BlobSource,
    scenes: BTreeMap<FileId, Vec<SceneHash>>,
    mih: MultiIndexHash,
    matches: BTreeMap<(FileId, FileId), ClipMatch>,
    short_count: usize,
    last_examined: usize,
    last_near_miss: NearMissCounts,
    last_rediscovered: usize,
    bootstrapped: bool,
    pending_persist: BTreeMap<FileId, Vec<SceneHash>>,
    pending_remove: BTreeSet<FileId>,
    full_reset: bool,
}

impl PartialClipIndex {
    #[must_use]
    pub fn new(params: AnchorParams) -> Self {
        Self::new_with_source(params, BlobSource::Tier2)
    }

    #[must_use]
    pub fn new_with_source(params: AnchorParams, blob_source: BlobSource) -> Self {
        Self {
            params,
            blob_source,
            scenes: BTreeMap::new(),
            mih: MultiIndexHash::new(DEFAULT_MIH_CHUNKS, params.max_distance()),
            matches: BTreeMap::new(),
            short_count: 0,
            last_examined: 0,
            last_near_miss: NearMissCounts::default(),
            last_rediscovered: 0,
            bootstrapped: false,
            pending_persist: BTreeMap::new(),
            pending_remove: BTreeSet::new(),
            full_reset: false,
        }
    }

    #[must_use]
    pub fn blob_source(&self) -> BlobSource {
        self.blob_source
    }

    fn params_fingerprint(&self) -> String {
        let p = self.params;
        format!(
            "v1;src={:?};bands={};maxd={};cov={};mm={};ms={}",
            self.blob_source,
            p.bands(),
            p.max_distance(),
            p.min_coverage_x1000(),
            p.min_matched(),
            p.min_scenes(),
        )
    }

    fn provenance_reconciled(&self, db: &Database) -> Result<bool> {
        let meta = SystemMetadataRepo::new(db.conn());
        if !meta.contains(RECONCILED_KEY)? {
            return Ok(false);
        }
        Ok(meta.get(PARTIAL_INDEX_PARAMS_FP_KEY)? == Some(self.params_fingerprint()))
    }

    #[must_use]
    pub fn params(&self) -> AnchorParams {
        self.params
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scenes.is_empty()
    }

    #[must_use]
    pub fn last_rediscovered(&self) -> usize {
        self.last_rediscovered
    }

    #[must_use]
    pub fn plan(&self) -> PartialClipPlan {
        PartialClipPlan {
            matches: self.matches.values().copied().collect(),
            candidate_offsets_examined: self.last_examined,
            skipped_short: self.short_count,
            dropped_below_coverage: self.last_near_miss.below_coverage,
            dropped_single_vote: self.last_near_miss.single_vote,
        }
    }

    #[must_use]
    pub fn matches(&self) -> Vec<ClipMatch> {
        self.matches.values().copied().collect()
    }

    #[must_use]
    pub fn mih_candidates_count(&self, phash: u64) -> usize {
        self.mih.candidates(phash).len()
    }

    pub fn upsert(&mut self, file_id: FileId, scenes: Vec<SceneHash>) {
        let old_len = self.remove_postings_and_scenes(file_id);
        let new_len = scenes.len();
        for (scene_index, scene) in scenes.iter().enumerate() {
            self.mih.insert(
                scene.phash,
                Posting {
                    file_id,
                    scene_index,
                },
            );
        }
        self.scenes.insert(file_id, scenes);
        self.update_short_count(old_len, Some(new_len));
        self.drop_matches_touching(file_id);
    }

    pub fn remove(&mut self, file_id: FileId) {
        let old_len = self.remove_postings_and_scenes(file_id);
        self.update_short_count(old_len, None);
        self.drop_matches_touching(file_id);
    }

    pub fn rediscover(&mut self, changed: &BTreeSet<FileId>) {
        for &file_id in changed {
            self.drop_matches_touching(file_id);
        }
        let mut examined = 0usize;
        let mut near_miss = NearMissCounts::default();
        let mut rediscovered = 0usize;
        for &file_id in changed {
            if !self.scenes.contains_key(&file_id) {
                continue;
            }
            rediscovered += 1;
            for m in self.discover_for(file_id, &mut examined, &mut near_miss) {
                self.matches.insert((m.clip, m.alignment.source), m);
            }
        }
        self.last_examined = examined;
        self.last_near_miss = near_miss;
        self.last_rediscovered = rediscovered;
    }

    pub fn bootstrap<I>(&mut self, corpus: I)
    where
        I: IntoIterator<Item = (FileId, Tier2Fingerprint)>,
    {
        let mut all: BTreeSet<FileId> = BTreeSet::new();
        for (file_id, fp) in corpus {
            self.upsert(file_id, fp.scenes);
            all.insert(file_id);
        }
        self.rediscover(&all);
        self.bootstrapped = true;
    }

    fn remove_postings_and_scenes(&mut self, file_id: FileId) -> Option<usize> {
        let scenes = self.scenes.remove(&file_id)?;
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        for scene in &scenes {
            if seen.insert(scene.phash) {
                self.mih.remove(scene.phash, file_id);
            }
        }
        Some(scenes.len())
    }

    fn update_short_count(&mut self, old: Option<usize>, new: Option<usize>) {
        let min = self.params.min_scenes();
        if matches!(old, Some(n) if n < min) {
            self.short_count -= 1;
        }
        if matches!(new, Some(n) if n < min) {
            self.short_count += 1;
        }
    }

    fn drop_matches_touching(&mut self, file_id: FileId) {
        self.matches
            .retain(|&(clip, source), _| clip != file_id && source != file_id);
    }

    fn discover_for(
        &self,
        file_id: FileId,
        examined: &mut usize,
        near_miss: &mut NearMissCounts,
    ) -> Vec<ClipMatch> {
        let Some(f_scenes) = self.scenes.get(&file_id) else {
            return Vec::new();
        };
        let f_len = f_scenes.len();

        let mut votes: BTreeMap<(FileId, i64), usize> = BTreeMap::new();
        for scene in f_scenes {
            if scene.phash == 0 {
                continue;
            }
            let f_ts = ms_to_i64(scene.timestamp_ms);
            for posting in self.mih.candidates(scene.phash) {
                if posting.file_id == file_id {
                    continue;
                }
                let Some(other_scenes) = self.scenes.get(&posting.file_id) else {
                    continue;
                };
                let Some(other_scene) = other_scenes.get(posting.scene_index) else {
                    continue;
                };
                let other_ts = ms_to_i64(other_scene.timestamp_ms);
                let offset_ms = match other_scenes.len().cmp(&f_len) {
                    Ordering::Greater => other_ts - f_ts,
                    Ordering::Less => f_ts - other_ts,
                    Ordering::Equal => continue,
                };
                for bucket in spanned_buckets(offset_ms) {
                    *votes.entry((posting.file_id, bucket)).or_default() += 1;
                }
            }
        }

        let mut best: BTreeMap<FileId, ClipMatch> = BTreeMap::new();
        for (&(other, bucket), &count) in &votes {
            if count < VERIFY_MIN_VOTES {
                near_miss.single_vote += 1;
                continue;
            }
            let Some(other_scenes) = self.scenes.get(&other) else {
                continue;
            };
            let (clip_id, clip_scenes, source_id, source_scenes) = if other_scenes.len() > f_len {
                (file_id, f_scenes, other, other_scenes)
            } else {
                (other, other_scenes, file_id, f_scenes)
            };
            if clip_scenes.len() < self.params.min_scenes() {
                continue;
            }
            *examined += 1;
            match verify_alignment(
                clip_scenes,
                source_id,
                source_scenes,
                bucket * VOTE_BUCKET_MS,
                self.params,
            ) {
                Some(alignment) => {
                    let candidate = ClipMatch {
                        clip: clip_id,
                        alignment,
                    };
                    let keep = match best.get(&other) {
                        Some(prev) => is_better(&candidate.alignment, &prev.alignment),
                        None => true,
                    };
                    if keep {
                        best.insert(other, candidate);
                    }
                }
                None => near_miss.below_coverage += 1,
            }
        }
        best.into_values().collect()
    }

    pub fn bootstrap_from_db(
        &mut self,
        db: &mut Database,
        changed: &BTreeSet<FileId>,
    ) -> Result<()> {
        let reconciled = self.provenance_reconciled(db)?;
        if reconciled {
            let prior = SimilarityEdgesRepo::new(db.conn()).list_by_trust(TrustLevel::Possible)?;
            let pmih = PartialMihRepo::new(db.conn());
            let mut scene_count: BTreeMap<FileId, usize> = BTreeMap::new();
            for edge in &prior {
                for id in [edge.file_a, edge.file_b] {
                    if let std::collections::btree_map::Entry::Vacant(slot) = scene_count.entry(id)
                    {
                        if let Some(count) = pmih.scene_count(id)? {
                            slot.insert(count);
                        }
                    }
                }
            }
            for edge in &prior {
                let m = super::reconstruct_prev_match(edge, &scene_count);
                if scene_count.contains_key(&m.clip)
                    && scene_count.contains_key(&m.alignment.source)
                {
                    self.matches.insert((m.clip, m.alignment.source), m);
                }
            }
            self.apply_delta_db(db, changed)?;
        } else {
            if SystemMetadataRepo::new(db.conn()).contains(RECONCILED_KEY)? {
                tracing::info!(
                    blob_source = ?self.blob_source,
                    "durable partial index provenance changed; cold-rebuilding",
                );
            }
            self.cold_build_paged(db, COLD_BUILD_PAGE)?;
        }
        self.bootstrapped = true;
        Ok(())
    }

    fn cold_build_paged(&mut self, db: &mut Database, page: usize) -> Result<()> {
        let page = page.max(1);
        let keygen = MultiIndexHash::new(DEFAULT_MIH_CHUNKS, self.params.max_distance());
        let fp_repo = FingerprintsRepo::new(db.conn());
        let ids = match self.blob_source {
            BlobSource::Tier2 => fp_repo.list_active_tier2_ids()?,
            BlobSource::Partial => fp_repo.list_active_partial_ids()?,
        };
        let id_count = ids.len();

        let stored = SystemMetadataRepo::new(db.conn()).get(COLD_CHECKPOINT_KEY)?;
        let resume_after_page = match stored.as_deref().and_then(parse_cold_checkpoint) {
            Some((last_page, ck_count)) if ck_count == id_count => Some(last_page),
            _ => None,
        };

        if resume_after_page.is_none() {
            db.transaction(|conn| {
                let pmih = PartialMihRepo::new(conn);
                pmih.clear_postings()?;
                pmih.clear_scene_counts()
            })?;
        }
        for (page_idx, chunk) in ids.chunks(page).enumerate() {
            if matches!(resume_after_page, Some(done) if page_idx <= done) {
                continue;
            }
            let blob_source = self.blob_source;
            db.transaction(|conn| {
                let fp = FingerprintsRepo::new(conn);
                let pmih = PartialMihRepo::new(conn);
                for &file_id in chunk {
                    if let Some(blob) = fetch_active_blob(&fp, blob_source, file_id)? {
                        let scenes = format::decode_tier2(&blob)?.scenes;
                        persist_file_postings(&pmih, &keygen, file_id, &scenes)?;
                    }
                }
                let ck = format_cold_checkpoint(page_idx, id_count);
                SystemMetadataRepo::new(conn).set(COLD_CHECKPOINT_KEY, &ck)
            })?;
        }

        let mut examined = 0usize;
        let mut near_miss = NearMissCounts::default();
        let mut rediscovered = 0usize;
        for chunk in ids.chunks(page) {
            let page_set: BTreeSet<FileId> = chunk.iter().copied().collect();
            let (page_examined, page_rediscovered, page_near_miss) =
                self.discover_delta_db(db, &page_set, false)?;
            examined += page_examined;
            near_miss.below_coverage += page_near_miss.below_coverage;
            near_miss.single_vote += page_near_miss.single_vote;
            rediscovered += page_rediscovered;
        }
        self.last_examined = examined;
        self.last_near_miss = near_miss;
        self.last_rediscovered = rediscovered;
        Ok(())
    }

    pub fn apply_change_from_db(
        &mut self,
        db: &Database,
        changed: &BTreeSet<FileId>,
    ) -> Result<()> {
        self.apply_delta_db(db, changed)
    }

    fn apply_delta_db(&mut self, db: &Database, changed: &BTreeSet<FileId>) -> Result<()> {
        let (examined, rediscovered, near_miss) = self.discover_delta_db(db, changed, true)?;
        self.last_examined = examined;
        self.last_near_miss = near_miss;
        self.last_rediscovered = rediscovered;
        Ok(())
    }

    fn discover_delta_db(
        &mut self,
        db: &Database,
        changed: &BTreeSet<FileId>,
        stage: bool,
    ) -> Result<(usize, usize, NearMissCounts)> {
        let fp = FingerprintsRepo::new(db.conn());
        let pmih = PartialMihRepo::new(db.conn());
        let keygen = MultiIndexHash::new(DEFAULT_MIH_CHUNKS, self.params.max_distance());

        let mut present: BTreeMap<FileId, Vec<SceneHash>> = BTreeMap::new();
        let mut removed: BTreeSet<FileId> = BTreeSet::new();
        for &file_id in changed {
            match fetch_active_blob(&fp, self.blob_source, file_id)? {
                Some(blob) => {
                    present.insert(file_id, format::decode_tier2(&blob)?.scenes);
                }
                None => {
                    removed.insert(file_id);
                }
            }
        }

        let mut subgraph: BTreeMap<FileId, Vec<SceneHash>> = present.clone();
        let mut candidates: BTreeSet<FileId> = BTreeSet::new();
        for scenes in present.values() {
            candidates.extend(db_candidate_files(&pmih, &keygen, scenes)?);
        }
        for cand in candidates {
            if subgraph.contains_key(&cand) {
                continue;
            }
            if let Some(blob) = fetch_active_blob(&fp, self.blob_source, cand)? {
                subgraph.insert(cand, format::decode_tier2(&blob)?.scenes);
            }
        }

        let mut eph = PartialClipIndex::new_with_source(self.params, self.blob_source);
        for (id, scenes) in &subgraph {
            eph.upsert(*id, scenes.clone());
        }
        let present_changed: BTreeSet<FileId> = present.keys().copied().collect();
        eph.rediscover(&present_changed);

        for &file_id in changed {
            self.drop_matches_touching(file_id);
        }
        for m in eph.matches() {
            self.matches.insert((m.clip, m.alignment.source), m);
        }

        if stage {
            for file_id in removed {
                self.pending_remove.insert(file_id);
                self.pending_persist.remove(&file_id);
            }
            for (file_id, scenes) in present {
                self.pending_remove.remove(&file_id);
                self.pending_persist.insert(file_id, scenes);
            }
        }
        Ok((eph.last_examined, present_changed.len(), eph.last_near_miss))
    }

    pub fn write_to_db(
        &mut self,
        db: &mut Database,
        now_unix_s: i64,
    ) -> Result<PartialRebuildOutcome> {
        let min_scenes = self.params.min_scenes();
        let full_reset = self.full_reset;
        let params_fp = self.params_fingerprint();
        let keygen = MultiIndexHash::new(DEFAULT_MIH_CHUNKS, self.params.max_distance());
        let matches: Vec<ClipMatch> = self.matches.values().copied().collect();
        let last_examined = self.last_examined;
        let last_near_miss = self.last_near_miss;
        let pending_persist = std::mem::take(&mut self.pending_persist);
        let pending_remove = std::mem::take(&mut self.pending_remove);
        let cold_scenes = if full_reset {
            std::mem::take(&mut self.scenes)
        } else {
            BTreeMap::new()
        };

        let outcome = db.transaction(|conn| {
            let pmih = PartialMihRepo::new(conn);
            if full_reset {
                pmih.clear_postings()?;
                pmih.clear_scene_counts()?;
                for (file_id, scenes) in &cold_scenes {
                    persist_file_postings(&pmih, &keygen, *file_id, scenes)?;
                }
            } else {
                for &file_id in &pending_remove {
                    pmih.delete_file_postings(file_id)?;
                    pmih.delete_scene_count(file_id)?;
                }
                for (file_id, scenes) in &pending_persist {
                    pmih.delete_file_postings(*file_id)?;
                    persist_file_postings(&pmih, &keygen, *file_id, scenes)?;
                }
            }
            let skipped_short = pmih.count_short(min_scenes)?;

            let groups = DuplicateGroupsRepo::new(conn);
            let edges = SimilarityEdgesRepo::new(conn);
            let files = FilesRepo::new(conn);
            let cleared = groups.delete_by_trust(TrustLevel::Possible)?;
            let plan = PartialClipPlan {
                matches: matches.clone(),
                candidate_offsets_examined: last_examined,
                skipped_short,
                dropped_below_coverage: last_near_miss.below_coverage,
                dropped_single_vote: last_near_miss.single_vote,
            };
            let durations = load_active_durations(&files)?;
            let outcome =
                write_partial_plan(&groups, &edges, &plan, now_unix_s, cleared, &durations)?;
            let meta = SystemMetadataRepo::new(conn);
            meta.set(RECONCILED_KEY, "1")?;
            meta.set(PARTIAL_INDEX_PARAMS_FP_KEY, &params_fp)?;
            meta.delete(COLD_CHECKPOINT_KEY)?;
            Ok(outcome)
        })?;

        self.short_count = outcome.skipped_short;
        if full_reset {
            self.mih = MultiIndexHash::new(DEFAULT_MIH_CHUNKS, self.params.max_distance());
            self.full_reset = false;
        }
        Ok(outcome)
    }
}

fn fetch_active_blob(
    fp: &FingerprintsRepo<'_>,
    blob_source: BlobSource,
    file_id: FileId,
) -> Result<Option<Vec<u8>>> {
    match blob_source {
        BlobSource::Tier2 => fp.get_active_tier2(file_id),
        BlobSource::Partial => fp.get_active_partial(file_id),
    }
}

fn db_candidate_files(
    pmih: &PartialMihRepo<'_>,
    keygen: &MultiIndexHash,
    scenes: &[SceneHash],
) -> Result<BTreeSet<FileId>> {
    let mut per_chunk: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();
    for scene in scenes {
        for (chunk, keys) in keygen.query_keys(scene.phash) {
            per_chunk.entry(chunk).or_default().extend(keys);
        }
    }
    let mut out = BTreeSet::new();
    for (chunk, keys) in per_chunk {
        let keys: Vec<u64> = keys.into_iter().collect();
        out.extend(pmih.candidate_files(chunk, &keys)?);
    }
    Ok(out)
}

fn persist_file_postings(
    pmih: &PartialMihRepo<'_>,
    keygen: &MultiIndexHash,
    file_id: FileId,
    scenes: &[SceneHash],
) -> Result<()> {
    let mut postings = Vec::new();
    for (scene_index, scene) in scenes.iter().enumerate() {
        for (chunk, slice_value) in keygen.post_keys(scene.phash) {
            postings.push(MihPosting {
                chunk,
                slice_value,
                file_id,
                scene_index,
            });
        }
    }
    pmih.insert_postings(&postings)?;
    pmih.set_scene_count(file_id, scenes.len())?;
    Ok(())
}

pub fn rebuild_partial_clip_groups_durable(
    index: &mut PartialClipIndex,
    db: &mut Database,
    now_unix_s: i64,
    changed: &BTreeSet<FileId>,
) -> Result<PartialRebuildOutcome> {
    if index.bootstrapped {
        index.apply_change_from_db(db, changed)?;
    } else {
        index.bootstrap_from_db(db, changed)?;
    }
    index.write_to_db(db, now_unix_s)
}

pub fn rebuild_partial_clip_groups_from_fingerprints(
    db: &mut Database,
    params: AnchorParams,
    now_unix_s: i64,
) -> Result<PartialRebuildOutcome> {
    db.transaction(|conn| {
        let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
        for (id, blob) in FingerprintsRepo::new(conn).list_active_partial()? {
            if let Ok(fp) = format::decode_tier2(&blob) {
                corpus.push((id, fp));
            }
        }
        let plan = plan_partial_clips(corpus, params);
        let groups = DuplicateGroupsRepo::new(conn);
        let edges = SimilarityEdgesRepo::new(conn);
        let files = FilesRepo::new(conn);
        let cleared = groups.delete_by_trust(TrustLevel::Possible)?;
        let durations = load_active_durations(&files)?;
        write_partial_plan(&groups, &edges, &plan, now_unix_s, cleared, &durations)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partial::{partial_clip_params, plan_partial_clips};

    #[test]
    fn cold_checkpoint_round_trips_and_rejects_malformed() {
        let s = format_cold_checkpoint(7, 12_000);
        assert_eq!(parse_cold_checkpoint(&s), Some((7, 12_000)));
        assert_eq!(
            parse_cold_checkpoint(&format_cold_checkpoint(0, 0)),
            Some((0, 0))
        );
        assert_eq!(parse_cold_checkpoint("nope"), None);
        assert_eq!(parse_cold_checkpoint("3:"), None);
        assert_eq!(parse_cold_checkpoint("3:x"), None);
    }

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

    fn source_embedding(
        seed: u64,
        n: usize,
        at: usize,
        clip: &Tier2Fingerprint,
    ) -> Tier2Fingerprint {
        let mut src = source_seq(seed, n);
        for (k, s) in clip.scenes.iter().enumerate() {
            src.scenes[at + k] = scene((at + k) as u64 * 1000, s.phash);
        }
        src
    }

    fn planted_corpus(sources: usize) -> Vec<(FileId, Tier2Fingerprint)> {
        let mut corpus = Vec::new();
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

    fn assert_matches_full(index: &PartialClipIndex, corpus: &[(FileId, Tier2Fingerprint)]) {
        let full = plan_partial_clips(corpus.iter().cloned(), index.params());
        assert_eq!(
            index.matches(),
            full.matches,
            "durable matches must equal the full plan",
        );
        assert_eq!(
            index.plan().skipped_short,
            full.skipped_short,
            "skipped_short must match the full plan",
        );
    }

    #[test]
    fn bootstrap_matches_full_plan() {
        let corpus = planted_corpus(15);
        let mut index = PartialClipIndex::new(AnchorParams::default());
        index.bootstrap(corpus.iter().cloned());
        assert!(!index.matches().is_empty(), "fixture plants real matches");
        assert_matches_full(&index, &corpus);
    }

    #[test]
    fn last_rediscovered_tracks_files_actually_searched() {
        let corpus = planted_corpus(5);
        let mut index = PartialClipIndex::new(AnchorParams::default());
        index.bootstrap(corpus.iter().cloned());
        assert_eq!(
            index.last_rediscovered(),
            corpus.len(),
            "bootstrap discovers every file",
        );

        let one = corpus[0].0;
        index.rediscover(&[one].into_iter().collect());
        assert_eq!(index.last_rediscovered(), 1);

        index.rediscover(&[FileId(999_999)].into_iter().collect());
        assert_eq!(index.last_rediscovered(), 0);
    }

    #[test]
    fn source_only_burst_rediscovers_via_changed_source_query() {
        let params = AnchorParams::default();
        let clip = source_seq(0xC11D, 6);
        let mut index = PartialClipIndex::new(params);
        index.bootstrap([(FileId(1), clip.clone())]);
        assert!(index.matches().is_empty(), "lone clip matches nothing");

        let source = source_embedding(0x5005, 40, 20, &clip);
        index.upsert(FileId(2), source.scenes.clone());
        index.rediscover(&[FileId(2)].into_iter().collect());

        let corpus = vec![(FileId(1), clip), (FileId(2), source)];
        assert_matches_full(&index, &corpus);
        assert_eq!(index.matches().len(), 1);
        assert_eq!(index.matches()[0].clip, FileId(1));
        assert_eq!(index.matches()[0].alignment.source, FileId(2));
    }

    #[test]
    fn clip_only_burst_rediscovers_via_changed_clip_query() {
        let params = AnchorParams::default();
        let source = source_seq(0x00A1, 40);
        let mut index = PartialClipIndex::new(params);
        index.bootstrap([(FileId(1), source.clone())]);
        assert!(index.matches().is_empty());

        let clip = clip_of(&source, 12, 6, 3);
        index.upsert(FileId(2), clip.scenes.clone());
        index.rediscover(&[FileId(2)].into_iter().collect());

        let corpus = vec![(FileId(1), source), (FileId(2), clip)];
        assert_matches_full(&index, &corpus);
        assert_eq!(index.matches().len(), 1);
        assert_eq!(index.matches()[0].clip, FileId(2));
        assert_eq!(index.matches()[0].alignment.source, FileId(1));
    }

    #[test]
    fn removing_a_source_drops_its_carried_match() {
        let params = AnchorParams::default();
        let source = source_seq(0x2222, 40);
        let clip = clip_of(&source, 8, 6, 2);
        let mut index = PartialClipIndex::new(params);
        index.bootstrap([(FileId(1), source), (FileId(2), clip.clone())]);
        assert_eq!(index.matches().len(), 1);

        index.remove(FileId(1));
        index.rediscover(&BTreeSet::new());
        assert!(
            index.matches().is_empty(),
            "removing the source leaves the clip with nothing to match",
        );
        let corpus = vec![(FileId(2), clip)];
        assert_matches_full(&index, &corpus);
    }

    #[test]
    fn mutating_a_source_out_from_under_a_clip_drops_the_match() {
        let params = AnchorParams::default();
        let source = source_seq(0x3333, 40);
        let clip = clip_of(&source, 8, 6, 2);
        let mut index = PartialClipIndex::new(params);
        index.bootstrap([(FileId(1), source), (FileId(2), clip.clone())]);
        assert_eq!(index.matches().len(), 1);

        let unrelated = source_seq(0x9999, 40);
        index.upsert(FileId(1), unrelated.scenes.clone());
        index.rediscover(&[FileId(1)].into_iter().collect());
        assert!(index.matches().is_empty());
        let corpus = vec![(FileId(1), unrelated), (FileId(2), clip)];
        assert_matches_full(&index, &corpus);
    }

    #[test]
    fn both_endpoints_changed_is_deduped_not_doubled() {
        let params = AnchorParams::default();
        let source = source_seq(0x7777, 40);
        let clip = clip_of(&source, 15, 6, 2);
        let mut index = PartialClipIndex::new(params);
        index.upsert(FileId(1), source.scenes.clone());
        index.upsert(FileId(2), clip.scenes.clone());
        index.rediscover(&[FileId(1), FileId(2)].into_iter().collect());
        assert_eq!(index.matches().len(), 1, "the pair is not double-counted");
        let corpus = vec![(FileId(1), source), (FileId(2), clip)];
        assert_matches_full(&index, &corpus);
    }

    #[test]
    fn many_burst_sequence_stays_equal_to_full_plan() {
        let params = AnchorParams::default();
        let mut index = PartialClipIndex::new(params);
        let mut corpus: BTreeMap<FileId, Tier2Fingerprint> = BTreeMap::new();

        let initial = planted_corpus(12);
        for (id, fp) in &initial {
            corpus.insert(*id, fp.clone());
        }
        index.bootstrap(initial.iter().cloned());
        assert_matches_full(
            &index,
            &corpus
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        );

        let target_clip = corpus[&FileId(10_000)].clone();
        let embed = source_embedding(0xD00D, 50, 18, &target_clip);
        corpus.insert(FileId(50_001), embed.clone());
        index.upsert(FileId(50_001), embed.scenes.clone());
        index.rediscover(&[FileId(50_001)].into_iter().collect());
        assert_matches_full(
            &index,
            &corpus
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        );

        let noise = source_seq(0xBADD, 40);
        corpus.insert(FileId(2), noise.clone());
        index.upsert(FileId(2), noise.scenes.clone());
        index.rediscover(&[FileId(2)].into_iter().collect());
        assert_matches_full(
            &index,
            &corpus
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        );

        corpus.remove(&FileId(10_002));
        index.remove(FileId(10_002));
        index.rediscover(&BTreeSet::new());
        assert_matches_full(
            &index,
            &corpus
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        );

        let s3 = corpus[&FileId(4)].clone();
        let new_clip = clip_of(&s3, 5, 7, 3);
        corpus.insert(FileId(60_001), new_clip.clone());
        index.upsert(FileId(60_001), new_clip.scenes.clone());
        index.rediscover(&[FileId(60_001)].into_iter().collect());
        assert_matches_full(
            &index,
            &corpus
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        );
    }

    fn seed_db(
        corpus: &[(FileId, Tier2Fingerprint)],
    ) -> (Database, Vec<(FileId, Tier2Fingerprint)>) {
        use vidcull_core::types::{Codec, NormalizedPath};
        use vidcull_db::open_in_memory;
        use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile};
        use vidcull_fingerprint::format::{self, FORMAT_VERSION};
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

        let db = open_in_memory().expect("open db");
        let mut remapped = Vec::with_capacity(corpus.len());
        for (i, (_, fp)) in corpus.iter().enumerate() {
            let path = format!("/v/{i:06}.mp4");
            let new_file = NewFile {
                path: NormalizedPath::new(&path),
                size_bytes: 1024,
                mtime_ns: 1,
                first_seen_at: 0,
                last_seen_at: 0,
                ..Default::default()
            };
            let id = FilesRepo::new(db.conn()).insert(&new_file).expect("insert");
            let t1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash: fp.scenes.first().map_or(0, |s| s.phash),
            };
            FingerprintsRepo::new(db.conn())
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: format::encode_tier1(&t1).expect("encode t1"),
                    tier2_temporal: Some(format::encode_tier2(fp).expect("encode t2")),
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: 0,
                })
                .expect("upsert fp");
            remapped.push((id, fp.clone()));
        }
        (db, remapped)
    }

    #[test]
    fn cold_paged_build_matches_full_plan_across_pages() {
        let params = AnchorParams::default();
        let (mut db, corpus) = seed_db(&planted_corpus(8));

        let mut index = PartialClipIndex::new(params);
        index
            .cold_build_paged(&mut db, 1)
            .expect("paged cold build");

        let full = plan_partial_clips(corpus.iter().cloned(), params);
        assert!(!full.matches.is_empty(), "fixture plants real matches");
        assert_eq!(
            index.matches(),
            full.matches,
            "paged cold build must equal the full plan",
        );

        assert!(
            index.is_empty(),
            "paged build retains no resident corpus scene map",
        );
        assert_eq!(
            index.last_rediscovered(),
            corpus.len(),
            "a genuine first plan searches every file",
        );
    }

    #[test]
    fn cold_paged_build_persists_postings_and_writes_groups() {
        use vidcull_db::repo::{DuplicateGroupsRepo, PartialMihRepo, TrustLevel};

        let params = AnchorParams::default();
        let (mut db, corpus) = seed_db(&planted_corpus(5));

        let mut index = PartialClipIndex::new(params);
        index
            .cold_build_paged(&mut db, 3)
            .expect("paged cold build");
        let outcome = index.write_to_db(&mut db, 0).expect("write");

        let counts = PartialMihRepo::new(db.conn())
            .load_all_scene_counts()
            .expect("counts");
        assert_eq!(counts.len(), corpus.len());

        let full = plan_partial_clips(corpus.iter().cloned(), params);
        let groups = DuplicateGroupsRepo::new(db.conn());
        let possible = (1..=512)
            .filter_map(|gid| groups.get(gid).expect("get"))
            .filter(|g| g.trust_level == TrustLevel::Possible)
            .count();
        assert_eq!(possible, full.matches.len(), "one group per planted match");
        assert_eq!(outcome.groups_created, full.matches.len());
    }

    #[test]
    fn cold_build_resume_after_pass1_equals_uninterrupted() {
        use vidcull_db::repo::SystemMetadataRepo;
        let params = AnchorParams::default();
        let (mut db, corpus) = seed_db(&planted_corpus(8));
        let full = plan_partial_clips(corpus.iter().cloned(), params);
        assert!(!full.matches.is_empty(), "fixture plants real matches");

        let mut crashed = PartialClipIndex::new(params);
        crashed
            .cold_build_paged(&mut db, 1)
            .expect("interrupted build");
        assert_eq!(crashed.matches(), full.matches);
        {
            let meta = SystemMetadataRepo::new(db.conn());
            let has_ck = meta.contains(COLD_CHECKPOINT_KEY).expect("ck");
            let reconciled = meta.contains(RECONCILED_KEY).expect("mk");
            assert!(has_ck, "checkpoint left for resume");
            assert!(!reconciled, "marker not set pre-write");
        }

        let mut resumed = PartialClipIndex::new(params);
        resumed.cold_build_paged(&mut db, 1).expect("resume build");
        assert_eq!(
            resumed.matches(),
            full.matches,
            "resume after Pass-1 reproduces the uninterrupted match set",
        );
        assert!(resumed.is_empty(), "resume retains no resident corpus");
    }

    #[test]
    fn cold_build_resume_mid_pass1_equals_uninterrupted() {
        use vidcull_db::repo::{FingerprintsRepo, SystemMetadataRepo};
        let params = AnchorParams::default();
        let (mut db, corpus) = seed_db(&planted_corpus(8));
        let full = plan_partial_clips(corpus.iter().cloned(), params);

        let mut first = PartialClipIndex::new(params);
        first.cold_build_paged(&mut db, 1).expect("first build");
        let id_count = FingerprintsRepo::new(db.conn())
            .list_active_tier2_ids()
            .expect("ids")
            .len();
        SystemMetadataRepo::new(db.conn())
            .set(COLD_CHECKPOINT_KEY, &format_cold_checkpoint(0, id_count))
            .expect("rewind checkpoint");

        let mut resumed = PartialClipIndex::new(params);
        resumed.cold_build_paged(&mut db, 1).expect("resume build");
        assert_eq!(
            resumed.matches(),
            full.matches,
            "resume from a mid Pass-1 checkpoint reproduces the uninterrupted match set",
        );
    }

    #[test]
    fn cold_build_snapshot_idcount_mismatch_restarts_clean() {
        use vidcull_db::repo::SystemMetadataRepo;
        let params = AnchorParams::default();
        let (mut db, corpus) = seed_db(&planted_corpus(5));
        let full = plan_partial_clips(corpus.iter().cloned(), params);

        SystemMetadataRepo::new(db.conn())
            .set(COLD_CHECKPOINT_KEY, &format_cold_checkpoint(3, 99_999))
            .expect("stale checkpoint");

        let mut index = PartialClipIndex::new(params);
        index.cold_build_paged(&mut db, 2).expect("clean rebuild");
        assert_eq!(
            index.matches(),
            full.matches,
            "id-count mismatch restarts clean and equals the full plan",
        );
    }

    #[test]
    fn cold_build_marker_set_and_checkpoint_cleared_atomically() {
        use vidcull_db::repo::SystemMetadataRepo;
        let params = AnchorParams::default();
        let (mut db, _corpus) = seed_db(&planted_corpus(5));

        let mut index = PartialClipIndex::new(params);
        index.cold_build_paged(&mut db, 3).expect("cold build");
        {
            let meta = SystemMetadataRepo::new(db.conn());
            let has_ck = meta.contains(COLD_CHECKPOINT_KEY).expect("ck");
            let reconciled = meta.contains(RECONCILED_KEY).expect("mk");
            assert!(has_ck, "checkpoint present pre-write");
            assert!(!reconciled, "marker absent pre-write");
        }
        index.write_to_db(&mut db, 0).expect("write");
        let meta = SystemMetadataRepo::new(db.conn());
        let reconciled = meta.contains(RECONCILED_KEY).expect("mk2");
        let has_ck = meta.contains(COLD_CHECKPOINT_KEY).expect("ck2");
        assert!(reconciled, "marker set after write");
        assert!(!has_ck, "checkpoint cleared atomically with the marker");
    }

    fn seed_db_partial(
        corpus: &[(FileId, Tier2Fingerprint)],
    ) -> (Database, Vec<(FileId, Tier2Fingerprint)>) {
        use vidcull_core::types::{Codec, NormalizedPath};
        use vidcull_db::open_in_memory;
        use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile};
        use vidcull_fingerprint::format::{self, FORMAT_VERSION};
        use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

        let db = open_in_memory().expect("open db");
        let mut remapped = Vec::with_capacity(corpus.len());
        for (i, (_, fp)) in corpus.iter().enumerate() {
            let path = format!("/v/{i:06}.mp4");
            let new_file = NewFile {
                path: NormalizedPath::new(&path),
                size_bytes: 1024,
                mtime_ns: 1,
                first_seen_at: 0,
                last_seen_at: 0,
                ..Default::default()
            };
            let id = FilesRepo::new(db.conn()).insert(&new_file).expect("insert");
            let dummy_tier2 = Tier2Fingerprint {
                scenes: vec![scene(0, 0xDEAD_BEEF | u64::try_from(i).unwrap())],
            };
            let t1 = Tier1Fingerprint {
                duration_ms: 60_000,
                codec: Codec::H264,
                gop: GopSignature::from_durations(&[]),
                global_phash: fp.scenes.first().map_or(0, |s| s.phash),
            };
            let fp_repo = FingerprintsRepo::new(db.conn());
            fp_repo
                .upsert(&Fingerprint {
                    file_id: id,
                    tier1_global: format::encode_tier1(&t1).expect("encode t1"),
                    tier2_temporal: Some(format::encode_tier2(&dummy_tier2).expect("encode t2")),
                    format_version: u32::from(FORMAT_VERSION),
                    created_at: 0,
                })
                .expect("upsert fp");
            fp_repo
                .set_partial(id, &format::encode_tier2(fp).expect("encode partial"))
                .expect("set partial");
            remapped.push((id, fp.clone()));
        }
        (db, remapped)
    }

    #[test]
    fn partial_source_cold_build_reads_partial_not_tier2() {
        use vidcull_db::repo::FingerprintsRepo;
        let params = partial_clip_params();
        let (mut db, corpus) = seed_db_partial(&planted_corpus(6));

        let full_partial = plan_partial_clips(corpus.iter().cloned(), params);
        assert!(
            !full_partial.matches.is_empty(),
            "partial corpus plants real matches",
        );
        let tier2_corpus: Vec<(FileId, Tier2Fingerprint)> = corpus
            .iter()
            .map(|(id, _)| {
                let blob = FingerprintsRepo::new(db.conn())
                    .get_active_tier2(*id)
                    .expect("tier2")
                    .expect("tier2 present");
                (*id, format::decode_tier2(&blob).expect("decode tier2"))
            })
            .collect();
        assert!(
            plan_partial_clips(tier2_corpus.into_iter(), params)
                .matches
                .is_empty(),
            "contradictory tier2 corpus must group nothing",
        );

        let mut index = PartialClipIndex::new_with_source(params, BlobSource::Partial);
        assert_eq!(index.blob_source(), BlobSource::Partial);
        index
            .cold_build_paged(&mut db, 1)
            .expect("partial cold build");
        assert_eq!(
            index.matches(),
            full_partial.matches,
            "partial-source cold build must equal the full plan over partial_temporal",
        );
    }

    #[test]
    fn partial_source_delta_burst_reads_partial_not_tier2() {
        let params = partial_clip_params();
        let (mut db, corpus) = seed_db_partial(&planted_corpus(6));
        let full_partial = plan_partial_clips(corpus.iter().cloned(), params);
        assert!(!full_partial.matches.is_empty());

        let mut index = PartialClipIndex::new_with_source(params, BlobSource::Partial);
        index
            .bootstrap_from_db(&mut db, &BTreeSet::new())
            .expect("bootstrap");
        index.write_to_db(&mut db, 0).expect("write");

        let clip_id = corpus
            .iter()
            .map(|(id, _)| *id)
            .find(|id| full_partial.matches.iter().any(|m| m.clip == *id))
            .expect("a planted clip id");
        let changed: BTreeSet<FileId> = [clip_id].into_iter().collect();
        index
            .apply_change_from_db(&db, &changed)
            .expect("delta burst");
        assert_eq!(
            index.matches(),
            full_partial.matches,
            "partial-source delta burst must equal the full plan over partial_temporal",
        );
    }

    #[test]
    fn discover_for_offset_sign_correct_in_both_orientations() {
        let params = AnchorParams::default();
        let clip = source_seq(0xC11D, 6);
        let source = source_embedding(0x5005, 40, 20, &clip);
        let expected_offset = 20_000i64;

        let mut via_clip = PartialClipIndex::new(params);
        via_clip.upsert(FileId(1), clip.scenes.clone());
        via_clip.upsert(FileId(2), source.scenes.clone());
        via_clip.rediscover(&[FileId(1)].into_iter().collect());
        let m_clip = via_clip.matches();

        let mut via_source = PartialClipIndex::new(params);
        via_source.upsert(FileId(1), clip.scenes.clone());
        via_source.upsert(FileId(2), source.scenes.clone());
        via_source.rediscover(&[FileId(2)].into_iter().collect());
        let m_source = via_source.matches();

        assert_eq!(
            m_clip, m_source,
            "both orientations must yield the identical ClipMatch set (offset sign)",
        );
        assert_eq!(m_clip.len(), 1, "exactly the embedded clip⊂source pair");
        assert_eq!(m_clip[0].clip, FileId(1));
        assert_eq!(m_clip[0].alignment.source, FileId(2));
        assert_eq!(
            m_clip[0].alignment.source_offset, expected_offset,
            "source-frame offset is +20_000 ms in both orientations",
        );
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    fn jittered_corpus(pairs: usize) -> Vec<(FileId, Tier2Fingerprint)> {
        let mut corpus = Vec::new();
        for s in 0..pairs {
            let su = u64::try_from(s).unwrap();
            let mut content = 0x71D5_0000 + su;
            let mut src_ts_st = 0x5151_0000 + su;
            let mut ts: u64 = 0;
            let mut src_scenes = Vec::new();
            let mut src_hashes = Vec::new();
            for _ in 0..30 {
                let h = splitmix64(&mut content) | 1;
                src_hashes.push(h);
                src_scenes.push(scene(ts, h));
                let jit = (splitmix64(&mut src_ts_st) % 401) as i64 - 200;
                ts = u64::try_from((ts as i64) + 2500 + jit).unwrap();
            }
            let mut clip_ts_st = 0x9999_0000 + su;
            let mut clip_ts: u64 = 0;
            let mut clip_scenes = Vec::new();
            for k in 0..8usize {
                clip_scenes.push(scene(clip_ts, flip_low_bits(src_hashes[8 + k], 3)));
                let jit = (splitmix64(&mut clip_ts_st) % 401) as i64 - 200;
                clip_ts = u64::try_from((clip_ts as i64) + 2500 + jit).unwrap();
            }
            let src_id = FileId(i64::try_from(s).unwrap() * 2 + 1);
            let clip_id = FileId(i64::try_from(s).unwrap() * 2 + 2);
            corpus.push((src_id, Tier2Fingerprint { scenes: src_scenes }));
            corpus.push((
                clip_id,
                Tier2Fingerprint {
                    scenes: clip_scenes,
                },
            ));
        }
        corpus
    }

    #[test]
    fn durable_equals_in_memory_on_jittered_native_idr_corpus() {
        let params = AnchorParams::default();
        let (mut db, corpus) = seed_db(&jittered_corpus(4));

        let in_memory = plan_partial_clips(corpus.iter().cloned(), params);
        assert!(
            !in_memory.matches.is_empty(),
            "jittered fixture must plant real matches (else the lock is vacuous)",
        );

        let mut index = PartialClipIndex::new(params);
        index
            .bootstrap_from_db(&mut db, &BTreeSet::new())
            .expect("cold build over jittered corpus");
        assert_eq!(
            index.matches(),
            in_memory.matches,
            "durable (discover_delta_db) must equal in-memory plan on a jittered \
             native-IDR corpus",
        );
    }
}
