use std::path::Path;

use vidcull_core::types::{Codec, Resolution, VideoDuration};
use vidcull_core::{Error, Result};

use crate::cancel::Cancel;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContainerKind {
    Mp4,
    Mov,
    ThreeGp,
    Mkv,
    WebM,
    UnsupportedFastPath(String),
}

impl ContainerKind {
    #[must_use]
    pub fn short_name(&self) -> &str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::ThreeGp => "3gp",
            Self::Mkv => "mkv",
            Self::WebM => "webm",
            Self::UnsupportedFastPath(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoMetadata {
    pub container: ContainerKind,
    pub codec: Codec,
    pub resolution: Resolution,
    pub duration: Option<VideoDuration>,
    pub fps_x1000: Option<u32>,
    pub has_b_frames: Option<bool>,
    pub bitrate_bps: Option<u64>,
    pub encoder_tags: Option<String>,
}

#[must_use]
pub fn container_kind_from_path(path: &Path) -> ContainerKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("mp4" | "m4v") => ContainerKind::Mp4,
        Some("mov") => ContainerKind::Mov,
        Some("3gp") => ContainerKind::ThreeGp,
        Some("mkv") => ContainerKind::Mkv,
        Some("webm") => ContainerKind::WebM,
        Some(other) => ContainerKind::UnsupportedFastPath(other.to_owned()),
        None => ContainerKind::UnsupportedFastPath(String::new()),
    }
}

pub fn probe<P: AsRef<Path>>(path: P) -> Result<VideoMetadata> {
    probe_cancellable(path, Cancel::default())
}

pub fn probe_cancellable<P: AsRef<Path>>(path: P, cancel: Cancel<'_>) -> Result<VideoMetadata> {
    probe_with_context_cancellable(path, cancel, crate::mp4::PreParsedMp4::NotAttempted)
        .map(|(metadata, _)| metadata)
}

pub(crate) fn probe_with_context_cancellable<P: AsRef<Path>>(
    path: P,
    cancel: Cancel<'_>,
    pre_parsed: crate::mp4::PreParsedMp4,
) -> Result<(VideoMetadata, Option<mp4parse::MediaContext>)> {
    let path = path.as_ref();
    let kind = container_kind_from_path(path);
    match kind {
        ContainerKind::Mp4 | ContainerKind::Mov | ContainerKind::ThreeGp => match pre_parsed {
            crate::mp4::PreParsedMp4::Parsed(context) => {
                let file_size_bytes = std::fs::metadata(path)?.len();
                let metadata =
                    crate::mp4::probe_mp4_from_context(&context, path, kind, file_size_bytes)?;
                Ok((metadata, Some(*context)))
            }
            crate::mp4::PreParsedMp4::Failed => Err(Error::Parse(
                "mp4parse: fused hash+parse pass already declined".into(),
            )),
            crate::mp4::PreParsedMp4::MkvParsed(_) | crate::mp4::PreParsedMp4::MkvFailed => Err(
                Error::Parse("mp4parse: mismatched fused container context".into()),
            ),
            crate::mp4::PreParsedMp4::NotAttempted => {
                let (metadata, context) =
                    crate::mp4::probe_mp4_with_context_cancellable(path, kind, cancel)?;
                Ok((metadata, Some(context)))
            }
        },
        ContainerKind::Mkv | ContainerKind::WebM => match pre_parsed {
            crate::mp4::PreParsedMp4::MkvParsed(metadata) => Ok((metadata, None)),
            crate::mp4::PreParsedMp4::MkvFailed => Err(Error::Parse(
                "matroska: fused hash+parse pass already declined".into(),
            )),
            crate::mp4::PreParsedMp4::NotAttempted => {
                Ok((crate::mkv::probe_mkv(path, kind)?, None))
            }
            crate::mp4::PreParsedMp4::Parsed(_) | crate::mp4::PreParsedMp4::Failed => Err(
                Error::Parse("matroska: mismatched fused container context".into()),
            ),
        },
        ContainerKind::UnsupportedFastPath(ext) => Err(Error::Unsupported(format!(
            "no fast-path parser for extension `{ext}`"
        ))),
    }
}

pub(crate) fn no_video_track(container: &str) -> Error {
    Error::Parse(format!("{container}: no video track found"))
}

#[must_use]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub(crate) fn fps_to_x1000(fps_hz: f64) -> Option<u32> {
    if !fps_hz.is_finite() || fps_hz <= 0.0 {
        return None;
    }
    let scaled = (fps_hz * 1000.0).round();
    if scaled <= 0.0 || scaled > f64::from(u32::MAX) {
        return None;
    }
    Some(scaled as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_x1000_handles_common_rates() {
        assert_eq!(fps_to_x1000(24.0), Some(24_000));
        assert_eq!(fps_to_x1000(29.97), Some(29_970));
        assert_eq!(fps_to_x1000(60.0), Some(60_000));
    }

    #[test]
    fn fps_x1000_rejects_invalid_inputs() {
        assert_eq!(fps_to_x1000(0.0), None);
        assert_eq!(fps_to_x1000(-30.0), None);
        assert_eq!(fps_to_x1000(f64::NAN), None);
        assert_eq!(fps_to_x1000(f64::INFINITY), None);
    }

    #[test]
    fn container_kind_routes_extensions() {
        assert_eq!(
            container_kind_from_path(Path::new("/x/a.MP4")),
            ContainerKind::Mp4
        );
        assert_eq!(
            container_kind_from_path(Path::new("clip.mkv")),
            ContainerKind::Mkv
        );
        assert_eq!(
            container_kind_from_path(Path::new("clip.WebM")),
            ContainerKind::WebM
        );
        assert_eq!(
            container_kind_from_path(Path::new("clip.avi")),
            ContainerKind::UnsupportedFastPath("avi".into()),
        );
        assert_eq!(
            container_kind_from_path(Path::new("noext")),
            ContainerKind::UnsupportedFastPath(String::new()),
        );
    }

    #[test]
    fn probe_rejects_unsupported_extension_cheaply() {
        let err = probe(Path::new("/does/not/exist.avi"))
            .expect_err("avi must route to fallback path, not native parser");
        match err {
            Error::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn container_kind_short_names() {
        assert_eq!(ContainerKind::Mp4.short_name(), "mp4");
        assert_eq!(ContainerKind::Mkv.short_name(), "mkv");
        assert_eq!(ContainerKind::WebM.short_name(), "webm");
        assert_eq!(
            ContainerKind::UnsupportedFastPath("avi".into()).short_name(),
            "avi"
        );
    }

    #[test]
    fn no_video_track_renders_container_label() {
        match no_video_track("mp4") {
            Error::Parse(msg) => assert!(msg.starts_with("mp4: "), "got: {msg}"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn fused_mkv_metadata_is_reused_without_reopening_the_file() {
        let metadata = VideoMetadata {
            container: ContainerKind::Mkv,
            codec: Codec::H264,
            resolution: Resolution::new(1920, 1080),
            duration: Some(VideoDuration::from_millis(10_000)),
            fps_x1000: Some(30_000),
            has_b_frames: None,
            bitrate_bps: Some(8_000_000),
            encoder_tags: None,
        };
        let (reused, context) = probe_with_context_cancellable(
            Path::new("/definitely/missing/video.mkv"),
            Cancel::default(),
            crate::mp4::PreParsedMp4::MkvParsed(metadata.clone()),
        )
        .expect("pre-parsed MKV metadata must not touch the path");
        assert_eq!(reused, metadata);
        assert!(context.is_none());
    }
}
