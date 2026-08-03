#![allow(missing_docs)]
#![forbid(unsafe_code)]

mod connection;
mod migrations;
pub mod repo;

pub use connection::{Database, open_file, open_in_memory};
pub use migrations::{LATEST_VERSION, MIGRATIONS, Migration};

#[doc(hidden)]
pub mod test_support {
    use rusqlite::Connection;
    use vidcull_core::Result;

    use crate::connection::map_err;
    use crate::migrations::{Migration, run_pending_migrations};

    pub fn run_failing_migration(db: &mut crate::Database) -> Result<()> {
        let bad = [Migration {
            version: 999,
            name: "intentional_failure",
            sql: "CREATE TABLE rollback_canary (id INTEGER PRIMARY KEY);\n\
                  INSERT INTO no_such_table VALUES (1);",
        }];
        apply_unsequenced(db.conn_mut(), &bad)?;
        Ok(())
    }

    fn apply_unsequenced(conn: &mut Connection, set: &[Migration]) -> Result<()> {
        run_pending_migrations(conn, crate::MIGRATIONS)?;
        for m in set {
            let tx = conn.transaction().map_err(map_err)?;
            tx.execute_batch(m.sql).map_err(map_err)?;
            tx.commit().map_err(map_err)?;
        }
        Ok(())
    }
}
