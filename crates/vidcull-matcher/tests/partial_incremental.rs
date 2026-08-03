use std::collections::BTreeSet;

use vidcull_core::Result;
use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::{
    AnchorParams, rebuild_partial_clip_groups, rebuild_partial_clip_groups_incremental,
};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
    let mut state = seed;
    let scenes = (0..n)
        .map(|i| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: splitmix64(&mut state) | 1,
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn clip_of(source: &Tier2Fingerprint, start: usize, len: usize, perturb: u32) -> Tier2Fingerprint {
    let scenes = source.scenes[start..start + len]
        .iter()
        .enumerate()
        .map(|(i, s)| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: flip_low_bits(s.phash, perturb),
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn source_embedding(seed: u64, n: usize, at: usize, clip: &Tier2Fingerprint) -> Tier2Fingerprint {
    let mut src = source_seq(seed, n);
    for (k, s) in clip.scenes.iter().enumerate() {
        src.scenes[at + k] = SceneHash {
            timestamp_ms: (at + k) as u64 * 1000,
            phash: s.phash,
        };
    }
    src
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

fn set_tier2(db: &Database, file_id: FileId, tier2: &Tier2Fingerprint) {
    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: tier2.scenes.first().map_or(0, |s| s.phash),
    };
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id,
            tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
            tier2_temporal: Some(format::encode_tier2(tier2).expect("encode tier2")),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
}

fn seed_with_tier2(db: &Database, path: &str, tier2: &Tier2Fingerprint) -> FileId {
    let id = seed_file(db, path);
    set_tier2(db, id, tier2);
    id
}

fn members_snapshot(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut snapshots = Vec::new();
    for gid in 1..=256 {
        match repo.get(gid).expect("get group") {
            Some(group) if group.trust_level == TrustLevel::Possible => {
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
    let source = source_seq(0x1234, 40);
    let clip1 = clip_of(&source, 10, 6, 3);
    let long_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c1_id = seed_with_tier2(&db, "/v/clip1.mp4", &clip1);
    rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;

    let clip2 = clip_of(&source, 25, 6, 2);
    let c2_id = seed_with_tier2(&db, "/v/clip2.mp4", &clip2);
    let source2 = source_embedding(0xBEEF, 40, 14, &clip1);
    let embed_id = seed_with_tier2(&db, "/v/source2.mp4", &source2);
    let changed: BTreeSet<FileId> = [c2_id, embed_id].into_iter().collect();

    let out =
        rebuild_partial_clip_groups_incremental(&mut db, AnchorParams::default(), T0, &changed)?;
    assert_eq!(
        out.groups_cleared, 1,
        "the prior clip1→source group is cleared"
    );
    assert_eq!(
        out.groups_created, 3,
        "clip1→source (carried), clip2→source (B), clip1→source2 (C)",
    );

    let mut reference = open_in_memory()?;
    seed_with_tier2(&reference, "/v/source.mp4", &source);
    seed_with_tier2(&reference, "/v/clip1.mp4", &clip1);
    seed_with_tier2(&reference, "/v/clip2.mp4", &clip2);
    seed_with_tier2(&reference, "/v/source2.mp4", &source2);
    rebuild_partial_clip_groups(&mut reference, AnchorParams::default(), T0)?;

    assert_eq!(
        members_snapshot(&db),
        members_snapshot(&reference),
        "incremental grouping must equal a full rebuild of the final corpus",
    );
    let mut expected = vec![
        sorted_pair(long_id, c1_id),
        sorted_pair(long_id, c2_id),
        sorted_pair(embed_id, c1_id),
    ];
    expected.sort();
    assert_eq!(members_snapshot(&db), expected);
    Ok(())
}

#[test]
fn incremental_after_mutating_a_source_matches_full() -> Result<()> {
    let mut db = open_in_memory()?;
    let source = source_seq(0x2222, 40);
    let clip = clip_of(&source, 8, 6, 2);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(members_snapshot(&db), vec![sorted_pair(s_id, c_id)]);

    let mutated = source_seq(0x9999, 40);
    set_tier2(&db, s_id, &mutated);
    let changed: BTreeSet<FileId> = [s_id].into_iter().collect();
    rebuild_partial_clip_groups_incremental(&mut db, AnchorParams::default(), T0, &changed)?;

    let mut reference = open_in_memory()?;
    seed_with_tier2(&reference, "/v/source.mp4", &mutated);
    seed_with_tier2(&reference, "/v/clip.mp4", &clip);
    rebuild_partial_clip_groups(&mut reference, AnchorParams::default(), T0)?;

    assert_eq!(members_snapshot(&db), members_snapshot(&reference));
    assert!(
        members_snapshot(&db).is_empty(),
        "the clip's source changed out from under it; the match is gone",
    );
    Ok(())
}

#[test]
fn incremental_cold_start_equals_full_when_all_changed() -> Result<()> {
    let mut db = open_in_memory()?;
    let source = source_seq(0x3333, 40);
    let clip = clip_of(&source, 12, 6, 3);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    let changed: BTreeSet<FileId> = [s_id, c_id].into_iter().collect();

    let out =
        rebuild_partial_clip_groups_incremental(&mut db, AnchorParams::default(), T0, &changed)?;
    assert_eq!(out.groups_cleared, 0, "cold start clears nothing");
    assert_eq!(out.groups_created, 1);
    assert_eq!(members_snapshot(&db), vec![sorted_pair(s_id, c_id)]);
    Ok(())
}

#[test]
fn incremental_excludes_soft_deleted_without_changed_entry() -> Result<()> {
    let mut db = open_in_memory()?;
    let source = source_seq(0x4444, 40);
    let clip = clip_of(&source, 5, 6, 2);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(members_snapshot(&db), vec![sorted_pair(s_id, c_id)]);

    FilesRepo::new(db.conn()).mark_deleted(c_id, T0 + 1)?;
    let changed: BTreeSet<FileId> = BTreeSet::new();
    rebuild_partial_clip_groups_incremental(&mut db, AnchorParams::default(), T0 + 2, &changed)?;

    assert!(
        members_snapshot(&db).is_empty(),
        "the soft-deleted clip drops out without any `changed` entry",
    );
    Ok(())
}

fn sorted_pair(a: FileId, b: FileId) -> Vec<i64> {
    let mut v = vec![a.0, b.0];
    v.sort_unstable();
    v
}
