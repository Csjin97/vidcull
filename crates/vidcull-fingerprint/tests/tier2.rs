use vidcull_fingerprint::tier1::GrayFrame;
use vidcull_fingerprint::tier2::{
    SEQUENCE_STABILITY_THRESHOLD, SceneHash, Tier2Fingerprint, TimedFrame, build_tier2,
    sequence_similarity,
};

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blob_frame(w: u32, h: u32, shift: f64) -> Vec<u8> {
    let centers = [
        (0.30_f64 + shift, 0.35_f64, 0.18_f64),
        (0.70 - shift, 0.65, 0.12),
    ];
    let mut px = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let fx = f64::from(x) / f64::from(w);
            let fy = f64::from(y) / f64::from(h);
            let mut v = 0.0;
            for (cx, cy, s) in centers {
                let d2 = (fx - cx).powi(2) + (fy - cy).powi(2);
                v += (-d2 / (2.0 * s * s)).exp();
            }
            px[(y * w + x) as usize] = (v.min(1.0) * 255.0).round() as u8;
        }
    }
    px
}

fn split_frame(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            px[(y * w + x) as usize] = if x < w / 2 { 20 } else { 235 };
        }
    }
    px
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn reencode(base: &[u8], seed: u64, amp: i32, brightness: i32) -> Vec<u8> {
    let mut s = seed;
    base.iter()
        .map(|&p| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let n = ((s >> 33) as i32 % (2 * amp + 1)) - amp;
            (i32::from(p) + n + brightness).clamp(0, 255) as u8
        })
        .collect()
}

fn timed(buf: &[u8], w: u32, h: u32, ts: u64) -> TimedFrame<'_> {
    TimedFrame {
        timestamp_ms: ts,
        frame: GrayFrame {
            width: w,
            height: h,
            pixels: buf,
        },
    }
}

fn sample_clip(w: u32, h: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        blob_frame(w, h, 0.0),
        blob_frame(w, h, 0.12),
        split_frame(w, h),
    )
}

#[track_caller]
fn assert_score(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-12, "expected {want}, got {got}");
}

#[test]
fn identical_keyframes_yield_identical_sequence() {
    let (a, b, c) = sample_clip(320, 180);
    let frames = [
        timed(&a, 320, 180, 0),
        timed(&b, 320, 180, 1000),
        timed(&c, 320, 180, 2000),
    ];

    let fp1 = build_tier2(&frames);
    let fp2 = build_tier2(&frames);

    assert_eq!(fp1, fp2, "deterministic build");
    assert_eq!(fp1.to_bytes().unwrap(), fp2.to_bytes().unwrap());
    assert_score(sequence_similarity(&fp1, &fp2), 1.0);
}

#[test]
fn build_emits_one_scene_per_frame_with_timestamps() {
    let (a, b, c) = sample_clip(128, 96);
    let frames = [
        timed(&a, 128, 96, 0),
        timed(&b, 128, 96, 1500),
        timed(&c, 128, 96, 4200),
    ];
    let fp = build_tier2(&frames);
    assert_eq!(fp.scenes.len(), 3);
    assert_eq!(fp.scenes[0].timestamp_ms, 0);
    assert_eq!(fp.scenes[1].timestamp_ms, 1500);
    assert_eq!(fp.scenes[2].timestamp_ms, 4200);
    assert_ne!(fp.scenes[0].phash, fp.scenes[2].phash);
}

#[test]
fn degenerate_frames_are_skipped() {
    let good = blob_frame(160, 90, 0.0);
    let good2 = split_frame(160, 90);
    let empty: Vec<u8> = Vec::new();
    let frames = [
        timed(&good, 160, 90, 0),
        timed(&empty, 0, 0, 500),
        timed(&good2, 160, 90, 1000),
    ];
    let fp = build_tier2(&frames);
    assert_eq!(fp.scenes.len(), 2);
    assert_eq!(fp.scenes[0].timestamp_ms, 0);
    assert_eq!(fp.scenes[1].timestamp_ms, 1000);
}

#[test]
fn serialization_round_trips_preserving_order() {
    let (a, b, c) = sample_clip(256, 144);
    let frames = [
        timed(&a, 256, 144, 0),
        timed(&b, 256, 144, 2000),
        timed(&c, 256, 144, 5000),
    ];
    let fp = build_tier2(&frames);
    let bytes = fp.to_bytes().unwrap();
    let decoded = Tier2Fingerprint::from_bytes(&bytes).unwrap();
    assert_eq!(fp, decoded);
    let ts: Vec<u64> = decoded.scenes.iter().map(|s| s.timestamp_ms).collect();
    assert_eq!(ts, vec![0, 2000, 5000]);
}

#[test]
fn serialized_sequence_stays_well_under_20kb() {
    let scenes: Vec<SceneHash> = (0..256u64)
        .map(|i| SceneHash {
            timestamp_ms: i * 1_000_000 + 0x7FFF_FFFF_FFFF_FFFF,
            phash: 0xDEAD_BEEF_F00D_BAAD ^ i,
        })
        .collect();
    let fp = Tier2Fingerprint { scenes };
    let bytes = fp.to_bytes().unwrap();
    assert!(
        bytes.len() < 20 * 1024,
        "tier2 blob must stay <20KB, got {} bytes for 256 scenes",
        bytes.len()
    );
}

#[test]
fn empty_sequence_round_trips_and_matches_itself() {
    let fp = build_tier2(&[]);
    assert!(fp.scenes.is_empty());
    let bytes = fp.to_bytes().unwrap();
    assert_eq!(Tier2Fingerprint::from_bytes(&bytes).unwrap(), fp);
    assert_score(sequence_similarity(&fp, &fp), 1.0);
}

#[test]
fn temporal_flow_is_consecutive_hamming() {
    let (a, b, c) = sample_clip(200, 120);
    let frames = [
        timed(&a, 200, 120, 0),
        timed(&b, 200, 120, 1000),
        timed(&c, 200, 120, 2000),
    ];
    let fp = build_tier2(&frames);
    let flow = fp.temporal_flow();
    assert_eq!(flow.len(), 2);
    let h01 = (fp.scenes[0].phash ^ fp.scenes[1].phash).count_ones();
    let h12 = (fp.scenes[1].phash ^ fp.scenes[2].phash).count_ones();
    assert_eq!(flow, vec![h01, h12]);
}

#[test]
fn single_scene_has_empty_flow() {
    let a = blob_frame(64, 64, 0.0);
    let fp = build_tier2(&[timed(&a, 64, 64, 0)]);
    assert_eq!(fp.scenes.len(), 1);
    assert!(fp.temporal_flow().is_empty());
}

#[test]
fn reencoded_video_still_matches_original_sequence() {
    let (a, b, c) = sample_clip(320, 180);
    let original = build_tier2(&[
        timed(&a, 320, 180, 0),
        timed(&b, 320, 180, 1000),
        timed(&c, 320, 180, 2000),
    ]);

    let (ra, rb, rc) = (
        reencode(&a, 0x11, 12, 25),
        reencode(&b, 0x22, 12, 25),
        reencode(&c, 0x33, 12, 25),
    );
    let reencoded = build_tier2(&[
        timed(&ra, 320, 180, 0),
        timed(&rb, 320, 180, 1000),
        timed(&rc, 320, 180, 2000),
    ]);

    let score = sequence_similarity(&original, &reencoded);
    assert!(
        score >= SEQUENCE_STABILITY_THRESHOLD,
        "re-encode sequence similarity {score:.3} below threshold {SEQUENCE_STABILITY_THRESHOLD:.3}"
    );
}

#[test]
fn distinct_videos_have_low_sequence_similarity() {
    let (a, b, c) = sample_clip(256, 256);
    let one = build_tier2(&[
        timed(&a, 256, 256, 0),
        timed(&b, 256, 256, 1000),
        timed(&c, 256, 256, 2000),
    ]);
    let s = split_frame(256, 256);
    let two = build_tier2(&[
        timed(&s, 256, 256, 0),
        timed(&s, 256, 256, 1000),
        timed(&a, 256, 256, 2000),
    ]);
    let score = sequence_similarity(&one, &two);
    assert!(
        score < SEQUENCE_STABILITY_THRESHOLD,
        "unrelated clips should not match: score={score:.3}"
    );
}

#[test]
fn length_mismatch_penalizes_similarity() {
    let (a, b, c) = sample_clip(160, 90);
    let full = build_tier2(&[
        timed(&a, 160, 90, 0),
        timed(&b, 160, 90, 1000),
        timed(&c, 160, 90, 2000),
    ]);
    let prefix = build_tier2(&[timed(&a, 160, 90, 0), timed(&b, 160, 90, 1000)]);
    let score = sequence_similarity(&full, &prefix);
    assert!(score > 0.0 && score < 1.0, "prefix score={score:.3}");
    assert!(
        (score - 2.0 / 3.0).abs() < 1e-9,
        "expected 2/3, got {score:.6}"
    );
}

#[test]
fn empty_versus_nonempty_does_not_match() {
    let a = blob_frame(64, 64, 0.0);
    let nonempty = build_tier2(&[timed(&a, 64, 64, 0)]);
    let empty = build_tier2(&[]);
    assert_score(sequence_similarity(&empty, &nonempty), 0.0);
    assert_score(sequence_similarity(&nonempty, &empty), 0.0);
}

#[test]
fn similar_consecutive_frames_are_pruned() {
    let good = blob_frame(160, 90, 0.0);
    let good2 = split_frame(160, 90);

    let frames_dup = [timed(&good, 160, 90, 0), timed(&good, 160, 90, 1000)];
    let fp_dup = build_tier2(&frames_dup);
    assert_eq!(fp_dup.scenes.len(), 1);
    assert_eq!(fp_dup.scenes[0].timestamp_ms, 0);

    let frames_diff = [timed(&good, 160, 90, 0), timed(&good2, 160, 90, 1000)];
    let fp_diff = build_tier2(&frames_diff);
    assert_eq!(fp_diff.scenes.len(), 2);
    assert_eq!(fp_diff.scenes[0].timestamp_ms, 0);
    assert_eq!(fp_diff.scenes[1].timestamp_ms, 1000);
}
