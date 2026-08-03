use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use vidcull_core::Result;
use vidcull_core::types::FileId;

use crate::connection::map_err;

const IN_BATCH: usize = 800;

/// SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is a few thousand; each
/// posting binds 4 params, so this keeps a batch comfortably under that
/// limit while still cutting statement-prepare overhead by ~200x versus
/// one `INSERT` per posting.
const INSERT_BATCH: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MihPosting {
    pub chunk: u32,
    pub slice_value: u64,
    pub file_id: FileId,
    pub scene_index: usize,
}

pub struct PartialMihRepo<'a> {
    conn: &'a Connection,
}

impl<'a> PartialMihRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert_posting(&self, posting: &MihPosting) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO partial_mih_postings \
                 (chunk, slice_value, file_id, scene_index) VALUES (?1, ?2, ?3, ?4)",
                params![
                    posting.chunk,
                    to_i64(posting.slice_value),
                    posting.file_id.0,
                    to_i64_usize(posting.scene_index),
                ],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Same as [`Self::insert_posting`] but for many postings at once: one
    /// multi-row `INSERT` per [`INSERT_BATCH`]-sized slice instead of one
    /// prepare+execute per posting (a single file's scenes can produce
    /// hundreds of postings during a cold rebuild).
    pub fn insert_postings(&self, postings: &[MihPosting]) -> Result<()> {
        for batch in postings.chunks(INSERT_BATCH) {
            if batch.is_empty() {
                continue;
            }
            let placeholders = vec!["(?, ?, ?, ?)"; batch.len()].join(",");
            let sql = format!(
                "INSERT OR IGNORE INTO partial_mih_postings \
                 (chunk, slice_value, file_id, scene_index) VALUES {placeholders}",
            );
            let mut binds: Vec<i64> = Vec::with_capacity(batch.len() * 4);
            for posting in batch {
                binds.push(i64::from(posting.chunk));
                binds.push(to_i64(posting.slice_value));
                binds.push(posting.file_id.0);
                binds.push(to_i64_usize(posting.scene_index));
            }
            let mut stmt = self.conn.prepare_cached(&sql).map_err(map_err)?;
            stmt.execute(rusqlite::params_from_iter(binds))
                .map_err(map_err)?;
        }
        Ok(())
    }

    pub fn delete_file_postings(&self, file_id: FileId) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM partial_mih_postings WHERE file_id = ?1",
                params![file_id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn load_all_postings(&self) -> Result<Vec<MihPosting>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT chunk, slice_value, file_id, scene_index \
                 FROM partial_mih_postings ORDER BY file_id ASC, chunk ASC, scene_index ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(MihPosting {
                    chunk: row.get::<_, i64>(0)?.try_into().unwrap_or(0),
                    slice_value: from_i64(row.get::<_, i64>(1)?),
                    file_id: FileId(row.get::<_, i64>(2)?),
                    scene_index: usize::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                })
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn clear_postings(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM partial_mih_postings", [])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn set_scene_count(&self, file_id: FileId, scene_count: usize) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO partial_index_files (file_id, scene_count) VALUES (?1, ?2) \
                 ON CONFLICT(file_id) DO UPDATE SET scene_count = excluded.scene_count",
                params![file_id.0, to_i64_usize(scene_count)],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn delete_scene_count(&self, file_id: FileId) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM partial_index_files WHERE file_id = ?1",
                params![file_id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn scene_count(&self, file_id: FileId) -> Result<Option<usize>> {
        self.conn
            .query_row(
                "SELECT scene_count FROM partial_index_files WHERE file_id = ?1",
                params![file_id.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_err)
            .map(|opt| opt.map(|n| usize::try_from(n).unwrap_or(0)))
    }

    pub fn load_all_scene_counts(&self) -> Result<Vec<(FileId, usize)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_id, scene_count FROM partial_index_files ORDER BY file_id ASC")
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    FileId(row.get::<_, i64>(0)?),
                    usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn clear_scene_counts(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM partial_index_files", [])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn candidate_files(&self, chunk: u32, slice_values: &[u64]) -> Result<BTreeSet<FileId>> {
        let mut out = BTreeSet::new();
        for batch in slice_values.chunks(IN_BATCH) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT DISTINCT file_id FROM partial_mih_postings \
                 WHERE chunk = ? AND slice_value IN ({placeholders})",
            );
            let mut stmt = self.conn.prepare(&sql).map_err(map_err)?;
            let mut binds: Vec<i64> = Vec::with_capacity(batch.len() + 1);
            binds.push(i64::from(chunk));
            binds.extend(batch.iter().map(|&v| to_i64(v)));
            let rows = stmt
                .query_map(rusqlite::params_from_iter(binds), |row| {
                    Ok(FileId(row.get::<_, i64>(0)?))
                })
                .map_err(map_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_err)?;
            out.extend(rows);
        }
        Ok(out)
    }

    pub fn count_short(&self, min_scenes: usize) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM partial_index_files WHERE scene_count < ?1",
                params![to_i64_usize(min_scenes)],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

fn to_i64(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

fn from_i64(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

fn to_i64_usize(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}
