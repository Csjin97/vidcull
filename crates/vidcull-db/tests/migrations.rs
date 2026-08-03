use std::collections::BTreeSet;

use rusqlite::OptionalExtension;
use tempfile::tempdir;
use vidcull_db::{Database, LATEST_VERSION, MIGRATIONS, open_file, open_in_memory};

const REQUIRED_TABLES: &[&str] = &[
    "files",
    "fingerprints",
    "scene_hashes",
    "duplicate_groups",
    "similarity_edges",
    "scan_state",
    "task_queue",
];

fn user_tables(db: &Database) -> BTreeSet<String> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_schema \
             WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name <> 'schema_migrations'",
        )
        .expect("prepare table list");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query tables")
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("collect tables")
}

fn pragma_string(db: &Database, name: &str) -> String {
    db.conn()
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|err| panic!("pragma {name} failed: {err}"))
}

fn pragma_int(db: &Database, name: &str) -> i64 {
    db.conn()
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
        .unwrap_or_else(|err| panic!("pragma {name} failed: {err}"))
}

fn explain_plan(db: &Database, sql: &str, params: &[&dyn rusqlite::ToSql]) -> String {
    let mut stmt = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare explain");
    stmt.query_map(params, |row| row.get::<_, String>(3))
        .expect("explain query")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("collect explain")
        .join(" | ")
}

#[test]
fn open_in_memory_applies_all_migrations() {
    let db = open_in_memory().expect("open in-memory");

    assert_eq!(
        db.schema_version().expect("read schema version"),
        LATEST_VERSION,
        "open_in_memory must leave the schema at the latest version",
    );

    let tables = user_tables(&db);
    for required in REQUIRED_TABLES {
        assert!(
            tables.contains(*required),
            "missing required table `{required}`; got {tables:?}"
        );
    }
}

#[test]
fn open_in_memory_sets_recommended_pragmas() {
    let db = open_in_memory().expect("open in-memory");

    assert_eq!(pragma_string(&db, "journal_mode").to_lowercase(), "memory");
    assert_eq!(
        pragma_int(&db, "synchronous"),
        1,
        "synchronous must be NORMAL (1)"
    );
    assert_eq!(
        pragma_int(&db, "foreign_keys"),
        1,
        "foreign_keys must be ON"
    );
    assert!(
        pragma_int(&db, "busy_timeout") >= 1000,
        "busy_timeout must be >= 1s",
    );
    assert_eq!(
        pragma_int(&db, "cache_size"),
        -65_536,
        "cache_size must be -64 MiB (negative ⇒ KiB)",
    );
    assert_eq!(
        pragma_int(&db, "temp_store"),
        2,
        "temp_store must be MEMORY (2)"
    );
}

#[test]
fn open_file_sets_performance_pragmas() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("vidcull.db");
    let db = open_file(&path).expect("open file db");

    assert_eq!(
        pragma_int(&db, "cache_size"),
        -65_536,
        "cache_size must be -64 MiB",
    );
    assert_eq!(
        pragma_int(&db, "temp_store"),
        2,
        "temp_store must be MEMORY (2)"
    );
    assert_eq!(
        pragma_int(&db, "mmap_size"),
        256 * 1024 * 1024,
        "mmap_size must be 256 MiB for a file-backed db",
    );
}

#[test]
fn v012_indexes_are_used_by_their_target_queries() {
    let db = open_in_memory().expect("open in-memory");
    assert_eq!(db.schema_version().expect("schema version"), LATEST_VERSION);

    let plan = explain_plan(
        &db,
        "SELECT id FROM task_queue \
         WHERE state = 'PENDING' AND kind = ?1 AND enqueued_at <= ?2 \
         ORDER BY priority DESC, enqueued_at ASC, id ASC LIMIT 1",
        &[&"scan", &100_i64],
    );
    assert!(
        plan.contains("idx_task_queue_dequeue"),
        "dequeue must use idx_task_queue_dequeue; got: {plan}",
    );

    let payload = vec![1u8, 2, 3];
    let plan = explain_plan(
        &db,
        "SELECT COUNT(1) FROM task_queue \
         WHERE kind = ?1 AND payload = ?2 AND state IN ('PENDING', 'RUNNING')",
        &[&"scan", &payload],
    );
    assert!(
        plan.contains("idx_task_queue_active_payload"),
        "existence probe must use idx_task_queue_active_payload; got: {plan}",
    );

    let plan = explain_plan(
        &db,
        "SELECT id, trust_level, best_file_id, created_at, updated_at \
         FROM duplicate_groups WHERE trust_level = ?1 ORDER BY id ASC LIMIT ?2 OFFSET ?3",
        &[&"EXACT", &10_i64, &0_i64],
    );
    assert!(
        plan.contains("idx_dup_groups_trust"),
        "trust paging must use idx_dup_groups_trust; got: {plan}",
    );
}

#[test]
fn count_failed_by_payload_query_uses_idx_task_queue_failed_payload() {
    // v017 added a `(payload, size_bytes) WHERE state = 'FAILED'` partial index
    // for `has_failed_with_size`'s lookup. It's a better match for this query
    // too (same state='FAILED' + payload equality shape), so the planner now
    // prefers it over idx_task_queue_dequeue — a net win, not a regression.
    let db = open_in_memory().expect("open in-memory");
    let payload = vec![9u8, 8, 7];
    let plan = explain_plan(
        &db,
        "SELECT COUNT(1) FROM task_queue \
         WHERE state = 'FAILED' AND kind = ?1 AND payload = ?2",
        &[&"scan", &payload],
    );
    assert!(
        plan.contains("idx_task_queue_failed_payload"),
        "count_failed_by_payload must use idx_task_queue_failed_payload; got: {plan}",
    );
}

#[test]
fn open_file_enables_wal_and_persists_schema() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("vidcull.db");

    let db = open_file(&path).expect("open file db");
    assert_eq!(
        pragma_string(&db, "journal_mode").to_lowercase(),
        "wal",
        "file-backed db must use WAL mode",
    );
    let initial_version = db.schema_version().expect("schema version");
    assert_eq!(initial_version, LATEST_VERSION);
    drop(db);

    let db2 = open_file(&path).expect("reopen file db");
    assert_eq!(
        db2.schema_version().expect("schema version after reopen"),
        LATEST_VERSION,
        "schema version should survive reopening",
    );
}

#[test]
fn migrations_are_idempotent_when_reapplied() {
    let mut db = open_in_memory().expect("open in-memory");
    let before = db.schema_version().expect("schema version pre");
    let applied = db.run_migrations().expect("re-run migrations");
    let after = db.schema_version().expect("schema version post");

    assert_eq!(before, after);
    assert_eq!(
        applied, 0,
        "no migrations should be re-applied on a fresh DB"
    );
}

#[test]
fn migration_table_records_each_applied_version() {
    let db = open_in_memory().expect("open in-memory");
    let mut stmt = db
        .conn()
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .expect("prepare select migrations");
    let recorded: Vec<i64> = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("query versions")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect versions");

    let expected: Vec<i64> = MIGRATIONS.iter().map(|m| i64::from(m.version)).collect();
    assert_eq!(recorded, expected);
}

#[test]
fn migrations_apply_in_a_single_transaction() {
    use vidcull_db::test_support::run_failing_migration;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("vidcull.db");
    let mut db = open_file(&path).expect("open file db");

    let err = run_failing_migration(&mut db).expect_err("failing migration must error");
    let msg = err.to_string();
    assert!(
        msg.contains("no such table"),
        "error must surface the underlying SQL failure: {msg}"
    );

    let canary_exists: Option<String> = db
        .conn()
        .query_row(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name='rollback_canary'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("query canary");
    assert!(
        canary_exists.is_none(),
        "rollback_canary must not exist after a failed migration",
    );
}

#[test]
fn schema_includes_expected_indexes() {
    let db = open_in_memory().expect("open in-memory");
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM sqlite_schema WHERE type='index' AND name NOT LIKE 'sqlite_%'")
        .expect("prepare index list");
    let indexes: BTreeSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query indexes")
        .collect::<Result<_, _>>()
        .expect("collect indexes");

    for required in [
        "idx_files_path",
        "idx_files_content_hash",
        "idx_fingerprints_file",
        "idx_scene_hashes_file",
        "idx_similarity_edges_group",
        "idx_task_queue_state",
    ] {
        assert!(
            indexes.contains(required),
            "missing required index `{required}`; got {indexes:?}",
        );
    }
}

#[test]
fn vacuum_into_produces_readable_snapshot() {
    let src_dir = tempdir().expect("src tempdir");
    let src_path = src_dir.path().join("source.db");
    let dest_path = src_dir.path().join("snapshot.db");

    let db = open_file(&src_path).expect("open source db");
    let expected_version = db.schema_version().expect("schema_version");

    db.vacuum_into(&dest_path).expect("vacuum_into");

    assert!(dest_path.exists(), "snapshot file must be created");

    let snap = open_file(&dest_path).expect("open snapshot db");
    assert_eq!(
        snap.schema_version().expect("schema_version on snapshot"),
        expected_version,
        "snapshot schema version must match the source",
    );
}

#[test]
fn vacuum_into_fails_when_dest_exists() {
    let dir = tempdir().expect("tempdir");
    let src_path = dir.path().join("source.db");
    let dest_path = dir.path().join("snapshot.db");

    let db = open_file(&src_path).expect("open source db");
    db.vacuum_into(&dest_path).expect("first vacuum_into");

    assert!(
        db.vacuum_into(&dest_path).is_err(),
        "vacuum_into must fail when destination already exists",
    );
}
