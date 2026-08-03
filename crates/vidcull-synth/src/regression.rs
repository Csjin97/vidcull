use std::path::Path;

use vidcull_core::Result;
use vidcull_core::types::Codec;

use crate::corpus::{ClipVariant, sub_seed};
use crate::plan::plan;
use crate::transform::{Container, Filter, Recipe};

pub fn plan_regression_corpus(
    source: &Path,
    source_duration_ms: u64,
    source_width: u32,
    source_height: u32,
    seed: u64,
    out_dir: &Path,
) -> Result<Vec<ClipVariant>> {
    let third = source_duration_ms / 3;

    let recipes: [(&'static str, Recipe); 9] = [
        ("exact_remux", Recipe::remux(source, Container::Mkv)),
        ("reencode_h265", Recipe::reencode(source, Codec::H265)),
        (
            "resized_half",
            Recipe::reencode(source, Codec::H264).with_filter(Filter::Resize {
                width: (source_width / 2).max(2),
                height: (source_height / 2).max(2),
            }),
        ),
        (
            "watermarked",
            Recipe::reencode(source, Codec::H264).with_filter(Filter::Watermark),
        ),
        (
            "bitrate_reduced",
            Recipe::reencode(source, Codec::H264).with_bitrate(500),
        ),
        (
            "fps_ntsc",
            Recipe::reencode(source, Codec::H264).with_filter(Filter::Fps { fps_x1000: 29_970 }),
        ),
        (
            "short_clip",
            Recipe::reencode(source, Codec::H264).with_clip(third, third),
        ),
        (
            "subtitled",
            Recipe::reencode(source, Codec::H264).with_subtitle("vidcull regression"),
        ),
        (
            "brightness_up",
            Recipe::reencode(source, Codec::H264)
                .with_filter(Filter::Brightness { delta_percent: 20 }),
        ),
    ];

    recipes
        .into_iter()
        .enumerate()
        .map(|(idx, (label, recipe))| {
            let variant_seed = sub_seed(seed, idx);
            Ok(ClipVariant {
                label,
                plan: plan(&recipe, variant_seed, out_dir)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "/src/long.mp4";
    const DURATION_MS: u64 = 30_000;
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    const SEED: u64 = 0xDEAD_BEEF;
    const OUT: &str = "/out";

    fn make() -> Vec<ClipVariant> {
        plan_regression_corpus(
            Path::new(SOURCE),
            DURATION_MS,
            WIDTH,
            HEIGHT,
            SEED,
            Path::new(OUT),
        )
        .expect("plan_regression_corpus")
    }

    #[test]
    fn determinism_same_seed_yields_identical_vec() {
        assert_eq!(make(), make());
    }

    #[test]
    fn all_nine_labels_present_in_order() {
        let labels: Vec<&str> = make().iter().map(|v| v.label).collect();
        assert_eq!(
            labels,
            [
                "exact_remux",
                "reencode_h265",
                "resized_half",
                "watermarked",
                "bitrate_reduced",
                "fps_ntsc",
                "short_clip",
                "subtitled",
                "brightness_up",
            ]
        );
    }

    #[test]
    fn each_label_present_exactly_once() {
        let variants = make();
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(v.label), "duplicate label: {}", v.label);
        }
        assert_eq!(seen.len(), 9);
    }

    #[test]
    fn distinct_sub_seeds_per_variant() {
        let sub_seeds: Vec<u64> = (0..9).map(|i| sub_seed(SEED, i)).collect();
        let unique: std::collections::HashSet<u64> = sub_seeds.iter().copied().collect();
        assert_eq!(unique.len(), 9, "sub-seeds must all differ: {sub_seeds:?}");
    }

    #[test]
    fn different_seeds_produce_distinct_plans() {
        let a = plan_regression_corpus(
            Path::new(SOURCE),
            DURATION_MS,
            WIDTH,
            HEIGHT,
            1,
            Path::new(OUT),
        )
        .expect("a");
        let b = plan_regression_corpus(
            Path::new(SOURCE),
            DURATION_MS,
            WIDTH,
            HEIGHT,
            2,
            Path::new(OUT),
        )
        .expect("b");
        assert_ne!(a, b);
    }
}
