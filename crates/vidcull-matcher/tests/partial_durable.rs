use std::collections::BTreeSet;

use vidcull_core::Result;
use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, PartialMihRepo,
    SystemMetadataRepo, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
};
use vidcull_matcher::partial::rebuild_partial_clip_groups;
use vidcull_matcher::partial::{AnchorParams, partial_clip_params};

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

fn set_partial(db: &Database, file_id: FileId, partial: &Tier2Fingerprint) {
    FingerprintsRepo::new(db.conn())
        .set_partial(
            file_id,
            &format::encode_tier2(partial).expect("encode tier2"),
        )
        .expect("set partial");
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

fn reference_snapshot(corpus: &[(&str, &Tier2Fingerprint)]) -> Result<Vec<Vec<i64>>> {
    let mut reference = open_in_memory()?;
    for (path, fp) in corpus {
        seed_with_tier2(&reference, path, fp);
    }
    rebuild_partial_clip_groups(&mut reference, AnchorParams::default(), T0)?;
    Ok(members_snapshot(&reference))
}

#[test]
fn bootstrap_then_bursts_track_full_rebuild() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());

    let source = source_seq(0x1234, 40);
    let clip1 = clip_of(&source, 10, 6, 3);
    let long_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c1_id = seed_with_tier2(&db, "/v/clip1.mp4", &clip1);
    let out = rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(out.groups_created, 1, "the clip1→source pair");
    assert_eq!(members_snapshot(&db), vec![sorted_pair(long_id, c1_id)]);

    let clip2 = clip_of(&source, 25, 6, 2);
    let c2_id = seed_with_tier2(&db, "/v/clip2.mp4", &clip2);
    let source2 = source_embedding(0xBEEF, 40, 14, &clip1);
    let embed_id = seed_with_tier2(&db, "/v/source2.mp4", &source2);
    let changed: BTreeSet<FileId> = [c2_id, embed_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &changed)?;

    let expect = reference_snapshot(&[
        ("/v/source.mp4", &source),
        ("/v/clip1.mp4", &clip1),
        ("/v/clip2.mp4", &clip2),
        ("/v/source2.mp4", &source2),
    ])?;
    assert_eq!(
        members_snapshot(&db),
        expect,
        "burst 1 equals a full rebuild"
    );
    let mut want = vec![
        sorted_pair(long_id, c1_id),
        sorted_pair(long_id, c2_id),
        sorted_pair(embed_id, c1_id),
    ];
    want.sort();
    assert_eq!(members_snapshot(&db), want);

    let noise = source_seq(0x9999, 40);
    set_tier2(&db, long_id, &noise);
    let changed: BTreeSet<FileId> = [long_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &changed)?;
    let expect = reference_snapshot(&[
        ("/v/source.mp4", &noise),
        ("/v/clip1.mp4", &clip1),
        ("/v/clip2.mp4", &clip2),
        ("/v/source2.mp4", &source2),
    ])?;
    assert_eq!(
        members_snapshot(&db),
        expect,
        "burst 2 equals a full rebuild"
    );
    assert_eq!(
        members_snapshot(&db),
        vec![sorted_pair(embed_id, c1_id)],
        "only clip1↔source2 survives",
    );
    Ok(())
}

#[test]
fn removed_file_must_be_in_the_delta() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());

    let source = source_seq(0x4444, 40);
    let clip = clip_of(&source, 5, 6, 2);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(members_snapshot(&db), vec![sorted_pair(s_id, c_id)]);

    FilesRepo::new(db.conn()).mark_deleted(c_id, T0 + 1)?;
    let changed: BTreeSet<FileId> = [c_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 2, &changed)?;

    assert!(
        members_snapshot(&db).is_empty(),
        "the soft-deleted clip's match is gone",
    );
    let expect = reference_snapshot(&[("/v/source.mp4", &source)])?;
    assert_eq!(members_snapshot(&db), expect);
    Ok(())
}

#[test]
fn cold_bootstrap_with_many_pairs_equals_full() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());

    let mut reference_corpus: Vec<(String, Tier2Fingerprint)> = Vec::new();
    for s in 0..12u64 {
        let source = source_seq(0xC0DE + s, 40);
        let clip = clip_of(&source, 10, 6, 3);
        let sp = format!("/v/src{s}.mp4");
        let cp = format!("/v/clip{s}.mp4");
        seed_with_tier2(&db, &sp, &source);
        seed_with_tier2(&db, &cp, &clip);
        reference_corpus.push((sp, source));
        reference_corpus.push((cp, clip));
    }
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;

    let refs: Vec<(&str, &Tier2Fingerprint)> = reference_corpus
        .iter()
        .map(|(p, fp)| (p.as_str(), fp))
        .collect();
    assert_eq!(members_snapshot(&db), reference_snapshot(&refs)?);
    assert_eq!(members_snapshot(&db).len(), 12, "twelve planted pairs");
    Ok(())
}

#[test]
fn restart_loads_prior_edges_then_applies_pending_delta() -> Result<()> {
    let mut db = open_in_memory()?;

    let source = source_seq(0x5151, 40);
    let clip1 = clip_of(&source, 8, 6, 3);
    let clip2 = clip_of(&source, 22, 6, 2);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c1_id = seed_with_tier2(&db, "/v/clip1.mp4", &clip1);
    let c2_id = seed_with_tier2(&db, "/v/clip2.mp4", &clip2);
    {
        let mut index1 = PartialClipIndex::new(AnchorParams::default());
        rebuild_partial_clip_groups_durable(&mut index1, &mut db, T0, &BTreeSet::new())?;
    }
    let mut want = vec![sorted_pair(s_id, c1_id), sorted_pair(s_id, c2_id)];
    want.sort();
    assert_eq!(members_snapshot(&db), want, "run 1 grouped both clips");

    let mut index2 = PartialClipIndex::new(AnchorParams::default());
    let source2 = source_embedding(0xABAB, 40, 12, &clip1);
    let embed_id = seed_with_tier2(&db, "/v/source2.mp4", &source2);
    let changed: BTreeSet<FileId> = [embed_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index2, &mut db, T0 + 10, &changed)?;

    let expect = reference_snapshot(&[
        ("/v/source.mp4", &source),
        ("/v/clip1.mp4", &clip1),
        ("/v/clip2.mp4", &clip2),
        ("/v/source2.mp4", &source2),
    ])?;
    assert_eq!(
        members_snapshot(&db),
        expect,
        "restart load + pending delta equals a full rebuild",
    );
    let mut want = vec![
        sorted_pair(s_id, c1_id),
        sorted_pair(s_id, c2_id),
        sorted_pair(embed_id, c1_id),
    ];
    want.sort();
    assert_eq!(members_snapshot(&db), want);

    FilesRepo::new(db.conn()).mark_deleted(c2_id, T0 + 11)?;
    let changed: BTreeSet<FileId> = [c2_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index2, &mut db, T0 + 12, &changed)?;
    let mut want = vec![sorted_pair(s_id, c1_id), sorted_pair(embed_id, c1_id)];
    want.sort();
    assert_eq!(
        members_snapshot(&db),
        want,
        "clip2's match drops; clip1's two matches survive",
    );
    Ok(())
}

const RECONCILED_KEY: &str = "partial_index_reconciled";

#[test]
fn zero_match_corpus_skips_full_replan_on_restart() -> Result<()> {
    let mut db = open_in_memory()?;

    let a = source_seq(0x10, 40);
    let b = source_seq(0x20, 40);
    let lone = source_seq(0x30, 6);
    seed_with_tier2(&db, "/v/a.mp4", &a);
    seed_with_tier2(&db, "/v/b.mp4", &b);
    seed_with_tier2(&db, "/v/lone.mp4", &lone);

    {
        let mut index1 = PartialClipIndex::new(AnchorParams::default());
        rebuild_partial_clip_groups_durable(&mut index1, &mut db, T0, &BTreeSet::new())?;
        assert!(
            members_snapshot(&db).is_empty(),
            "this corpus has no partial-clip matches",
        );
    }
    assert!(
        SystemMetadataRepo::new(db.conn())
            .contains(RECONCILED_KEY)
            .expect("marker read"),
        "the first reconcile must record the durable marker even with zero matches",
    );

    let mut index2 = PartialClipIndex::new(AnchorParams::default());
    rebuild_partial_clip_groups_durable(&mut index2, &mut db, T0 + 1, &BTreeSet::new())?;
    assert_eq!(
        index2.last_rediscovered(),
        0,
        "a restart over a match-free corpus must not replan the whole active set",
    );
    assert!(members_snapshot(&db).is_empty(), "still no matches");
    Ok(())
}

#[test]
fn reconciled_marker_lets_restart_find_a_new_match_via_delta_only() -> Result<()> {
    let mut db = open_in_memory()?;

    let clip = source_seq(0x77, 6);
    let unrelated = source_seq(0x88, 40);
    let clip_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    seed_with_tier2(&db, "/v/unrelated.mp4", &unrelated);
    {
        let mut index1 = PartialClipIndex::new(AnchorParams::default());
        rebuild_partial_clip_groups_durable(&mut index1, &mut db, T0, &BTreeSet::new())?;
        assert!(
            members_snapshot(&db).is_empty(),
            "the lone clip matches nothing yet",
        );
    }

    let source2 = source_embedding(0xBEEF, 40, 12, &clip);
    let embed_id = seed_with_tier2(&db, "/v/source2.mp4", &source2);

    let mut index2 = PartialClipIndex::new(AnchorParams::default());
    let changed: BTreeSet<FileId> = [embed_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index2, &mut db, T0 + 1, &changed)?;
    assert_eq!(
        index2.last_rediscovered(),
        1,
        "only the one changed source is rediscovered, not the whole corpus",
    );
    assert_eq!(members_snapshot(&db), vec![sorted_pair(clip_id, embed_id)]);

    let expect = reference_snapshot(&[
        ("/v/clip.mp4", &clip),
        ("/v/unrelated.mp4", &unrelated),
        ("/v/source2.mp4", &source2),
    ])?;
    assert_eq!(
        members_snapshot(&db),
        expect,
        "delta-only restart equals a full rebuild of the post-change corpus",
    );
    Ok(())
}

#[allow(clippy::similar_names)]
#[test]
fn partial_clip_groups_form_independently_of_near_dup_groups() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());

    let source = source_seq(0xAB_CD_EF, 40);
    let clip = clip_of(&source, 15, 6, 3);

    let unrelated = source_seq(0x11_22_33, 40);

    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    seed_with_tier2(&db, "/v/unrelated.mp4", &unrelated);

    let out = rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;

    assert_eq!(
        out.groups_created, 1,
        "one partial-clip group must form even when there are no near-dup groups"
    );
    let mut want = vec![{
        let mut v = vec![s_id.0, c_id.0];
        v.sort_unstable();
        v
    }];
    want.sort();
    assert_eq!(
        members_snapshot(&db),
        want,
        "the clip and its source are grouped as POSSIBLE independently of near-dup groups"
    );

    let clip2 = clip_of(&source, 28, 6, 2);
    let c2_id = seed_with_tier2(&db, "/v/clip2.mp4", &clip2);
    let changed: BTreeSet<FileId> = [c2_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 1, &changed)?;

    let snap = members_snapshot(&db);
    assert_eq!(
        snap.len(),
        2,
        "two POSSIBLE groups total: clip1↔source and clip2↔source; got {snap:?}"
    );

    let has_c1_source = snap
        .iter()
        .any(|members| members.contains(&c_id.0) && members.contains(&s_id.0));
    assert!(
        has_c1_source,
        "clip1 must remain grouped with the source: {snap:?}"
    );
    let has_c2_source = snap
        .iter()
        .any(|members| members.contains(&c2_id.0) && members.contains(&s_id.0));
    assert!(
        has_c2_source,
        "clip2 must be grouped with the source: {snap:?}"
    );
    Ok(())
}

#[test]
fn partial_source_index_reads_partial_temporal_not_tier2() -> Result<()> {
    let source = source_seq(0x2121, 40);
    let clip = clip_of(&source, 12, 6, 3);
    let noise_long = source_seq(0xAAAA, 40);
    let noise_short = source_seq(0xBBBB, 6);

    let mut partial_db = open_in_memory()?;
    let s_id = seed_with_tier2(&partial_db, "/v/source.mp4", &noise_long);
    let c_id = seed_with_tier2(&partial_db, "/v/clip.mp4", &noise_short);
    set_partial(&partial_db, s_id, &source);
    set_partial(&partial_db, c_id, &clip);
    let mut p_index =
        PartialClipIndex::new_with_source(AnchorParams::default(), BlobSource::Partial);
    assert_eq!(p_index.blob_source(), BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut p_index, &mut partial_db, T0, &BTreeSet::new())?;
    assert_eq!(
        members_snapshot(&partial_db),
        vec![sorted_pair(s_id, c_id)],
        "partial-source index groups the clip⊂source pair read from partial_temporal",
    );

    let mut tier2_db = open_in_memory()?;
    let s2 = seed_with_tier2(&tier2_db, "/v/source.mp4", &noise_long);
    let c2 = seed_with_tier2(&tier2_db, "/v/clip.mp4", &noise_short);
    set_partial(&tier2_db, s2, &source);
    set_partial(&tier2_db, c2, &clip);
    let mut t_index = PartialClipIndex::new(AnchorParams::default());
    assert_eq!(
        t_index.blob_source(),
        BlobSource::Tier2,
        "new() defaults to tier2"
    );
    rebuild_partial_clip_groups_durable(&mut t_index, &mut tier2_db, T0, &BTreeSet::new())?;
    assert!(
        members_snapshot(&tier2_db).is_empty(),
        "tier2-source index sees only unrelated noise → no partial-clip group",
    );
    Ok(())
}

#[test]
fn provenance_mismatch_forces_cold_rebuild_not_stale_delta() -> Result<()> {
    let mut db = open_in_memory()?;
    let source = source_seq(0x3131, 40);
    let clip = clip_of(&source, 10, 6, 3);
    let s_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let c_id = seed_with_tier2(&db, "/v/clip.mp4", &clip);
    set_partial(&db, s_id, &source);
    set_partial(&db, c_id, &clip);

    let mut t2 = PartialClipIndex::new(AnchorParams::default());
    rebuild_partial_clip_groups_durable(&mut t2, &mut db, T0, &BTreeSet::new())?;

    let mut t2_restart = PartialClipIndex::new(AnchorParams::default());
    rebuild_partial_clip_groups_durable(&mut t2_restart, &mut db, T0 + 1, &BTreeSet::new())?;
    assert_eq!(
        t2_restart.last_rediscovered(),
        0,
        "matching provenance restart takes the delta path (no replan)",
    );

    let mut partial = PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut partial, &mut db, T0 + 2, &BTreeSet::new())?;
    assert_eq!(
        partial.last_rediscovered(),
        2,
        "provenance mismatch forces a cold rebuild over the whole partial corpus",
    );
    Ok(())
}

#[test]
fn toggle_scene_count_reflects_active_corpus_cardinality() -> Result<()> {
    let mut db = open_in_memory()?;
    let big = source_seq(0x6161, 40);
    let small = clip_of(&big, 0, 6, 0);
    let f_id = seed_with_tier2(&db, "/v/f.mp4", &big);
    set_partial(&db, f_id, &small);

    let mut t2 = PartialClipIndex::new(AnchorParams::default());
    rebuild_partial_clip_groups_durable(&mut t2, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(
        PartialMihRepo::new(db.conn()).scene_count(f_id)?,
        Some(40),
        "tier2 corpus scene cardinality",
    );

    let mut partial = PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut partial, &mut db, T0 + 1, &BTreeSet::new())?;
    assert_eq!(
        PartialMihRepo::new(db.conn()).scene_count(f_id)?,
        Some(6),
        "partial corpus cardinality after toggle (not the stale tier2 count of 40)",
    );
    Ok(())
}

#[test]
fn durable_bootstrap_outcome_tallies_near_miss_drops() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());

    let source = source_seq(0x5555, 40);
    let mut clip = clip_of(&source, 10, 10, 0);
    for s in clip.scenes.iter_mut().skip(4) {
        s.phash = flip_low_bits(s.phash, 40);
    }
    seed_with_tier2(&db, "/v/source.mp4", &source);
    seed_with_tier2(&db, "/v/nearmiss.mp4", &clip);

    let out = rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(
        out.groups_created, 0,
        "the 40 % near-miss must NOT be force-matched (recall ceiling unchanged)",
    );
    assert!(
        out.dropped_below_coverage > 0,
        "durable bootstrap must tally the below-coverage near-miss (was silently 0)",
    );
    Ok(())
}

#[test]
fn durable_burst_outcome_tallies_near_miss_drops() -> Result<()> {
    let mut db = open_in_memory()?;
    let mut index = PartialClipIndex::new(AnchorParams::default());
    let source = source_seq(0x5555, 40);
    let src_id = seed_with_tier2(&db, "/v/source.mp4", &source);
    let out = rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())?;
    assert_eq!(out.groups_created, 0, "a lone source has nothing to match");

    let mut garbled = clip_of(&source, 10, 10, 0);
    for s in garbled.scenes.iter_mut().skip(4) {
        s.phash = flip_low_bits(s.phash, 40);
    }
    let garbled_id = seed_with_tier2(&db, "/v/nearmiss.mp4", &garbled);
    let changed: BTreeSet<FileId> = [garbled_id].into_iter().collect();
    let out = rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 1, &changed)?;
    assert_eq!(out.groups_created, 0, "near-miss must not group");
    assert!(
        out.dropped_below_coverage > 0,
        "burst outcome must tally the below-coverage near-miss (was silently 0)",
    );

    let mut state = 0x00C0_FFEEu64;
    let lone = Tier2Fingerprint {
        scenes: vec![
            SceneHash {
                timestamp_ms: 0,
                phash: source.scenes[5].phash,
            },
            SceneHash {
                timestamp_ms: 1000,
                phash: source.scenes[20].phash,
            },
            SceneHash {
                timestamp_ms: 2000,
                phash: splitmix64(&mut state) | 1,
            },
        ],
    };
    let lone_id = seed_with_tier2(&db, "/v/lonevotes.mp4", &lone);
    let changed: BTreeSet<FileId> = [lone_id].into_iter().collect();
    let out = rebuild_partial_clip_groups_durable(&mut index, &mut db, T0 + 2, &changed)?;
    assert_eq!(out.groups_created, 0, "single-vote offsets must not group");
    assert!(
        out.dropped_single_vote > 0,
        "burst outcome must tally single-vote Hough drops (was silently 0)",
    );
    let _ = src_id;
    Ok(())
}
