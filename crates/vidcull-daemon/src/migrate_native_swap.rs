use vidcull_core::Result;
use vidcull_db::Database;
use vidcull_db::repo::{FingerprintsRepo, PartialSkipMarker, RegroupQueueRepo, SystemMetadataRepo};

use crate::indexing::PARTIAL_PRIORITY;
use crate::watcher::{ChangeKind, ChangeTask, enqueue_changes};

const PARTIAL_NATIVE_SWAP_KEY: &str = "partial_native_swap_migrated";

const MIGRATED_NON_FAST_PATH_REASON: &str = "migrated_non_fast_path";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeSwapMigration {
    pub reenqueued: usize,
    pub cleaned: usize,
    pub already_migrated: bool,
}

pub fn migrate_partial_native_swap(
    db: &mut Database,
    task_kind: &str,
    now: i64,
) -> Result<NativeSwapMigration> {
    if SystemMetadataRepo::new(db.conn()).contains(PARTIAL_NATIVE_SWAP_KEY)? {
        return Ok(NativeSwapMigration {
            already_migrated: true,
            ..NativeSwapMigration::default()
        });
    }

    let fast_path = FingerprintsRepo::new(db.conn()).list_partial_migration_fast_path()?;
    let changes: Vec<ChangeTask> = fast_path
        .into_iter()
        .map(|(_id, path)| ChangeTask {
            path,
            change: ChangeKind::PartialFingerprint,
            size_bytes: 0,
        })
        .collect();
    let reenqueued = enqueue_changes(db, &changes, task_kind, PARTIAL_PRIORITY, now)?;

    let non_fast = FingerprintsRepo::new(db.conn()).list_partial_migration_non_fast_path()?;
    let cleaned = non_fast.len();
    if cleaned > 0 {
        db.transaction(|conn| {
            let fingerprints = FingerprintsRepo::new(conn);
            let regroup = RegroupQueueRepo::new(conn);
            for (file_id, size_bytes, mtime_ns) in &non_fast {
                fingerprints.clear_partial_and_mark_skip(
                    *file_id,
                    &PartialSkipMarker {
                        reason: MIGRATED_NON_FAST_PATH_REASON.to_owned(),
                        size_bytes: *size_bytes,
                        mtime_ns: *mtime_ns,
                    },
                )?;
                regroup.mark(*file_id, now)?;
            }
            Ok(())
        })?;
    }

    SystemMetadataRepo::new(db.conn()).set(PARTIAL_NATIVE_SWAP_KEY, "1")?;

    Ok(NativeSwapMigration {
        reenqueued,
        cleaned,
        already_migrated: false,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use vidcull_core::types::{Codec, FileId, NormalizedPath};
    use vidcull_db::repo::{
        DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, PartialMihRepo,
        RegroupQueueRepo, SystemMetadataRepo, TaskQueueRepo, TaskState,
    };
    use vidcull_db::{Database, open_in_memory};
    use vidcull_fingerprint::format::{FORMAT_VERSION, encode_tier2};
    use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
    use vidcull_matcher::partial::durable::{
        BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
    };
    use vidcull_matcher::partial::partial_clip_params;

    use super::*;

    fn insert_file(db: &Database, path: &str, codec: Option<Codec>) -> FileId {
        FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: NormalizedPath::new(path),
                size_bytes: 1_000,
                mtime_ns: 2_000,
                codec,
                ..NewFile::default()
            })
            .expect("insert file")
    }

    fn seed_partial(db: &Database, file_id: FileId, blob: &[u8]) {
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&Fingerprint {
            file_id,
            tier1_global: vec![0u8; 64],
            tier2_temporal: None,
            format_version: u32::from(FORMAT_VERSION),
            created_at: 0,
        })
        .expect("upsert fingerprint");
        repo.set_partial(file_id, blob).expect("set partial");
    }

    fn pending_partial_count(db: &Database) -> usize {
        TaskQueueRepo::new(db.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending tasks")
            .iter()
            .filter(|t| {
                t.payload
                    .as_deref()
                    .and_then(|p| ChangeTask::from_payload(p).ok())
                    .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
            })
            .count()
    }

    #[test]
    fn migration_enqueues_cleans_and_sets_marker_once() {
        let mut db = open_in_memory().expect("open db");
        let fast = insert_file(&db, "/m/h264.mp4", Some(Codec::H264));
        seed_partial(&db, fast, &[1u8, 2, 3]);
        let av1 = insert_file(&db, "/m/av1.mp4", Some(Codec::Av1));
        seed_partial(&db, av1, &[4u8, 5, 6]);

        let out = migrate_partial_native_swap(&mut db, "scan", 100).expect("migrate");
        assert!(!out.already_migrated, "first boot runs the migration");
        assert_eq!(out.reenqueued, 1, "fast-path file re-enqueued");
        assert_eq!(out.cleaned, 1, "confirmed non-fast-path blob cleaned");

        assert!(
            SystemMetadataRepo::new(db.conn())
                .contains(PARTIAL_NATIVE_SWAP_KEY)
                .expect("marker"),
            "marker recorded after enqueue+cleanup",
        );
        assert_eq!(pending_partial_count(&db), 1, "one PartialFingerprint task");
        assert!(
            FingerprintsRepo::new(db.conn())
                .get_active_partial(fast)
                .expect("get")
                .is_some(),
            "fast-path blob is overwritten by the worker, not cleared here",
        );
        let fps = FingerprintsRepo::new(db.conn());
        assert!(
            fps.get_active_partial(av1).expect("get").is_none(),
            "av1 blob NULLed"
        );
        let marker = fps
            .get_partial_skip(av1)
            .expect("skip")
            .expect("marker present");
        assert_eq!(marker.reason, "migrated_non_fast_path");
        assert_eq!(marker.size_bytes, 1_000);
        assert_eq!(marker.mtime_ns, 2_000);
        assert!(
            RegroupQueueRepo::new(db.conn())
                .load()
                .expect("regroup")
                .contains(&av1),
            "av1 staged for the regroup delta",
        );

        let out2 = migrate_partial_native_swap(&mut db, "scan", 200).expect("migrate again");
        assert!(out2.already_migrated, "marker prevents a second run");
        assert_eq!(out2.reenqueued, 0);
        assert_eq!(out2.cleaned, 0);
        assert_eq!(
            pending_partial_count(&db),
            1,
            "no duplicate task on second boot"
        );
    }

    #[test]
    fn migration_resume_is_idempotent_with_task_dedup() {
        let mut db = open_in_memory().expect("open db");
        let fast = insert_file(&db, "/m/h264.mp4", Some(Codec::H264));
        seed_partial(&db, fast, &[1u8, 2, 3]);
        let av1 = insert_file(&db, "/m/av1.mp4", Some(Codec::Av1));
        seed_partial(&db, av1, &[4u8, 5, 6]);

        let out1 = migrate_partial_native_swap(&mut db, "scan", 100).expect("first run");
        assert_eq!(out1.reenqueued, 1);
        assert_eq!(out1.cleaned, 1);
        assert_eq!(pending_partial_count(&db), 1);

        SystemMetadataRepo::new(db.conn())
            .delete(PARTIAL_NATIVE_SWAP_KEY)
            .expect("clear marker");

        let out2 = migrate_partial_native_swap(&mut db, "scan", 200).expect("resume run");
        assert!(!out2.already_migrated);
        assert_eq!(out2.reenqueued, 0, "active PartialFingerprint task deduped");
        assert_eq!(out2.cleaned, 0, "already-cleaned av1 not re-cleaned");
        assert_eq!(pending_partial_count(&db), 1, "no duplicate task");

        for task in TaskQueueRepo::new(db.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending")
        {
            TaskQueueRepo::new(db.conn())
                .mark_done(task.id, 0)
                .expect("done");
        }
        SystemMetadataRepo::new(db.conn())
            .delete(PARTIAL_NATIVE_SWAP_KEY)
            .expect("clear marker");
        let out3 = migrate_partial_native_swap(&mut db, "scan", 300).expect("resume run 2");
        assert_eq!(
            out3.reenqueued, 1,
            "drained task re-enqueued (bounded re-decode)"
        );
        assert_eq!(pending_partial_count(&db), 1);
    }

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn scene(ts: u64, phash: u64) -> SceneHash {
        SceneHash {
            timestamp_ms: ts,
            phash,
        }
    }

    fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
        let mut state = seed;
        let scenes = (0..n)
            .map(|i| scene(i as u64 * 1000, splitmix64(&mut state) | 1))
            .collect();
        Tier2Fingerprint { scenes }
    }

    fn clip_of(source: &Tier2Fingerprint, start: usize, len: usize) -> Tier2Fingerprint {
        let scenes = source.scenes[start..start + len]
            .iter()
            .enumerate()
            .map(|(i, s)| scene(i as u64 * 1000, s.phash))
            .collect();
        Tier2Fingerprint { scenes }
    }

    #[test]
    fn cleanup_drops_partial_postings_and_group_via_delta() {
        let mut db = open_in_memory().expect("open db");
        let source = source_seq(0xC0DE_0001, 40);
        let clip = clip_of(&source, 10, 6);
        let source_id = insert_file(&db, "/m/source.mp4", Some(Codec::H264));
        let clip_id = insert_file(&db, "/m/clip.mp4", Some(Codec::Av1));
        seed_partial(
            &db,
            source_id,
            &encode_tier2(&source).expect("encode source"),
        );
        seed_partial(&db, clip_id, &encode_tier2(&clip).expect("encode clip"));

        let mut index =
            PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial);
        let all: BTreeSet<FileId> = [source_id, clip_id].into_iter().collect();
        rebuild_partial_clip_groups_durable(&mut index, &mut db, 0, &all).expect("bootstrap");

        let before = PartialMihRepo::new(db.conn())
            .load_all_postings()
            .expect("postings");
        assert!(
            before.iter().any(|p| p.file_id == clip_id),
            "clip has postings"
        );
        assert!(
            !DuplicateGroupsRepo::new(db.conn())
                .find_groups_containing(clip_id)
                .expect("groups")
                .is_empty(),
            "a POSSIBLE group formed for the clip⊂source pair",
        );

        let out = migrate_partial_native_swap(&mut db, "scan", 1).expect("migrate");
        assert_eq!(out.cleaned, 1, "av1 clip cleaned");
        assert_eq!(out.reenqueued, 1, "h264 source re-enqueued");

        let changed = RegroupQueueRepo::new(db.conn()).load().expect("regroup");
        assert!(changed.contains(&clip_id), "clip staged for the delta");
        rebuild_partial_clip_groups_durable(&mut index, &mut db, 2, &changed).expect("delta");

        let after = PartialMihRepo::new(db.conn())
            .load_all_postings()
            .expect("postings");
        assert!(
            !after.iter().any(|p| p.file_id == clip_id),
            "clip postings dropped by the delta",
        );
        assert!(
            DuplicateGroupsRepo::new(db.conn())
                .find_groups_containing(clip_id)
                .expect("groups")
                .is_empty(),
            "the POSSIBLE group dissolved",
        );
    }
}
