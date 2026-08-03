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
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
    rebuild_partial_clip_groups_from_fingerprints,
};
use vidcull_matcher::partial::{AnchorParams, partial_clip_params, rebuild_partial_clip_groups};

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
        codec: Some(Codec::H264),
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn upsert_fp(db: &Database, file_id: FileId, tier2: Option<&Tier2Fingerprint>, seed_phash: u64) {
    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: seed_phash,
    };
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id,
            tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
            tier2_temporal: tier2.map(|t| format::encode_tier2(t).expect("encode tier2")),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
}

fn set_partial(db: &Database, file_id: FileId, partial: &Tier2Fingerprint) {
    FingerprintsRepo::new(db.conn())
        .set_partial(
            file_id,
            &format::encode_tier2(partial).expect("encode tier2"),
        )
        .expect("set partial");
}

fn seed_with_partial(db: &Database, path: &str, partial: &Tier2Fingerprint) -> FileId {
    let id = seed_file(db, path);
    upsert_fp(db, id, None, partial.scenes.first().map_or(0, |s| s.phash));
    set_partial(db, id, partial);
    id
}

fn seed_with_both(
    db: &Database,
    path: &str,
    tier2: &Tier2Fingerprint,
    partial: &Tier2Fingerprint,
) -> FileId {
    let id = seed_file(db, path);
    upsert_fp(
        db,
        id,
        Some(tier2),
        tier2.scenes.first().map_or(0, |s| s.phash),
    );
    set_partial(db, id, partial);
    id
}

fn members_snapshot(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut snapshots = Vec::new();
    for gid in 1..=512 {
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

fn sorted_pair(a: FileId, b: FileId) -> Vec<i64> {
    let mut v = vec![a.0, b.0];
    v.sort_unstable();
    v
}

fn reference_partial(corpus: &[(&str, &Tier2Fingerprint)]) -> Result<Vec<Vec<i64>>> {
    let mut reference = open_in_memory()?;
    for (path, fp) in corpus {
        seed_with_partial(&reference, path, fp);
    }
    rebuild_partial_clip_groups_from_fingerprints(&mut reference, partial_clip_params(), T0)?;
    Ok(members_snapshot(&reference))
}

fn reference_tier2(corpus: &[(&str, &Tier2Fingerprint)]) -> Result<Vec<Vec<i64>>> {
    let mut reference = open_in_memory()?;
    for (path, fp) in corpus {
        let id = seed_file(&reference, path);
        upsert_fp(
            &reference,
            id,
            Some(fp),
            fp.scenes.first().map_or(0, |s| s.phash),
        );
    }
    rebuild_partial_clip_groups(&mut reference, AnchorParams::default(), T0)?;
    Ok(members_snapshot(&reference))
}

#[test]
fn partial_on_durable_incremental_equals_from_scratch() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial);

    let source = source_seq(0x1234, 40);
    let clip1 = clip_of(&source, 10, 6, 3);
    let s_id = seed_with_partial(&db, "/v/source.mp4", &source);
    let c1_id = seed_with_partial(&db, "/v/clip1.mp4", &clip1);
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(
        members_snapshot(&db),
        reference_partial(&[("/v/source.mp4", &source), ("/v/clip1.mp4", &clip1)])?,
        "cold start equals a partial-ON full rebuild",
    );

    let clip2 = clip_of(&source, 24, 6, 2);
    let c2_id = seed_with_partial(&db, "/v/clip2.mp4", &clip2);
    let source2 = source_embedding(0xBEEF, 40, 14, &clip1);
    let embed_id = seed_with_partial(&db, "/v/source2.mp4", &source2);
    let changed: BTreeSet<FileId> = [c2_id, embed_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &changed)?;

    let expect = reference_partial(&[
        ("/v/source.mp4", &source),
        ("/v/clip1.mp4", &clip1),
        ("/v/clip2.mp4", &clip2),
        ("/v/source2.mp4", &source2),
    ])?;
    assert_eq!(
        members_snapshot(&db),
        expect,
        "burst equals a partial-ON full rebuild"
    );
    let mut want = vec![
        sorted_pair(s_id, c1_id),
        sorted_pair(s_id, c2_id),
        sorted_pair(embed_id, c1_id),
    ];
    want.sort();
    assert_eq!(members_snapshot(&db), want);
    Ok(())
}

#[allow(clippy::similar_names)]
#[test]
fn partial_toggle_roundtrip_equals_from_scratch_each_transition() -> Result<()> {
    let mut db = open_in_memory()?;
    let source = source_seq(0x5151, 40);
    let clip = clip_of(&source, 12, 6, 3);
    let s_id = seed_with_both(&db, "/v/source.mp4", &source, &source);
    let c_id = seed_with_both(&db, "/v/clip.mp4", &clip, &clip);

    let mut index = PartialClipIndex::new(AnchorParams::default());
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(
        members_snapshot(&db),
        reference_tier2(&[("/v/source.mp4", &source), ("/v/clip.mp4", &clip)])?,
        "OFF equals a tier2 full rebuild",
    );
    assert_eq!(members_snapshot(&db), vec![sorted_pair(s_id, c_id)]);

    index = PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 1, &BTreeSet::new())?;
    assert_eq!(
        members_snapshot(&db),
        reference_partial(&[("/v/source.mp4", &source), ("/v/clip.mp4", &clip)])?,
        "ON after toggle equals a partial full rebuild",
    );

    let clip2 = clip_of(&source, 26, 6, 2);
    let c2_id = seed_with_partial(&db, "/v/clip2.mp4", &clip2);
    let changed: BTreeSet<FileId> = [c2_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 2, &changed)?;
    assert_eq!(
        members_snapshot(&db),
        reference_partial(&[
            ("/v/source.mp4", &source),
            ("/v/clip.mp4", &clip),
            ("/v/clip2.mp4", &clip2),
        ])?,
        "ON incremental equals a partial full rebuild of the changed corpus",
    );

    index = PartialClipIndex::new(AnchorParams::default());
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 3, &BTreeSet::new())?;
    assert_eq!(
        members_snapshot(&db),
        reference_tier2(&[("/v/source.mp4", &source), ("/v/clip.mp4", &clip)])?,
        "OFF after round-trip equals a tier2 full rebuild (clip2 absent from tier2)",
    );
    assert_eq!(members_snapshot(&db), vec![sorted_pair(s_id, c_id)]);
    Ok(())
}
