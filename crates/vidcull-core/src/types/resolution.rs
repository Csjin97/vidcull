use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    #[must_use]
    pub const fn pixels(self) -> u64 {
        (self.width as u64) * (self.height as u64)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_count_for_common_resolutions() {
        assert_eq!(Resolution::new(1920, 1080).pixels(), 2_073_600);
        assert_eq!(Resolution::new(3840, 2160).pixels(), 8_294_400);
    }

    #[test]
    fn is_empty_when_either_axis_is_zero() {
        assert!(Resolution::new(0, 1080).is_empty());
        assert!(Resolution::new(1920, 0).is_empty());
        assert!(!Resolution::new(640, 480).is_empty());
    }

    #[test]
    fn postcard_round_trip() {
        let r = Resolution::new(1920, 1080);
        let bytes = postcard::to_allocvec(&r).expect("encode");
        let decoded: Resolution = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(r, decoded);
    }

    #[test]
    fn pixels_does_not_overflow_for_8k() {
        assert_eq!(Resolution::new(7680, 4320).pixels(), 33_177_600);
    }

    #[test]
    fn both_zero_is_empty() {
        assert!(Resolution::new(0, 0).is_empty());
    }

    #[test]
    fn zero_dimension_has_zero_pixels() {
        assert_eq!(Resolution::new(0, 1080).pixels(), 0);
    }
}
