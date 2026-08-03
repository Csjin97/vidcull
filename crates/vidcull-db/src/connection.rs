use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use vidcull_core::{Error, Result};

use crate::migrations::{LATEST_VERSION, run_pending_migrations};

const BUSY_TIMEOUT_MS: i64 = 5_000;

const CACHE_SIZE_KIB: i64 = -65_536;

const MMAP_SIZE_BYTES: i64 = 256 * 1024 * 1024;

const SLOW_TX_MS: u64 = 500;

pub struct Database {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Database {
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn schema_version(&self) -> Result<u32> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='schema_migrations'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|_| true)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(map_err(other)),
            })?;

        if !exists {
            return Ok(0);
        }

        let version: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        u32::try_from(version)
            .map_err(|_| Error::Database(format!("schema version {version} out of range")))
    }

    pub fn run_migrations(&mut self) -> Result<usize> {
        run_pending_migrations(&mut self.conn, crate::migrations::MIGRATIONS)
    }

    pub fn vacuum_into(&self, dest: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![dest.to_string_lossy().as_ref()])
            .map(|_| ())
            .map_err(map_err)
    }

    pub fn transaction<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let start = std::time::Instant::now();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(map_err)?;
        let outcome = f(&tx)?;
        tx.commit().map_err(map_err)?;
        let elapsed = start.elapsed();
        if elapsed >= std::time::Duration::from_millis(SLOW_TX_MS) {
            tracing::debug!(elapsed_ms = %elapsed.as_millis(), "slow database transaction");
        }
        Ok(outcome)
    }
}

pub fn open_file(path: &Path) -> Result<Database> {
    let conn = Connection::open(path).map_err(map_err)?;
    apply_pragmas(&conn, false)?;
    let mut db = Database {
        conn,
        path: Some(path.to_path_buf()),
    };
    db.run_migrations()?;
    record_target_os(&db.conn)?;
    debug_assert_eq!(db.schema_version()?, LATEST_VERSION);
    tracing::info!(
        schema_version = LATEST_VERSION,
        journal_mode = "WAL",
        busy_timeout_ms = BUSY_TIMEOUT_MS,
        "index database opened",
    );
    Ok(db)
}

pub fn open_in_memory() -> Result<Database> {
    let conn = Connection::open_in_memory().map_err(map_err)?;
    apply_pragmas(&conn, true)?;
    let mut db = Database { conn, path: None };
    db.run_migrations()?;
    record_target_os(&db.conn)?;
    debug_assert_eq!(db.schema_version()?, LATEST_VERSION);
    Ok(db)
}

fn record_target_os(conn: &Connection) -> Result<()> {
    let current_os = std::env::consts::OS;

    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='system_metadata'",
            [],
            |_| Ok(true),
        )
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(other),
        })
        .map_err(map_err)?;

    if !table_exists {
        return Ok(());
    }

    let saved_os: Option<String> = conn
        .query_row(
            "SELECT value FROM system_metadata WHERE key = 'target_os'",
            [],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(map_err)?;

    match saved_os {
        None => {
            conn.execute(
                "INSERT INTO system_metadata (key, value) VALUES ('target_os', ?1)",
                [current_os],
            )
            .map_err(map_err)?;
        }
        Some(os) if os != current_os => {
            tracing::warn!(
                previous_os = %os,
                current_os,
                "database was first indexed on a different OS; floating-point pHash \
                 bit-margins may differ slightly (absorbed by the matcher's Hamming \
                 tolerance). Fingerprints are left intact — re-index explicitly only if \
                 exact bit-reproducibility is required."
            );
            conn.execute(
                "UPDATE system_metadata SET value = ?1 WHERE key = 'target_os'",
                [current_os],
            )
            .map_err(map_err)?;
        }
        _ => {}
    }

    Ok(())
}

fn apply_pragmas(conn: &Connection, in_memory: bool) -> Result<()> {
    if !in_memory {
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(map_err)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::Database(format!(
                "failed to enable WAL mode (got `{mode}`)",
            )));
        }
    }
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(map_err)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(map_err)?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)
        .map_err(map_err)?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)
        .map_err(map_err)?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .map_err(map_err)?;
    if !in_memory {
        conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)
            .map_err(map_err)?;
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn map_err(err: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(ffi, _) = &err {
        if matches!(
            ffi.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            tracing::warn!(
                code = ?ffi.code,
                busy_timeout_ms = BUSY_TIMEOUT_MS,
                "SQLite database busy/locked past busy_timeout — write contention; the operation failed and the caller will retry",
            );
        }
    }
    Error::Database(err.to_string())
}
