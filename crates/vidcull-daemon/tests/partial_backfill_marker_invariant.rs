use vidcull_core::types::{Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath};
use vidcull_daemon::{ChangeKind, ChangeTask, enqueue_partial_backfill};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, NewTask,
    PartialSkipMarker, TaskQueueRepo, TaskState, TrustLevel,
};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};

const KIND: &str = "scan";
const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn seed_indexed_file(db: &Database, path: &str, hash_byte: u8) -> FileId {
    let hash = Blake3Hash::from_bytes([hash_byte; HASH_LEN]);
    let id = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: NormalizedPath::new(path),
            size_bytes: 4096,
            mtime_ns: MTIME,
            content_hash: Some(hash),
            codec: Some(Codec::H264),
            first_seen_at: T0,
            last_seen_at: T0,
            ..Default::default()
        })
        .expect("insert file");
    let t1 = Tier1Fingerprint {
        duration_ms: 10_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: 1,
    };
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
            tier2_temporal: Some(vec![0u8; 8]),
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
    id
}

fn pending_partial_paths(db: &Database) -> Vec<String> {
    let mut paths: Vec<String> = TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Pending)
        .expect("list pending")
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref()?).ok())
        .filter(|c| c.change == ChangeKind::PartialFingerprint)
        .map(|c| c.path.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

fn mark_all_pending_partial_done(db: &Database, when: i64) -> usize {
    let repo = TaskQueueRepo::new(db.conn());
    let tasks = repo
        .list_by_state(TaskState::Pending)
        .expect("list pending");
    let mut marked = 0;
    for t in tasks {
        let Some(payload) = t.payload.as_ref() else {
            continue;
        };
        let Ok(change) = ChangeTask::from_payload(payload) else {
            continue;
        };
        if change.change == ChangeKind::PartialFingerprint {
            repo.mark_done(t.id, when).expect("mark done");
            marked += 1;
        }
    }
    marked
}

#[test]
fn only_the_durable_state_free_file_is_enqueued_then_backfill_reaches_a_fixed_point() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate228.db")).expect("open");

    let has_fp = seed_indexed_file(&db, "/lib/has_fp.mp4", 0x11);
    FingerprintsRepo::new(db.conn())
        .set_partial(has_fp, &[0u8; 8])
        .expect("set_partial");

    let has_marker = seed_indexed_file(&db, "/lib/has_marker.mp4", 0x22);
    FingerprintsRepo::new(db.conn())
        .set_partial_skip(
            has_marker,
            &PartialSkipMarker {
                reason: "duration-cap".to_owned(),
                size_bytes: 4096,
                mtime_ns: MTIME,
            },
        )
        .expect("set_partial_skip");

    let exact_a = seed_indexed_file(&db, "/lib/exact_a.mp4", 0x33);
    let exact_b = seed_indexed_file(&db, "/lib/exact_b.mp4", 0x33);
    let exact_gid = DuplicateGroupsRepo::new(db.conn())
        .create(TrustLevel::Exact, T0)
        .expect("create EXACT group");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(exact_gid, exact_a)
        .expect("add exact_a");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(exact_gid, exact_b)
        .expect("add exact_b");

    let missing = seed_indexed_file(&db, "/lib/missing.mp4", 0x44);

    let n1 = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #1");
    assert_eq!(n1, 1, "only the durable-state-free file is missing");
    assert_eq!(
        pending_partial_paths(&db),
        vec!["/lib/missing.mp4".to_owned()],
        "classes A/B/C must all stay excluded; only class D is enqueued"
    );

    FingerprintsRepo::new(db.conn())
        .set_partial_skip(
            missing,
            &PartialSkipMarker {
                reason: "no-scenes".to_owned(),
                size_bytes: 4096,
                mtime_ns: MTIME,
            },
        )
        .expect("set_partial_skip on missing");
    mark_all_pending_partial_done(&db, T0 + 1);

    let n2 = enqueue_partial_backfill(&mut db, KIND, T0 + 2).expect("backfill #2 (fixed point)");
    assert_eq!(
        n2, 0,
        "every file now carries a durable state — backfill is a fixed point"
    );
    assert!(pending_partial_paths(&db).is_empty(), "no new tasks queued");

    let n3 = enqueue_partial_backfill(&mut db, KIND, T0 + 3).expect("backfill #3 (fixed point)");
    assert_eq!(n3, 0, "the fixed point holds on repeated drains");

    let done_partial_count = TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Done)
        .expect("list done")
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref()?).ok())
        .filter(|c| c.change == ChangeKind::PartialFingerprint)
        .count();
    assert_eq!(
        done_partial_count, 1,
        "exactly the 1 DONE row from cycle 1's single enqueue — no growth after"
    );
}

#[test]
fn exact_full_dup_marker_is_observability_only_join_is_the_exclusion_authority() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate232.db")).expect("open");

    let marker_less = seed_indexed_file(&db, "/lib/twin_marker_less.mp4", 0x55);
    let survivor = seed_indexed_file(&db, "/lib/twin_survivor.mp4", 0x55);
    let gid = DuplicateGroupsRepo::new(db.conn())
        .create(TrustLevel::Exact, T0)
        .expect("create EXACT group");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(gid, marker_less)
        .expect("add marker_less");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(gid, survivor)
        .expect("add survivor");
    FingerprintsRepo::new(db.conn())
        .set_partial_skip(
            survivor,
            &PartialSkipMarker {
                reason: "exact-full-dup".to_owned(),
                size_bytes: 4096,
                mtime_ns: MTIME,
            },
        )
        .expect("set_partial_skip on survivor");

    let n1 = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #1 (group intact)");
    assert_eq!(
        n1, 0,
        "both the marker-less and the marker-bearing EXACT member stay \
         excluded while the group exists"
    );
    assert!(
        pending_partial_paths(&db).is_empty(),
        "no tasks queued for either member"
    );

    let groups = DuplicateGroupsRepo::new(db.conn());
    groups
        .remove_member(gid, marker_less)
        .expect("remove marker_less from group");
    assert!(
        groups.list_members(gid).expect("list members").len() < 2,
        "only the survivor remains"
    );
    groups
        .delete(gid)
        .expect("delete the now-undersized EXACT group");
    FilesRepo::new(db.conn())
        .mark_deleted(marker_less, T0 + 1)
        .expect("soft-delete marker_less");

    let n2 = enqueue_partial_backfill(&mut db, KIND, T0 + 2).expect("backfill #2 (sibling gone)");
    assert_eq!(
        n2, 1,
        "once the sibling is gone, the survivor's stale exact-full-dup marker \
         must NOT permanently strand it — it must be re-enqueued"
    );
    assert_eq!(
        pending_partial_paths(&db),
        vec!["/lib/twin_survivor.mp4".to_owned()],
        "the survivor, and only the survivor, is re-enqueued"
    );
}

#[test]
fn failed_rows_alone_never_bound_the_backfill_churn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate231a.db")).expect("open");

    seed_indexed_file(&db, "/lib/flaky.mp4", 0x66);
    let payload = ChangeTask {
        path: NormalizedPath::new("/lib/flaky.mp4"),
        change: ChangeKind::PartialFingerprint,
        size_bytes: 0,
    }
    .to_payload()
    .expect("encode payload");

    let repo = TaskQueueRepo::new(db.conn());
    for attempt in 0..5 {
        let id = repo
            .enqueue(&NewTask {
                kind: KIND.to_owned(),
                priority: -200,
                payload: Some(payload.clone()),
                enqueued_at: T0 + attempt,
                size_bytes: 0,
            })
            .expect("enqueue");
        repo.dequeue_next(KIND, T0 + attempt)
            .expect("dequeue")
            .expect("task");
        repo.mark_failed(id, T0 + attempt, "io error")
            .expect("mark failed");
    }

    let n = enqueue_partial_backfill(&mut db, KIND, T0 + 100).expect("backfill");
    assert_eq!(
        n, 1,
        "no durable marker exists yet, so the backfill re-enqueues regardless \
         of 5 prior FAILED rows — the bound must come from the producer \
         stamping a marker, not from enqueue_partial_backfill itself"
    );
}

#[test]
fn terminal_failure_class_below_budget_retries_then_reaches_fixed_point_at_budget() {
    const RETRY_BUDGET: i64 = 2;

    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate231b.db")).expect("open");
    let path = "/lib/flaky2.mp4";
    let file_id = seed_indexed_file(&db, path, 0x77);

    let n0 = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #0");
    assert_eq!(
        n0, 1,
        "first drain enqueues the file (no durable state yet)"
    );

    for attempt in 1..RETRY_BUDGET {
        let repo = TaskQueueRepo::new(db.conn());
        let task = repo
            .dequeue_next(KIND, T0 + attempt)
            .expect("dequeue")
            .expect("the enqueued task");
        repo.mark_failed(task.id, T0 + attempt, "io error")
            .expect("mark failed");

        let n = enqueue_partial_backfill(&mut db, KIND, T0 + attempt + 1).expect("backfill");
        assert_eq!(
            n, 1,
            "attempt {attempt}: below PARTIAL_RETRY_BUDGET, still no durable \
             state → the backfill re-enqueues"
        );
    }

    let repo = TaskQueueRepo::new(db.conn());
    let task = repo
        .dequeue_next(KIND, T0 + 100)
        .expect("dequeue")
        .expect("the enqueued task");
    FingerprintsRepo::new(db.conn())
        .set_partial_skip(
            file_id,
            &PartialSkipMarker {
                reason: "retry-exhausted".to_owned(),
                size_bytes: 4096,
                mtime_ns: MTIME,
            },
        )
        .expect("set_partial_skip at budget");
    TaskQueueRepo::new(db.conn())
        .mark_done(task.id, T0 + 100)
        .expect("mark done (skip exit is Ok)");

    let n_fixed =
        enqueue_partial_backfill(&mut db, KIND, T0 + 101).expect("backfill (fixed point)");
    assert_eq!(
        n_fixed, 0,
        "once the retry-budget marker lands, the backfill reaches a fixed \
         point even though RETRY_BUDGET-1 prior FAILED rows remain in history"
    );
}
