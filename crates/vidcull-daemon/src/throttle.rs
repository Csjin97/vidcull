use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use vidcull_core::types::NormalizedPath;
use vidcull_ipc::CpuThrottle;

use crate::bridge::path_under_root;

#[derive(Debug, Default)]
pub struct ThrottleControl {
    cooldown_floor_ns: AtomicU64,
    max_performance: AtomicBool,
    level: AtomicU8,
    idle_workers: AtomicUsize,
    io_budget_cap: AtomicUsize,
    partial_clips: AtomicBool,
    indexing_paused: AtomicBool,
    removed_roots: Mutex<Vec<NormalizedPath>>,
    removed_cleanup_pending: AtomicBool,
    active_decodes: Mutex<Vec<(NormalizedPath, Arc<AtomicBool>)>>,
}

impl ThrottleControl {
    pub fn set_level(&self, level: CpuThrottle) {
        let floor = u64::try_from(cpu_throttle_cooldown(level).as_nanos()).unwrap_or(u64::MAX);
        self.cooldown_floor_ns.store(floor, Ordering::Relaxed);
        self.max_performance
            .store(matches!(level, CpuThrottle::Full), Ordering::Relaxed);
        self.level
            .store(cpu_throttle_to_u8(level), Ordering::Relaxed);
    }

    #[must_use]
    pub fn level(&self) -> CpuThrottle {
        cpu_throttle_from_u8(self.level.load(Ordering::Relaxed))
    }

    #[must_use]
    pub fn is_max_performance(&self) -> bool {
        self.max_performance.load(Ordering::Relaxed)
    }

    pub fn set_idle_workers(&self, workers: Option<usize>) {
        self.idle_workers
            .store(workers.unwrap_or(0), Ordering::Relaxed);
    }

    #[must_use]
    pub fn idle_workers_override(&self) -> Option<usize> {
        match self.idle_workers.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    pub fn set_io_budget_cap(&self, cap: usize) {
        self.io_budget_cap.store(cap, Ordering::Relaxed);
    }

    #[must_use]
    pub fn io_budget_cap(&self) -> Option<usize> {
        match self.io_budget_cap.load(Ordering::Relaxed) {
            0 => None,
            cap => Some(cap),
        }
    }

    pub fn set_partial_clips(&self, enabled: bool) {
        self.partial_clips.store(enabled, Ordering::Relaxed);
    }

    #[must_use]
    pub fn partial_clips_enabled(&self) -> bool {
        self.partial_clips.load(Ordering::Relaxed)
    }

    pub fn set_indexing_enabled(&self, enabled: bool) {
        self.indexing_paused.store(!enabled, Ordering::Relaxed);
    }

    #[must_use]
    pub fn indexing_enabled(&self) -> bool {
        !self.indexing_paused.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.indexing_paused
    }

    #[must_use]
    pub fn register_decode(&self, path: &NormalizedPath) -> DecodeRegistration<'_> {
        let token = Arc::new(AtomicBool::new(false));
        let mut active = self
            .active_decodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let removed = self
                .removed_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if removed
                .iter()
                .any(|root| path_under_root(path.as_str(), root.as_str()))
            {
                token.store(true, Ordering::Relaxed);
            }
        }
        active.push((path.clone(), Arc::clone(&token)));
        DecodeRegistration {
            control: self,
            token,
        }
    }

    fn deregister_decode(&self, token: &Arc<AtomicBool>) {
        let mut active = self
            .active_decodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.retain(|(_, t)| !Arc::ptr_eq(t, token));
    }

    pub fn mark_roots_removed(&self, roots: &[NormalizedPath]) {
        if roots.is_empty() {
            return;
        }
        {
            let mut removed = self
                .removed_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            removed.extend(roots.iter().cloned());
            self.removed_cleanup_pending.store(true, Ordering::Relaxed);
        }
        let active = self
            .active_decodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (path, token) in active.iter() {
            if roots
                .iter()
                .any(|root| path_under_root(path.as_str(), root.as_str()))
            {
                token.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn is_path_removed(&self, path: &NormalizedPath) -> bool {
        let removed = self
            .removed_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        removed
            .iter()
            .any(|root| path_under_root(path.as_str(), root.as_str()))
    }

    #[must_use]
    pub fn removed_cleanup_pending(&self) -> bool {
        self.removed_cleanup_pending.load(Ordering::Relaxed)
    }

    pub(crate) fn clear_removed_cleanup_pending(&self) {
        self.removed_cleanup_pending.store(false, Ordering::Relaxed);
    }

    pub fn clear_removed_root_overlap(&self, added: &NormalizedPath) {
        let mut removed = self
            .removed_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        removed.retain(|root| {
            !(path_under_root(added.as_str(), root.as_str())
                || path_under_root(root.as_str(), added.as_str()))
        });
    }

    #[must_use]
    pub fn effective_cooldown(&self, activity_cooldown: Duration) -> Duration {
        if self.max_performance.load(Ordering::Relaxed) {
            Duration::ZERO
        } else {
            let floor = Duration::from_nanos(self.cooldown_floor_ns.load(Ordering::Relaxed));
            activity_cooldown.max(floor)
        }
    }
}

pub struct DecodeRegistration<'a> {
    control: &'a ThrottleControl,
    token: Arc<AtomicBool>,
}

impl DecodeRegistration<'_> {
    #[must_use]
    pub fn token(&self) -> &AtomicBool {
        self.token.as_ref()
    }
}

impl Drop for DecodeRegistration<'_> {
    fn drop(&mut self) {
        self.control.deregister_decode(&self.token);
    }
}

#[must_use]
pub fn cpu_throttle_cooldown(level: CpuThrottle) -> Duration {
    match level {
        CpuThrottle::Full => Duration::ZERO,
        CpuThrottle::Balanced => Duration::from_millis(50),
        CpuThrottle::Eco => Duration::from_millis(200),
    }
}

fn cpu_throttle_to_u8(level: CpuThrottle) -> u8 {
    match level {
        CpuThrottle::Full => 0,
        CpuThrottle::Balanced => 1,
        CpuThrottle::Eco => 2,
    }
}

fn cpu_throttle_from_u8(value: u8) -> CpuThrottle {
    match value {
        1 => CpuThrottle::Balanced,
        2 => CpuThrottle::Eco,
        _ => CpuThrottle::Full,
    }
}

#[must_use]
pub fn cpu_throttle_idle_budget(level: CpuThrottle, base_idle: usize, cores: usize) -> usize {
    let base = base_idle.max(1);
    let cores = cores.max(1);
    match level {
        CpuThrottle::Full => base.saturating_mul(2).min(cores),
        CpuThrottle::Balanced => base,
        CpuThrottle::Eco => (base / 2).max(1),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    UserActive,
}

#[derive(Debug, Clone)]
pub struct RateLimiter {
    rate_per_sec: u64,
    capacity: u64,
    available: u64,
    last_ns: i64,
}

impl RateLimiter {
    #[must_use]
    pub fn new(rate_per_sec: u64, capacity: u64, now_ns: i64) -> Self {
        let capacity = capacity.max(1);
        Self {
            rate_per_sec: rate_per_sec.max(1),
            capacity,
            available: capacity,
            last_ns: now_ns,
        }
    }

    fn refill(&mut self, now_ns: i64) {
        let elapsed_ns = u128::try_from(now_ns.saturating_sub(self.last_ns)).unwrap_or(0);
        let added = elapsed_ns.saturating_mul(u128::from(self.rate_per_sec)) / 1_000_000_000;
        let added = u64::try_from(added).unwrap_or(u64::MAX);
        self.available = self.available.saturating_add(added).min(self.capacity);
        self.last_ns = now_ns;
    }

    pub fn acquire(&mut self, cost: u64, now_ns: i64) -> Duration {
        self.refill(now_ns);
        if self.available >= cost || self.available >= self.capacity {
            self.available = self.available.saturating_sub(cost);
            return Duration::ZERO;
        }
        let deficit = cost - self.available;
        let wait_ns = (u128::from(deficit) * 1_000_000_000).div_ceil(u128::from(self.rate_per_sec));
        Duration::from_nanos(u64::try_from(wait_ns).unwrap_or(u64::MAX))
    }

    #[must_use]
    pub fn available(&self) -> u64 {
        self.available
    }

    #[must_use]
    pub fn rate_per_sec(&self) -> u64 {
        self.rate_per_sec
    }

    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
}

#[derive(Debug, Clone)]
pub struct ThrottleConfig {
    pub idle_workers: usize,
    pub active_workers: usize,
    pub idle_disk_bytes_per_sec: u64,
    pub active_disk_bytes_per_sec: u64,
    pub disk_burst_bytes: u64,
    pub active_cooldown: Duration,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        Self {
            idle_workers: cores.div_ceil(2).max(1),
            active_workers: 1,
            idle_disk_bytes_per_sec: 256 * 1024 * 1024,
            active_disk_bytes_per_sec: 16 * 1024 * 1024,
            disk_burst_bytes: 64 * 1024 * 1024,
            active_cooldown: Duration::from_millis(25),
        }
    }
}

impl ThrottleConfig {
    #[must_use]
    pub fn worker_budget(&self, activity: Activity) -> usize {
        let workers = match activity {
            Activity::Idle => self.idle_workers,
            Activity::UserActive => self.active_workers,
        };
        workers.max(1)
    }

    #[must_use]
    pub fn disk_rate(&self, activity: Activity) -> u64 {
        match activity {
            Activity::Idle => self.idle_disk_bytes_per_sec,
            Activity::UserActive => self.active_disk_bytes_per_sec,
        }
    }

    #[must_use]
    pub fn disk_limiter(&self, activity: Activity, now_ns: i64) -> RateLimiter {
        RateLimiter::new(self.disk_rate(activity), self.disk_burst_bytes, now_ns)
    }

    #[must_use]
    pub fn cooldown(&self, activity: Activity) -> Duration {
        match activity {
            Activity::Idle => Duration::ZERO,
            Activity::UserActive => self.active_cooldown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC_NS: i64 = 1_000_000_000;

    #[test]
    fn limiter_starts_full() {
        let limiter = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(limiter.available(), 1_000);
        assert_eq!(limiter.capacity(), 1_000);
        assert_eq!(limiter.rate_per_sec(), 1_000);
    }

    #[test]
    fn limiter_clamps_zero_rate_and_capacity_to_one() {
        let limiter = RateLimiter::new(0, 0, 0);
        assert_eq!(limiter.rate_per_sec(), 1);
        assert_eq!(limiter.capacity(), 1);
    }

    #[test]
    fn acquire_within_budget_is_immediate_and_consumes() {
        let mut limiter = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(limiter.acquire(400, 0), Duration::ZERO);
        assert_eq!(limiter.available(), 600);
        assert_eq!(limiter.acquire(600, 0), Duration::ZERO);
        assert_eq!(limiter.available(), 0);
    }

    #[test]
    fn acquire_beyond_budget_returns_exact_wait_without_consuming() {
        let mut limiter = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(limiter.acquire(1_000, 0), Duration::ZERO);
        assert_eq!(limiter.acquire(500, 0), Duration::from_millis(500));
        assert_eq!(limiter.available(), 0);
    }

    #[test]
    fn tokens_refill_over_time_up_to_capacity() {
        let mut limiter = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(limiter.acquire(1_000, 0), Duration::ZERO);
        assert_eq!(limiter.acquire(250, SEC_NS / 4), Duration::ZERO);
        assert_eq!(limiter.available(), 0);
        let mut full = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(full.acquire(1_000, 0), Duration::ZERO);
        full.acquire(0, 10 * SEC_NS);
        assert_eq!(full.available(), 1_000);
    }

    #[test]
    fn partial_refill_sizes_the_next_wait() {
        let mut limiter = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(limiter.acquire(1_000, 0), Duration::ZERO);
        assert_eq!(limiter.acquire(251, SEC_NS / 4), Duration::from_millis(1));
    }

    #[test]
    fn oversized_cost_passes_once_bucket_is_full() {
        let mut limiter = RateLimiter::new(1_000, 1_000, 0);
        assert_eq!(limiter.acquire(5_000, 0), Duration::ZERO);
        assert_eq!(limiter.available(), 0);
    }

    #[test]
    fn worker_budget_throttles_harder_when_active() {
        let cfg = ThrottleConfig {
            idle_workers: 8,
            active_workers: 2,
            ..ThrottleConfig::default()
        };
        assert_eq!(cfg.worker_budget(Activity::Idle), 8);
        assert_eq!(cfg.worker_budget(Activity::UserActive), 2);
    }

    #[test]
    fn worker_budget_is_never_zero() {
        let cfg = ThrottleConfig {
            idle_workers: 0,
            active_workers: 0,
            ..ThrottleConfig::default()
        };
        assert_eq!(cfg.worker_budget(Activity::Idle), 1);
        assert_eq!(cfg.worker_budget(Activity::UserActive), 1);
    }

    #[test]
    fn disk_rate_is_lower_when_active() {
        let cfg = ThrottleConfig::default();
        assert!(cfg.disk_rate(Activity::UserActive) < cfg.disk_rate(Activity::Idle));
    }

    #[test]
    fn cooldown_is_zero_when_idle_and_set_when_active() {
        let cfg = ThrottleConfig {
            active_cooldown: Duration::from_millis(40),
            ..ThrottleConfig::default()
        };
        assert_eq!(cfg.cooldown(Activity::Idle), Duration::ZERO);
        assert_eq!(
            cfg.cooldown(Activity::UserActive),
            Duration::from_millis(40)
        );
    }

    #[test]
    fn disk_limiter_is_sized_for_the_activity() {
        let cfg = ThrottleConfig {
            active_disk_bytes_per_sec: 7_777,
            disk_burst_bytes: 9_999,
            ..ThrottleConfig::default()
        };
        let limiter = cfg.disk_limiter(Activity::UserActive, 0);
        assert_eq!(limiter.rate_per_sec(), 7_777);
        assert_eq!(limiter.capacity(), 9_999);
    }

    #[test]
    fn default_idle_budget_is_at_least_one() {
        let cfg = ThrottleConfig::default();
        assert!(cfg.idle_workers >= 1);
        assert_eq!(cfg.active_workers, 1);
    }

    #[test]
    fn cpu_throttle_cooldown_is_zero_for_full_and_grows_with_strictness() {
        assert_eq!(cpu_throttle_cooldown(CpuThrottle::Full), Duration::ZERO);
        let balanced = cpu_throttle_cooldown(CpuThrottle::Balanced);
        let eco = cpu_throttle_cooldown(CpuThrottle::Eco);
        assert!(
            Duration::ZERO < balanced && balanced < eco,
            "stricter settings pause longer: full=0 < balanced={balanced:?} < eco={eco:?}",
        );
    }

    #[test]
    fn throttle_control_default_only_passes_through_the_activity_cooldown() {
        let c = ThrottleControl::default();
        assert!(!c.is_max_performance());
        assert_eq!(c.effective_cooldown(Duration::ZERO), Duration::ZERO);
        assert_eq!(
            c.effective_cooldown(Duration::from_millis(25)),
            Duration::from_millis(25),
        );
    }

    #[test]
    fn throttle_control_eco_floors_the_cooldown_even_when_idle() {
        let c = ThrottleControl::default();
        c.set_level(CpuThrottle::Eco);
        assert!(!c.is_max_performance());
        assert_eq!(
            c.effective_cooldown(Duration::ZERO),
            cpu_throttle_cooldown(CpuThrottle::Eco),
        );
        assert_eq!(
            c.effective_cooldown(Duration::from_millis(1)),
            cpu_throttle_cooldown(CpuThrottle::Eco),
        );
    }

    #[test]
    fn throttle_control_idle_workers_override_roundtrips_and_clears() {
        let c = ThrottleControl::default();
        assert_eq!(c.idle_workers_override(), None);
        c.set_idle_workers(Some(7));
        assert_eq!(c.idle_workers_override(), Some(7));
        c.set_idle_workers(Some(0));
        assert_eq!(c.idle_workers_override(), None);
        c.set_idle_workers(Some(4));
        assert_eq!(c.idle_workers_override(), Some(4));
        c.set_idle_workers(None);
        assert_eq!(c.idle_workers_override(), None);
    }

    #[test]
    fn throttle_control_full_is_max_performance_and_ignores_cooldowns() {
        let c = ThrottleControl::default();
        c.set_level(CpuThrottle::Eco);
        c.set_level(CpuThrottle::Full);
        assert!(c.is_max_performance());
        assert_eq!(
            c.effective_cooldown(Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn throttle_control_level_defaults_to_full_and_roundtrips() {
        let c = ThrottleControl::default();
        assert_eq!(c.level(), CpuThrottle::Full);
        c.set_level(CpuThrottle::Balanced);
        assert_eq!(c.level(), CpuThrottle::Balanced);
        c.set_level(CpuThrottle::Eco);
        assert_eq!(c.level(), CpuThrottle::Eco);
        c.set_level(CpuThrottle::Full);
        assert_eq!(c.level(), CpuThrottle::Full);
    }

    #[test]
    fn cpu_throttle_idle_budget_scales_per_mode_at_each_core_count() {
        for cores in [1usize, 2, 4, 32] {
            let base_idle = cores.div_ceil(2).max(1);
            let full = cpu_throttle_idle_budget(CpuThrottle::Full, base_idle, cores);
            let balanced = cpu_throttle_idle_budget(CpuThrottle::Balanced, base_idle, cores);
            let eco = cpu_throttle_idle_budget(CpuThrottle::Eco, base_idle, cores);
            assert_eq!(full, cores, "Full → all cores at cores={cores}");
            assert_eq!(balanced, base_idle, "Balanced → baseline at cores={cores}");
            assert_eq!(
                eco,
                (base_idle / 2).max(1),
                "Eco → ~quarter at cores={cores}"
            );
            assert!(full >= 1 && balanced >= 1 && eco >= 1);
            assert!(full >= balanced && balanced >= eco);
        }
    }

    #[test]
    fn cpu_throttle_idle_budget_32_core_gradient_is_32_16_8() {
        let base_idle = 32usize.div_ceil(2);
        assert_eq!(
            cpu_throttle_idle_budget(CpuThrottle::Full, base_idle, 32),
            32
        );
        assert_eq!(
            cpu_throttle_idle_budget(CpuThrottle::Balanced, base_idle, 32),
            16
        );
        assert_eq!(cpu_throttle_idle_budget(CpuThrottle::Eco, base_idle, 32), 8);
    }

    fn np(p: &str) -> NormalizedPath {
        NormalizedPath::new(p)
    }

    fn fired(reg: &DecodeRegistration<'_>) -> bool {
        reg.token().load(Ordering::Relaxed)
    }

    #[test]
    fn register_decode_under_removed_root_pre_fires_and_off_root_stays_clear() {
        let c = ThrottleControl::default();
        c.mark_roots_removed(&[np("C:/lib/a")]);
        let under = c.register_decode(&np("C:/lib/a/sub/clip.mp4"));
        assert!(fired(&under), "decode under a removed root must pre-fire");
        let exact = c.register_decode(&np("C:/lib/a"));
        assert!(fired(&exact), "the removed root path itself must pre-fire");
        let off = c.register_decode(&np("C:/lib/b/clip.mp4"));
        assert!(
            !fired(&off),
            "a decode off every removed root must not fire"
        );
        let near = c.register_decode(&np("C:/lib/ab/clip.mp4"));
        assert!(
            !fired(&near),
            "a string-prefix that is not a path segment must not fire"
        );
    }

    #[test]
    fn mark_roots_removed_fires_in_flight_token() {
        let c = ThrottleControl::default();
        let reg = c.register_decode(&np("C:/lib/a/clip.mp4"));
        assert!(!fired(&reg), "decode starts clear before any removal");
        c.mark_roots_removed(&[np("C:/lib/a")]);
        assert!(
            fired(&reg),
            "removing the folder must fire the in-flight token"
        );
    }

    #[test]
    fn deregister_is_by_identity_not_path() {
        let c = ThrottleControl::default();
        let a = c.register_decode(&np("C:/lib/a/clip.mp4"));
        let b = c.register_decode(&np("C:/lib/a/clip.mp4"));
        drop(a);
        c.mark_roots_removed(&[np("C:/lib/a")]);
        assert!(fired(&b), "the surviving same-path decode must still fire");
        drop(b);
        assert_eq!(
            c.active_decodes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            0,
            "every guard's Drop must deregister its own token by identity",
        );
    }

    #[test]
    fn clear_removed_root_overlap_unblocks_a_readded_folder() {
        let c = ThrottleControl::default();
        c.mark_roots_removed(&[np("C:/lib/a")]);
        c.clear_removed_root_overlap(&np("C:/lib/a/c"));
        let child = c.register_decode(&np("C:/lib/a/c/clip.mp4"));
        assert!(
            !fired(&child),
            "a re-added child must not be pre-fired by a stale parent"
        );
        c.mark_roots_removed(&[np("C:/lib/d/e")]);
        c.clear_removed_root_overlap(&np("C:/lib/d"));
        let parent = c.register_decode(&np("C:/lib/d/e/clip.mp4"));
        assert!(
            !fired(&parent),
            "a re-added parent must clear a stale child root"
        );
    }

    #[test]
    fn is_path_removed_gate() {
        let c = ThrottleControl::default();
        c.mark_roots_removed(&[np("C:/videos")]);

        assert!(c.is_path_removed(&np("C:/videos")), "exact root match");
        assert!(c.is_path_removed(&np("C:/videos/clip.mp4")), "child path");
        assert!(
            c.is_path_removed(&np("C:/videos/sub/a.mp4")),
            "nested child"
        );
        assert!(
            !c.is_path_removed(&np("C:/other/clip.mp4")),
            "unrelated path"
        );
        assert!(
            !c.is_path_removed(&np("C:/videosbak/clip.mp4")),
            "prefix-only, not a subpath"
        );

        c.clear_removed_root_overlap(&np("C:/videos"));
        assert!(
            !c.is_path_removed(&np("C:/videos/clip.mp4")),
            "re-added root clears removal"
        );
    }
}
