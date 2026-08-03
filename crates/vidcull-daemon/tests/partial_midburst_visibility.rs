use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_daemon::indexing::PARTIAL_PRIORITY;
use vidcull_daemon::{ChangeKind, ChangeTask, Daemon, DaemonConfig, IndexingHandler, TaskHandler};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, NewTask,
    RegroupQueueRepo, TaskQueueRepo, TrustLevel,
};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_parser::fallback::FfmpegBinaries;

const T0: i64 = 1_700_000_000;

fn now_t0() -> i64 {
    T0
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

fn clean_clip(source: &Tier2Fingerprint, start: usize, len: usize) -> Tier2Fingerprint {
    let scenes = source.scenes[start..start + len]
        .iter()
        .enumerate()
        .map(|(i, s)| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: s.phash,
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn seed_tier1(db: &Database, path: &str, global_phash: u64) -> FileId {
    let id = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(path),
            ..Default::default()
        })
        .expect("insert file row");
    let t1 = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash,
    };
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
    RegroupQueueRepo::new(db.conn())
        .mark(id, T0)
        .expect("mark regroup");
    id
}

fn set_partial(db: &Database, id: FileId, tier2: &Tier2Fingerprint) {
    let written = FingerprintsRepo::new(db.conn())
        .set_partial(id, &format::encode_tier2(tier2).expect("encode partial"))
        .expect("set partial");
    assert_eq!(written, 1, "partial blob must land on the existing row");
}

fn possible_groups(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut out: Vec<Vec<i64>> = repo
        .list_all()
        .expect("list groups")
        .into_iter()
        .filter(|g| g.trust_level == TrustLevel::Possible)
        .map(|g| {
            let mut members: Vec<i64> = repo
                .list_members(g.id)
                .expect("members")
                .into_iter()
                .map(|f| f.0)
                .collect();
            members.sort_unstable();
            members
        })
        .collect();
    out.sort();
    out
}

fn sorted_pair(a: FileId, b: FileId) -> Vec<i64> {
    let mut v = vec![a.0, b.0];
    v.sort_unstable();
    v
}

fn enqueue_foreground_burst(db: &mut Database, n: usize) {
    let changes: Vec<ChangeTask> = (0..n)
        .map(|i| ChangeTask {
            path: NormalizedPath::new(format!("/v/never_existed_{i}.mp4")),
            change: ChangeKind::Remove,
            size_bytes: 0,
        })
        .collect();
    vidcull_daemon::enqueue_changes(db, &changes, "scan", 0, T0).expect("enqueue foreground burst");
}

#[test]
fn possible_group_should_appear_before_foreground_queue_drains() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("index.db");

    let src = source_seq(0x5678, 40);
    let clip = clean_clip(&src, 10, 8);
    let (source_id, clip_id) = {
        let db = vidcull_db::open_file(&db_path).expect("open seed db");
        let source_id = seed_tier1(&db, "/v/source.mp4", 0x0102_0304_0506_0708);
        let clip_id = seed_tier1(&db, "/v/clip.mp4", 0xF1F2_F3F4_F5F6_F7F8);
        set_partial(&db, source_id, &src);
        set_partial(&db, clip_id, &clip);
        (source_id, clip_id)
    };

    {
        let mut db = vidcull_db::open_file(&db_path).expect("open enqueue db");
        enqueue_foreground_burst(&mut db, 6);
    }

    let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now_t0).with_partial_clips(true);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());

    for _ in 0..5 {
        daemon
            .step(&worker_db, &mut handler, T0)
            .expect("step")
            .expect("a foreground task was processed");
    }

    let pending_foreground = TaskQueueRepo::new(worker_db.conn())
        .count_pending_min_priority(0)
        .expect("count pending foreground");
    assert_eq!(
        pending_foreground, 1,
        "exactly one foreground task must remain PENDING (mid-burst precondition)"
    );

    let _ = &mut handler;
    let verify = vidcull_db::open_file(&db_path).expect("reopen db to verify");
    let groups = possible_groups(&verify);
    assert!(
        groups.contains(&sorted_pair(source_id, clip_id)),
        "the reframe pair's POSSIBLE group should be visible while foreground \
         work is still in flight (1 task still PENDING), not only after the \
         whole queue drains; possible_groups={groups:?}"
    );
}

#[test]
fn possible_group_appears_once_foreground_queue_fully_drains() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("index.db");

    let src = source_seq(0x5678, 40);
    let clip = clean_clip(&src, 10, 8);
    let (source_id, clip_id) = {
        let db = vidcull_db::open_file(&db_path).expect("open seed db");
        let source_id = seed_tier1(&db, "/v/source.mp4", 0x0102_0304_0506_0708);
        let clip_id = seed_tier1(&db, "/v/clip.mp4", 0xF1F2_F3F4_F5F6_F7F8);
        set_partial(&db, source_id, &src);
        set_partial(&db, clip_id, &clip);
        (source_id, clip_id)
    };
    {
        let mut db = vidcull_db::open_file(&db_path).expect("open enqueue db");
        enqueue_foreground_burst(&mut db, 6);
    }

    let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now_t0).with_partial_clips(true);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());

    let partial_tasks_in_queue = TaskQueueRepo::new(worker_db.conn())
        .list_by_state(vidcull_db::repo::TaskState::Pending)
        .expect("list pending")
        .iter()
        .filter(|t| {
            t.payload
                .as_deref()
                .and_then(|p| ChangeTask::from_payload(p).ok())
                .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
        })
        .count();
    eprintln!("[GATE-1] ① PartialFingerprint tasks queued pre-drain: {partial_tasks_in_queue}");
    assert_eq!(
        partial_tasks_in_queue, 0,
        "this fixture seeds completed partial fingerprints directly and never \
         enqueues a PartialFingerprint task, isolating the rebuild-trigger gap \
         (c) from an enqueue-wiring gap (b)"
    );

    let mut steps = 0;
    while daemon
        .step(&worker_db, &mut handler, T0)
        .expect("step")
        .is_some()
    {
        steps += 1;
        assert!(steps <= 6, "drained more tasks than enqueued");
    }
    eprintln!("[GATE-1] ③ steps drained to reach queue_drained()==true: {steps}");
    assert_eq!(steps, 6, "all six foreground tasks drain");

    let verify = vidcull_db::open_file(&db_path).expect("reopen db to verify");
    let groups = possible_groups(&verify);
    eprintln!("[GATE-1] possible_groups after full drain: {groups:?}");
    assert!(
        groups.contains(&sorted_pair(source_id, clip_id)),
        "once the foreground queue fully drains, `queue_drained()` trips true \
         and `handle()`'s `rebuild_matches()` (which includes `rebuild_partial`) \
         finally runs -- the reframe pair's POSSIBLE group must appear here; \
         possible_groups={groups:?}"
    );
}

#[test]
fn parallel_foreground_branch_rebuilds_partial_groups() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("index.db");

    let src = source_seq(0x5678, 40);
    let clip = clean_clip(&src, 10, 8);
    let (source_id, clip_id) = {
        let db = vidcull_db::open_file(&db_path).expect("open seed db");
        let source_id = seed_tier1(&db, "/v/source.mp4", 0x0102_0304_0506_0708);
        let clip_id = seed_tier1(&db, "/v/clip.mp4", 0xF1F2_F3F4_F5F6_F7F8);
        set_partial(&db, source_id, &src);
        set_partial(&db, clip_id, &clip);
        (source_id, clip_id)
    };

    {
        let db = vidcull_db::open_file(&db_path).expect("open enqueue db");
        TaskQueueRepo::new(db.conn())
            .enqueue(&NewTask {
                kind: "scan".to_owned(),
                priority: PARTIAL_PRIORITY,
                payload: Some(b"partial-task".to_vec()),
                enqueued_at: T0,
                size_bytes: 0,
            })
            .expect("enqueue low-priority partial task");
    }

    let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now_t0).with_partial_clips(true);

    handler
        .after_burst_chunk(2, false)
        .expect("after_burst_chunk with partial still pending");

    let verify = vidcull_db::open_file(&db_path).expect("reopen db to verify");
    let groups = possible_groups(&verify);
    eprintln!("[GATE-1] possible_groups after foreground-only after_burst_chunk: {groups:?}");
    assert!(
        groups.contains(&sorted_pair(source_id, clip_id)),
        "`after_burst_chunk`'s foreground-drained branch must also \
         run `rebuild_partial_foreground`, so an already-complete reframe pair \
         is visible with nothing left to decode; possible_groups={groups:?}"
    );
}
