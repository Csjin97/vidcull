use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_db::open_file;
use vidcull_db::repo::{
    BatchFileRole, DeleteBatchMode, DeleteJournalRepo, FilesRepo, NewDeleteBatch, NewFile,
    TrustLevel,
};

fn open(path: &std::path::Path) -> vidcull_db::Database {
    open_file(path).expect("open file db")
}

fn insert_file(db: &vidcull_db::Database, path: &str) -> FileId {
    FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(path),
            ..Default::default()
        })
        .expect("insert file")
}

#[test]
fn committed_batch_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let batch_id: i64;
    let f1_raw: i64;
    {
        let db = open(&db_path);
        let f1 = insert_file(&db, "a.mp4");
        let f2 = insert_file(&db, "b.mp4");
        f1_raw = f1.0;

        let repo = DeleteJournalRepo::new(db.conn());
        let id = repo
            .record_pending(
                42,
                TrustLevel::Exact,
                false,
                Some(f1),
                DeleteBatchMode::Trash,
                &[(f1, "a.mp4".to_owned())],
                1_000,
            )
            .unwrap();

        repo.finalize_committed(
            id,
            true,
            &[
                (f1, "a.mp4".to_owned(), BatchFileRole::Deleted),
                (f2, "b.mp4".to_owned(), BatchFileRole::Survivor),
            ],
        )
        .unwrap();

        batch_id = id;
    }

    let db2 = open(&db_path);
    let repo2 = DeleteJournalRepo::new(db2.conn());

    let last = repo2
        .last()
        .unwrap()
        .expect("AC6.1: COMMITTED batch must survive reopen");

    assert_eq!(last.id, batch_id);
    assert_eq!(last.group_id, 42);
    assert!(last.group_dropped, "group_dropped=true must persist");
    assert_eq!(last.mode, DeleteBatchMode::Trash);
    assert_eq!(last.files.len(), 2, "both files must survive reopen");

    let deleted_file = last
        .files
        .iter()
        .find(|f| f.file_id.0 == f1_raw)
        .expect("deleted file must be present");
    assert_eq!(deleted_file.role, BatchFileRole::Deleted);
    assert_eq!(deleted_file.path, "a.mp4");

    let survivor = last
        .files
        .iter()
        .find(|f| f.file_id.0 != f1_raw)
        .expect("survivor file must be present");
    assert_eq!(survivor.role, BatchFileRole::Survivor);
    assert_eq!(survivor.path, "b.mp4");
}

#[test]
fn pending_batch_survives_reopen_and_excluded_from_last() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let batch_id: i64;
    {
        let db = open(&db_path);
        let f1 = insert_file(&db, "c.mp4");

        let repo = DeleteJournalRepo::new(db.conn());
        batch_id = repo
            .record_pending(
                99,
                TrustLevel::VeryLikely,
                false,
                None,
                DeleteBatchMode::Permanent,
                &[(f1, "c.mp4".to_owned())],
                2_000,
            )
            .unwrap();
    }

    let db2 = open(&db_path);
    let repo2 = DeleteJournalRepo::new(db2.conn());

    let pending = repo2.list_pending().unwrap();
    assert_eq!(pending.len(), 1, "AC6.2: PENDING batch must survive reopen");
    assert_eq!(pending[0].id, batch_id);
    assert_eq!(pending[0].group_id, 99);
    assert_eq!(pending[0].files.len(), 1);
    assert_eq!(pending[0].files[0].path, "c.mp4");

    assert!(
        repo2.last().unwrap().is_none(),
        "AC6.2: PENDING must not appear in last()"
    );
}

#[test]
fn v008_migration_status_default_committed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let db = open(&db_path);
    let f1 = insert_file(&db, "d.mp4");

    let repo = DeleteJournalRepo::new(db.conn());
    let id = repo
        .record(&NewDeleteBatch {
            group_id: 7,
            trust_level: TrustLevel::Possible,
            non_transitive: false,
            best_file_id: None,
            group_dropped: false,
            mode: DeleteBatchMode::Trash,
            files: &[(f1, "d.mp4".to_owned(), BatchFileRole::Deleted)],
            created_at: 3_000,
        })
        .unwrap();

    let last = repo
        .last()
        .unwrap()
        .expect("AC6.3: record() must use DEFAULT COMMITTED from v008 migration");
    assert_eq!(last.id, id);

    assert!(
        repo.list_pending().unwrap().is_empty(),
        "AC6.3: COMMITTED batch must not appear in list_pending()"
    );
}
