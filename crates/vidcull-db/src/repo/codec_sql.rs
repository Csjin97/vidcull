use vidcull_core::types::Codec;

pub(super) fn to_text(codec: &Codec) -> &str {
    codec.short_name()
}

pub(super) fn from_text(s: &str) -> Codec {
    match s {
        "h264" => Codec::H264,
        "hevc" | "h265" => Codec::H265,
        "av1" => Codec::Av1,
        "vp9" => Codec::Vp9,
        "mpeg2video" | "mpeg2" => Codec::Mpeg2,
        other => Codec::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_variants_round_trip() {
        for codec in [
            Codec::H264,
            Codec::H265,
            Codec::Av1,
            Codec::Vp9,
            Codec::Mpeg2,
        ] {
            let text = to_text(&codec);
            assert_eq!(from_text(text), codec, "codec {codec:?} did not round-trip");
        }
    }

    #[test]
    fn unknown_label_falls_through_to_other() {
        assert_eq!(from_text("prores"), Codec::Other("prores".into()));
    }

    #[test]
    fn other_variant_round_trips_label() {
        let original = Codec::Other("ffv1".into());
        let text = to_text(&original);
        assert_eq!(from_text(text), original);
    }

    #[test]
    fn legacy_h265_alias_is_accepted() {
        assert_eq!(from_text("h265"), Codec::H265);
    }
}
