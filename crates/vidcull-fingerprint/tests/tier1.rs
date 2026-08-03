use vidcull_core::{Codec, VideoDuration};
use vidcull_fingerprint::tier1::{
    GopSignature, GrayFrame, REENCODE_STABILITY_THRESHOLD, Tier1Fingerprint, build_tier1,
    hamming_distance, phash_frames,
};

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn blob_frame(w: u32, h: u32) -> Vec<u8> {
    let centers = [(0.30_f64, 0.35_f64, 0.18_f64), (0.70, 0.65, 0.12)];
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

fn frame(buf: &[u8], w: u32, h: u32) -> GrayFrame<'_> {
    GrayFrame {
        width: w,
        height: h,
        pixels: buf,
    }
}

#[test]
fn identical_input_yields_identical_fingerprint() {
    let buf = blob_frame(320, 180);
    let frames = [frame(&buf, 320, 180)];
    let gops = [500_u64, 500, 480];

    let a = build_tier1(
        VideoDuration::from_millis(1_480),
        Codec::H264,
        &gops,
        &frames,
    );
    let b = build_tier1(
        VideoDuration::from_millis(1_480),
        Codec::H264,
        &gops,
        &frames,
    );

    assert_eq!(a, b);
    assert_eq!(a.to_bytes().unwrap(), b.to_bytes().unwrap());
}

#[test]
fn serialized_fingerprint_is_well_under_256_bytes() {
    let buf = blob_frame(320, 180);
    let frames = [frame(&buf, 320, 180)];
    let gops = [1_000_u64; 64];
    let fp = build_tier1(
        VideoDuration::from_millis(3_600_000),
        Codec::Other("some_unusually_long_codec_identifier".into()),
        &gops,
        &frames,
    );

    let bytes = fp.to_bytes().unwrap();
    assert!(
        bytes.len() < 256,
        "tier1 blob must stay <256 bytes, got {}",
        bytes.len()
    );
}

#[test]
fn serialization_round_trips() {
    let buf = blob_frame(256, 144);
    let frames = [frame(&buf, 256, 144)];
    let gops = [333_u64, 333, 334];
    let fp = build_tier1(
        VideoDuration::from_millis(1_000),
        Codec::H265,
        &gops,
        &frames,
    );

    let bytes = fp.to_bytes().unwrap();
    let decoded = Tier1Fingerprint::from_bytes(&bytes).unwrap();
    assert_eq!(fp, decoded);
}

#[test]
fn codec_signature_is_preserved() {
    let buf = blob_frame(64, 64);
    let frames = [frame(&buf, 64, 64)];
    let fp = build_tier1(
        VideoDuration::from_millis(500),
        Codec::Other("av1".into()),
        &[500],
        &frames,
    );
    assert_eq!(fp.codec, Codec::Other("av1".into()));
    assert_eq!(fp.duration_ms, 500);
}

#[test]
fn gop_signature_summarizes_spans() {
    let sig = GopSignature::from_durations(&[400, 600, 800]);
    assert_eq!(sig.keyframe_count, 3);
    assert_eq!(sig.mean_gop_ms, 600);
    assert_eq!(sig.max_gop_ms, 800);
}

#[test]
fn gop_signature_handles_empty_input() {
    let sig = GopSignature::from_durations(&[]);
    assert_eq!(sig.keyframe_count, 0);
    assert_eq!(sig.mean_gop_ms, 0);
    assert_eq!(sig.max_gop_ms, 0);
}

#[test]
fn empty_frame_set_is_handled_without_panic() {
    let fp = build_tier1(VideoDuration::from_millis(0), Codec::H264, &[], &[]);
    assert_eq!(fp.global_phash, 0);
    let bytes = fp.to_bytes().unwrap();
    assert_eq!(Tier1Fingerprint::from_bytes(&bytes).unwrap(), fp);
}

#[test]
fn global_phash_is_order_independent() {
    let a = blob_frame(128, 96);
    let b = split_frame(128, 96);
    let c = reencode(&a, 7, 8, 10);

    let forward = phash_frames(&[frame(&a, 128, 96), frame(&b, 128, 96), frame(&c, 128, 96)]);
    let shuffled = phash_frames(&[frame(&c, 128, 96), frame(&a, 128, 96), frame(&b, 128, 96)]);
    assert_eq!(forward, shuffled, "averaging must be order-independent");
}

#[test]
fn repeating_a_frame_does_not_change_the_hash() {
    let a = blob_frame(200, 120);
    let one = phash_frames(&[frame(&a, 200, 120)]);
    let many = phash_frames(&[
        frame(&a, 200, 120),
        frame(&a, 200, 120),
        frame(&a, 200, 120),
    ]);
    assert_eq!(one, many, "mean of duplicates equals the single frame");
}

#[test]
fn global_phash_is_resize_robust() {
    let big = blob_frame(640, 360);
    let small = blob_frame(160, 90);
    let h_big = phash_frames(&[frame(&big, 640, 360)]);
    let h_small = phash_frames(&[frame(&small, 160, 90)]);

    let hd = hamming_distance(h_big, h_small);
    assert!(hd <= 6, "resize hamming distance too large: {hd}");
}

#[test]
fn global_phash_survives_reencode_perturbations() {
    let base = blob_frame(320, 180);
    let encoded = reencode(&base, 0x1234_5678, 12, 25);

    let h_base = phash_frames(&[frame(&base, 320, 180)]);
    let h_enc = phash_frames(&[frame(&encoded, 320, 180)]);

    let hd = hamming_distance(h_base, h_enc);
    let stability = f64::from(64 - hd) / 64.0;
    assert!(
        stability >= REENCODE_STABILITY_THRESHOLD,
        "re-encode stability {stability:.3} below threshold {REENCODE_STABILITY_THRESHOLD:.3} (hd={hd})"
    );
}

#[test]
fn distinct_content_yields_distant_hashes() {
    let blobs = blob_frame(256, 256);
    let split = split_frame(256, 256);
    let hd = hamming_distance(
        phash_frames(&[frame(&blobs, 256, 256)]),
        phash_frames(&[frame(&split, 256, 256)]),
    );
    assert!(
        hd >= 10,
        "distinct content should be far apart, got hd={hd}"
    );
}

#[test]
fn hamming_distance_counts_differing_bits() {
    assert_eq!(hamming_distance(0, 0), 0);
    assert_eq!(hamming_distance(0b1011, 0b0001), 2);
    assert_eq!(hamming_distance(u64::MAX, 0), 64);
}
