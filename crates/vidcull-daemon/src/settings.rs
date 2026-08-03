use std::path::PathBuf;

use vidcull_core::{Error, Result};
use vidcull_db::Database;
use vidcull_db::repo::{DaemonSettingsRepo, SystemMetadataRepo};
use vidcull_ipc::DaemonSettings;

const PARTIAL_CLIPS_DEFAULT_ON_KEY: &str = "partial_clips_default_on_migrated";

#[must_use]
pub fn data_dir() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local).join("vidcull")
    } else {
        std::env::temp_dir().join("vidcull")
    }
}

#[must_use]
pub fn load(db: &Database) -> DaemonSettings {
    let mut settings = match DaemonSettingsRepo::new(db.conn()).load() {
        Ok(Some(bytes)) => match postcard::from_bytes(&bytes) {
            Ok(settings) => settings,
            Err(err) => {
                tracing::warn!(error = %err, "corrupt daemon_settings blob; using defaults");
                DaemonSettings::default()
            }
        },
        Ok(None) => DaemonSettings::default(),
        Err(err) => {
            tracing::warn!(error = %err, "could not read daemon_settings; using defaults");
            DaemonSettings::default()
        }
    };
    settings.cpu_cores = live_cores();
    settings
}

#[must_use]
pub fn ensure_system_excludes(rules: &mut Vec<String>) -> bool {
    let mut added = false;
    for sys in ["$RECYCLE.BIN", "System Volume Information"] {
        if !rules.iter().any(|r| r.eq_ignore_ascii_case(sys)) {
            rules.push(sys.to_owned());
            added = true;
        }
    }
    added
}

#[must_use]
pub fn live_cores() -> u32 {
    u32::try_from(
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1),
    )
    .unwrap_or(u32::MAX)
}

#[must_use]
pub fn clamp_idle_workers(pick: Option<u32>) -> Option<usize> {
    let cores = usize::try_from(live_cores()).unwrap_or(usize::MAX).max(1);
    pick.map(|n| usize::try_from(n).unwrap_or(usize::MAX).clamp(1, cores))
}

pub fn save(db: &Database, settings: &DaemonSettings) -> Result<()> {
    for folder in &settings.scan_folders {
        if !std::path::Path::new(folder).is_absolute() {
            return Err(Error::Unsupported(format!(
                "scan_folders entry is not an absolute path: {folder:?}"
            )));
        }
    }
    let bytes = postcard::to_allocvec(settings)
        .map_err(|e| Error::Database(format!("settings encode error: {e}")))?;
    DaemonSettingsRepo::new(db.conn()).save(&bytes)
}

pub fn migrate_partial_clips_default_on(db: &Database, settings: &mut DaemonSettings) -> bool {
    let meta = SystemMetadataRepo::new(db.conn());
    match meta.contains(PARTIAL_CLIPS_DEFAULT_ON_KEY) {
        Ok(true) => return false,
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(error = %err, "could not read partial-clips migration marker; skipping");
            return false;
        }
    }
    let flipped = !settings.partial_clips_enabled;
    settings.partial_clips_enabled = true;
    if flipped {
        if let Err(err) = save(db, settings) {
            tracing::warn!(error = %err, "could not persist partial-clips default-on migration");
            return false;
        }
    }
    if let Err(err) = meta.set(PARTIAL_CLIPS_DEFAULT_ON_KEY, "1") {
        tracing::warn!(error = %err, "could not record partial-clips migration marker");
    }
    flipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use vidcull_ipc::CpuThrottle;

    fn mem_db() -> Database {
        vidcull_db::open_in_memory().expect("open in-memory db")
    }

    fn loaded_default() -> DaemonSettings {
        DaemonSettings {
            cpu_cores: live_cores(),
            ..DaemonSettings::default()
        }
    }

    #[test]
    fn load_on_fresh_db_returns_default() {
        let db = mem_db();
        assert_eq!(load(&db), loaded_default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let db = mem_db();
        let settings = DaemonSettings {
            scan_folders: vec!["C:/videos".into(), "D:/archive".into()],
            background_enabled: false,
            auto_index: true,
            exclude_rules: vec!["node_modules".into()],
            run_on_boot: true,
            cpu_throttle: CpuThrottle::Eco,
            best_copy_mode: vidcull_ipc::BestCopyMode::SpaceSaving,
            idle_worker_count: Some(3),
            cpu_cores: live_cores(),
            partial_clips_enabled: true,
            indexing_enabled: false,
        };
        save(&db, &settings).expect("save");
        assert_eq!(load(&db), settings);
    }

    #[test]
    fn save_overwrites_previous_value() {
        let db = mem_db();
        save(&db, &DaemonSettings::default()).expect("first save");
        let updated = DaemonSettings {
            scan_folders: vec!["E:/clips".into()],
            cpu_throttle: CpuThrottle::Balanced,
            idle_worker_count: Some(1),
            ..DaemonSettings::default()
        };
        save(&db, &updated).expect("second save");
        assert_eq!(
            load(&db),
            DaemonSettings {
                cpu_cores: live_cores(),
                ..updated
            }
        );
    }

    #[test]
    fn corrupt_blob_degrades_to_default() {
        let db = mem_db();
        DaemonSettingsRepo::new(db.conn())
            .save(b"\xff\xff not a settings blob \x00\x01")
            .expect("save raw");
        assert_eq!(load(&db), loaded_default());
    }

    #[test]
    fn load_always_stamps_live_cpu_cores() {
        let db = mem_db();
        save(
            &db,
            &DaemonSettings {
                cpu_cores: 99_999,
                ..DaemonSettings::default()
            },
        )
        .expect("save");
        assert_eq!(load(&db).cpu_cores, live_cores());
    }

    #[test]
    fn clamp_idle_workers_bounds_to_cores_and_passes_none() {
        let cores = usize::try_from(live_cores()).unwrap_or(usize::MAX).max(1);
        assert_eq!(clamp_idle_workers(None), None);
        assert_eq!(clamp_idle_workers(Some(0)), Some(1));
        assert_eq!(clamp_idle_workers(Some(1)), Some(1));
        assert_eq!(clamp_idle_workers(Some(live_cores())), Some(cores),);
        assert_eq!(clamp_idle_workers(Some(u32::MAX)), Some(cores));
    }

    #[test]
    fn live_cores_is_at_least_one() {
        assert!(live_cores() >= 1);
    }

    #[test]
    fn save_rejects_relative_scan_folder() {
        let db = mem_db();
        let settings = DaemonSettings {
            scan_folders: vec!["relative/path".into()],
            ..DaemonSettings::default()
        };
        let err = save(&db, &settings).expect_err("relative path must be rejected");
        assert!(
            matches!(err, vidcull_core::Error::Unsupported(_)),
            "expected Error::Unsupported, got {err:?}"
        );
    }

    #[test]
    fn save_accepts_absolute_scan_folders() {
        let db = mem_db();
        let settings = DaemonSettings {
            scan_folders: vec!["C:/videos".into(), "D:/archive".into()],
            ..DaemonSettings::default()
        };
        save(&db, &settings).expect("absolute paths must be accepted");
        let loaded = load(&db);
        assert_eq!(loaded.scan_folders, settings.scan_folders);
    }

    #[test]
    fn default_partial_clips_is_on() {
        assert!(DaemonSettings::default().partial_clips_enabled);
        let db = mem_db();
        assert!(load(&db).partial_clips_enabled, "fresh db loads ON");
    }

    #[test]
    fn migrate_partial_clips_flips_existing_off_row_to_on() {
        let db = mem_db();
        save(
            &db,
            &DaemonSettings {
                partial_clips_enabled: false,
                ..DaemonSettings::default()
            },
        )
        .expect("seed off row");
        let mut settings = load(&db);
        assert!(!settings.partial_clips_enabled, "seeded row loads OFF");
        let flipped = migrate_partial_clips_default_on(&db, &mut settings);
        assert!(flipped, "an OFF row must be flipped on first migration");
        assert!(settings.partial_clips_enabled, "in-memory settings now ON");
        assert!(
            load(&db).partial_clips_enabled,
            "row persisted ON for next boot"
        );
    }

    #[test]
    fn migrate_partial_clips_is_idempotent_and_reversible() {
        let db = mem_db();
        save(
            &db,
            &DaemonSettings {
                partial_clips_enabled: false,
                ..DaemonSettings::default()
            },
        )
        .expect("seed off row");
        let mut first = load(&db);
        assert!(
            migrate_partial_clips_default_on(&db, &mut first),
            "first flip"
        );
        save(
            &db,
            &DaemonSettings {
                partial_clips_enabled: false,
                ..first
            },
        )
        .expect("user turns it off");
        let mut second = load(&db);
        assert!(!second.partial_clips_enabled, "user OFF choice loaded");
        let flipped = migrate_partial_clips_default_on(&db, &mut second);
        assert!(!flipped, "marker prevents a second flip");
        assert!(
            !second.partial_clips_enabled,
            "user OFF choice preserved in memory"
        );
        assert!(
            !load(&db).partial_clips_enabled,
            "row stays OFF (reversible)"
        );
    }

    #[test]
    fn migrate_partial_clips_on_fresh_db_sets_marker_only() {
        let db = mem_db();
        let mut settings = load(&db);
        assert!(settings.partial_clips_enabled, "fresh default is ON");
        let flipped = migrate_partial_clips_default_on(&db, &mut settings);
        assert!(!flipped, "fresh db needs no flip (default already ON)");
        assert!(settings.partial_clips_enabled, "stays ON");
        save(
            &db,
            &DaemonSettings {
                partial_clips_enabled: false,
                ..settings
            },
        )
        .expect("user turns it off later");
        let mut later = load(&db);
        let flipped_again = migrate_partial_clips_default_on(&db, &mut later);
        assert!(!flipped_again, "marker set on fresh db prevents re-flip");
        assert!(
            !later.partial_clips_enabled,
            "user OFF preserved after fresh-db marker"
        );
    }
}
