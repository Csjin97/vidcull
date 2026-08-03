use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct VideoDuration {
    millis: u64,
}

impl VideoDuration {
    pub const ZERO: Self = Self { millis: 0 };

    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    #[must_use]
    pub fn from_secs_f64(secs: f64) -> Self {
        if !secs.is_finite() || secs <= 0.0 {
            return Self::ZERO;
        }
        let millis = (secs * 1000.0).round();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let clamped = if millis >= 1.0e19_f64 {
            u64::MAX
        } else {
            millis as u64
        };
        Self { millis: clamped }
    }

    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.millis
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_secs_f64(self) -> f64 {
        self.millis as f64 / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_millis_round_trip() {
        let d = VideoDuration::from_millis(123_456);
        assert_eq!(d.as_millis(), 123_456);
    }

    #[test]
    fn from_secs_f64_rounds_to_nearest_millisecond() {
        let d = VideoDuration::from_secs_f64(1.2345);
        assert_eq!(d.as_millis(), 1235);
    }

    #[test]
    fn negative_seconds_clamp_to_zero() {
        assert_eq!(VideoDuration::from_secs_f64(-3.0), VideoDuration::ZERO);
    }

    #[test]
    fn nan_clamps_to_zero() {
        assert_eq!(VideoDuration::from_secs_f64(f64::NAN), VideoDuration::ZERO);
    }

    #[test]
    fn ordering_is_numeric() {
        let mut ds = vec![
            VideoDuration::from_millis(500),
            VideoDuration::from_millis(50),
            VideoDuration::from_millis(5000),
        ];
        ds.sort();
        assert_eq!(
            ds,
            vec![
                VideoDuration::from_millis(50),
                VideoDuration::from_millis(500),
                VideoDuration::from_millis(5000),
            ]
        );
    }

    #[test]
    fn postcard_round_trip() {
        let d = VideoDuration::from_millis(987_654_321);
        let bytes = postcard::to_allocvec(&d).expect("encode");
        let decoded: VideoDuration = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(d, decoded);
    }

    #[test]
    fn infinity_clamps_to_zero() {
        assert_eq!(
            VideoDuration::from_secs_f64(f64::INFINITY),
            VideoDuration::ZERO
        );
    }

    #[test]
    fn zero_secs_clamp_to_zero() {
        assert_eq!(VideoDuration::from_secs_f64(0.0), VideoDuration::ZERO);
    }

    #[test]
    fn huge_secs_saturate_to_max() {
        assert_eq!(VideoDuration::from_secs_f64(1.0e19).as_millis(), u64::MAX);
    }

    #[test]
    fn as_secs_f64_inverse() {
        assert!((VideoDuration::from_millis(1500).as_secs_f64() - 1.5).abs() < f64::EPSILON);
    }
}
