use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct DecodeTiming {
    pub frames: usize,
    pub total: Duration,
}

impl DecodeTiming {
    #[must_use]
    pub fn per_frame(&self) -> Duration {
        if self.frames == 0 {
            return Duration::ZERO;
        }
        self.total / u32::try_from(self.frames).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn extrapolate(&self, target_frames: usize) -> Duration {
        self.per_frame() * u32::try_from(target_frames).unwrap_or(u32::MAX)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SpawnDecompose {
    pub decode_marginal: Duration,
    pub spawn_once: Duration,
}

impl SpawnDecompose {
    #[must_use]
    pub fn from_batch_pair(small: DecodeTiming, large: DecodeTiming) -> Self {
        let (lo, hi) = if large.frames >= small.frames {
            (small, large)
        } else {
            (large, small)
        };
        let dn = hi.frames.saturating_sub(lo.frames);
        if dn == 0 {
            return Self {
                decode_marginal: Duration::ZERO,
                spawn_once: lo.total,
            };
        }
        let decode_marginal =
            hi.total.saturating_sub(lo.total) / u32::try_from(dn).unwrap_or(u32::MAX);
        let lo_decode = decode_marginal * u32::try_from(lo.frames).unwrap_or(u32::MAX);
        let spawn_once = lo.total.saturating_sub(lo_decode);
        Self {
            decode_marginal,
            spawn_once,
        }
    }

    #[must_use]
    pub fn extrapolate_batch(&self, count: usize) -> Duration {
        self.spawn_once + self.decode_marginal * u32::try_from(count).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn spawn_seek_fixed(&self, perframe_per_frame: Duration) -> Duration {
        perframe_per_frame.saturating_sub(self.decode_marginal)
    }
}

#[must_use]
pub fn ratio_of(baseline: Duration, other: Duration) -> f64 {
    if baseline.is_zero() {
        return 0.0;
    }
    other.as_secs_f64() / baseline.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_frame_divides_total_by_count() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let t = DecodeTiming {
            frames: 12,
            total: Duration::from_millis(1200),
        };
        assert_eq!(t.per_frame(), Duration::from_millis(100));
    }

    #[test]
    fn per_frame_of_zero_frames_is_zero_not_a_panic() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let t = DecodeTiming {
            frames: 0,
            total: Duration::from_millis(500),
        };
        assert_eq!(t.per_frame(), Duration::ZERO);
    }

    #[test]
    fn extrapolate_scales_per_frame_to_target() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let t = DecodeTiming {
            frames: 8,
            total: Duration::from_millis(800),
        };
        assert_eq!(t.extrapolate(12), Duration::from_millis(1200));
    }

    #[test]
    fn extrapolate_back_to_measured_count_round_trips() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let t = DecodeTiming {
            frames: 12,
            total: Duration::from_millis(2400),
        };
        assert_eq!(t.extrapolate(12), Duration::from_millis(2400));
    }

    #[test]
    fn spawn_decompose_recovers_slope_and_intercept() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let small = DecodeTiming {
            frames: 12,
            total: Duration::from_millis(320),
        };
        let large = DecodeTiming {
            frames: 500,
            total: Duration::from_millis(5200),
        };
        let d = SpawnDecompose::from_batch_pair(small, large);
        assert_eq!(d.decode_marginal, Duration::from_millis(10));
        assert_eq!(d.spawn_once, Duration::from_millis(200));
        assert_eq!(d.extrapolate_batch(500), Duration::from_millis(5200));
    }

    #[test]
    fn spawn_decompose_orders_inputs_by_frame_count() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let small = DecodeTiming {
            frames: 12,
            total: Duration::from_millis(320),
        };
        let large = DecodeTiming {
            frames: 500,
            total: Duration::from_millis(5200),
        };
        let d = SpawnDecompose::from_batch_pair(large, small);
        assert_eq!(d.decode_marginal, Duration::from_millis(10));
        assert_eq!(d.spawn_once, Duration::from_millis(200));
    }

    #[test]
    fn spawn_decompose_equal_counts_falls_back_without_panic() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let a = DecodeTiming {
            frames: 12,
            total: Duration::from_millis(320),
        };
        let d = SpawnDecompose::from_batch_pair(a, a);
        assert_eq!(d.decode_marginal, Duration::ZERO);
        assert_eq!(d.spawn_once, Duration::from_millis(320));
    }

    #[test]
    fn spawn_decompose_noise_never_goes_negative() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let small = DecodeTiming {
            frames: 12,
            total: Duration::from_millis(500),
        };
        let large = DecodeTiming {
            frames: 500,
            total: Duration::from_millis(400),
        };
        let d = SpawnDecompose::from_batch_pair(small, large);
        assert_eq!(d.decode_marginal, Duration::ZERO);
        assert_eq!(
            d.spawn_seek_fixed(Duration::from_millis(5)),
            Duration::from_millis(5)
        );
    }

    #[test]
    fn spawn_seek_fixed_is_perframe_minus_marginal() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        let d = SpawnDecompose {
            decode_marginal: Duration::from_millis(10),
            spawn_once: Duration::from_millis(200),
        };
        assert_eq!(
            d.spawn_seek_fixed(Duration::from_millis(35)),
            Duration::from_millis(25)
        );
    }

    #[test]
    fn ratio_of_computes_relative_cost() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        assert!(
            (ratio_of(Duration::from_millis(50), Duration::from_millis(150)) - 3.0).abs() < 1e-9
        );
        assert!(
            (ratio_of(Duration::from_millis(100), Duration::from_millis(105)) - 1.05).abs() < 1e-9
        );
    }

    #[test]
    fn ratio_of_zero_baseline_is_zero_not_a_panic() {
        let _guard = crate::TEST_LOCK.lock().unwrap();
        assert!(ratio_of(Duration::ZERO, Duration::from_millis(10)).abs() < f64::EPSILON);
    }
}
