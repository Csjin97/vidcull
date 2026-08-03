use vidcull_core::types::FileId;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::near::{LshParams, plan_near_duplicates};
use vidcull_matcher::partial::{AnchorParams, plan_partial_clips};

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn make_source(seed: u64, n: usize) -> Tier2Fingerprint {
    let mut state = seed;
    let scenes = (0..n)
        .map(|i| SceneHash {
            timestamp_ms: i as u64 * 1_000,
            phash: splitmix64(&mut state) | 1,
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn phash_at_distance(base: u64, k: u32) -> u64 {
    if k == 0 {
        return base;
    }
    let mask: u64 = if k >= 64 { u64::MAX } else { (1u64 << k) - 1 };
    base ^ mask
}

fn make_clip(
    source_scenes: &[SceneHash],
    source_start: usize,
    clip_len: usize,
    matched: usize,
    match_dist: u32,
    noise_seed: u64,
) -> Tier2Fingerprint {
    debug_assert!(matched <= clip_len);
    debug_assert!(source_start + matched <= source_scenes.len());
    let mut noise_state = noise_seed;
    let scenes = (0..clip_len)
        .map(|i| {
            if i < matched {
                let src_phash = source_scenes[source_start + i].phash;
                SceneHash {
                    timestamp_ms: i as u64 * 1_000,
                    phash: phash_at_distance(src_phash, match_dist),
                }
            } else {
                SceneHash {
                    timestamp_ms: i as u64 * 1_000,
                    phash: splitmix64(&mut noise_state) | 1,
                }
            }
        })
        .collect();
    Tier2Fingerprint { scenes }
}

#[test]
fn default_lsh_params_guarantees_recall() {
    let params = LshParams::default();
    assert_eq!(params.bands(), LshParams::DEFAULT_BANDS);
    assert_eq!(params.max_distance(), LshParams::DEFAULT_MAX_DISTANCE);
    let bands = params.bands();
    let max_dist = params.max_distance();
    assert!(
        params.guarantees_recall(),
        "default LshParams must guarantee 100% recall \
         (bands={bands} > max_distance={max_dist})"
    );
}

#[test]
fn hamming_sweep_near_dup_boundary() {
    let cases: &[(u32, bool)] = &[(5, true), (6, true), (7, false), (8, false)];

    for &(k, expect_grouped) in cases {
        let hash_a: u64 = 0xA5A5_A5A5_A5A5_A5A5;
        let hash_b = phash_at_distance(hash_a, k);

        assert_eq!(
            (hash_a ^ hash_b).count_ones(),
            k,
            "phash_at_distance({k}) produced the wrong bit count"
        );

        let items = vec![(FileId(1), hash_a), (FileId(2), hash_b)];
        let plan = plan_near_duplicates(items, LshParams::default());

        if expect_grouped {
            assert_eq!(
                plan.groups.len(),
                1,
                "distance {k} (≤ DEFAULT_MAX_DISTANCE=6) must produce exactly 1 group"
            );
            let grp = &plan.groups[0];
            assert_eq!(grp.members.len(), 2);
            assert!(
                grp.members.contains(&FileId(1)) && grp.members.contains(&FileId(2)),
                "group for distance {k} must contain both file ids"
            );
            assert_eq!(grp.edges.len(), 1);
            assert_eq!(
                grp.edges[0].distance, k,
                "edge distance must equal the Hamming distance"
            );
        } else {
            assert_eq!(
                plan.groups.len(),
                0,
                "distance {k} (> DEFAULT_MAX_DISTANCE=6) must produce 0 groups"
            );
        }
    }
}

#[test]
fn coverage_at_floor_is_accepted() {
    let source = make_source(0x1111_2222_3333_4444, 20);
    let clip = make_clip(&source.scenes, 0, 10, 6, 5, 0xDEAD_BEEF_CAFE_0001);

    let plan = plan_partial_clips(
        vec![(FileId(1), source), (FileId(2), clip)],
        AnchorParams::default(),
    );

    assert_eq!(
        plan.matches.len(),
        1,
        "coverage 600/1000 (= floor) must be accepted; got {} matches",
        plan.matches.len()
    );
    let m = &plan.matches[0];
    assert_eq!(m.clip, FileId(2), "the shorter video must be the clip");
    assert_eq!(
        m.alignment.source,
        FileId(1),
        "the longer video must be the source"
    );
    assert_eq!(
        m.alignment.coverage_x1000, 600,
        "coverage_x1000 must be exactly 600 at the boundary"
    );
    assert_eq!(m.alignment.matched_scenes, 6, "exactly 6 scenes aligned");
}

#[test]
fn coverage_below_floor_is_rejected() {
    let source = make_source(0xAAAA_BBBB_CCCC_DDDD, 20);
    let clip = make_clip(&source.scenes, 0, 10, 5, 5, 0xDEAD_BEEF_CAFE_0002);

    let plan = plan_partial_clips(
        vec![(FileId(1), source), (FileId(2), clip)],
        AnchorParams::default(),
    );

    assert_eq!(
        plan.matches.len(),
        0,
        "coverage 500/1000 (< floor 600) must be rejected; got {} matches",
        plan.matches.len()
    );
}
