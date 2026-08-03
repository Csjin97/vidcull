use vidcull_core::types::Codec;
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint, phash_frames};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_fingerprint::{GrayFrame, TIER2_BUDGET_BYTES, hamming_distance, sequence_similarity};

use proptest::prelude::*;

const FRAME_SIDE: u32 = 32;
const FRAME_PIXELS: usize = 32 * 32;

fn flip_low(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn one_scene(phash: u64) -> Tier2Fingerprint {
    Tier2Fingerprint {
        scenes: vec![SceneHash {
            timestamp_ms: 0,
            phash,
        }],
    }
}

fn codec_strategy() -> impl Strategy<Value = Codec> {
    prop_oneof![
        Just(Codec::H264),
        Just(Codec::H265),
        Just(Codec::Av1),
        Just(Codec::Vp9),
        Just(Codec::Mpeg2),
        "[a-z0-9_]{1,40}".prop_map(Codec::Other),
    ]
}

fn scene_sequence(max_len: usize) -> impl Strategy<Value = Tier2Fingerprint> {
    prop::collection::vec((any::<u64>(), any::<u64>()), 0..=max_len).prop_map(|pairs| {
        Tier2Fingerprint {
            scenes: pairs
                .into_iter()
                .map(|(timestamp_ms, phash)| SceneHash {
                    timestamp_ms,
                    phash,
                })
                .collect(),
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]


    #[test]
    fn hamming_identity_and_separation(a in any::<u64>(), b in any::<u64>()) {
        prop_assert_eq!(hamming_distance(a, a), 0);
        if a != b {
            prop_assert!(hamming_distance(a, b) > 0);
        }
    }

    #[test]
    fn hamming_is_symmetric_and_bounded(a in any::<u64>(), b in any::<u64>()) {
        prop_assert_eq!(hamming_distance(a, b), hamming_distance(b, a));
        prop_assert!(hamming_distance(a, b) <= 64);
    }

    #[test]
    fn hamming_obeys_triangle_inequality(
        a in any::<u64>(),
        b in any::<u64>(),
        c in any::<u64>(),
    ) {
        prop_assert!(
            hamming_distance(a, c) <= hamming_distance(a, b) + hamming_distance(b, c)
        );
    }


    #[test]
    fn uniform_brightness_shift_preserves_phash(
        lo in 100u8..=150,
        delta in 0u8..=60,
        texture in prop::collection::vec(0u8..=40, FRAME_PIXELS),
    ) {
        let base: Vec<u8> = texture.iter().map(|&t| lo + t).collect();
        let shifted: Vec<u8> = base.iter().map(|&p| p + delta).collect();
        let fb = GrayFrame { width: FRAME_SIDE, height: FRAME_SIDE, pixels: &base };
        let fs = GrayFrame { width: FRAME_SIDE, height: FRAME_SIDE, pixels: &shifted };
        prop_assert_eq!(phash_frames(&[fb]), phash_frames(&[fs]));
    }

    #[test]
    fn phash_is_deterministic(
        pixels in prop::collection::vec(any::<u8>(), FRAME_PIXELS),
    ) {
        let f = GrayFrame { width: FRAME_SIDE, height: FRAME_SIDE, pixels: &pixels };
        prop_assert_eq!(phash_frames(&[f]), phash_frames(&[f]));
    }

    #[test]
    fn phash_is_order_and_repetition_independent(
        a in prop::collection::vec(any::<u8>(), FRAME_PIXELS),
        b in prop::collection::vec(any::<u8>(), FRAME_PIXELS),
    ) {
        let fa = GrayFrame { width: FRAME_SIDE, height: FRAME_SIDE, pixels: &a };
        let fb = GrayFrame { width: FRAME_SIDE, height: FRAME_SIDE, pixels: &b };
        prop_assert_eq!(phash_frames(&[fa, fb]), phash_frames(&[fb, fa]));
        prop_assert_eq!(phash_frames(&[fa]), phash_frames(&[fa, fa]));
    }


    #[test]
    fn gop_signature_mean_never_exceeds_max(
        durations in prop::collection::vec(0u64..=u64::from(u32::MAX), 0..=256),
    ) {
        let sig = GopSignature::from_durations(&durations);
        prop_assert_eq!(sig.keyframe_count as usize, durations.len());
        if let (Some(&min), Some(&max)) = (durations.iter().min(), durations.iter().max()) {
            let (min, max) = (u32::try_from(min).unwrap(), u32::try_from(max).unwrap());
            prop_assert!(sig.mean_gop_ms >= min);
            prop_assert!(sig.mean_gop_ms <= max);
            prop_assert_eq!(sig.max_gop_ms, max);
        } else {
            prop_assert_eq!(sig.mean_gop_ms, 0);
            prop_assert_eq!(sig.max_gop_ms, 0);
        }
    }


    #[test]
    fn tier1_round_trips(
        duration_ms in any::<u64>(),
        codec in codec_strategy(),
        keyframe_count in any::<u32>(),
        mean_gop_ms in any::<u32>(),
        max_gop_ms in any::<u32>(),
        global_phash in any::<u64>(),
    ) {
        let fp = Tier1Fingerprint {
            duration_ms,
            codec,
            gop: GopSignature { keyframe_count, mean_gop_ms, max_gop_ms },
            global_phash,
        };
        let bytes = fp.to_bytes().expect("encode");
        prop_assert!(bytes.len() < 256, "tier1 blob {} bytes ≥ 256", bytes.len());
        prop_assert_eq!(Tier1Fingerprint::from_bytes(&bytes).expect("decode"), fp);
    }


    #[test]
    fn tier2_round_trips_within_budget(fp in scene_sequence(256)) {
        let bytes = fp.to_bytes().expect("encode");
        prop_assert!(
            bytes.len() <= TIER2_BUDGET_BYTES,
            "tier2 blob {} bytes exceeds 20KB budget at {} scenes",
            bytes.len(),
            fp.len(),
        );
        prop_assert_eq!(Tier2Fingerprint::from_bytes(&bytes).expect("decode"), fp);
    }

    #[test]
    fn temporal_flow_length_tracks_scene_count(fp in scene_sequence(64)) {
        let flow = fp.temporal_flow();
        prop_assert_eq!(flow.len(), fp.len().saturating_sub(1));
        for d in flow {
            prop_assert!(d <= 64);
        }
    }


    #[test]
    fn sequence_similarity_is_bounded_and_symmetric(
        a in scene_sequence(32),
        b in scene_sequence(32),
    ) {
        let s = sequence_similarity(&a, &b);
        prop_assert!((0.0..=1.0).contains(&s), "similarity {s} out of [0,1]");
        prop_assert!((s - sequence_similarity(&b, &a)).abs() < 1e-12);
    }

    #[test]
    fn sequence_self_similarity_is_one(fp in scene_sequence(32)) {
        prop_assert!((sequence_similarity(&fp, &fp) - 1.0).abs() < 1e-12);
    }


    #[test]
    fn similarity_is_monotone_in_flipped_bits(
        base in any::<u64>(),
        k1 in 0u32..=64,
        k2 in 0u32..=64,
    ) {
        let (fewer, more) = if k1 <= k2 { (k1, k2) } else { (k2, k1) };
        let anchor = one_scene(base);
        let near = one_scene(flip_low(base, fewer));
        let far = one_scene(flip_low(base, more));
        let s_near = sequence_similarity(&anchor, &near);
        let s_far = sequence_similarity(&anchor, &far);
        prop_assert!(
            s_near + 1e-12 >= s_far,
            "more flips ({more}) scored {s_far} > fewer ({fewer}) {s_near}",
        );
    }
}
