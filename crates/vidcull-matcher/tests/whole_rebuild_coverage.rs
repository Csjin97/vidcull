use vidcull_core::Result;
use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{FORMAT_VERSION, encode_tier2};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::whole::{WholeFileParams, rebuild_whole_file_groups};

const T0: i64 = 1_700_000_000;
const GRID_MS: u64 = vidcull_core::SPARSE_GRID_INTERVAL_MS;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn reencode_pair(seed: u64, n: usize) -> (Tier2Fingerprint, Tier2Fingerprint) {
    let mut st = seed;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| SceneHash {
            timestamp_ms: i as u64 * GRID_MS,
            phash: splitmix64(&mut st) | 1,
        })
        .collect();
    let b: Vec<SceneHash> = a
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let ph = if i % 4 == 0 {
                s.phash ^ 0b110
            } else {
                splitmix64(&mut st) | 1
            };
            SceneHash {
                timestamp_ms: s.timestamp_ms + GRID_MS,
                phash: ph,
            }
        })
        .collect();
    (
        Tier2Fingerprint { scenes: a },
        Tier2Fingerprint { scenes: b },
    )
}

fn seed_file(db: &Database, path: &str, tier2: Option<&Tier2Fingerprint>) -> Result<FileId> {
    let id = FilesRepo::new(db.conn()).insert(&NewFile {
        path: NormalizedPath::new(path),
        ..Default::default()
    })?;
    if let Some(tier2) = tier2 {
        FingerprintsRepo::new(db.conn()).upsert(&Fingerprint {
            file_id: id,
            tier1_global: vec![0],
            tier2_temporal: Some(encode_tier2(tier2)?),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })?;
    }
    Ok(id)
}

fn seed_group(db: &Database, trust: TrustLevel, members: &[FileId]) -> Result<i64> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo.create(trust, T0)?;
    for &m in members {
        repo.add_member(gid, m)?;
    }
    Ok(gid)
}

fn groups_at(db: &Database, trust: TrustLevel) -> Vec<(Vec<i64>, bool)> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut out: Vec<(Vec<i64>, bool)> = repo
        .list_all()
        .expect("list groups")
        .into_iter()
        .filter(|g| g.trust_level == trust)
        .map(|g| {
            let mut members: Vec<i64> = repo
                .list_members(g.id)
                .expect("members")
                .into_iter()
                .map(|f| f.0)
                .collect();
            members.sort_unstable();
            (members, g.non_transitive)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn pair_bridged_across_exact_and_near_groups_is_not_re_emitted() -> Result<()> {
    let mut db = open_in_memory()?;
    let (fa, fb) = reencode_pair(0xB21D_6E00, 400);
    let a = seed_file(&db, "/v/bridged_a.mp4", Some(&fa))?;
    let b = seed_file(&db, "/v/bridged_b.mp4", Some(&fb))?;
    let c = seed_file(&db, "/v/bridged_c.mp4", None)?;
    seed_group(&db, TrustLevel::Exact, &[a, c])?;
    seed_group(&db, TrustLevel::VeryLikely, &[b, c])?;

    let outcome = rebuild_whole_file_groups(&mut db, WholeFileParams::default(), T0)?;

    assert_eq!(
        outcome.groups_created, 0,
        "a pair already inside one transitive component must not be re-emitted"
    );
    let mut pre_existing = [b.0, c.0];
    pre_existing.sort_unstable();
    assert_eq!(
        groups_at(&db, TrustLevel::VeryLikely),
        vec![(pre_existing.to_vec(), false)],
        "the pre-existing transitive VERY_LIKELY group must be the only one left"
    );
    Ok(())
}

#[test]
fn possible_only_coverage_still_emits_the_whole_file_group() -> Result<()> {
    let mut db = open_in_memory()?;
    let (fa, fb) = reencode_pair(0x5EED_CAFE, 400);
    let a = seed_file(&db, "/v/possible_a.mp4", Some(&fa))?;
    let b = seed_file(&db, "/v/possible_b.mp4", Some(&fb))?;
    seed_group(&db, TrustLevel::Possible, &[a, b])?;

    let outcome = rebuild_whole_file_groups(&mut db, WholeFileParams::default(), T0)?;

    assert_eq!(
        outcome.groups_created, 1,
        "POSSIBLE-only coverage must not suppress the promotion"
    );
    let mut pair = [a.0, b.0];
    pair.sort_unstable();
    assert_eq!(
        groups_at(&db, TrustLevel::VeryLikely),
        vec![(pair.to_vec(), true)],
        "the pair must surface as the non_transitive VERY_LIKELY card"
    );
    Ok(())
}
