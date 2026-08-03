use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, SimilarityEdgesRepo,
    TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_matcher::exact::rebuild_exact_groups;
use vidcull_matcher::near::{LshParams, rebuild_near_duplicate_groups};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn fresh_db() -> Database {
    open_in_memory().expect("open in-memory db")
}

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
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
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn seed_with_phash(db: &Database, path: &str, phash: u64) -> FileId {
    let id = seed_file(db, path, None);
    let fp = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: phash,
    };
    let blob = format::encode_tier1(&fp).expect("encode tier1 envelope");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: blob,
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
    id
}

const BASE_A: u64 = 0x0F0F_0F0F_0F0F_0F0F;
const BASE_B: u64 = 0xF0F0_F0F0_F0F0_F0F0;

#[test]
fn resize_and_reencode_variants_land_in_one_very_likely_group() -> Result<()> {
    let mut db = fresh_db();
    let original = seed_with_phash(&db, "/v/original.mp4", BASE_A);
    let resized = seed_with_phash(&db, "/v/resized.mp4", flip_low_bits(BASE_A, 2));
    let reencoded = seed_with_phash(&db, "/v/reencoded.mkv", flip_low_bits(BASE_A, 4));

    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(out.groups_created, 1, "one near-duplicate component");
    assert_eq!(out.members_added, 3);
    assert_eq!(out.skipped_uninformative, 0);

    let repo = DuplicateGroupsRepo::new(db.conn());
    let group = repo.get(1)?.expect("the one new group");
    assert_eq!(group.trust_level, TrustLevel::VeryLikely);
    assert_eq!(group.created_at, T0);

    let mut members = repo.list_members(group.id)?;
    members.sort_unstable();
    let mut expected = vec![original, resized, reencoded];
    expected.sort_unstable();
    assert_eq!(members, expected);
    Ok(())
}

#[test]
fn distinct_videos_are_not_grouped() -> Result<()> {
    let mut db = fresh_db();
    seed_with_phash(&db, "/v/a.mp4", BASE_A);
    seed_with_phash(&db, "/v/b.mp4", BASE_B);

    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(out.groups_created, 0, "far-apart hashes form no group");
    assert_eq!(out.members_added, 0);
    Ok(())
}

#[test]
fn exact_identical_phash_pair_scores_full() -> Result<()> {
    let mut db = fresh_db();
    let f1 = seed_with_phash(&db, "/v/1.mp4", BASE_A);
    let f2 = seed_with_phash(&db, "/v/2.mp4", BASE_A);

    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(out.groups_created, 1);
    assert_eq!(out.edges_added, 1);

    let groups = DuplicateGroupsRepo::new(db.conn());
    let gid = groups.find_exact_group_containing(f1)?;
    assert_eq!(gid, None, "near grouping does not create EXACT rows");

    let group = groups.get(1)?.expect("group");
    let edges = SimilarityEdgesRepo::new(db.conn()).list_for_group(group.id)?;
    assert_eq!(edges.len(), 1);
    assert_eq!(
        edges[0].score_x1000, 1000,
        "distance 0 ⇒ perfect similarity"
    );
    assert_eq!(edges[0].file_a, f1.min(f2));
    assert_eq!(edges[0].file_b, f1.max(f2));
    Ok(())
}

#[test]
fn rebuild_leaves_exact_groups_untouched() -> Result<()> {
    let mut db = fresh_db();
    let digest = Blake3Hash::from_bytes([0xAB; HASH_LEN]);
    let e1 = seed_file(&db, "/v/exact1.mp4", Some(digest));
    let e2 = seed_file(&db, "/v/exact2.mp4", Some(digest));
    seed_with_phash_for(&db, e1, BASE_A);
    seed_with_phash_for(&db, e2, BASE_A);
    seed_with_phash(&db, "/v/c.mp4", BASE_B);
    seed_with_phash(&db, "/v/d.mp4", flip_low_bits(BASE_B, 3));

    rebuild_exact_groups(&mut db, T0)?;
    let exact_gid = DuplicateGroupsRepo::new(db.conn())
        .find_exact_group_containing(e1)?
        .expect("exact group exists");

    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(out.groups_cleared, 0, "first near rebuild clears nothing");

    let repo = DuplicateGroupsRepo::new(db.conn());
    let exact = repo.get(exact_gid)?.expect("exact group survives");
    assert_eq!(exact.trust_level, TrustLevel::Exact);
    let mut exact_members = repo.list_members(exact_gid)?;
    exact_members.sort_unstable();
    let mut expected = vec![e1, e2];
    expected.sort_unstable();
    assert_eq!(exact_members, expected);

    assert_eq!(out.groups_created, 2);
    Ok(())
}

#[test]
fn idempotent_rerun_yields_same_membership() -> Result<()> {
    let mut db = fresh_db();
    seed_with_phash(&db, "/v/a.mp4", BASE_A);
    seed_with_phash(&db, "/v/b.mp4", flip_low_bits(BASE_A, 3));

    let first = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(first.groups_cleared, 0);
    assert_eq!(first.groups_created, 1);
    let first_members = members_snapshot(&db);

    let second = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0 + 100)?;
    assert_eq!(
        second.groups_cleared, 1,
        "second pass clears the prior group"
    );
    assert_eq!(second.groups_created, 1);
    let second_members = members_snapshot(&db);

    assert_eq!(
        first_members, second_members,
        "rebuild is logically idempotent on membership",
    );
    Ok(())
}

#[test]
fn soft_deleted_files_are_excluded() -> Result<()> {
    let mut db = fresh_db();
    let a = seed_with_phash(&db, "/v/a.mp4", BASE_A);
    let b = seed_with_phash(&db, "/v/b.mp4", flip_low_bits(BASE_A, 2));
    let c = seed_with_phash(&db, "/v/c.mp4", flip_low_bits(BASE_A, 4));

    FilesRepo::new(db.conn()).mark_deleted(c, T0 + 1)?;

    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0 + 2)?;
    assert_eq!(out.members_added, 2, "only the two live files are grouped");

    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut members = repo.list_members(1)?;
    members.sort_unstable();
    let mut expected = vec![a, b];
    expected.sort_unstable();
    assert_eq!(members, expected);
    Ok(())
}

#[test]
fn uninformative_zero_phash_is_never_grouped() -> Result<()> {
    let mut db = fresh_db();
    seed_with_phash(&db, "/v/black1.mp4", 0);
    seed_with_phash(&db, "/v/black2.mp4", 0);

    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(
        out.groups_created, 0,
        "flat/black videos must not collapse together"
    );
    assert_eq!(out.skipped_uninformative, 2);
    Ok(())
}

fn seed_with_phash_for(db: &Database, file_id: FileId, phash: u64) {
    let fp = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: phash,
    };
    let blob = format::encode_tier1(&fp).expect("encode tier1 envelope");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id,
            tier1_global: blob,
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
}

fn members_snapshot(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut snapshots = Vec::new();
    for gid in 1..=64 {
        match repo.get(gid).expect("get group") {
            Some(group) if group.trust_level == TrustLevel::VeryLikely => {
                let mut members: Vec<i64> = repo
                    .list_members(gid)
                    .expect("members")
                    .into_iter()
                    .map(|f| f.0)
                    .collect();
                members.sort_unstable();
                snapshots.push(members);
            }
            _ => {}
        }
    }
    snapshots.sort();
    snapshots
}
