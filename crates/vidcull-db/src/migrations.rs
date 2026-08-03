use rusqlite::Connection;
use vidcull_core::{Error, Result};

use crate::connection::map_err;

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        sql: include_str!("schema/v001.sql"),
    },
    Migration {
        version: 2,
        name: "regroup_queue",
        sql: include_str!("schema/v002.sql"),
    },
    Migration {
        version: 3,
        name: "system_metadata",
        sql: include_str!("schema/v003.sql"),
    },
    Migration {
        version: 4,
        name: "daemon_settings",
        sql: include_str!("schema/v004.sql"),
    },
    Migration {
        version: 5,
        name: "analysis_metadata",
        sql: include_str!("schema/v005.sql"),
    },
    Migration {
        version: 6,
        name: "partial_mih_index",
        sql: include_str!("schema/v006.sql"),
    },
    Migration {
        version: 7,
        name: "delete_journal",
        sql: include_str!("schema/v007.sql"),
    },
    Migration {
        version: 8,
        name: "two_phase_delete_journal",
        sql: include_str!("schema/v008.sql"),
    },
    Migration {
        version: 9,
        name: "task_queue_size_bytes",
        sql: include_str!("schema/v009.sql"),
    },
    Migration {
        version: 10,
        name: "partial_clip_fingerprint",
        sql: include_str!("schema/v010.sql"),
    },
    Migration {
        version: 11,
        name: "partial_skip_marker",
        sql: include_str!("schema/v011.sql"),
    },
    Migration {
        version: 12,
        name: "read_path_indexes",
        sql: include_str!("schema/v012.sql"),
    },
    Migration {
        version: 13,
        name: "partial_clip_edge_spans",
        sql: include_str!("schema/v013.sql"),
    },
    Migration {
        version: 14,
        name: "whole_file_non_transitive_flag",
        sql: include_str!("schema/v014.sql"),
    },
    Migration {
        version: 15,
        name: "delete_batch_non_transitive_snapshot",
        sql: include_str!("schema/v015.sql"),
    },
    Migration {
        version: 16,
        name: "partial_edge_intro_outro_tag",
        sql: include_str!("schema/v016.sql"),
    },
    Migration {
        version: 17,
        name: "task_queue_failed_payload_index",
        sql: include_str!("schema/v017.sql"),
    },
];

pub const LATEST_VERSION: u32 = latest_version_const();

const fn latest_version_const() -> u32 {
    let mut max = 0u32;
    let mut i = 0;
    while i < MIGRATIONS.len() {
        if MIGRATIONS[i].version > max {
            max = MIGRATIONS[i].version;
        }
        i += 1;
    }
    max
}

pub fn run_pending_migrations(conn: &mut Connection, set: &[Migration]) -> Result<usize> {
    ensure_migration_table(conn)?;
    validate_sequence(set)?;

    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(map_err)?;

    let mut applied = 0usize;
    for migration in set {
        if i64::from(migration.version) <= current {
            continue;
        }
        apply_one(conn, migration)?;
        applied += 1;
    }
    Ok(applied)
}

fn ensure_migration_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
             version    INTEGER PRIMARY KEY,\
             name       TEXT    NOT NULL,\
             applied_at INTEGER NOT NULL\
         ) STRICT;",
    )
    .map_err(map_err)
}

fn validate_sequence(set: &[Migration]) -> Result<()> {
    let mut expected: u32 = 1;
    for migration in set {
        if migration.version != expected {
            return Err(Error::Database(format!(
                "migration sequence broken at V{:03}: expected V{:03}",
                migration.version, expected,
            )));
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| Error::Database("migration version overflow".into()))?;
    }
    Ok(())
}

fn apply_one(conn: &mut Connection, migration: &Migration) -> Result<()> {
    let tx = conn.transaction().map_err(map_err)?;
    tx.execute_batch(migration.sql).map_err(map_err)?;
    tx.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, strftime('%s','now'))",
        rusqlite::params![migration.version, migration.name],
    )
    .map_err(map_err)?;
    tx.commit().map_err(map_err)?;
    tracing::info!(
        version = migration.version,
        name = migration.name,
        "applied migration",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_version_matches_listed_migrations() {
        let max = MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0);
        assert_eq!(LATEST_VERSION, max);
    }

    #[test]
    fn migrations_are_sequenced_from_one() {
        validate_sequence(MIGRATIONS).expect("shipped migrations must be dense from V001");
    }

    #[test]
    fn validate_sequence_rejects_gap() {
        let bad = [Migration {
            version: 2,
            name: "skips_v1",
            sql: "",
        }];
        let err = validate_sequence(&bad).expect_err("gap must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("V002") && msg.contains("V001"), "got: {msg}");
    }

    #[test]
    fn v008_preserves_existing_delete_batches_and_defaults_status() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..7]).expect("apply through v007");
        conn.execute(
            "INSERT INTO delete_batches \
                 (group_id, trust_level, best_file_id, group_dropped, mode, created_at) \
             VALUES (5, 'EXACT', NULL, 0, 'TRASH', 100)",
            [],
        )
        .expect("insert pre-v008 batch");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..8]).expect("apply v008");
        assert_eq!(applied, 1, "only v008 was pending");

        let (group_id, status): (i64, String) = conn
            .query_row("SELECT group_id, status FROM delete_batches", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("row survives v008");
        assert_eq!(group_id, 5, "row preserved across v008");
        assert_eq!(status, "COMMITTED", "existing rows default to COMMITTED");
    }

    #[test]
    fn v009_preserves_existing_tasks_and_defaults_size_bytes() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..8]).expect("apply through v008");
        conn.execute(
            "INSERT INTO task_queue (kind, state, priority, payload, enqueued_at) \
             VALUES ('scan', 'PENDING', 0, NULL, 100)",
            [],
        )
        .expect("insert pre-v009 task");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..9]).expect("apply v009");
        assert_eq!(applied, 1, "only v009 was pending");

        let (kind, size_bytes): (String, i64) = conn
            .query_row("SELECT kind, size_bytes FROM task_queue", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("row survives v009");
        assert_eq!(kind, "scan", "row preserved across v009");
        assert_eq!(size_bytes, 0, "existing rows default to 0 bytes");
    }

    #[test]
    fn v011_preserves_existing_fingerprints_and_defaults_skip_marker_null() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..10]).expect("apply through v010");
        conn.execute(
            "INSERT INTO files (path, size_bytes, mtime_ns, first_seen_at, last_seen_at) \
             VALUES ('/lib/clip.mp4', 1000, 5, 100, 100)",
            [],
        )
        .expect("insert pre-v011 file");
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO fingerprints (file_id, tier1_global, format_version, created_at) \
             VALUES (?1, X'0102', 1, 100)",
            [file_id],
        )
        .expect("insert pre-v011 fingerprint");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..11]).expect("apply v011");
        assert_eq!(applied, 1, "only v011 was pending");

        let (tier1, reason): (Vec<u8>, Option<String>) = conn
            .query_row(
                "SELECT tier1_global, partial_skip_reason FROM fingerprints WHERE file_id = ?1",
                [file_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row survives v011");
        assert_eq!(tier1, vec![0x01, 0x02], "payload preserved across v011");
        assert!(
            reason.is_none(),
            "existing rows default to NULL skip marker"
        );
    }

    #[test]
    fn v012_adds_read_path_indexes_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..11]).expect("apply through v011");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..12]).expect("apply v012");
        assert_eq!(applied, 1, "only v012 was pending");
        let again = run_pending_migrations(&mut conn, &MIGRATIONS[..12]).expect("re-run v012");
        assert_eq!(again, 0, "v012 is idempotent");

        let indexes: std::collections::BTreeSet<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type='index'")
            .expect("prepare")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        for required in [
            "idx_task_queue_dequeue",
            "idx_task_queue_active_payload",
            "idx_dup_groups_trust",
        ] {
            assert!(
                indexes.contains(required),
                "missing index {required}: {indexes:?}"
            );
        }
    }

    #[test]
    fn v013_preserves_existing_edges_and_defaults_partial_span_null() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..12]).expect("apply through v012");
        conn.execute(
            "INSERT INTO duplicate_groups (trust_level, created_at, updated_at) \
             VALUES ('POSSIBLE', 100, 100)",
            [],
        )
        .expect("insert group");
        conn.execute(
            "INSERT INTO files (path, size_bytes, mtime_ns, first_seen_at, last_seen_at) \
             VALUES ('/m/a.mp4', 1, 1, 100, 100), ('/m/b.mp4', 1, 1, 100, 100)",
            [],
        )
        .expect("insert files");
        conn.execute(
            "INSERT INTO similarity_edges (group_id, file_a, file_b, score_x1000) \
             VALUES (1, 1, 2, 615)",
            [],
        )
        .expect("insert pre-v013 edge");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..13]).expect("apply v013");
        assert_eq!(applied, 1, "only v013 was pending");

        let (score, clip_start, matched): (i32, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT score_x1000, clip_start_ms, matched_scenes FROM similarity_edges \
                 WHERE group_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("row survives v013");
        assert_eq!(score, 615, "score preserved across v013");
        assert!(
            clip_start.is_none(),
            "existing rows default to NULL clip_start_ms"
        );
        assert!(
            matched.is_none(),
            "existing rows default to NULL matched_scenes"
        );

        let again = run_pending_migrations(&mut conn, &MIGRATIONS[..13]).expect("re-run v013");
        assert_eq!(again, 0, "v013 is idempotent");
    }

    #[test]
    fn v014_preserves_existing_groups_and_defaults_non_transitive_zero() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..13]).expect("apply through v013");
        conn.execute(
            "INSERT INTO duplicate_groups (trust_level, created_at, updated_at) \
             VALUES ('VERY_LIKELY', 100, 100)",
            [],
        )
        .expect("insert pre-v014 group");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..14]).expect("apply v014");
        assert_eq!(applied, 1, "only v014 was pending");

        let (trust, non_transitive): (String, i64) = conn
            .query_row(
                "SELECT trust_level, non_transitive FROM duplicate_groups",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row survives v014");
        assert_eq!(trust, "VERY_LIKELY", "trust_level preserved across v014");
        assert_eq!(
            non_transitive, 0,
            "existing rows default to non_transitive=0"
        );

        let again = run_pending_migrations(&mut conn, &MIGRATIONS[..14]).expect("re-run v014");
        assert_eq!(again, 0, "v014 is idempotent");
    }

    #[test]
    fn v015_preserves_existing_delete_batches_and_defaults_non_transitive_zero() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..14]).expect("apply through v014");
        conn.execute(
            "INSERT INTO delete_batches \
                 (group_id, trust_level, best_file_id, group_dropped, mode, created_at) \
             VALUES (5, 'VERY_LIKELY', NULL, 1, 'TRASH', 100)",
            [],
        )
        .expect("insert pre-v015 batch");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..15]).expect("apply v015");
        assert_eq!(applied, 1, "only v015 was pending");

        let (trust_level, non_transitive): (String, i64) = conn
            .query_row(
                "SELECT trust_level, non_transitive FROM delete_batches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row survives v015");
        assert_eq!(
            trust_level, "VERY_LIKELY",
            "trust_level preserved across v015"
        );
        assert_eq!(
            non_transitive, 0,
            "existing rows default to non_transitive=0"
        );

        let again = run_pending_migrations(&mut conn, &MIGRATIONS[..15]).expect("re-run v015");
        assert_eq!(again, 0, "v015 is idempotent");
    }

    #[test]
    fn v016_preserves_existing_edges_and_defaults_intro_outro_zero() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..15]).expect("apply through v015");
        conn.execute(
            "INSERT INTO duplicate_groups (trust_level, created_at, updated_at) \
             VALUES ('POSSIBLE', 100, 100)",
            [],
        )
        .expect("insert group");
        conn.execute(
            "INSERT INTO files (path, size_bytes, mtime_ns, first_seen_at, last_seen_at) \
             VALUES ('/m/a.mp4', 1, 1, 100, 100), ('/m/b.mp4', 1, 1, 100, 100)",
            [],
        )
        .expect("insert files");
        conn.execute(
            "INSERT INTO similarity_edges (group_id, file_a, file_b, score_x1000) \
             VALUES (1, 1, 2, 615)",
            [],
        )
        .expect("insert pre-v016 edge");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..16]).expect("apply v016");
        assert_eq!(applied, 1, "only v016 was pending");

        let (score, intro_outro): (i32, i64) = conn
            .query_row(
                "SELECT score_x1000, intro_outro FROM similarity_edges WHERE group_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row survives v016");
        assert_eq!(score, 615, "score preserved across v016");
        assert_eq!(intro_outro, 0, "existing rows default to intro_outro=0");

        let again = run_pending_migrations(&mut conn, &MIGRATIONS[..16]).expect("re-run v016");
        assert_eq!(again, 0, "v016 is idempotent");
    }

    #[test]
    fn v017_adds_failed_payload_index_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().expect("open");
        run_pending_migrations(&mut conn, &MIGRATIONS[..16]).expect("apply through v016");

        let applied = run_pending_migrations(&mut conn, &MIGRATIONS[..17]).expect("apply v017");
        assert_eq!(applied, 1, "only v017 was pending");
        let again = run_pending_migrations(&mut conn, &MIGRATIONS[..17]).expect("re-run v017");
        assert_eq!(again, 0, "v017 is idempotent");

        let indexes: std::collections::BTreeSet<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type='index'")
            .expect("prepare")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert!(
            indexes.contains("idx_task_queue_failed_payload"),
            "missing index idx_task_queue_failed_payload: {indexes:?}"
        );
    }
}
