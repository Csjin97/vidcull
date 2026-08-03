use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Default)]
pub struct WorkerHealth {
    inner: Arc<HealthInner>,
}

#[derive(Default)]
struct HealthInner {
    dead_workers: AtomicU32,
    panic_count: AtomicU32,
}

impl WorkerHealth {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_death(&self) {
        self.inner.dead_workers.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn dead_workers(&self) -> u32 {
        self.inner.dead_workers.load(Ordering::Relaxed)
    }

    pub fn record_panic(&self) {
        self.inner.panic_count.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn panic_count(&self) -> u32 {
        self.inner.panic_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        let h = WorkerHealth::new();
        assert_eq!(h.dead_workers(), 0);
        assert_eq!(h.panic_count(), 0);
    }

    #[test]
    fn record_death_increments() {
        let h = WorkerHealth::new();
        h.record_death();
        h.record_death();
        assert_eq!(h.dead_workers(), 2);
    }

    #[test]
    fn record_panic_increments() {
        let h = WorkerHealth::new();
        h.record_panic();
        assert_eq!(h.panic_count(), 1);
    }

    #[test]
    fn clone_shares_state() {
        let h = WorkerHealth::new();
        let h2 = h.clone();
        h.record_death();
        assert_eq!(h2.dead_workers(), 1);
        h2.record_panic();
        assert_eq!(h.panic_count(), 1);
    }
}
