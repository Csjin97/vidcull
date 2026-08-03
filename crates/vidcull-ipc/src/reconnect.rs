use std::time::Duration;

use vidcull_core::Result;

use crate::IpcClient;

pub const DEFAULT_BASE: Duration = Duration::from_millis(200);

pub const DEFAULT_CAP: Duration = Duration::from_secs(30);

#[must_use]
pub fn next_backoff(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let factor = 1u128.checked_shl(attempt).unwrap_or(u128::MAX);
    let scaled = base.as_nanos().saturating_mul(factor);
    let capped = scaled.min(cap.as_nanos());
    let secs = u64::try_from(capped / 1_000_000_000).unwrap_or(u64::MAX);
    let nanos = u32::try_from(capped % 1_000_000_000).unwrap_or(0);
    Duration::new(secs, nanos)
}

#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    pub base: Duration,
    pub cap: Duration,
    pub attempt_timeout: Duration,
    pub max_attempts: u32,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base: DEFAULT_BASE,
            cap: DEFAULT_CAP,
            attempt_timeout: Duration::from_millis(500),
            max_attempts: u32::MAX,
        }
    }
}

pub async fn connect_with_backoff(address: &str, policy: BackoffPolicy) -> Result<IpcClient> {
    let attempts = policy.max_attempts.max(1);
    let mut last_err = None;
    for attempt in 0..attempts {
        match IpcClient::connect_timeout(address, policy.attempt_timeout).await {
            Ok(client) => return Ok(client),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    tokio::time::sleep(next_backoff(attempt, policy.base, policy.cap)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        vidcull_core::Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "no connection attempts were made",
        ))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(100);
    const CAP: Duration = Duration::from_secs(30);

    #[test]
    fn first_attempt_is_the_base_delay() {
        assert_eq!(next_backoff(0, BASE, CAP), BASE);
    }

    #[test]
    fn doubles_each_attempt() {
        assert_eq!(next_backoff(1, BASE, CAP), Duration::from_millis(200));
        assert_eq!(next_backoff(2, BASE, CAP), Duration::from_millis(400));
        assert_eq!(next_backoff(3, BASE, CAP), Duration::from_millis(800));
    }

    #[test]
    fn clamps_to_the_cap() {
        assert_eq!(next_backoff(10, BASE, CAP), CAP);
    }

    #[test]
    fn is_monotonic_non_decreasing() {
        let mut prev = Duration::ZERO;
        for attempt in 0..40 {
            let cur = next_backoff(attempt, BASE, CAP);
            assert!(cur >= prev, "backoff dropped at attempt {attempt}");
            prev = cur;
        }
    }

    #[test]
    fn huge_attempt_saturates_to_cap_without_overflow() {
        assert_eq!(next_backoff(u32::MAX, BASE, CAP), CAP);
        assert_eq!(next_backoff(200, BASE, CAP), CAP);
    }

    #[test]
    fn zero_base_stays_zero() {
        assert_eq!(next_backoff(5, Duration::ZERO, CAP), Duration::ZERO);
    }
}
