use std::ffi::OsString;
use std::path::{Path, PathBuf};

use vidcull_core::{Error, Result};

use crate::rng::SplitMix64;
use crate::transform::{Encode, Filter, Recipe};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarSrt {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderPlan {
    pub args: Vec<OsString>,
    pub output: PathBuf,
    pub sidecar_srt: Option<SidecarSrt>,
}

const WATERMARK_SALT: u64 = 0x7761_7465_726D_6B21;

const WATERMARK_W: u32 = 48;
const WATERMARK_H: u32 = 16;

pub fn plan(recipe: &Recipe, seed: u64, out_dir: &Path) -> Result<RenderPlan> {
    validate(recipe)?;

    let output = out_dir.join(output_file_name(recipe, seed));
    let sidecar_srt = recipe.subtitle.as_ref().map(|text| SidecarSrt {
        path: out_dir.join(format!("{}.{seed:016x}.srt", recipe.source_stem())),
        content: srt_cue(text),
    });

    let mut args: Vec<OsString> = Vec::new();
    push_all(
        &mut args,
        &[
            "-v",
            "error",
            "-hide_banner",
            "-nostdin",
            "-y",
            "-fflags",
            "+bitexact",
        ],
    );

    if let Some(clip) = &recipe.clip {
        push_all(&mut args, &["-ss"]);
        args.push(secs(clip.start_ms).into());
    }
    push_all(&mut args, &["-i"]);
    args.push(recipe.source.clone().into_os_string());

    if let Some(sidecar) = &sidecar_srt {
        push_all(&mut args, &["-i"]);
        args.push(sidecar.path.clone().into_os_string());
    }

    if let Some(clip) = &recipe.clip {
        push_all(&mut args, &["-t"]);
        args.push(secs(clip.duration_ms).into());
    }

    if sidecar_srt.is_some() {
        push_all(&mut args, &["-map", "0:v:0", "-map", "1:0"]);
    }

    if !recipe.filters.is_empty() {
        push_all(&mut args, &["-vf"]);
        args.push(filtergraph(&recipe.filters, seed).into());
    }

    push_codec_args(&mut args, &recipe.encode)?;

    if sidecar_srt.is_some() {
        push_all(&mut args, &["-c:s"]);
        push_all(&mut args, &[recipe.container.subtitle_codec()]);
    }

    push_all(&mut args, &["-an", "-map_metadata", "-1", "-bitexact"]);

    args.push(output.clone().into_os_string());

    Ok(RenderPlan {
        args,
        output,
        sidecar_srt,
    })
}

fn validate(recipe: &Recipe) -> Result<()> {
    if matches!(recipe.encode, Encode::Copy) && !recipe.filters.is_empty() {
        return Err(Error::Unsupported(
            "stream-copy (remux) cannot apply video filters; re-encode instead".into(),
        ));
    }
    if let Encode::Reencode { codec, .. } = &recipe.encode {
        if Encode::video_encoder(codec).is_none() {
            return Err(Error::Unsupported(format!(
                "no synthetic encoder wired for codec {}",
                codec.short_name()
            )));
        }
    }
    Ok(())
}

fn push_codec_args(args: &mut Vec<OsString>, encode: &Encode) -> Result<()> {
    match encode {
        Encode::Copy => push_all(args, &["-c", "copy"]),
        Encode::Reencode {
            codec,
            bitrate_kbps,
        } => {
            let enc = Encode::video_encoder(codec).ok_or_else(|| {
                Error::Unsupported(format!("no encoder for codec {}", codec.short_name()))
            })?;
            push_all(args, &["-c:v", enc]);
            if enc == "libx264" || enc == "libx265" {
                push_all(args, &["-preset", "ultrafast"]);
                push_all(
                    args,
                    &["-g", "30", "-keyint_min", "30", "-sc_threshold", "0"],
                );
            }
            push_all(args, &["-pix_fmt", "yuv420p"]);
            if let Some(kbps) = bitrate_kbps {
                push_all(args, &["-b:v"]);
                args.push(format!("{kbps}k").into());
            }
        }
    }
    Ok(())
}

fn filtergraph(filters: &[Filter], seed: u64) -> String {
    filters
        .iter()
        .map(|f| filter_clause(f, seed))
        .collect::<Vec<_>>()
        .join(",")
}

fn filter_clause(filter: &Filter, seed: u64) -> String {
    match filter {
        Filter::Resize { width, height } => format!("scale={width}:{height}"),
        Filter::Watermark => {
            let (x, y) = watermark_pos(seed);
            format!("drawbox=x={x}:y={y}:w={WATERMARK_W}:h={WATERMARK_H}:color=white@0.6:t=fill")
        }
        Filter::Fps { fps_x1000 } => format!("fps={fps_x1000}/1000"),
        Filter::Brightness { delta_percent } => {
            let b = f64::from(*delta_percent) / 100.0;
            format!("eq=brightness={b:.2}")
        }
    }
}

fn watermark_pos(seed: u64) -> (u32, u32) {
    let mut rng = SplitMix64::new(seed ^ WATERMARK_SALT);
    let x = rng.below(256);
    let y = rng.below(128);
    (x, y)
}

fn output_file_name(recipe: &Recipe, seed: u64) -> String {
    format!(
        "{}.{}.{seed:016x}.{}",
        recipe.source_stem(),
        recipe_tag(recipe),
        recipe.container.extension()
    )
}

fn recipe_tag(recipe: &Recipe) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(clip) = &recipe.clip {
        parts.push(format!("clip{}-{}", clip.start_ms, clip.duration_ms));
    }
    for filter in &recipe.filters {
        parts.push(match filter {
            Filter::Resize { width, height } => format!("scale{width}x{height}"),
            Filter::Watermark => "wm".to_owned(),
            Filter::Fps { fps_x1000 } => format!("fps{fps_x1000}"),
            Filter::Brightness { delta_percent } => format!("bri{delta_percent}"),
        });
    }
    if recipe.subtitle.is_some() {
        parts.push("sub".to_owned());
    }
    match &recipe.encode {
        Encode::Copy => parts.push("remux".to_owned()),
        Encode::Reencode {
            codec,
            bitrate_kbps,
        } => {
            parts.push(codec.short_name().to_owned());
            if let Some(kbps) = bitrate_kbps {
                parts.push(format!("{kbps}k"));
            }
        }
    }
    if parts.is_empty() {
        "passthrough".to_owned()
    } else {
        parts.join("_")
    }
}

fn srt_cue(text: &str) -> String {
    format!("1\n00:00:00,000 --> 00:00:01,000\n{text}\n")
}

fn secs(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

fn push_all(args: &mut Vec<OsString>, flags: &[&str]) {
    for flag in flags {
        args.push((*flag).into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::Container;
    use vidcull_core::types::Codec;

    #[test]
    fn watermark_position_is_deterministic_per_seed() {
        assert_eq!(watermark_pos(123), watermark_pos(123));
        assert_ne!(watermark_pos(1), watermark_pos(2));
    }

    #[test]
    fn watermark_stays_within_320x180_frame() {
        for seed in 0..2000 {
            let (x, y) = watermark_pos(seed);
            assert!(x + WATERMARK_W <= 320, "x={x}");
            assert!(y + WATERMARK_H <= 180, "y={y}");
        }
    }

    #[test]
    fn secs_pads_milliseconds() {
        assert_eq!(secs(0), "0.000");
        assert_eq!(secs(1500), "1.500");
        assert_eq!(secs(60_007), "60.007");
    }

    #[test]
    fn srt_cue_is_one_second_subrip() {
        assert_eq!(srt_cue("hi"), "1\n00:00:00,000 --> 00:00:01,000\nhi\n");
    }

    #[test]
    fn brightness_clause_scales_percent() {
        assert_eq!(
            filter_clause(&Filter::Brightness { delta_percent: 20 }, 0),
            "eq=brightness=0.20"
        );
        assert_eq!(
            filter_clause(&Filter::Brightness { delta_percent: -50 }, 0),
            "eq=brightness=-0.50"
        );
    }

    #[test]
    fn fps_clause_keeps_fractional_rate_exact() {
        assert_eq!(
            filter_clause(&Filter::Fps { fps_x1000: 29_970 }, 0),
            "fps=29970/1000"
        );
    }

    #[test]
    fn plan_reencode_h264_produces_expected_args() {
        let recipe = Recipe::reencode("source.mp4", Codec::H264);
        let out_dir = Path::new("out");
        let plan_res = plan(&recipe, 42, out_dir).unwrap();

        assert_eq!(
            plan_res.output,
            out_dir.join("source.h264.000000000000002a.mp4")
        );
        assert!(plan_res.args.contains(&OsString::from("libx264")));
        assert!(plan_res.sidecar_srt.is_none());
    }

    #[test]
    fn plan_remux_with_filters_errors() {
        let mut recipe = Recipe::remux("source.mp4", Container::Mp4);
        recipe.filters.push(Filter::Watermark);
        let out_dir = Path::new("out");
        let plan_res = plan(&recipe, 42, out_dir);

        assert!(plan_res.is_err());
    }

    #[test]
    fn plan_codec_other_errors() {
        let recipe = Recipe::reencode("source.mp4", Codec::Other("custom".to_owned()));
        let out_dir = Path::new("out");
        let plan_res = plan(&recipe, 42, out_dir);

        assert!(plan_res.is_err());
    }

    #[test]
    fn plan_with_clip_adds_ss_and_t_args() {
        let recipe = Recipe::reencode("source.mp4", Codec::H264).with_clip(1500, 5000);
        let out_dir = Path::new("out");
        let plan_res = plan(&recipe, 42, out_dir).unwrap();

        let args_str: Vec<String> = plan_res
            .args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args_str.contains(&"-ss".to_owned()));
        assert!(args_str.contains(&"1.500".to_owned()));
        assert!(args_str.contains(&"-t".to_owned()));
        assert!(args_str.contains(&"5.000".to_owned()));
    }

    #[test]
    fn plan_with_subtitle_creates_sidecar() {
        let mut recipe = Recipe::reencode("source.mp4", Codec::H264);
        recipe.subtitle = Some("Hello World".to_owned());
        let out_dir = Path::new("out");
        let plan_res = plan(&recipe, 42, out_dir).unwrap();

        assert!(plan_res.sidecar_srt.is_some());
        let sidecar = plan_res.sidecar_srt.unwrap();
        assert!(sidecar.content.contains("Hello World"));
        assert!(
            sidecar
                .path
                .to_string_lossy()
                .contains("source.000000000000002a.srt")
        );
    }
}
