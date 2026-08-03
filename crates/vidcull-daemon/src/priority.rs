use vidcull_core::Result;

pub fn lower_process_priority() -> Result<()> {
    imp::lower()
}

pub fn restore_normal_priority() -> Result<()> {
    imp::restore_normal()
}

#[cfg(windows)]
mod imp {
    use std::io;

    use vidcull_core::{Error, Result};
    use windows_sys::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, NORMAL_PRIORITY_CLASS, SetPriorityClass,
    };

    pub fn lower() -> Result<()> {
        set_class(BELOW_NORMAL_PRIORITY_CLASS)
    }

    pub fn restore_normal() -> Result<()> {
        set_class(NORMAL_PRIORITY_CLASS)
    }

    fn set_class(class: u32) -> Result<()> {
        #[allow(unsafe_code)]
        let ok = unsafe { SetPriorityClass(GetCurrentProcess(), class) };
        if ok == 0 {
            Err(Error::Io(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }
}

#[cfg(all(unix, not(windows)))]
mod imp {
    use std::io;

    use vidcull_core::{Error, Result};

    const BACKGROUND_NICENESS: i32 = 10;

    pub fn lower() -> Result<()> {
        set_niceness(BACKGROUND_NICENESS)
    }

    pub fn restore_normal() -> Result<()> {
        set_niceness(0)
    }

    fn set_niceness(niceness: i32) -> Result<()> {
        #[allow(unsafe_code)]
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS as _, 0, niceness) };
        if rc == -1 {
            Err(Error::Io(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use vidcull_core::Result;

    pub fn lower() -> Result<()> {
        Ok(())
    }

    pub fn restore_normal() -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_priority_succeeds_and_is_idempotent() {
        lower_process_priority().expect("lower priority");
        lower_process_priority().expect("lower priority again");
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_below_normal_after_lowering() {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetPriorityClass,
        };

        lower_process_priority().expect("lower");
        #[allow(unsafe_code)]
        let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
        assert_eq!(class, BELOW_NORMAL_PRIORITY_CLASS);
    }
}
