use std::collections::BTreeSet;

use rusqlite::{Connection, params, params_from_iter};
use vidcull_core::Result;
use vidcull_core::types::FileId;

use crate::connection::map_err;

pub struct RegroupQueueRepo<'a> {
    conn: &'a Connection,
}

impl<'a> RegroupQueueRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn mark(&self, file_id: FileId, enqueued_at: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO regroup_queue (file_id, enqueued_at) VALUES (?1, ?2) \
                 ON CONFLICT(file_id) DO UPDATE SET enqueued_at = excluded.enqueued_at",
            )
            .map_err(map_err)?
            .execute(params![file_id.0, enqueued_at])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn load(&self) -> Result<BTreeSet<FileId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_id FROM regroup_queue")
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(map_err)?;
        let mut set = BTreeSet::new();
        for row in rows {
            set.insert(FileId(row.map_err(map_err)?));
        }
        Ok(set)
    }

    pub fn len(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM regroup_queue", [], |row| row.get(0))
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn clear<I>(&self, ids: I) -> Result<usize>
    where
        I: IntoIterator<Item = FileId>,
    {
        const CHUNK: usize = 512;
        let ids: Vec<i64> = ids.into_iter().map(|id| id.0).collect();
        let mut removed = 0;
        for chunk in ids.chunks(CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("DELETE FROM regroup_queue WHERE file_id IN ({placeholders})");
            removed += self
                .conn
                .execute(&sql, params_from_iter(chunk.iter()))
                .map_err(map_err)?;
        }
        Ok(removed)
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
    fn clear_removes_all_marked_across_chunk_boundary() {
        let db = open_in_memory().unwrap();
        let repo = RegroupQueueRepo::new(db.conn());
        let ids: Vec<FileId> = (0..600)
            .map(|i| {
                let id = insert_file(db.conn(), &format!("f{i}.mp4"));
                repo.mark(id, 0).unwrap();
                id
            })
            .collect();
        assert_eq!(repo.len().unwrap(), 600);

        let removed = repo.clear(ids).unwrap();
        assert_eq!(removed, 600);
        assert!(repo.is_empty().unwrap());
    }

    #[test]
    fn clear_only_removes_requested_ids() {
        let db = open_in_memory().unwrap();
        let repo = RegroupQueueRepo::new(db.conn());
        let keep = insert_file(db.conn(), "keep.mp4");
        let drop = insert_file(db.conn(), "drop.mp4");
        repo.mark(keep, 0).unwrap();
        repo.mark(drop, 0).unwrap();

        let removed = repo.clear([drop]).unwrap();
        assert_eq!(removed, 1);
        let remaining: Vec<FileId> = repo.load().unwrap().into_iter().collect();
        assert_eq!(remaining, vec![keep]);
    }

    #[test]
    fn clear_empty_input_is_a_noop() {
        let db = open_in_memory().unwrap();
        let repo = RegroupQueueRepo::new(db.conn());
        assert_eq!(repo.clear(std::iter::empty()).unwrap(), 0);
    }
}
