use vidcull_core::types::{BestCopyMode, Codec, FileId, Resolution};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::near::{LshIndex, LshParams};
use vidcull_matcher::partial::{AnchorIndex, AnchorParams};
use vidcull_matcher::ranking::{QualityScore, score_quality, select_best};

use proptest::prelude::*;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn flip_low(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
    let mut state = seed;
    let scenes = (0..n)
        .map(|i| SceneHash {
            timestamp_ms: u64::try_from(i).unwrap_or(0) * 1000,
            phash: splitmix64(&mut state) | (1 << 40),
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn clip_of(source: &Tier2Fingerprint, start: usize, len: usize, perturb: u32) -> Tier2Fingerprint {
    let scenes = source.scenes[start..start + len]
        .iter()
        .enumerate()
        .map(|(i, s)| SceneHash {
            timestamp_ms: u64::try_from(i).unwrap_or(0) * 1000,
            phash: flip_low(s.phash, perturb),
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn score_test(
    resolution: Option<Resolution>,
    bitrate_bps: Option<i64>,
    codec: Option<&Codec>,
    size_bytes: i64,
) -> QualityScore {
    score_quality(
        resolution,
        bitrate_bps,
        codec,
        None,
        size_bytes,
        None,
        None,
        None,
        None,
        BestCopyMode::SpaceSaving,
    )
}

fn quality_strategy() -> impl Strategy<Value = QualityScore> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(
            |(pixels, encoder, laplacian, dct, bpp, bitrate, size)| QualityScore {
                pixels: u64::from(pixels),
                encoder_score: encoder % 2,
                laplacian_variance: u64::from(laplacian),
                dct_energy: u64::from(dct),
                bpp_scaled: u64::from(bpp),
                effective_bitrate: u64::from(bitrate),
                size_bytes: u64::from(size),
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]


    #[test]
    fn lsh_collides_within_band_minus_one_bits(
        base in any::<u64>(),
        flips in prop::collection::hash_set(1u32..64, 0..=7),
    ) {
        let params = LshParams::new(8, 6).unwrap();
        let anchor = base | 1;
        let mut other = anchor;
        for &b in &flips {
            other ^= 1u64 << b;
        }
        let index = LshIndex::build([(FileId(1), anchor), (FileId(2), other)], params);
        prop_assert!(
            index.candidates(anchor).contains(&FileId(2)),
            "{} flipped bits must still share a band",
            flips.len(),
        );
    }

    #[test]
    fn lsh_query_is_thresholded_and_ordered(
        hashes in prop::collection::vec(any::<u64>(), 0..=64),
        query in any::<u64>(),
    ) {
        let params = LshParams::default();
        let items: Vec<(FileId, u64)> = hashes
            .into_iter()
            .enumerate()
            .map(|(i, h)| (FileId(i64::try_from(i).unwrap()), h))
            .collect();
        let index = LshIndex::build(items, params);
        let matches = index.query(query);
        for m in &matches {
            prop_assert!(m.distance <= params.max_distance());
        }
        for w in matches.windows(2) {
            prop_assert!(w[0].distance <= w[1].distance);
        }
    }


    #[test]
    fn score_is_monotone_in_resolution(
        w1 in 1u32..=8000, h1 in 1u32..=8000,
        w2 in 1u32..=8000, h2 in 1u32..=8000,
        bitrate in 0i64..=100_000_000,
        size in 0i64..=10_000_000_000,
    ) {
        let r1 = Resolution::new(w1, h1);
        let r2 = Resolution::new(w2, h2);
        let s1 = score_test(Some(r1), Some(bitrate), Some(&Codec::H264), size);
        let s2 = score_test(Some(r2), Some(bitrate), Some(&Codec::H264), size);
        match r1.pixels().cmp(&r2.pixels()) {
            std::cmp::Ordering::Greater => prop_assert!(s1 > s2),
            std::cmp::Ordering::Less => prop_assert!(s1 < s2),
            std::cmp::Ordering::Equal => prop_assert_eq!(s1, s2),
        }
    }

    #[test]
    fn select_best_is_max_then_smallest_id(
        scores in prop::collection::vec(quality_strategy(), 0..=20),
    ) {
        let candidates: Vec<(FileId, QualityScore)> = scores
            .iter()
            .copied()
            .enumerate()
            .map(|(i, s)| (FileId(i64::try_from(i).unwrap()), s))
            .collect();
        let best = select_best(candidates.clone());
        if candidates.is_empty() {
            prop_assert!(best.is_none());
        } else {
            let best_id = best.expect("non-empty input yields a best");
            let best_score = candidates
                .iter()
                .find(|(id, _)| *id == best_id)
                .expect("best id is one of the inputs")
                .1;
            for (_, s) in &candidates {
                prop_assert!(best_score >= *s);
            }
            let smallest_max = candidates
                .iter()
                .filter(|(_, s)| *s == best_score)
                .map(|(id, _)| *id)
                .min()
                .expect("at least one maximum");
            prop_assert_eq!(best_id, smallest_max);
        }
    }


    #[test]
    fn planted_clip_recalls_with_monotone_span(
        seed in any::<u64>(),
        start in 0usize..=25,
        clip_len in 3usize..=8,
        perturb in 0u32..=5,
    ) {
        const SOURCE_LEN: usize = 40;
        let start = start.min(SOURCE_LEN - clip_len);
        let source = source_seq(seed, SOURCE_LEN);
        let clip = clip_of(&source, start, clip_len, perturb);
        let params = AnchorParams::default();
        let index = AnchorIndex::build([(FileId(1), source)], params);

        let hits = index.search(&clip.scenes, None);
        prop_assert_eq!(hits.len(), 1, "the one source must be located");
        let a = hits[0];
        prop_assert_eq!(a.source, FileId(1));
        prop_assert!(a.start_ms <= a.end_ms, "matched span must be timestamp-monotone");
        prop_assert!(a.matched_scenes <= a.clip_scenes);
        prop_assert!(
            a.coverage_x1000 >= params.min_coverage_x1000(),
            "a reported match must clear the coverage floor",
        );
    }
}
