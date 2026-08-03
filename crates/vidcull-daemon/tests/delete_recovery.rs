use std::fs;
use std::path::Path;

use tempfile::{TempDir, tempdir};
use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_daemon::bridge::reconcile_pending_deletes;
use vidcull_db::Database;
use vidcull_db::repo::{
    BatchFileRole, DeleteBatchMode, DeleteJournalRepo, DuplicateGroupsRepo, FilesRepo, NewFile,
    TrustLevel,
};

const NOW: i64 = 1_000;

fn make_file(db: &Database, dir: &Path, name: &str) -> (FileId, String) {
    let path = dir.join(name);
    fs::write(&path, b"bytes").unwrap();
    let norm = path.to_string_lossy().replace('\\', "/");
    let id = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(&norm),
            ..Default::default()
        })
        .unwrap();
    (id, norm)
}

fn setup() -> (TempDir, Database, FileId, String, i64) {
    let dir = tempdir().unwrap();
    let db = vidcull_db::open_file(&dir.path().join("av.db")).unwrap();
    let (f1, p1) = make_file(&db, dir.path(), "f1.mp4");
    let (f2, _) = make_file(&db, dir.path(), "f2.mp4");
    let (f3, _) = make_file(&db, dir.path(), "f3.mp4");

    let groups = DuplicateGroupsRepo::new(db.conn());
    let gid = groups.create(TrustLevel::Exact, NOW).unwrap();
    groups.add_member(gid, f1).unwrap();
    groups.add_member(gid, f2).unwrap();
    groups.add_member(gid, f3).unwrap();
    groups.set_best(gid, Some(f2), NOW).unwrap();

    DeleteJournalRepo::new(db.conn())
        .record_pending(
            gid,
            TrustLevel::Exact,
            false,
            Some(f2),
            DeleteBatchMode::Trash,
            &[(f1, p1.clone())],
            NOW,
        )
        .unwrap();

    (dir, db, f1, p1, gid)
}

#[test]
fn finalizes_a_pending_batch_whose_file_left_disk() {
    let (_dir, mut db, f1, p1, gid) = setup();
    fs::remove_file(&p1).unwrap();

    let finalized = reconcile_pending_deletes(&mut db, NOW + 1).unwrap();
    assert_eq!(finalized, 1);

    let deleted_at = FilesRepo::new(db.conn())
        .get(f1)
        .unwrap()
        .unwrap()
        .deleted_at;
    assert!(deleted_at.is_some(), "f1 must be soft-deleted");
    let members = DuplicateGroupsRepo::new(db.conn())
        .list_members(gid)
        .unwrap();
    assert!(!members.contains(&f1), "f1 must be un-grouped");

    let journal = DeleteJournalRepo::new(db.conn());
    assert!(
        journal.list_pending().unwrap().is_empty(),
        "no longer pending"
    );
    let last = journal.last().unwrap().expect("batch is now committed");
    let deleted: Vec<FileId> = last
        .files
        .iter()
        .filter(|f| f.role == BatchFileRole::Deleted)
        .map(|f| f.file_id)
        .collect();
    assert_eq!(deleted, vec![f1], "f1 recorded as the deleted member");
}

#[test]
fn rolls_back_a_pending_batch_whose_file_is_still_present() {
    let (_dir, mut db, f1, _p1, gid) = setup();

    let finalized = reconcile_pending_deletes(&mut db, NOW + 1).unwrap();
    assert_eq!(finalized, 0);

    let deleted_at = FilesRepo::new(db.conn())
        .get(f1)
        .unwrap()
        .unwrap()
        .deleted_at;
    assert!(deleted_at.is_none(), "f1 must stay active");
    let members = DuplicateGroupsRepo::new(db.conn())
        .list_members(gid)
        .unwrap();
    assert!(members.contains(&f1), "f1 must stay grouped");

    let journal = DeleteJournalRepo::new(db.conn());
    assert!(
        journal.list_pending().unwrap().is_empty(),
        "intent rolled back"
    );
    assert!(journal.last().unwrap().is_none(), "nothing committed");
}

#[test]
fn reconcile_is_idempotent_on_a_second_run() {
    let (_dir, mut db, _f1, p1, _gid) = setup();
    fs::remove_file(&p1).unwrap();

    assert_eq!(reconcile_pending_deletes(&mut db, NOW + 1).unwrap(), 1);
    assert_eq!(reconcile_pending_deletes(&mut db, NOW + 2).unwrap(), 0);
}
