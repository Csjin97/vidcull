use std::path::Path;

pub const DEFAULT_HDD_IO_BUDGET: usize = 4;

pub const HDD_MODE_ENV: &str = "VIDCULL_HDD_MODE";
pub const HDD_IO_BUDGET_ENV: &str = "VIDCULL_HDD_IO_BUDGET";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    Hdd,
    Ssd,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HddMode {
    Auto,
    Off,
}

fn parse_hdd_mode(raw: Option<&str>) -> HddMode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("off" | "0" | "false" | "no") => HddMode::Off,
        _ => HddMode::Auto,
    }
}

fn hdd_mode() -> HddMode {
    parse_hdd_mode(std::env::var(HDD_MODE_ENV).ok().as_deref())
}

fn parse_hdd_io_budget(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_HDD_IO_BUDGET)
}

fn hdd_io_budget() -> usize {
    parse_hdd_io_budget(std::env::var(HDD_IO_BUDGET_ENV).ok().as_deref())
}

fn reduce_class(classes: impl IntoIterator<Item = StorageClass>) -> StorageClass {
    let mut saw_ssd = false;
    for class in classes {
        match class {
            StorageClass::Hdd => return StorageClass::Hdd,
            StorageClass::Ssd => saw_ssd = true,
            StorageClass::Unknown => {}
        }
    }
    if saw_ssd {
        StorageClass::Ssd
    } else {
        StorageClass::Unknown
    }
}

fn io_budget_cap_for(class: StorageClass, hdd_budget: usize, mode: HddMode) -> usize {
    match (mode, class) {
        (HddMode::Auto, StorageClass::Hdd) => hdd_budget.max(1),
        _ => 0,
    }
}

#[must_use]
pub fn clamp_budget(raw: usize, cap: usize) -> usize {
    if cap == 0 { raw } else { raw.min(cap).max(1) }
}

#[must_use]
pub fn detect_io_budget_cap(scan_folders: &[String]) -> usize {
    let mode = hdd_mode();
    if matches!(mode, HddMode::Off) {
        return 0;
    }
    let class = reduce_class(
        scan_folders
            .iter()
            .filter(|f| !f.trim().is_empty())
            .map(|f| classify_path(Path::new(f))),
    );
    let cap = io_budget_cap_for(class, hdd_io_budget(), mode);
    if cap > 0 {
        tracing::info!(
            cap,
            roots = scan_folders.len(),
            "HDD detected among scan folders — clamping concurrent-I/O worker \
             budget; override with VIDCULL_HDD_MODE=off / \
             VIDCULL_HDD_IO_BUDGET=N",
        );
    }
    cap
}

#[must_use]
pub fn classify_path(path: &Path) -> StorageClass {
    imp::classify_path(path)
}

#[cfg(windows)]
mod imp {
    use std::path::{Component, Path, Prefix};

    use super::StorageClass;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    fn volume_device_path(path: &Path) -> Option<Vec<u16>> {
        let letter = match path.components().next()? {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(byte) | Prefix::VerbatimDisk(byte) => byte,
                _ => return None,
            },
            _ => return None,
        };
        let name = format!("\\\\.\\{}:", (letter as char).to_ascii_uppercase());
        Some(name.encode_utf16().chain(std::iter::once(0)).collect())
    }

    pub(super) fn classify_path(path: &Path) -> StorageClass {
        let Some(volume) = volume_device_path(path) else {
            return StorageClass::Unknown;
        };
        match query_seek_penalty(&volume) {
            Some(true) => StorageClass::Hdd,
            Some(false) => StorageClass::Ssd,
            None => StorageClass::Unknown,
        }
    }

    fn query_seek_penalty(volume: &[u16]) -> Option<bool> {
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileW(
                volume.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0; 1],
        };
        let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR {
            Version: 0,
            Size: 0,
            IncursSeekPenalty: false,
        };
        let mut returned: u32 = 0;
        #[allow(unsafe_code)]
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&raw const query).cast(),
                u32::try_from(std::mem::size_of::<STORAGE_PROPERTY_QUERY>()).unwrap_or(0),
                (&raw mut descriptor).cast(),
                u32::try_from(std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()).unwrap_or(0),
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(handle);
        }
        if ok == 0 || (returned as usize) < std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
            return None;
        }
        Some(descriptor.IncursSeekPenalty)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    use super::StorageClass;

    pub(super) fn classify_path(_path: &Path) -> StorageClass {
        StorageClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdd_mode_off_synonyms_disable() {
        for raw in ["off", "OFF", " off ", "0", "false", "FALSE", "no"] {
            assert_eq!(parse_hdd_mode(Some(raw)), HddMode::Off, "raw={raw:?}");
        }
    }

    #[test]
    fn hdd_mode_defaults_to_auto() {
        assert_eq!(parse_hdd_mode(None), HddMode::Auto);
        for raw in ["auto", "1", "on", "", "garbage"] {
            assert_eq!(parse_hdd_mode(Some(raw)), HddMode::Auto, "raw={raw:?}");
        }
    }

    #[test]
    fn hdd_io_budget_parses_and_floors() {
        assert_eq!(parse_hdd_io_budget(None), DEFAULT_HDD_IO_BUDGET);
        assert_eq!(parse_hdd_io_budget(Some("garbage")), DEFAULT_HDD_IO_BUDGET);
        assert_eq!(parse_hdd_io_budget(Some("0")), DEFAULT_HDD_IO_BUDGET);
        assert_eq!(parse_hdd_io_budget(Some("1")), 1);
        assert_eq!(parse_hdd_io_budget(Some(" 8 ")), 8);
    }

    #[test]
    fn reduce_class_any_hdd_wins() {
        use StorageClass::{Hdd, Ssd, Unknown};
        assert_eq!(
            reduce_class([Ssd, Hdd, Ssd]),
            Hdd,
            "one HDD root clamps all"
        );
        assert_eq!(reduce_class([Unknown, Hdd]), Hdd);
        assert_eq!(reduce_class([Ssd, Ssd]), Ssd);
        assert_eq!(reduce_class([Ssd, Unknown]), Ssd);
        assert_eq!(reduce_class([Unknown, Unknown]), Unknown);
        assert_eq!(
            reduce_class(std::iter::empty()),
            Unknown,
            "empty → no clamp"
        );
    }

    #[test]
    fn io_budget_cap_only_clamps_hdd_in_auto() {
        use StorageClass::{Hdd, Ssd, Unknown};
        assert_eq!(
            io_budget_cap_for(Hdd, 4, HddMode::Auto),
            4,
            "HDD/auto clamps"
        );
        assert_eq!(
            io_budget_cap_for(Ssd, 4, HddMode::Auto),
            0,
            "SSD never clamps"
        );
        assert_eq!(
            io_budget_cap_for(Unknown, 4, HddMode::Auto),
            0,
            "Unknown never clamps"
        );
        assert_eq!(
            io_budget_cap_for(Hdd, 4, HddMode::Off),
            0,
            "Off overrides HDD"
        );
        assert_eq!(
            io_budget_cap_for(Hdd, 0, HddMode::Auto),
            1,
            "cap floored to 1 worker"
        );
    }

    #[test]
    fn clamp_budget_reduces_only_above_cap() {
        assert_eq!(
            clamp_budget(32, 0),
            32,
            "cap 0 = no clamp (SSD path unchanged)"
        );
        assert_eq!(clamp_budget(32, 4), 4, "HDD clamp reduces 32 → 4");
        assert_eq!(clamp_budget(2, 4), 2, "already below cap = untouched");
        assert_eq!(clamp_budget(4, 4), 4);
        assert_eq!(clamp_budget(1, 4), 1);
        assert_eq!(clamp_budget(0, 4), 1, "never spawns zero workers");
    }

    #[test]
    fn classify_path_unclassifiable_inputs_are_unknown() {
        assert_eq!(
            classify_path(Path::new("relative/path")),
            StorageClass::Unknown
        );
        assert_eq!(classify_path(Path::new("")), StorageClass::Unknown);
    }

    #[test]
    fn classify_path_real_absolute_path_does_not_panic() {
        let dir = std::env::temp_dir();
        let class = classify_path(&dir);
        eprintln!("classify_path({}) = {class:?}", dir.display());
        assert!(matches!(
            class,
            StorageClass::Hdd | StorageClass::Ssd | StorageClass::Unknown
        ));
    }
}
