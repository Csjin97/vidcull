use std::path::{Path, PathBuf};

use vidcull_core::types::{Codec, NormalizedPath, Resolution, VideoDuration};
use vidcull_db::repo::{FilesRepo, NewFile};
use vidcull_scanner::{ScanOptions, collect, walk};

const FAKE_SEEN_AT: i64 = 1_700_000_000;

fn parser_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vidcull-parser")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn copy_into(scan_dir: &Path, fixture_name: &str) -> PathBuf {
    let dest = scan_dir.join(fixture_name);
    std::fs::copy(parser_fixture(fixture_name), &dest).expect("copy fixture");
    dest
}

fn new_file_from_collected(c: &vidcull_scanner::CollectedFile) -> NewFile {
    let mtime_ns = i64::try_from(c.fingerprint.mtime_ns).expect("mtime fits in i64");
    let size_bytes = i64::try_from(c.fingerprint.size_bytes).expect("size fits in i64");
    let inode = c.fingerprint.inode.map(|n| {
        #[allow(clippy::cast_possible_wrap)]
        let signed = n as i64;
        signed
    });
    let fps_x1000 = c
        .video
        .fps_x1000
        .map(|v| i32::try_from(v).expect("fps fits in i32"));
    let bitrate_bps = c
        .video
        .bitrate_bps
        .map(|v| i64::try_from(v).expect("bitrate fits in i64"));

    NewFile {
        path: c.path.clone(),
        size_bytes,
        mtime_ns,
        inode,
        content_hash: None,
        codec: Some(c.video.codec.clone()),
        container: Some(c.video.container.short_name().to_string()),
        duration: c.video.duration,
        fps_x1000,
        bitrate_bps,
        resolution: Some(c.video.resolution),
        first_seen_at: FAKE_SEEN_AT,
        last_seen_at: FAKE_SEEN_AT,
        laplacian_variance: None,
        dct_energy: None,
        bpp: None,
        encoder_tags: None,
    }
}

#[test]
fn walk_then_collect_then_persist_round_trips_all_six_fields() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mp4 = copy_into(tmp.path(), "black_320x180_30fps_1s.mp4");
    let mkv = copy_into(tmp.path(), "black_320x180_30fps_1s.mkv");

    let entries: Vec<_> = walk(tmp.path(), &ScanOptions::default())
        .map(|r| r.expect("walk error"))
        .collect();
    assert_eq!(
        entries.len(),
        2,
        "fixture dir should yield exactly two files"
    );

    let mut db = vidcull_db::open_in_memory().expect("open db");
    let inserted_ids = db
        .transaction(|conn| {
            let repo = FilesRepo::new(conn);
            let mut ids = Vec::with_capacity(entries.len());
            for entry in entries {
                let collected = collect(entry)?;
                ids.push(repo.insert(&new_file_from_collected(&collected))?);
            }
            Ok(ids)
        })
        .expect("insert metadata");

    assert_eq!(inserted_ids.len(), 2);

    let repo = FilesRepo::new(db.conn());
    let active = repo.list_active().expect("list");
    assert_eq!(active.len(), 2);

    let mp4_row = active
        .iter()
        .find(|r| r.path == NormalizedPath::new(&mp4))
        .expect("mp4 row");
    assert_eq!(mp4_row.codec, Some(Codec::H264));
    assert_eq!(mp4_row.container.as_deref(), Some("mp4"));
    assert_eq!(mp4_row.resolution, Some(Resolution::new(320, 180)));
    assert_eq!(mp4_row.duration, Some(VideoDuration::from_millis(1000)));
    assert_eq!(mp4_row.fps_x1000, Some(30_000));
    assert_eq!(mp4_row.bitrate_bps, Some(22_416));
    assert_eq!(mp4_row.size_bytes, 2802);

    let mkv_row = active
        .iter()
        .find(|r| r.path == NormalizedPath::new(&mkv))
        .expect("mkv row");
    assert_eq!(mkv_row.codec, Some(Codec::H264));
    assert_eq!(mkv_row.container.as_deref(), Some("mkv"));
    assert_eq!(mkv_row.resolution, Some(Resolution::new(320, 180)));
    assert_eq!(mkv_row.duration, Some(VideoDuration::from_millis(1000)));
    assert_eq!(mkv_row.fps_x1000, Some(30_000));
    assert_eq!(mkv_row.bitrate_bps, Some(21_024));
    assert_eq!(mkv_row.size_bytes, 2628);
}

#[test]
fn collect_propagates_parser_unsupported_to_caller() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let avi_path = tmp.path().join("dummy.avi");
    std::fs::write(&avi_path, b"not really an avi").unwrap();

    let entry = vidcull_scanner::ScanEntry {
        path: NormalizedPath::new(&avi_path),
        fingerprint: vidcull_scanner::FsFingerprint::new(17, 0, None),
    };
    let err = collect(entry).expect_err("avi must surface Unsupported");
    assert!(
        matches!(err, vidcull_core::Error::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn collect_preserves_scan_entry_fingerprint_into_collected_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    copy_into(tmp.path(), "black_320x180_30fps_1s.mp4");

    let entry = walk(tmp.path(), &ScanOptions::default())
        .next()
        .expect("one entry")
        .expect("walk ok");

    let original_fp = entry.fingerprint;
    let collected = collect(entry).expect("collect");
    assert_eq!(collected.fingerprint, original_fp);
    assert_eq!(collected.fingerprint.size_bytes, 2802);
}
