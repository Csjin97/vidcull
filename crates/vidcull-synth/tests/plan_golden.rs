use std::path::Path;

use vidcull_core::types::Codec;
use vidcull_synth::{Container, Filter, Recipe, plan, plan_clip_corpus, variant_outputs};

fn args_of(recipe: &Recipe, seed: u64) -> Vec<String> {
    let p = plan(recipe, seed, Path::new("/out")).expect("plan");
    p.args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn contains_seq(args: &[String], needle: &[&str]) -> bool {
    args.windows(needle.len()).any(|w| w == needle)
}

fn out_ext(recipe: &Recipe, seed: u64) -> String {
    plan(recipe, seed, Path::new("/out"))
        .expect("plan")
        .output
        .extension()
        .and_then(|e| e.to_str())
        .expect("extension")
        .to_owned()
}

#[test]
fn transcode_to_av1_uses_libaom_and_mp4_container() {
    let r = Recipe::reencode("/src/a.mp4", Codec::Av1);
    let args = args_of(&r, 0);
    assert!(contains_seq(&args, &["-c:v", "libaom-av1"]), "{args:?}");
    assert!(!args.contains(&"-preset".to_string()));
    assert_eq!(out_ext(&r, 0), "mp4");
}

#[test]
fn transcode_to_h264_adds_ultrafast_preset() {
    let args = args_of(&Recipe::reencode("/src/a.mp4", Codec::H264), 0);
    assert!(contains_seq(&args, &["-c:v", "libx264"]), "{args:?}");
    assert!(contains_seq(&args, &["-preset", "ultrafast"]), "{args:?}");
}

#[test]
fn resize_builds_scale_filter() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264).with_filter(Filter::Resize {
        width: 160,
        height: 90,
    });
    let args = args_of(&r, 0);
    let vf = args.iter().position(|a| a == "-vf").expect("-vf");
    assert_eq!(args[vf + 1], "scale=160:90");
}

#[test]
fn clip_places_ss_before_input_and_t_after() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264).with_clip(1500, 2000);
    let args = args_of(&r, 0);
    let ss = args.iter().position(|a| a == "-ss").expect("-ss");
    let i = args.iter().position(|a| a == "-i").expect("-i");
    let t = args.iter().position(|a| a == "-t").expect("-t");
    assert!(ss < i, "-ss must precede -i: {args:?}");
    assert!(i < t, "-t must follow -i: {args:?}");
    assert_eq!(args[ss + 1], "1.500");
    assert_eq!(args[t + 1], "2.000");
}

#[test]
fn watermark_clause_is_deterministic_for_a_seed() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264).with_filter(Filter::Watermark);
    let a1 = args_of(&r, 12345);
    let a2 = args_of(&r, 12345);
    assert_eq!(a1, a2, "same seed must yield identical args");
    let vf = a1.iter().position(|a| a == "-vf").expect("-vf");
    assert!(a1[vf + 1].starts_with("drawbox=x="), "{:?}", a1[vf + 1]);
}

#[test]
fn watermark_position_changes_with_seed() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264).with_filter(Filter::Watermark);
    let a1 = args_of(&r, 1);
    let a2 = args_of(&r, 2);
    assert_ne!(a1, a2, "different seeds should move the watermark");
}

#[test]
fn bitrate_sets_b_v() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264).with_bitrate(750);
    let args = args_of(&r, 0);
    let b = args.iter().position(|a| a == "-b:v").expect("-b:v");
    assert_eq!(args[b + 1], "750k");
}

#[test]
fn fps_builds_exact_fractional_filter() {
    let r =
        Recipe::reencode("/src/a.mp4", Codec::H264).with_filter(Filter::Fps { fps_x1000: 29_970 });
    let args = args_of(&r, 0);
    let vf = args.iter().position(|a| a == "-vf").expect("-vf");
    assert_eq!(args[vf + 1], "fps=29970/1000");
}

#[test]
fn remux_uses_copy_and_changes_container() {
    let r = Recipe::remux("/src/a.mp4", Container::Mkv);
    let args = args_of(&r, 0);
    assert!(contains_seq(&args, &["-c", "copy"]), "{args:?}");
    assert_eq!(out_ext(&r, 0), "mkv");
    assert!(!args.iter().any(|a| a == "-c:v"), "{args:?}");
}

#[test]
fn subtitle_softmux_adds_second_input_codec_and_sidecar() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264).with_subtitle("hello");
    let p = plan(&r, 0, Path::new("/out")).expect("plan");
    let args: Vec<String> = p
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args.iter().filter(|a| *a == "-i").count(), 2, "{args:?}");
    assert!(contains_seq(&args, &["-c:s", "mov_text"]), "{args:?}");
    let sidecar = p.sidecar_srt.expect("sidecar");
    assert_eq!(sidecar.content, "1\n00:00:00,000 --> 00:00:01,000\nhello\n");
    assert!(sidecar.path.extension().is_some_and(|e| e == "srt"));
}

#[test]
fn brightness_builds_eq_filter() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264)
        .with_filter(Filter::Brightness { delta_percent: 25 });
    let args = args_of(&r, 0);
    let vf = args.iter().position(|a| a == "-vf").expect("-vf");
    assert_eq!(args[vf + 1], "eq=brightness=0.25");
}

#[test]
fn every_plan_carries_bitexact_determinism_flags() {
    let args = args_of(&Recipe::reencode("/src/a.mp4", Codec::H264), 0);
    assert!(contains_seq(&args, &["-fflags", "+bitexact"]), "{args:?}");
    assert!(contains_seq(&args, &["-map_metadata", "-1"]), "{args:?}");
    assert!(args.iter().any(|a| a == "-bitexact"), "{args:?}");
}

#[test]
fn copy_with_video_filter_is_unsupported() {
    let r = Recipe::remux("/src/a.mp4", Container::Mkv).with_filter(Filter::Resize {
        width: 1,
        height: 1,
    });
    let err = plan(&r, 0, Path::new("/out")).expect_err("copy + filter must error");
    assert!(
        matches!(err, vidcull_core::Error::Unsupported(_)),
        "{err:?}"
    );
}

#[test]
fn reencode_to_unknown_codec_is_unsupported() {
    let r = Recipe::reencode("/src/a.mp4", Codec::Other("prores".into()));
    let err = plan(&r, 0, Path::new("/out")).expect_err("Other codec must error");
    assert!(
        matches!(err, vidcull_core::Error::Unsupported(_)),
        "{err:?}"
    );
}

#[test]
fn same_seed_and_recipe_yields_byte_identical_plan() {
    let r = Recipe::reencode("/src/a.mp4", Codec::H264)
        .with_clip(1000, 2000)
        .with_filter(Filter::Watermark)
        .with_filter(Filter::Brightness { delta_percent: 10 });
    let a = plan(&r, 777, Path::new("/out")).expect("plan a");
    let b = plan(&r, 777, Path::new("/out")).expect("plan b");
    assert_eq!(a, b);
}

#[test]
fn output_names_are_distinct_per_variant_and_seeded() {
    let base = Recipe::reencode("/src/clip.mp4", Codec::H264);
    let resized = base.clone().with_filter(Filter::Resize {
        width: 160,
        height: 90,
    });
    let p_base = plan(&base, 5, Path::new("/out")).expect("plan");
    let p_resized = plan(&resized, 5, Path::new("/out")).expect("plan");
    assert_ne!(p_base.output, p_resized.output);
    let p_other_seed = plan(&base, 6, Path::new("/out")).expect("plan");
    assert_ne!(p_base.output, p_other_seed.output);
}

#[test]
fn clip_corpus_is_deterministic_and_three_distinct_variants() {
    let make = || {
        plan_clip_corpus(
            Path::new("/src/long.mp4"),
            9000,
            320,
            180,
            42,
            Path::new("/out"),
        )
        .expect("corpus")
    };
    let a = make();
    let b = make();
    assert_eq!(a.len(), 3);
    assert_eq!(a, b, "corpus planning must be reproducible");

    let labels: Vec<&str> = a.iter().map(|v| v.label).collect();
    assert_eq!(labels, ["clip_plain", "clip_resized", "clip_watermarked"]);

    let outs = variant_outputs(&a);
    assert_ne!(outs[0], outs[1]);
    assert_ne!(outs[1], outs[2]);
    assert_ne!(outs[0], outs[2]);

    let plain_args: Vec<String> = a[0]
        .plan
        .args
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let ss = plain_args.iter().position(|s| s == "-ss").expect("-ss");
    assert_eq!(plain_args[ss + 1], "3.000");
}
