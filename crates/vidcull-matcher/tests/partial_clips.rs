use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, SimilarityEdgesRepo,
    TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::exact::rebuild_exact_groups;
use vidcull_matcher::near::{LshParams, rebuild_near_duplicate_groups};
use vidcull_matcher::partial::{AnchorParams, rebuild_partial_clip_groups};

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

fn seed_with_tier2(db: &Database, path: &str, tier2: &Tier2Fingerprint) -> FileId {
    seed_with_tier2_hash(db, path, None, tier2)
}

fn seed_with_tier2_hash(
    db: &Database,
    path: &str,
    content_hash: Option<Blake3Hash>,
    tier2: &Tier2Fingerprint,
) -> FileId {
    let id = seed_file(db, path, content_hash);
    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: tier2.scenes.first().map_or(0, |s| s.phash),
    };
    let tier1_global = format::encode_tier1(&t1).expect("encode tier1");
    let tier2_temporal = format::encode_tier2(tier2).expect("encode tier2");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global,
            tier2_temporal: Some(tier2_temporal),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
    id
}

#[test]
fn thirty_second_clip_is_grouped_with_its_source() -> Result<()> {
    let mut db = fresh_db();
    let source = source_seq(0x1234, 60);
    let clip = clip_of(&source, 20, 6, 4);

    let src_id = seed_with_tier2(&db, "/v/long.mp4", &source);
    let clip_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);

    let out = rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(out.groups_created, 1, "one clip → source match");
    assert_eq!(out.members_added, 2);
    assert_eq!(out.edges_added, 1);

    let groups = DuplicateGroupsRepo::new(db.conn());
    let group = groups.get(1)?.expect("the partial-clip group");
    assert_eq!(group.trust_level, TrustLevel::Possible);
    assert_eq!(group.created_at, T0);

    let mut members = groups.list_members(group.id)?;
    members.sort_unstable();
    let mut expected = vec![src_id, clip_id];
    expected.sort_unstable();
    assert_eq!(members, expected);

    let edges = SimilarityEdgesRepo::new(db.conn()).list_for_group(group.id)?;
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].score_x1000, 1000, "full coverage ⇒ score 1000");
    assert_eq!(edges[0].file_a, clip_id.min(src_id));
    assert_eq!(edges[0].file_b, clip_id.max(src_id));
    Ok(())
}

#[test]
fn compilation_groups_with_each_component() -> Result<()> {
    let mut db = fresh_db();
    let a = source_seq(0xAAAA, 8);
    let b = source_seq(0xBBBB, 9);
    let mut comp_scenes: Vec<SceneHash> = Vec::new();
    for (i, s) in a.scenes.iter().chain(b.scenes.iter()).enumerate() {
        comp_scenes.push(SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: flip_low_bits(s.phash, 3),
        });
    }
    let compilation = Tier2Fingerprint {
        scenes: comp_scenes,
    };

    let a_id = seed_with_tier2(&db, "/v/a.mp4", &a);
    let b_id = seed_with_tier2(&db, "/v/b.mp4", &b);
    let comp_id = seed_with_tier2(&db, "/v/compilation.mp4", &compilation);

    let out = rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(
        out.groups_created, 2,
        "A and B are both clips of the compilation"
    );
    assert_eq!(out.members_added, 4);

    let groups = DuplicateGroupsRepo::new(db.conn());
    let mut found_pairs: Vec<(FileId, FileId)> = Vec::new();
    for gid in 1..=2 {
        let mut members = groups.list_members(gid)?;
        members.sort_unstable();
        assert_eq!(members.len(), 2);
        found_pairs.push((members[0], members[1]));
    }
    assert!(found_pairs.contains(&(a_id.min(comp_id), a_id.max(comp_id))));
    assert!(found_pairs.contains(&(b_id.min(comp_id), b_id.max(comp_id))));
    Ok(())
}

#[test]
fn rebuild_is_idempotent_and_clears_prior_possible_groups() -> Result<()> {
    let mut db = fresh_db();
    let source = source_seq(0x5151, 40);
    let clip = clip_of(&source, 10, 6, 2);
    seed_with_tier2(&db, "/v/long.mp4", &source);
    seed_with_tier2(&db, "/v/clip.mp4", &clip);

    let first = rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(first.groups_cleared, 0, "nothing to clear on first pass");
    assert_eq!(first.groups_created, 1);

    let second = rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0 + 100)?;
    assert_eq!(
        second.groups_cleared, 1,
        "second pass clears the prior group"
    );
    assert_eq!(second.groups_created, 1, "and recreates the same match");
    Ok(())
}

#[test]
fn partial_rebuild_leaves_exact_and_very_likely_untouched() -> Result<()> {
    let mut db = fresh_db();

    let digest = Blake3Hash::from_bytes([0xCD; HASH_LEN]);
    let whole = source_seq(0xE1E1, 10);
    let e1 = seed_with_tier2_hash(&db, "/v/exact1.mp4", Some(digest), &whole);
    let e2 = seed_with_tier2_hash(&db, "/v/exact2.mp4", Some(digest), &whole);

    let source = source_seq(0x2727, 40);
    let clip = clip_of(&source, 5, 6, 2);
    seed_with_tier2(&db, "/v/source.mp4", &source);
    seed_with_tier2(&db, "/v/clip.mp4", &clip);

    rebuild_exact_groups(&mut db, T0)?;
    let groups = DuplicateGroupsRepo::new(db.conn());
    let exact_gid = groups
        .find_exact_group_containing(e1)?
        .expect("exact group exists");

    rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)?;

    let out = rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(out.groups_cleared, 0, "no prior POSSIBLE groups to clear");
    assert_eq!(out.groups_created, 1, "the one clip → source match");

    let groups = DuplicateGroupsRepo::new(db.conn());
    let exact = groups.get(exact_gid)?.expect("exact group survives");
    assert_eq!(exact.trust_level, TrustLevel::Exact);
    let mut exact_members = groups.list_members(exact_gid)?;
    exact_members.sort_unstable();
    let mut expected = vec![e1, e2];
    expected.sort_unstable();
    assert_eq!(exact_members, expected, "EXACT membership intact");

    let very_likely_present = (1..=16).any(|gid| {
        groups
            .get(gid)
            .ok()
            .flatten()
            .is_some_and(|g| g.trust_level == TrustLevel::VeryLikely)
    });
    assert!(
        very_likely_present,
        "VERY_LIKELY grouping survives the partial rebuild"
    );
    Ok(())
}

#[test]
fn videos_without_tier2_are_ignored() -> Result<()> {
    let mut db = fresh_db();
    let id = seed_file(&db, "/v/tier1only.mp4", None);
    let t1 = Tier1Fingerprint {
        duration_ms: 1000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: 0xDEAD_BEEF,
    };
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert");

    let out = rebuild_partial_clip_groups(&mut db, AnchorParams::default(), T0)?;
    assert_eq!(out.groups_created, 0);
    assert_eq!(
        out.skipped_short, 0,
        "tier1-only rows never reach the planner"
    );
    Ok(())
}
