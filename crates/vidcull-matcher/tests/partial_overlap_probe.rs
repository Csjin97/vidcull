use std::collections::BTreeSet;

use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, SimilarityEdge,
    SimilarityEdgesRepo, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION, decode_tier2};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
};
use vidcull_matcher::partial::{partial_clip_params, plan_partial_clips};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
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

fn seed_with_partial(db: &Database, path: &str, fp: &Tier2Fingerprint) -> FileId {
    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: fp.scenes.first().map_or(0, |s| s.phash),
    };
    let tier2_blob = format::encode_tier2(fp).expect("encode tier2");
    let tier1_blob = format::encode_tier1(&t1).expect("encode tier1");
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
    let file_id = FilesRepo::new(db.conn()).insert(&new_file).expect("insert");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id,
            tier1_global: tier1_blob,
            tier2_temporal: Some(tier2_blob.clone()),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
    FingerprintsRepo::new(db.conn())
        .set_partial(file_id, &tier2_blob)
        .expect("set_partial");
    file_id
}

fn canonical_pair(a: FileId, b: FileId) -> (FileId, FileId) {
    if a.0 <= b.0 { (a, b) } else { (b, a) }
}

fn persisted_possible_pairs(db: &Database) -> BTreeSet<(FileId, FileId)> {
    SimilarityEdgesRepo::new(db.conn())
        .list_by_trust(TrustLevel::Possible)
        .expect("list possible edges")
        .into_iter()
        .map(|e: SimilarityEdge| canonical_pair(e.file_a, e.file_b))
        .collect()
}

fn load_partial_corpus(db: &Database) -> Vec<(FileId, Tier2Fingerprint)> {
    FingerprintsRepo::new(db.conn())
        .list_active_partial()
        .expect("list_active_partial")
        .into_iter()
        .map(|(id, blob)| (id, decode_tier2(&blob).expect("decode partial blob")))
        .collect()
}

fn plan_pairs_for_members(
    matches: &[vidcull_matcher::partial::ClipMatch],
    member_set: &BTreeSet<FileId>,
) -> BTreeSet<(FileId, FileId)> {
    matches
        .iter()
        .filter(|m| member_set.contains(&m.clip) && member_set.contains(&m.alignment.source))
        .map(|m| canonical_pair(m.clip, m.alignment.source))
        .collect()
}

fn find_possible_group_members(db: &Database, expected: &BTreeSet<FileId>) -> BTreeSet<FileId> {
    let groups = DuplicateGroupsRepo::new(db.conn());
    for gid in 1_i64..=1024 {
        let Some(group) = groups.get(gid).expect("get group") else {
            break;
        };
        if group.trust_level != TrustLevel::Possible {
            continue;
        }
        let members: BTreeSet<FileId> = groups
            .list_members(gid)
            .expect("list members")
            .into_iter()
            .collect();
        if members == *expected {
            return members;
        }
    }
    panic!(
        "no POSSIBLE group with members {:?}; persisted possible groups: {:?}",
        expected,
        persisted_possible_pairs(db),
    );
}

#[test]
fn partial_overlap_probe_member_filter_vs_full_corpus() {
    let mut db = open_in_memory().expect("open in-memory db");

    let source = source_seq(0xABCD_1234_5678_9F01, 40);
    let clip = clip_of(&source, 10, 6, 4);
    let distractor_a = source_seq(0x1111_2222_3333_4444, 40);
    let distractor_b = source_seq(0x5555_6666_7777_8888, 40);

    let source_id = seed_with_partial(&db, "/probe/source.mp4", &source);
    let clip_id = seed_with_partial(&db, "/probe/clip.mp4", &clip);
    let _dist_a_id = seed_with_partial(&db, "/probe/distractor_a.mp4", &distractor_a);
    let _dist_b_id = seed_with_partial(&db, "/probe/distractor_b.mp4", &distractor_b);

    let params = partial_clip_params();
    let mut index = PartialClipIndex::new_with_source(params, BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())
        .expect("durable grouping");

    let expected_members: BTreeSet<FileId> = [clip_id, source_id].into_iter().collect();
    let member_set = find_possible_group_members(&db, &expected_members);

    let persisted_pairs = persisted_possible_pairs(&db);
    let expected_pair = canonical_pair(clip_id, source_id);
    assert!(
        persisted_pairs.contains(&expected_pair),
        "grouping must have persisted the clip⊂source edge; persisted: {persisted_pairs:?}",
    );
    let persisted_for_group: BTreeSet<(FileId, FileId)> = {
        let mut s = BTreeSet::new();
        s.insert(expected_pair);
        s
    };

    let all_corpus = load_partial_corpus(&db);
    assert_eq!(
        all_corpus.len(),
        4,
        "all 4 files must have partial_temporal set"
    );

    let filtered_corpus: Vec<(FileId, Tier2Fingerprint)> = all_corpus
        .iter()
        .filter(|(id, _)| member_set.contains(id))
        .cloned()
        .collect();
    assert_eq!(
        filtered_corpus.len(),
        2,
        "member-filtered corpus must contain exactly the 2 group members"
    );
    let plan_i = plan_partial_clips(filtered_corpus, params);
    let pairs_i = plan_pairs_for_members(&plan_i.matches, &member_set);
    println!(
        "[probe] (i) member-filtered (corpus=2): matches={} skipped_short={} \
         dropped_single_vote={} pairs={:?}",
        plan_i.matches.len(),
        plan_i.skipped_short,
        plan_i.dropped_single_vote,
        pairs_i,
    );

    let plan_ii = plan_partial_clips(all_corpus, params);
    let pairs_ii = plan_pairs_for_members(&plan_ii.matches, &member_set);
    println!(
        "[probe] (ii) full-corpus projected (corpus=4): matches={} skipped_short={} \
         dropped_single_vote={} pairs_for_group={:?}",
        plan_ii.matches.len(),
        plan_ii.skipped_short,
        plan_ii.dropped_single_vote,
        pairs_ii,
    );

    assert_eq!(
        pairs_ii, persisted_for_group,
        "(ii) full-corpus plan projected to member_set must reproduce the grouping's \
         persisted match set exactly (same pairs). 1-B is only viable if this holds."
    );

    assert_eq!(
        pairs_i,
        persisted_for_group,
        "DIAGNOSTIC: member-filtered (i) does NOT reproduce grouping's match set \
         (corpus=2, matches={}, skipped_short={}, dropped_single_vote={}). \
         Full-corpus (ii) reproduces it ({:?}). \
         Root cause confirmed: member-filtering before plan_partial_clips causes empty result. \
         Fix: use full-corpus plan + project to member_set (Option 1-B).",
        plan_i.matches.len(),
        plan_i.skipped_short,
        plan_i.dropped_single_vote,
        pairs_ii,
    );
}
