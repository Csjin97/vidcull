use rusqlite::{Connection, Row, params};
use vidcull_core::Result;
use vidcull_core::types::FileId;

use crate::connection::map_err;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneHash {
    pub id: i64,
    pub file_id: FileId,
    pub ts_ms: i64,
    pub phash: Vec<u8>,
    pub band_index: i64,
}

pub struct SceneHashesRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SceneHashesRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, hash: &SceneHash) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO scene_hashes (file_id, ts_ms, phash, band_index) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![hash.file_id.0, hash.ts_ms, hash.phash, hash.band_index],
            )
            .map_err(map_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_for_file(&self, file_id: FileId) -> Result<Vec<SceneHash>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, file_id, ts_ms, phash, band_index \
                 FROM scene_hashes WHERE file_id = ?1 ORDER BY ts_ms ASC, id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![file_id.0], row_to_scene_hash)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }
}

fn row_to_scene_hash(row: &Row<'_>) -> rusqlite::Result<SceneHash> {
    Ok(SceneHash {
        id: row.get("id")?,
        file_id: FileId(row.get("file_id")?),
        ts_ms: row.get("ts_ms")?,
        phash: row.get("phash")?,
        band_index: row.get("band_index")?,
    })
}
