use std::collections::BTreeSet;

use vidcull_core::Result;
use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile, PartialMihRepo};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::AnchorParams;
use vidcull_matcher::partial::durable::{PartialClipIndex, rebuild_partial_clip_groups_durable};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;
const CHUNKS: usize = 4;

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
        codec: Some(Codec::H264),
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

fn possible_member_pairs(db: &Database) -> Vec<Vec<i64>> {
    use vidcull_db::repo::{DuplicateGroupsRepo, TrustLevel};
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut out = Vec::new();
    for gid in 1..=512 {
        if let Some(group) = repo.get(gid).expect("get group") {
            if group.trust_level == TrustLevel::Possible {
                let mut m: Vec<i64> = repo
                    .list_members(gid)
                    .expect("members")
                    .into_iter()
                    .map(|f| f.0)
                    .collect();
                m.sort_unstable();
                out.push(m);
            }
        }
    }
    out.sort();
    out
}

fn rebuild(index: &mut PartialClipIndex, db: &mut Database, changed: &BTreeSet<FileId>) {
    rebuild_partial_clip_groups_durable(index, db, T0, changed).expect("rebuild");
}

#[test]
fn cold_plan_persists_postings_and_scene_counts() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());
    let source = source_seq(0x1234, 40);
    let clip = clip_of(&source, 10, 6, 3);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);

    rebuild(&mut index, &mut db, &BTreeSet::new());

    let pmih = PartialMihRepo::new(db.conn());
    let postings = pmih.load_all_postings()?;
    assert_eq!(
        postings.len(),
        (40 + 6) * CHUNKS,
        "source+clip postings persisted"
    );
    assert_eq!(pmih.load_all_scene_counts()?, vec![(s_id, 40), (c_id, 6)],);
    Ok(())
}

#[test]
fn restart_reconstructs_grouping_without_decoding_unchanged_files() -> Result<()> {
    let mut db = open_in_memory()?;
    let source = source_seq(0x2222, 40);
    let clip = clip_of(&source, 8, 6, 2);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    {
        let mut index1 = PartialClipIndex::new(AnchorParams::default());
        rebuild(&mut index1, &mut db, &BTreeSet::new());
    }
    let mut want = vec![{
        let mut v = vec![s_id.0, c_id.0];
        v.sort_unstable();
        v
    }];
    want.sort();
    assert_eq!(
        possible_member_pairs(&db),
        want,
        "cold plan grouped the pair"
    );

    db.conn()
        .execute(
            "UPDATE fingerprints SET tier2_temporal = NULL WHERE file_id = ?1",
            [s_id.0],
        )
        .expect("null source tier2");

    let mut index2 = PartialClipIndex::new(AnchorParams::default());
    rebuild(&mut index2, &mut db, &BTreeSet::new());
    assert_eq!(
        index2.last_rediscovered(),
        0,
        "an empty-delta restart rediscovers nothing",
    );
    assert_eq!(
        possible_member_pairs(&db),
        want,
        "grouping survives a restart that cannot decode the unchanged source",
    );
    Ok(())
}

#[test]
fn reindexing_a_file_replaces_its_postings_not_appends() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());
    let source = source_seq(0x3333, 40);
    let clip = clip_of(&source, 10, 6, 3);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    rebuild(&mut index, &mut db, &BTreeSet::new());

    let clip2 = clip_of(&source, 25, 6, 2);
    set_tier2(&db, c_id, &clip2);
    rebuild(&mut index, &mut db, &[c_id].into_iter().collect());

    let pmih = PartialMihRepo::new(db.conn());
    assert_eq!(pmih.load_all_postings()?.len(), (40 + 6) * CHUNKS);
    assert_eq!(pmih.scene_count(c_id)?, Some(6));
    assert_eq!(pmih.scene_count(s_id)?, Some(40));
    Ok(())
}

#[test]
fn soft_deleting_a_file_drops_its_durable_rows() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());
    let source = source_seq(0x4444, 40);
    let clip = clip_of(&source, 5, 6, 2);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    rebuild(&mut index, &mut db, &BTreeSet::new());
    assert_eq!(PartialMihRepo::new(db.conn()).scene_count(c_id)?, Some(6));

    FilesRepo::new(db.conn()).mark_deleted(c_id, T0 + 1)?;
    rebuild(&mut index, &mut db, &[c_id].into_iter().collect());

    let pmih = PartialMihRepo::new(db.conn());
    assert_eq!(
        pmih.scene_count(c_id)?,
        None,
        "clip scene-count row dropped"
    );
    assert_eq!(pmih.load_all_postings()?.len(), 40 * CHUNKS);
    assert!(possible_member_pairs(&db).is_empty(), "the match is gone");
    assert_eq!(pmih.scene_count(s_id)?, Some(40));
    Ok(())
}

#[test]
fn restart_finds_a_new_source_embedding_an_unchanged_clip_via_db_postings() -> Result<()> {
    let mut db = open_in_memory()?;
    let clip = source_seq(0x77, 6);
    let unrelated = source_seq(0x88, 40);
    let clip_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    seed_with_tier2(&db, "/v/unrelated.mp4", &unrelated);
    {
        let mut index1 = PartialClipIndex::new(AnchorParams::default());
        rebuild(&mut index1, &mut db, &BTreeSet::new());
        assert!(
            possible_member_pairs(&db).is_empty(),
            "lone clip matches nothing"
        );
    }

    let source2 = source_embedding(0xBEEF, 40, 12, &clip);
    let embed_id = seed_with_tier2(&db, "/v/source2.mp4", &source2);

    let mut index2 = PartialClipIndex::new(AnchorParams::default());
    rebuild(&mut index2, &mut db, &[embed_id].into_iter().collect());
    assert_eq!(
        index2.last_rediscovered(),
        1,
        "only the changed source is searched"
    );

    let mut want = vec![{
        let mut v = vec![clip_id.0, embed_id.0];
        v.sort_unstable();
        v
    }];
    want.sort();
    assert_eq!(
        possible_member_pairs(&db),
        want,
        "match found via DB postings"
    );
    Ok(())
}
