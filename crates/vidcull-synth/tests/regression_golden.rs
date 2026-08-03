use std::path::Path;

use vidcull_synth::plan_regression_corpus;

const SOURCE: &str = "/src/long.mp4";
const DURATION_MS: u64 = 30_000;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const SEED: u64 = 0xDEAD_BEEF;
const OUT: &str = "/out";

fn make() -> Vec<vidcull_synth::ClipVariant> {
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

fn str_args(variant: &vidcull_synth::ClipVariant) -> Vec<String> {
    variant
        .plan
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn contains_seq(args: &[String], needle: &[&str]) -> bool {
    args.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn label_set_is_complete_and_ordered() {
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
        ],
        "label order must be stable"
    );
}

#[test]
fn exact_remux_golden() {
    let v = &make()[0];
    assert_eq!(v.label, "exact_remux");

    let args = str_args(v);
    assert!(contains_seq(&args, &["-c", "copy"]), "{args:?}");
    assert!(!args.iter().any(|a| a == "-c:v"), "{args:?}");
    assert!(contains_seq(&args, &["-fflags", "+bitexact"]), "{args:?}");
    assert!(contains_seq(&args, &["-map_metadata", "-1"]), "{args:?}");
    assert!(args.iter().any(|a| a == "-bitexact"), "{args:?}");

    let out = v.plan.output.to_string_lossy();
    assert!(out.ends_with(".mkv"), "expected .mkv output, got {out}");
    assert!(
        out.contains("4adfb90f68c9eb9b"),
        "seed hex must appear in output name: {out}"
    );

    assert_eq!(
        args,
        vec![
            "-v",
            "error",
            "-hide_banner",
            "-nostdin",
            "-y",
            "-fflags",
            "+bitexact",
            "-i",
            "/src/long.mp4",
            "-c",
            "copy",
            "-an",
            "-map_metadata",
            "-1",
            "-bitexact",
            &v.plan.output.to_string_lossy(),
        ],
        "exact_remux golden args mismatch"
    );
}

#[test]
fn resized_half_golden() {
    let v = &make()[2];
    assert_eq!(v.label, "resized_half");

    let args = str_args(v);
    let vf = args.iter().position(|a| a == "-vf").expect("-vf missing");
    assert_eq!(
        args[vf + 1],
        "scale=160:90",
        "resized_half must scale to 160:90 (half of 320×180)"
    );
    assert!(contains_seq(&args, &["-c:v", "libx264"]), "{args:?}");

    let out = v.plan.output.to_string_lossy();
    assert!(out.ends_with(".mp4"), "expected .mp4, got {out}");
    assert!(
        out.contains("021fbc2f8e1cfc1d"),
        "seed hex must appear in output name: {out}"
    );
}

#[test]
fn fps_ntsc_golden() {
    let v = &make()[5];
    assert_eq!(v.label, "fps_ntsc");

    let args = str_args(v);
    let vf = args.iter().position(|a| a == "-vf").expect("-vf missing");
    assert_eq!(
        args[vf + 1],
        "fps=29970/1000",
        "fps_ntsc must use fps=29970/1000"
    );
    assert!(
        args[vf + 1].contains("29970"),
        "fps filter must contain 29970: {}",
        args[vf + 1]
    );
    assert!(
        args[vf + 1].contains("1000"),
        "fps filter must contain 1000: {}",
        args[vf + 1]
    );

    let out = v.plan.output.to_string_lossy();
    assert!(
        out.contains("ab203e503cb55b3f"),
        "seed hex must appear in output name: {out}"
    );
}

#[test]
fn same_seed_yields_identical_corpus_twice() {
    let a = make();
    let b = make();
    assert_eq!(a.len(), b.len());
    for (va, vb) in a.iter().zip(b.iter()) {
        assert_eq!(va.label, vb.label);
        assert_eq!(
            va.plan.args, vb.plan.args,
            "label {} args must be byte-identical across calls",
            va.label
        );
        assert_eq!(va.plan.output, vb.plan.output);
    }
}

#[test]
fn different_seeds_produce_different_output_names() {
    let a = plan_regression_corpus(
        Path::new(SOURCE),
        DURATION_MS,
        WIDTH,
        HEIGHT,
        1,
        Path::new(OUT),
    )
    .expect("seed 1");
    let b = plan_regression_corpus(
        Path::new(SOURCE),
        DURATION_MS,
        WIDTH,
        HEIGHT,
        2,
        Path::new(OUT),
    )
    .expect("seed 2");
    let a_outs: Vec<_> = a.iter().map(|v| v.plan.output.clone()).collect();
    let b_outs: Vec<_> = b.iter().map(|v| v.plan.output.clone()).collect();
    assert_ne!(
        a_outs, b_outs,
        "different seeds must produce different output paths"
    );
}

#[test]
fn resized_variant_args_contain_scale_filter() {
    let v = make()
        .into_iter()
        .find(|v| v.label == "resized_half")
        .expect("resized_half");
    let args = str_args(&v);
    assert!(
        args.iter().any(|a| a.starts_with("scale=")),
        "resized_half must have a scale= vf clause: {args:?}"
    );
}

#[test]
fn fps_variant_args_contain_29970_over_1000() {
    let v = make()
        .into_iter()
        .find(|v| v.label == "fps_ntsc")
        .expect("fps_ntsc");
    let args = str_args(&v);
    assert!(
        args.iter().any(|a| *a == "fps=29970/1000"),
        "fps_ntsc must have fps=29970/1000 in vf: {args:?}"
    );
}
