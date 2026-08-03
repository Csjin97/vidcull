use std::path::Path;
use vidcull_core::types::{Codec, VideoDuration};
use vidcull_fingerprint::tier1::REENCODE_STABILITY_THRESHOLD;
use vidcull_fingerprint::{
    GrayFrame, SEQUENCE_STABILITY_THRESHOLD, Tier1Fingerprint, Tier2Fingerprint, TimedFrame,
    build_tier1, build_tier2, hamming_distance, sequence_similarity,
};
use vidcull_parser::probe_and_decode_sparse;
use vidcull_synth::{
    Container, Encode, FfmpegBinaries, Filter, Recipe, render_recipe, render_source,
};

const BUDGET: usize = 12;

fn binaries_or_skip(test: &str) -> Option<FfmpegBinaries> {
    match FfmpegBinaries::resolve() {
        Ok(bins) => Some(bins),
        Err(e) => {
            eprintln!("SKIP {test}: ffmpeg not resolvable ({e})");
            None
        }
    }
}

fn fingerprint(bins: &FfmpegBinaries, path: &Path) -> (Tier1Fingerprint, Tier2Fingerprint) {
    let decoded = probe_and_decode_sparse(bins, path, BUDGET)
        .unwrap_or_else(|e| panic!("decode {} failed: {e}", path.display()));
    let views: Vec<GrayFrame<'_>> = decoded
        .frames
        .iter()
        .map(|f| GrayFrame {
            width: f.width,
            height: f.height,
            pixels: &f.pixels,
        })
        .collect();
    let duration = decoded.metadata.duration.unwrap_or(VideoDuration::ZERO);
    let t1 = build_tier1(duration, decoded.metadata.codec.clone(), &[], &views);
    let timed: Vec<TimedFrame<'_>> = decoded
        .frames
        .iter()
        .map(|f| TimedFrame {
            timestamp_ms: f.timestamp_ms,
            frame: GrayFrame {
                width: f.width,
                height: f.height,
                pixels: &f.pixels,
            },
        })
        .collect();
    let t2 = build_tier2(&timed);
    (t1, t2)
}

fn tier1_similarity(a: &Tier1Fingerprint, b: &Tier1Fingerprint) -> f64 {
    let hd = hamming_distance(a.global_phash, b.global_phash);
    f64::from(64 - hd) / 64.0
}

#[test]
#[allow(clippy::too_many_lines)]
fn reencode_variants_stay_similar_and_unrelated_sources_separate() {
    let Some(bins) = binaries_or_skip("reencode_variants_stay_similar") else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    let src_a = render_source(&bins, dir, "a_testsrc", "testsrc", 4000, 320, 180, 30, 10)
        .expect("render source A");
    let src_b = render_source(
        &bins,
        dir,
        "b_mandelbrot",
        "mandelbrot",
        4000,
        320,
        180,
        30,
        10,
    )
    .expect("render source B");

    let h265 = render_recipe(&bins, &Recipe::reencode(&src_a, Codec::H265), 1, dir)
        .expect("render H.265 variant");
    let resized = render_recipe(
        &bins,
        &Recipe {
            source: src_a.clone(),
            clip: None,
            filters: vec![Filter::Resize {
                width: 160,
                height: 90,
            }],
            subtitle: None,
            encode: Encode::Reencode {
                codec: Codec::H264,
                bitrate_kbps: None,
            },
            container: Container::Mp4,
        },
        2,
        dir,
    )
    .expect("render resized variant");
    let watermarked = render_recipe(
        &bins,
        &Recipe {
            source: src_a.clone(),
            clip: None,
            filters: vec![Filter::Watermark],
            subtitle: None,
            encode: Encode::Reencode {
                codec: Codec::H264,
                bitrate_kbps: None,
            },
            container: Container::Mp4,
        },
        3,
        dir,
    )
    .expect("render watermarked variant");
    let brighter = render_recipe(
        &bins,
        &Recipe {
            source: src_a.clone(),
            clip: None,
            filters: vec![Filter::Brightness { delta_percent: 15 }],
            subtitle: None,
            encode: Encode::Reencode {
                codec: Codec::H264,
                bitrate_kbps: None,
            },
            container: Container::Mp4,
        },
        4,
        dir,
    )
    .expect("render brightness variant");

    let (a1, a2) = fingerprint(&bins, &src_a);
    let (b1, b2) = fingerprint(&bins, &src_b);

    for (label, path) in [
        ("h265", &h265),
        ("resized", &resized),
        ("watermarked", &watermarked),
        ("brighter", &brighter),
    ] {
        let (v1, v2) = fingerprint(&bins, path);
        let t1 = tier1_similarity(&a1, &v1);
        let t2 = sequence_similarity(&a2, &v2);
        eprintln!(
            "[robustness] {label:<12} tier1={t1:.3} (hd={}) tier2={t2:.3}",
            hamming_distance(a1.global_phash, v1.global_phash)
        );
        assert!(
            t1 >= REENCODE_STABILITY_THRESHOLD,
            "{label}: tier1 similarity {t1:.3} below floor {REENCODE_STABILITY_THRESHOLD}"
        );
        assert!(
            t2 >= SEQUENCE_STABILITY_THRESHOLD,
            "{label}: tier2 similarity {t2:.3} below floor {SEQUENCE_STABILITY_THRESHOLD}"
        );
    }

    let cross_t1 = tier1_similarity(&a1, &b1);
    let cross_t2 = sequence_similarity(&a2, &b2);
    eprintln!(
        "[distinctness] a_vs_b tier1={cross_t1:.3} (hd={}) tier2={cross_t2:.3}",
        hamming_distance(a1.global_phash, b1.global_phash)
    );
    assert!(
        cross_t1 < REENCODE_STABILITY_THRESHOLD,
        "unrelated sources should not reach the robustness floor on tier1 (got {cross_t1:.3})"
    );
}
