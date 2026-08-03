/**
 * @file    `files.rs`
 * @brief   인덱싱 파일 메타데이터 저장소
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 대용량 초기 스캔용 경량 파일 지문 조회 추가
 */
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use vidcull_core::Result;
use vidcull_core::types::{
    Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath, Resolution, VideoDuration,
};

use crate::connection::map_err;
use crate::repo::codec_sql;

#[derive(Debug, Clone, PartialEq)]
pub struct NewFile {
    pub path: NormalizedPath,
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub inode: Option<i64>,
    pub content_hash: Option<Blake3Hash>,
    pub codec: Option<Codec>,
    pub container: Option<String>,
    pub duration: Option<VideoDuration>,
    pub fps_x1000: Option<i32>,
    pub bitrate_bps: Option<i64>,
    pub resolution: Option<Resolution>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub laplacian_variance: Option<f64>,
    pub dct_energy: Option<f64>,
    pub bpp: Option<f64>,
    pub encoder_tags: Option<String>,
}

impl Default for NewFile {
    fn default() -> Self {
        Self {
            path: NormalizedPath::new(""),
            size_bytes: 0,
            mtime_ns: 0,
            inode: None,
            content_hash: None,
            codec: None,
            container: None,
            duration: None,
            fps_x1000: None,
            bitrate_bps: None,
            resolution: None,
            first_seen_at: 0,
            last_seen_at: 0,
            laplacian_variance: None,
            dct_energy: None,
            bpp: None,
            encoder_tags: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub id: FileId,
    pub path: NormalizedPath,
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub inode: Option<i64>,
    pub content_hash: Option<Blake3Hash>,
    pub codec: Option<Codec>,
    pub container: Option<String>,
    pub duration: Option<VideoDuration>,
    pub fps_x1000: Option<i32>,
    pub bitrate_bps: Option<i64>,
    pub resolution: Option<Resolution>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub deleted_at: Option<i64>,
    pub laplacian_variance: Option<f64>,
    pub dct_energy: Option<f64>,
    pub bpp: Option<f64>,
    pub encoder_tags: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFingerprintRecord {
    pub path: NormalizedPath,
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub inode: Option<i64>,
}

pub struct FilesRepo<'a> {
    conn: &'a Connection,
}

impl<'a> FilesRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, file: &NewFile) -> Result<FileId> {
        let (w, h) = split_resolution(file.resolution);
        let hash_blob = file.content_hash.as_ref().map(Blake3Hash::as_bytes);
        let codec_text = file.codec.as_ref().map(codec_sql::to_text);
        let duration_ms = duration_to_sql(file.duration);

        self.conn
            .prepare_cached(
                "INSERT INTO files (\
                    path, size_bytes, mtime_ns, inode, content_hash, \
                    codec, container, duration_ms, fps_x1000, bitrate_bps, \
                    width_px, height_px, first_seen_at, last_seen_at, \
                    laplacian_variance, dct_energy, bpp, encoder_tags\
                 ) VALUES (\
                    ?1, ?2, ?3, ?4, ?5, \
                    ?6, ?7, ?8, ?9, ?10, \
                    ?11, ?12, ?13, ?14, \
                    ?15, ?16, ?17, ?18)",
            )
            .map_err(map_err)?
            .execute(params![
                file.path.as_str(),
                file.size_bytes,
                file.mtime_ns,
                file.inode,
                hash_blob.map(<[u8; HASH_LEN]>::as_slice),
                codec_text,
                file.container,
                duration_ms,
                file.fps_x1000,
                file.bitrate_bps,
                w,
                h,
                file.first_seen_at,
                file.last_seen_at,
                file.laplacian_variance,
                file.dct_energy,
                file.bpp,
                file.encoder_tags,
            ])
            .map_err(map_err)?;
        Ok(FileId(self.conn.last_insert_rowid()))
    }

    pub fn get(&self, id: FileId) -> Result<Option<FileRecord>> {
        self.conn
            .prepare_cached(SELECT_ALL_WHERE_ID)
            .map_err(map_err)?
            .query_row(params![id.0], row_to_record)
            .optional()
            .map_err(map_err)
    }

    pub fn find_by_path(&self, path: &NormalizedPath) -> Result<Option<FileRecord>> {
        self.conn
            .prepare_cached(SELECT_ALL_WHERE_PATH)
            .map_err(map_err)?
            .query_row(params![path.as_str()], row_to_record)
            .optional()
            .map_err(map_err)
    }

    pub fn list_active(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(SELECT_ALL_ACTIVE).map_err(map_err)?;
        let rows = stmt
            .query_map([], row_to_record)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn count_active(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        usize::try_from(count).map_err(|_| {
            vidcull_core::Error::Database(format!("active file count is out of range: {count}"))
        })
    }

    pub fn visit_active_scan_fingerprints(
        &self,
        mut visit: impl FnMut(ScanFingerprintRecord),
    ) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size_bytes, mtime_ns, inode FROM files WHERE deleted_at IS NULL")
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ScanFingerprintRecord {
                    path: NormalizedPath::new(row.get::<_, String>(0)?),
                    size_bytes: row.get(1)?,
                    mtime_ns: row.get(2)?,
                    inode: row.get(3)?,
                })
            })
            .map_err(map_err)?;
        for row in rows {
            visit(row.map_err(map_err)?);
        }
        Ok(())
    }

    pub fn list_hashed_active(&self) -> Result<Vec<(FileId, Blake3Hash)>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id, content_hash FROM files \
                 WHERE deleted_at IS NULL AND content_hash IS NOT NULL \
                 ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                let actual_len = blob.len();
                let arr: [u8; HASH_LEN] = blob.try_into().map_err(|_: Vec<u8>| {
                    conversion_failure(
                        Type::Blob,
                        format!("content_hash blob has {actual_len} bytes, expected {HASH_LEN}"),
                    )
                })?;
                Ok((FileId(id), Blake3Hash::from_bytes(arr)))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn find_active_twin_by_hash(
        &self,
        hash: &Blake3Hash,
        exclude: &NormalizedPath,
    ) -> Result<Option<FileRecord>> {
        let blob: &[u8] = hash.as_bytes();
        self.conn
            .query_row(
                "SELECT id, path, size_bytes, mtime_ns, inode, content_hash, \
                        codec, container, duration_ms, fps_x1000, bitrate_bps, \
                        width_px, height_px, first_seen_at, last_seen_at, deleted_at, \
                        laplacian_variance, dct_energy, bpp, encoder_tags \
                 FROM files \
                 WHERE deleted_at IS NULL AND content_hash = ?1 AND path <> ?2 \
                   AND EXISTS (SELECT 1 FROM fingerprints WHERE fingerprints.file_id = files.id) \
                 ORDER BY id ASC LIMIT 1",
                params![blob, exclude.as_str()],
                row_to_record,
            )
            .optional()
            .map_err(map_err)
    }

    pub fn list_active_by_hash(&self, hash: &Blake3Hash) -> Result<Vec<FileRecord>> {
        let blob: &[u8] = hash.as_bytes();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, path, size_bytes, mtime_ns, inode, content_hash, \
                        codec, container, duration_ms, fps_x1000, bitrate_bps, \
                        width_px, height_px, first_seen_at, last_seen_at, deleted_at, \
                        laplacian_variance, dct_energy, bpp, encoder_tags \
                 FROM files \
                 WHERE deleted_at IS NULL AND content_hash = ?1 \
                 ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![blob], row_to_record)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn update_quality_stats(
        &self,
        id: FileId,
        laplacian_variance: Option<f64>,
        dct_energy: Option<f64>,
        bpp: Option<f64>,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE files SET laplacian_variance = ?1, dct_energy = ?2, bpp = ?3 \
                 WHERE id = ?4",
            )
            .map_err(map_err)?
            .execute(params![laplacian_variance, dct_energy, bpp, id.0])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn update_metadata(&self, id: FileId, file: &NewFile) -> Result<()> {
        let (w, h) = split_resolution(file.resolution);
        let hash_blob = file.content_hash.as_ref().map(Blake3Hash::as_bytes);
        let codec_text = file.codec.as_ref().map(codec_sql::to_text);
        let duration_ms = duration_to_sql(file.duration);

        self.conn
            .prepare_cached(
                "UPDATE files SET \
                    path = ?1, size_bytes = ?2, mtime_ns = ?3, inode = ?4, \
                    content_hash = ?5, codec = ?6, container = ?7, \
                    duration_ms = ?8, fps_x1000 = ?9, bitrate_bps = ?10, \
                    width_px = ?11, height_px = ?12, \
                    first_seen_at = ?13, last_seen_at = ?14, \
                    laplacian_variance = ?15, dct_energy = ?16, bpp = ?17, encoder_tags = ?18, \
                    deleted_at = NULL \
                 WHERE id = ?19",
            )
            .map_err(map_err)?
            .execute(params![
                file.path.as_str(),
                file.size_bytes,
                file.mtime_ns,
                file.inode,
                hash_blob.map(<[u8; HASH_LEN]>::as_slice),
                codec_text,
                file.container,
                duration_ms,
                file.fps_x1000,
                file.bitrate_bps,
                w,
                h,
                file.first_seen_at,
                file.last_seen_at,
                file.laplacian_variance,
                file.dct_energy,
                file.bpp,
                file.encoder_tags,
                id.0,
            ])
            .map_err(map_err)?;

        if file
            .codec
            .as_ref()
            .is_some_and(Codec::is_fast_path_eligible)
        {
            self.conn
                .prepare_cached(
                    "UPDATE fingerprints SET \
                        partial_skip_reason     = NULL, \
                        partial_skip_size_bytes = NULL, \
                        partial_skip_mtime_ns   = NULL \
                     WHERE file_id = ?1 AND partial_skip_reason IS NOT NULL",
                )
                .map_err(map_err)?
                .execute(params![id.0])
                .map_err(map_err)?;
        }
        Ok(())
    }

    pub fn set_content_hash(&self, id: FileId, hash: Blake3Hash) -> Result<()> {
        let blob: &[u8] = hash.as_bytes();
        self.conn
            .prepare_cached("UPDATE files SET content_hash = ?1 WHERE id = ?2")
            .map_err(map_err)?
            .execute(params![blob, id.0])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn clear_content_hash(&self, id: FileId) -> Result<()> {
        self.conn
            .execute(
                "UPDATE files SET content_hash = NULL WHERE id = ?1",
                params![id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn touch_last_seen(&self, id: FileId, when: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE files SET last_seen_at = ?1 WHERE id = ?2")
            .map_err(map_err)?
            .execute(params![when, id.0])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn clear_deleted(&self, id: FileId) -> Result<()> {
        self.conn
            .execute(
                "UPDATE files SET deleted_at = NULL WHERE id = ?1",
                params![id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn mark_deleted(&self, id: FileId, when: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE files SET deleted_at = ?1 WHERE id = ?2",
                params![when, id.0],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn count_active_indexed(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files \
                 WHERE deleted_at IS NULL AND content_hash IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn sum_active_size_bytes(&self) -> Result<u64> {
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM files \
                 WHERE deleted_at IS NULL AND content_hash IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(total).unwrap_or(0))
    }

    pub fn list_active_paths_under_root(
        &self,
        root: &NormalizedPath,
    ) -> Result<Vec<NormalizedPath>> {
        let trimmed = root.as_str().trim_end_matches('/');
        let escaped = escape_like(trimmed);
        let descendants = format!("{escaped}/%");

        let mut stmt = self
            .conn
            .prepare(
                "SELECT path FROM files \
                 WHERE deleted_at IS NULL \
                   AND (path = ?1 OR path LIKE ?2 ESCAPE '\\') \
                 ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![trimmed, descendants], |row| {
                Ok(NormalizedPath::new(row.get::<_, String>(0)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn list_active_under_root(&self, root: &NormalizedPath) -> Result<Vec<FileRecord>> {
        let trimmed = root.as_str().trim_end_matches('/');
        let escaped = escape_like(trimmed);
        let descendants = format!("{escaped}/%");
        let mut stmt = self
            .conn
            .prepare(SELECT_ALL_ACTIVE_UNDER_ROOT)
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![trimmed, descendants], row_to_record)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn active_hashed_paths_in(
        &self,
        paths: &[NormalizedPath],
    ) -> Result<std::collections::HashSet<String>> {
        const IN_BATCH: usize = 900;
        let mut out = std::collections::HashSet::new();
        for batch in paths.chunks(IN_BATCH) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT path FROM files \
                 WHERE deleted_at IS NULL AND content_hash IS NOT NULL \
                   AND path IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql).map_err(map_err)?;
            let rows = stmt
                .query_map(
                    rusqlite::params_from_iter(batch.iter().map(NormalizedPath::as_str)),
                    |row| row.get::<_, String>(0),
                )
                .map_err(map_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_err)?;
            out.extend(rows);
        }
        Ok(out)
    }

    pub fn delete(&self, id: FileId) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE id = ?1", params![id.0])
            .map_err(map_err)?;
        Ok(())
    }
}

const SELECT_ALL_WHERE_ID: &str = "SELECT id, path, size_bytes, mtime_ns, inode, content_hash, \
            codec, container, duration_ms, fps_x1000, bitrate_bps, \
            width_px, height_px, first_seen_at, last_seen_at, deleted_at, \
            laplacian_variance, dct_energy, bpp, encoder_tags \
     FROM files WHERE id = ?1";

const SELECT_ALL_WHERE_PATH: &str = "SELECT id, path, size_bytes, mtime_ns, inode, content_hash, \
            codec, container, duration_ms, fps_x1000, bitrate_bps, \
            width_px, height_px, first_seen_at, last_seen_at, deleted_at, \
            laplacian_variance, dct_energy, bpp, encoder_tags \
     FROM files WHERE path = ?1";

const SELECT_ALL_ACTIVE: &str = "SELECT id, path, size_bytes, mtime_ns, inode, content_hash, \
            codec, container, duration_ms, fps_x1000, bitrate_bps, \
            width_px, height_px, first_seen_at, last_seen_at, deleted_at, \
            laplacian_variance, dct_energy, bpp, encoder_tags \
     FROM files WHERE deleted_at IS NULL ORDER BY id ASC";

const SELECT_ALL_ACTIVE_UNDER_ROOT: &str = "SELECT id, path, size_bytes, mtime_ns, inode, content_hash, \
            codec, container, duration_ms, fps_x1000, bitrate_bps, \
            width_px, height_px, first_seen_at, last_seen_at, deleted_at, \
            laplacian_variance, dct_energy, bpp, encoder_tags \
     FROM files \
     WHERE deleted_at IS NULL AND (path = ?1 OR path LIKE ?2 ESCAPE '\\') \
     ORDER BY id ASC";

fn escape_like(literal: &str) -> String {
    literal
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn split_resolution(resolution: Option<Resolution>) -> (Option<i64>, Option<i64>) {
    match resolution {
        Some(r) => (Some(i64::from(r.width)), Some(i64::from(r.height))),
        None => (None, None),
    }
}

fn combine_resolution(w: Option<i64>, h: Option<i64>) -> rusqlite::Result<Option<Resolution>> {
    match (w, h) {
        (None, None) => Ok(None),
        (Some(w), Some(h)) => {
            let width = u32::try_from(w).map_err(|_| {
                conversion_failure(Type::Integer, format!("width_px {w} out of u32 range"))
            })?;
            let height = u32::try_from(h).map_err(|_| {
                conversion_failure(Type::Integer, format!("height_px {h} out of u32 range"))
            })?;
            Ok(Some(Resolution::new(width, height)))
        }
        (some_w, some_h) => Err(conversion_failure(
            Type::Integer,
            format!(
                "resolution must be both-NULL or both-set; got width={some_w:?}, height={some_h:?}"
            ),
        )),
    }
}

#[allow(clippy::cast_possible_wrap)]
fn duration_to_sql(d: Option<VideoDuration>) -> Option<i64> {
    d.map(|v| v.as_millis() as i64)
}

fn duration_from_sql(ms: Option<i64>) -> rusqlite::Result<Option<VideoDuration>> {
    let Some(value) = ms else { return Ok(None) };
    let positive = u64::try_from(value).map_err(|_| {
        conversion_failure(Type::Integer, format!("duration_ms {value} is negative"))
    })?;
    Ok(Some(VideoDuration::from_millis(positive)))
}

fn hash_from_blob(blob: Option<Vec<u8>>) -> rusqlite::Result<Option<Blake3Hash>> {
    let Some(bytes) = blob else { return Ok(None) };
    let actual_len = bytes.len();
    let arr: [u8; HASH_LEN] = bytes.try_into().map_err(|_: Vec<u8>| {
        conversion_failure(
            Type::Blob,
            format!("content_hash blob has {actual_len} bytes, expected {HASH_LEN}"),
        )
    })?;
    Ok(Some(Blake3Hash::from_bytes(arr)))
}

fn conversion_failure(ty: Type, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, ty, msg.into())
}

fn row_to_record(row: &Row<'_>) -> rusqlite::Result<FileRecord> {
    let codec_text: Option<String> = row.get("codec")?;
    let content_hash_blob: Option<Vec<u8>> = row.get("content_hash")?;
    let duration_ms: Option<i64> = row.get("duration_ms")?;
    let width_px: Option<i64> = row.get("width_px")?;
    let height_px: Option<i64> = row.get("height_px")?;

    Ok(FileRecord {
        id: FileId(row.get("id")?),
        path: NormalizedPath::new(row.get::<_, String>("path")?),
        size_bytes: row.get("size_bytes")?,
        mtime_ns: row.get("mtime_ns")?,
        inode: row.get("inode")?,
        content_hash: hash_from_blob(content_hash_blob)?,
        codec: codec_text.map(|s| codec_sql::from_text(&s)),
        container: row.get("container")?,
        duration: duration_from_sql(duration_ms)?,
        fps_x1000: row.get("fps_x1000")?,
        bitrate_bps: row.get("bitrate_bps")?,
        resolution: combine_resolution(width_px, height_px)?,
        first_seen_at: row.get("first_seen_at")?,
        last_seen_at: row.get("last_seen_at")?,
        deleted_at: row.get("deleted_at")?,
        laplacian_variance: row.get("laplacian_variance")?,
        dct_energy: row.get("dct_energy")?,
        bpp: row.get("bpp")?,
        encoder_tags: row.get("encoder_tags")?,
    })
}
