use std::sync::Arc;
use std::thread;

use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, NewTask, TaskQueueRepo,
    TaskState, TrustLevel,
};
use vidcull_db::{Database, open_file, open_in_memory};
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_matcher::near::{LshParams, rebuild_near_duplicate_groups};
use vidcull_matcher::ranking::assign_best_copies;

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;
const BASE_A: u64 = 0x0F0F_0F0F_0F0F_0F0F;
const BASE_B: u64 = 0xF0F0_F0F0_F0F0_F0F0;

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn seed_with_phash(db: &Database, path: &str, phash: u64) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: MTIME,
        inode: None,
        content_hash: None,
        codec: None,
        container: None,
        duration: None,
        fps_x1000: None,
        bitrate_bps: None,
        resolution: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    let id = FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file");
    let fp = Tier1Fingerprint {
        duration_ms: 60_000,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: phash,
    };
    let blob = format::encode_tier1(&fp).expect("encode tier1");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: blob,
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: T0,
        })
        .expect("upsert fingerprint");
    id
}

fn seed_corpus(db: &Database) {
    seed_with_phash(db, "/v/a1.mp4", BASE_A);
    seed_with_phash(db, "/v/a2.mp4", flip_low_bits(BASE_A, 2));
    seed_with_phash(db, "/v/a3.mp4", flip_low_bits(BASE_A, 4));
    seed_with_phash(db, "/v/b1.mp4", BASE_B);
    seed_with_phash(db, "/v/b2.mp4", flip_low_bits(BASE_B, 3));
}

fn members_snapshot(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut snapshots = Vec::new();
    for gid in 1..=256 {
        if let Some(group) = repo.get(gid).expect("get group") {
            if group.trust_level == TrustLevel::VeryLikely {
                let mut members: Vec<i64> = repo
                    .list_members(gid)
                    .expect("members")
                    .into_iter()
                    .map(|f| f.0)
                    .collect();
                members.sort_unstable();
                snapshots.push(members);
            }
        }
    }
    snapshots.sort();
    snapshots
}

#[test]
fn rebuild_stays_consistent_under_concurrent_queue_writes() {
    const ENQUEUES: usize = 200;
    const REBUILDS: usize = 5;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = Arc::new(dir.path().join("av.db"));

    {
        let db = open_file(path.as_ref()).expect("seed open");
        seed_corpus(&db);
    }

    let writer = {
        let path = Arc::clone(&path);
        thread::spawn(move || {
            let db = open_file(path.as_ref()).expect("writer open");
            let repo = TaskQueueRepo::new(db.conn());
            for _ in 0..ENQUEUES {
                repo.enqueue(&NewTask {
                    kind: "scan".to_owned(),
                    priority: 0,
                    payload: None,
                    enqueued_at: T0,
                    size_bytes: 0,
                })
                .expect("enqueue under contention");
                thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };

    let mut db = open_file(path.as_ref()).expect("matcher open");
    for _ in 0..REBUILDS {
        rebuild_near_duplicate_groups(&mut db, LshParams::default(), T0)
            .expect("near rebuild under contention (no SQLITE_BUSY)");
        assign_best_copies(&mut db, T0).expect("assign best copies under contention");
    }

    writer
        .join()
        .expect("writer thread (no SQLITE_BUSY surfaced)");

    let mut reference = open_in_memory().expect("reference db");
    seed_corpus(&reference);
    rebuild_near_duplicate_groups(&mut reference, LshParams::default(), T0)
        .expect("reference rebuild");
    assert_eq!(
        members_snapshot(&db),
        members_snapshot(&reference),
        "concurrent queue writes must not change the matcher's grouping",
    );

    assert_eq!(
        TaskQueueRepo::new(db.conn())
            .count_by_state(TaskState::Pending)
            .expect("count pending"),
        ENQUEUES as u64,
        "all concurrently-enqueued tasks persisted",
    );
}
