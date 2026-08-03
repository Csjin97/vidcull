use rusqlite::{Connection, OptionalExtension, params};
use vidcull_core::Result;

use crate::connection::map_err;

pub struct DaemonSettingsRepo<'a> {
    conn: &'a Connection,
}

impl<'a> DaemonSettingsRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn load(&self) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT payload FROM daemon_settings WHERE id = 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(map_err)
    }

    pub fn save(&self, payload: &[u8]) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO daemon_settings (id, payload) VALUES (1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
                params![payload],
            )
            .map_err(map_err)?;
        Ok(())
    }
}
