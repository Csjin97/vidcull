use std::path::{Path, PathBuf};

use vidcull_core::Result;

pub const KEEP_SNAPSHOTS: usize = 3;

#[must_use]
pub fn default_backup_dir() -> PathBuf {
    crate::settings::data_dir().join("backups")
}

pub fn snapshot_into(db: &vidcull_db::Database, dir: &Path, now: i64) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(vidcull_core::Error::Io)?;

    let dest = unique_dest(dir, now);

    db.vacuum_into(&dest)?;

    prune(dir);

    Ok(dest)
}

pub fn prune(dir: &Path) {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_snapshot_name)
            })
            .collect(),
        Err(err) => {
            tracing::warn!(error = %err, dir = %crate::redact::redact_fs_path(dir), "backup prune: could not read dir");
            return;
        }
    };

    entries.sort_by(|a, b| {
        let mtime_a = mtime(a);
        let mtime_b = mtime(b);
        mtime_b
            .cmp(&mtime_a)
            .then_with(|| b.file_name().cmp(&a.file_name()))
    });

    for stale in entries.iter().skip(KEEP_SNAPSHOTS) {
        if let Err(err) = std::fs::remove_file(stale) {
            tracing::warn!(
                error = %err,
                path = %crate::redact::redact_fs_path(stale),
                "backup prune: could not delete old snapshot (best-effort; ignoring)",
            );
        }
    }
}

fn is_snapshot_name(name: &str) -> bool {
    name.starts_with("index-")
        && Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
}

fn unique_dest(dir: &Path, now: i64) -> PathBuf {
    let primary = dir.join(format!("index-{now}.db"));
    if !primary.exists() {
        return primary;
    }
    let mut n: u32 = 1;
    loop {
        let candidate = dir.join(format!("index-{now}-{n}.db"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

fn mtime(path: &Path) -> u64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db() -> vidcull_db::Database {
        vidcull_db::open_in_memory().expect("open in-memory db")
    }

    #[test]
    fn snapshot_creates_readable_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = make_db();
        let expected_version = db.schema_version().expect("schema_version");

        let dest = snapshot_into(&db, dir.path(), 1_700_000_000).expect("snapshot");

        assert!(dest.exists(), "snapshot file must exist");
        let snap = vidcull_db::open_file(&dest).expect("open snapshot");
        assert_eq!(
            snap.schema_version().expect("schema_version on snapshot"),
            expected_version,
            "snapshot schema version must match source",
        );
    }

    #[test]
    fn prune_keeps_only_newest_three() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = make_db();

        for i in 0..4_i64 {
            snapshot_into(&db, dir.path(), 1_700_000_000 + i).expect("snapshot");
        }

        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_str().is_some_and(is_snapshot_name))
            .collect();

        assert_eq!(
            remaining.len(),
            KEEP_SNAPSHOTS,
            "expected exactly {KEEP_SNAPSHOTS} snapshots after prune, got {}",
            remaining.len(),
        );
    }

    #[test]
    fn collision_avoidance_appends_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = make_db();
        let now = 1_700_000_000_i64;

        let first = snapshot_into(&db, dir.path(), now).expect("first snapshot");
        let second = snapshot_into(&db, dir.path(), now).expect("second snapshot");

        assert_ne!(first, second, "collision must produce distinct paths");
        assert!(first.exists(), "first snapshot must exist");
        assert!(second.exists(), "second snapshot must exist");
        assert!(
            second
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("-1."),
            "second file should have -1 suffix, got {:?}",
            second.file_name(),
        );
    }
}
