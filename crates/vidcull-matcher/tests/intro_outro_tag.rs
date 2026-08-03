use vidcull_core::types::{Codec, FileId, NormalizedPath, VideoDuration};
use vidcull_db::repo::{FilesRepo, NewFile, SimilarityEdgesRepo, TrustLevel};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::{
    ClipAlignment, is_intro_outro, partial_clip_params, plan_partial_clips,
    rebuild_partial_clip_groups, rebuild_partial_clip_groups_incremental,
};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;
const GRID_MS: u64 = 2_500;

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

fn scene(ts: u64, phash: u64) -> SceneHash {
    SceneHash {
        timestamp_ms: ts,
        phash,
    }
}

fn unrelated_seq(seed: u64, n: usize) -> Vec<SceneHash> {
    let mut state = seed;
    (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state) | 1))
        .collect()
}

fn dur_ms(n: usize) -> u64 {
    (n as u64) * GRID_MS
}

fn seed_file_with_duration(db: &Database, path: &str, dur_ms: u64) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: MTIME,
        codec: Some(Codec::H264),
        duration: Some(VideoDuration::from_millis(dur_ms)),
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn seed_file_no_duration(db: &Database, path: &str) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: MTIME,
        codec: Some(Codec::H264),
        duration: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn canonical_pair(a: FileId, b: FileId) -> (FileId, FileId) {
    if a.0 <= b.0 { (a, b) } else { (b, a) }
}

fn persisted_edge(db: &Database, clip: FileId, source: FileId) -> vidcull_db::repo::SimilarityEdge {
    let want = canonical_pair(clip, source);
    SimilarityEdgesRepo::new(db.conn())
        .list_by_trust(TrustLevel::Possible)
        .expect("list possible edges")
        .into_iter()
        .find(|e| canonical_pair(e.file_a, e.file_b) == want)
        .unwrap_or_else(|| panic!("no persisted POSSIBLE edge for clip⊂source pair {want:?}"))
}

fn shared_intro_pair(
    seed_shared: u64,
    seed_a: u64,
    seed_b: u64,
    k: usize,
    a_len: usize,
    b_len: usize,
) -> (Tier2Fingerprint, Tier2Fingerprint) {
    let mut state_shared = seed_shared;
    let shared: Vec<u64> = (0..k).map(|_| splitmix64(&mut state_shared) | 1).collect();
    let mut state_a = seed_a;
    let mut a_scenes: Vec<SceneHash> = (0..k)
        .map(|i| scene(i as u64 * GRID_MS, shared[i]))
        .collect();
    a_scenes.extend((k..a_len).map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_a) | 1)));
    let mut state_b = seed_b;
    let mut b_scenes: Vec<SceneHash> = (0..k)
        .map(|i| scene(i as u64 * GRID_MS, shared[i]))
        .collect();
    b_scenes.extend((k..b_len).map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_b) | 1)));
    (
        Tier2Fingerprint { scenes: a_scenes },
        Tier2Fingerprint { scenes: b_scenes },
    )
}

fn shared_outro_pair(
    seed_shared: u64,
    seed_a: u64,
    seed_b: u64,
    k: usize,
    a_len: usize,
    b_len: usize,
) -> (Tier2Fingerprint, Tier2Fingerprint) {
    let mut state_shared = seed_shared;
    let shared: Vec<u64> = (0..k).map(|_| splitmix64(&mut state_shared) | 1).collect();
    let mut state_a = seed_a;
    let mut a_scenes: Vec<SceneHash> = (0..(a_len - k))
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_a) | 1))
        .collect();
    a_scenes.extend((0..k).map(|i| scene((a_len - k + i) as u64 * GRID_MS, shared[i])));
    let mut state_b = seed_b;
    let mut b_scenes: Vec<SceneHash> = (0..(b_len - k))
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_b) | 1))
        .collect();
    b_scenes.extend((0..k).map(|i| scene((b_len - k + i) as u64 * GRID_MS, shared[i])));
    (
        Tier2Fingerprint { scenes: a_scenes },
        Tier2Fingerprint { scenes: b_scenes },
    )
}

fn group7_reframe(
    seed_shared: u64,
    seed_clip_body: u64,
    seed_source_body: u64,
    clip_len: usize,
    source_len: usize,
    aligned_at: usize,
    source_offset: usize,
) -> (Tier2Fingerprint, Tier2Fingerprint) {
    let mut state_shared = seed_shared;
    let shared: [u64; 3] = [
        splitmix64(&mut state_shared) | 1,
        splitmix64(&mut state_shared) | 1,
        splitmix64(&mut state_shared) | 1,
    ];
    let mut state_clip = seed_clip_body;
    let mut clip_scenes: Vec<SceneHash> = (0..clip_len)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_clip) | 1))
        .collect();
    for (i, &h) in shared.iter().enumerate() {
        clip_scenes[aligned_at + i] = scene((aligned_at + i) as u64 * GRID_MS, h);
    }
    let mut state_src = seed_source_body;
    let mut src_scenes: Vec<SceneHash> = (0..source_len)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_src) | 1))
        .collect();
    for (i, &h) in shared.iter().enumerate() {
        src_scenes[source_offset + i] = scene((source_offset + i) as u64 * GRID_MS, h);
    }
    (
        Tier2Fingerprint {
            scenes: clip_scenes,
        },
        Tier2Fingerprint { scenes: src_scenes },
    )
}

fn clip_embedded_at(
    source: &Tier2Fingerprint,
    at: usize,
    len: usize,
    perturb: u32,
) -> Tier2Fingerprint {
    let scenes = source.scenes[at..at + len]
        .iter()
        .enumerate()
        .map(|(i, s)| scene(i as u64 * GRID_MS, flip_low_bits(s.phash, perturb)))
        .collect();
    Tier2Fingerprint { scenes }
}

fn verified_alignment(
    corpus: Vec<(FileId, Tier2Fingerprint)>,
    clip: FileId,
    source: FileId,
) -> ClipAlignment {
    let plan = plan_partial_clips(corpus, partial_clip_params());
    plan.matches
        .into_iter()
        .find(|m| m.clip == clip && m.alignment.source == source)
        .unwrap_or_else(|| panic!("no verified alignment for clip⊂source pair"))
        .alignment
}

#[test]
fn shared_intro_pair_is_tagged() {
    let (fp_source, fp_clip) = shared_intro_pair(0xA000_0001, 0xA100_0001, 0xA200_0001, 3, 400, 24);
    let clip = FileId(2);
    let source = FileId(1);
    let corpus = vec![(source, fp_source), (clip, fp_clip)];
    let alignment = verified_alignment(corpus, clip, source);
    assert!(
        alignment.clip_scenes > 0,
        "must be a real, non-legacy match"
    );
    assert!(
        is_intro_outro(&alignment, Some(dur_ms(24)), Some(dur_ms(400))),
        "a shared stock intro (short span, head/head) must be tagged"
    );
}

#[test]
fn shared_outro_pair_is_tagged() {
    let (fp_source, fp_clip) = shared_outro_pair(0xB000_0001, 0xB100_0001, 0xB200_0001, 3, 400, 24);
    let clip = FileId(2);
    let source = FileId(1);
    let corpus = vec![(source, fp_source), (clip, fp_clip)];
    let alignment = verified_alignment(corpus, clip, source);
    assert!(
        is_intro_outro(&alignment, Some(dur_ms(24)), Some(dur_ms(400))),
        "a shared stock outro (short span, tail/tail) must be tagged"
    );
}

#[test]
fn group7_reframe_mid_source_is_not_tagged() {
    let (fp_clip, fp_source) =
        group7_reframe(0xF000_0001, 0xF100_0001, 0xF200_0001, 46, 300, 2, 150);
    let clip = FileId(2);
    let source = FileId(1);
    let corpus = vec![(source, fp_source), (clip, fp_clip)];
    let alignment = verified_alignment(corpus, clip, source);
    assert!(
        !is_intro_outro(&alignment, Some(dur_ms(46)), Some(dur_ms(300))),
        "group #7 low-coverage reframe (source-mid) must NOT be tagged — recall-critical"
    );
}

#[test]
fn group7_reframe_dispersed_source_tail_is_not_tagged() {
    let clip_len = 46;
    let source_len = 300;
    let mut state = 0xF300_0001u64;
    let shared: [u64; 3] = [
        splitmix64(&mut state) | 1,
        splitmix64(&mut state) | 1,
        splitmix64(&mut state) | 1,
    ];
    let mut state_clip = 0xF400_0001u64;
    let mut clip_scenes: Vec<SceneHash> = (0..clip_len)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_clip) | 1))
        .collect();
    let dispersed_at = [2usize, 23, 43];
    let offset_d = 255usize;
    for (i, &at) in dispersed_at.iter().enumerate() {
        clip_scenes[at] = scene(at as u64 * GRID_MS, shared[i]);
    }
    let mut state_src = 0xF500_0001u64;
    let mut src_scenes: Vec<SceneHash> = (0..source_len)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut state_src) | 1))
        .collect();
    for (i, &at) in dispersed_at.iter().enumerate() {
        let src_at = at + offset_d;
        src_scenes[src_at] = scene(src_at as u64 * GRID_MS, shared[i]);
    }
    let clip = FileId(2);
    let source = FileId(1);
    let corpus = vec![
        (source, Tier2Fingerprint { scenes: src_scenes }),
        (
            clip,
            Tier2Fingerprint {
                scenes: clip_scenes,
            },
        ),
    ];
    let alignment = verified_alignment(corpus, clip, source);
    assert!(
        !is_intro_outro(&alignment, Some(dur_ms(clip_len)), Some(dur_ms(source_len))),
        "dispersed low-coverage reframe must NOT be tagged"
    );
}

#[test]
fn short_clip_matched_whole_at_source_head_is_not_tagged() {
    let source = Tier2Fingerprint {
        scenes: unrelated_seq(0xE000_0001, 300),
    };
    let clip_len = 24;
    let clip_fp = clip_embedded_at(&source, 4, clip_len, 3);
    let clip = FileId(2);
    let src_id = FileId(1);
    let corpus = vec![(src_id, source), (clip, clip_fp)];
    let alignment = verified_alignment(corpus, clip, src_id);
    assert!(
        !is_intro_outro(&alignment, Some(dur_ms(clip_len)), Some(dur_ms(300))),
        "a short clip matched almost entirely (span ~100% of ITS OWN duration) \
         must not be tagged even though its position is head/head"
    );
}

#[test]
fn both_heads_adversarial_case_is_tagged_accepted_tradeoff() {
    let (fp_clip, fp_source) = group7_reframe(0xF600_0001, 0xF700_0001, 0xF800_0001, 46, 300, 1, 3);
    let clip = FileId(2);
    let source = FileId(1);
    let corpus = vec![(source, fp_source), (clip, fp_clip)];
    let alignment = verified_alignment(corpus, clip, source);
    assert!(
        is_intro_outro(&alignment, Some(dur_ms(46)), Some(dur_ms(300))),
        "both-heads adversarial case is tagged — accepted trade-off, see ADR-2"
    );
}

#[test]
fn legacy_zero_clip_scenes_is_never_tagged() {
    let alignment = ClipAlignment {
        source: FileId(1),
        source_offset: 0,
        matched_scenes: 0,
        clip_scenes: 0,
        coverage_x1000: 700,
        start_ms: 0,
        end_ms: 0,
        clip_start_ms: 0,
        clip_end_ms: 0,
    };
    assert!(!is_intro_outro(&alignment, Some(60_000), Some(600_000)));
}

#[test]
fn unknown_duration_is_never_tagged() {
    let (fp_source, fp_clip) = shared_intro_pair(0xA000_0001, 0xA100_0001, 0xA200_0001, 3, 400, 24);
    let clip = FileId(2);
    let source = FileId(1);
    let corpus = vec![(source, fp_source), (clip, fp_clip)];
    let alignment = verified_alignment(corpus, clip, source);
    assert!(
        !is_intro_outro(&alignment, None, Some(dur_ms(400))),
        "unknown clip duration must yield untagged, not a guess"
    );
    assert!(
        !is_intro_outro(&alignment, Some(dur_ms(24)), None),
        "unknown source duration must yield untagged, not a guess"
    );
}

fn seed_shared_intro_corpus(db: &Database) -> (FileId, FileId) {
    let (fp_source, fp_clip) = shared_intro_pair(0xA000_0001, 0xA100_0001, 0xA200_0001, 3, 400, 24);
    let source_id = seed_file_with_duration(db, "/tag/source.mp4", dur_ms(400));
    let clip_id = seed_file_with_duration(db, "/tag/clip.mp4", dur_ms(24));
    upsert_tier2(db, source_id, &fp_source);
    upsert_tier2(db, clip_id, &fp_clip);
    (clip_id, source_id)
}

fn upsert_tier2(db: &Database, file_id: FileId, fp: &Tier2Fingerprint) {
    use vidcull_db::repo::{Fingerprint, FingerprintsRepo};
    use vidcull_fingerprint::format::{self, FORMAT_VERSION};
    use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: fp.scenes.first().map_or(0, |s| s.phash),
    };
    let tier2_blob = format::encode_tier2(fp).expect("encode tier2");
    let tier1_blob = format::encode_tier1(&t1).expect("encode tier1");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id,
            tier1_global: tier1_blob,
            tier2_temporal: Some(tier2_blob),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
}

#[test]
fn full_rebuild_persists_tag_and_counts_it() {
    let mut db = open_in_memory().expect("open in-memory db");
    let (clip_id, source_id) = seed_shared_intro_corpus(&db);

    let outcome =
        rebuild_partial_clip_groups(&mut db, partial_clip_params(), T0).expect("full rebuild");
    assert_eq!(
        outcome.groups_created, 1,
        "the match is created, not dropped"
    );
    assert_eq!(outcome.tagged_intro_outro, 1, "the one match is tagged");

    let edge = persisted_edge(&db, clip_id, source_id);
    assert!(edge.intro_outro, "persisted edge carries the tag");
    assert!(
        edge.partial_span.is_some_and(|s| s.clip_scenes > 0),
        "span still persisted (additive)"
    );
}

#[test]
fn incremental_rebuild_persists_tag_on_first_discovery() {
    let mut db = open_in_memory().expect("open in-memory db");
    let (clip_id, source_id) = seed_shared_intro_corpus(&db);
    let changed: std::collections::BTreeSet<FileId> = [clip_id, source_id].into_iter().collect();

    let outcome =
        rebuild_partial_clip_groups_incremental(&mut db, partial_clip_params(), T0, &changed)
            .expect("incremental rebuild");
    assert_eq!(outcome.groups_created, 1);
    assert_eq!(outcome.tagged_intro_outro, 1);
    assert!(persisted_edge(&db, clip_id, source_id).intro_outro);
}

#[test]
fn incremental_carry_forward_preserves_the_tag() {
    let mut db = open_in_memory().expect("open in-memory db");
    let (clip_id, source_id) = seed_shared_intro_corpus(&db);
    let all_changed: std::collections::BTreeSet<FileId> =
        [clip_id, source_id].into_iter().collect();
    rebuild_partial_clip_groups_incremental(&mut db, partial_clip_params(), T0, &all_changed)
        .expect("cold incremental rebuild");
    assert!(
        persisted_edge(&db, clip_id, source_id).intro_outro,
        "tagged on first discovery"
    );

    let extra_id = seed_file_with_duration(&db, "/tag/extra.mp4", dur_ms(50));
    upsert_tier2(
        &db,
        extra_id,
        &Tier2Fingerprint {
            scenes: unrelated_seq(0xCCCC_0001, 50),
        },
    );
    let delta: std::collections::BTreeSet<FileId> = [extra_id].into_iter().collect();
    let outcome =
        rebuild_partial_clip_groups_incremental(&mut db, partial_clip_params(), T0 + 1, &delta)
            .expect("delta incremental rebuild");
    assert_eq!(
        outcome.groups_created, 1,
        "the carried match is still written"
    );
    assert!(
        persisted_edge(&db, clip_id, source_id).intro_outro,
        "carry-forward must preserve the tag, not reset it"
    );
}

#[test]
fn legacy_duration_unknown_file_is_not_tagged_through_full_rebuild() {
    let mut db = open_in_memory().expect("open in-memory db");
    let (fp_source, fp_clip) = shared_intro_pair(0xA000_0001, 0xA100_0001, 0xA200_0001, 3, 400, 24);
    let source_id = seed_file_no_duration(&db, "/tag/nodur_source.mp4");
    let clip_id = seed_file_no_duration(&db, "/tag/nodur_clip.mp4");
    upsert_tier2(&db, source_id, &fp_source);
    upsert_tier2(&db, clip_id, &fp_clip);

    let outcome =
        rebuild_partial_clip_groups(&mut db, partial_clip_params(), T0).expect("full rebuild");
    assert_eq!(
        outcome.groups_created, 1,
        "match still created — additive only"
    );
    assert_eq!(
        outcome.tagged_intro_outro, 0,
        "duration-unknown pair is not tagged"
    );
    assert!(!persisted_edge(&db, clip_id, source_id).intro_outro);
}
