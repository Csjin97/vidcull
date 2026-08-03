use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, FileId, HASH_LEN, NormalizedPath};
use vidcull_db::repo::{DuplicateGroupsRepo, FilesRepo, NewFile, TrustLevel};
use vidcull_db::{Database, open_in_memory};
use vidcull_matcher::exact::rebuild_exact_groups;

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn fresh_db() -> Database {
    open_in_memory().expect("open in-memory db")
}

fn hash(byte: u8) -> Blake3Hash {
    Blake3Hash::from_bytes([byte; HASH_LEN])
}

fn seed_file(db: &Database, path: &str, content_hash: Option<Blake3Hash>) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: MTIME,
        inode: None,
        content_hash,
        codec: None,
        container: None,
        duration: None,
        fps_x1000: None,
        bitrate_bps: None,
        resolution: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn()).insert(&new_file).expect("insert")
}

#[test]
fn three_identical_hashes_form_one_exact_group() -> Result<()> {
    let mut db = fresh_db();
    let digest = hash(0xab);
    let f1 = seed_file(&db, "/a/one.mp4", Some(digest));
    let f2 = seed_file(&db, "/a/two.mp4", Some(digest));
    let f3 = seed_file(&db, "/a/three.mp4", Some(digest));

    let out = rebuild_exact_groups(&mut db, T0)?;
    assert_eq!(out.groups_created, 1, "single bucket → single group");
    assert_eq!(out.members_added, 3);
    assert_eq!(out.groups_extended, 0);
    assert_eq!(out.buckets_skipped_ambiguous, 0);

    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo
        .find_exact_group_containing(f1)?
        .expect("f1 belongs to a group");
    assert_eq!(repo.find_exact_group_containing(f2)?, Some(gid));
    assert_eq!(repo.find_exact_group_containing(f3)?, Some(gid));

    let group = repo.get(gid)?.expect("group row");
    assert_eq!(group.trust_level, TrustLevel::Exact);
    assert_eq!(group.created_at, T0);
    assert_eq!(group.updated_at, T0);

    let mut members = repo.list_members(gid)?;
    members.sort_unstable();
    assert_eq!(members, vec![f1, f2, f3]);
    Ok(())
}

#[test]
fn different_hashes_form_separate_groups() -> Result<()> {
    let mut db = fresh_db();
    let hash_a = hash(0xaa);
    let hash_b = hash(0xbb);
    let a1 = seed_file(&db, "/a/1.mp4", Some(hash_a));
    let a2 = seed_file(&db, "/a/2.mp4", Some(hash_a));
    let b1 = seed_file(&db, "/b/1.mp4", Some(hash_b));
    let b2 = seed_file(&db, "/b/2.mp4", Some(hash_b));

    let out = rebuild_exact_groups(&mut db, T0)?;
    assert_eq!(out.groups_created, 2);
    assert_eq!(out.members_added, 4);

    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid_a = repo
        .find_exact_group_containing(a1)?
        .expect("group containing a1");
    let gid_b = repo
        .find_exact_group_containing(b1)?
        .expect("group containing b1");
    assert_ne!(
        gid_a, gid_b,
        "different content hashes must be in different groups",
    );
    assert_eq!(repo.find_exact_group_containing(a2)?, Some(gid_a));
    assert_eq!(repo.find_exact_group_containing(b2)?, Some(gid_b));
    Ok(())
}

#[test]
fn singleton_hash_buckets_are_not_grouped() -> Result<()> {
    let mut db = fresh_db();
    let unique = seed_file(&db, "/a/only.mp4", Some(hash(0xcc)));

    let out = rebuild_exact_groups(&mut db, T0)?;
    assert_eq!(out.groups_created, 0);
    assert_eq!(out.members_added, 0);

    let repo = DuplicateGroupsRepo::new(db.conn());
    assert_eq!(
        repo.find_exact_group_containing(unique)?,
        None,
        "a lone hash must not produce a duplicate group",
    );
    Ok(())
}

#[test]
fn files_without_hash_are_excluded() -> Result<()> {
    let mut db = fresh_db();
    let _unhashed1 = seed_file(&db, "/a/1.mp4", None);
    let _unhashed2 = seed_file(&db, "/a/2.mp4", None);

    let out = rebuild_exact_groups(&mut db, T0)?;
    assert_eq!(out.groups_created, 0);
    assert_eq!(out.members_added, 0);
    Ok(())
}

#[test]
fn soft_deleted_files_are_excluded() -> Result<()> {
    let mut db = fresh_db();
    let digest = hash(0xdd);
    let f1 = seed_file(&db, "/a/1.mp4", Some(digest));
    let f2 = seed_file(&db, "/a/2.mp4", Some(digest));
    let f3 = seed_file(&db, "/a/3.mp4", Some(digest));

    FilesRepo::new(db.conn()).mark_deleted(f3, T0 + 1)?;

    let out = rebuild_exact_groups(&mut db, T0 + 2)?;
    assert_eq!(out.groups_created, 1);
    assert_eq!(out.members_added, 2);

    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo
        .find_exact_group_containing(f1)?
        .expect("group containing f1");
    let mut members = repo.list_members(gid)?;
    members.sort_unstable();
    assert_eq!(members, vec![f1, f2]);
    assert_eq!(
        repo.find_exact_group_containing(f3)?,
        None,
        "soft-deleted files must not join any group",
    );
    Ok(())
}

#[test]
fn idempotent_rerun_yields_no_changes() -> Result<()> {
    let mut db = fresh_db();
    let digest = hash(0xee);
    seed_file(&db, "/a/1.mp4", Some(digest));
    seed_file(&db, "/a/2.mp4", Some(digest));

    let first = rebuild_exact_groups(&mut db, T0)?;
    assert_eq!(first.groups_created, 1);
    assert_eq!(first.members_added, 2);

    let second = rebuild_exact_groups(&mut db, T0 + 100)?;
    assert_eq!(
        second.groups_created, 0,
        "second pass over an unchanged DB must not create new groups",
    );
    assert_eq!(second.groups_extended, 0);
    assert_eq!(second.members_added, 0);
    assert_eq!(second.buckets_skipped_ambiguous, 0);
    Ok(())
}

#[test]
fn new_member_extends_existing_group() -> Result<()> {
    let mut db = fresh_db();
    let digest = hash(0xff);
    let f1 = seed_file(&db, "/a/1.mp4", Some(digest));
    let f2 = seed_file(&db, "/a/2.mp4", Some(digest));
    rebuild_exact_groups(&mut db, T0)?;

    let f3 = seed_file(&db, "/a/3.mp4", Some(digest));
    let out = rebuild_exact_groups(&mut db, T0 + 60)?;
    assert_eq!(out.groups_created, 0);
    assert_eq!(out.groups_extended, 1);
    assert_eq!(out.members_added, 1);

    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo
        .find_exact_group_containing(f1)?
        .expect("original group");
    assert_eq!(repo.find_exact_group_containing(f2)?, Some(gid));
    assert_eq!(
        repo.find_exact_group_containing(f3)?,
        Some(gid),
        "newly arrived duplicate must join the existing group",
    );
    let mut members = repo.list_members(gid)?;
    members.sort_unstable();
    assert_eq!(members, vec![f1, f2, f3]);
    Ok(())
}

#[test]
fn soft_deleted_member_marks_extension_ambiguous() -> Result<()> {
    let mut db = fresh_db();
    let digest = hash(0x77);
    let f1 = seed_file(&db, "/a/1.mp4", Some(digest));
    let f2 = seed_file(&db, "/a/2.mp4", Some(digest));
    rebuild_exact_groups(&mut db, T0)?;

    FilesRepo::new(db.conn()).mark_deleted(f2, T0 + 1)?;

    let f3 = seed_file(&db, "/a/3.mp4", Some(digest));
    let out = rebuild_exact_groups(&mut db, T0 + 2)?;
    assert_eq!(out.groups_created, 0);
    assert_eq!(out.groups_extended, 0);
    assert_eq!(
        out.buckets_skipped_ambiguous, 1,
        "foreign (soft-deleted) member must trip the ambiguity guard",
    );

    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo
        .find_exact_group_containing(f1)?
        .expect("original group");
    let mut members = repo.list_members(gid)?;
    members.sort_unstable();
    assert_eq!(members, vec![f1, f2]);
    assert_eq!(
        repo.find_exact_group_containing(f3)?,
        None,
        "f3 is left ungrouped pending rebalance",
    );
    Ok(())
}
