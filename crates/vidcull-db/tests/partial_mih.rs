use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{FilesRepo, MihPosting, NewFile, PartialMihRepo};
use vidcull_db::{Database, open_in_memory};

const T0: i64 = 1_700_000_000;

fn seed_file(db: &Database, path: &str) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        inode: None,
        content_hash: None,
        codec: Some(Codec::H264),
        container: None,
        duration: None,
        fps_x1000: None,
        bitrate_bps: None,
        resolution: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn posting(chunk: u32, slice: u64, file: FileId, scene: usize) -> MihPosting {
    MihPosting {
        chunk,
        slice_value: slice,
        file_id: file,
        scene_index: scene,
    }
}

#[test]
fn postings_round_trip_in_deterministic_order() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let repo = PartialMihRepo::new(db.conn());

    let inserted = [
        posting(0, 0x1234, f2, 1),
        posting(3, 0xFFFF, f1, 0),
        posting(0, 0x1234, f1, 0),
        posting(1, 0x00AB, f1, 2),
    ];
    for p in &inserted {
        repo.insert_posting(p).expect("insert posting");
    }

    let loaded = repo.load_all_postings().expect("load");
    let expected = vec![
        posting(0, 0x1234, f1, 0),
        posting(1, 0x00AB, f1, 2),
        posting(3, 0xFFFF, f1, 0),
        posting(0, 0x1234, f2, 1),
    ];
    assert_eq!(loaded, expected);
}

#[test]
fn full_width_slice_value_round_trips_loss_free() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let repo = PartialMihRepo::new(db.conn());
    let high = 0xFFFF_FFFF_FFFF_FFFF;
    repo.insert_posting(&posting(0, high, f1, 0))
        .expect("insert");
    let loaded = repo.load_all_postings().expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].slice_value, high);
}

#[test]
fn insert_postings_batch_matches_one_at_a_time_insert() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let repo = PartialMihRepo::new(db.conn());

    let batch = [
        posting(0, 0x1234, f2, 1),
        posting(3, 0xFFFF, f1, 0),
        posting(0, 0x1234, f1, 0),
        posting(1, 0x00AB, f1, 2),
    ];
    repo.insert_postings(&batch).expect("batch insert");

    let loaded = repo.load_all_postings().expect("load");
    let expected = vec![
        posting(0, 0x1234, f1, 0),
        posting(1, 0x00AB, f1, 2),
        posting(3, 0xFFFF, f1, 0),
        posting(0, 0x1234, f2, 1),
    ];
    assert_eq!(loaded, expected);
}

#[test]
fn insert_postings_batches_past_the_bind_limit_and_ignores_duplicates() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let repo = PartialMihRepo::new(db.conn());

    let mut batch: Vec<MihPosting> = (0..500u64).map(|i| posting(0, i, f1, 0)).collect();
    batch.extend((0..500u64).map(|i| posting(0, i, f1, 0)));
    repo.insert_postings(&batch).expect("large batch insert");

    assert_eq!(
        repo.load_all_postings().expect("load").len(),
        500,
        "duplicates within an over-sized batch are ignored, not double-inserted"
    );
}

#[test]
fn insert_postings_on_an_empty_slice_is_a_no_op() {
    let db = open_in_memory().expect("open db");
    let repo = PartialMihRepo::new(db.conn());
    repo.insert_postings(&[]).expect("empty batch is fine");
    assert!(repo.load_all_postings().expect("load").is_empty());
}

#[test]
fn duplicate_posting_is_idempotent() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let repo = PartialMihRepo::new(db.conn());
    let p = posting(0, 0x42, f1, 0);
    repo.insert_posting(&p).expect("first");
    repo.insert_posting(&p)
        .expect("second is ignored, not an error");
    assert_eq!(repo.load_all_postings().expect("load").len(), 1);
}

#[test]
fn delete_file_postings_removes_only_that_file() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.insert_posting(&posting(0, 1, f1, 0)).unwrap();
    repo.insert_posting(&posting(1, 2, f1, 1)).unwrap();
    repo.insert_posting(&posting(0, 3, f2, 0)).unwrap();

    repo.delete_file_postings(f1).expect("delete");
    let loaded = repo.load_all_postings().expect("load");
    assert_eq!(loaded, vec![posting(0, 3, f2, 0)], "only f2 survives");
}

#[test]
fn scene_counts_upsert_and_round_trip() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.set_scene_count(f1, 40).unwrap();
    repo.set_scene_count(f2, 6).unwrap();
    repo.set_scene_count(f1, 42).unwrap();

    assert_eq!(
        repo.load_all_scene_counts().expect("load"),
        vec![(f1, 42), (f2, 6)],
    );

    repo.delete_scene_count(f1).unwrap();
    assert_eq!(repo.load_all_scene_counts().expect("load"), vec![(f2, 6)]);
}

#[test]
fn clear_wipes_both_tables() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.insert_posting(&posting(0, 1, f1, 0)).unwrap();
    repo.set_scene_count(f1, 3).unwrap();

    repo.clear_postings().expect("clear postings");
    repo.clear_scene_counts().expect("clear counts");
    assert!(repo.load_all_postings().unwrap().is_empty());
    assert!(repo.load_all_scene_counts().unwrap().is_empty());
}

#[test]
fn candidate_files_returns_distinct_ids_for_matching_chunk_and_slices() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let f3 = seed_file(&db, "/v/c.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.insert_posting(&posting(0, 100, f1, 0)).unwrap();
    repo.insert_posting(&posting(0, 100, f1, 5)).unwrap();
    repo.insert_posting(&posting(0, 200, f2, 0)).unwrap();
    repo.insert_posting(&posting(1, 100, f3, 0)).unwrap();

    let got = repo.candidate_files(0, &[100, 200]).expect("candidates");
    assert_eq!(got, [f1, f2].into_iter().collect());

    assert!(repo.candidate_files(0, &[999]).unwrap().is_empty());

    assert_eq!(
        repo.candidate_files(1, &[100]).unwrap(),
        [f3].into_iter().collect(),
    );

    assert!(repo.candidate_files(0, &[]).unwrap().is_empty());
}

#[test]
fn candidate_files_batches_past_the_bind_limit() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.insert_posting(&posting(0, 1500, f1, 0)).unwrap();
    let keys: Vec<u64> = (0..2000u64).collect();
    let got = repo.candidate_files(0, &keys).expect("candidates");
    assert_eq!(got, [f1].into_iter().collect());
}

#[test]
fn count_short_counts_files_below_min_scenes() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let f3 = seed_file(&db, "/v/c.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.set_scene_count(f1, 40).unwrap();
    repo.set_scene_count(f2, 3).unwrap();
    repo.set_scene_count(f3, 5).unwrap();

    assert_eq!(repo.count_short(5).unwrap(), 1, "only f2 is < 5");
    assert_eq!(repo.count_short(6).unwrap(), 2, "f2 and f3 are < 6");
    assert_eq!(repo.count_short(1).unwrap(), 0);
}

#[test]
fn hard_deleting_a_file_cascades_to_postings_and_counts() {
    let db = open_in_memory().expect("open db");
    let f1 = seed_file(&db, "/v/a.mp4");
    let repo = PartialMihRepo::new(db.conn());
    repo.insert_posting(&posting(0, 1, f1, 0)).unwrap();
    repo.set_scene_count(f1, 3).unwrap();

    db.conn()
        .execute("DELETE FROM files WHERE id = ?1", [f1.0])
        .expect("hard delete file");

    assert!(
        repo.load_all_postings().unwrap().is_empty(),
        "postings cascade"
    );
    assert!(
        repo.load_all_scene_counts().unwrap().is_empty(),
        "scene counts cascade",
    );
}
