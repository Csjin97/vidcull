use rusqlite::{Connection, OptionalExtension, Row, params};
use vidcull_core::Result;
use vidcull_core::types::{FileId, NormalizedPath};

use crate::connection::map_err;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub file_id: FileId,
    pub tier1_global: Vec<u8>,
    pub tier2_temporal: Option<Vec<u8>>,
    pub format_version: u32,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialSkipMarker {
    pub reason: String,
    pub size_bytes: i64,
    pub mtime_ns: i64,
}

pub struct FingerprintsRepo<'a> {
    conn: &'a Connection,
}

impl<'a> FingerprintsRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn upsert(&self, fp: &Fingerprint) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO fingerprints (\
                    file_id, tier1_global, tier2_temporal, format_version, created_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(file_id) DO UPDATE SET \
                    tier1_global   = excluded.tier1_global, \
                    tier2_temporal = excluded.tier2_temporal, \
                    format_version = excluded.format_version, \
                    created_at     = excluded.created_at",
            )
            .map_err(map_err)?
            .execute(params![
                fp.file_id.0,
                fp.tier1_global,
                fp.tier2_temporal,
                fp.format_version,
                fp.created_at,
            ])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn list_active_tier1(&self) -> Result<Vec<(FileId, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id, fp.tier1_global FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((FileId(row.get::<_, i64>(0)?), row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_active_tier2(&self) -> Result<Vec<(FileId, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id, fp.tier2_temporal FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.tier2_temporal IS NOT NULL \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((FileId(row.get::<_, i64>(0)?), row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_active_tier2_ids(&self) -> Result<Vec<FileId>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.tier2_temporal IS NOT NULL \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| Ok(FileId(row.get::<_, i64>(0)?)))
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn get_active_tier2(&self, file_id: FileId) -> Result<Option<Vec<u8>>> {
        self.conn
            .prepare_cached(
                "SELECT fp.tier2_temporal FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fp.file_id = ?1 AND fi.deleted_at IS NULL \
                 AND fp.tier2_temporal IS NOT NULL",
            )
            .map_err(map_err)?
            .query_row(params![file_id.0], |row| row.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(map_err)
    }

    pub fn set_partial(&self, file_id: FileId, partial_temporal: &[u8]) -> Result<usize> {
        self.conn
            .prepare_cached(
                "UPDATE fingerprints SET \
                    partial_temporal        = ?2, \
                    partial_skip_reason     = NULL, \
                    partial_skip_size_bytes = NULL, \
                    partial_skip_mtime_ns   = NULL \
                 WHERE file_id = ?1",
            )
            .map_err(map_err)?
            .execute(params![file_id.0, partial_temporal])
            .map_err(map_err)
    }

    pub fn list_active_partial(&self) -> Result<Vec<(FileId, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id, fp.partial_temporal FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.partial_temporal IS NOT NULL \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((FileId(row.get::<_, i64>(0)?), row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_active_partial_ids(&self) -> Result<Vec<FileId>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.partial_temporal IS NOT NULL \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| Ok(FileId(row.get::<_, i64>(0)?)))
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_active_partial_or_skipped(&self) -> Result<Vec<FileId>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL \
                 AND (fp.partial_temporal IS NOT NULL OR fp.partial_skip_reason IS NOT NULL) \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| Ok(FileId(row.get::<_, i64>(0)?)))
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_active_partial_or_skipped_excluding_reason(
        &self,
        exclude_skip_reason: &str,
    ) -> Result<Vec<FileId>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL \
                 AND (fp.partial_temporal IS NOT NULL \
                      OR (fp.partial_skip_reason IS NOT NULL \
                          AND fp.partial_skip_reason <> ?1)) \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![exclude_skip_reason], |row| {
                Ok(FileId(row.get::<_, i64>(0)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn count_partial_skip_by_reason(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.partial_skip_reason, COUNT(*) \
                 FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.partial_skip_reason IS NOT NULL \
                 GROUP BY fp.partial_skip_reason \
                 ORDER BY fp.partial_skip_reason ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn get_active_partial(&self, file_id: FileId) -> Result<Option<Vec<u8>>> {
        self.conn
            .prepare_cached(
                "SELECT fp.partial_temporal FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fp.file_id = ?1 AND fi.deleted_at IS NULL \
                 AND fp.partial_temporal IS NOT NULL",
            )
            .map_err(map_err)?
            .query_row(params![file_id.0], |row| row.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(map_err)
    }

    pub fn set_partial_skip(&self, file_id: FileId, marker: &PartialSkipMarker) -> Result<usize> {
        self.conn
            .prepare_cached(
                "UPDATE fingerprints SET \
                    partial_skip_reason     = ?2, \
                    partial_skip_size_bytes = ?3, \
                    partial_skip_mtime_ns   = ?4 \
                 WHERE file_id = ?1",
            )
            .map_err(map_err)?
            .execute(params![
                file_id.0,
                marker.reason,
                marker.size_bytes,
                marker.mtime_ns,
            ])
            .map_err(map_err)
    }

    pub fn get_partial_skip(&self, file_id: FileId) -> Result<Option<PartialSkipMarker>> {
        self.conn
            .query_row(
                "SELECT partial_skip_reason, partial_skip_size_bytes, partial_skip_mtime_ns \
                 FROM fingerprints \
                 WHERE file_id = ?1 AND partial_skip_reason IS NOT NULL",
                params![file_id.0],
                |row| {
                    Ok(PartialSkipMarker {
                        reason: row.get(0)?,
                        size_bytes: row.get(1)?,
                        mtime_ns: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(map_err)
    }

    pub fn clear_partial_skip(&self, file_id: FileId) -> Result<usize> {
        self.conn
            .prepare_cached(
                "UPDATE fingerprints SET \
                    partial_skip_reason     = NULL, \
                    partial_skip_size_bytes = NULL, \
                    partial_skip_mtime_ns   = NULL \
                 WHERE file_id = ?1",
            )
            .map_err(map_err)?
            .execute(params![file_id.0])
            .map_err(map_err)
    }

    pub fn list_partial_migration_fast_path(&self) -> Result<Vec<(FileId, NormalizedPath)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id, fi.path FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.partial_temporal IS NOT NULL \
                 AND (fi.codec IN ('h264', 'hevc', 'h265') OR fi.codec IS NULL) \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    FileId(row.get::<_, i64>(0)?),
                    NormalizedPath::new(row.get::<_, String>(1)?),
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_partial_migration_non_fast_path(&self) -> Result<Vec<(FileId, i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT fp.file_id, fi.size_bytes, fi.mtime_ns FROM fingerprints fp \
                 INNER JOIN files fi ON fi.id = fp.file_id \
                 WHERE fi.deleted_at IS NULL AND fp.partial_temporal IS NOT NULL \
                 AND fi.codec IS NOT NULL AND fi.codec NOT IN ('h264', 'hevc', 'h265') \
                 ORDER BY fp.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    FileId(row.get::<_, i64>(0)?),
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn clear_partial_and_mark_skip(
        &self,
        file_id: FileId,
        marker: &PartialSkipMarker,
    ) -> Result<usize> {
        self.conn
            .prepare_cached(
                "UPDATE fingerprints SET \
                    partial_temporal        = NULL, \
                    partial_skip_reason     = ?2, \
                    partial_skip_size_bytes = ?3, \
                    partial_skip_mtime_ns   = ?4 \
                 WHERE file_id = ?1",
            )
            .map_err(map_err)?
            .execute(params![
                file_id.0,
                marker.reason,
                marker.size_bytes,
                marker.mtime_ns,
            ])
            .map_err(map_err)
    }

    pub fn get(&self, file_id: FileId) -> Result<Option<Fingerprint>> {
        self.conn
            .prepare_cached(
                "SELECT file_id, tier1_global, tier2_temporal, format_version, created_at \
                 FROM fingerprints WHERE file_id = ?1",
            )
            .map_err(map_err)?
            .query_row(params![file_id.0], row_to_fingerprint)
            .optional()
            .map_err(map_err)
    }

    pub fn delete(&self, file_id: FileId) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM fingerprints WHERE file_id = ?1")
            .map_err(map_err)?
            .execute(params![file_id.0])
            .map_err(map_err)?;
        Ok(())
    }
}

fn row_to_fingerprint(row: &Row<'_>) -> rusqlite::Result<Fingerprint> {
    Ok(Fingerprint {
        file_id: FileId(row.get("file_id")?),
        tier1_global: row.get("tier1_global")?,
        tier2_temporal: row.get("tier2_temporal")?,
        format_version: row.get("format_version")?,
        created_at: row.get("created_at")?,
    })
}
