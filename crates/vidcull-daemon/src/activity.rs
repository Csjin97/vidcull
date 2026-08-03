use std::time::Duration;

use crate::throttle::Activity;

pub const USER_IDLE_THRESHOLD: Duration = Duration::from_secs(60);

#[must_use]
pub fn idle_duration() -> Option<Duration> {
    imp::idle_duration()
}

#[must_use]
pub fn current() -> Activity {
    current_with_threshold(USER_IDLE_THRESHOLD)
}

#[must_use]
pub fn current_with_threshold(threshold: Duration) -> Activity {
    match idle_duration() {
        Some(idle) if idle < threshold => Activity::UserActive,
        _ => Activity::Idle,
    }
}

#[cfg(windows)]
mod imp {
    use std::mem::size_of;
    use std::time::Duration;

    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    pub fn idle_duration() -> Option<Duration> {
        let mut info = LASTINPUTINFO {
            cbSize: u32::try_from(size_of::<LASTINPUTINFO>()).unwrap_or(0),
            dwTime: 0,
        };
        #[allow(unsafe_code)]
        let ok = unsafe { GetLastInputInfo(&raw mut info) };
        if ok == 0 {
            return None;
        }
        #[allow(unsafe_code)]
        let now = unsafe { GetTickCount() };
        let idle_ms = now.wrapping_sub(info.dwTime);
        Some(Duration::from_millis(u64::from(idle_ms)))
    }
}

#[cfg(not(windows))]
mod imp {
    use std::time::Duration;

    pub fn idle_duration() -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_falls_back_to_idle_when_undetectable() {
        assert_eq!(current_with_threshold(Duration::ZERO), Activity::Idle);
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_a_non_negative_idle_duration() {
        let idle = idle_duration().expect("windows reports idle time");
        let _ = idle;
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_has_no_idle_source() {
        assert!(idle_duration().is_none());
    }
}
