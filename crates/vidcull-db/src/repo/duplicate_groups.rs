use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use vidcull_core::types::{Codec, FileId, Resolution};
use vidcull_core::{Error, Result};

use crate::connection::map_err;
use crate::repo::codec_sql;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrustLevel {
    Exact,
    VeryLikely,
    Possible,
}

impl TrustLevel {
    pub(super) fn as_text(self) -> &'static str {
        match self {
            Self::Exact => "EXACT",
            Self::VeryLikely => "VERY_LIKELY",
            Self::Possible => "POSSIBLE",
        }
    }

    pub(super) fn from_text(s: &str) -> Result<Self> {
        match s {
            "EXACT" => Ok(Self::Exact),
            "VERY_LIKELY" => Ok(Self::VeryLikely),
            "POSSIBLE" => Ok(Self::Possible),
            other => Err(Error::Database(format!(
                "unknown trust_level `{other}`; expected EXACT/VERY_LIKELY/POSSIBLE",
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub id: i64,
    pub trust_level: TrustLevel,
    pub best_file_id: Option<FileId>,
    pub created_at: i64,
    pub updated_at: i64,
    pub non_transitive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupMemberRecord {
    pub file_id: FileId,
    pub resolution: Option<Resolution>,
    pub bitrate_bps: Option<i64>,
    pub codec: Option<Codec>,
    pub container: Option<String>,
    pub size_bytes: i64,
    pub laplacian_variance: Option<f64>,
    pub dct_energy: Option<f64>,
    pub bpp: Option<f64>,
    pub encoder_tags: Option<String>,
}

pub struct DuplicateGroupsRepo<'a> {
    conn: &'a Connection,
}

impl<'a> DuplicateGroupsRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn create(&self, trust_level: TrustLevel, when: i64) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO duplicate_groups (trust_level, created_at, updated_at) \
                 VALUES (?1, ?2, ?2)",
                params![trust_level.as_text(), when],
            )
            .map_err(map_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn create_non_transitive(&self, trust_level: TrustLevel, when: i64) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO duplicate_groups (trust_level, non_transitive, created_at, updated_at) \
                 VALUES (?1, 1, ?2, ?2)",
                params![trust_level.as_text(), when],
            )
            .map_err(map_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<DuplicateGroup>> {
        self.conn
            .query_row(
                "SELECT id, trust_level, best_file_id, created_at, updated_at, non_transitive \
                 FROM duplicate_groups WHERE id = ?1",
                params![id],
                row_to_group,
            )
            .optional()
            .map_err(map_err)
            .and_then(Option::transpose)
    }

    pub fn list_all(&self) -> Result<Vec<DuplicateGroup>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, trust_level, best_file_id, created_at, updated_at, non_transitive \
                 FROM duplicate_groups ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], row_to_group)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        rows.into_iter().collect()
    }

    pub fn set_best(&self, id: i64, best: Option<FileId>, when: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE duplicate_groups SET best_file_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![best.map(|f| f.0), when, id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn add_member(&self, group_id: i64, file_id: FileId) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO duplicate_group_members (group_id, file_id) VALUES (?1, ?2)",
                params![group_id, file_id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn remove_member(&self, group_id: i64, file_id: FileId) -> Result<usize> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM duplicate_group_members WHERE group_id = ?1 AND file_id = ?2",
                params![group_id, file_id.0],
            )
            .map_err(map_err)?;
        Ok(affected)
    }

    pub fn list_members(&self, group_id: i64) -> Result<Vec<FileId>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT file_id FROM duplicate_group_members \
                 WHERE group_id = ?1 ORDER BY file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![group_id], |row| Ok(FileId(row.get(0)?)))
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_all_with_members(&self) -> Result<Vec<(DuplicateGroup, Vec<FileId>)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT g.id, g.trust_level, g.best_file_id, g.created_at, g.updated_at, \
                        g.non_transitive, m.file_id \
                 FROM duplicate_groups g \
                 LEFT JOIN duplicate_group_members m ON m.group_id = g.id \
                 LEFT JOIN files f ON f.id = m.file_id \
                 WHERE m.file_id IS NULL OR f.deleted_at IS NULL \
                 ORDER BY g.id ASC, m.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                let group = row_to_group(row)?;
                let member: Option<i64> = row.get("file_id")?;
                Ok((group, member.map(FileId)))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;

        let mut out: Vec<(DuplicateGroup, Vec<FileId>)> = Vec::new();
        for (group, member) in rows {
            let group = group?;
            match out.last_mut() {
                Some((last, members)) if last.id == group.id => {
                    if let Some(fid) = member {
                        members.push(fid);
                    }
                }
                _ => out.push((group, member.map(|f| vec![f]).unwrap_or_default())),
            }
        }
        Ok(out)
    }

    pub fn list_groups_with_member_records(
        &self,
    ) -> Result<Vec<(DuplicateGroup, Vec<GroupMemberRecord>)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT g.id, g.trust_level, g.best_file_id, g.created_at, g.updated_at, \
                        g.non_transitive, m.file_id, f.width_px, f.height_px, f.bitrate_bps, \
                        f.codec, f.container, f.size_bytes, f.laplacian_variance, \
                        f.dct_energy, f.bpp, f.encoder_tags \
                 FROM duplicate_groups g \
                 LEFT JOIN (duplicate_group_members m JOIN files f \
                            ON f.id = m.file_id AND f.deleted_at IS NULL) \
                        ON m.group_id = g.id \
                 ORDER BY g.id ASC, m.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                let group = row_to_group(row)?;
                let member = row_to_group_member(row)?;
                Ok((group, member))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;

        let mut out: Vec<(DuplicateGroup, Vec<GroupMemberRecord>)> = Vec::new();
        for (group, member) in rows {
            let group = group?;
            match out.last_mut() {
                Some((last, members)) if last.id == group.id => {
                    if let Some(record) = member {
                        members.push(record);
                    }
                }
                _ => out.push((group, member.map(|r| vec![r]).unwrap_or_default())),
            }
        }
        Ok(out)
    }

    pub fn list_page(
        &self,
        trust: Option<TrustLevel>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<DuplicateGroup>> {
        let limit = i64::from(limit);
        let offset = i64::from(offset);
        let rows = if let Some(trust) = trust {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, trust_level, best_file_id, created_at, updated_at, non_transitive \
                     FROM duplicate_groups WHERE trust_level = ?1 \
                     ORDER BY id ASC LIMIT ?2 OFFSET ?3",
                )
                .map_err(map_err)?;
            stmt.query_map(params![trust.as_text(), limit, offset], row_to_group)
                .map_err(map_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_err)?
        } else {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT id, trust_level, best_file_id, created_at, updated_at, non_transitive \
                     FROM duplicate_groups ORDER BY id ASC LIMIT ?1 OFFSET ?2",
                )
                .map_err(map_err)?;
            stmt.query_map(params![limit, offset], row_to_group)
                .map_err(map_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_err)?
        };
        rows.into_iter().collect()
    }

    pub fn member_counts(&self, group_ids: &[i64]) -> Result<HashMap<i64, i64>> {
        if group_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; group_ids.len()].join(",");
        let sql = format!(
            "SELECT group_id, COUNT(*) FROM duplicate_group_members \
             WHERE group_id IN ({placeholders}) GROUP BY group_id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt
            .query_map(params_from_iter(group_ids.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn all_member_sizes(&self) -> Result<Vec<(i64, FileId, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.group_id, m.file_id, f.size_bytes \
                 FROM duplicate_group_members m \
                 JOIN files f ON f.id = m.file_id \
                 ORDER BY m.group_id ASC, m.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    FileId(row.get::<_, i64>(1)?),
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn find_exact_group_containing(&self, file_id: FileId) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT g.id FROM duplicate_groups g \
                 INNER JOIN duplicate_group_members m ON m.group_id = g.id \
                 WHERE m.file_id = ?1 AND g.trust_level = 'EXACT' \
                 LIMIT 1",
                params![file_id.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_err)
    }

    pub fn find_groups_containing(&self, file_id: FileId) -> Result<Vec<DuplicateGroup>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT g.id, g.trust_level, g.best_file_id, g.created_at, g.updated_at, \
                        g.non_transitive \
                 FROM duplicate_groups g \
                 INNER JOIN duplicate_group_members m ON m.group_id = g.id \
                 WHERE m.file_id = ?1 ORDER BY g.id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![file_id.0], row_to_group)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        rows.into_iter().collect()
    }

    pub fn list_exact_group_member_ids(&self) -> Result<Vec<FileId>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT m.file_id \
                 FROM duplicate_group_members m \
                 JOIN duplicate_groups g ON g.id = m.group_id \
                 WHERE g.trust_level = 'EXACT' \
                 ORDER BY m.file_id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| Ok(FileId(row.get::<_, i64>(0)?)))
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM duplicate_groups WHERE id = ?1", params![id])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn create_with_id(
        &self,
        id: i64,
        trust_level: TrustLevel,
        non_transitive: bool,
        when: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO duplicate_groups \
                     (id, trust_level, non_transitive, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id, trust_level.as_text(), i64::from(non_transitive), when],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn add_member_if_absent(&self, group_id: i64, file_id: FileId) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO duplicate_group_members (group_id, file_id) \
                 VALUES (?1, ?2)",
                params![group_id, file_id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn delete_by_trust(&self, trust: TrustLevel) -> Result<usize> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM duplicate_groups WHERE trust_level = ?1",
                params![trust.as_text()],
            )
            .map_err(map_err)?;
        Ok(affected)
    }

    pub fn delete_non_transitive_by_trust(&self, trust: TrustLevel) -> Result<usize> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM duplicate_groups WHERE trust_level = ?1 AND non_transitive = 1",
                params![trust.as_text()],
            )
            .map_err(map_err)?;
        Ok(affected)
    }
}

fn row_to_group(row: &Row<'_>) -> rusqlite::Result<Result<DuplicateGroup>> {
    let id: i64 = row.get("id")?;
    let trust_text: String = row.get("trust_level")?;
    let best: Option<i64> = row.get("best_file_id")?;
    let created_at: i64 = row.get("created_at")?;
    let updated_at: i64 = row.get("updated_at")?;
    let non_transitive: i64 = row.get("non_transitive")?;
    Ok(
        TrustLevel::from_text(&trust_text).map(|trust_level| DuplicateGroup {
            id,
            trust_level,
            best_file_id: best.map(FileId),
            created_at,
            updated_at,
            non_transitive: non_transitive != 0,
        }),
    )
}

fn row_to_group_member(row: &Row<'_>) -> rusqlite::Result<Option<GroupMemberRecord>> {
    let file_id: Option<i64> = row.get("file_id")?;
    let Some(file_id) = file_id else {
        return Ok(None);
    };
    let width_px: Option<i64> = row.get("width_px")?;
    let height_px: Option<i64> = row.get("height_px")?;
    let codec_text: Option<String> = row.get("codec")?;
    Ok(Some(GroupMemberRecord {
        file_id: FileId(file_id),
        resolution: combine_resolution(width_px, height_px)?,
        bitrate_bps: row.get("bitrate_bps")?,
        codec: codec_text.map(|s| codec_sql::from_text(&s)),
        container: row.get("container")?,
        size_bytes: row.get("size_bytes")?,
        laplacian_variance: row.get("laplacian_variance")?,
        dct_energy: row.get("dct_energy")?,
        bpp: row.get("bpp")?,
        encoder_tags: row.get("encoder_tags")?,
    }))
}

fn combine_resolution(w: Option<i64>, h: Option<i64>) -> rusqlite::Result<Option<Resolution>> {
    match (w, h) {
        (None, None) => Ok(None),
        (Some(w), Some(h)) => {
            let width = u32::try_from(w).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    format!("width_px {w} out of u32 range").into(),
                )
            })?;
            let height = u32::try_from(h).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    format!("height_px {h} out of u32 range").into(),
                )
            })?;
            Ok(Some(Resolution::new(width, height)))
        }
        (some_w, some_h) => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            format!(
                "resolution must be both-NULL or both-set; got width={some_w:?}, height={some_h:?}"
            )
            .into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_in_memory;
    use crate::repo::{FilesRepo, NewFile};
    use vidcull_core::types::NormalizedPath;

    fn insert_file(conn: &Connection, path: &str, size: i64) -> FileId {
        FilesRepo::new(conn)
            .insert(&NewFile {
                path: NormalizedPath::new(path),
                size_bytes: size,
                ..Default::default()
            })
            .unwrap()
    }

    #[test]
    fn trust_level_from_text_rejects_unknown() {
        assert!(TrustLevel::from_text("EXACT").is_ok());
        assert!(TrustLevel::from_text("VERY_LIKELY").is_ok());
        assert!(TrustLevel::from_text("POSSIBLE").is_ok());
        assert!(TrustLevel::from_text("UNKNOWN").is_err());
    }

    #[test]
    fn list_page_filters_by_trust_and_pages_in_sql() {
        let db = open_in_memory().unwrap();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let mut exact = Vec::new();
        for _ in 0..3 {
            exact.push(repo.create(TrustLevel::Exact, 0).unwrap());
        }
        for _ in 0..2 {
            repo.create(TrustLevel::Possible, 0).unwrap();
        }

        let page = repo.list_page(None, 2, 1).unwrap();
        assert_eq!(page.len(), 2);
        assert!(page[0].id < page[1].id);

        let exact_page = repo.list_page(Some(TrustLevel::Exact), 10, 0).unwrap();
        assert_eq!(exact_page.len(), 3);
        assert!(
            exact_page
                .iter()
                .all(|g| g.trust_level == TrustLevel::Exact)
        );
        assert_eq!(exact_page.iter().map(|g| g.id).collect::<Vec<_>>(), exact);

        assert!(
            repo.list_page(Some(TrustLevel::Exact), 10, 3)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn list_exact_group_member_ids_dedups_and_ignores_other_trust() {
        let db = open_in_memory().unwrap();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let f1 = insert_file(db.conn(), "exact1.mp4", 10);
        let f2 = insert_file(db.conn(), "exact2.mp4", 20);
        let f3 = insert_file(db.conn(), "very_likely.mp4", 30);
        let f4 = insert_file(db.conn(), "possible.mp4", 40);
        let f5 = insert_file(db.conn(), "dual.mp4", 50);

        let g_exact = repo.create(TrustLevel::Exact, 0).unwrap();
        repo.add_member(g_exact, f1).unwrap();
        repo.add_member(g_exact, f2).unwrap();
        repo.add_member(g_exact, f5).unwrap();

        let g_vl = repo.create(TrustLevel::VeryLikely, 0).unwrap();
        repo.add_member(g_vl, f3).unwrap();

        let g_possible = repo.create(TrustLevel::Possible, 0).unwrap();
        repo.add_member(g_possible, f4).unwrap();
        repo.add_member(g_possible, f5).unwrap();

        let ids = repo.list_exact_group_member_ids().unwrap();
        assert_eq!(
            ids,
            vec![f1, f2, f5],
            "only EXACT members, deduped, ordered by file_id ASC"
        );
    }

    #[test]
    fn member_counts_batches_and_omits_empty_groups() {
        let db = open_in_memory().unwrap();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let g1 = repo.create(TrustLevel::Exact, 0).unwrap();
        let g2 = repo.create(TrustLevel::Exact, 0).unwrap();
        let g3 = repo.create(TrustLevel::Exact, 0).unwrap();
        let f1 = insert_file(db.conn(), "a.mp4", 10);
        let f2 = insert_file(db.conn(), "b.mp4", 20);
        let f3 = insert_file(db.conn(), "c.mp4", 30);
        repo.add_member(g1, f1).unwrap();
        repo.add_member(g1, f2).unwrap();
        repo.add_member(g2, f3).unwrap();

        let counts = repo.member_counts(&[g1, g2, g3]).unwrap();
        assert_eq!(counts.get(&g1).copied(), Some(2));
        assert_eq!(counts.get(&g2).copied(), Some(1));
        assert_eq!(counts.get(&g3), None, "empty group is absent, not zero");

        assert!(repo.member_counts(&[]).unwrap().is_empty());
    }

    #[test]
    fn all_member_sizes_joins_in_one_query_and_keeps_soft_deleted() {
        let db = open_in_memory().unwrap();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let g1 = repo.create(TrustLevel::Exact, 0).unwrap();
        let f1 = insert_file(db.conn(), "a.mp4", 10);
        let f2 = insert_file(db.conn(), "b.mp4", 25);
        repo.add_member(g1, f1).unwrap();
        repo.add_member(g1, f2).unwrap();

        let rows = repo.all_member_sizes().unwrap();
        assert_eq!(rows, vec![(g1, f1, 10), (g1, f2, 25)]);

        db.conn()
            .execute(
                "UPDATE files SET deleted_at = 1 WHERE id = ?1",
                params![f2.0],
            )
            .unwrap();
        let rows = repo.all_member_sizes().unwrap();
        assert_eq!(rows, vec![(g1, f1, 10), (g1, f2, 25)]);
    }

    #[test]
    fn list_all_with_members_matches_list_all_plus_list_members() {
        let db = open_in_memory().unwrap();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let g1 = repo.create(TrustLevel::Exact, 0).unwrap();
        let g2 = repo.create(TrustLevel::Possible, 0).unwrap();
        let _g3 = repo.create(TrustLevel::Exact, 0).unwrap();
        let f1 = insert_file(db.conn(), "a.mp4", 10);
        let f2 = insert_file(db.conn(), "b.mp4", 20);
        let f3 = insert_file(db.conn(), "c.mp4", 30);
        repo.add_member(g1, f2).unwrap();
        repo.add_member(g1, f1).unwrap();
        repo.add_member(g2, f3).unwrap();

        let joined: Vec<(i64, TrustLevel, Vec<FileId>)> = repo
            .list_all_with_members()
            .unwrap()
            .into_iter()
            .map(|(g, m)| (g.id, g.trust_level, m))
            .collect();
        let expected: Vec<(i64, TrustLevel, Vec<FileId>)> = repo
            .list_all()
            .unwrap()
            .into_iter()
            .map(|g| (g.id, g.trust_level, repo.list_members(g.id).unwrap()))
            .collect();
        assert_eq!(joined, expected);

        assert_eq!(joined.len(), 3);
        assert_eq!(joined[0], (g1, TrustLevel::Exact, vec![f1, f2]));
        assert_eq!(joined[1], (g2, TrustLevel::Possible, vec![f3]));
        assert_eq!(joined[2].2, Vec::<FileId>::new(), "memberless group kept");
    }

    #[test]
    fn list_all_with_members_excludes_soft_deleted_members() {
        let db = open_in_memory().unwrap();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let g1 = repo.create(TrustLevel::Exact, 0).unwrap();
        let g2 = repo.create(TrustLevel::Exact, 0).unwrap();
        let g3 = repo.create(TrustLevel::Possible, 0).unwrap();
        let f1 = insert_file(db.conn(), "live.mp4", 10);
        let f2 = insert_file(db.conn(), "gone.mp4", 20);
        let f3 = insert_file(db.conn(), "gone2.mp4", 30);
        let f4 = insert_file(db.conn(), "gone3.mp4", 40);
        repo.add_member(g1, f1).unwrap();
        repo.add_member(g1, f2).unwrap();
        repo.add_member(g2, f3).unwrap();
        repo.add_member(g2, f4).unwrap();

        for f in [f2, f3, f4] {
            db.conn()
                .execute(
                    "UPDATE files SET deleted_at = 1 WHERE id = ?1",
                    params![f.0],
                )
                .unwrap();
        }

        let joined: Vec<(i64, TrustLevel, Vec<FileId>)> = repo
            .list_all_with_members()
            .unwrap()
            .into_iter()
            .map(|(g, m)| (g.id, g.trust_level, m))
            .collect();

        assert_eq!(
            joined,
            vec![
                (g1, TrustLevel::Exact, vec![f1]),
                (g3, TrustLevel::Possible, Vec::<FileId>::new()),
            ],
            "soft-deleted members filtered; all-deleted group dropped; memberless kept",
        );
    }

    fn insert_orphan_member(conn: &Connection, group_id: i64, file_id: i64) {
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO duplicate_group_members (group_id, file_id) VALUES (?1, ?2)",
            params![group_id, file_id],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    }

    fn legacy_member_records(
        groups: &DuplicateGroupsRepo<'_>,
        files: &FilesRepo<'_>,
        group_id: i64,
    ) -> Vec<GroupMemberRecord> {
        let mut out = Vec::new();
        for member in groups.list_members(group_id).unwrap() {
            let Some(record) = files.get(member).unwrap() else {
                continue;
            };
            if record.deleted_at.is_some() {
                continue;
            }
            out.push(GroupMemberRecord {
                file_id: member,
                resolution: record.resolution,
                bitrate_bps: record.bitrate_bps,
                codec: record.codec,
                container: record.container,
                size_bytes: record.size_bytes,
                laplacian_variance: record.laplacian_variance,
                dct_energy: record.dct_energy,
                bpp: record.bpp,
                encoder_tags: record.encoder_tags,
            });
        }
        out
    }

    #[test]
    fn list_groups_with_member_records_matches_legacy_walk_across_fixtures() {
        let db = open_in_memory().unwrap();
        let conn = db.conn();
        let groups = DuplicateGroupsRepo::new(conn);
        let files = FilesRepo::new(conn);

        let g_all_orphan = groups.create(TrustLevel::Exact, 0).unwrap();
        insert_orphan_member(conn, g_all_orphan, 9_001);
        insert_orphan_member(conn, g_all_orphan, 9_002);

        let g_all_deleted = groups.create(TrustLevel::Exact, 0).unwrap();
        let d1 = insert_file(conn, "deleted1.mp4", 10);
        let d2 = insert_file(conn, "deleted2.mp4", 20);
        groups.add_member(g_all_deleted, d1).unwrap();
        groups.add_member(g_all_deleted, d2).unwrap();
        files.mark_deleted(d1, 1).unwrap();
        files.mark_deleted(d2, 1).unwrap();

        let g_memberless = groups.create(TrustLevel::Possible, 0).unwrap();

        let g_mixed = groups.create(TrustLevel::VeryLikely, 0).unwrap();
        let live = insert_file(conn, "live.mp4", 30);
        let gone = insert_file(conn, "gone.mp4", 40);
        groups.add_member(g_mixed, live).unwrap();
        groups.add_member(g_mixed, gone).unwrap();
        files.mark_deleted(gone, 1).unwrap();
        insert_orphan_member(conn, g_mixed, 9_003);

        let g_healthy = groups.create(TrustLevel::Exact, 0).unwrap();
        let h1 = insert_file(conn, "healthy1.mp4", 50);
        let h2 = insert_file(conn, "healthy2.mp4", 60);
        groups.add_member(g_healthy, h1).unwrap();
        groups.add_member(g_healthy, h2).unwrap();

        let all_group_ids = [
            g_all_orphan,
            g_all_deleted,
            g_memberless,
            g_mixed,
            g_healthy,
        ];

        let joined = groups.list_groups_with_member_records().unwrap();

        let legacy_group_ids: Vec<i64> = groups.list_all().unwrap().iter().map(|g| g.id).collect();
        let joined_group_ids: Vec<i64> = joined.iter().map(|(g, _)| g.id).collect();
        assert_eq!(joined_group_ids, legacy_group_ids);
        for gid in all_group_ids {
            assert!(
                joined_group_ids.contains(&gid),
                "group {gid} missing from JOIN result",
            );
        }

        for (group, members) in &joined {
            let expected = legacy_member_records(&groups, &files, group.id);
            assert_eq!(
                *members, expected,
                "group {} member records diverge from the legacy N+1 walk",
                group.id,
            );
        }

        let members_of = |gid: i64| -> Vec<GroupMemberRecord> {
            joined
                .iter()
                .find(|(g, _)| g.id == gid)
                .map(|(_, m)| m.clone())
                .unwrap()
        };
        assert_eq!(
            members_of(g_all_orphan),
            Vec::<GroupMemberRecord>::new(),
            "all-orphan group has zero member records",
        );
        assert_eq!(
            members_of(g_all_deleted),
            Vec::<GroupMemberRecord>::new(),
            "all-soft-deleted group has zero member records",
        );
        assert_eq!(
            members_of(g_memberless),
            Vec::<GroupMemberRecord>::new(),
            "memberless group has zero member records",
        );
        let mixed = members_of(g_mixed);
        assert_eq!(mixed.len(), 1, "mixed group keeps only its live member");
        assert_eq!(mixed[0].file_id, live);
        assert_eq!(members_of(g_healthy).len(), 2, "healthy group keeps both");
    }
}
