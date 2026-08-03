use std::collections::BTreeSet;

use vidcull_core::Result;
use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_matcher::near::{
    LshParams, rebuild_near_duplicate_groups, rebuild_near_duplicate_groups_incremental,
};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

const BASE_A: u64 = 0x0F0F_0F0F_0F0F_0F0F;
const BASE_B: u64 = 0xF0F0_F0F0_F0F0_F0F0;

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn seed_file(db: &Database, path: &str) -> FileId {
    let new_file = NewFile {
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
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn set_phash(db: &Database, file_id: FileId, phash: u64) {
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

fn seed_with_phash(db: &Database, path: &str, phash: u64) -> FileId {
    let id = seed_file(db, path);
    set_phash(db, id, phash);
    id
}

fn members_snapshot(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut snapshots = Vec::new();
    for gid in 1..=256 {
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

#[test]
fn incremental_after_additions_matches_full_rebuild() -> Result<()> {
    let mut db = open_in_memory()?;
    let _f1 = seed_with_phash(&db, "/v/a1.mp4", BASE_A);
    let _f2 = seed_with_phash(&db, "/v/a2.mp4", flip_low_bits(BASE_A, 2));
    let _f3 = seed_with_phash(&db, "/v/b1.mp4", BASE_B);
    rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;

    let f4 = seed_with_phash(&db, "/v/a3.mkv", flip_low_bits(BASE_A, 4));
    let f5 = seed_with_phash(&db, "/v/b2.mkv", flip_low_bits(BASE_B, 3));
    let changed: BTreeSet<FileId> = [f4, f5].into_iter().collect();

    let out =
        rebuild_near_duplicate_groups_incremental(&mut db, LshParams::default(), T0, &changed)?;
    assert_eq!(out.groups_cleared, 1, "the prior A group is cleared");
    assert_eq!(out.groups_created, 2, "A-cluster and B-cluster");

    let mut reference = open_in_memory()?;
    seed_with_phash(&reference, "/v/a1.mp4", BASE_A);
    seed_with_phash(&reference, "/v/a2.mp4", flip_low_bits(BASE_A, 2));
    seed_with_phash(&reference, "/v/b1.mp4", BASE_B);
    seed_with_phash(&reference, "/v/a3.mkv", flip_low_bits(BASE_A, 4));
    seed_with_phash(&reference, "/v/b2.mkv", flip_low_bits(BASE_B, 3));
    rebuild_near_duplicate_groups(&mut reference, LshParams::default(), T0)?;

    assert_eq!(
        members_snapshot(&db),
        members_snapshot(&reference),
        "incremental grouping must equal a full rebuild of the final corpus",
    );
    Ok(())
}

#[test]
fn incremental_after_mutating_a_member_drops_its_stale_edges() -> Result<()> {
    let mut db = open_in_memory()?;
    let f1 = seed_with_phash(&db, "/v/a1.mp4", BASE_A);
    let f2 = seed_with_phash(&db, "/v/a2.mp4", flip_low_bits(BASE_A, 2));
    let f3 = seed_with_phash(&db, "/v/a3.mp4", flip_low_bits(BASE_A, 4));
    rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(members_snapshot(&db), vec![vec![f1.0, f2.0, f3.0]]);

    set_phash(&db, f2, BASE_B);
    let changed: BTreeSet<FileId> = [f2].into_iter().collect();
    rebuild_near_duplicate_groups_incremental(&mut db, LshParams::default(), T0, &changed)?;

    let mut reference = open_in_memory()?;
    seed_with_phash(&reference, "/v/a1.mp4", BASE_A);
    seed_with_phash(&reference, "/v/a2.mp4", BASE_B);
    seed_with_phash(&reference, "/v/a3.mp4", flip_low_bits(BASE_A, 4));
    rebuild_near_duplicate_groups(&mut reference, LshParams::default(), T0)?;

    assert_eq!(members_snapshot(&db), members_snapshot(&reference));
    assert_eq!(
        members_snapshot(&db),
        vec![vec![f1.0, f3.0]],
        "f2 left the cluster; f1 and a3 remain grouped",
    );
    Ok(())
}

#[test]
fn incremental_cold_start_equals_full_when_all_changed() -> Result<()> {
    let mut db = open_in_memory()?;
    let a = seed_with_phash(&db, "/v/a.mp4", BASE_A);
    let b = seed_with_phash(&db, "/v/b.mp4", flip_low_bits(BASE_A, 3));
    let c = seed_with_phash(&db, "/v/c.mp4", BASE_B);
    let changed: BTreeSet<FileId> = [a, b, c].into_iter().collect();

    let out =
        rebuild_near_duplicate_groups_incremental(&mut db, LshParams::default(), T0, &changed)?;
    assert_eq!(out.groups_cleared, 0, "cold start clears nothing");
    assert_eq!(out.groups_created, 1);
    assert_eq!(members_snapshot(&db), vec![vec![a.0, b.0]]);
    Ok(())
}

#[test]
fn incremental_excludes_soft_deleted_without_changed_entry() -> Result<()> {
    let mut db = open_in_memory()?;
    let a = seed_with_phash(&db, "/v/a.mp4", BASE_A);
    let b = seed_with_phash(&db, "/v/b.mp4", flip_low_bits(BASE_A, 2));
    let c = seed_with_phash(&db, "/v/c.mp4", flip_low_bits(BASE_A, 4));
    rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;
    assert_eq!(members_snapshot(&db), vec![vec![a.0, b.0, c.0]]);

    FilesRepo::new(db.conn()).mark_deleted(c, T0 + 1)?;
    let changed: BTreeSet<FileId> = BTreeSet::new();
    rebuild_near_duplicate_groups_incremental(&mut db, LshParams::default(), T0 + 2, &changed)?;

    assert_eq!(
        members_snapshot(&db),
        vec![vec![a.0, b.0]],
        "the soft-deleted file drops out without any `changed` entry",
    );
    Ok(())
}
