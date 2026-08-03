use std::path::PathBuf;

use vidcull_core::types::Codec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
    WebM,
    Mpeg,
}

impl Container {
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
            Container::WebM => "webm",
            Container::Mpeg => "mpg",
        }
    }

    #[must_use]
    pub fn for_codec(codec: &Codec) -> Self {
        match codec {
            Codec::H264 | Codec::H265 | Codec::Av1 => Container::Mp4,
            Codec::Vp9 => Container::WebM,
            Codec::Mpeg2 => Container::Mpeg,
            Codec::Other(_) => Container::Mkv,
        }
    }

    #[must_use]
    pub fn subtitle_codec(self) -> &'static str {
        match self {
            Container::Mp4 => "mov_text",
            Container::Mkv | Container::WebM | Container::Mpeg => "srt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    Resize { width: u32, height: u32 },
    Watermark,
    Fps { fps_x1000: u32 },
    Brightness { delta_percent: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encode {
    Copy,
    Reencode {
        codec: Codec,
        bitrate_kbps: Option<u32>,
    },
}

impl Encode {
    #[must_use]
    pub fn video_encoder(codec: &Codec) -> Option<&'static str> {
        match codec {
            Codec::H264 => Some("libx264"),
            Codec::H265 => Some("libx265"),
            Codec::Av1 => Some("libaom-av1"),
            Codec::Vp9 => Some("libvpx-vp9"),
            Codec::Mpeg2 => Some("mpeg2video"),
            Codec::Other(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clip {
    pub start_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub source: PathBuf,
    pub clip: Option<Clip>,
    pub filters: Vec<Filter>,
    pub subtitle: Option<String>,
    pub encode: Encode,
    pub container: Container,
}

impl Recipe {
    #[must_use]
    pub fn reencode(source: impl Into<PathBuf>, codec: Codec) -> Self {
        let container = Container::for_codec(&codec);
        Self {
            source: source.into(),
            clip: None,
            filters: Vec::new(),
            subtitle: None,
            encode: Encode::Reencode {
                codec,
                bitrate_kbps: None,
            },
            container,
        }
    }

    #[must_use]
    pub fn remux(source: impl Into<PathBuf>, container: Container) -> Self {
        Self {
            source: source.into(),
            clip: None,
            filters: Vec::new(),
            subtitle: None,
            encode: Encode::Copy,
            container,
        }
    }

    #[must_use]
    pub fn with_clip(mut self, start_ms: u64, duration_ms: u64) -> Self {
        self.clip = Some(Clip {
            start_ms,
            duration_ms,
        });
        self
    }

    #[must_use]
    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filters.push(filter);
        self
    }

    #[must_use]
    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        if let Encode::Reencode { bitrate_kbps, .. } = &mut self.encode {
            *bitrate_kbps = Some(kbps);
        }
        self
    }

    #[must_use]
    pub fn with_subtitle(mut self, text: impl Into<String>) -> Self {
        self.subtitle = Some(text.into());
        self
    }

    #[must_use]
    pub fn source_stem(&self) -> &str {
        self.source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_for_codec_matches_natural_mux() {
        assert_eq!(Container::for_codec(&Codec::H264), Container::Mp4);
        assert_eq!(Container::for_codec(&Codec::H265), Container::Mp4);
        assert_eq!(Container::for_codec(&Codec::Av1), Container::Mp4);
        assert_eq!(Container::for_codec(&Codec::Vp9), Container::WebM);
        assert_eq!(Container::for_codec(&Codec::Mpeg2), Container::Mpeg);
        assert_eq!(
            Container::for_codec(&Codec::Other("prores".into())),
            Container::Mkv
        );
    }

    #[test]
    fn encoder_names_cover_known_codecs_and_reject_other() {
        assert_eq!(Encode::video_encoder(&Codec::H264), Some("libx264"));
        assert_eq!(Encode::video_encoder(&Codec::H265), Some("libx265"));
        assert_eq!(Encode::video_encoder(&Codec::Av1), Some("libaom-av1"));
        assert_eq!(Encode::video_encoder(&Codec::Vp9), Some("libvpx-vp9"));
        assert_eq!(Encode::video_encoder(&Codec::Mpeg2), Some("mpeg2video"));
        assert_eq!(Encode::video_encoder(&Codec::Other("x".into())), None);
    }

    #[test]
    fn builder_composes_fields() {
        let r = Recipe::reencode("/src/long.mp4", Codec::H264)
            .with_clip(1000, 2000)
            .with_filter(Filter::Resize {
                width: 160,
                height: 90,
            })
            .with_bitrate(500)
            .with_subtitle("hi");
        assert_eq!(
            r.clip,
            Some(Clip {
                start_ms: 1000,
                duration_ms: 2000
            })
        );
        assert_eq!(r.filters.len(), 1);
        assert_eq!(r.subtitle.as_deref(), Some("hi"));
        assert_eq!(r.source_stem(), "long");
        assert!(matches!(
            r.encode,
            Encode::Reencode {
                bitrate_kbps: Some(500),
                ..
            }
        ));
    }

    #[test]
    fn with_bitrate_is_a_noop_on_copy() {
        let r = Recipe::remux("/src/a.mp4", Container::Mkv).with_bitrate(800);
        assert_eq!(r.encode, Encode::Copy);
    }
}
