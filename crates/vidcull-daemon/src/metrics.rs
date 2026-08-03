use std::sync::{Mutex, PoisonError};
use std::time::Instant;

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessMetrics {
    pub cpu_permille: u32,
    pub rss_bytes: u64,
}

pub struct MetricsCollector {
    inner: Mutex<State>,
}

struct State {
    prev_system: Option<SystemCpuTimes>,
}

#[derive(Debug, Clone, Copy)]
struct SystemCpuTimes {
    idle: u64,
    total: u64,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsCollector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State {
                prev_system: imp::system_cpu_times(),
            }),
        }
    }

    #[must_use]
    pub fn sample(&self) -> ProcessMetrics {
        let current = imp::system_cpu_times();
        let rss_bytes = imp::rss_bytes();

        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        let cpu_permille = system_busy_permille(state.prev_system, current);

        state.prev_system = current;

        ProcessMetrics {
            cpu_permille,
            rss_bytes,
        }
    }
}

fn system_busy_permille(prev: Option<SystemCpuTimes>, current: Option<SystemCpuTimes>) -> u32 {
    let (Some(prev), Some(current)) = (prev, current) else {
        return 0;
    };
    let total_delta = current.total.saturating_sub(prev.total);
    if total_delta == 0 {
        return 0;
    }
    let idle_delta = current.idle.saturating_sub(prev.idle);
    let busy_delta = total_delta.saturating_sub(idle_delta);
    let permille = (u128::from(busy_delta) * 1000 / u128::from(total_delta)).min(1000);
    #[allow(clippy::cast_possible_truncation)]
    let permille = permille as u32;
    permille
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use super::SystemCpuTimes;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    #[allow(clippy::cast_sign_loss)]
    pub fn system_cpu_times() -> Option<SystemCpuTimes> {
        let mut idle = 0i64;
        let mut kernel = 0i64;
        let mut user = 0i64;
        let ok = unsafe {
            GetSystemTimes(
                std::ptr::addr_of_mut!(idle).cast(),
                std::ptr::addr_of_mut!(kernel).cast(),
                std::ptr::addr_of_mut!(user).cast(),
            )
        };
        if ok == 0 {
            return None;
        }
        let total = (kernel as u64).saturating_add(user as u64);
        Some(SystemCpuTimes {
            idle: idle as u64,
            total,
        })
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn rss_bytes() -> u64 {
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                std::ptr::addr_of_mut!(counters),
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        };
        if ok == 0 {
            return 0;
        }
        counters.WorkingSetSize as u64
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod imp {
    use super::SystemCpuTimes;

    pub fn system_cpu_times() -> Option<SystemCpuTimes> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let first = stat.lines().next()?;
        let mut fields = first.split_ascii_whitespace();
        if fields.next()? != "cpu" {
            return None;
        }
        let values: Vec<u64> = fields.filter_map(|f| f.parse::<u64>().ok()).collect();
        if values.len() < 4 {
            return None;
        }
        let total: u64 = values.iter().copied().fold(0u64, u64::saturating_add);
        let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
        Some(SystemCpuTimes { idle, total })
    }

    #[allow(clippy::cast_sign_loss)]
    pub fn rss_bytes() -> u64 {
        let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
            return 0;
        };
        let Some(resident_pages) = statm
            .split_ascii_whitespace()
            .nth(1)
            .and_then(|field| field.parse::<u64>().ok())
        else {
            return 0;
        };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return 0;
        }
        resident_pages.saturating_mul(page_size as u64)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::SystemCpuTimes;

    pub fn system_cpu_times() -> Option<SystemCpuTimes> {
        None
    }

    pub fn rss_bytes() -> u64 {
        0
    }
}

pub struct ThroughputTracker {
    inner: Mutex<ThroughputState>,
}

#[allow(clippy::struct_field_names)]
struct ThroughputState {
    prev_bytes: u64,
    prev_time: Instant,
}

impl Default for ThroughputTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ThroughputTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ThroughputState {
                prev_bytes: 0,
                prev_time: Instant::now(),
            }),
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    #[must_use]
    pub fn update(&self, total_bytes: u64) -> u64 {
        let now = Instant::now();
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let elapsed = now.duration_since(state.prev_time);
        let delta_bytes = total_bytes.saturating_sub(state.prev_bytes);
        let bps = if elapsed.as_millis() > 0 {
            (delta_bytes as f64 / elapsed.as_secs_f64()) as u64
        } else {
            0
        };
        state.prev_bytes = total_bytes;
        state.prev_time = now;
        bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_collector_returns_valid_values() {
        let c = MetricsCollector::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let m = c.sample();
        assert!(m.cpu_permille <= 1000);
        #[cfg(any(windows, target_os = "linux"))]
        assert!(m.rss_bytes > 0, "rss_bytes must be measured, not stubbed");
    }

    fn snap(idle: u64, total: u64) -> SystemCpuTimes {
        SystemCpuTimes { idle, total }
    }

    #[test]
    fn system_busy_permille_computes_idle_complement() {
        let prev = snap(0, 0);

        assert_eq!(
            system_busy_permille(Some(prev), Some(snap(0, 100))),
            1000,
            "no idle progress over the interval is 100% busy",
        );
        assert_eq!(
            system_busy_permille(Some(prev), Some(snap(100, 100))),
            0,
            "all-idle interval is 0% busy",
        );
        assert_eq!(
            system_busy_permille(Some(prev), Some(snap(50, 100))),
            500,
            "half the ticks busy is 500‰",
        );
    }

    #[test]
    fn system_busy_permille_is_zero_without_baseline_or_progress() {
        let s = snap(10, 50);
        assert_eq!(system_busy_permille(None, Some(s)), 0, "no baseline → 0");
        assert_eq!(system_busy_permille(Some(s), None), 0, "unsupported → 0");
        assert_eq!(
            system_busy_permille(Some(s), Some(s)),
            0,
            "no tick progress → 0 (no divide-by-zero)",
        );
    }

    #[test]
    fn system_busy_permille_clamps_anomalous_idle_overshoot() {
        let prev = snap(100, 100);
        let current = snap(250, 200);
        assert_eq!(
            system_busy_permille(Some(prev), Some(current)),
            0,
            "idle overshoot clamps busy to 0, never underflows",
        );
    }

    #[test]
    fn throughput_tracker_basic() {
        let t = ThroughputTracker::new();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let bps = t.update(1_000_000);
        assert!(bps > 0, "expected non-zero throughput, got {bps}");
    }

    #[test]
    fn child_cpu_ticks_are_included_in_system_busy_permille() {
        let prev = snap(800, 1000);
        let current = snap(800, 1200);

        let permille = system_busy_permille(Some(prev), Some(current));
        assert_eq!(
            permille, 1000,
            "child-process busy ticks must be reflected: idle_delta=0, \
             total_delta=200 → 1000‰ (self-only reversion would yield 0‰)"
        );
    }

    #[test]
    fn partial_child_busy_gives_nonzero_system_permille() {
        let prev = snap(0, 0);
        let current = snap(400, 1000);

        let permille = system_busy_permille(Some(prev), Some(current));
        assert_eq!(
            permille, 600,
            "600/1000 busy ticks should yield 600‰; \
             self-only reversion would see daemon-self_delta=0 → 0‰"
        );
    }
}
