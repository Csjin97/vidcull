use std::collections::BTreeSet;

use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    FilesRepo, Fingerprint, FingerprintsRepo, NewFile, PartialEdgeSpan, SimilarityEdge,
    SimilarityEdgesRepo, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
};
use vidcull_matcher::partial::{ClipAlignment, partial_clip_params, plan_partial_clips};

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

fn persisted_edge(db: &Database, clip: FileId, source: FileId) -> SimilarityEdge {
    let want = canonical_pair(clip, source);
    SimilarityEdgesRepo::new(db.conn())
        .list_by_trust(TrustLevel::Possible)
        .expect("list possible edges")
        .into_iter()
        .find(|e| canonical_pair(e.file_a, e.file_b) == want)
        .unwrap_or_else(|| panic!("no persisted POSSIBLE edge for clip⊂source pair {want:?}"))
}

fn expected_alignment(db: &Database, clip: FileId, source: FileId) -> ClipAlignment {
    let corpus: Vec<(FileId, Tier2Fingerprint)> = FingerprintsRepo::new(db.conn())
        .list_active_partial()
        .expect("list_active_partial")
        .into_iter()
        .map(|(id, blob)| (id, format::decode_tier2(&blob).expect("decode")))
        .collect();
    let plan = plan_partial_clips(corpus, partial_clip_params());
    plan.matches
        .iter()
        .find(|m| m.clip == clip && m.alignment.source == source)
        .unwrap_or_else(|| panic!("full-corpus plan found no clip⊂source match"))
        .alignment
}

fn span_of(a: &ClipAlignment) -> PartialEdgeSpan {
    PartialEdgeSpan {
        clip_start_ms: a.clip_start_ms,
        clip_end_ms: a.clip_end_ms,
        source_start_ms: a.start_ms,
        source_end_ms: a.end_ms,
        matched_scenes: a.matched_scenes,
        clip_scenes: a.clip_scenes,
    }
}

#[test]
fn persisted_offsets_survive_restart_and_incremental_burst() {
    let mut db = open_in_memory().expect("open in-memory db");

    let source = source_seq(0xABCD_1234_5678_9F01, 40);
    let clip = clip_of(&source, 10, 6, 4);
    let distractor_a = source_seq(0x1111_2222_3333_4444, 40);
    let distractor_b = source_seq(0x5555_6666_7777_8888, 40);

    let source_id = seed_with_partial(&db, "/rt/source.mp4", &source);
    let clip_id = seed_with_partial(&db, "/rt/clip.mp4", &clip);
    let _da = seed_with_partial(&db, "/rt/distractor_a.mp4", &distractor_a);
    let _db_id = seed_with_partial(&db, "/rt/distractor_b.mp4", &distractor_b);
    assert!(
        source_id.0 < clip_id.0,
        "source must sort before clip for this test"
    );

    let params = partial_clip_params();
    let expected = span_of(&expected_alignment(&db, clip_id, source_id));
    assert_ne!(
        expected.clip_start_ms, expected.source_start_ms,
        "fixture must be asymmetric (clip on t=0, source offset) so transpose is detectable"
    );
    assert!(
        expected.clip_scenes > 0 && expected.matched_scenes > 0,
        "real alignment"
    );

    let mut index = PartialClipIndex::new_with_source(params, BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut index, &mut db, T0, &BTreeSet::new())
        .expect("cold durable grouping");
    let after_cold = persisted_edge(&db, clip_id, source_id).partial_span;
    assert_eq!(
        after_cold,
        Some(expected),
        "grouping must persist the real alignment span (no zeros)"
    );

    let mut index2 = PartialClipIndex::new_with_source(params, BlobSource::Partial);
    rebuild_partial_clip_groups_durable(&mut index2, &mut db, T0 + 1, &BTreeSet::new())
        .expect("restart durable reload");
    let reloaded = index2
        .plan()
        .matches
        .into_iter()
        .find(|m| m.clip == clip_id && m.alignment.source == source_id)
        .expect("reloaded match present");
    assert_eq!(
        span_of(&reloaded.alignment),
        expected,
        "restart reload (reconstruct_prev_match) must read the span back, oriented correctly"
    );
    let after_restart = persisted_edge(&db, clip_id, source_id).partial_span;
    assert_eq!(
        after_restart,
        Some(expected),
        "re-persist after restart must preserve the span (the pre-blank regression)"
    );

    let extra = source_seq(0x9999_AAAA_BBBB_CCCC, 40);
    let extra_id = seed_with_partial(&db, "/rt/extra.mp4", &extra);
    let changed: BTreeSet<FileId> = [extra_id].into_iter().collect();
    rebuild_partial_clip_groups_durable(&mut index2, &mut db, T0 + 2, &changed)
        .expect("incremental burst");
    let after_burst = persisted_edge(&db, clip_id, source_id).partial_span;
    assert_eq!(
        after_burst,
        Some(expected),
        "carry-forward across an incremental burst must preserve the span values"
    );
}
