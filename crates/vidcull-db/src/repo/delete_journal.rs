use rusqlite::{Connection, OptionalExtension, params};
use vidcull_core::Result;
use vidcull_core::types::FileId;

use crate::connection::map_err;
use crate::repo::duplicate_groups::TrustLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBatchMode {
    Trash,
    Permanent,
}

impl DeleteBatchMode {
    pub(super) fn as_text(self) -> &'static str {
        match self {
            Self::Trash => "TRASH",
            Self::Permanent => "PERMANENT",
        }
    }

    pub(super) fn from_text(s: &str) -> Result<Self> {
        match s {
            "TRASH" => Ok(Self::Trash),
            "PERMANENT" => Ok(Self::Permanent),
            other => Err(vidcull_core::Error::Database(format!(
                "unknown delete_batch mode `{other}`; expected TRASH/PERMANENT",
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchFileRole {
    Deleted,
    Survivor,
}

impl BatchFileRole {
    pub(super) fn as_text(self) -> &'static str {
        match self {
            Self::Deleted => "DELETED",
            Self::Survivor => "SURVIVOR",
        }
    }

    pub(super) fn from_text(s: &str) -> Result<Self> {
        match s {
            "DELETED" => Ok(Self::Deleted),
            "SURVIVOR" => Ok(Self::Survivor),
            other => Err(vidcull_core::Error::Database(format!(
                "unknown batch_file role `{other}`; expected DELETED/SURVIVOR",
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteBatchFile {
    pub file_id: FileId,
    pub path: String,
    pub role: BatchFileRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteBatch {
    pub id: i64,
    pub group_id: i64,
    pub trust_level: TrustLevel,
    pub non_transitive: bool,
    pub best_file_id: Option<FileId>,
    pub group_dropped: bool,
    pub mode: DeleteBatchMode,
    pub created_at: i64,
    pub files: Vec<DeleteBatchFile>,
}

pub struct NewDeleteBatch<'x> {
    pub group_id: i64,
    pub trust_level: TrustLevel,
    pub non_transitive: bool,
    pub best_file_id: Option<FileId>,
    pub group_dropped: bool,
    pub mode: DeleteBatchMode,
    pub files: &'x [(FileId, String, BatchFileRole)],
    pub created_at: i64,
}

pub struct DeleteJournalRepo<'a> {
    conn: &'a Connection,
}

impl<'a> DeleteJournalRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn record(&self, batch: &NewDeleteBatch<'_>) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO delete_batches \
                     (group_id, trust_level, non_transitive, best_file_id, group_dropped, \
                      mode, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    batch.group_id,
                    batch.trust_level.as_text(),
                    i64::from(batch.non_transitive),
                    batch.best_file_id.map(|f| f.0),
                    i64::from(batch.group_dropped),
                    batch.mode.as_text(),
                    batch.created_at,
                ],
            )
            .map_err(map_err)?;
        let batch_id = self.conn.last_insert_rowid();

        for (file_id, path, role) in batch.files {
            self.conn
                .execute(
                    "INSERT INTO delete_batch_files (batch_id, file_id, path, role) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![batch_id, file_id.0, path, role.as_text()],
                )
                .map_err(map_err)?;
        }

        Ok(batch_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_pending(
        &self,
        group_id: i64,
        trust_level: TrustLevel,
        non_transitive: bool,
        best_file_id: Option<FileId>,
        mode: DeleteBatchMode,
        deleted: &[(FileId, String)],
        created_at: i64,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO delete_batches \
                     (group_id, trust_level, non_transitive, best_file_id, group_dropped, \
                      mode, created_at, status) \
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, 'PENDING')",
                params![
                    group_id,
                    trust_level.as_text(),
                    i64::from(non_transitive),
                    best_file_id.map(|f| f.0),
                    mode.as_text(),
                    created_at,
                ],
            )
            .map_err(map_err)?;
        let batch_id = self.conn.last_insert_rowid();
        for (file_id, path) in deleted {
            self.conn
                .execute(
                    "INSERT INTO delete_batch_files (batch_id, file_id, path, role) \
                     VALUES (?1, ?2, ?3, 'DELETED')",
                    params![batch_id, file_id.0, path],
                )
                .map_err(map_err)?;
        }
        Ok(batch_id)
    }

    pub fn finalize_committed(
        &self,
        batch_id: i64,
        group_dropped: bool,
        files: &[(FileId, String, BatchFileRole)],
    ) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM delete_batch_files WHERE batch_id = ?1",
                params![batch_id],
            )
            .map_err(map_err)?;
        for (file_id, path, role) in files {
            self.conn
                .execute(
                    "INSERT INTO delete_batch_files (batch_id, file_id, path, role) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![batch_id, file_id.0, path, role.as_text()],
                )
                .map_err(map_err)?;
        }
        self.conn
            .execute(
                "UPDATE delete_batches SET status = 'COMMITTED', group_dropped = ?2 WHERE id = ?1",
                params![batch_id, i64::from(group_dropped)],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn list_pending(&self) -> Result<Vec<DeleteBatch>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, group_id, trust_level, non_transitive, best_file_id, group_dropped, \
                        mode, created_at \
                 FROM delete_batches WHERE status = 'PENDING' ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let metas = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>("id")?,
                    row.get::<_, i64>("group_id")?,
                    row.get::<_, String>("trust_level")?,
                    row.get::<_, i64>("non_transitive")?,
                    row.get::<_, Option<i64>>("best_file_id")?,
                    row.get::<_, i64>("group_dropped")?,
                    row.get::<_, String>("mode")?,
                    row.get::<_, i64>("created_at")?,
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        let mut out = Vec::with_capacity(metas.len());
        for (
            id,
            group_id,
            trust_text,
            non_transitive_int,
            best_raw,
            group_dropped_int,
            mode_text,
            created_at,
        ) in metas
        {
            out.push(DeleteBatch {
                id,
                group_id,
                trust_level: TrustLevel::from_text(&trust_text)?,
                non_transitive: non_transitive_int != 0,
                best_file_id: best_raw.map(FileId),
                group_dropped: group_dropped_int != 0,
                mode: DeleteBatchMode::from_text(&mode_text)?,
                created_at,
                files: self.load_files(id)?,
            });
        }
        Ok(out)
    }

    pub fn last(&self) -> Result<Option<DeleteBatch>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, group_id, trust_level, non_transitive, best_file_id, group_dropped, \
                        mode, created_at \
                 FROM delete_batches WHERE status = 'COMMITTED' ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>("id")?,
                        row.get::<_, i64>("group_id")?,
                        row.get::<_, String>("trust_level")?,
                        row.get::<_, i64>("non_transitive")?,
                        row.get::<_, Option<i64>>("best_file_id")?,
                        row.get::<_, i64>("group_dropped")?,
                        row.get::<_, String>("mode")?,
                        row.get::<_, i64>("created_at")?,
                    ))
                },
            )
            .optional()
            .map_err(map_err)?;

        let Some((
            id,
            group_id,
            trust_text,
            non_transitive_int,
            best_raw,
            group_dropped_int,
            mode_text,
            created_at,
        )) = row
        else {
            return Ok(None);
        };

        let trust_level = TrustLevel::from_text(&trust_text)?;
        let mode = DeleteBatchMode::from_text(&mode_text)?;
        let best_file_id = best_raw.map(FileId);
        let group_dropped = group_dropped_int != 0;
        let non_transitive = non_transitive_int != 0;

        let files = self.load_files(id)?;

        Ok(Some(DeleteBatch {
            id,
            group_id,
            trust_level,
            non_transitive,
            best_file_id,
            group_dropped,
            mode,
            created_at,
            files,
        }))
    }

    pub fn remove(&self, batch_id: i64) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM delete_batches WHERE id = ?1",
                params![batch_id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    fn load_files(&self, batch_id: i64) -> Result<Vec<DeleteBatchFile>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT file_id, path, role FROM delete_batch_files \
                 WHERE batch_id = ?1 ORDER BY file_id ASC",
            )
            .map_err(map_err)?;

        let rows = stmt
            .query_map(params![batch_id], |row| {
                Ok((
                    row.get::<_, i64>("file_id")?,
                    row.get::<_, String>("path")?,
                    row.get::<_, String>("role")?,
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;

        rows.into_iter()
            .map(|(fid, path, role_text)| {
                BatchFileRole::from_text(&role_text).map(|role| DeleteBatchFile {
                    file_id: FileId(fid),
                    path,
                    role,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_in_memory;
    use crate::repo::{FilesRepo, NewFile};
    use vidcull_core::types::NormalizedPath;

    fn insert_file(conn: &Connection, path: &str) -> FileId {
        FilesRepo::new(conn)
            .insert(&NewFile {
                path: NormalizedPath::new(path),
                ..Default::default()
            })
            .unwrap()
    }

    #[test]
    fn pending_batch_is_hidden_from_last_until_finalized() {
        let db = open_in_memory().unwrap();
        let repo = DeleteJournalRepo::new(db.conn());
        let f1 = insert_file(db.conn(), "a.mp4");

        let id = repo
            .record_pending(
                7,
                TrustLevel::Exact,
                false,
                Some(f1),
                DeleteBatchMode::Trash,
                &[(f1, "a.mp4".to_owned())],
                100,
            )
            .unwrap();
        assert!(repo.last().unwrap().is_none(), "PENDING is not undoable");
        let pending = repo.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].files.len(), 1);

        let f2 = insert_file(db.conn(), "b.mp4");
        repo.finalize_committed(
            id,
            false,
            &[
                (f1, "a.mp4".to_owned(), BatchFileRole::Deleted),
                (f2, "b.mp4".to_owned(), BatchFileRole::Survivor),
            ],
        )
        .unwrap();
        assert!(repo.list_pending().unwrap().is_empty());
        let last = repo.last().unwrap().expect("committed batch");
        assert_eq!(last.id, id);
        assert_eq!(last.files.len(), 2);
        assert!(!last.group_dropped);
    }

    #[test]
    fn record_writes_a_committed_batch_directly() {
        let db = open_in_memory().unwrap();
        let repo = DeleteJournalRepo::new(db.conn());
        let f1 = insert_file(db.conn(), "a.mp4");
        let id = repo
            .record(&NewDeleteBatch {
                group_id: 1,
                trust_level: TrustLevel::Exact,
                non_transitive: false,
                best_file_id: None,
                group_dropped: false,
                mode: DeleteBatchMode::Trash,
                files: &[(f1, "a.mp4".to_owned(), BatchFileRole::Deleted)],
                created_at: 1,
            })
            .unwrap();
        assert_eq!(repo.last().unwrap().unwrap().id, id);
        assert!(repo.list_pending().unwrap().is_empty());
    }
}
