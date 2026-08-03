use std::path::Path;
use std::process::Command;

use vidcull_core::types::{Codec, Resolution, VideoDuration};
use vidcull_core::{Error, Result};

use super::timeout::{PROBE_TIMEOUT_SECS, effective_timeout, run_with_timeout_cancellable};
use serde_json::Value;

use super::binary::FfmpegBinaries;
use crate::cancel::Cancel;
use crate::probe::{ContainerKind, VideoMetadata, container_kind_from_path, fps_to_x1000};

const MAX_PROBE_DIMENSION: u32 = 16_384;

pub fn probe_fallback(bins: &FfmpegBinaries, path: &Path) -> Result<VideoMetadata> {
    probe_fallback_cancellable(bins, path, Cancel::default())
}

pub fn probe_fallback_cancellable(
    bins: &FfmpegBinaries,
    path: &Path,
    cancel: Cancel<'_>,
) -> Result<VideoMetadata> {
    let output = run_with_timeout_cancellable(
        Command::new(bins.ffprobe())
            .args(FFPROBE_ARGS)
            .arg("--")
            .arg(path),
        effective_timeout(PROBE_TIMEOUT_SECS),
        cancel,
        "probe",
    )?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let stderr_scrubbed = super::decode::scrub_input_path(&stderr_raw, path);
        return Err(Error::Decode(format!(
            "ffprobe failed ({}): {}",
            output.status,
            stderr_scrubbed.trim()
        )));
    }
    let json = String::from_utf8_lossy(&output.stdout);
    parse_ffprobe_metadata(&json, container_kind_from_path(path))
}

const FFPROBE_ARGS: [&str; 7] = [
    "-v",
    "error",
    "-hide_banner",
    "-show_format",
    "-show_streams",
    "-of",
    "json",
];

fn parse_ffprobe_metadata(json: &str, container: ContainerKind) -> Result<VideoMetadata> {
    let root: Value =
        serde_json::from_str(json).map_err(|e| Error::Parse(format!("ffprobe json: {e}")))?;

    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Parse("ffprobe json: missing `streams` array".into()))?;
    let video = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .ok_or_else(|| Error::Parse("ffprobe json: no video stream".into()))?;

    let codec_name = video
        .get("codec_name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Parse("ffprobe json: stream has no `codec_name`".into()))?;
    let codec = codec_from_name(codec_name);

    let width = u32_field(video, "width")?;
    let height = u32_field(video, "height")?;
    if width > MAX_PROBE_DIMENSION || height > MAX_PROBE_DIMENSION {
        return Err(Error::Parse(format!(
            "ffprobe json: video dimensions {width}x{height} exceed the \
             {MAX_PROBE_DIMENSION}px sanity cap"
        )));
    }
    let resolution = Resolution::new(width, height);

    let format = root.get("format");
    let duration = as_secs(video.get("duration"))
        .or_else(|| as_secs(format.and_then(|f| f.get("duration"))))
        .map(VideoDuration::from_secs_f64);
    let fps_x1000 = video
        .get("avg_frame_rate")
        .and_then(Value::as_str)
        .and_then(rational_to_x1000);
    let has_b_frames = as_u64(video.get("has_b_frames")).map(|n| n > 0);
    let bitrate_bps = as_u64(format.and_then(|f| f.get("bit_rate")));

    let mut tags = Vec::new();
    if let Some(t) = video.get("tags").and_then(Value::as_object) {
        for (k, v) in t {
            if let Some(val) = v.as_str() {
                tags.push(format!("{}:{}", k.to_lowercase(), val.to_lowercase()));
            }
        }
    }
    if let Some(f) = format {
        if let Some(t) = f.get("tags").and_then(Value::as_object) {
            for (k, v) in t {
                if let Some(val) = v.as_str() {
                    tags.push(format!("{}:{}", k.to_lowercase(), val.to_lowercase()));
                }
            }
        }
    }
    let encoder_tags = if tags.is_empty() {
        None
    } else {
        Some(tags.join(";"))
    };

    Ok(VideoMetadata {
        container,
        codec,
        resolution,
        duration,
        fps_x1000,
        has_b_frames,
        bitrate_bps,
        encoder_tags,
    })
}

fn codec_from_name(name: &str) -> Codec {
    match name {
        "h264" => Codec::H264,
        "hevc" | "h265" => Codec::H265,
        "av1" => Codec::Av1,
        "vp9" => Codec::Vp9,
        "mpeg2video" => Codec::Mpeg2,
        other => Codec::Other(other.to_owned()),
    }
}

fn u32_field(stream: &Value, key: &str) -> Result<u32> {
    let raw = stream
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Parse(format!("ffprobe json: stream missing `{key}`")))?;
    u32::try_from(raw)
        .map_err(|_| Error::Parse(format!("ffprobe json: `{key}` out of range: {raw}")))
}

fn rational_to_x1000(rational: &str) -> Option<u32> {
    let (num, den) = rational.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 {
        return None;
    }
    fps_to_x1000(num / den)
}

fn as_secs(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(s) = value.as_str() {
        s.trim().parse().ok()
    } else {
        value.as_f64()
    }
}

fn as_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(s) = value.as_str() {
        s.trim().parse().ok()
    } else {
        value.as_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AV1_JSON: &str = r#"{
      "streams": [{
        "index": 0, "codec_name": "av1", "codec_type": "video",
        "width": 320, "height": 180, "avg_frame_rate": "30/1",
        "duration": "1.000000", "bit_rate": "27296", "nb_frames": "30"
      }],
      "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
                  "duration": "1.000000", "bit_rate": "34984" }
    }"#;

    const VP9_JSON: &str = r#"{
      "streams": [{
        "index": 0, "codec_name": "vp9", "codec_type": "video",
        "width": 320, "height": 180, "avg_frame_rate": "30/1",
        "time_base": "1/1000"
      }],
      "format": { "format_name": "matroska,webm",
                  "duration": "1.000000", "bit_rate": "161336" }
    }"#;

    const MPEG2_JSON: &str = r#"{
      "streams": [{
        "index": 0, "codec_name": "mpeg2video", "codec_type": "video",
        "width": 320, "height": 180, "avg_frame_rate": "30/1",
        "has_b_frames": 1, "duration": "1.000000"
      }],
      "format": { "format_name": "mpeg", "duration": "1.000000", "bit_rate": "458752" }
    }"#;

    #[test]
    fn rejects_oversized_dimensions() {
        const HUGE_JSON: &str = r#"{
          "streams": [{
            "index": 0, "codec_name": "av1", "codec_type": "video",
            "width": 99999, "height": 99999, "avg_frame_rate": "30/1",
            "duration": "1.000000"
          }],
          "format": { "format_name": "mov,mp4,m4a", "duration": "1.000000" }
        }"#;
        let err = parse_ffprobe_metadata(HUGE_JSON, ContainerKind::Mp4).unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn parses_av1_stream() {
        let m = parse_ffprobe_metadata(AV1_JSON, ContainerKind::Mp4).expect("parse");
        assert_eq!(m.codec, Codec::Av1);
        assert_eq!(m.resolution, Resolution::new(320, 180));
        assert_eq!(m.duration, Some(VideoDuration::from_millis(1000)));
        assert_eq!(m.fps_x1000, Some(30_000));
        assert_eq!(m.bitrate_bps, Some(34_984));
        assert_eq!(m.has_b_frames, None);
    }

    #[test]
    fn parses_vp9_stream_without_stream_bitrate() {
        let m = parse_ffprobe_metadata(VP9_JSON, ContainerKind::WebM).expect("parse");
        assert_eq!(m.codec, Codec::Vp9);
        assert_eq!(m.resolution, Resolution::new(320, 180));
        assert_eq!(m.bitrate_bps, Some(161_336));
    }

    #[test]
    fn parses_mpeg2_stream() {
        let m =
            parse_ffprobe_metadata(MPEG2_JSON, ContainerKind::UnsupportedFastPath("mpg".into()))
                .expect("parse");
        assert_eq!(m.codec, Codec::Mpeg2);
        assert_eq!(m.duration, Some(VideoDuration::from_millis(1000)));
        assert_eq!(m.bitrate_bps, Some(458_752));
        assert_eq!(m.has_b_frames, Some(true));
    }

    #[test]
    fn parses_h264_has_b_frames_field() {
        const H264_NO_BF: &str = r#"{
          "streams": [{
            "index": 0, "codec_name": "h264", "codec_type": "video",
            "width": 320, "height": 180, "avg_frame_rate": "30/1",
            "has_b_frames": 0, "duration": "1.000000"
          }],
          "format": { "format_name": "mov,mp4,m4a", "duration": "1.000000" }
        }"#;
        const H264_BF_STR: &str = r#"{
          "streams": [{
            "index": 0, "codec_name": "h264", "codec_type": "video",
            "width": 320, "height": 180, "avg_frame_rate": "30/1",
            "has_b_frames": "2", "duration": "1.000000"
          }],
          "format": { "format_name": "mov,mp4,m4a", "duration": "1.000000" }
        }"#;
        let m = parse_ffprobe_metadata(H264_NO_BF, ContainerKind::Mp4).expect("parse");
        assert_eq!(m.codec, Codec::H264);
        assert_eq!(m.has_b_frames, Some(false));

        let m = parse_ffprobe_metadata(H264_BF_STR, ContainerKind::Mp4).expect("parse");
        assert_eq!(m.has_b_frames, Some(true));
    }

    #[test]
    fn absent_has_b_frames_is_none() {
        const NO_FIELD: &str = r#"{
          "streams": [{
            "index": 0, "codec_name": "h264", "codec_type": "video",
            "width": 320, "height": 180, "avg_frame_rate": "30/1",
            "duration": "1.000000"
          }],
          "format": { "format_name": "mov,mp4,m4a", "duration": "1.000000" }
        }"#;
        let m = parse_ffprobe_metadata(NO_FIELD, ContainerKind::Mp4).expect("parse");
        assert_eq!(m.has_b_frames, None);
    }

    #[test]
    fn codec_name_mapping_covers_fast_and_fallback() {
        assert_eq!(codec_from_name("h264"), Codec::H264);
        assert_eq!(codec_from_name("hevc"), Codec::H265);
        assert_eq!(codec_from_name("h265"), Codec::H265);
        assert_eq!(codec_from_name("av1"), Codec::Av1);
        assert_eq!(codec_from_name("vp9"), Codec::Vp9);
        assert_eq!(codec_from_name("mpeg2video"), Codec::Mpeg2);
        assert_eq!(codec_from_name("prores"), Codec::Other("prores".into()));
    }

    #[test]
    fn rational_parses_fractional_rates() {
        assert_eq!(rational_to_x1000("30/1"), Some(30_000));
        assert_eq!(rational_to_x1000("30000/1001"), Some(29_970));
        assert_eq!(rational_to_x1000("0/0"), None);
        assert_eq!(rational_to_x1000("garbage"), None);
    }

    #[test]
    fn missing_video_stream_is_parse_error() {
        let audio_only = r#"{ "streams": [{ "codec_type": "audio", "codec_name": "aac" }] }"#;
        let err = parse_ffprobe_metadata(audio_only, ContainerKind::Mp4)
            .expect_err("audio-only must error");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }

    #[test]
    fn malformed_json_is_parse_error() {
        let err = parse_ffprobe_metadata("{not json", ContainerKind::Mp4)
            .expect_err("malformed json must error");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }

    #[test]
    fn missing_dimensions_is_parse_error() {
        let no_dims = r#"{ "streams": [{ "codec_type": "video", "codec_name": "av1" }] }"#;
        let err = parse_ffprobe_metadata(no_dims, ContainerKind::Mp4)
            .expect_err("missing width/height must error");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }

    #[test]
    fn ffprobe_arg_vector_has_separator_before_path() {
        use std::ffi::OsStr;

        let mut cmd = std::process::Command::new("ffprobe");
        cmd.args(FFPROBE_ARGS).arg("--").arg("/some/-dash.mp4");

        let args: Vec<&OsStr> = cmd.get_args().collect();
        let sep_pos = args.iter().position(|a| *a == OsStr::new("--"));
        let path_pos = args
            .iter()
            .position(|a| *a == OsStr::new("/some/-dash.mp4"));

        assert!(sep_pos.is_some(), "`--` must be present in the arg list");
        assert!(path_pos.is_some(), "path must be present in the arg list");
        assert_eq!(
            sep_pos.unwrap() + 1,
            path_pos.unwrap(),
            "`--` must be immediately before the path arg"
        );
    }
}
