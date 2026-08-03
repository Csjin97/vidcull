use std::path::{Path, PathBuf};
use vidcull_core::types::{Codec, VideoDuration};
use vidcull_fingerprint::tier1::REENCODE_STABILITY_THRESHOLD;
use vidcull_fingerprint::{GrayFrame, Tier1Fingerprint, build_tier1, hamming_distance};
use vidcull_parser::probe_and_decode_sparse;
use vidcull_synth::{Container, Encode, FfmpegBinaries, Filter, Recipe, render_recipe};

const BUDGET: usize = 12;
const TRIM_MS: u64 = 4_000;
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "m4v", "avi", "mpg", "mpeg", "ts",
];

fn binaries_or_skip(test: &str) -> Option<FfmpegBinaries> {
    match FfmpegBinaries::resolve() {
        Ok(bins) => Some(bins),
        Err(e) => {
            eprintln!("SKIP {test}: ffmpeg not resolvable ({e})");
            None
        }
    }
}

fn real_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/real")
}

fn real_videos() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(real_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_video = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|e| VIDEO_EXTS.contains(&e.as_str()));
        if is_video {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn short_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn try_tier1(bins: &FfmpegBinaries, path: &Path) -> Option<Tier1Fingerprint> {
    match probe_and_decode_sparse(bins, path, BUDGET) {
        Ok(decoded) => {
            let views: Vec<GrayFrame<'_>> = decoded
                .frames
                .iter()
                .map(|f| GrayFrame {
                    width: f.width,
                    height: f.height,
                    pixels: &f.pixels,
                })
                .collect();
            Some(build_tier1(
                decoded.metadata.duration.unwrap_or(VideoDuration::ZERO),
                decoded.metadata.codec.clone(),
                &[],
                &views,
            ))
        }
        Err(e) => {
            eprintln!("[real] SKIP {}: {e}", short_name(path));
            None
        }
    }
}

fn tier1_similarity(a: &Tier1Fingerprint, b: &Tier1Fingerprint) -> f64 {
    f64::from(64 - hamming_distance(a.global_phash, b.global_phash)) / 64.0
}

#[test]
#[ignore = "on-demand: depends on mutable user files in fixtures/real/; run with --ignored"]
#[allow(clippy::too_many_lines)]
fn real_videos_are_distinct_and_survive_reencode() {
    let Some(bins) = binaries_or_skip("real_videos_are_distinct_and_survive_reencode") else {
        return;
    };
    let videos = real_videos();
    if videos.is_empty() {
        eprintln!(
            "SKIP real_videos_are_distinct_and_survive_reencode: no files in {} \
             (drop real video there to enable this layer)",
            real_dir().display()
        );
        return;
    }
    eprintln!(
        "[real] {} candidate file(s) in {}",
        videos.len(),
        real_dir().display()
    );

    let decoded: Vec<(PathBuf, Tier1Fingerprint)> = videos
        .iter()
        .filter_map(|p| try_tier1(&bins, p).map(|fp| (p.clone(), fp)))
        .collect();

    if decoded.is_empty() {
        eprintln!(
            "SKIP real_videos_are_distinct_and_survive_reencode: none of the {} file(s) decoded",
            videos.len()
        );
        return;
    }
    eprintln!("[real] {} file(s) decoded", decoded.len());

    for i in 0..decoded.len() {
        for j in (i + 1)..decoded.len() {
            let sim = tier1_similarity(&decoded[i].1, &decoded[j].1);
            eprintln!(
                "[real][distinct] {} vs {} -> tier1={sim:.3}",
                short_name(&decoded[i].0),
                short_name(&decoded[j].0),
            );
            assert!(
                sim < REENCODE_STABILITY_THRESHOLD,
                "distinct real videos should be separable on tier1 (got {sim:.3} for {} vs {})",
                short_name(&decoded[i].0),
                short_name(&decoded[j].0)
            );
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    for (path, _) in &decoded {
        let Some(ref_fp) = render_then_tier1(
            &bins,
            dir,
            &Recipe::reencode(path, Codec::H264).with_clip(0, TRIM_MS),
            1,
        ) else {
            eprintln!(
                "[real] SKIP robustness for {}: reference render/decode failed",
                short_name(path)
            );
            continue;
        };

        let variants = [
            (
                "h265",
                Recipe::reencode(path, Codec::H265).with_clip(0, TRIM_MS),
            ),
            (
                "resized",
                Recipe {
                    source: path.clone(),
                    clip: Some(vidcull_synth::Clip {
                        start_ms: 0,
                        duration_ms: TRIM_MS,
                    }),
                    filters: vec![Filter::Resize {
                        width: 256,
                        height: 144,
                    }],
                    subtitle: None,
                    encode: Encode::Reencode {
                        codec: Codec::H264,
                        bitrate_kbps: None,
                    },
                    container: Container::Mp4,
                },
            ),
        ];
        for (label, recipe) in variants {
            let Some(v) = render_then_tier1(&bins, dir, &recipe, 2) else {
                eprintln!(
                    "[real] SKIP {label} for {}: render/decode failed",
                    short_name(path)
                );
                continue;
            };
            let sim = tier1_similarity(&ref_fp, &v);
            eprintln!(
                "[real][robust] {} {label} -> tier1={sim:.3}",
                short_name(path)
            );
            assert!(
                sim >= REENCODE_STABILITY_THRESHOLD,
                "real-content {label} robustness {sim:.3} below floor for {}",
                short_name(path)
            );
        }
    }
}

fn render_then_tier1(
    bins: &FfmpegBinaries,
    dir: &Path,
    recipe: &Recipe,
    seed: u64,
) -> Option<Tier1Fingerprint> {
    let path = render_recipe(bins, recipe, seed, dir).ok()?;
    try_tier1(bins, &path)
}
