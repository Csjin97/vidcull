use rusqlite::{Connection, OptionalExtension, params};
use vidcull_core::Result;

use crate::connection::map_err;

const GROUPS_REVISION_KEY: &str = "groups_revision";

pub struct SystemMetadataRepo<'a> {
    conn: &'a Connection,
}

impl<'a> SystemMetadataRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM system_metadata WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_err)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO system_metadata (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn contains(&self, key: &str) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM system_metadata WHERE key = ?1", params![key])
            .map_err(map_err)?;
        Ok(())
    }

    /// 그룹 구성(멤버·베스트·존재 여부)이 바뀔 수 있는 작업(재매칭 커밋, 삭제, undo)
    /// 끝에서 한 번씩 호출한다. UI가 이 값의 변화만으로 클러스터 목록을 실제로
    /// 다시 받아야 하는지 판단할 수 있게 하는 단조 증가 카운터다.
    pub fn bump_groups_revision(&self) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO system_metadata (key, value) VALUES (?1, '1') \
                 ON CONFLICT(key) DO UPDATE SET \
                 value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            )
            .map_err(map_err)?
            .execute(params![GROUPS_REVISION_KEY])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn groups_revision(&self) -> Result<u64> {
        Ok(self
            .get(GROUPS_REVISION_KEY)?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0))
    }
}
