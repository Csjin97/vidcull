use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_db::open_in_memory;
use vidcull_db::repo::{DuplicateGroupsRepo, FilesRepo, NewFile, TrustLevel};

fn insert_file(db: &vidcull_db::Database, path: &str) -> FileId {
    FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(path),
            ..Default::default()
        })
        .expect("insert file")
}

#[test]
fn add_member_duplicate_returns_err() {
    let db = open_in_memory().unwrap();
    let groups = DuplicateGroupsRepo::new(db.conn());

    let gid = groups.create(TrustLevel::Exact, 0).unwrap();
    let file = insert_file(&db, "dup.mp4");

    groups
        .add_member(gid, file)
        .expect("AC7.1: first add_member must succeed");

    let result = groups.add_member(gid, file);
    assert!(
        result.is_err(),
        "AC7.1: duplicate add_member must return Err, got Ok"
    );
}

#[test]
fn add_member_if_absent_is_idempotent() {
    let db = open_in_memory().unwrap();
    let groups = DuplicateGroupsRepo::new(db.conn());

    let gid = groups.create(TrustLevel::VeryLikely, 0).unwrap();
    let file_a = insert_file(&db, "a.mp4");
    let file_b = insert_file(&db, "b.mp4");

    groups
        .add_member_if_absent(gid, file_a)
        .expect("AC7.2: first add_member_if_absent must succeed");

    groups
        .add_member_if_absent(gid, file_a)
        .expect("AC7.2: second add_member_if_absent on duplicate must succeed (idempotent)");

    groups
        .add_member_if_absent(gid, file_b)
        .expect("AC7.2: add_member_if_absent for different file must succeed");

    let members = groups.list_members(gid).unwrap();
    assert_eq!(
        members.len(),
        2,
        "AC7.2: group must have exactly 2 members (file_a de-duped, file_b added)"
    );
    assert!(members.contains(&file_a), "file_a must be a member");
    assert!(members.contains(&file_b), "file_b must be a member");
}
