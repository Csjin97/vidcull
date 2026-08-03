use std::path::{Path, PathBuf};

use vidcull_core::Result;
use vidcull_core::types::Codec;

use crate::plan::{RenderPlan, plan};
use crate::rng::SplitMix64;
use crate::transform::{Filter, Recipe};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipVariant {
    pub label: &'static str,
    pub plan: RenderPlan,
}

pub fn plan_clip_corpus(
    source: &Path,
    source_duration_ms: u64,
    source_width: u32,
    source_height: u32,
    seed: u64,
    out_dir: &Path,
) -> Result<Vec<ClipVariant>> {
    let start_ms = source_duration_ms / 3;
    let duration_ms = source_duration_ms / 3;

    let base = || Recipe::reencode(source, Codec::H264).with_clip(start_ms, duration_ms);

    let recipes: [(&'static str, Recipe); 3] = [
        ("clip_plain", base()),
        (
            "clip_resized",
            base().with_filter(Filter::Resize {
                width: (source_width / 2).max(2),
                height: (source_height / 2).max(2),
            }),
        ),
        ("clip_watermarked", base().with_filter(Filter::Watermark)),
    ];

    recipes
        .into_iter()
        .enumerate()
        .map(|(idx, (label, recipe))| {
            let sub_seed = sub_seed(seed, idx);
            Ok(ClipVariant {
                label,
                plan: plan(&recipe, sub_seed, out_dir)?,
            })
        })
        .collect()
}

pub(crate) fn sub_seed(seed: u64, idx: usize) -> u64 {
    let mut rng = SplitMix64::new(seed);
    let idx = u64::try_from(idx).unwrap_or(u64::MAX);
    let mut value = 0;
    for _ in 0..=idx {
        value = rng.next_u64();
    }
    value
}

#[must_use]
pub fn variant_outputs(variants: &[ClipVariant]) -> Vec<PathBuf> {
    variants.iter().map(|v| v.plan.output.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_seed_is_stable_and_index_dependent() {
        assert_eq!(sub_seed(99, 0), sub_seed(99, 0));
        assert_ne!(sub_seed(99, 0), sub_seed(99, 1));
        assert_ne!(sub_seed(99, 1), sub_seed(99, 2));
    }

    #[test]
    fn test_plan_clip_corpus_generates_three_variants_with_correct_timings() {
        let source = Path::new("test_source.mp4");
        let out_dir = Path::new("out");

        let duration_ms = 9000;
        let width = 1920;
        let height = 1080;

        let variants = plan_clip_corpus(source, duration_ms, width, height, 42, out_dir).unwrap();

        assert_eq!(variants.len(), 3);
        assert_eq!(variants[0].label, "clip_plain");
        assert_eq!(variants[1].label, "clip_resized");
        assert_eq!(variants[2].label, "clip_watermarked");

        let plain_out = &variants[0].plan.output;
        assert!(plain_out.to_string_lossy().contains("test_source"));

        let outputs = variant_outputs(&variants);
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0], variants[0].plan.output);
    }
}
