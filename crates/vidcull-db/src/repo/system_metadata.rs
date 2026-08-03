use rusqlite::{Connection, OptionalExtension, params};
use vidcull_core::Result;

use crate::connection::map_err;

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
}
