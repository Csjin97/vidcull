use vidcull_core::types::{Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath};
use vidcull_daemon::indexing::PARTIAL_PRIORITY;
use vidcull_daemon::{ChangeKind, ChangeTask, enqueue_partial_backfill};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, TaskQueueRepo,
    TaskState, TrustLevel,
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
fn exact_full_dup_twins_no_longer_churn_through_backfill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate226.db")).expect("open");

    let a = seed_indexed_file(&db, "/lib/mp4_ (125).mp4", 0xAA);
    let b = seed_indexed_file(&db, "/lib/mp4_125_partial.mp4", 0xAA);

    let n1 = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #1");
    assert_eq!(n1, 2, "both twins are missing-and-ungrouped → enqueued");
    assert_eq!(
        pending_partial_paths(&db),
        vec![
            "/lib/mp4_ (125).mp4".to_owned(),
            "/lib/mp4_125_partial.mp4".to_owned(),
        ]
    );

    let n1b = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #1 immediate re-drain");
    assert_eq!(n1b, 0, "PENDING dedup holds — no churn while still queued");

    let group_id = DuplicateGroupsRepo::new(db.conn())
        .create(TrustLevel::Exact, T0)
        .expect("create EXACT group");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(group_id, a)
        .expect("add member a");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(group_id, b)
        .expect("add member b");

    let done1 = mark_all_pending_partial_done(&db, T0 + 1);
    assert_eq!(
        done1, 2,
        "both PartialFingerprint tasks reach DONE via the gate short-circuit"
    );
    assert!(pending_partial_paths(&db).is_empty(), "queue drains");

    let have_partial_after_done = FingerprintsRepo::new(db.conn())
        .list_active_partial_or_skipped()
        .expect("list active partial or skipped after done");
    assert!(
        have_partial_after_done.is_empty(),
        "is_confirmed_full_dup short-circuit still writes NO marker (unchanged gate)"
    );

    let n2 = enqueue_partial_backfill(&mut db, KIND, T0 + 2).expect("backfill #2 (post-DONE)");
    assert_eq!(
        n2, 0,
        "fix-226: EXACT-group JOIN exclusion stops the post-DONE re-enqueue churn"
    );
    assert!(pending_partial_paths(&db).is_empty(), "no new tasks queued");

    let n3 =
        enqueue_partial_backfill(&mut db, KIND, T0 + 3).expect("backfill #3 (post-DONE again)");
    assert_eq!(
        n3, 0,
        "the exclusion holds on repeated drains, not just the first one"
    );

    let done_rows = TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Done)
        .expect("list done");
    let done_partial_count = done_rows
        .iter()
        .filter_map(|t| ChangeTask::from_payload(t.payload.as_ref()?).ok())
        .filter(|c| c.change == ChangeKind::PartialFingerprint)
        .count();
    assert_eq!(
        done_partial_count, 2,
        "exactly the 2 DONE rows from cycle 1 — no further growth across cycles 2/3"
    );
}

#[test]
fn a_stamped_skip_marker_still_stops_backfill_churn() {
    use vidcull_db::repo::PartialSkipMarker;

    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate226_control.db")).expect("open");

    let a = seed_indexed_file(&db, "/lib/marked_twin.mp4", 0xBB);

    let n1 = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #1");
    assert_eq!(n1, 1);
    mark_all_pending_partial_done(&db, T0 + 1);

    FingerprintsRepo::new(db.conn())
        .set_partial_skip(
            a,
            &PartialSkipMarker {
                reason: "duration-cap".to_owned(),
                size_bytes: 4096,
                mtime_ns: MTIME,
            },
        )
        .expect("stamp marker");

    let n2 = enqueue_partial_backfill(&mut db, KIND, T0 + 2).expect("backfill #2");
    assert_eq!(
        n2, 0,
        "a stamped marker still stops the churn (pre-existing mechanism, untouched)"
    );
}

#[test]
fn removing_one_exact_twin_lets_the_surviving_sibling_resume_partial_backfill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate226_recall.db")).expect("open");

    let a = seed_indexed_file(&db, "/lib/twin_a.mp4", 0xCC);
    let b = seed_indexed_file(&db, "/lib/twin_b.mp4", 0xCC);

    let n1 = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill #1");
    assert_eq!(n1, 2);
    let group_id = DuplicateGroupsRepo::new(db.conn())
        .create(TrustLevel::Exact, T0)
        .expect("create EXACT group");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(group_id, a)
        .expect("add a");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(group_id, b)
        .expect("add b");
    mark_all_pending_partial_done(&db, T0 + 1);

    let n2 = enqueue_partial_backfill(&mut db, KIND, T0 + 2).expect("backfill #2 (still twinned)");
    assert_eq!(
        n2, 0,
        "both twins remain EXACT-excluded while still grouped"
    );

    FilesRepo::new(db.conn())
        .mark_deleted(a, T0 + 3)
        .expect("soft-delete twin a");
    let groups = DuplicateGroupsRepo::new(db.conn());
    for group in groups
        .find_groups_containing(a)
        .expect("groups containing a")
    {
        groups
            .remove_member(group.id, a)
            .expect("remove a from group");
        if groups.list_members(group.id).expect("members").len() < 2 {
            groups.delete(group.id).expect("delete undersized group");
        }
    }

    let n3 = enqueue_partial_backfill(&mut db, KIND, T0 + 4).expect("backfill #3 (post-delete)");
    assert_eq!(
        n3, 1,
        "the surviving twin must be enqueued once its EXACT group is gone (relational recall)"
    );
    assert_eq!(
        pending_partial_paths(&db),
        vec!["/lib/twin_b.mp4".to_owned()]
    );
}

#[test]
fn very_likely_and_possible_group_members_are_still_enqueued_by_backfill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = vidcull_db::open_file(&dir.path().join("gate226_guard_a.db")).expect("open");

    let very_likely = seed_indexed_file(&db, "/lib/near_dup.mp4", 0xDD);
    let vl_gid = DuplicateGroupsRepo::new(db.conn())
        .create(TrustLevel::VeryLikely, T0)
        .expect("create VERY_LIKELY group");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(vl_gid, very_likely)
        .expect("add VL member");

    let possible = seed_indexed_file(&db, "/lib/possible.mp4", 0xEE);
    let poss_gid = DuplicateGroupsRepo::new(db.conn())
        .create(TrustLevel::Possible, T0)
        .expect("create POSSIBLE group");
    DuplicateGroupsRepo::new(db.conn())
        .add_member(poss_gid, possible)
        .expect("add POSSIBLE member");

    let n = enqueue_partial_backfill(&mut db, KIND, T0).expect("backfill");
    assert_eq!(
        n, 2,
        "VERY_LIKELY and POSSIBLE members must still be enqueued (recall, guard A)"
    );
    assert_eq!(
        pending_partial_paths(&db),
        vec![
            "/lib/near_dup.mp4".to_owned(),
            "/lib/possible.mp4".to_owned()
        ]
    );

    let pending = TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Pending)
        .expect("list pending for priority check");
    assert!(
        pending.iter().all(|t| t.priority == PARTIAL_PRIORITY),
        "backfilled tasks sit at PARTIAL_PRIORITY (-200)"
    );
}
