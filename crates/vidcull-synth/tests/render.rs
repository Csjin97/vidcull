use std::path::Path;
use std::process::Command;

use vidcull_core::types::Codec;
use vidcull_synth::{
    FfmpegBinaries, Filter, Recipe, plan_clip_corpus, render, render_recipe, render_testsrc,
};

fn binaries_or_skip(test: &str) -> Option<FfmpegBinaries> {
    match FfmpegBinaries::resolve() {
        Ok(bins) => Some(bins),
        Err(e) => {
            eprintln!(
                "SKIP {test}: ffmpeg not resolvable ({e}); set VIDCULL_FFMPEG_DIR or install on PATH"
            );
            None
        }
    }
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read rendered output")
}

fn probe_duration_secs(bins: &FfmpegBinaries, path: &Path) -> Option<f64> {
    let out = Command::new(bins.ffprobe())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[test]
fn same_seed_renders_identical_bytes() {
    let Some(bins) = binaries_or_skip("same_seed_renders_identical_bytes") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let src = render_testsrc(&bins, dir.path(), 1, 2000, 320, 180).expect("testsrc");

    let recipe = Recipe::reencode(&src, Codec::H264)
        .with_clip(500, 1000)
        .with_filter(Filter::Watermark)
        .with_filter(Filter::Brightness { delta_percent: 15 });

    let out_a = dir.path().join("a");
    let out_b = dir.path().join("b");
    std::fs::create_dir_all(&out_a).expect("mkdir a");
    std::fs::create_dir_all(&out_b).expect("mkdir b");

    let a = render_recipe(&bins, &recipe, 4242, &out_a).expect("render a");
    let b = render_recipe(&bins, &recipe, 4242, &out_b).expect("render b");

    assert_eq!(
        bytes(&a),
        bytes(&b),
        "identical seed + recipe must produce byte-identical output on a fixed ffmpeg build"
    );
    assert!(!bytes(&a).is_empty(), "render produced an empty file");
}

#[test]
fn testsrc_source_is_reproducible() {
    let Some(bins) = binaries_or_skip("testsrc_source_is_reproducible") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let dir_a = dir.path().join("a");
    let dir_b = dir.path().join("b");
    std::fs::create_dir_all(&dir_a).expect("mkdir a");
    std::fs::create_dir_all(&dir_b).expect("mkdir b");

    let a = render_testsrc(&bins, &dir_a, 7, 1000, 320, 180).expect("testsrc a");
    let b = render_testsrc(&bins, &dir_b, 7, 1000, 320, 180).expect("testsrc b");
    assert_eq!(
        bytes(&a),
        bytes(&b),
        "same testsrc parameters must produce byte-identical sources"
    );
}

#[test]
fn rendered_clip_is_shorter_than_source() {
    let Some(bins) = binaries_or_skip("rendered_clip_is_shorter_than_source") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let src = render_testsrc(&bins, dir.path(), 3, 3000, 320, 180).expect("testsrc");
    let src_dur = probe_duration_secs(&bins, &src).expect("source duration");

    let recipe = Recipe::reencode(&src, Codec::H264).with_clip(500, 1000);
    let clip = render_recipe(&bins, &recipe, 1, dir.path()).expect("render clip");
    let clip_dur = probe_duration_secs(&bins, &clip).expect("clip duration");

    assert!(
        clip_dur < src_dur,
        "clip ({clip_dur}s) must be shorter than source ({src_dur}s)"
    );
    assert!(clip_dur <= 1.8, "clip unexpectedly long: {clip_dur}s");
}

#[test]
fn clip_corpus_renders_all_three_variants() {
    let Some(bins) = binaries_or_skip("clip_corpus_renders_all_three_variants") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let src = render_testsrc(&bins, dir.path(), 5, 9000, 320, 180).expect("testsrc");

    let variants = plan_clip_corpus(&src, 9000, 320, 180, 42, dir.path()).expect("corpus");
    assert_eq!(variants.len(), 3);
    for variant in &variants {
        let out = render(&bins, &variant.plan).expect("render variant");
        assert!(out.exists(), "{} not written", out.display());
        assert!(!bytes(&out).is_empty(), "{} empty", out.display());
    }
}
