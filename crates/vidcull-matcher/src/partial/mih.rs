use std::collections::HashMap;

use super::Posting;

const PHASH_BITS: u32 = 64;

#[derive(Debug, Clone)]
pub(crate) struct MultiIndexHash {
    chunk_bits: u32,
    radius: u32,
    buckets: Vec<HashMap<u64, Vec<Posting>>>,
}

impl MultiIndexHash {
    pub(crate) fn new(chunks: u32, max_distance: u32) -> Self {
        let chunks = normalize_chunks(chunks);
        let chunk_bits = PHASH_BITS / chunks;
        let radius = max_distance.min(PHASH_BITS) / chunks;
        Self {
            chunk_bits,
            radius,
            buckets: vec![HashMap::new(); chunks as usize],
        }
    }

    #[cfg(test)]
    pub(crate) fn chunks(&self) -> u32 {
        u32::try_from(self.buckets.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn radius(&self) -> u32 {
        self.radius
    }

    pub(crate) fn insert(&mut self, phash: u64, posting: Posting) {
        if phash == 0 {
            return;
        }
        for (c, bucket) in self.buckets.iter_mut().enumerate() {
            let value = chunk_value(phash, u32::try_from(c).unwrap_or(0), self.chunk_bits);
            bucket.entry(value).or_default().push(posting);
        }
    }

    pub(crate) fn remove(&mut self, phash: u64, file_id: vidcull_core::types::FileId) {
        if phash == 0 {
            return;
        }
        for (c, bucket) in self.buckets.iter_mut().enumerate() {
            let value = chunk_value(phash, u32::try_from(c).unwrap_or(0), self.chunk_bits);
            if let Some(list) = bucket.get_mut(&value) {
                list.retain(|p| p.file_id != file_id);
                if list.is_empty() {
                    bucket.remove(&value);
                }
            }
        }
    }

    pub(crate) fn post_keys(&self, phash: u64) -> Vec<(u32, u64)> {
        if phash == 0 {
            return Vec::new();
        }
        (0..self.buckets.len())
            .map(|c| {
                let c = u32::try_from(c).unwrap_or(0);
                (c, chunk_value(phash, c, self.chunk_bits))
            })
            .collect()
    }

    pub(crate) fn query_keys(&self, phash: u64) -> Vec<(u32, Vec<u64>)> {
        if phash == 0 {
            return Vec::new();
        }
        let mut keys = Vec::new();
        let mut out = Vec::with_capacity(self.buckets.len());
        for c in 0..self.buckets.len() {
            let c = u32::try_from(c).unwrap_or(0);
            let value = chunk_value(phash, c, self.chunk_bits);
            hamming_ball(value, self.chunk_bits, self.radius, &mut keys);
            out.push((c, keys.clone()));
        }
        out
    }

    pub(crate) fn candidates(&self, phash: u64) -> Vec<Posting> {
        if phash == 0 {
            return Vec::new();
        }
        let mut candidates = Vec::with_capacity(32);
        let mut keys: Vec<u64> = Vec::new();
        for (c, bucket) in self.buckets.iter().enumerate() {
            let value = chunk_value(phash, u32::try_from(c).unwrap_or(0), self.chunk_bits);
            hamming_ball(value, self.chunk_bits, self.radius, &mut keys);
            for key in &keys {
                if let Some(list) = bucket.get(key) {
                    candidates.extend(list.iter().copied());
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }
}

fn chunk_value(phash: u64, c: u32, chunk_bits: u32) -> u64 {
    let mask = if chunk_bits >= PHASH_BITS {
        u64::MAX
    } else {
        (1u64 << chunk_bits) - 1
    };
    (phash >> (chunk_bits * c)) & mask
}

fn normalize_chunks(chunks: u32) -> u32 {
    if chunks == 0 {
        return 1;
    }
    if chunks >= PHASH_BITS {
        return PHASH_BITS;
    }
    let mut p = 1u32;
    while p * 2 <= chunks {
        p *= 2;
    }
    p
}

fn hamming_ball(value: u64, width: u32, radius: u32, out: &mut Vec<u64>) {
    out.clear();
    out.push(value);
    if radius == 0 {
        return;
    }
    let width = width.min(PHASH_BITS) as usize;
    let mut combo: Vec<usize> = Vec::new();
    for k in 1..=radius as usize {
        combinations(width, k, 0, &mut combo, &mut |bits| {
            let mut v = value;
            for &b in bits {
                v ^= 1u64 << b;
            }
            out.push(v);
        });
    }
}

fn combinations(
    width: usize,
    k: usize,
    start: usize,
    combo: &mut Vec<usize>,
    emit: &mut impl FnMut(&[usize]),
) {
    if combo.len() == k {
        emit(combo);
        return;
    }
    let need = k - combo.len();
    let mut i = start;
    while i + need <= width {
        combo.push(i);
        combinations(width, k, i + 1, combo, emit);
        combo.pop();
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use vidcull_core::types::FileId;
    use vidcull_fingerprint::hamming_distance;

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn posting(id: i64, idx: usize) -> Posting {
        Posting {
            file_id: FileId(id),
            scene_index: idx,
        }
    }

    #[test]
    fn default_config_is_four_chunks_radius_one_for_distance_six() {
        let mih = MultiIndexHash::new(4, 6);
        assert_eq!(mih.chunks(), 4);
        assert_eq!(
            mih.radius(),
            1,
            "floor(6/4) = 1 — the pigeonhole recall bound"
        );
    }

    #[test]
    fn non_divisor_chunk_count_is_rounded_down_to_a_divisor() {
        assert_eq!(MultiIndexHash::new(3, 6).chunks(), 2);
        assert_eq!(MultiIndexHash::new(0, 6).chunks(), 1);
        assert_eq!(MultiIndexHash::new(7, 6).chunks(), 4);
        assert_eq!(MultiIndexHash::new(100, 6).chunks(), 64);
    }

    #[test]
    fn hamming_ball_radius_one_is_value_plus_single_flips() {
        let mut out = Vec::new();
        hamming_ball(0b0000, 4, 1, &mut out);
        out.sort_unstable();
        assert_eq!(out, vec![0b0000, 0b0001, 0b0010, 0b0100, 0b1000]);
    }

    #[test]
    fn hamming_ball_radius_two_includes_all_pairs() {
        let mut out = Vec::new();
        hamming_ball(0, 4, 2, &mut out);
        out.sort_unstable();
        out.dedup();
        assert_eq!(out.len(), 11);
    }

    #[test]
    fn exact_match_is_always_a_candidate() {
        let mut mih = MultiIndexHash::new(4, 6);
        let h = 0xDEAD_BEEF_1234_5678;
        mih.insert(h, posting(1, 0));
        let got = mih.candidates(h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].file_id, FileId(1));
    }

    #[test]
    fn within_distance_pairs_are_recall_complete_vs_brute_force() {
        let max_distance = 6;
        let mih_chunks = 4;
        let mut state = 0x5151_2727_9393_0101u64;
        let mut hashes: Vec<u64> = Vec::new();
        let mut mih = MultiIndexHash::new(mih_chunks, max_distance);
        for i in 0..4000i64 {
            let h = splitmix64(&mut state);
            hashes.push(h);
            mih.insert(h, posting(i, 0));
        }

        for q in 0..2000usize {
            let query = if q % 2 == 0 {
                splitmix64(&mut state)
            } else {
                let base = hashes[q % hashes.len()];
                let flips = u32::try_from(splitmix64(&mut state) % u64::from(max_distance + 1))
                    .unwrap_or(0);
                flip_random_bits(base, flips, &mut state)
            };
            let candidates: BTreeSet<i64> = mih
                .candidates(query)
                .into_iter()
                .map(|p| p.file_id.0)
                .collect();
            for (i, &h) in hashes.iter().enumerate() {
                if hamming_distance(query, h) <= max_distance {
                    assert!(
                        candidates.contains(&i64::try_from(i).unwrap()),
                        "MIH missed a within-{max_distance} pair: query={query:x} hash={h:x}",
                    );
                }
            }
        }
    }

    #[test]
    fn db_resident_keys_reproduce_the_in_memory_candidate_set() {
        let max_distance = 6;
        let mih_chunks = 4;
        let mut state = 0xABCD_1234_5678_9999u64;
        let mut mih = MultiIndexHash::new(mih_chunks, max_distance);

        let mut table: HashMap<(u32, u64), BTreeSet<i64>> = HashMap::new();
        let mut hashes: Vec<u64> = Vec::new();
        for i in 0..1500i64 {
            let h = splitmix64(&mut state);
            hashes.push(h);
            mih.insert(h, posting(i, 0));
            for (chunk, slice) in mih.post_keys(h) {
                table.entry((chunk, slice)).or_default().insert(i);
            }
        }

        for q in 0..800usize {
            let query = if q % 2 == 0 {
                splitmix64(&mut state)
            } else {
                let base = hashes[q % hashes.len()];
                let flips = u32::try_from(splitmix64(&mut state) % u64::from(max_distance + 1))
                    .unwrap_or(0);
                flip_random_bits(base, flips, &mut state)
            };

            let in_mem: BTreeSet<i64> = mih
                .candidates(query)
                .into_iter()
                .map(|p| p.file_id.0)
                .collect();

            let mut db_side: BTreeSet<i64> = BTreeSet::new();
            for (chunk, keys) in mih.query_keys(query) {
                for key in keys {
                    if let Some(ids) = table.get(&(chunk, key)) {
                        db_side.extend(ids.iter().copied());
                    }
                }
            }
            assert_eq!(
                in_mem, db_side,
                "DB-resident keys must yield the same candidate files as candidates()",
            );
        }
    }

    #[test]
    fn post_keys_and_query_keys_skip_the_zero_hash() {
        let mih = MultiIndexHash::new(4, 6);
        assert!(mih.post_keys(0).is_empty());
        assert!(mih.query_keys(0).is_empty());
        assert_eq!(mih.post_keys(0xDEAD_BEEF_1234_5678).len(), 4);
    }

    #[test]
    fn remove_drops_every_posting_of_a_file() {
        let mut mih = MultiIndexHash::new(4, 6);
        let h1 = 0x1111_2222_3333_4444;
        let h2 = 0x1111_2222_3333_4445;
        mih.insert(h1, posting(1, 0));
        mih.insert(h2, posting(2, 0));
        mih.remove(h1, FileId(1));
        let got = mih.candidates(h1);
        assert!(
            got.iter().all(|p| p.file_id != FileId(1)),
            "file 1's postings are gone",
        );
        assert!(mih.candidates(h2).iter().any(|p| p.file_id == FileId(2)));
    }

    #[test]
    fn zero_hash_is_never_posted_or_queried() {
        let mut mih = MultiIndexHash::new(4, 6);
        mih.insert(0, posting(1, 0));
        assert!(mih.candidates(0).is_empty(), "all-zero is uninformative");
    }

    fn flip_random_bits(h: u64, n: u32, state: &mut u64) -> u64 {
        let mut out = h;
        let mut chosen: BTreeSet<u32> = BTreeSet::new();
        while u32::try_from(chosen.len()).unwrap_or(u32::MAX) < n {
            let bit = u32::try_from(splitmix64(state) % 64).unwrap_or(0);
            chosen.insert(bit);
        }
        for b in chosen {
            out ^= 1u64 << b;
        }
        out
    }
}
