use std::collections::{BTreeMap, BTreeSet, HashMap};

use rayon::prelude::*;
use vidcull_core::types::FileId;
use vidcull_core::{Error, Result};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FingerprintsRepo, SimilarityEdge, SimilarityEdgesRepo, TrustLevel,
};
use vidcull_fingerprint::format;
use vidcull_fingerprint::{hamming_distance, hamming_distance_batch};

const PHASH_BITS: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LshParams {
    bands: usize,
    max_distance: u32,
}

impl LshParams {
    pub const DEFAULT_BANDS: usize = 8;
    pub const DEFAULT_MAX_DISTANCE: u32 = 6;

    pub fn new(bands: usize, max_distance: u32) -> Result<Self> {
        let bands_u32 = u32::try_from(bands)
            .map_err(|_| Error::Unsupported(format!("LSH band count {bands} is out of range")))?;
        if bands_u32 == 0 || bands_u32 > PHASH_BITS || PHASH_BITS % bands_u32 != 0 {
            return Err(Error::Unsupported(format!(
                "LSH band count {bands} must be a divisor of {PHASH_BITS} (1, 2, 4, 8, 16, 32, 64)"
            )));
        }
        if max_distance > PHASH_BITS {
            return Err(Error::Unsupported(format!(
                "LSH max_distance {max_distance} cannot exceed {PHASH_BITS} bits"
            )));
        }
        Ok(Self {
            bands,
            max_distance,
        })
    }

    #[must_use]
    pub fn bands(&self) -> usize {
        self.bands
    }

    #[must_use]
    pub fn max_distance(&self) -> u32 {
        self.max_distance
    }

    #[must_use]
    pub fn band_bits(&self) -> u32 {
        PHASH_BITS / self.bands_u32()
    }

    #[must_use]
    pub fn guarantees_recall(&self) -> bool {
        self.bands_u32() > self.max_distance
    }

    fn bands_u32(&self) -> u32 {
        u32::try_from(self.bands).unwrap_or(PHASH_BITS)
    }

    fn for_each_band(&self, phash: u64, mut f: impl FnMut(u8, u64)) {
        let bits = self.band_bits();
        let mask = if bits >= PHASH_BITS {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        for band in 0..self.bands {
            let shift = bits * u32::try_from(band).unwrap_or(0);
            let value = (phash >> shift) & mask;
            f(u8::try_from(band).unwrap_or(u8::MAX), value);
        }
    }
}

impl Default for LshParams {
    fn default() -> Self {
        Self {
            bands: Self::DEFAULT_BANDS,
            max_distance: Self::DEFAULT_MAX_DISTANCE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NearMatch {
    pub file_id: FileId,
    pub distance: u32,
}

#[derive(Debug, Clone)]
pub struct LshIndex {
    params: LshParams,
    buckets: HashMap<(u8, u64), Vec<FileId>>,
    phash_of: HashMap<FileId, u64>,
    skipped_uninformative: usize,
}

impl LshIndex {
    #[must_use]
    pub fn build<I>(items: I, params: LshParams) -> Self
    where
        I: IntoIterator<Item = (FileId, u64)>,
    {
        let mut buckets: HashMap<(u8, u64), Vec<FileId>> = HashMap::new();
        let mut phash_of: HashMap<FileId, u64> = HashMap::new();
        let mut skipped_uninformative = 0usize;
        for (file_id, phash) in items {
            if phash == 0 {
                skipped_uninformative += 1;
                continue;
            }
            if phash_of.insert(file_id, phash).is_some() {
                continue;
            }
            params.for_each_band(phash, |band, value| {
                buckets.entry((band, value)).or_default().push(file_id);
            });
        }
        Self {
            params,
            buckets,
            phash_of,
            skipped_uninformative,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.phash_of.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.phash_of.is_empty()
    }

    #[must_use]
    pub fn skipped_uninformative(&self) -> usize {
        self.skipped_uninformative
    }

    #[must_use]
    pub fn params(&self) -> LshParams {
        self.params
    }

    #[must_use]
    pub fn candidates(&self, phash: u64) -> Vec<FileId> {
        // A flat Vec + sort_unstable + dedup instead of a BTreeSet: same
        // sorted-unique output (BTreeSet iteration is sorted with no
        // duplicates, exactly what sort_unstable+dedup produces), but
        // without a tree-node allocation per candidate — one contiguous,
        // cache-friendly buffer instead.
        let mut candidates = Vec::with_capacity(32);
        self.params.for_each_band(phash, |band, value| {
            if let Some(ids) = self.buckets.get(&(band, value)) {
                candidates.extend(ids.iter().copied());
            }
        });
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }

    #[must_use]
    pub fn query(&self, phash: u64) -> Vec<NearMatch> {
        let mut matches: Vec<NearMatch> = self
            .candidates(phash)
            .into_iter()
            .filter_map(|file_id| {
                let other = *self.phash_of.get(&file_id)?;
                let distance = hamming_distance(phash, other);
                (distance <= self.params.max_distance).then_some(NearMatch { file_id, distance })
            })
            .collect();
        matches.sort_unstable_by(|a, b| {
            a.distance
                .cmp(&b.distance)
                .then_with(|| a.file_id.cmp(&b.file_id))
        });
        matches
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NearEdge {
    pub a: FileId,
    pub b: FileId,
    pub distance: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearGroup {
    pub members: Vec<FileId>,
    pub edges: Vec<NearEdge>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NearDuplicatePlan {
    pub groups: Vec<NearGroup>,
    pub candidate_pairs_examined: usize,
    pub skipped_uninformative: usize,
}

#[must_use]
pub fn plan_near_duplicates<I>(items: I, params: LshParams) -> NearDuplicatePlan
where
    I: IntoIterator<Item = (FileId, u64)>,
{
    let index = LshIndex::build(items, params);

    let mut ids: Vec<FileId> = index.phash_of.keys().copied().collect();
    ids.sort_unstable();
    let position: HashMap<FileId, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    // Per-id candidate search only reads the shared, already-built `index`,
    // so it's embarrassingly parallel; the union-find itself is inherently
    // sequential (each union depends on prior ones), so it's kept as a
    // serial merge pass over the parallel-computed edges. Collecting through
    // `.par_iter().collect()` before the merge preserves the exact same
    // per-id, per-candidate order as the old sequential loop, so `edges`
    // (and every dsu.union call) happens in identical order regardless of
    // which worker thread finishes first — required for this crate's
    // determinism guarantees.
    let per_id: Vec<(usize, Vec<NearEdge>)> = ids
        .par_iter()
        .map(|&id| {
            let phash = index.phash_of[&id];
            let candidates: Vec<FileId> = index
                .candidates(phash)
                .into_iter()
                .filter(|&cand| cand > id)
                .collect();
            if candidates.is_empty() {
                return (0, Vec::new());
            }
            let hashes: Vec<u64> = candidates.iter().map(|c| index.phash_of[c]).collect();
            let distances = hamming_distance_batch(phash, &hashes);
            let examined = candidates.len();
            let local_edges: Vec<NearEdge> = candidates
                .into_iter()
                .zip(distances)
                .filter_map(|(cand, distance)| {
                    (distance <= params.max_distance)
                        .then_some(NearEdge { a: id, b: cand, distance })
                })
                .collect();
            (examined, local_edges)
        })
        .collect();

    let mut dsu = DisjointSet::new(ids.len());
    let mut edges: Vec<NearEdge> = Vec::new();
    let mut candidate_pairs_examined = 0usize;
    for (examined, local_edges) in per_id {
        candidate_pairs_examined += examined;
        for edge in local_edges {
            dsu.union(position[&edge.a], position[&edge.b]);
            edges.push(edge);
        }
    }

    let groups = assemble_groups(&ids, &position, &mut dsu, edges);
    NearDuplicatePlan {
        groups,
        candidate_pairs_examined,
        skipped_uninformative: index.skipped_uninformative,
    }
}

#[must_use]
pub fn plan_near_duplicates_incremental<I>(
    items: I,
    prev_edges: &[NearEdge],
    changed: &BTreeSet<FileId>,
    params: LshParams,
) -> NearDuplicatePlan
where
    I: IntoIterator<Item = (FileId, u64)>,
{
    let index = LshIndex::build(items, params);

    let mut ids: Vec<FileId> = index.phash_of.keys().copied().collect();
    ids.sort_unstable();
    let position: HashMap<FileId, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    let mut edges: Vec<NearEdge> = Vec::new();

    for e in prev_edges {
        let unchanged = !changed.contains(&e.a) && !changed.contains(&e.b);
        let (Some(&pa), Some(&pb)) = (index.phash_of.get(&e.a), index.phash_of.get(&e.b)) else {
            continue;
        };
        if !unchanged {
            continue;
        }
        let distance = hamming_distance(pa, pb);
        if distance <= params.max_distance {
            edges.push(NearEdge {
                a: e.a.min(e.b),
                b: e.a.max(e.b),
                distance,
            });
        }
    }

    // Same rationale as plan_near_duplicates: per-changed-id search only reads
    // the shared `index`, so it parallelizes cleanly. `changed` is a BTreeSet
    // (already-sorted iteration), so collecting to a Vec first preserves the
    // exact same per-id order the old sequential loop used before merging.
    let changed_ids: Vec<FileId> = changed.iter().copied().collect();
    let per_id: Vec<(usize, Vec<NearEdge>)> = changed_ids
        .par_iter()
        .map(|&d| {
            let Some(&phash) = index.phash_of.get(&d) else {
                return (0, Vec::new());
            };
            let candidates: Vec<FileId> = index
                .candidates(phash)
                .into_iter()
                .filter(|&cand| cand != d && !(changed.contains(&cand) && cand < d))
                .collect();
            if candidates.is_empty() {
                return (0, Vec::new());
            }
            let hashes: Vec<u64> = candidates.iter().map(|c| index.phash_of[c]).collect();
            let distances = hamming_distance_batch(phash, &hashes);
            let examined = candidates.len();
            let local_edges: Vec<NearEdge> = candidates
                .into_iter()
                .zip(distances)
                .filter_map(|(cand, distance)| {
                    (distance <= params.max_distance).then_some(NearEdge {
                        a: d.min(cand),
                        b: d.max(cand),
                        distance,
                    })
                })
                .collect();
            (examined, local_edges)
        })
        .collect();

    let mut candidate_pairs_examined = 0usize;
    for (examined, local_edges) in per_id {
        candidate_pairs_examined += examined;
        edges.extend(local_edges);
    }

    let mut dsu = DisjointSet::new(ids.len());
    for e in &edges {
        dsu.union(position[&e.a], position[&e.b]);
    }

    let groups = assemble_groups(&ids, &position, &mut dsu, edges);
    NearDuplicatePlan {
        groups,
        candidate_pairs_examined,
        skipped_uninformative: index.skipped_uninformative,
    }
}

fn assemble_groups(
    ids: &[FileId],
    position: &HashMap<FileId, usize>,
    dsu: &mut DisjointSet,
    edges: Vec<NearEdge>,
) -> Vec<NearGroup> {
    let mut members_by_root: BTreeMap<usize, Vec<FileId>> = BTreeMap::new();
    for &id in ids {
        let root = dsu.find(position[&id]);
        members_by_root.entry(root).or_default().push(id);
    }
    let mut edges_by_root: BTreeMap<usize, Vec<NearEdge>> = BTreeMap::new();
    for edge in edges {
        let root = dsu.find(position[&edge.a]);
        edges_by_root.entry(root).or_default().push(edge);
    }

    let mut groups: Vec<NearGroup> = members_by_root
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|(root, mut members)| {
            members.sort_unstable();
            let mut group_edges = edges_by_root.remove(&root).unwrap_or_default();
            group_edges.sort_unstable_by(|x, y| x.a.cmp(&y.a).then_with(|| x.b.cmp(&y.b)));
            NearGroup {
                members,
                edges: group_edges,
            }
        })
        .collect();
    groups.sort_by_key(|g| g.members[0]);
    groups
}

struct DisjointSet {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl DisjointSet {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NearRebuildOutcome {
    pub groups_cleared: usize,
    pub groups_created: usize,
    pub members_added: usize,
    pub edges_added: usize,
    pub skipped_uninformative: usize,
}

pub fn rebuild_near_duplicate_groups(
    db: &mut Database,
    params: LshParams,
    now_unix_s: i64,
) -> Result<NearRebuildOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let fingerprints = FingerprintsRepo::new(conn);
        let edges_repo = SimilarityEdgesRepo::new(conn);

        let groups_cleared = groups.delete_by_trust(TrustLevel::VeryLikely)?;
        let items = load_active_phashes(&fingerprints)?;
        let plan = plan_near_duplicates(items, params);
        write_near_plan(&groups, &edges_repo, &plan, now_unix_s, groups_cleared)
    })
}

pub fn rebuild_near_duplicate_groups_incremental(
    db: &mut Database,
    params: LshParams,
    now_unix_s: i64,
    changed: &BTreeSet<FileId>,
) -> Result<NearRebuildOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let fingerprints = FingerprintsRepo::new(conn);
        let edges_repo = SimilarityEdgesRepo::new(conn);

        let prev_edges: Vec<NearEdge> = edges_repo
            .list_by_trust(TrustLevel::VeryLikely)?
            .into_iter()
            .map(|e| NearEdge {
                a: e.file_a,
                b: e.file_b,
                distance: 0,
            })
            .collect();

        let groups_cleared = groups.delete_by_trust(TrustLevel::VeryLikely)?;
        let items = load_active_phashes(&fingerprints)?;
        let plan = plan_near_duplicates_incremental(items, &prev_edges, changed, params);
        write_near_plan(&groups, &edges_repo, &plan, now_unix_s, groups_cleared)
    })
}

fn load_active_phashes(fingerprints: &FingerprintsRepo<'_>) -> Result<Vec<(FileId, u64)>> {
    let mut items: Vec<(FileId, u64)> = Vec::new();
    for (file_id, blob) in fingerprints.list_active_tier1()? {
        let phash = format::decode_tier1(&blob)?.global_phash;
        items.push((file_id, phash));
    }
    Ok(items)
}

fn write_near_plan(
    groups: &DuplicateGroupsRepo<'_>,
    edges_repo: &SimilarityEdgesRepo<'_>,
    plan: &NearDuplicatePlan,
    now_unix_s: i64,
    groups_cleared: usize,
) -> Result<NearRebuildOutcome> {
    let mut outcome = NearRebuildOutcome {
        groups_cleared,
        skipped_uninformative: plan.skipped_uninformative,
        ..NearRebuildOutcome::default()
    };
    for group in &plan.groups {
        let gid = groups.create(TrustLevel::VeryLikely, now_unix_s)?;
        for member in &group.members {
            groups.add_member(gid, *member)?;
        }
        for edge in &group.edges {
            edges_repo.insert(&SimilarityEdge {
                group_id: gid,
                file_a: edge.a,
                file_b: edge.b,
                score_x1000: similarity_score_x1000(edge.distance),
                partial_span: None,
                intro_outro: false,
            })?;
        }
        outcome.groups_created += 1;
        outcome.members_added += group.members.len();
        outcome.edges_added += group.edges.len();
    }
    Ok(outcome)
}

fn similarity_score_x1000(distance: u32) -> i32 {
    let clamped = distance.min(PHASH_BITS);
    let score = (PHASH_BITS - clamped) * 1000 / PHASH_BITS;
    i32::try_from(score).unwrap_or(1000)
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

    #[test]
    fn params_reject_non_divisor_band_counts() {
        for bad in [0usize, 3, 5, 7, 65] {
            assert!(
                LshParams::new(bad, 4).is_err(),
                "{bad} bands must be rejected"
            );
        }
        for ok in [1usize, 2, 4, 8, 16, 32, 64] {
            assert!(LshParams::new(ok, 4).is_ok(), "{ok} bands must be accepted");
        }
    }

    #[test]
    fn params_reject_oversized_distance() {
        assert!(LshParams::new(8, 65).is_err());
        assert!(LshParams::new(8, 64).is_ok());
    }

    #[test]
    fn band_bits_and_recall_guarantee() {
        let p = LshParams::new(8, 6).unwrap();
        assert_eq!(p.band_bits(), 8);
        assert!(p.guarantees_recall(), "8 bands > 6 distance ⇒ guaranteed");

        let loose = LshParams::new(4, 10).unwrap();
        assert!(
            !loose.guarantees_recall(),
            "4 bands < 10 distance ⇒ probabilistic"
        );
    }

    #[test]
    fn default_is_eight_bands_distance_six() {
        let p = LshParams::default();
        assert_eq!(p.bands(), 8);
        assert_eq!(p.max_distance(), 6);
        assert_eq!(p.band_bits(), 8);
    }

    #[test]
    fn pigeonhole_guarantees_a_shared_band_within_threshold() {
        let params = LshParams::new(8, 7).unwrap();
        let base = 0xDEAD_BEEF_1234_5678u64;
        for flips in 0..u32::try_from(params.bands()).unwrap() {
            let other = flip_low_bits(base, flips);
            let index = LshIndex::build([(FileId(1), base), (FileId(2), other)], params);
            assert!(
                index.candidates(base).contains(&FileId(2)),
                "{flips} flipped bits must still collide in some band",
            );
        }
    }

    #[test]
    fn build_skips_uninformative_zero_hashes() {
        let index = LshIndex::build(
            [(FileId(1), 0), (FileId(2), 0), (FileId(3), 0xFF)],
            LshParams::default(),
        );
        assert_eq!(index.len(), 1, "only the non-zero hash is indexed");
        assert_eq!(index.skipped_uninformative(), 2);
    }

    #[test]
    fn query_returns_near_and_rejects_far() {
        let params = LshParams::new(8, 6).unwrap();
        let base = 0x0123_4567_89AB_CDEFu64;
        let near = flip_low_bits(base, 4);
        let far = flip_low_bits(base, 20);
        let index = LshIndex::build(
            [(FileId(10), base), (FileId(20), near), (FileId(30), far)],
            params,
        );
        let hits: Vec<FileId> = index.query(base).into_iter().map(|m| m.file_id).collect();
        assert!(
            hits.contains(&FileId(10)),
            "self hash matches at distance 0"
        );
        assert!(
            hits.contains(&FileId(20)),
            "near copy within threshold matches"
        );
        assert!(
            !hits.contains(&FileId(30)),
            "far copy beyond threshold filtered"
        );
    }

    #[test]
    fn query_orders_by_distance_then_id() {
        let params = LshParams::new(8, 6).unwrap();
        let base = 0xFFFF_0000_FFFF_0000u64;
        let index = LshIndex::build(
            [
                (FileId(1), flip_low_bits(base, 5)),
                (FileId(2), base),
                (FileId(3), flip_low_bits(base, 2)),
            ],
            params,
        );
        let order: Vec<(FileId, u32)> = index
            .query(base)
            .into_iter()
            .map(|m| (m.file_id, m.distance))
            .collect();
        assert_eq!(order, vec![(FileId(2), 0), (FileId(3), 2), (FileId(1), 5)]);
    }

    #[test]
    fn plan_groups_near_copies_and_separates_distinct() {
        let params = LshParams::new(8, 6).unwrap();
        let a = 0x1111_2222_3333_4444u64;
        let b = 0xAAAA_BBBB_CCCC_DDDDu64;
        let plan = plan_near_duplicates(
            [
                (FileId(1), a),
                (FileId(2), flip_low_bits(a, 3)),
                (FileId(3), b),
                (FileId(4), flip_low_bits(b, 2)),
            ],
            params,
        );
        assert_eq!(plan.groups.len(), 2, "two distinct near-duplicate clusters");
        assert_eq!(plan.groups[0].members, vec![FileId(1), FileId(2)]);
        assert_eq!(plan.groups[1].members, vec![FileId(3), FileId(4)]);
        assert_eq!(plan.skipped_uninformative, 0);
    }

    #[test]
    fn plan_unions_transitive_chain_into_one_group() {
        let params = LshParams::new(8, 6).unwrap();
        let a = 0xF0F0_F0F0_F0F0_F0F0u64;
        let b = flip_low_bits(a, 3);
        let c = flip_low_bits(a, 5);
        let plan = plan_near_duplicates([(FileId(1), a), (FileId(2), b), (FileId(3), c)], params);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(
            plan.groups[0].members,
            vec![FileId(1), FileId(2), FileId(3)]
        );
    }

    #[test]
    fn plan_is_deterministic() {
        let params = LshParams::default();
        let items = [
            (FileId(7), 0x00FF_00FF_00FF_00FFu64),
            (FileId(3), flip_low_bits(0x00FF_00FF_00FF_00FF, 2)),
            (FileId(9), 0x1234_5678_9ABC_DEF0u64),
        ];
        let first = plan_near_duplicates(items, params);
        let second = plan_near_duplicates(items, params);
        assert_eq!(first, second);
    }

    #[test]
    fn candidate_generation_stays_well_below_all_pairs() {
        const N: u64 = 2_000;
        let params = LshParams::default();
        let mut state = 0x5151_5151_5151_5151u64;
        let items: Vec<(FileId, u64)> = (1..=N)
            .map(|i| {
                let h = splitmix64(&mut state) | 1;
                #[allow(clippy::cast_possible_wrap)]
                (FileId(i as i64), h)
            })
            .collect();

        let plan = plan_near_duplicates(items, params);
        let all_pairs = (N * (N - 1) / 2) as usize;
        assert!(
            plan.candidate_pairs_examined < all_pairs / 10,
            "examined {} of {} all-pairs — index is not pruning",
            plan.candidate_pairs_examined,
            all_pairs,
        );
    }

    #[test]
    fn similarity_score_endpoints() {
        assert_eq!(similarity_score_x1000(0), 1000);
        assert_eq!(similarity_score_x1000(64), 0);
        assert_eq!(similarity_score_x1000(6), (58 * 1000) / 64);
    }

    fn clustered_corpus(n: usize, seed: u64) -> Vec<(FileId, u64)> {
        let mut noise = seed;
        (0..n)
            .map(|i| {
                let id = FileId(i64::try_from(i).unwrap_or(i64::MAX) + 1);
                let slot = i % 5;
                let phash = if slot < 2 {
                    let mut s = (i / 5) as u64;
                    s = s.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                    let base = splitmix64(&mut s) | 1;
                    flip_low_bits(base, u32::try_from(slot).unwrap_or(0) * 2)
                } else {
                    splitmix64(&mut noise) | 1
                };
                (id, phash)
            })
            .collect()
    }

    fn plan_edges(plan: &NearDuplicatePlan) -> Vec<NearEdge> {
        plan.groups
            .iter()
            .flat_map(|g| g.edges.iter().copied())
            .collect()
    }

    #[test]
    fn incremental_with_all_changed_equals_full() {
        let params = LshParams::default();
        let items = clustered_corpus(500, 0xABCD_0001);
        let full = plan_near_duplicates(items.iter().copied(), params);
        let changed: BTreeSet<FileId> = items.iter().map(|(id, _)| *id).collect();
        let incr = plan_near_duplicates_incremental(items.iter().copied(), &[], &changed, params);
        assert_eq!(incr.groups, full.groups, "groups must match the full plan");
        assert_eq!(
            incr.candidate_pairs_examined, full.candidate_pairs_examined,
            "all-changed must examine the same pairs as the full pass",
        );
    }

    #[test]
    fn incremental_delta_matches_full_and_examines_far_fewer_pairs() {
        let params = LshParams::default();
        let old = clustered_corpus(2_000, 0x1234_5678);
        let prev = plan_edges(&plan_near_duplicates(old.iter().copied(), params));

        let mut new_items = old.clone();
        let mut changed: BTreeSet<FileId> = BTreeSet::new();
        let mut s = 0x9999_0001u64;
        for idx in [10usize, 20, 30, 40, 50] {
            new_items[idx].1 = splitmix64(&mut s) | 1;
            changed.insert(new_items[idx].0);
        }
        let base0 = old[0].1;
        for k in 0..3u32 {
            let id = FileId(2_001 + i64::from(k));
            new_items.push((id, flip_low_bits(base0, k * 2)));
            changed.insert(id);
        }

        let full_new = plan_near_duplicates(new_items.iter().copied(), params);
        let incr =
            plan_near_duplicates_incremental(new_items.iter().copied(), &prev, &changed, params);

        assert_eq!(
            incr.groups, full_new.groups,
            "incremental grouping must equal a full rebuild of the new corpus",
        );
        assert!(
            incr.candidate_pairs_examined < full_new.candidate_pairs_examined / 5,
            "incremental examined {} pairs vs full {} — delta work is not bounded",
            incr.candidate_pairs_examined,
            full_new.candidate_pairs_examined,
        );
    }

    #[test]
    fn incremental_drops_edges_of_removed_files() {
        let params = LshParams::default();
        let old = clustered_corpus(100, 0x0077_0077);
        let prev = plan_edges(&plan_near_duplicates(old.iter().copied(), params));

        let new_items: Vec<(FileId, u64)> = old
            .iter()
            .copied()
            .filter(|(id, _)| *id != FileId(1))
            .collect();
        let changed: BTreeSet<FileId> = BTreeSet::new();

        let full_new = plan_near_duplicates(new_items.iter().copied(), params);
        let incr =
            plan_near_duplicates_incremental(new_items.iter().copied(), &prev, &changed, params);
        assert_eq!(incr.groups, full_new.groups);
    }

    #[test]
    fn incremental_is_deterministic() {
        let params = LshParams::default();
        let items = clustered_corpus(64, 5);
        let changed: BTreeSet<FileId> = [FileId(3), FileId(9)].into_iter().collect();
        let first = plan_near_duplicates_incremental(items.iter().copied(), &[], &changed, params);
        let second = plan_near_duplicates_incremental(items.iter().copied(), &[], &changed, params);
        assert_eq!(first, second);
    }
}
