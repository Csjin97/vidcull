use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp9,
    Mpeg2,
    Other(String),
}

impl Codec {
    #[must_use]
    pub fn is_fast_path_eligible(&self) -> bool {
        matches!(self, Self::H264 | Self::H265)
    }

    #[must_use]
    pub fn short_name(&self) -> &str {
        match self {
            Self::H264 => "h264",
            Self::H265 => "hevc",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Mpeg2 => "mpeg2video",
            Self::Other(s) => s.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_includes_only_h264_and_h265() {
        assert!(Codec::H264.is_fast_path_eligible());
        assert!(Codec::H265.is_fast_path_eligible());
        assert!(!Codec::Av1.is_fast_path_eligible());
        assert!(!Codec::Vp9.is_fast_path_eligible());
        assert!(!Codec::Mpeg2.is_fast_path_eligible());
        assert!(!Codec::Other("prores".into()).is_fast_path_eligible());
    }

    #[test]
    fn short_names_match_ffprobe_conventions() {
        assert_eq!(Codec::H264.short_name(), "h264");
        assert_eq!(Codec::H265.short_name(), "hevc");
        assert_eq!(Codec::Av1.short_name(), "av1");
        assert_eq!(Codec::Vp9.short_name(), "vp9");
        assert_eq!(Codec::Mpeg2.short_name(), "mpeg2video");
        assert_eq!(Codec::Other("prores".into()).short_name(), "prores");
    }

    #[test]
    fn postcard_round_trip_for_unit_variants() {
        for codec in [
            Codec::H264,
            Codec::H265,
            Codec::Av1,
            Codec::Vp9,
            Codec::Mpeg2,
        ] {
            let bytes = postcard::to_allocvec(&codec).expect("encode");
            let decoded: Codec = postcard::from_bytes(&bytes).expect("decode");
            assert_eq!(codec, decoded);
        }
    }

    #[test]
    fn postcard_round_trip_for_other_preserves_label() {
        let original = Codec::Other("ffv1".to_string());
        let bytes = postcard::to_allocvec(&original).expect("encode");
        let decoded: Codec = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(original, decoded);
    }
}
