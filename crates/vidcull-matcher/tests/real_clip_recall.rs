use std::path::Path;
use vidcull_core::types::{Codec, FileId};
use vidcull_fingerprint::{GrayFrame, Tier2Fingerprint, TimedFrame, build_tier2};
use vidcull_matcher::partial::{AnchorIndex, AnchorParams};
use vidcull_parser::probe_and_decode_sparse;
use vidcull_synth::{FfmpegBinaries, Recipe, render_recipe, render_source};

const PATTERNS: &[(&str, &str)] = &[
    ("testsrc", "testsrc"),
    ("testsrc2", "testsrc2"),
    ("mandelbrot", "mandelbrot"),
    ("life", "life"),
];

const SOURCE_MS: u64 = 60_000;
const CLIP_START_MS: u64 = 20_000;
const CLIP_MS: u64 = 15_000;
const FPS: u32 = 30;
const GOP: u32 = 30;
const SCENES_PER_SEC: u64 = 4;

fn binaries_or_skip(test: &str) -> Option<FfmpegBinaries> {
    match FfmpegBinaries::resolve() {
        Ok(bins) => Some(bins),
        Err(e) => {
            eprintln!("SKIP {test}: ffmpeg not resolvable ({e})");
            None
        }
    }
}

fn decode_tier2(bins: &FfmpegBinaries, path: &Path, duration_ms: u64) -> Tier2Fingerprint {
    let budget = usize::try_from((duration_ms * SCENES_PER_SEC) / 1000)
        .unwrap_or(0)
        .max(1);
    let decoded = probe_and_decode_sparse(bins, path, budget)
        .unwrap_or_else(|e| panic!("decode {} failed: {e}", path.display()));
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
    build_tier2(&timed)
}

#[test]
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn real_clips_match_their_source_with_zero_false_positives() {
    let Some(bins) = binaries_or_skip("real_clips_match_their_source") else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
    let mut clips: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
    for (i, (name, pattern)) in PATTERNS.iter().enumerate() {
        let source_id = FileId(i64::try_from(i).expect("corpus index fits i64"));
        let src = render_source(&bins, dir, name, pattern, SOURCE_MS, 320, 180, FPS, GOP)
            .unwrap_or_else(|e| panic!("render source {name}: {e}"));
        let source_fp = decode_tier2(&bins, &src, SOURCE_MS);
        corpus.push((source_id, source_fp));

        let clip_recipe = Recipe::reencode(&src, Codec::H264).with_clip(CLIP_START_MS, CLIP_MS);
        let clip_path = render_recipe(&bins, &clip_recipe, 7, dir)
            .unwrap_or_else(|e| panic!("render clip {name}: {e}"));
        let clip_fp = decode_tier2(&bins, &clip_path, CLIP_MS);
        eprintln!(
            "[recall] {name:<11} source_scenes={} clip_scenes={}",
            corpus[i].1.scenes.len(),
            clip_fp.scenes.len()
        );
        clips.push((source_id, clip_fp));
    }

    let index = AnchorIndex::build(corpus.clone(), AnchorParams::default());

    let mut hits = 0usize;
    let mut false_positives = 0usize;
    for (true_source, clip_fp) in &clips {
        let alignments = index.search(&clip_fp.scenes, Some(*true_source));
        let full = index.search(&clip_fp.scenes, None);
        let matched_own = full.iter().any(|a| a.source == *true_source);
        if matched_own {
            hits += 1;
        }
        for a in &full {
            if a.source != *true_source {
                false_positives += 1;
                eprintln!(
                    "[recall] FALSE POSITIVE clip of {:?} matched source {:?} (cov={})",
                    true_source, a.source, a.coverage_x1000
                );
            }
        }
        let best_cov = full
            .iter()
            .find(|a| a.source == *true_source)
            .map_or(0, |a| a.coverage_x1000);
        eprintln!(
            "[recall] clip of {true_source:?}: own_match={matched_own} best_cov={best_cov} \
             cross_excluded_hits={}",
            alignments.len()
        );
    }

    let recall = hits as f64 / clips.len() as f64;
    eprintln!(
        "[recall] RECALL = {recall:.3} ({hits}/{}), false_positives = {false_positives}",
        clips.len()
    );
    assert!(
        recall >= 0.95,
        "real partial-clip recall {recall:.3} below the 0.95 floor"
    );
    assert_eq!(false_positives, 0, "clips must not match unrelated sources");
}
