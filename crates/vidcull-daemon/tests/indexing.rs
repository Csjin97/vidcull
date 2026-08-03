/**
 * @file    `indexing.rs`
 * @brief   데몬 인덱싱 파이프라인 통합 테스트
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 초기 스캔 대용량 배치 검증을 5,000개 파일로 강화
 */
use std::path::{Path, PathBuf};

use vidcull_core::types::{Codec, NormalizedPath};
use vidcull_daemon::{
    ChangeKind, ChangeTask, Daemon, DaemonConfig, IndexingHandler, ShutdownToken, enqueue_changes,
    enqueue_initial_scan, enqueue_initial_scan_until,
};
use vidcull_db::Database;
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, RegroupQueueRepo,
    TaskQueueRepo, TaskState, TrustLevel,
};
use vidcull_synth::{FfmpegBinaries, render_source};

const NOW: i64 = 1_700_000_000;

fn now() -> i64 {
    NOW
}

fn now_later() -> i64 {
    NOW + 100
}

#[test]
fn indexes_files_and_forms_exact_duplicate_group() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP indexes_files_and_forms_exact_duplicate_group: ffmpeg not resolvable");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let copy_a = render_source(&bins, dir, "copy_a", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy A");
    let copy_b = render_source(&bins, dir, "copy_b", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy B");
    let other = render_source(&bins, dir, "other", "mandelbrot", 2000, 320, 180, 30, 6)
        .expect("render other");

    let changes: Vec<ChangeTask> = [&copy_a, &copy_b, &other]
        .iter()
        .map(|p| ChangeTask {
            path: NormalizedPath::new(p),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        })
        .collect();
    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let n = enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue");
        assert_eq!(n, 3, "all three changes enqueued");
    }

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    let mut processed = 0;
    while daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .is_some()
    {
        processed += 1;
        assert!(processed <= 3, "drained more tasks than enqueued");
    }
    assert_eq!(processed, 3, "exactly three tasks processed");

    let metrics = handler.fallback_metrics();
    assert_eq!(
        metrics.native_count(),
        2,
        "two genuine native decodes recorded"
    );
    assert_eq!(
        metrics.fallback_count(),
        0,
        "no fallback codecs in this corpus"
    );
    assert!(
        metrics.fallback_rate().abs() < f64::EPSILON,
        "fallback rate is zero with an all-native corpus",
    );

    let verify = vidcull_db::open_file(&db_path).expect("reopen db");
    let files = FilesRepo::new(verify.conn());
    let active = files.list_active().expect("list files");
    assert_eq!(active.len(), 3, "three files indexed");

    let fps = FingerprintsRepo::new(verify.conn());
    for f in &active {
        let fp = fps.get(f.id).expect("get fingerprint");
        assert!(fp.is_some(), "every file has a fingerprint: {:?}", f.path);
        assert!(
            f.content_hash.is_some(),
            "every file has a content hash: {:?}",
            f.path
        );
    }

    assert!(
        RegroupQueueRepo::new(verify.conn())
            .is_empty()
            .expect("regroup_queue len"),
        "the handler cleared the durable regroup delta after the drained burst",
    );

    let groups = DuplicateGroupsRepo::new(verify.conn());
    let exact: Vec<_> = groups
        .list_all()
        .expect("list groups")
        .into_iter()
        .filter(|g| g.trust_level == TrustLevel::Exact)
        .collect();
    assert_eq!(exact.len(), 1, "exactly one EXACT group, got {exact:?}");
    let members = groups.list_members(exact[0].id).expect("members");
    assert_eq!(
        members.len(),
        2,
        "the EXACT group holds both identical copies"
    );
    assert!(
        exact[0].best_file_id.is_some(),
        "best copy assigned to the EXACT group"
    );
    assert!(
        members.contains(&exact[0].best_file_id.expect("best")),
        "the best copy is one of the group members"
    );
}

#[test]
fn prewarm_fills_the_thumbnail_cache_for_a_new_cluster() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!(
            "SKIP prewarm_fills_the_thumbnail_cache_for_a_new_cluster: ffmpeg not resolvable"
        );
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");
    let cache_dir = dir.join("thumbs");

    let copy_a = render_source(&bins, dir, "copy_a", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy A");
    let copy_b = render_source(&bins, dir, "copy_b", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy B");

    let changes: Vec<ChangeTask> = [&copy_a, &copy_b]
        .iter()
        .map(|p| ChangeTask {
            path: NormalizedPath::new(p),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        })
        .collect();
    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let n = enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue");
        assert_eq!(n, 2, "both changes enqueued");
    }

    let thumbnails = std::sync::Arc::new(vidcull_daemon::thumbnails::ThumbnailProvider::new(
        cache_dir.clone(),
        Some(bins.clone()),
    ));
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now)
        .with_thumbnails(std::sync::Arc::clone(&thumbnails));
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    while daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .is_some()
    {}

    let mut cached_files = 0usize;
    for _ in 0..50 {
        cached_files = std::fs::read_dir(&cache_dir)
            .map(Iterator::count)
            .unwrap_or(0);
        if cached_files > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        cached_files > 0,
        "prewarm must have written at least one cached thumbnail under {cache_dir:?}, found none \
         after waiting up to 5s",
    );
}

fn drain_burst(
    db_path: &Path,
    paths: &[&PathBuf],
    worker_db: &Database,
    handler: &mut IndexingHandler,
    daemon: &Daemon,
) {
    let changes: Vec<ChangeTask> = paths
        .iter()
        .map(|p| ChangeTask {
            path: NormalizedPath::new(p),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        })
        .collect();
    {
        let mut db = vidcull_db::open_file(db_path).expect("open enqueue db");
        enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue burst");
    }
    while daemon
        .step(worker_db, handler, NOW)
        .expect("step")
        .is_some()
    {}
}

#[test]
fn remove_change_soft_deletes_the_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("index.db");
    let path = "/lib/gone.mp4";

    let file_id = {
        let db = vidcull_db::open_file(&db_path).expect("open seed db");
        FilesRepo::new(db.conn())
            .insert(&NewFile {
                path: NormalizedPath::new(path),
                ..Default::default()
            })
            .expect("insert file row")
    };

    {
        let mut db = vidcull_db::open_file(&db_path).expect("open enqueue db");
        enqueue_changes(
            &mut db,
            &[ChangeTask {
                path: NormalizedPath::new(path),
                change: ChangeKind::Remove,
                size_bytes: 0,
            }],
            "scan",
            0,
            NOW,
        )
        .expect("enqueue remove");
    }

    let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    while daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .is_some()
    {}

    let verify = vidcull_db::open_file(&db_path).expect("reopen db");
    let record = FilesRepo::new(verify.conn())
        .get(file_id)
        .expect("get")
        .expect("file row still exists (soft-deleted, not hard-deleted)");
    assert!(
        record.deleted_at.is_some(),
        "Remove must soft-delete the file"
    );
}

#[test]
fn partial_clips_off_enqueues_no_partial_fingerprint_pass() {
    fn pending_partial_passes(bins: &FfmpegBinaries, enabled: bool) -> usize {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let db_path = dir.join("index.db");
        let clip = render_source(bins, dir, "clip", "testsrc", 2000, 320, 180, 30, 6)
            .expect("render clip");
        {
            let mut db = vidcull_db::open_file(&db_path).expect("open db");
            enqueue_changes(
                &mut db,
                &[ChangeTask {
                    path: NormalizedPath::new(&clip),
                    change: ChangeKind::Upsert,
                    size_bytes: 0,
                }],
                "scan",
                0,
                NOW,
            )
            .expect("enqueue upsert");
        }

        let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
        let mut handler =
            IndexingHandler::new(handler_db, bins.clone(), now).with_partial_clips(enabled);
        let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
        let daemon = Daemon::new(DaemonConfig::default());

        daemon
            .step(&worker_db, &mut handler, NOW)
            .expect("step")
            .expect("the upsert task was processed");

        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        TaskQueueRepo::new(verify.conn())
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

    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!(
            "SKIP partial_clips_off_enqueues_no_partial_fingerprint_pass: ffmpeg not resolvable"
        );
        return;
    };

    assert_eq!(
        pending_partial_passes(&bins, false),
        0,
        "partial OFF must enqueue no PartialFingerprint pass",
    );
    assert_eq!(
        pending_partial_passes(&bins, true),
        1,
        "partial ON enqueues exactly one PartialFingerprint pass — proving the gate, \
         not an unrelated path, is what makes OFF inert",
    );
}

fn very_likely_groups(db: &Database) -> Vec<Vec<i64>> {
    let groups = DuplicateGroupsRepo::new(db.conn());
    let mut out: Vec<Vec<i64>> = groups
        .list_all()
        .expect("list groups")
        .into_iter()
        .filter(|g| g.trust_level == TrustLevel::VeryLikely)
        .map(|g| {
            let mut members: Vec<i64> = groups
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

#[test]
fn second_burst_incrementally_groups_a_new_near_duplicate() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!(
            "SKIP second_burst_incrementally_groups_a_new_near_duplicate: ffmpeg not resolvable"
        );
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let copy_a = render_source(&bins, dir, "copy_a", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy A");
    let other = render_source(&bins, dir, "other", "mandelbrot", 2000, 320, 180, 30, 6)
        .expect("render other");
    let copy_b = render_source(&bins, dir, "copy_b", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy B");

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());

    drain_burst(
        &db_path,
        &[&copy_a, &other],
        &worker_db,
        &mut handler,
        &daemon,
    );
    {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        assert!(
            very_likely_groups(&verify).is_empty(),
            "testsrc and mandelbrot are not near-duplicates",
        );
    }

    drain_burst(&db_path, &[&copy_b], &worker_db, &mut handler, &daemon);
    let verify = vidcull_db::open_file(&db_path).expect("reopen db");
    let very = very_likely_groups(&verify);
    assert_eq!(
        very.len(),
        1,
        "the new copy forms one VERY_LIKELY group, got {very:?}",
    );
    assert_eq!(very[0].len(), 2, "the group holds both identical copies");
}

#[test]
fn unchanged_file_is_skipped_on_rescan() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP unchanged_file_is_skipped_on_rescan: ffmpeg not resolvable");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");
    let clip =
        render_source(&bins, dir, "clip", "testsrc", 2000, 320, 180, 30, 6).expect("render clip");

    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());

    {
        let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
        let mut handler = IndexingHandler::new(handler_db, bins.clone(), now);
        drain_burst(&db_path, &[&clip], &worker_db, &mut handler, &daemon);
    }
    let (id, created_at) = {
        let v = vidcull_db::open_file(&db_path).expect("reopen");
        let active = FilesRepo::new(v.conn()).list_active().expect("list");
        assert_eq!(active.len(), 1, "the clip is indexed once");
        let fp = FingerprintsRepo::new(v.conn())
            .get(active[0].id)
            .expect("get fp")
            .expect("fp exists");
        (active[0].id, fp.created_at)
    };
    assert_eq!(created_at, NOW, "first scan stamps the fingerprint at NOW");

    {
        let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
        let mut handler = IndexingHandler::new(handler_db, bins, now_later);
        drain_burst(&db_path, &[&clip], &worker_db, &mut handler, &daemon);
    }
    let v = vidcull_db::open_file(&db_path).expect("reopen");
    let fp = FingerprintsRepo::new(v.conn())
        .get(id)
        .expect("get fp")
        .expect("fp exists");
    assert_eq!(
        fp.created_at, NOW,
        "an unchanged file must be skipped, not re-fingerprinted (created_at unchanged)"
    );
    assert_eq!(
        FilesRepo::new(v.conn()).list_active().expect("list").len(),
        1,
        "still exactly one file"
    );
}

#[test]
fn byte_identical_copy_reuses_fingerprint_without_decoding() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP byte_identical_copy_reuses_fingerprint_without_decoding: ffmpeg");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let copy_a = render_source(&bins, dir, "copy_a", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy A");
    let copy_b = render_source(&bins, dir, "copy_b", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy B");

    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());

    {
        let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
        let mut handler = IndexingHandler::new(handler_db, bins, now);
        drain_burst(&db_path, &[&copy_a], &worker_db, &mut handler, &daemon);
    }

    let bogus = FfmpegBinaries::from_dir(dir);
    {
        let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
        let mut handler = IndexingHandler::new(handler_db, bogus, now);
        drain_burst(&db_path, &[&copy_b], &worker_db, &mut handler, &daemon);
    }

    let v = vidcull_db::open_file(&db_path).expect("reopen");
    let files = FilesRepo::new(v.conn());
    let active = files.list_active().expect("list");
    assert_eq!(
        active.len(),
        2,
        "both copies are indexed — the second via fingerprint reuse, not decode"
    );
    let fps = FingerprintsRepo::new(v.conn());
    let a = active
        .iter()
        .find(|f| f.path.as_str().contains("copy_a"))
        .expect("copy_a row");
    let b = active
        .iter()
        .find(|f| f.path.as_str().contains("copy_b"))
        .expect("copy_b row");
    let fp_a = fps.get(a.id).expect("get").expect("copy_a fp");
    let fp_b = fps.get(b.id).expect("get").expect("copy_b fp");
    assert_eq!(
        fp_b.tier1_global, fp_a.tier1_global,
        "the copy reuses the twin's tier1 fingerprint verbatim"
    );
    assert_eq!(
        fp_b.tier2_temporal, fp_a.tier2_temporal,
        "the copy reuses the twin's tier2 fingerprint verbatim"
    );

    let groups = DuplicateGroupsRepo::new(v.conn());
    let exact = groups
        .list_all()
        .expect("groups")
        .into_iter()
        .filter(|g| g.trust_level == TrustLevel::Exact)
        .count();
    assert_eq!(exact, 1, "the two identical copies form one EXACT group");
}

#[test]
fn initial_scan_enqueues_existing_video_files_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    for name in ["a.mp4", "b.mkv", "nested/c.mov"] {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, b"stub").expect("write video stub");
    }
    for name in ["notes.txt", "cover.jpg"] {
        std::fs::write(root.join(name), b"stub").expect("write non-video");
    }

    let db_path = root.join("scan.db");
    let roots = vec![root.to_path_buf()];
    let mut db = vidcull_db::open_file(&db_path).expect("open db");
    let enqueued = enqueue_initial_scan(&mut db, &roots, "scan", NOW, &[]).expect("initial scan");
    assert_eq!(enqueued, 3, "exactly the three video files are enqueued");

    let pending = TaskQueueRepo::new(db.conn())
        .count_by_state(TaskState::Pending)
        .expect("count");
    assert_eq!(pending, 3, "three pending tasks queued");
}

#[test]
fn initial_scan_skips_already_indexed_unchanged_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    std::fs::write(root.join("a.mp4"), b"stub-a").expect("write a");
    std::fs::write(root.join("b.mkv"), b"stub-b").expect("write b");

    let db_path = root.join("scan.db");
    let roots = vec![root.to_path_buf()];
    let mut db = vidcull_db::open_file(&db_path).expect("open db");

    assert_eq!(
        enqueue_initial_scan(&mut db, &roots, "scan", NOW, &[]).expect("scan1"),
        2
    );

    {
        let repo = TaskQueueRepo::new(db.conn());
        while let Some(t) = repo.dequeue_next("scan", NOW).expect("dq") {
            repo.mark_done(t.id, NOW).expect("done");
        }
    }

    {
        let opts = vidcull_scanner::ScanOptions::default();
        let files = FilesRepo::new(db.conn());
        for entry in vidcull_scanner::walk(root, &opts).flatten() {
            files
                .insert(&NewFile {
                    path: entry.path.clone(),
                    size_bytes: i64::try_from(entry.fingerprint.size_bytes).unwrap_or(0),
                    mtime_ns: i64::try_from(entry.fingerprint.mtime_ns).unwrap_or(0),
                    inode: entry
                        .fingerprint
                        .inode
                        .map(|i| i64::try_from(i).unwrap_or(0)),
                    ..Default::default()
                })
                .expect("seed indexed row");
        }
    }

    assert_eq!(
        enqueue_initial_scan(&mut db, &roots, "scan", NOW, &[]).expect("scan2"),
        0,
        "already-indexed unchanged files must not be re-enqueued"
    );

    std::fs::write(root.join("a.mp4"), b"stub-a-now-longer-content").expect("modify a");
    assert_eq!(
        enqueue_initial_scan(&mut db, &roots, "scan", NOW, &[]).expect("scan3"),
        1,
        "a changed file is re-enqueued; the unchanged one is still skipped"
    );
}

#[test]
fn initial_scan_streams_large_corpus_across_batch_boundaries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    const N: usize = 5_000;
    for i in 0..N {
        std::fs::write(root.join(format!("v{i:05}.mp4")), b"stub").expect("write video");
    }

    let db_path = root.join("scan.db");
    let roots = vec![root.to_path_buf()];
    let mut db = vidcull_db::open_file(&db_path).expect("open db");

    assert_eq!(
        enqueue_initial_scan(&mut db, &roots, "scan", NOW, &[]).expect("scan"),
        N,
        "every file across all batches is enqueued exactly once"
    );
    let pending = TaskQueueRepo::new(db.conn())
        .count_by_state(TaskState::Pending)
        .expect("count");
    assert_eq!(
        pending, N as u64,
        "one pending task per file, no drops or duplicates"
    );

    assert_eq!(
        enqueue_initial_scan(&mut db, &roots, "scan", NOW, &[]).expect("rescan"),
        0,
        "a second scan re-enqueues nothing"
    );
}

#[test]
fn initial_scan_until_stops_early_when_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    for name in ["a.mp4", "b.mkv", "c.mov"] {
        std::fs::write(root.join(name), b"stub").expect("write video");
    }

    let db_path = root.join("scan.db");
    let roots = vec![root.to_path_buf()];
    let mut db = vidcull_db::open_file(&db_path).expect("open db");

    let discovered = std::sync::atomic::AtomicU64::new(0);
    let enqueued =
        enqueue_initial_scan_until(&mut db, &roots, "scan", NOW, &[], &|| true, &discovered)
            .expect("scan");
    assert_eq!(enqueued, 0, "an already-cancelled scan enqueues nothing");

    let discovered = std::sync::atomic::AtomicU64::new(0);
    let enqueued =
        enqueue_initial_scan_until(&mut db, &roots, "scan", NOW, &[], &|| false, &discovered)
            .expect("scan");
    assert_eq!(enqueued, 3, "without cancel the whole corpus is enqueued");
    assert_eq!(
        discovered.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "discovered counter tracks the enqueued total for the live UI indicator"
    );
}

#[test]
fn corrupted_files_fail_individually_while_the_daemon_keeps_going() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP corrupted_files_fail_individually_while_the_daemon_keeps_going: ffmpeg");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let valid =
        render_source(&bins, dir, "valid", "testsrc", 2000, 320, 180, 30, 6).expect("render valid");

    let zero = dir.join("zero.mp4");
    std::fs::write(&zero, b"").expect("write zero-byte file");

    let random = dir.join("random.mp4");
    let noise: Vec<u8> = (0..4096u32)
        .map(|i| i.wrapping_mul(31).wrapping_add(7).to_le_bytes()[0])
        .collect();
    std::fs::write(&random, &noise).expect("write random file");

    let truncated = dir.join("truncated.mp4");
    let head = std::fs::read(&valid).expect("read valid");
    assert!(
        head.len() > 128,
        "render is larger than the truncation point"
    );
    std::fs::write(&truncated, &head[..128]).expect("write truncated file");

    let order = [&valid, &zero, &random, &truncated];
    let changes: Vec<ChangeTask> = order
        .iter()
        .map(|p| ChangeTask {
            path: NormalizedPath::new(p),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        })
        .collect();
    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let n = enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue");
        assert_eq!(n, 4, "all four changes enqueued");
    }

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    let mut processed = 0;
    while daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .is_some()
    {
        processed += 1;
        assert!(processed <= 4, "drained more tasks than enqueued");
    }
    assert_eq!(processed, 4, "every task was claimed and resolved");

    let verify = vidcull_db::open_file(&db_path).expect("reopen db");
    let queue = TaskQueueRepo::new(verify.conn());
    assert_eq!(
        queue.count_by_state(TaskState::Done).expect("done count"),
        1,
        "only the valid file's task finished"
    );
    let failed = queue.list_by_state(TaskState::Failed).expect("failed rows");
    assert_eq!(failed.len(), 3, "the three corrupt files each failed");
    for task in &failed {
        let reason = task.last_error.as_deref().unwrap_or("");
        assert!(
            !reason.is_empty(),
            "a failed task records why it failed: {task:?}"
        );
    }

    let files = FilesRepo::new(verify.conn());
    let active = files.list_active().expect("list files");
    assert_eq!(active.len(), 1, "exactly the valid file is indexed");
    let fps = FingerprintsRepo::new(verify.conn());
    assert!(
        fps.get(active[0].id).expect("get fp").is_some(),
        "the valid file has a fingerprint"
    );
}

#[tokio::test]
async fn concurrent_indexing_runs_rebuild_exactly_once() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP concurrent_indexing_runs_rebuild_exactly_once: ffmpeg not resolvable");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let copy_a = render_source(&bins, dir, "copy_a", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy A");
    let copy_b = render_source(&bins, dir, "copy_b", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy B");
    let other1 = render_source(&bins, dir, "other1", "mandelbrot", 2000, 320, 180, 30, 6)
        .expect("render other1");
    let other2 = render_source(&bins, dir, "other2", "mandelbrot", 2000, 320, 180, 30, 7)
        .expect("render other2");

    let changes: Vec<ChangeTask> = [&copy_a, &copy_b, &other1, &other2]
        .iter()
        .map(|p| ChangeTask {
            path: NormalizedPath::new(p),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        })
        .collect();
    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let n = enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue");
        assert_eq!(n, 4, "all changes enqueued");
    }

    let token = ShutdownToken::new();
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let handler = IndexingHandler::new(handler_db, bins, now);

    let throttle_control = std::sync::Arc::new(vidcull_daemon::ThrottleControl::default());
    throttle_control.set_level(vidcull_ipc::CpuThrottle::Full);

    let config = DaemonConfig {
        kind: "scan".to_owned(),
        poll_interval: std::time::Duration::from_millis(50),
        throttle: vidcull_daemon::ThrottleConfig {
            idle_workers: 4,
            ..Default::default()
        },
        throttle_control,
    };
    let daemon = Daemon::new(config);

    let db = vidcull_db::open_file(&db_path).expect("open daemon db");
    let run_token = token.clone();
    let daemon_task = tokio::spawn(async move {
        daemon
            .run_async_throttled(
                db,
                handler,
                run_token,
                || NOW,
                || vidcull_daemon::Activity::Idle,
            )
            .await
    });

    let verify = vidcull_db::open_file(&db_path).expect("reopen verification db");
    let queue = TaskQueueRepo::new(verify.conn());
    let mut success = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(pending) = queue.count_by_state(TaskState::Pending) {
            if pending == 0 {
                success = true;
                break;
            }
        }
    }
    assert!(success, "tasks failed to drain in time");

    token.trigger();

    let stats = daemon_task.await.expect("join daemon").expect("daemon run");
    assert_eq!(stats.processed, 4, "exactly four files processed");

    let files = FilesRepo::new(verify.conn());
    let active = files.list_active().expect("list files");
    assert_eq!(active.len(), 4, "four files indexed");

    let groups = DuplicateGroupsRepo::new(verify.conn());
    let exact: Vec<_> = groups
        .list_all()
        .expect("list groups")
        .into_iter()
        .filter(|g| g.trust_level == TrustLevel::Exact)
        .collect();
    assert_eq!(exact.len(), 1, "exactly one EXACT group formed");

    let very = very_likely_groups(&verify);
    assert_eq!(very.len(), 2, "exactly two VERY_LIKELY groups formed");
}

#[test]
fn concurrent_indexing_runs_rebuild_exactly_once_sync() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP concurrent_indexing_runs_rebuild_exactly_once_sync: ffmpeg not resolvable");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let copy_a = render_source(&bins, dir, "copy_a", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy A");
    let copy_b = render_source(&bins, dir, "copy_b", "testsrc", 2000, 320, 180, 30, 6)
        .expect("render copy B");

    let changes: Vec<ChangeTask> = [&copy_a, &copy_b]
        .iter()
        .map(|p| ChangeTask {
            path: NormalizedPath::new(p),
            change: ChangeKind::Upsert,
            size_bytes: 0,
        })
        .collect();
    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let n = enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue");
        assert_eq!(n, 2);
    }

    let token = ShutdownToken::new();
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let handler = IndexingHandler::new(handler_db, bins, now);

    let throttle_control = std::sync::Arc::new(vidcull_daemon::ThrottleControl::default());
    throttle_control.set_level(vidcull_ipc::CpuThrottle::Full);

    let config = DaemonConfig {
        kind: "scan".to_owned(),
        poll_interval: std::time::Duration::from_millis(50),
        throttle: vidcull_daemon::ThrottleConfig {
            idle_workers: 2,
            ..Default::default()
        },
        throttle_control,
    };
    let daemon = Daemon::new(config);

    let run_token = token.clone();
    let shutdown_trigger = token.clone();
    let db_path_clone = db_path.clone();

    let join_handle = std::thread::spawn(move || {
        let mut worker_db = vidcull_db::open_file(&db_path_clone).expect("open daemon db");
        let mut handler = handler;
        let stats = daemon
            .run_throttled(&mut worker_db, &mut handler, &run_token, now, || {
                vidcull_daemon::Activity::Idle
            })
            .expect("run_throttled");
        (stats, handler)
    });

    let verify = vidcull_db::open_file(&db_path).expect("reopen verification db");
    let queue = TaskQueueRepo::new(verify.conn());
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(pending) = queue.count_by_state(TaskState::Pending) {
            if pending == 0 {
                break;
            }
        }
    }
    shutdown_trigger.trigger();

    let (stats, handler) = join_handle.join().expect("join");
    assert_eq!(stats.processed, 2);
    assert_eq!(
        handler.rebuild_count(),
        1,
        "trailing matching reconstruction must be run exactly once"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn capped_fallback_pass_queues_and_drains_a_densify_revisit() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP capped_fallback_pass_queues_and_drains_a_densify_revisit: no ffmpeg");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");

    let clip = dir.join("mpeg2_clip.mp4");
    let status = std::process::Command::new(bins.ffmpeg())
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("mandelbrot=size=320x180:rate=30")
        .args(["-t", "6", "-c:v", "mpeg2video", "-b:v", "2M", "-bitexact"])
        .arg(&clip)
        .status()
        .expect("spawn ffmpeg render");
    assert!(status.success(), "mpeg2 render failed");
    let copy = dir.join("mpeg2_copy.mp4");
    std::fs::copy(&clip, &copy).expect("copy clip");

    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let changes: Vec<ChangeTask> = [&clip, &copy]
            .iter()
            .map(|p| ChangeTask {
                path: NormalizedPath::new(p),
                change: ChangeKind::Upsert,
                size_bytes: 0,
            })
            .collect();
        enqueue_changes(&mut db, &changes, "scan", 0, NOW).expect("enqueue");
    }

    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins, now).with_fallback_budget(2);
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());

    daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step clip")
        .expect("clip task processed");
    let verify = vidcull_db::open_file(&db_path).expect("reopen db");
    let pending = TaskQueueRepo::new(verify.conn())
        .list_by_state(TaskState::Pending)
        .expect("pending");
    let densify: Vec<_> = pending
        .iter()
        .filter(|t| t.priority == vidcull_daemon::DENSIFY_PRIORITY)
        .collect();
    assert_eq!(densify.len(), 1, "one densify revisit queued: {pending:?}");
    let revisit = ChangeTask::from_payload(densify[0].payload.as_deref().expect("payload"))
        .expect("decode payload");
    assert_eq!(revisit.change, ChangeKind::Densify);
    assert_eq!(revisit.path, NormalizedPath::new(&clip));

    daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step copy")
        .expect("copy task processed");
    let metrics = handler.fallback_metrics();
    assert_eq!(metrics.fallback_count(), 1, "one genuine fallback decode");
    assert_eq!(metrics.native_count(), 0, "mpeg2 never decodes natively");

    let tier2_timestamps = |conn: &Database, path: &PathBuf| -> Vec<u64> {
        let files = FilesRepo::new(conn.conn());
        let row = files
            .find_by_path(&NormalizedPath::new(path))
            .expect("find")
            .expect("row");
        let fp = FingerprintsRepo::new(conn.conn())
            .get(row.id)
            .expect("get fp")
            .expect("fp");
        vidcull_fingerprint::format::decode_tier2(fp.tier2_temporal.as_deref().expect("tier2"))
            .expect("decode tier2")
            .scenes
            .iter()
            .map(|s| s.timestamp_ms)
            .collect()
    };
    let capped = tier2_timestamps(&verify, &clip);
    assert!(
        capped.contains(&5000),
        "strided first pass reaches the clip tail: {capped:?}"
    );
    assert!(
        !capped.contains(&2500),
        "capped pass cannot contain a grid point it never decoded: {capped:?}"
    );
    assert_eq!(
        capped,
        tier2_timestamps(&verify, &copy),
        "twin copy inherited the capped fingerprint verbatim"
    );

    daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step densify")
        .expect("densify task processed");
    assert!(
        daemon
            .step(&worker_db, &mut handler, NOW)
            .expect("step empty")
            .is_none(),
        "queue fully drained after the revisit"
    );

    let dense = tier2_timestamps(&verify, &clip);
    assert!(
        dense.contains(&2500),
        "densify restored the dropped grid point: {dense:?}"
    );
    assert!(
        dense.len() >= capped.len(),
        "densify never loses scenes: {} -> {}",
        capped.len(),
        dense.len()
    );
    assert_eq!(
        dense,
        tier2_timestamps(&verify, &copy),
        "densify fanned the dense fingerprint out to the twin copy"
    );
    assert_eq!(
        handler.fallback_metrics().fallback_count(),
        1,
        "the densify re-decode is not recorded in the entry-rate snapshot"
    );

    assert!(
        RegroupQueueRepo::new(verify.conn())
            .is_empty()
            .expect("regroup_queue"),
        "regroup delta consumed after the densify burst"
    );
    let groups = DuplicateGroupsRepo::new(verify.conn());
    let exact: Vec<_> = groups
        .list_all()
        .expect("list groups")
        .into_iter()
        .filter(|g| g.trust_level == TrustLevel::Exact)
        .collect();
    assert_eq!(exact.len(), 1, "one EXACT group, got {exact:?}");
    assert_eq!(
        groups.list_members(exact[0].id).expect("members").len(),
        2,
        "both copies grouped"
    );
}

fn seed_av1_partial_fixture(
    db_path: &Path,
    file_path: &Path,
    size_bytes: i64,
    mtime_ns: i64,
) -> (NormalizedPath, vidcull_core::types::FileId) {
    std::fs::write(file_path, b"stub bytes").expect("write stub av1 file");
    let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());
    let db = vidcull_db::open_file(db_path).expect("open db");
    let id = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: norm.clone(),
            size_bytes,
            mtime_ns,
            codec: Some(Codec::Av1),
            ..Default::default()
        })
        .expect("insert av1 row");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: vec![1, 2, 3],
            tier2_temporal: None,
            format_version: 1,
            created_at: NOW,
        })
        .expect("seed fingerprint");
    (norm, id)
}

fn drain_one_av1_partial(db_path: &Path, norm: &NormalizedPath, bins: &FfmpegBinaries) -> u64 {
    {
        let mut db = vidcull_db::open_file(db_path).expect("open db");
        enqueue_changes(
            &mut db,
            &[ChangeTask {
                path: norm.clone(),
                change: ChangeKind::PartialFingerprint,
                size_bytes: 0,
            }],
            "scan",
            -200,
            NOW,
        )
        .expect("enqueue partial");
    }
    let handler_db = vidcull_db::open_file(db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins.clone(), now).with_partial_clips(true);
    let worker_db = vidcull_db::open_file(db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .expect("partial task processed");
    handler.fallback_metrics().fallback_count() + handler.fallback_metrics().native_count()
}

fn bins_or_placeholder() -> FfmpegBinaries {
    FfmpegBinaries::resolve()
        .unwrap_or_else(|_| FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into()))
}

#[test]
fn av1_partial_task_done_skips_at_consumption_without_decode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("av1.db");
    let (norm, file_id) =
        seed_av1_partial_fixture(&db_path, &dir.join("big_av1.mp4"), 4_000_000_000, 7);
    let bins = bins_or_placeholder();

    assert_eq!(
        drain_one_av1_partial(&db_path, &norm, &bins),
        0,
        "AV1 partial must skip before any decode",
    );
    {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        let marker = FingerprintsRepo::new(verify.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker stamped on first pass");
        assert_eq!(marker.size_bytes, 4_000_000_000);
        let pending_partial = TaskQueueRepo::new(verify.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending")
            .iter()
            .filter(|t| {
                t.payload
                    .as_deref()
                    .and_then(|p| ChangeTask::from_payload(p).ok())
                    .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
            })
            .count();
        assert_eq!(
            pending_partial, 0,
            "the partial task reached DONE, not stuck PENDING",
        );
    }

    assert_eq!(
        drain_one_av1_partial(&db_path, &norm, &bins),
        0,
        "a restart re-queue still skips at consumption — zero decodes (no retry storm)",
    );
}

#[test]
fn av1_replacement_restamps_skip_marker_without_decode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("av1.db");
    let (norm, file_id) =
        seed_av1_partial_fixture(&db_path, &dir.join("big_av1.mp4"), 4_000_000_000, 7);
    let bins = bins_or_placeholder();

    assert_eq!(drain_one_av1_partial(&db_path, &norm, &bins), 0);

    {
        let db = vidcull_db::open_file(&db_path).expect("open db");
        FilesRepo::new(db.conn())
            .update_metadata(
                file_id,
                &NewFile {
                    path: norm.clone(),
                    size_bytes: 60_000,
                    mtime_ns: 4242,
                    codec: Some(Codec::Av1),
                    ..Default::default()
                },
            )
            .expect("replace file (still av1)");
    }

    assert_eq!(
        drain_one_av1_partial(&db_path, &norm, &bins),
        0,
        "still-AV1 replacement re-skips, zero decodes",
    );
    let refreshed = {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        FingerprintsRepo::new(verify.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker present after replace")
    };
    assert_eq!(
        refreshed.size_bytes, 60_000,
        "stale marker re-stamped at the replacement identity",
    );
    assert_eq!(refreshed.mtime_ns, 4242);
}

fn seed_h264_partial_fixture(
    db_path: &Path,
    file_path: &Path,
    size_bytes: i64,
    mtime_ns: i64,
) -> (NormalizedPath, vidcull_core::types::FileId) {
    std::fs::write(file_path, b"not valid h264 content").expect("write stub h264 file");
    let norm = NormalizedPath::new(file_path.to_string_lossy().as_ref());
    let db = vidcull_db::open_file(db_path).expect("open db");
    let id = FilesRepo::new(db.conn())
        .insert(&NewFile {
            path: norm.clone(),
            size_bytes,
            mtime_ns,
            codec: Some(Codec::H264),
            ..Default::default()
        })
        .expect("insert h264 row");
    FingerprintsRepo::new(db.conn())
        .upsert(&Fingerprint {
            file_id: id,
            tier1_global: vec![1, 2, 3],
            tier2_temporal: None,
            format_version: 1,
            created_at: NOW,
        })
        .expect("seed fingerprint");
    (norm, id)
}

fn drain_one_h264_partial(db_path: &Path, norm: &NormalizedPath, bins: &FfmpegBinaries) {
    {
        let mut db = vidcull_db::open_file(db_path).expect("open db");
        enqueue_changes(
            &mut db,
            &[ChangeTask {
                path: norm.clone(),
                change: ChangeKind::PartialFingerprint,
                size_bytes: 0,
            }],
            "scan",
            -200,
            NOW,
        )
        .expect("enqueue partial");
    }
    let handler_db = vidcull_db::open_file(db_path).expect("open handler db");
    let mut handler = IndexingHandler::new(handler_db, bins.clone(), now).with_partial_clips(true);
    let worker_db = vidcull_db::open_file(db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .expect("partial task processed");
}

#[test]
fn h264_partial_content_fail_stamps_skip_marker_and_self_heals() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!(
            "SKIP h264_partial_content_fail_stamps_skip_marker_and_self_heals: ffmpeg not found"
        );
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("h264.db");
    let (norm, file_id) = seed_h264_partial_fixture(&db_path, &dir.join("clip.mp4"), 22, 1234);

    drain_one_h264_partial(&db_path, &norm, &bins);

    {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        let marker = FingerprintsRepo::new(verify.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("decode-failed skip marker must be stamped on content error");
        assert_eq!(
            marker.reason, "decode-failed",
            "skip marker reason must be 'decode-failed'",
        );
        assert_eq!(marker.size_bytes, 22);
        assert_eq!(marker.mtime_ns, 1234);
        let pending_partial = TaskQueueRepo::new(verify.conn())
            .list_by_state(TaskState::Pending)
            .expect("pending")
            .iter()
            .filter(|t| {
                t.payload
                    .as_deref()
                    .and_then(|p| ChangeTask::from_payload(p).ok())
                    .is_some_and(|c| c.change == ChangeKind::PartialFingerprint)
            })
            .count();
        assert_eq!(
            pending_partial, 0,
            "the partial task reached DONE, not stuck PENDING",
        );
    }

    drain_one_h264_partial(&db_path, &norm, &bins);
    {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        let marker = FingerprintsRepo::new(verify.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker must still be present after second pass");
        assert_eq!(
            marker.reason, "decode-failed",
            "marker reason unchanged after restart DONE-skip",
        );
    }
}

#[test]
fn h264_partial_decode_fail_skip_self_heals_on_replace() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!("SKIP h264_partial_decode_fail_skip_self_heals_on_replace: ffmpeg not found");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("h264_heal.db");
    let (norm, file_id) = seed_h264_partial_fixture(&db_path, &dir.join("clip.mp4"), 22, 1234);

    drain_one_h264_partial(&db_path, &norm, &bins);
    {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        FingerprintsRepo::new(verify.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("initial decode-failed marker must be stamped");
    }

    {
        let db = vidcull_db::open_file(&db_path).expect("open db");
        FilesRepo::new(db.conn())
            .update_metadata(
                file_id,
                &NewFile {
                    path: norm.clone(),
                    size_bytes: 99_000,
                    mtime_ns: 9_999_999,
                    codec: Some(Codec::H264),
                    ..Default::default()
                },
            )
            .expect("update file identity (simulate replace)");
    }

    drain_one_h264_partial(&db_path, &norm, &bins);
    {
        let verify = vidcull_db::open_file(&db_path).expect("reopen db");
        let refreshed = FingerprintsRepo::new(verify.conn())
            .get_partial_skip(file_id)
            .expect("get marker")
            .expect("marker must be re-stamped at the replacement identity");
        assert_eq!(
            refreshed.size_bytes, 99_000,
            "re-stamped marker carries the replacement size",
        );
        assert_eq!(
            refreshed.mtime_ns, 9_999_999,
            "re-stamped marker carries the replacement mtime",
        );
        assert_eq!(refreshed.reason, "decode-failed");
    }
}
