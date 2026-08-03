use vidcull_core::Result;
use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, RegroupQueueRepo,
    TrustLevel,
};
use vidcull_db::{Database, open_file, open_in_memory};
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

fn index_file(db: &mut Database, path: &str, phash: u64) -> Result<FileId> {
    let fp = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: phash,
    };
    let blob = format::encode_tier1(&fp)?;
    db.transaction(|conn| {
        let id = FilesRepo::new(conn).insert(&NewFile {
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
        })?;
        FingerprintsRepo::new(conn).upsert(&Fingerprint {
            file_id: id,
            tier1_global: blob,
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })?;
        RegroupQueueRepo::new(conn).mark(id, T0)?;
        Ok(id)
    })
}

fn rebuild_from_delta(db: &mut Database) -> Result<()> {
    let changed = RegroupQueueRepo::new(db.conn()).load()?;
    rebuild_near_duplicate_groups_incremental(db, LshParams::default(), T0, &changed)?;
    db.transaction(|conn| RegroupQueueRepo::new(conn).clear(changed.iter().copied()))?;
    Ok(())
}

fn members_snapshot(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut snapshots = Vec::new();
    for gid in 1..=256 {
        if let Some(group) = repo.get(gid).expect("get group") {
            if group.trust_level == TrustLevel::VeryLikely {
                let mut members: Vec<i64> = repo
                    .list_members(gid)
                    .expect("members")
                    .into_iter()
                    .map(|f| f.0)
                    .collect();
                members.sort_unstable();
                snapshots.push(members);
            }
        }
    }
    snapshots.sort();
    snapshots
}

#[test]
fn durable_delta_survives_a_crash_before_the_trailing_rebuild() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("index.db");

    {
        let mut db = open_file(&db_path)?;
        index_file(&mut db, "/v/a1.mp4", BASE_A)?;
        index_file(&mut db, "/v/a2.mp4", flip_low_bits(BASE_A, 2))?;
        index_file(&mut db, "/v/b1.mp4", BASE_B)?;
        rebuild_from_delta(&mut db)?;
        assert!(
            RegroupQueueRepo::new(db.conn()).is_empty()?,
            "a clean rebuild clears the delta",
        );
    }

    let a3 = {
        let mut db = open_file(&db_path)?;
        index_file(&mut db, "/v/a3.mkv", flip_low_bits(BASE_A, 4))?
    };

    let mut db = open_file(&db_path)?;
    let delta = RegroupQueueRepo::new(db.conn()).load()?;
    assert!(
        delta.contains(&a3),
        "the pre-crash change survived in the durable delta: {delta:?}",
    );

    rebuild_from_delta(&mut db)?;

    let mut reference = open_in_memory()?;
    index_file(&mut reference, "/v/a1.mp4", BASE_A)?;
    index_file(&mut reference, "/v/a2.mp4", flip_low_bits(BASE_A, 2))?;
    index_file(&mut reference, "/v/b1.mp4", BASE_B)?;
    index_file(&mut reference, "/v/a3.mkv", flip_low_bits(BASE_A, 4))?;
    rebuild_near_duplicate_groups(&mut reference, LshParams::default(), T0)?;

    assert_eq!(
        members_snapshot(&db),
        members_snapshot(&reference),
        "the recovered grouping equals a full rebuild — the crashed change was not lost",
    );
    let groups = members_snapshot(&db);
    assert_eq!(groups.len(), 1, "exactly one A-cluster, got {groups:?}");
    assert_eq!(groups[0].len(), 3, "all three A-copies grouped");
    assert!(
        groups[0].contains(&a3.0),
        "a3 was recovered into the cluster after the crash",
    );
    assert!(
        RegroupQueueRepo::new(db.conn()).is_empty()?,
        "the recovery rebuild cleared the consumed delta",
    );
    Ok(())
}
