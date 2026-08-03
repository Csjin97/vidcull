use rusqlite::{Connection, OptionalExtension, Row, params};
use vidcull_core::Result;
use vidcull_core::types::NormalizedPath;

use crate::connection::map_err;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStateEntry {
    pub root_path: NormalizedPath,
    pub last_scan_at: i64,
    pub cursor: Option<Vec<u8>>,
    pub files_seen: i64,
    pub bytes_seen: i64,
}

pub struct ScanStateRepo<'a> {
    conn: &'a Connection,
}

impl<'a> ScanStateRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn upsert(&self, entry: &ScanStateEntry) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO scan_state (root_path, last_scan_at, cursor, files_seen, bytes_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(root_path) DO UPDATE SET \
                    last_scan_at = excluded.last_scan_at, \
                    cursor       = excluded.cursor, \
                    files_seen   = excluded.files_seen, \
                    bytes_seen   = excluded.bytes_seen",
                params![
                    entry.root_path.as_str(),
                    entry.last_scan_at,
                    entry.cursor,
                    entry.files_seen,
                    entry.bytes_seen,
                ],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn get(&self, root_path: &NormalizedPath) -> Result<Option<ScanStateEntry>> {
        self.conn
            .query_row(
                "SELECT root_path, last_scan_at, cursor, files_seen, bytes_seen \
                 FROM scan_state WHERE root_path = ?1",
                params![root_path.as_str()],
                row_to_entry,
            )
            .optional()
            .map_err(map_err)
    }

    pub fn delete(&self, root_path: &NormalizedPath) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM scan_state WHERE root_path = ?1",
                params![root_path.as_str()],
            )
            .map_err(map_err)?;
        Ok(())
    }
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<ScanStateEntry> {
    Ok(ScanStateEntry {
        root_path: NormalizedPath::new(row.get::<_, String>("root_path")?),
        last_scan_at: row.get("last_scan_at")?,
        cursor: row.get("cursor")?,
        files_seen: row.get("files_seen")?,
        bytes_seen: row.get("bytes_seen")?,
    })
}
