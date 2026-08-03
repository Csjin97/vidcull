use vidcull_core::Error;
use vidcull_core::types::{
    Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath, Resolution, VideoDuration,
};
use vidcull_db::repo::{
    BatchFileRole, DaemonSettingsRepo, DeleteBatchFile, DeleteBatchMode, DeleteJournalRepo,
    DuplicateGroup, DuplicateGroupsRepo, FileRecord, FilesRepo, Fingerprint, FingerprintsRepo,
    NewDeleteBatch, NewFile, NewTask, PartialEdgeSpan, PartialSkipMarker, ScanStateEntry,
    ScanStateRepo, SceneHash, SceneHashesRepo, SimilarityEdge, SimilarityEdgesRepo,
    SystemMetadataRepo, Task, TaskQueueRepo, TaskState, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};

fn fresh_db() -> Database {
    open_in_memory().expect("open in-memory db")
}

fn now() -> i64 {
    1_700_000_000
}

fn sample_file(path: &str) -> NewFile {
    NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 12_345,
        mtime_ns: 1_700_000_000_000_000_000,
        inode: Some(42),
        content_hash: None,
        codec: Some(Codec::H264),
        container: Some("mp4".into()),
        duration: Some(VideoDuration::from_millis(60_000)),
        fps_x1000: Some(30_000),
        bitrate_bps: Some(5_000_000),
        resolution: Some(Resolution::new(1920, 1080)),
        first_seen_at: now(),
        last_seen_at: now(),
        laplacian_variance: None,
        dct_energy: None,
        bpp: None,
        encoder_tags: None,
    }
}

fn seed_file(db: &Database, path: &str) -> FileId {
    FilesRepo::new(db.conn())
        .insert(&sample_file(path))
        .expect("seed file")
}

fn assert_equivalent_to_new(record: &FileRecord, expected: &NewFile) {
    assert_eq!(&record.path, &expected.path);
    assert_eq!(record.size_bytes, expected.size_bytes);
    assert_eq!(record.mtime_ns, expected.mtime_ns);
    assert_eq!(record.inode, expected.inode);
    assert_eq!(record.content_hash, expected.content_hash);
    assert_eq!(record.codec, expected.codec);
    assert_eq!(record.container, expected.container);
    assert_eq!(record.duration, expected.duration);
    assert_eq!(record.fps_x1000, expected.fps_x1000);
    assert_eq!(record.bitrate_bps, expected.bitrate_bps);
    assert_eq!(record.resolution, expected.resolution);
    assert_eq!(record.first_seen_at, expected.first_seen_at);
    assert_eq!(record.last_seen_at, expected.last_seen_at);
}

mod files {
    use super::*;

    #[test]
    fn insert_assigns_id_and_round_trips_every_field() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let new = sample_file("/library/clip.mp4");

        let id = repo.insert(&new).expect("insert");
        assert_ne!(id, FileId::UNASSIGNED, "INSERT must allocate a real id");

        let read = repo.get(id).expect("get").expect("row must exist");
        assert_eq!(read.id, id);
        assert_eq!(read.deleted_at, None);
        assert_equivalent_to_new(&read, &new);
    }

    #[test]
    fn find_by_path_locates_the_record() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let id = repo
            .insert(&sample_file("/library/by-path.mkv"))
            .expect("insert");
        let found = repo
            .find_by_path(&NormalizedPath::new("/library/by-path.mkv"))
            .expect("find")
            .expect("present");
        assert_eq!(found.id, id);
    }

    #[test]
    fn list_active_by_hash_returns_all_live_copies_and_skips_tombstones() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let hash = Blake3Hash::from_bytes([0x42; HASH_LEN]);

        let mut a = sample_file("/library/copy_a.mp4");
        a.content_hash = Some(hash);
        let id_a = repo.insert(&a).expect("insert a");
        let mut b = sample_file("/library/copy_b.mp4");
        b.content_hash = Some(hash);
        let id_b = repo.insert(&b).expect("insert b");
        let mut gone = sample_file("/library/copy_gone.mp4");
        gone.content_hash = Some(hash);
        let id_gone = repo.insert(&gone).expect("insert gone");
        repo.mark_deleted(id_gone, now()).expect("tombstone");
        let mut other = sample_file("/library/other.mp4");
        other.content_hash = Some(Blake3Hash::from_bytes([0x43; HASH_LEN]));
        repo.insert(&other).expect("insert other");

        let rows = repo
            .list_active_by_hash(&hash)
            .expect("list_active_by_hash");
        let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![id_a, id_b], "live copies only, id order");
    }

    #[test]
    fn update_quality_stats_touches_only_the_quality_columns() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let id = repo
            .insert(&sample_file("/library/quality.mp4"))
            .expect("insert");
        repo.mark_deleted(id, now()).expect("tombstone");

        repo.update_quality_stats(id, Some(123.5), Some(45.25), Some(0.125))
            .expect("update_quality_stats");

        let read = repo.get(id).expect("get").expect("present");
        assert_eq!(read.laplacian_variance, Some(123.5));
        assert_eq!(read.dct_energy, Some(45.25));
        assert_eq!(read.bpp, Some(0.125));
        assert!(read.deleted_at.is_some(), "tombstone preserved");
    }

    #[test]
    fn find_by_path_returns_none_when_absent() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let missing = repo
            .find_by_path(&NormalizedPath::new("/nope.mp4"))
            .expect("find");
        assert!(missing.is_none());
    }

    #[test]
    fn unique_path_constraint_rejects_duplicate_insert() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let file = sample_file("/library/dup.mp4");
        repo.insert(&file).expect("first insert");
        let err = repo.insert(&file).expect_err("second insert must fail");
        assert!(
            matches!(err, Error::Database(_)),
            "UNIQUE violation should surface as Error::Database, got {err:?}"
        );
    }

    #[test]
    fn update_metadata_persists_every_changed_field() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let mut file = sample_file("/library/changing.mp4");
        let id = repo.insert(&file).expect("insert");

        file.size_bytes = 99_999;
        file.last_seen_at = now() + 100;
        file.content_hash = Some(Blake3Hash::from_bytes([0x7Fu8; HASH_LEN]));
        file.codec = Some(Codec::H265);
        file.container = Some("mkv".into());
        file.duration = Some(VideoDuration::from_millis(180_000));
        file.resolution = Some(Resolution::new(3840, 2160));
        file.inode = None;
        file.bitrate_bps = None;
        file.fps_x1000 = None;
        repo.update_metadata(id, &file).expect("update_metadata");

        let read = repo.get(id).expect("get").expect("present");
        assert_equivalent_to_new(&read, &file);
    }

    #[test]
    fn mark_deleted_sets_tombstone_and_list_active_skips_it() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let a = repo
            .insert(&sample_file("/library/a.mp4"))
            .expect("insert a");
        let _b = repo
            .insert(&sample_file("/library/b.mp4"))
            .expect("insert b");

        repo.mark_deleted(a, now() + 50).expect("mark_deleted");

        let active_paths: Vec<NormalizedPath> = repo
            .list_active()
            .expect("list_active")
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(active_paths, vec![NormalizedPath::new("/library/b.mp4")]);

        let still_there = repo.get(a).expect("get").expect("soft-deleted row remains");
        assert_eq!(still_there.deleted_at, Some(now() + 50));
    }

    #[test]
    fn delete_removes_row_completely() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let id = repo
            .insert(&sample_file("/library/gone.mp4"))
            .expect("insert");
        repo.delete(id).expect("delete");
        assert!(repo.get(id).expect("get").is_none());
    }

    #[test]
    fn codec_other_variant_round_trips() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let mut file = sample_file("/library/odd-codec.mp4");
        file.codec = Some(Codec::Other("prores".into()));
        let id = repo.insert(&file).expect("insert");
        let read = repo.get(id).expect("get").expect("present");
        assert_eq!(read.codec, Some(Codec::Other("prores".into())));
    }

    #[test]
    fn list_hashed_active_returns_only_hashed_live_files() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        let mut f1 = sample_file("/library/hashed_live.mp4");
        f1.content_hash = Some(Blake3Hash::from_bytes([0x11; HASH_LEN]));
        let id1 = repo.insert(&f1).expect("insert hashed live");

        let mut f2 = sample_file("/library/unhashed_live.mp4");
        f2.content_hash = None;
        let _id2 = repo.insert(&f2).expect("insert unhashed live");

        let mut f3 = sample_file("/library/hashed_deleted.mp4");
        f3.content_hash = Some(Blake3Hash::from_bytes([0x22; HASH_LEN]));
        let id3 = repo.insert(&f3).expect("insert hashed deleted");
        repo.mark_deleted(id3, now()).expect("soft delete");

        let hashed_active = repo.list_hashed_active().expect("list_hashed_active");

        assert_eq!(hashed_active.len(), 1);
        assert_eq!(hashed_active[0].0, id1);
        assert_eq!(hashed_active[0].1.as_bytes(), &[0x11; HASH_LEN]);
    }

    #[test]
    fn list_hashed_active_returns_empty_when_no_hashes() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        let mut f1 = sample_file("/library/unhashed.mp4");
        f1.content_hash = None;
        repo.insert(&f1).expect("insert unhashed");

        let hashed_active = repo.list_hashed_active().expect("list_hashed_active");
        assert!(hashed_active.is_empty());
    }

    #[test]
    fn set_content_hash_writes_hash_and_preserves_other_fields() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let new_file = sample_file("/library/pre_hash.mp4");
        let id = repo.insert(&new_file).expect("insert");

        let hash = Blake3Hash::from_bytes([0x77; HASH_LEN]);
        repo.set_content_hash(id, hash).expect("set_content_hash");

        let read = repo.get(id).expect("get").expect("present");
        assert_eq!(read.content_hash, Some(hash));

        assert_eq!(read.path, new_file.path);
        assert_eq!(read.size_bytes, new_file.size_bytes);
        assert_eq!(read.inode, new_file.inode);
        assert_eq!(read.container, new_file.container);
    }

    #[test]
    fn set_content_hash_on_nonexistent_id_is_silent_noop() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let hash = Blake3Hash::from_bytes([0x88; HASH_LEN]);
        assert!(repo.set_content_hash(FileId(999_999), hash).is_ok());
    }

    #[test]
    fn count_active_indexed_counts_only_live_hashed_files() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        let mut hashed_live = sample_file("/library/hashed_live.mp4");
        hashed_live.content_hash = Some(Blake3Hash::from_bytes([0x11; HASH_LEN]));
        repo.insert(&hashed_live).expect("insert hashed live");

        let mut unhashed = sample_file("/library/unhashed.mp4");
        unhashed.content_hash = None;
        repo.insert(&unhashed).expect("insert unhashed");

        let mut hashed_deleted = sample_file("/library/hidden.mp4");
        hashed_deleted.content_hash = Some(Blake3Hash::from_bytes([0x22; HASH_LEN]));
        let hidden_id = repo.insert(&hashed_deleted).expect("insert hashed deleted");
        repo.mark_deleted(hidden_id, now()).expect("soft delete");

        assert_eq!(
            repo.count_active_indexed().expect("count"),
            1,
            "only the live, hashed file counts toward 완료",
        );
        let hidden = repo.get(hidden_id).expect("get").expect("row remains");
        assert_eq!(
            hidden.content_hash,
            Some(Blake3Hash::from_bytes([0x22; HASH_LEN])),
            "a hidden file still stores its hash",
        );
    }

    #[test]
    fn list_active_paths_under_root_matches_subtree_only() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        repo.insert(&sample_file("/lib/a.mp4")).expect("a");
        repo.insert(&sample_file("/lib/sub/b.mp4")).expect("b");
        repo.insert(&sample_file("/library/c.mp4")).expect("c");
        let gone = repo.insert(&sample_file("/lib/gone.mp4")).expect("gone");
        repo.mark_deleted(gone, now()).expect("soft delete");

        let mut under = repo
            .list_active_paths_under_root(&NormalizedPath::new("/lib"))
            .expect("list under root")
            .into_iter()
            .map(|p| p.as_str().to_owned())
            .collect::<Vec<_>>();
        under.sort();
        assert_eq!(
            under,
            vec!["/lib/a.mp4".to_owned(), "/lib/sub/b.mp4".to_owned()],
            "only live files under /lib (not the /library sibling, not the tombstone)",
        );

        let with_slash = repo
            .list_active_paths_under_root(&NormalizedPath::new("/lib/"))
            .expect("list with trailing slash");
        assert_eq!(with_slash.len(), 2, "trailing slash matches the same files");
    }

    #[test]
    fn list_active_under_root_returns_full_records_for_subtree() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        repo.insert(&sample_file("/lib")).expect("root-exact");
        let mut fa = sample_file("/lib/a.mp4");
        fa.size_bytes = 4242;
        repo.insert(&fa).expect("a");
        repo.insert(&sample_file("/lib/sub/b.mp4")).expect("b");
        repo.insert(&sample_file("/library/c.mp4")).expect("c");
        let gone = repo.insert(&sample_file("/lib/gone.mp4")).expect("gone");
        repo.mark_deleted(gone, now()).expect("soft delete");

        let records = repo
            .list_active_under_root(&NormalizedPath::new("/lib"))
            .expect("list under root");
        let mut paths = records
            .iter()
            .map(|r| r.path.as_str().to_owned())
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "/lib".to_owned(),
                "/lib/a.mp4".to_owned(),
                "/lib/sub/b.mp4".to_owned(),
            ],
            "root-exact + live subtree only (not the /library sibling, not the tombstone)",
        );

        let a = records
            .iter()
            .find(|r| r.path.as_str() == "/lib/a.mp4")
            .expect("a present");
        assert_eq!(
            a.size_bytes, 4242,
            "record carries size for the diff fingerprint"
        );

        let with_slash = repo
            .list_active_under_root(&NormalizedPath::new("/lib/"))
            .expect("trailing slash");
        assert_eq!(
            with_slash.len(),
            3,
            "trailing slash matches the same root-exact + subtree set",
        );
    }

    #[test]
    fn active_hashed_paths_in_returns_only_active_hashed() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        let mut ah = sample_file("/lib/active_hashed.mp4");
        ah.content_hash = Some(Blake3Hash::from_bytes([0x33; HASH_LEN]));
        repo.insert(&ah).expect("ah");

        let mut au = sample_file("/lib/active_unhashed.mp4");
        au.content_hash = None;
        repo.insert(&au).expect("au");

        let mut dh = sample_file("/lib/deleted_hashed.mp4");
        dh.content_hash = Some(Blake3Hash::from_bytes([0x44; HASH_LEN]));
        let dh_id = repo.insert(&dh).expect("dh");
        repo.mark_deleted(dh_id, now()).expect("soft delete");

        let query = [
            NormalizedPath::new("/lib/active_hashed.mp4"),
            NormalizedPath::new("/lib/active_unhashed.mp4"),
            NormalizedPath::new("/lib/deleted_hashed.mp4"),
            NormalizedPath::new("/lib/never_inserted.mp4"),
        ];
        let got = repo.active_hashed_paths_in(&query).expect("batch lookup");
        assert_eq!(got.len(), 1, "only the live, hashed file");
        assert!(got.contains("/lib/active_hashed.mp4"));

        assert!(repo.active_hashed_paths_in(&[]).expect("empty").is_empty());
    }

    #[test]
    fn update_metadata_clears_soft_delete_tombstone() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let mut file = sample_file("/library/reappears.mp4");
        let id = repo.insert(&file).expect("insert");
        repo.mark_deleted(id, now() + 10).expect("soft delete");
        assert!(
            repo.get(id)
                .expect("get")
                .expect("row")
                .deleted_at
                .is_some(),
            "precondition: the row is soft-deleted",
        );

        file.last_seen_at = now() + 20;
        repo.update_metadata(id, &file).expect("update_metadata");

        let read = repo.get(id).expect("get").expect("row");
        assert_eq!(read.deleted_at, None, "re-indexing un-hides the file");
    }

    #[test]
    fn update_metadata_clears_skip_marker_when_recodec_is_fast_path() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let mut file = sample_file("/library/recodec.mp4");
        file.codec = Some(Codec::Av1);
        let id = repo.insert(&file).expect("insert");

        let fps = FingerprintsRepo::new(db.conn());
        fps.upsert(&Fingerprint {
            file_id: id,
            tier1_global: vec![1, 2, 3],
            tier2_temporal: None,
            format_version: 1,
            created_at: now(),
        })
        .expect("upsert fp");
        fps.set_partial_skip(
            id,
            &PartialSkipMarker {
                reason: "unsupported-codec".into(),
                size_bytes: file.size_bytes,
                mtime_ns: file.mtime_ns,
            },
        )
        .expect("set marker");

        file.codec = Some(Codec::H264);
        repo.update_metadata(id, &file).expect("update_metadata");

        assert!(
            fps.get_partial_skip(id).expect("get marker").is_none(),
            "a fast-path re-codec must clear the skip marker",
        );
    }

    #[test]
    fn update_metadata_keeps_skip_marker_when_recodec_is_still_fallback() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let mut file = sample_file("/library/still_av1.mp4");
        file.codec = Some(Codec::Av1);
        let id = repo.insert(&file).expect("insert");

        let fps = FingerprintsRepo::new(db.conn());
        fps.upsert(&Fingerprint {
            file_id: id,
            tier1_global: vec![9],
            tier2_temporal: None,
            format_version: 1,
            created_at: now(),
        })
        .expect("upsert fp");
        fps.set_partial_skip(
            id,
            &PartialSkipMarker {
                reason: "unsupported-codec".into(),
                size_bytes: file.size_bytes,
                mtime_ns: file.mtime_ns,
            },
        )
        .expect("set marker");

        file.size_bytes += 1;
        repo.update_metadata(id, &file)
            .expect("update_metadata still-av1");
        assert!(
            fps.get_partial_skip(id).expect("get marker").is_some(),
            "a still-fallback re-observation keeps the marker",
        );

        file.codec = None;
        repo.update_metadata(id, &file)
            .expect("update_metadata null-codec");
        assert!(
            fps.get_partial_skip(id).expect("get marker").is_some(),
            "an unknown (NULL) codec keeps the marker",
        );
    }

    #[test]
    fn update_metadata_clears_retry_exhausted_marker_on_fast_path_replace() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());
        let mut file = sample_file("/library/flaky_then_replaced.mp4");
        file.codec = Some(Codec::H264);
        let id = repo.insert(&file).expect("insert");

        let fps = FingerprintsRepo::new(db.conn());
        fps.upsert(&Fingerprint {
            file_id: id,
            tier1_global: vec![4, 5, 6],
            tier2_temporal: None,
            format_version: 1,
            created_at: now(),
        })
        .expect("upsert fp");
        fps.set_partial_skip(
            id,
            &PartialSkipMarker {
                reason: "retry-exhausted".into(),
                size_bytes: file.size_bytes,
                mtime_ns: file.mtime_ns,
            },
        )
        .expect("set marker");

        file.size_bytes += 12_345;
        file.mtime_ns += 1;
        repo.update_metadata(id, &file)
            .expect("update_metadata replace");

        assert!(
            fps.get_partial_skip(id).expect("get marker").is_none(),
            "a fast-path replacement must clear a stale retry-exhausted marker \
             so the new content gets a fresh partial-analysis opportunity",
        );
    }

    #[test]
    fn sum_active_size_bytes_calculates_correctly() {
        let db = fresh_db();
        let repo = FilesRepo::new(db.conn());

        assert_eq!(repo.sum_active_size_bytes().expect("sum empty"), 0);

        let mut f1 = sample_file("/library/f1.mp4");
        f1.size_bytes = 1000;
        f1.content_hash = None;
        let id1 = repo.insert(&f1).expect("insert f1");
        assert_eq!(repo.sum_active_size_bytes().expect("sum unindexed"), 0);

        repo.set_content_hash(id1, Blake3Hash::from_bytes([0x11; HASH_LEN]))
            .expect("set hash");
        assert_eq!(repo.sum_active_size_bytes().expect("sum indexed"), 1000);

        let mut f2 = sample_file("/library/f2.mp4");
        f2.size_bytes = 2000;
        f2.content_hash = Some(Blake3Hash::from_bytes([0x22; HASH_LEN]));
        let _id2 = repo.insert(&f2).expect("insert f2");
        assert_eq!(repo.sum_active_size_bytes().expect("sum multiple"), 3000);

        repo.mark_deleted(id1, now()).expect("soft delete");
        assert_eq!(
            repo.sum_active_size_bytes().expect("sum after delete"),
            2000
        );
    }
}

mod fingerprints {
    use super::*;

    fn fp(file_id: FileId) -> Fingerprint {
        Fingerprint {
            file_id,
            tier1_global: (0u8..64).collect(),
            tier2_temporal: Some((0u8..200).rev().collect()),
            format_version: 1,
            created_at: now(),
        }
    }

    #[test]
    fn upsert_round_trips_binary_blob() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/one.mp4");
        let repo = FingerprintsRepo::new(db.conn());

        let original = fp(file_id);
        repo.upsert(&original).expect("upsert");
        let read = repo.get(file_id).expect("get").expect("present");
        assert_eq!(read, original);
        assert_eq!(
            read.tier1_global.as_slice(),
            original.tier1_global.as_slice()
        );
    }

    #[test]
    fn upsert_overwrites_existing_row() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/two.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(file_id)).expect("first upsert");

        let replacement = Fingerprint {
            file_id,
            tier1_global: vec![0xAA; 128],
            tier2_temporal: None,
            format_version: 2,
            created_at: now() + 1,
        };
        repo.upsert(&replacement).expect("second upsert");
        let read = repo.get(file_id).expect("get").expect("present");
        assert_eq!(read, replacement);
    }

    #[test]
    fn get_returns_none_when_absent() {
        let db = fresh_db();
        let repo = FingerprintsRepo::new(db.conn());
        assert!(repo.get(FileId(9999)).expect("get").is_none());
    }

    #[test]
    fn deleting_file_cascades_to_fingerprint() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/cascade.mp4");
        let fp_repo = FingerprintsRepo::new(db.conn());
        fp_repo.upsert(&fp(file_id)).expect("upsert");

        FilesRepo::new(db.conn())
            .delete(file_id)
            .expect("hard delete parent file");

        assert!(
            fp_repo.get(file_id).expect("get").is_none(),
            "ON DELETE CASCADE must remove dependent fingerprint",
        );
    }

    #[test]
    fn list_active_tier1_projects_only_live_files_in_id_order() {
        let db = fresh_db();
        let f1 = seed_file(&db, "/fp/a.mp4");
        let f2 = seed_file(&db, "/fp/b.mp4");
        let f3 = seed_file(&db, "/fp/c.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(f1)).expect("upsert f1");
        repo.upsert(&fp(f2)).expect("upsert f2");
        repo.upsert(&fp(f3)).expect("upsert f3");

        FilesRepo::new(db.conn())
            .mark_deleted(f2, now() + 1)
            .expect("soft delete f2");

        let rows = repo.list_active_tier1().expect("list_active_tier1");
        assert_eq!(
            rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![f1, f3],
            "only live files, ordered by id",
        );
        assert_eq!(rows[0].1, fp(f1).tier1_global);
    }

    #[test]
    fn list_active_tier2_skips_null_temporal_and_soft_deleted() {
        let db = fresh_db();
        let f1 = seed_file(&db, "/fp/a.mp4");
        let f2 = seed_file(&db, "/fp/b.mp4");
        let f3 = seed_file(&db, "/fp/c.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(f1)).expect("upsert f1");
        repo.upsert(&Fingerprint {
            file_id: f2,
            tier1_global: (0u8..64).collect(),
            tier2_temporal: None,
            format_version: 1,
            created_at: now(),
        })
        .expect("upsert f2");
        repo.upsert(&fp(f3)).expect("upsert f3");

        let rows = repo.list_active_tier2().expect("list_active_tier2");
        assert_eq!(
            rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![f1, f3],
            "only live files with a non-null tier2_temporal, ordered by id",
        );
        assert_eq!(
            rows[0].1,
            fp(f1).tier2_temporal.expect("f1 has tier2"),
            "projected blob is the stored tier2_temporal verbatim",
        );

        FilesRepo::new(db.conn())
            .mark_deleted(f1, now() + 1)
            .expect("soft delete f1");
        let rows = repo.list_active_tier2().expect("list_active_tier2 again");
        assert_eq!(
            rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![f3],
            "soft-deleted file is excluded",
        );
    }

    #[test]
    fn partial_fingerprint_set_get_list_and_survives_reupsert() {
        let db = fresh_db();
        let f1 = seed_file(&db, "/fp/a.mp4");
        let f2 = seed_file(&db, "/fp/b.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(f1)).expect("upsert f1");
        repo.upsert(&fp(f2)).expect("upsert f2");

        assert!(repo.get_active_partial(f1).expect("get").is_none());
        assert!(repo.list_active_partial().expect("list").is_empty());

        let blob = vec![9u8, 8, 7, 6, 5];
        assert_eq!(repo.set_partial(f1, &blob).expect("set"), 1);

        repo.upsert(&fp(f1)).expect("re-upsert f1");
        assert_eq!(
            repo.get_active_partial(f1).expect("get").as_deref(),
            Some(blob.as_slice()),
            "partial_temporal preserved across a tier1/tier2 re-upsert",
        );

        let rows = repo.list_active_partial().expect("list");
        assert_eq!(rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![f1]);
        assert_eq!(rows[0].1, blob);

        FilesRepo::new(db.conn())
            .mark_deleted(f1, now() + 1)
            .expect("soft delete f1");
        assert!(repo.list_active_partial().expect("list").is_empty());

        let f3 = seed_file(&db, "/fp/c.mp4");
        assert_eq!(repo.set_partial(f3, &blob).expect("set f3"), 0);
    }

    #[test]
    fn list_active_tier2_ids_matches_list_active_tier2_without_blobs() {
        let db = fresh_db();
        let f1 = seed_file(&db, "/fp/a.mp4");
        let f2 = seed_file(&db, "/fp/b.mp4");
        let f3 = seed_file(&db, "/fp/c.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(f1)).expect("upsert f1");
        repo.upsert(&Fingerprint {
            file_id: f2,
            tier1_global: (0u8..64).collect(),
            tier2_temporal: None,
            format_version: 1,
            created_at: now(),
        })
        .expect("upsert f2");
        repo.upsert(&fp(f3)).expect("upsert f3");

        assert_eq!(
            repo.list_active_tier2_ids().expect("ids"),
            repo.list_active_tier2()
                .expect("full")
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
        );

        FilesRepo::new(db.conn())
            .mark_deleted(f1, now() + 1)
            .expect("soft delete f1");
        assert_eq!(
            repo.list_active_tier2_ids().expect("ids again"),
            vec![f3],
            "soft-deleted file is excluded from the id projection",
        );
    }

    #[test]
    fn delete_removes_fingerprint_row() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/deletable.mp4");
        let repo = FingerprintsRepo::new(db.conn());

        let fp_record = fp(file_id);
        repo.upsert(&fp_record).expect("upsert");
        assert!(repo.get(file_id).expect("get").is_some());

        repo.delete(file_id).expect("delete");
        assert!(repo.get(file_id).expect("get").is_none());
    }

    fn marker(size_bytes: i64, mtime_ns: i64) -> PartialSkipMarker {
        PartialSkipMarker {
            reason: "unsupported-codec".into(),
            size_bytes,
            mtime_ns,
        }
    }

    #[test]
    fn partial_skip_marker_set_get_round_trips() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/av1.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(file_id)).expect("upsert fp row");

        assert!(repo.get_partial_skip(file_id).expect("get").is_none());

        let m = marker(4_000_000_000, 5);
        assert_eq!(
            repo.set_partial_skip(file_id, &m).expect("set"),
            1,
            "marker UPDATE hits the one fingerprint row"
        );
        assert_eq!(
            repo.get_partial_skip(file_id)
                .expect("get")
                .expect("present"),
            m,
            "marker round-trips reason + identity verbatim"
        );
    }

    #[test]
    fn partial_skip_marker_set_is_noop_without_fingerprint_row() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/no_fp.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        assert_eq!(
            repo.set_partial_skip(file_id, &marker(100, 1))
                .expect("set"),
            0,
        );
        assert!(repo.get_partial_skip(file_id).expect("get").is_none());
    }

    #[test]
    fn partial_skip_marker_clear_resets_to_none() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/cleared.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(file_id)).expect("upsert fp row");
        repo.set_partial_skip(file_id, &marker(100, 1))
            .expect("set");
        assert!(repo.get_partial_skip(file_id).expect("get").is_some());

        repo.clear_partial_skip(file_id).expect("clear");
        assert!(
            repo.get_partial_skip(file_id).expect("get").is_none(),
            "cleared marker reads back as None",
        );
    }

    #[test]
    fn partial_skip_marker_leaves_partial_temporal_null() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/fp/marked.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(file_id)).expect("upsert fp row");
        repo.set_partial_skip(file_id, &marker(100, 1))
            .expect("set");

        assert!(
            repo.get_active_partial(file_id)
                .expect("get partial")
                .is_none(),
            "marker must not populate partial_temporal",
        );
        assert!(
            repo.list_active_partial().expect("list").is_empty(),
            "a marked-only file never enters the partial matching corpus",
        );
    }

    #[test]
    fn list_active_partial_or_skipped_unions_partial_and_marked() {
        let db = fresh_db();
        let with_partial = seed_file(&db, "/fp/has_partial.mp4");
        let marked = seed_file(&db, "/fp/skip_marked.mp4");
        let plain = seed_file(&db, "/fp/plain_active.mp4");
        let deleted_marked = seed_file(&db, "/fp/deleted.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(with_partial)).expect("upsert with_partial");
        repo.upsert(&fp(marked)).expect("upsert marked");
        repo.upsert(&fp(plain)).expect("upsert plain");
        repo.upsert(&fp(deleted_marked)).expect("upsert deleted");

        repo.set_partial(with_partial, &[1u8, 2, 3])
            .expect("set partial");
        repo.set_partial_skip(marked, &marker(100, 1))
            .expect("mark");
        repo.set_partial_skip(deleted_marked, &marker(200, 2))
            .expect("mark deleted");
        FilesRepo::new(db.conn())
            .mark_deleted(deleted_marked, now() + 1)
            .expect("soft delete");

        let ids = repo.list_active_partial_or_skipped().expect("list");
        assert_eq!(
            ids,
            vec![with_partial, marked],
            "union of partial-carrying and skip-marked active files, id ASC; the plain \
             active file and the soft-deleted marked file are excluded",
        );
    }

    #[test]
    fn list_active_partial_or_skipped_excluding_reason_treats_excluded_reason_as_missing() {
        let db = fresh_db();
        let with_partial = seed_file(&db, "/fp/has_partial.mp4");
        let excluded_reason_only = seed_file(&db, "/fp/exact_dup_only.mp4");
        let other_reason = seed_file(&db, "/fp/duration_cap.mp4");
        let both_partial_and_excluded_reason = seed_file(&db, "/fp/stale_dup_marker.mp4");
        let plain = seed_file(&db, "/fp/plain_active.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        for id in [
            with_partial,
            excluded_reason_only,
            other_reason,
            both_partial_and_excluded_reason,
            plain,
        ] {
            repo.upsert(&fp(id)).expect("upsert");
        }
        repo.set_partial(with_partial, &[1u8, 2, 3])
            .expect("set partial");
        repo.set_partial_skip(
            excluded_reason_only,
            &PartialSkipMarker {
                reason: "exact-full-dup".into(),
                size_bytes: 1,
                mtime_ns: 1,
            },
        )
        .expect("mark excluded_reason_only");
        repo.set_partial_skip(
            other_reason,
            &PartialSkipMarker {
                reason: "duration-cap".into(),
                size_bytes: 1,
                mtime_ns: 1,
            },
        )
        .expect("mark other_reason");
        repo.set_partial(both_partial_and_excluded_reason, &[9u8])
            .expect("set partial for both_partial_and_excluded_reason");
        repo.set_partial_skip(
            both_partial_and_excluded_reason,
            &PartialSkipMarker {
                reason: "exact-full-dup".into(),
                size_bytes: 1,
                mtime_ns: 1,
            },
        )
        .expect("mark both_partial_and_excluded_reason");

        let ids = repo
            .list_active_partial_or_skipped_excluding_reason("exact-full-dup")
            .expect("list excluding exact-full-dup");
        assert_eq!(
            ids,
            vec![with_partial, other_reason, both_partial_and_excluded_reason],
            "an excluded-reason-ONLY marker does not count as 'have partial'; a real \
             fingerprint or any OTHER reason still does, id ASC",
        );
    }

    #[test]
    fn set_partial_clears_any_stale_skip_marker() {
        let db = fresh_db();
        let id = seed_file(&db, "/fp/recovers.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(id)).expect("upsert");
        repo.set_partial_skip(
            id,
            &PartialSkipMarker {
                reason: "exact-full-dup".into(),
                size_bytes: 4096,
                mtime_ns: 1,
            },
        )
        .expect("mark");
        assert!(
            repo.get_partial_skip(id).expect("get").is_some(),
            "marker present before"
        );

        repo.set_partial(id, &[1u8, 2, 3]).expect("set partial");

        assert!(
            repo.get_partial_skip(id).expect("get after").is_none(),
            "a successful set_partial must clear any stale skip marker",
        );
        let stored = repo
            .get_active_partial(id)
            .expect("get partial")
            .expect("partial present");
        assert_eq!(stored, vec![1u8, 2, 3]);
        assert!(
            repo.count_partial_skip_by_reason()
                .expect("counts")
                .is_empty(),
            "the file must not be double-booked into the skip-reason tally anymore",
        );
    }

    #[test]
    fn count_partial_skip_by_reason_groups_active_markers() {
        let db = fresh_db();
        let codec_a = seed_file(&db, "/fp/av1_a.mp4");
        let codec_b = seed_file(&db, "/fp/av1_b.mp4");
        let dur = seed_file(&db, "/fp/long.mp4");
        let plain = seed_file(&db, "/fp/plain.mp4");
        let deleted = seed_file(&db, "/fp/deleted_av1.mp4");
        let repo = FingerprintsRepo::new(db.conn());
        for id in [codec_a, codec_b, dur, plain, deleted] {
            repo.upsert(&fp(id)).expect("upsert");
        }
        let reason_marker = |reason: &str| PartialSkipMarker {
            reason: reason.into(),
            size_bytes: 1,
            mtime_ns: 1,
        };
        repo.set_partial_skip(codec_a, &reason_marker("unsupported-codec"))
            .expect("mark a");
        repo.set_partial_skip(codec_b, &reason_marker("unsupported-codec"))
            .expect("mark b");
        repo.set_partial_skip(dur, &reason_marker("duration-cap"))
            .expect("mark dur");
        repo.set_partial_skip(deleted, &reason_marker("unsupported-codec"))
            .expect("mark deleted");
        FilesRepo::new(db.conn())
            .mark_deleted(deleted, now() + 1)
            .expect("soft delete");

        let counts = repo
            .count_partial_skip_by_reason()
            .expect("count by reason");
        assert_eq!(
            counts,
            vec![
                ("duration-cap".to_owned(), 1),
                ("unsupported-codec".to_owned(), 2),
            ],
            "active markers grouped by reason (ASC); the unmarked file and the \
             soft-deleted marked file are excluded",
        );
    }

    fn seed_file_codec(db: &Database, path: &str, codec: Option<Codec>) -> FileId {
        let mut f = sample_file(path);
        f.codec = codec;
        FilesRepo::new(db.conn())
            .insert(&f)
            .expect("seed file with codec")
    }

    #[test]
    fn list_partial_migration_fast_path_selects_fast_path_and_null_codec() {
        let db = fresh_db();
        let h264 = seed_file_codec(&db, "/m/h264.mp4", Some(Codec::H264));
        let hevc = seed_file_codec(&db, "/m/hevc.mp4", Some(Codec::H265));
        let null_codec = seed_file_codec(&db, "/m/null.mp4", None);
        let av1 = seed_file_codec(&db, "/m/av1.mp4", Some(Codec::Av1));
        let no_blob = seed_file_codec(&db, "/m/h264_noblob.mp4", Some(Codec::H264));
        let deleted = seed_file_codec(&db, "/m/h264_deleted.mp4", Some(Codec::H264));
        let repo = FingerprintsRepo::new(db.conn());
        for id in [h264, hevc, null_codec, av1, no_blob, deleted] {
            repo.upsert(&fp(id)).expect("upsert");
        }
        for id in [h264, hevc, null_codec, av1, deleted] {
            repo.set_partial(id, &[1u8, 2, 3]).expect("set partial");
        }
        FilesRepo::new(db.conn())
            .mark_deleted(deleted, now() + 1)
            .expect("soft delete");

        let rows = repo
            .list_partial_migration_fast_path()
            .expect("fast-path enum");
        let ids: Vec<FileId> = rows.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![h264, hevc, null_codec],
            "fast-path + NULL codec carrying a blob, id ASC; AV1, blob-less, and \
             soft-deleted rows excluded",
        );
        assert_eq!(rows[0].1, NormalizedPath::new("/m/h264.mp4"));
    }

    #[test]
    fn list_partial_migration_non_fast_path_selects_known_non_fast_path() {
        let db = fresh_db();
        let av1 = seed_file_codec(&db, "/m/av1.mp4", Some(Codec::Av1));
        let vp9 = seed_file_codec(&db, "/m/vp9.webm", Some(Codec::Vp9));
        let h264 = seed_file_codec(&db, "/m/h264.mp4", Some(Codec::H264));
        let null_codec = seed_file_codec(&db, "/m/null.mp4", None);
        let av1_noblob = seed_file_codec(&db, "/m/av1_noblob.mp4", Some(Codec::Av1));
        let av1_deleted = seed_file_codec(&db, "/m/av1_deleted.mp4", Some(Codec::Av1));
        let repo = FingerprintsRepo::new(db.conn());
        for id in [av1, vp9, h264, null_codec, av1_noblob, av1_deleted] {
            repo.upsert(&fp(id)).expect("upsert");
        }
        for id in [av1, vp9, h264, null_codec, av1_deleted] {
            repo.set_partial(id, &[7u8, 7]).expect("set partial");
        }
        FilesRepo::new(db.conn())
            .mark_deleted(av1_deleted, now() + 1)
            .expect("soft delete");

        let rows = repo
            .list_partial_migration_non_fast_path()
            .expect("non-fast-path enum");
        let ids: Vec<FileId> = rows.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(
            ids,
            vec![av1, vp9],
            "known non-fast-path carrying a blob, id ASC; H264, NULL codec, blob-less, \
             and soft-deleted rows excluded",
        );
        assert_eq!(rows[0].1, 12_345);
        assert_eq!(rows[0].2, 1_700_000_000_000_000_000);
    }

    #[test]
    fn clear_partial_and_mark_skip_nulls_blob_and_stamps_marker() {
        let db = fresh_db();
        let id = seed_file_codec(&db, "/m/av1.mp4", Some(Codec::Av1));
        let repo = FingerprintsRepo::new(db.conn());
        repo.upsert(&fp(id)).expect("upsert");
        repo.set_partial(id, &[9u8, 9, 9]).expect("set partial");
        assert!(repo.get_active_partial(id).expect("get").is_some());

        let m = PartialSkipMarker {
            reason: "migrated_non_fast_path".into(),
            size_bytes: 555,
            mtime_ns: 777,
        };
        assert_eq!(
            repo.clear_partial_and_mark_skip(id, &m)
                .expect("clear+mark"),
            1,
        );

        assert!(repo.get_active_partial(id).expect("get").is_none());
        assert!(repo.list_active_partial().expect("list").is_empty());
        let read = repo
            .get_partial_skip(id)
            .expect("get marker")
            .expect("present");
        assert_eq!(read, m);

        let ghost = seed_file_codec(&db, "/m/ghost.mp4", Some(Codec::Av1));
        assert_eq!(
            repo.clear_partial_and_mark_skip(ghost, &m).expect("noop"),
            0,
        );
    }
}

mod scene_hashes {
    use super::*;

    fn sh(file_id: FileId, ts_ms: i64, band: i64) -> SceneHash {
        let phash = vec![ts_ms.to_le_bytes()[0], band.to_le_bytes()[0], 0xAB, 0xCD];
        SceneHash {
            id: 0,
            file_id,
            ts_ms,
            phash,
            band_index: band,
        }
    }

    #[test]
    fn insert_and_list_for_file_preserves_order() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/sh/list.mp4");
        let repo = SceneHashesRepo::new(db.conn());

        let ids: Vec<i64> = (0..3)
            .map(|i| repo.insert(&sh(file_id, i * 1000, i)).expect("insert"))
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));

        let rows = repo.list_for_file(file_id).expect("list");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|r| r.ts_ms).collect::<Vec<_>>(),
            vec![0, 1000, 2000],
        );
        for row in &rows {
            assert_eq!(row.phash.len(), 4);
        }
    }

    #[test]
    fn deleting_file_cascades_to_scene_hashes() {
        let db = fresh_db();
        let file_id = seed_file(&db, "/sh/cascade.mp4");
        let repo = SceneHashesRepo::new(db.conn());
        for i in 0..5 {
            repo.insert(&sh(file_id, i * 500, i)).expect("insert");
        }
        FilesRepo::new(db.conn())
            .delete(file_id)
            .expect("delete parent");
        let remaining = repo.list_for_file(file_id).expect("list");
        assert!(
            remaining.is_empty(),
            "ON DELETE CASCADE must clear scene_hashes",
        );
    }
}

mod duplicate_groups {
    use super::*;

    #[test]
    fn create_and_get_round_trips_trust_level() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let id = repo
            .create(TrustLevel::VeryLikely, now())
            .expect("create group");
        let group = repo.get(id).expect("get").expect("present");
        assert_eq!(group.id, id);
        assert_eq!(group.trust_level, TrustLevel::VeryLikely);
        assert_eq!(group.best_file_id, None);
        assert_eq!(group.created_at, now());
        assert_eq!(group.updated_at, now());
    }

    #[test]
    fn list_all_returns_every_group_in_id_order() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let exact = repo.create(TrustLevel::Exact, now()).expect("create exact");
        let likely = repo
            .create(TrustLevel::VeryLikely, now())
            .expect("create likely");
        let possible = repo
            .create(TrustLevel::Possible, now())
            .expect("create possible");

        let all = repo.list_all().expect("list all");
        let ids: Vec<i64> = all.iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![exact, likely, possible], "ordered by id ASC");
        let trusts: Vec<TrustLevel> = all.iter().map(|g| g.trust_level).collect();
        assert_eq!(
            trusts,
            vec![
                TrustLevel::Exact,
                TrustLevel::VeryLikely,
                TrustLevel::Possible
            ],
        );
    }

    #[test]
    fn list_all_is_empty_on_a_fresh_database() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        assert!(repo.list_all().expect("list all").is_empty());
    }

    #[test]
    fn add_and_list_members() {
        let db = fresh_db();
        let group_id = DuplicateGroupsRepo::new(db.conn())
            .create(TrustLevel::Exact, now())
            .expect("create");
        let a = seed_file(&db, "/dup/a.mp4");
        let b = seed_file(&db, "/dup/b.mp4");
        let repo = DuplicateGroupsRepo::new(db.conn());
        repo.add_member(group_id, a).expect("add a");
        repo.add_member(group_id, b).expect("add b");

        let mut members = repo.list_members(group_id).expect("list members");
        members.sort_by_key(|id| id.0);
        assert_eq!(members, vec![a, b]);
    }

    #[test]
    fn set_best_updates_pointer_and_timestamp() {
        let db = fresh_db();
        let group_id = DuplicateGroupsRepo::new(db.conn())
            .create(TrustLevel::Exact, now())
            .expect("create");
        let file = seed_file(&db, "/dup/best.mp4");
        let repo = DuplicateGroupsRepo::new(db.conn());
        repo.add_member(group_id, file).expect("add member");
        repo.set_best(group_id, Some(file), now() + 5)
            .expect("set best");

        let group: DuplicateGroup = repo.get(group_id).expect("get").expect("present");
        assert_eq!(group.best_file_id, Some(file));
        assert_eq!(group.updated_at, now() + 5);
    }

    #[test]
    fn remove_member_and_delete_group() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let group_id = repo.create(TrustLevel::Possible, now()).expect("create");
        let file = seed_file(&db, "/dup/removable.mp4");
        repo.add_member(group_id, file).expect("add member");

        let removed = repo.remove_member(group_id, file).expect("remove member");
        assert_eq!(removed, 1);
        assert!(repo.list_members(group_id).expect("list").is_empty());

        repo.delete(group_id).expect("delete group");
        assert!(repo.get(group_id).expect("get").is_none());
    }

    #[test]
    fn deleting_file_cascades_to_group_membership_but_keeps_group() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let group_id = repo.create(TrustLevel::Exact, now()).expect("create");
        let file = seed_file(&db, "/dup/cascade.mp4");
        repo.add_member(group_id, file).expect("add");

        FilesRepo::new(db.conn()).delete(file).expect("delete file");

        let members = repo.list_members(group_id).expect("list");
        assert!(members.is_empty(), "membership row must cascade");
        assert!(repo.get(group_id).expect("get").is_some());
    }

    #[test]
    fn delete_by_trust_clears_only_the_named_level() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let exact = repo.create(TrustLevel::Exact, now()).expect("create exact");
        let likely1 = repo
            .create(TrustLevel::VeryLikely, now())
            .expect("create likely 1");
        let likely2 = repo
            .create(TrustLevel::VeryLikely, now())
            .expect("create likely 2");

        let cleared = repo
            .delete_by_trust(TrustLevel::VeryLikely)
            .expect("delete by trust");
        assert_eq!(cleared, 2, "both VERY_LIKELY groups removed");
        assert!(repo.get(likely1).expect("get").is_none());
        assert!(repo.get(likely2).expect("get").is_none());
        assert!(repo.get(exact).expect("get").is_some());
    }

    #[test]
    fn delete_by_trust_cascades_members_and_edges() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let gid = repo
            .create(TrustLevel::VeryLikely, now())
            .expect("create group");
        let a = seed_file(&db, "/dup/edge_a.mp4");
        let b = seed_file(&db, "/dup/edge_b.mp4");
        repo.add_member(gid, a).expect("add a");
        repo.add_member(gid, b).expect("add b");
        let edges = SimilarityEdgesRepo::new(db.conn());
        edges
            .insert(&SimilarityEdge {
                group_id: gid,
                file_a: a,
                file_b: b,
                score_x1000: 900,
                partial_span: None,
                intro_outro: false,
            })
            .expect("insert edge");

        assert_eq!(
            repo.delete_by_trust(TrustLevel::VeryLikely)
                .expect("delete"),
            1,
        );
        assert!(edges.list_for_group(gid).expect("list edges").is_empty());
    }

    #[test]
    fn find_exact_group_containing_returns_group_id() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let exact_gid = repo.create(TrustLevel::Exact, now()).expect("create exact");
        let file = seed_file(&db, "/dup/exact.mp4");
        repo.add_member(exact_gid, file).expect("add member");

        let found = repo
            .find_exact_group_containing(file)
            .expect("find exact group");
        assert_eq!(found, Some(exact_gid));
    }

    #[test]
    fn find_exact_group_containing_ignores_non_exact_groups() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let likely_gid = repo
            .create(TrustLevel::VeryLikely, now())
            .expect("create likely");
        let file = seed_file(&db, "/dup/likely.mp4");
        repo.add_member(likely_gid, file).expect("add member");

        let found = repo
            .find_exact_group_containing(file)
            .expect("find exact group");
        assert_eq!(found, None);
    }

    #[test]
    fn find_exact_group_containing_returns_none_when_not_member() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let file = seed_file(&db, "/dup/lone.mp4");

        let found = repo
            .find_exact_group_containing(file)
            .expect("find exact group");
        assert_eq!(found, None);
    }

    #[test]
    fn find_groups_containing_spans_every_trust_level() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        let exact = repo.create(TrustLevel::Exact, now()).expect("create exact");
        let possible = repo
            .create(TrustLevel::Possible, now())
            .expect("create possible");
        let shared = seed_file(&db, "/dup/shared.mp4");
        repo.add_member(exact, shared).expect("add to exact");
        repo.add_member(possible, shared).expect("add to possible");
        repo.set_best(exact, Some(shared), now()).expect("set best");

        let groups = repo
            .find_groups_containing(shared)
            .expect("find groups containing");
        let ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![exact, possible], "both groups, id-ordered");
        let exact_row = groups.iter().find(|g| g.id == exact).expect("exact row");
        let possible_row = groups
            .iter()
            .find(|g| g.id == possible)
            .expect("possible row");
        assert_eq!(exact_row.best_file_id, Some(shared), "kept in EXACT");
        assert_eq!(
            possible_row.best_file_id, None,
            "deletion candidate in POSSIBLE",
        );
    }

    #[test]
    fn find_groups_containing_returns_empty_when_not_member() {
        let db = fresh_db();
        let repo = DuplicateGroupsRepo::new(db.conn());
        repo.create(TrustLevel::Exact, now()).expect("create");
        let lone = seed_file(&db, "/dup/ungrouped.mp4");

        let groups = repo
            .find_groups_containing(lone)
            .expect("find groups containing");
        assert!(groups.is_empty());
    }
}

mod similarity_edges {
    use super::*;

    fn group_with_members(db: &Database) -> (i64, FileId, FileId, FileId) {
        let group_id = DuplicateGroupsRepo::new(db.conn())
            .create(TrustLevel::VeryLikely, now())
            .expect("create group");
        let a = seed_file(db, "/edges/a.mp4");
        let b = seed_file(db, "/edges/b.mp4");
        let c = seed_file(db, "/edges/c.mp4");
        let groups = DuplicateGroupsRepo::new(db.conn());
        for id in [a, b, c] {
            groups.add_member(group_id, id).expect("add member");
        }
        (group_id, a, b, c)
    }

    #[test]
    fn insert_orders_endpoints_canonically_and_lists_for_group() {
        let db = fresh_db();
        let (group_id, a, b, c) = group_with_members(&db);
        let repo = SimilarityEdgesRepo::new(db.conn());

        repo.insert(&SimilarityEdge {
            group_id,
            file_a: b,
            file_b: a,
            score_x1000: 950,
            partial_span: None,
            intro_outro: false,
        })
        .expect("insert reversed pair");
        repo.insert(&SimilarityEdge {
            group_id,
            file_a: a,
            file_b: c,
            score_x1000: 800,
            partial_span: None,
            intro_outro: false,
        })
        .expect("insert ordered pair");

        let edges = repo.list_for_group(group_id).expect("list edges");
        assert_eq!(edges.len(), 2);
        for e in &edges {
            assert!(
                e.file_a.0 < e.file_b.0,
                "edge endpoints must be stored a<b, got {e:?}",
            );
        }
    }

    #[test]
    fn rejects_self_loop() {
        let db = fresh_db();
        let (group_id, a, _b, _c) = group_with_members(&db);
        let repo = SimilarityEdgesRepo::new(db.conn());
        let err = repo
            .insert(&SimilarityEdge {
                group_id,
                file_a: a,
                file_b: a,
                score_x1000: 1000,
                partial_span: None,
                intro_outro: false,
            })
            .expect_err("self loop must fail");
        assert!(
            matches!(err, Error::Database(_)),
            "self-loop should surface as Error::Database (CHECK violation), got {err:?}",
        );
    }

    const ASYM_SPAN: PartialEdgeSpan = PartialEdgeSpan {
        clip_start_ms: 0,
        clip_end_ms: 5_000,
        source_start_ms: 10_000,
        source_end_ms: 15_000,
        matched_scenes: 6,
        clip_scenes: 6,
    };

    #[test]
    fn partial_span_round_trips_through_insert_and_list() {
        let db = fresh_db();
        let (group_id, a, b, _c) = group_with_members(&db);
        let repo = SimilarityEdgesRepo::new(db.conn());
        repo.insert(&SimilarityEdge {
            group_id,
            file_a: a,
            file_b: b,
            score_x1000: 600,
            partial_span: Some(ASYM_SPAN),
            intro_outro: false,
        })
        .expect("insert spanned edge");

        let edges = repo.list_for_group(group_id).expect("list edges");
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].partial_span,
            Some(ASYM_SPAN),
            "span must round-trip unchanged through insert/list",
        );
    }

    #[test]
    fn partial_span_is_not_transposed_when_endpoints_swap() {
        let db = fresh_db();
        let (group_id, a, b, _c) = group_with_members(&db);
        assert!(a.0 < b.0, "fixture invariant: a sorts before b");
        let repo = SimilarityEdgesRepo::new(db.conn());
        repo.insert(&SimilarityEdge {
            group_id,
            file_a: b,
            file_b: a,
            score_x1000: 600,
            partial_span: Some(ASYM_SPAN),
            intro_outro: false,
        })
        .expect("insert reversed spanned edge");

        let edges = repo.list_for_group(group_id).expect("list edges");
        assert_eq!(edges.len(), 1);
        assert!(
            edges[0].file_a.0 < edges[0].file_b.0,
            "endpoints stored canonically a<b",
        );
        assert_eq!(
            edges[0].partial_span,
            Some(ASYM_SPAN),
            "endpoint swap must leave the role-tagged span untouched (no transpose)",
        );
    }

    #[test]
    fn list_by_trust_carries_partial_span() {
        let db = fresh_db();
        let groups = DuplicateGroupsRepo::new(db.conn());
        let gid = groups.create(TrustLevel::Possible, now()).expect("group");
        let a = seed_file(&db, "/edges/possible_a.mp4");
        let b = seed_file(&db, "/edges/possible_b.mp4");
        groups.add_member(gid, a).expect("add a");
        groups.add_member(gid, b).expect("add b");
        let repo = SimilarityEdgesRepo::new(db.conn());
        repo.insert(&SimilarityEdge {
            group_id: gid,
            file_a: a,
            file_b: b,
            score_x1000: 600,
            partial_span: Some(ASYM_SPAN),
            intro_outro: false,
        })
        .expect("insert");

        let edges = repo
            .list_by_trust(TrustLevel::Possible)
            .expect("list by trust");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].partial_span, Some(ASYM_SPAN));
    }

    #[test]
    fn all_tagged_intro_outro_batches_one_group_by_query() {
        let db = fresh_db();
        let (gid_all, member_x, member_y, _unused) = group_with_members(&db);
        let repo = SimilarityEdgesRepo::new(db.conn());
        repo.insert(&SimilarityEdge {
            group_id: gid_all,
            file_a: member_x,
            file_b: member_y,
            score_x1000: 700,
            partial_span: None,
            intro_outro: true,
        })
        .expect("insert all-tagged edge");

        let gid_mixed = DuplicateGroupsRepo::new(db.conn())
            .create(TrustLevel::Possible, now())
            .expect("create mixed group");
        let mixed_p = seed_file(&db, "/edges/mixed_p.mp4");
        let mixed_q = seed_file(&db, "/edges/mixed_q.mp4");
        let mixed_r = seed_file(&db, "/edges/mixed_r.mp4");
        let groups2 = DuplicateGroupsRepo::new(db.conn());
        groups2.add_member(gid_mixed, mixed_p).expect("add p");
        groups2.add_member(gid_mixed, mixed_q).expect("add q");
        groups2.add_member(gid_mixed, mixed_r).expect("add r");
        repo.insert(&SimilarityEdge {
            group_id: gid_mixed,
            file_a: mixed_p,
            file_b: mixed_q,
            score_x1000: 700,
            partial_span: None,
            intro_outro: true,
        })
        .expect("insert tagged edge");
        repo.insert(&SimilarityEdge {
            group_id: gid_mixed,
            file_a: mixed_p,
            file_b: mixed_r,
            score_x1000: 700,
            partial_span: None,
            intro_outro: false,
        })
        .expect("insert untagged edge");

        let gid_empty = DuplicateGroupsRepo::new(db.conn())
            .create(TrustLevel::Exact, now())
            .expect("create empty group");

        let result = repo
            .all_tagged_intro_outro(&[gid_all, gid_mixed, gid_empty])
            .expect("batched aggregate");
        assert_eq!(
            result.get(&gid_all),
            Some(&true),
            "single tagged edge ⇒ all-tagged"
        );
        assert_eq!(
            result.get(&gid_mixed),
            Some(&false),
            "one untagged edge ⇒ not all-tagged"
        );
        assert_eq!(
            result.get(&gid_empty),
            None,
            "no edges ⇒ absent, not a fabricated entry"
        );

        assert!(
            repo.all_tagged_intro_outro(&[])
                .expect("empty input")
                .is_empty()
        );
    }
}

mod scan_state {
    use super::*;

    #[test]
    fn upsert_inserts_then_replaces_for_same_root() {
        let db = fresh_db();
        let repo = ScanStateRepo::new(db.conn());
        let entry = ScanStateEntry {
            root_path: NormalizedPath::new("/library"),
            last_scan_at: now(),
            cursor: Some(vec![1, 2, 3]),
            files_seen: 100,
            bytes_seen: 1_000_000,
        };
        repo.upsert(&entry).expect("first upsert");

        let read = repo
            .get(&NormalizedPath::new("/library"))
            .expect("get")
            .expect("present");
        assert_eq!(read, entry);

        let updated = ScanStateEntry {
            root_path: NormalizedPath::new("/library"),
            last_scan_at: now() + 10,
            cursor: None,
            files_seen: 200,
            bytes_seen: 2_000_000,
        };
        repo.upsert(&updated).expect("second upsert");
        let read2 = repo
            .get(&NormalizedPath::new("/library"))
            .expect("get")
            .expect("present");
        assert_eq!(read2, updated);
    }

    #[test]
    fn delete_removes_entry() {
        let db = fresh_db();
        let repo = ScanStateRepo::new(db.conn());
        let entry = ScanStateEntry {
            root_path: NormalizedPath::new("/nas/clips"),
            last_scan_at: now(),
            cursor: None,
            files_seen: 0,
            bytes_seen: 0,
        };
        repo.upsert(&entry).expect("upsert");
        repo.delete(&NormalizedPath::new("/nas/clips"))
            .expect("delete");
        assert!(
            repo.get(&NormalizedPath::new("/nas/clips"))
                .expect("get")
                .is_none()
        );
    }
}

mod task_queue {
    use super::*;

    fn task(kind: &str, priority: i32, payload: Option<Vec<u8>>) -> NewTask {
        NewTask {
            kind: kind.into(),
            priority,
            payload,
            enqueued_at: now(),
            size_bytes: 0,
        }
    }

    #[test]
    fn enqueue_then_get_round_trips() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let id = repo
            .enqueue(&task("scan", 5, Some(vec![0xDE, 0xAD, 0xBE, 0xEF])))
            .expect("enqueue");
        let read: Task = repo.get(id).expect("get").expect("present");
        assert_eq!(read.id, id);
        assert_eq!(read.kind, "scan");
        assert_eq!(read.state, TaskState::Pending);
        assert_eq!(read.priority, 5);
        assert_eq!(read.payload.as_deref(), Some(&[0xDE, 0xAD, 0xBE, 0xEF][..]));
        assert_eq!(read.attempts, 0);
        assert_eq!(read.enqueued_at, now());
        assert_eq!(read.started_at, None);
        assert_eq!(read.finished_at, None);
        assert!(read.last_error.is_none());
    }

    #[test]
    fn sum_outstanding_size_bytes_covers_pending_and_running() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let sized = |bytes: i64| NewTask {
            kind: "scan".into(),
            priority: 0,
            payload: None,
            enqueued_at: now(),
            size_bytes: bytes,
        };
        repo.enqueue(&sized(100)).expect("a");
        repo.enqueue(&sized(200)).expect("b");
        repo.enqueue(&sized(0)).expect("c");

        assert_eq!(repo.sum_outstanding_size_bytes().expect("sum"), 300);

        repo.dequeue_next("scan", now())
            .expect("dequeue")
            .expect("a task");
        assert_eq!(
            repo.sum_outstanding_size_bytes().expect("sum"),
            300,
            "RUNNING work is still outstanding bytes"
        );
    }

    #[test]
    fn count_distinct_files_by_state_dedups_reenqueued_rows() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let payload_a = Some(vec![1u8, 2, 3]);
        let payload_b = Some(vec![9u8, 9]);
        for _ in 0..3 {
            repo.enqueue(&task("scan", 0, payload_a.clone()))
                .expect("a");
        }
        repo.enqueue(&task("scan", 0, payload_a.clone()))
            .expect("a4");
        repo.enqueue(&task("scan", 0, payload_b.clone()))
            .expect("b");

        assert_eq!(repo.count_by_state(TaskState::Pending).expect("rows"), 5);
        assert_eq!(
            repo.count_distinct_files_by_state(TaskState::Pending)
                .expect("distinct"),
            2,
            "four rows for file A collapse to one distinct file"
        );

        repo.enqueue(&task("scan", -10, Some(vec![5u8, 5, 5])))
            .expect("densify");
        assert_eq!(repo.count_by_state(TaskState::Pending).expect("rows"), 6);
        assert_eq!(
            repo.count_distinct_files_by_state(TaskState::Pending)
                .expect("distinct"),
            2,
            "the negative-priority densify task is excluded from the count"
        );
    }

    #[test]
    fn has_failed_with_size_matches_only_same_payload_and_size() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let payload = vec![7u8, 7, 7];
        let id = repo
            .enqueue(&NewTask {
                kind: "scan".into(),
                priority: 0,
                payload: Some(payload.clone()),
                enqueued_at: now(),
                size_bytes: 90_000,
            })
            .expect("enqueue");
        repo.dequeue_next("scan", now()).expect("dq").expect("task");
        repo.mark_failed(id, now(), "decode error").expect("fail");

        assert!(repo.has_failed_with_size(&payload, 90_000).expect("q"));
        assert!(!repo.has_failed_with_size(&payload, 91_000).expect("q"));
        assert!(!repo.has_failed_with_size(&[1, 2], 90_000).expect("q"));
    }

    #[test]
    fn count_failed_by_payload_accumulates_across_separate_rows() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let payload = vec![5u8, 5, 5];

        assert_eq!(
            repo.count_failed_by_payload("scan", &payload).expect("q"),
            0,
            "no rows yet"
        );

        let id1 = repo
            .enqueue(&NewTask {
                kind: "scan".into(),
                priority: -200,
                payload: Some(payload.clone()),
                enqueued_at: now(),
                size_bytes: 0,
            })
            .expect("enqueue 1");
        repo.dequeue_next("scan", now()).expect("dq").expect("task");
        repo.mark_failed(id1, now(), "io error").expect("fail 1");
        assert_eq!(
            repo.count_failed_by_payload("scan", &payload).expect("q"),
            1,
            "one FAILED row counted"
        );

        let id2 = repo
            .enqueue(&NewTask {
                kind: "scan".into(),
                priority: -200,
                payload: Some(payload.clone()),
                enqueued_at: now(),
                size_bytes: 0,
            })
            .expect("enqueue 2");
        repo.dequeue_next("scan", now()).expect("dq").expect("task");
        repo.mark_failed(id2, now(), "io error").expect("fail 2");
        assert_eq!(
            repo.count_failed_by_payload("scan", &payload).expect("q"),
            2,
            "two separate FAILED rows counted"
        );

        assert_eq!(
            repo.count_failed_by_payload("hash", &payload).expect("q"),
            0,
            "a different kind must not match"
        );
        assert_eq!(
            repo.count_failed_by_payload("scan", &[1, 2]).expect("q"),
            0,
            "a different payload must not match"
        );

        repo.enqueue(&NewTask {
            kind: "scan".into(),
            priority: -200,
            payload: Some(payload.clone()),
            enqueued_at: now(),
            size_bytes: 0,
        })
        .expect("enqueue 3 (left PENDING)");
        assert_eq!(
            repo.count_failed_by_payload("scan", &payload).expect("q"),
            2,
            "a PENDING row must not inflate the FAILED count"
        );
    }

    #[test]
    fn list_active_payloads_returns_only_non_terminal_payloaded_rows() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());

        repo.enqueue(&task("scan", 0, Some(vec![1u8, 1])))
            .expect("pending");
        let running = repo
            .enqueue(&task("scan", 0, Some(vec![2u8, 2])))
            .expect("running enqueue");
        let failed = repo
            .enqueue(&task("scan", 0, Some(vec![8u8])))
            .expect("failed enqueue");
        repo.enqueue(&task("scan", 0, None)).expect("null payload");
        repo.enqueue(&task("other", 0, Some(vec![7u8])))
            .expect("other kind");

        loop {
            let t = repo.dequeue_next("scan", now()).expect("dq").expect("task");
            if t.id == failed {
                repo.mark_failed(failed, now(), "boom").expect("fail");
                break;
            }
            if t.id == running {}
        }

        let active: Vec<Vec<u8>> = repo
            .list_active_payloads("scan")
            .expect("list")
            .into_iter()
            .map(|(_, payload)| payload)
            .collect();
        assert!(
            active.contains(&vec![1u8, 1]),
            "PENDING/RUNNING [1,1] present"
        );
        assert!(active.contains(&vec![2u8, 2]), "RUNNING [2,2] present");
        assert!(!active.contains(&vec![8u8]), "FAILED row excluded");
        assert!(!active.contains(&vec![7u8]), "other kind excluded");
        assert_eq!(
            active.len(),
            2,
            "only the two non-terminal payloaded scan rows; NULL + FAILED + other excluded"
        );
    }

    #[test]
    fn sum_outstanding_size_bytes_is_zero_for_empty_queue() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        assert_eq!(repo.sum_outstanding_size_bytes().expect("sum"), 0);
    }

    #[test]
    fn dequeue_next_respects_priority_then_fifo() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let low = repo.enqueue(&task("scan", 0, None)).expect("low");
        let high = repo.enqueue(&task("scan", 10, None)).expect("high");
        let mid_a = repo.enqueue(&task("scan", 5, None)).expect("mid a");
        let mid_b = repo.enqueue(&task("scan", 5, None)).expect("mid b");

        let order = (0..4)
            .map(|_| {
                repo.dequeue_next("scan", now())
                    .expect("dequeue")
                    .expect("task available")
                    .id
            })
            .collect::<Vec<_>>();
        assert_eq!(order, vec![high, mid_a, mid_b, low]);
        assert!(repo.dequeue_next("scan", now()).expect("dequeue").is_none());
    }

    #[test]
    fn dequeue_marks_task_running_with_started_at() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let id = repo.enqueue(&task("hash", 1, None)).expect("enqueue");
        let claimed = repo
            .dequeue_next("hash", now() + 10)
            .expect("dequeue")
            .expect("present");
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.state, TaskState::Running);
        assert_eq!(claimed.started_at, Some(now() + 10));
    }

    #[test]
    fn mark_done_and_mark_failed_transition_terminal_states() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());

        let ok_id = repo.enqueue(&task("scan", 1, None)).expect("enqueue ok");
        repo.dequeue_next("scan", now())
            .expect("dq")
            .expect("present");
        repo.mark_done(ok_id, now() + 100).expect("mark_done");
        let done = repo.get(ok_id).expect("get").expect("present");
        assert_eq!(done.state, TaskState::Done);
        assert_eq!(done.finished_at, Some(now() + 100));

        let bad_id = repo.enqueue(&task("scan", 1, None)).expect("enqueue bad");
        repo.dequeue_next("scan", now())
            .expect("dq")
            .expect("present");
        repo.mark_failed(bad_id, now() + 200, "boom").expect("fail");
        let failed = repo.get(bad_id).expect("get").expect("present");
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.finished_at, Some(now() + 200));
        assert_eq!(failed.last_error.as_deref(), Some("boom"));
        assert_eq!(failed.attempts, 1);
    }

    #[test]
    fn delete_removes_a_task_row() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let keep = repo.enqueue(&task("scan", 1, None)).expect("keep");
        let gone = repo.enqueue(&task("scan", 1, None)).expect("gone");

        repo.delete(gone).expect("delete");

        assert!(repo.get(gone).expect("get gone").is_none(), "row deleted");
        assert!(
            repo.get(keep).expect("get keep").is_some(),
            "other untouched"
        );
        repo.delete(gone).expect("second delete is fine");
    }

    #[test]
    fn list_by_state_filters_correctly() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let a = repo.enqueue(&task("scan", 1, None)).expect("a");
        let b = repo.enqueue(&task("scan", 1, None)).expect("b");
        let _c = repo.enqueue(&task("scan", 1, None)).expect("c");
        repo.dequeue_next("scan", now()).expect("dq").expect("a");
        repo.dequeue_next("scan", now()).expect("dq").expect("b");
        repo.mark_done(a, now() + 1).expect("done a");

        let running: Vec<i64> = repo
            .list_by_state(TaskState::Running)
            .expect("list running")
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(running, vec![b]);

        let done: Vec<i64> = repo
            .list_by_state(TaskState::Done)
            .expect("list done")
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(done, vec![a]);

        let pending = repo
            .list_by_state(TaskState::Pending)
            .expect("list pending");
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn count_by_state_matches_list_len_without_materialising_rows() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        repo.enqueue(&task("scan", 1, None)).expect("a");
        repo.enqueue(&task("scan", 1, None)).expect("b");
        repo.enqueue(&task("scan", 1, None)).expect("c");
        repo.dequeue_next("scan", now()).expect("dq running");
        let done = repo.dequeue_next("scan", now()).expect("dq").expect("done");
        repo.mark_done(done.id, now() + 1).expect("done");

        assert_eq!(repo.count_by_state(TaskState::Failed).expect("failed"), 0);
        for state in [TaskState::Pending, TaskState::Running, TaskState::Done] {
            let listed = repo.list_by_state(state).expect("list").len() as u64;
            assert_eq!(
                repo.count_by_state(state).expect("count"),
                listed,
                "{state:?}"
            );
        }
        assert_eq!(repo.count_by_state(TaskState::Pending).expect("pending"), 1);
        assert_eq!(repo.count_by_state(TaskState::Running).expect("running"), 1);
        assert_eq!(repo.count_by_state(TaskState::Done).expect("done"), 1);
    }

    #[test]
    fn count_pending_min_priority_excludes_low_priority_backlog() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        repo.enqueue(&task("scan", 0, None)).expect("fresh a");
        repo.enqueue(&task("scan", 3, None)).expect("fresh b");
        repo.enqueue(&task("scan", -100, None)).expect("densify");

        assert_eq!(repo.count_pending_min_priority(0).expect("fresh"), 2);
        assert_eq!(repo.count_pending_min_priority(-100).expect("all"), 3);

        repo.dequeue_next("scan", now()).expect("dq a");
        repo.dequeue_next("scan", now()).expect("dq b");
        assert_eq!(repo.count_pending_min_priority(0).expect("drained"), 0);
        assert_eq!(repo.count_by_state(TaskState::Pending).expect("left"), 1);
    }

    #[test]
    fn count_distinct_files_at_priority_isolates_the_partial_pass() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let clip = Some(vec![1u8, 1, 1]);
        let source = Some(vec![2u8, 2, 2]);
        repo.enqueue(&task("scan", -200, clip.clone()))
            .expect("clip");
        repo.enqueue(&task("scan", -200, clip.clone()))
            .expect("clip dup");
        repo.enqueue(&task("scan", -200, source.clone()))
            .expect("source");
        repo.enqueue(&task("scan", 0, Some(vec![3u8, 3, 3])))
            .expect("fresh");
        repo.enqueue(&task("scan", -100, Some(vec![4u8, 4, 4])))
            .expect("densify");

        assert_eq!(
            repo.count_distinct_files_at_priority(-200, TaskState::Pending)
                .expect("partial pending"),
            2
        );
        assert_eq!(
            repo.count_distinct_files_by_state(TaskState::Pending)
                .expect("foreground pending"),
            1
        );
        assert_eq!(
            repo.count_distinct_files_at_priority(-200, TaskState::Running)
                .expect("partial running"),
            0
        );

        repo.dequeue_next("scan", now()).expect("fresh");
        repo.dequeue_next("scan", now()).expect("densify");
        repo.dequeue_next("scan", now()).expect("partial");
        assert_eq!(
            repo.count_distinct_files_at_priority(-200, TaskState::Running)
                .expect("partial running after claims"),
            1,
            "the claimed partial file is now in flight"
        );
    }

    #[test]
    fn dequeue_next_at_priority_serves_only_the_named_band() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let fresh = repo
            .enqueue(&task("scan", 0, Some(vec![1])))
            .expect("fresh");
        let partial_a = repo
            .enqueue(&task("scan", -200, Some(vec![2])))
            .expect("partial a");
        let partial_b = repo
            .enqueue(&task("scan", -200, Some(vec![3])))
            .expect("partial b");

        let first = repo
            .dequeue_next_at_priority("scan", -200, now())
            .expect("claim")
            .expect("a partial task");
        assert_eq!(first.id, partial_a, "FIFO within the band");
        let second = repo
            .dequeue_next_at_priority("scan", -200, now())
            .expect("claim")
            .expect("a partial task");
        assert_eq!(second.id, partial_b);
        assert!(
            repo.dequeue_next_at_priority("scan", -200, now())
                .expect("claim")
                .is_none(),
            "no more partial-band tasks despite a PENDING foreground task"
        );
        assert_eq!(
            repo.dequeue_next("scan", now())
                .expect("dq")
                .expect("fresh")
                .id,
            fresh
        );
    }

    #[test]
    fn requeue_running_recovers_inflight_tasks_preserving_attempts() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let claimed_a = repo.enqueue(&task("scan", 1, None)).expect("a");
        let claimed_b = repo.enqueue(&task("hash", 1, None)).expect("b");
        let _pending = repo.enqueue(&task("scan", 1, None)).expect("c");

        repo.dequeue_next("scan", now())
            .expect("dq scan")
            .expect("a");
        repo.dequeue_next("hash", now())
            .expect("dq hash")
            .expect("b");
        assert_eq!(
            repo.list_by_state(TaskState::Running)
                .expect("running")
                .len(),
            2
        );

        let recovered = repo.requeue_running().expect("requeue");
        assert_eq!(recovered, 2);

        assert!(
            repo.list_by_state(TaskState::Running)
                .expect("running")
                .is_empty()
        );
        assert_eq!(
            repo.list_by_state(TaskState::Pending)
                .expect("pending")
                .len(),
            3
        );

        let a = repo.get(claimed_a).expect("get a").expect("a");
        assert_eq!(a.state, TaskState::Pending);
        assert_eq!(a.started_at, None);
        assert_eq!(a.attempts, 1);
        let b = repo.get(claimed_b).expect("get b").expect("b");
        assert_eq!(b.attempts, 1);
    }

    #[test]
    fn requeue_running_is_noop_when_nothing_is_running() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        repo.enqueue(&task("scan", 1, None)).expect("enqueue");
        assert_eq!(repo.requeue_running().expect("requeue"), 0);
        assert_eq!(
            repo.list_by_state(TaskState::Pending)
                .expect("pending")
                .len(),
            1
        );
    }

    #[test]
    fn dequeue_next_filters_by_kind() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let _scan_id = repo.enqueue(&task("scan", 5, None)).expect("enqueue scan");
        let hash_id = repo.enqueue(&task("hash", 10, None)).expect("enqueue hash");

        let claimed = repo
            .dequeue_next("hash", now())
            .expect("dequeue")
            .expect("present");
        assert_eq!(claimed.id, hash_id);
        assert_eq!(claimed.kind, "hash");
    }

    #[test]
    fn dequeue_next_increments_attempts() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let id = repo.enqueue(&task("scan", 1, None)).expect("enqueue");

        let first = repo.get(id).expect("get").expect("present");
        assert_eq!(first.attempts, 0);

        let claimed = repo
            .dequeue_next("scan", now())
            .expect("dequeue")
            .expect("present");
        assert_eq!(claimed.attempts, 1);

        let second = repo.get(id).expect("get").expect("present");
        assert_eq!(second.attempts, 1);
    }

    #[test]
    fn requeue_busy_task_round_trips() {
        let db = fresh_db();
        let repo = TaskQueueRepo::new(db.conn());
        let id = repo.enqueue(&task("scan", 1, None)).expect("enqueue");

        let claimed = repo
            .dequeue_next("scan", now())
            .expect("dequeue")
            .expect("present");
        assert_eq!(claimed.state, TaskState::Running);

        repo.requeue_busy_task(id, now() + 10, 3).expect("requeue");

        let requeued = repo.get(id).expect("get").expect("present");
        assert_eq!(requeued.state, TaskState::Pending);
        assert_eq!(requeued.attempts, 3);
        assert_eq!(requeued.enqueued_at, now() + 10);
        assert_eq!(requeued.started_at, None);

        let none = repo.dequeue_next("scan", now()).expect("dequeue");
        assert!(none.is_none());

        let claimed_again = repo
            .dequeue_next("scan", now() + 10)
            .expect("dequeue")
            .expect("present");
        assert_eq!(claimed_again.id, id);
        assert_eq!(claimed_again.attempts, 4);
    }
}

mod transactions {
    use super::*;

    #[test]
    fn transaction_commits_on_ok() {
        let mut db = fresh_db();
        let id = db
            .transaction(|conn| {
                let repo = FilesRepo::new(conn);
                repo.insert(&sample_file("/tx/commit.mp4"))
            })
            .expect("commit");
        let read = FilesRepo::new(db.conn()).get(id).expect("get");
        assert!(read.is_some(), "committed row must be visible afterwards");
    }

    #[test]
    fn transaction_rolls_back_on_err() {
        let mut db = fresh_db();
        let pre = FilesRepo::new(db.conn())
            .insert(&sample_file("/tx/pre.mp4"))
            .expect("seed");

        let outcome = db.transaction(|conn| -> vidcull_core::Result<()> {
            FilesRepo::new(conn).insert(&sample_file("/tx/should-rollback.mp4"))?;
            Err(Error::Database("intentional".into()))
        });
        assert!(outcome.is_err());

        let repo = FilesRepo::new(db.conn());
        assert!(
            repo.find_by_path(&NormalizedPath::new("/tx/should-rollback.mp4"))
                .expect("find")
                .is_none(),
            "rolled-back insert must not be visible",
        );
        assert!(
            repo.get(pre).expect("get").is_some(),
            "pre-existing row must survive a rollback",
        );
    }
}

mod system_metadata {
    use super::*;

    #[test]
    fn get_missing_key_is_none() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        assert_eq!(repo.get("partial_index_reconciled").expect("get"), None);
        assert!(!repo.contains("partial_index_reconciled").expect("contains"));
    }

    #[test]
    fn set_then_get_round_trips() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        repo.set("partial_index_reconciled", "1").expect("set");
        assert_eq!(
            repo.get("partial_index_reconciled").expect("get"),
            Some("1".to_owned()),
        );
        assert!(repo.contains("partial_index_reconciled").expect("contains"));
    }

    #[test]
    fn set_overwrites_existing_value() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        repo.set("k", "first").expect("set");
        repo.set("k", "second").expect("overwrite");
        assert_eq!(repo.get("k").expect("get"), Some("second".to_owned()));
    }

    #[test]
    fn distinct_keys_are_independent() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        repo.set("a", "1").expect("set a");
        repo.set("b", "2").expect("set b");
        assert_eq!(repo.get("a").expect("get a"), Some("1".to_owned()));
        assert_eq!(repo.get("b").expect("get b"), Some("2".to_owned()));
        assert!(
            repo.contains("target_os")
                .expect("target_os recorded on open")
        );
    }

    #[test]
    fn delete_removes_key_and_is_idempotent_when_absent() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        repo.set("partial_cold_checkpoint", "7:12000").expect("set");
        assert!(repo.contains("partial_cold_checkpoint").expect("contains"));
        repo.delete("partial_cold_checkpoint").expect("delete");
        assert!(
            !repo
                .contains("partial_cold_checkpoint")
                .expect("after delete")
        );
        repo.delete("partial_cold_checkpoint")
            .expect("delete absent is ok");
    }

    #[test]
    fn groups_revision_starts_at_zero_with_no_key_set() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        assert_eq!(repo.groups_revision().expect("groups_revision"), 0);
    }

    #[test]
    fn bump_groups_revision_increments_from_zero_and_persists() {
        let db = fresh_db();
        let repo = SystemMetadataRepo::new(db.conn());
        repo.bump_groups_revision().expect("bump 1");
        assert_eq!(repo.groups_revision().expect("read after bump 1"), 1);
        repo.bump_groups_revision().expect("bump 2");
        repo.bump_groups_revision().expect("bump 3");
        assert_eq!(repo.groups_revision().expect("read after bump 3"), 3);
    }
}

mod delete_journal {
    use super::*;

    fn full_batch(group_id: i64, files: &[(FileId, String, BatchFileRole)]) -> NewDeleteBatch<'_> {
        NewDeleteBatch {
            group_id,
            trust_level: TrustLevel::VeryLikely,
            non_transitive: true,
            best_file_id: files.first().map(|(id, _, _)| *id),
            group_dropped: true,
            mode: DeleteBatchMode::Trash,
            files,
            created_at: now(),
        }
    }

    #[test]
    fn record_then_last_round_trips_every_field() {
        let db = fresh_db();
        let file_a = seed_file(&db, "/jrnl/a.mp4");
        let file_b = seed_file(&db, "/jrnl/b.mp4");

        let files = vec![
            (file_a, "/jrnl/a.mp4".to_owned(), BatchFileRole::Deleted),
            (file_b, "/jrnl/b.mp4".to_owned(), BatchFileRole::Survivor),
        ];
        let batch = full_batch(42, &files);
        let repo = DeleteJournalRepo::new(db.conn());
        let id = repo.record(&batch).expect("record");
        assert!(id > 0);

        let last = repo.last().expect("last").expect("present");
        assert_eq!(last.id, id);
        assert_eq!(last.group_id, 42);
        assert_eq!(last.trust_level, TrustLevel::VeryLikely);
        assert!(
            last.non_transitive,
            "non_transitive round-trips through record/last"
        );
        assert_eq!(last.best_file_id, Some(file_a));
        assert!(last.group_dropped);
        assert_eq!(last.mode, DeleteBatchMode::Trash);
        assert_eq!(last.created_at, now());
        assert_eq!(
            last.files,
            vec![
                DeleteBatchFile {
                    file_id: file_a,
                    path: "/jrnl/a.mp4".to_owned(),
                    role: BatchFileRole::Deleted,
                },
                DeleteBatchFile {
                    file_id: file_b,
                    path: "/jrnl/b.mp4".to_owned(),
                    role: BatchFileRole::Survivor,
                },
            ]
        );
    }

    #[test]
    fn last_returns_highest_id_batch_and_remove_reveals_previous() {
        let db = fresh_db();
        let file_a = seed_file(&db, "/jrnl2/a.mp4");
        let file_b = seed_file(&db, "/jrnl2/b.mp4");

        let files_1 = vec![(file_a, "/jrnl2/a.mp4".to_owned(), BatchFileRole::Deleted)];
        let files_2 = vec![(file_b, "/jrnl2/b.mp4".to_owned(), BatchFileRole::Deleted)];
        let repo = DeleteJournalRepo::new(db.conn());

        let id1 = repo
            .record(&NewDeleteBatch {
                group_id: 1,
                trust_level: TrustLevel::Exact,
                non_transitive: false,
                best_file_id: None,
                group_dropped: false,
                mode: DeleteBatchMode::Permanent,
                files: &files_1,
                created_at: now(),
            })
            .expect("record 1");
        let id2 = repo
            .record(&NewDeleteBatch {
                group_id: 2,
                trust_level: TrustLevel::Possible,
                non_transitive: false,
                best_file_id: None,
                group_dropped: true,
                mode: DeleteBatchMode::Trash,
                files: &files_2,
                created_at: now() + 1,
            })
            .expect("record 2");

        let last = repo.last().expect("last").expect("present");
        assert_eq!(last.id, id2);
        assert_eq!(last.group_id, 2);

        repo.remove(id2).expect("remove id2");
        let prev = repo.last().expect("last after remove").expect("present");
        assert_eq!(prev.id, id1);
        assert_eq!(prev.group_id, 1);

        repo.remove(id1).expect("remove id1");
        assert!(repo.last().expect("last empty").is_none());
    }

    #[test]
    fn remove_cascades_to_batch_files() {
        let db = fresh_db();
        let file_a = seed_file(&db, "/jrnl3/a.mp4");
        let files = vec![(file_a, "/jrnl3/a.mp4".to_owned(), BatchFileRole::Deleted)];
        let repo = DeleteJournalRepo::new(db.conn());
        let batch_id = repo.record(&full_batch(99, &files)).expect("record");

        let count_before: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM delete_batch_files WHERE batch_id = ?1",
                rusqlite::params![batch_id],
                |r| r.get(0),
            )
            .expect("count before");
        assert_eq!(count_before, 1);

        repo.remove(batch_id).expect("remove");

        let count_after: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM delete_batch_files WHERE batch_id = ?1",
                rusqlite::params![batch_id],
                |r| r.get(0),
            )
            .expect("count after");
        assert_eq!(
            count_after, 0,
            "ON DELETE CASCADE must remove batch file rows"
        );
    }

    #[test]
    fn hard_delete_file_cascades_journal_file_row() {
        let db = fresh_db();
        let file_a = seed_file(&db, "/jrnl4/a.mp4");
        let files = vec![(file_a, "/jrnl4/a.mp4".to_owned(), BatchFileRole::Deleted)];
        let repo = DeleteJournalRepo::new(db.conn());
        let batch_id = repo.record(&full_batch(77, &files)).expect("record");

        FilesRepo::new(db.conn())
            .delete(file_a)
            .expect("hard delete file");

        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM delete_batch_files WHERE batch_id = ?1",
                rusqlite::params![batch_id],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(
            count, 0,
            "hard-deleting a files row must cascade to delete_batch_files",
        );

        let count_batch: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM delete_batches WHERE id = ?1",
                rusqlite::params![batch_id],
                |r| r.get(0),
            )
            .expect("count batch");
        assert_eq!(count_batch, 1, "batch header row survives the file cascade");
    }

    #[test]
    fn clear_deleted_restores_soft_deleted_file_to_active() {
        let db = fresh_db();
        let files_repo = FilesRepo::new(db.conn());
        let id = files_repo
            .insert(&sample_file("/jrnl5/a.mp4"))
            .expect("insert");

        files_repo.mark_deleted(id, now()).expect("mark_deleted");
        assert!(
            files_repo.list_active().expect("list").is_empty(),
            "precondition: file must be hidden after mark_deleted",
        );

        files_repo.clear_deleted(id).expect("clear_deleted");
        let active = files_repo.list_active().expect("list active after undo");
        assert_eq!(
            active.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![id],
            "file must reappear in list_active after clear_deleted",
        );
        let row = files_repo.get(id).expect("get").expect("row");
        assert_eq!(row.deleted_at, None, "deleted_at must be NULL after undo");
    }

    #[test]
    fn create_with_id_restores_exact_group_id_and_add_member_if_absent_is_idempotent() {
        let db = fresh_db();
        let groups_repo = DuplicateGroupsRepo::new(db.conn());
        let file_a = seed_file(&db, "/jrnl6/a.mp4");
        let file_b = seed_file(&db, "/jrnl6/b.mp4");

        let fixed_id: i64 = 999;
        groups_repo
            .create_with_id(fixed_id, TrustLevel::Exact, false, now())
            .expect("create_with_id");

        let group = groups_repo.get(fixed_id).expect("get").expect("present");
        assert_eq!(group.id, fixed_id);
        assert_eq!(group.trust_level, TrustLevel::Exact);
        assert!(!group.non_transitive, "false passthrough preserved");

        groups_repo
            .add_member_if_absent(fixed_id, file_a)
            .expect("first add");
        groups_repo
            .add_member_if_absent(fixed_id, file_a)
            .expect("second add (idempotent)");
        groups_repo
            .add_member_if_absent(fixed_id, file_b)
            .expect("add file_b");

        let mut members = groups_repo.list_members(fixed_id).expect("list members");
        members.sort_by_key(|id| id.0);
        assert_eq!(
            members,
            vec![file_a, file_b],
            "both members present exactly once"
        );
    }
}

mod daemon_settings {
    use super::*;

    #[test]
    fn load_is_none_on_a_fresh_db() {
        let db = fresh_db();
        let repo = DaemonSettingsRepo::new(db.conn());
        assert_eq!(repo.load().expect("load"), None);
    }

    #[test]
    fn save_then_load_round_trips_the_payload() {
        let db = fresh_db();
        let repo = DaemonSettingsRepo::new(db.conn());
        let payload = b"\x00\x01\xff postcard-ish bytes \x10".to_vec();
        repo.save(&payload).expect("save");
        assert_eq!(repo.load().expect("load"), Some(payload));
    }

    #[test]
    fn save_overwrites_the_single_row() {
        let db = fresh_db();
        let repo = DaemonSettingsRepo::new(db.conn());
        repo.save(b"first").expect("first save");
        repo.save(b"second").expect("second save");
        assert_eq!(repo.load().expect("load"), Some(b"second".to_vec()));
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM daemon_settings", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1, "settings are a single row");
    }
}
