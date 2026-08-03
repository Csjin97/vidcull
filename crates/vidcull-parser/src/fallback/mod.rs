mod binary;
pub mod concurrency;
mod decode;
mod probe;
mod sidecar;
mod timeout;

use std::sync::atomic::{AtomicU64, Ordering};

use vidcull_core::Error;
use vidcull_core::types::Codec;

pub use binary::FfmpegBinaries;
pub use concurrency::{DecodeConcurrency, DecodePermit};
pub(crate) use decode::plan_fallback_timestamps;
pub use decode::{
    decode_batch_head, decode_frame_at, decode_sparse, decode_sparse_strided,
    decode_sparse_strided_with, decode_sparse_strided_with_streaming, decode_sparse_with,
    decode_sparse_with_streaming, decode_thumb_frame_at, fallback_spawn_plan, full_grid_len,
    thumb_decode_args,
};
pub use probe::{probe_fallback, probe_fallback_cancellable};
pub use timeout::{RENDER_TIMEOUT_SECS, TIMEOUT_TOKEN, effective_timeout, run_with_timeout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodePath {
    Native,
    Fallback,
}

#[must_use]
pub fn decode_path_for(codec: &Codec) -> DecodePath {
    if codec.is_fast_path_eligible() {
        DecodePath::Native
    } else {
        DecodePath::Fallback
    }
}

#[must_use]
pub fn should_probe_fallback(err: &Error) -> bool {
    matches!(err, Error::Unsupported(_) | Error::Parse(_))
}

#[derive(Debug, Default)]
pub struct FallbackMetrics {
    native: AtomicU64,
    fallback: AtomicU64,
}

impl FallbackMetrics {
    pub fn record(&self, path: DecodePath) {
        match path {
            DecodePath::Native => &self.native,
            DecodePath::Fallback => &self.fallback,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn native_count(&self) -> u64 {
        self.native.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn fallback_count(&self) -> u64 {
        self.fallback.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn fallback_rate(&self) -> f64 {
        let fallback = self.fallback_count();
        let total = self.native_count() + fallback;
        if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            (fallback as f64 / total as f64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_and_h265_stay_native() {
        assert_eq!(decode_path_for(&Codec::H264), DecodePath::Native);
        assert_eq!(decode_path_for(&Codec::H265), DecodePath::Native);
    }

    #[test]
    fn fallback_codecs_route_to_ffmpeg() {
        assert_eq!(decode_path_for(&Codec::Av1), DecodePath::Fallback);
        assert_eq!(decode_path_for(&Codec::Vp9), DecodePath::Fallback);
        assert_eq!(decode_path_for(&Codec::Mpeg2), DecodePath::Fallback);
        assert_eq!(
            decode_path_for(&Codec::Other("prores".into())),
            DecodePath::Fallback
        );
    }

    #[test]
    fn probe_fallback_escalates_on_unsupported_and_parse_only() {
        assert!(should_probe_fallback(&Error::Unsupported("av1".into())));
        assert!(should_probe_fallback(&Error::Parse("bad box".into())));
        assert!(!should_probe_fallback(&Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing"
        ))));
        assert!(!should_probe_fallback(&Error::Decode("x".into())));
    }

    #[test]
    fn metrics_count_each_path_and_compute_rate() {
        let m = FallbackMetrics::default();
        assert!(m.fallback_rate().abs() < f64::EPSILON);
        m.record(DecodePath::Native);
        m.record(DecodePath::Native);
        m.record(DecodePath::Native);
        m.record(DecodePath::Fallback);
        assert_eq!(m.native_count(), 3);
        assert_eq!(m.fallback_count(), 1);
        assert!((m.fallback_rate() - 0.25).abs() < f64::EPSILON);
    }
}
