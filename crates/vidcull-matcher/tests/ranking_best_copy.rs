use vidcull_core::Result;
use vidcull_core::types::{BestCopyMode, Codec, FileId, NormalizedPath, Resolution, VideoDuration};
use vidcull_db::repo::{DaemonSettingsRepo, DuplicateGroupsRepo, FilesRepo, NewFile, TrustLevel};
use vidcull_db::{Database, open_in_memory};
use vidcull_ipc::DaemonSettings;
use vidcull_matcher::ranking::{BestCopyOutcome, assign_best_copies};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn fresh_db() -> Database {
    open_in_memory().expect("open in-memory db")
}

fn seed_file(
    db: &Database,
    path: &str,
    resolution: Option<Resolution>,
    bitrate_bps: Option<i64>,
    codec: Option<Codec>,
    size_bytes: i64,
) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes,
        mtime_ns: MTIME,
        inode: None,
        content_hash: None,
        codec,
        container: None,
        duration: Some(VideoDuration::from_millis(60_000)),
        fps_x1000: None,
        bitrate_bps,
        resolution,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn()).insert(&new_file).expect("insert")
}

fn group_of(db: &Database, trust: TrustLevel, members: &[FileId]) -> i64 {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo.create(trust, T0).expect("create group");
    for m in members {
        repo.add_member(gid, *m).expect("add member");
    }
    gid
}

fn best_of(db: &Database, gid: i64) -> Option<FileId> {
    DuplicateGroupsRepo::new(db.conn())
        .get(gid)
        .expect("get")
        .expect("present")
        .best_file_id
}

#[expect(clippy::unnecessary_wraps, reason = "ergonomic test constructor")]
fn res(w: u32, h: u32) -> Option<Resolution> {
    Some(Resolution::new(w, h))
}

#[test]
fn higher_resolution_copy_is_chosen_as_best() -> Result<()> {
    let mut db = fresh_db();
    let sd = seed_file(
        &db,
        "/g/480p.mp4",
        res(640, 480),
        Some(2_000_000),
        Some(Codec::H264),
        50,
    );
    let hd = seed_file(
        &db,
        "/g/1080p.mp4",
        res(1920, 1080),
        Some(2_000_000),
        Some(Codec::H264),
        50,
    );
    let gid = group_of(&db, TrustLevel::Exact, &[sd, hd]);

    let out = assign_best_copies(&mut db, T0)?;
    assert_eq!(out.groups_updated, 1);
    assert_eq!(best_of(&db, gid), Some(hd), "1080p outranks 480p");
    Ok(())
}

#[test]
fn higher_bitrate_copy_wins_at_equal_resolution() -> Result<()> {
    let mut db = fresh_db();
    let low = seed_file(
        &db,
        "/g/low.mp4",
        res(1920, 1080),
        Some(4_000_000),
        Some(Codec::H264),
        100,
    );
    let high = seed_file(
        &db,
        "/g/high.mp4",
        res(1920, 1080),
        Some(9_000_000),
        Some(Codec::H264),
        100,
    );
    let gid = group_of(&db, TrustLevel::VeryLikely, &[low, high]);

    assign_best_copies(&mut db, T0)?;
    assert_eq!(best_of(&db, gid), Some(high), "9 Mbps outranks 4 Mbps");
    Ok(())
}

#[test]
fn possible_group_gets_best_copy_without_trust_escalation() -> Result<()> {
    let mut db = fresh_db();
    let a = seed_file(
        &db,
        "/g/a.mp4",
        res(1280, 720),
        Some(3_000_000),
        Some(Codec::H264),
        20,
    );
    let b = seed_file(
        &db,
        "/g/b.mp4",
        res(3840, 2160),
        Some(3_000_000),
        Some(Codec::H265),
        20,
    );
    let gid = group_of(&db, TrustLevel::Possible, &[a, b]);

    assign_best_copies(&mut db, T0)?;
    assert_eq!(best_of(&db, gid), Some(b), "4K member wins");

    let group = DuplicateGroupsRepo::new(db.conn())
        .get(gid)?
        .expect("present");
    assert_eq!(
        group.trust_level,
        TrustLevel::Possible,
        "trust level must remain POSSIBLE",
    );
    Ok(())
}

#[test]
fn soft_deleted_member_is_never_chosen_as_best() -> Result<()> {
    let mut db = fresh_db();
    let keep = seed_file(
        &db,
        "/g/keep.mp4",
        res(1920, 1080),
        Some(5_000_000),
        Some(Codec::H264),
        100,
    );
    let trashed = seed_file(
        &db,
        "/g/trashed.mp4",
        res(3840, 2160),
        Some(9_000_000),
        Some(Codec::H264),
        100,
    );
    let gid = group_of(&db, TrustLevel::Exact, &[keep, trashed]);
    FilesRepo::new(db.conn()).mark_deleted(trashed, T0 + 1)?;

    assign_best_copies(&mut db, T0)?;
    assert_eq!(
        best_of(&db, gid),
        Some(keep),
        "the higher-quality but soft-deleted copy is ineligible",
    );
    Ok(())
}

#[test]
fn group_with_no_active_members_clears_best_pointer() -> Result<()> {
    let mut db = fresh_db();
    let only = seed_file(
        &db,
        "/g/only.mp4",
        res(1920, 1080),
        Some(5_000_000),
        Some(Codec::H264),
        100,
    );
    let gid = group_of(&db, TrustLevel::Exact, &[only]);
    DuplicateGroupsRepo::new(db.conn()).set_best(gid, Some(only), T0)?;
    FilesRepo::new(db.conn()).mark_deleted(only, T0 + 1)?;

    let out = assign_best_copies(&mut db, T0 + 2)?;
    assert_eq!(out.groups_without_active_members, 1);
    assert_eq!(best_of(&db, gid), None, "best pointer cleared to NULL");
    Ok(())
}

#[test]
fn rerun_is_idempotent() -> Result<()> {
    let mut db = fresh_db();
    let a = seed_file(
        &db,
        "/g/a.mp4",
        res(1280, 720),
        Some(3_000_000),
        Some(Codec::H264),
        20,
    );
    let b = seed_file(
        &db,
        "/g/b.mp4",
        res(1920, 1080),
        Some(3_000_000),
        Some(Codec::H264),
        20,
    );
    let gid = group_of(&db, TrustLevel::Exact, &[a, b]);

    let first = assign_best_copies(&mut db, T0)?;
    assert_eq!(first.groups_updated, 1);
    assert_eq!(first.groups_unchanged, 0);

    let second = assign_best_copies(&mut db, T0 + 10)?;
    assert_eq!(second.groups_updated, 0, "no change on rerun");
    assert_eq!(second.groups_unchanged, 1);
    assert_eq!(best_of(&db, gid), Some(b));
    Ok(())
}

#[test]
fn empty_database_is_a_noop() -> Result<()> {
    let mut db = fresh_db();
    let out = assign_best_copies(&mut db, T0)?;
    assert_eq!(out, BestCopyOutcome::default());
    Ok(())
}

#[test]
fn synthetic_corpus_picks_the_best_copy_in_every_group_with_zero_false_positives() -> Result<()> {
    const GROUPS: usize = 40;
    let mut db = fresh_db();

    let ladder = [(640u32, 480u32), (1280, 720), (1920, 1080), (3840, 2160)];

    let mut expected: Vec<(i64, FileId)> = Vec::new();
    for g in 0..GROUPS {
        let mut members = Vec::new();
        let mut best: Option<(FileId, u64)> = None;
        for (variant, (w, h)) in ladder.iter().enumerate() {
            let codec = match variant % 3 {
                0 => Codec::H264,
                1 => Codec::H265,
                _ => Codec::Av1,
            };
            let variant_i64 = i64::try_from(variant).unwrap_or(0);
            let bitrate = 2_000_000 + variant_i64 * 500_000;
            let size = 10 + variant_i64 * 7;
            let path = format!("/corpus/g{g}/v{variant}.mp4");
            let fid = seed_file(&db, &path, res(*w, *h), Some(bitrate), Some(codec), size);
            members.push(fid);
            let pixels = u64::from(*w) * u64::from(*h);
            if best.is_none_or(|(_, p)| pixels > p) {
                best = Some((fid, pixels));
            }
        }
        let gid = group_of(&db, TrustLevel::VeryLikely, &members);
        expected.push((gid, best.expect("each group has members").0));
    }

    let out = assign_best_copies(&mut db, T0)?;
    assert_eq!(out.groups_updated, GROUPS);

    let mut false_positives = 0usize;
    for (gid, want) in expected {
        if best_of(&db, gid) != Some(want) {
            false_positives += 1;
        }
    }
    assert_eq!(
        false_positives, 0,
        "best-copy selection must have 0 false positives"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn seed_file_full(
    db: &Database,
    path: &str,
    resolution: Option<Resolution>,
    bitrate_bps: Option<i64>,
    codec: Option<Codec>,
    size_bytes: i64,
    container: Option<String>,
    encoder_tags: Option<String>,
) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes,
        mtime_ns: MTIME,
        inode: None,
        content_hash: None,
        codec,
        container,
        duration: Some(VideoDuration::from_millis(60_000)),
        fps_x1000: None,
        bitrate_bps,
        resolution,
        first_seen_at: T0,
        last_seen_at: T0,
        encoder_tags,
        ..Default::default()
    };
    FilesRepo::new(db.conn()).insert(&new_file).expect("insert")
}

#[test]
fn best_copy_mode_integration_flow() -> Result<()> {
    let mut db = fresh_db();

    let a = seed_file_full(
        &db,
        "/g/fileA.mp4",
        res(1920, 1080),
        Some(10_000_000),
        Some(Codec::H264),
        100_000_000,
        Some("mp4".to_string()),
        None,
    );

    let b = seed_file_full(
        &db,
        "/g/fileB.mkv",
        res(1280, 720),
        Some(1_500_000),
        Some(Codec::H265),
        10_000_000,
        Some("mkv".to_string()),
        Some("handbrake".to_string()),
    );

    let gid = group_of(&db, TrustLevel::Exact, &[a, b]);

    let out = assign_best_copies(&mut db, T0)?;
    assert_eq!(out.groups_updated, 1);
    assert_eq!(
        best_of(&db, gid),
        Some(a),
        "Default (Archival) mode should prefer the original file A"
    );

    let archival_settings = DaemonSettings {
        best_copy_mode: BestCopyMode::Archival,
        ..Default::default()
    };
    let payload = postcard::to_allocvec(&archival_settings).expect("serialize settings");
    DaemonSettingsRepo::new(db.conn()).save(&payload)?;

    let out = assign_best_copies(&mut db, T0 + 1)?;
    assert_eq!(out.groups_updated, 0);
    assert_eq!(out.groups_unchanged, 1);
    assert_eq!(
        best_of(&db, gid),
        Some(a),
        "Archival mode prefers original file A"
    );

    let min_size_settings = DaemonSettings {
        best_copy_mode: BestCopyMode::MinSize,
        ..Default::default()
    };
    let payload = postcard::to_allocvec(&min_size_settings).expect("serialize settings");
    DaemonSettingsRepo::new(db.conn()).save(&payload)?;

    let out = assign_best_copies(&mut db, T0 + 2)?;
    assert_eq!(
        out.groups_updated, 1,
        "Best copy should change to file B in MinSize mode"
    );
    assert_eq!(
        best_of(&db, gid),
        Some(b),
        "MinSize mode prefers smaller file B"
    );

    Ok(())
}

#[test]
fn best_copy_mode_compatible_flow() -> Result<()> {
    let mut db = fresh_db();

    let c = seed_file_full(
        &db,
        "/g/fileC.mkv",
        res(1920, 1080),
        Some(12_000_000),
        Some(Codec::H265),
        80_000_000,
        Some("mkv".to_string()),
        None,
    );

    let d = seed_file_full(
        &db,
        "/g/fileD.mp4",
        res(1920, 1080),
        Some(10_000_000),
        Some(Codec::H264),
        100_000_000,
        Some("mp4".to_string()),
        None,
    );

    let gid = group_of(&db, TrustLevel::Exact, &[c, d]);

    let archival_settings = DaemonSettings {
        best_copy_mode: BestCopyMode::Archival,
        ..Default::default()
    };
    let payload = postcard::to_allocvec(&archival_settings).expect("serialize");
    DaemonSettingsRepo::new(db.conn()).save(&payload)?;

    let out = assign_best_copies(&mut db, T0)?;
    assert_eq!(out.groups_updated, 1);
    assert_eq!(
        best_of(&db, gid),
        Some(c),
        "Archival mode prefers higher bitrate file C"
    );

    let compatible_settings = DaemonSettings {
        best_copy_mode: BestCopyMode::Compatible,
        ..Default::default()
    };
    let payload = postcard::to_allocvec(&compatible_settings).expect("serialize");
    DaemonSettingsRepo::new(db.conn()).save(&payload)?;

    let out = assign_best_copies(&mut db, T0 + 1)?;
    assert_eq!(
        out.groups_updated, 1,
        "Best copy should change to file D in Compatible mode"
    );
    assert_eq!(
        best_of(&db, gid),
        Some(d),
        "Compatible mode prefers H.264 MP4 file D"
    );

    Ok(())
}
