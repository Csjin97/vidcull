use std::collections::BTreeSet;

use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_db::open_in_memory;
use vidcull_db::repo::{FilesRepo, NewFile, RegroupQueueRepo};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn seed_file(db: &vidcull_db::Database, path: &str) -> FileId {
    FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(path),
            size_bytes: 1024,
            mtime_ns: MTIME,
            inode: None,
            content_hash: None,
            codec: None,
            container: None,
            duration: None,
            fps_x1000: None,
            bitrate_bps: None,
            resolution: None,
            first_seen_at: T0,
            last_seen_at: T0,
            laplacian_variance: None,
            dct_energy: None,
            bpp: None,
            encoder_tags: None,
        })
        .expect("insert file")
}

#[test]
fn mark_load_round_trips_the_delta() {
    let db = open_in_memory().expect("open");
    let a = seed_file(&db, "/v/a.mp4");
    let b = seed_file(&db, "/v/b.mp4");
    let repo = RegroupQueueRepo::new(db.conn());

    assert!(repo.is_empty().expect("is_empty"));
    repo.mark(a, T0).expect("mark a");
    repo.mark(b, T0 + 1).expect("mark b");

    assert_eq!(repo.len().expect("len"), 2);
    let loaded = repo.load().expect("load");
    assert_eq!(loaded, BTreeSet::from([a, b]));
}

#[test]
fn mark_is_idempotent() {
    let db = open_in_memory().expect("open");
    let a = seed_file(&db, "/v/a.mp4");
    let repo = RegroupQueueRepo::new(db.conn());

    repo.mark(a, T0).expect("mark 1");
    repo.mark(a, T0 + 5).expect("mark 2 (same file)");
    repo.mark(a, T0 + 9).expect("mark 3 (same file)");

    assert_eq!(repo.len().expect("len"), 1, "a repeated mark is one entry");
    assert_eq!(repo.load().expect("load"), BTreeSet::from([a]));
}

#[test]
fn clear_removes_only_consumed_ids() {
    let db = open_in_memory().expect("open");
    let a = seed_file(&db, "/v/a.mp4");
    let b = seed_file(&db, "/v/b.mp4");
    let c = seed_file(&db, "/v/c.mp4");
    let repo = RegroupQueueRepo::new(db.conn());
    for id in [a, b, c] {
        repo.mark(id, T0).expect("mark");
    }

    let removed = repo.clear([a, b]).expect("clear");
    assert_eq!(removed, 2);
    assert_eq!(
        repo.load().expect("load"),
        BTreeSet::from([c]),
        "clear drops only the ids passed, leaving later marks intact",
    );
}

#[test]
fn clearing_a_missing_id_is_a_no_op() {
    let db = open_in_memory().expect("open");
    let a = seed_file(&db, "/v/a.mp4");
    let repo = RegroupQueueRepo::new(db.conn());
    repo.mark(a, T0).expect("mark");

    let removed = repo.clear([FileId(999)]).expect("clear missing");
    assert_eq!(removed, 0, "clearing an absent id removes nothing");
    assert_eq!(repo.len().expect("len"), 1, "the real entry is untouched");
}

#[test]
fn hard_deleting_a_file_cascades_its_delta_entry() {
    let db = open_in_memory().expect("open");
    let a = seed_file(&db, "/v/a.mp4");
    RegroupQueueRepo::new(db.conn()).mark(a, T0).expect("mark");
    assert_eq!(RegroupQueueRepo::new(db.conn()).len().expect("len"), 1);

    db.conn()
        .execute("DELETE FROM files WHERE id = ?1", [a.0])
        .expect("hard delete file");

    assert_eq!(
        RegroupQueueRepo::new(db.conn()).len().expect("len"),
        0,
        "ON DELETE CASCADE removed the orphaned delta entry",
    );
}
