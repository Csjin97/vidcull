use std::collections::HashMap;

use rusqlite::{Connection, Row, params, params_from_iter};
use vidcull_core::Result;
use vidcull_core::types::FileId;

use super::duplicate_groups::TrustLevel;
use crate::connection::map_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialEdgeSpan {
    pub clip_start_ms: u64,
    pub clip_end_ms: u64,
    pub source_start_ms: u64,
    pub source_end_ms: u64,
    pub matched_scenes: usize,
    pub clip_scenes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarityEdge {
    pub group_id: i64,
    pub file_a: FileId,
    pub file_b: FileId,
    pub score_x1000: i32,
    pub partial_span: Option<PartialEdgeSpan>,
    pub intro_outro: bool,
}

pub struct SimilarityEdgesRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SimilarityEdgesRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, edge: &SimilarityEdge) -> Result<()> {
        let (a, b) = if edge.file_a.0 <= edge.file_b.0 {
            (edge.file_a, edge.file_b)
        } else {
            (edge.file_b, edge.file_a)
        };
        let span = edge.partial_span.as_ref();
        self.conn
            .execute(
                "INSERT INTO similarity_edges \
                 (group_id, file_a, file_b, score_x1000, clip_start_ms, clip_end_ms, \
                  source_start_ms, source_end_ms, matched_scenes, clip_scenes, intro_outro) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    edge.group_id,
                    a.0,
                    b.0,
                    edge.score_x1000,
                    span.map(|s| span_to_i64(s.clip_start_ms)),
                    span.map(|s| span_to_i64(s.clip_end_ms)),
                    span.map(|s| span_to_i64(s.source_start_ms)),
                    span.map(|s| span_to_i64(s.source_end_ms)),
                    span.map(|s| scenes_to_i64(s.matched_scenes)),
                    span.map(|s| scenes_to_i64(s.clip_scenes)),
                    i64::from(edge.intro_outro),
                ],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn list_for_group(&self, group_id: i64) -> Result<Vec<SimilarityEdge>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT group_id, file_a, file_b, score_x1000, clip_start_ms, clip_end_ms, \
                 source_start_ms, source_end_ms, matched_scenes, clip_scenes, intro_outro \
                 FROM similarity_edges WHERE group_id = ?1 \
                 ORDER BY file_a ASC, file_b ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![group_id], row_to_edge)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_by_trust(&self, trust: TrustLevel) -> Result<Vec<SimilarityEdge>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT e.group_id, e.file_a, e.file_b, e.score_x1000, e.clip_start_ms, \
                 e.clip_end_ms, e.source_start_ms, e.source_end_ms, e.matched_scenes, \
                 e.clip_scenes, e.intro_outro \
                 FROM similarity_edges e \
                 INNER JOIN duplicate_groups g ON g.id = e.group_id \
                 WHERE g.trust_level = ?1 \
                 ORDER BY e.file_a ASC, e.file_b ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![trust.as_text()], row_to_edge)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn all_tagged_intro_outro(&self, group_ids: &[i64]) -> Result<HashMap<i64, bool>> {
        if group_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = vec!["?"; group_ids.len()].join(",");
        let sql = format!(
            "SELECT group_id, COUNT(*), SUM(intro_outro) FROM similarity_edges \
             WHERE group_id IN ({placeholders}) GROUP BY group_id"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt
            .query_map(params_from_iter(group_ids.iter()), |row| {
                let total: i64 = row.get(1)?;
                let tagged: i64 = row.get(2)?;
                Ok((row.get::<_, i64>(0)?, total > 0 && total == tagged))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(map_err)?;
        Ok(rows)
    }
}

fn row_to_edge(row: &Row<'_>) -> rusqlite::Result<SimilarityEdge> {
    Ok(SimilarityEdge {
        group_id: row.get("group_id")?,
        file_a: FileId(row.get("file_a")?),
        file_b: FileId(row.get("file_b")?),
        score_x1000: row.get("score_x1000")?,
        partial_span: read_partial_span(row)?,
        intro_outro: row.get::<_, i64>("intro_outro")? != 0,
    })
}

fn read_partial_span(row: &Row<'_>) -> rusqlite::Result<Option<PartialEdgeSpan>> {
    let Some(clip_start_ms) = row.get::<_, Option<i64>>("clip_start_ms")? else {
        return Ok(None);
    };
    Ok(Some(PartialEdgeSpan {
        clip_start_ms: i64_to_u64(clip_start_ms),
        clip_end_ms: i64_to_u64(row.get::<_, i64>("clip_end_ms")?),
        source_start_ms: i64_to_u64(row.get::<_, i64>("source_start_ms")?),
        source_end_ms: i64_to_u64(row.get::<_, i64>("source_end_ms")?),
        matched_scenes: i64_to_usize(row.get::<_, i64>("matched_scenes")?),
        clip_scenes: i64_to_usize(row.get::<_, i64>("clip_scenes")?),
    }))
}

fn span_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn scenes_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn i64_to_usize(value: i64) -> usize {
    usize::try_from(value).unwrap_or(0)
}
