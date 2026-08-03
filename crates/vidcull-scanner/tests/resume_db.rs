use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use tempfile::tempdir;
use vidcull_core::NormalizedPath;
use vidcull_db::repo::{ScanStateEntry, ScanStateRepo};
use vidcull_scanner::{ResumableScan, ScanCursor, ScanOptions, ScanProgress, walk};

const FAKE_NOW: i64 = 1_700_000_000;
const CRASH_AFTER: usize = 3;

fn touch(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    let mut f = File::create(path).expect("create file");
    f.write_all(body).expect("write body");
    f.sync_all().expect("sync");
}

fn build_corpus(root: &Path) -> Vec<NormalizedPath> {
    touch(&root.join("a.mp4"), b"a---");
    touch(&root.join("a-dir/file1.mp4"), b"d1");
    touch(&root.join("a-dir/file2.mp4"), b"d22");
    touch(&root.join("b.mp4"), b"bb");
    touch(&root.join("c.mp4"), b"ccc");
    touch(&root.join("d/nested/deep.mp4"), b"dddd");

    walk(root, &ScanOptions::default())
        .map(|r| r.expect("walk ok").path)
        .collect()
}

fn snapshot_state_entry(root_path: &NormalizedPath, progress: &ScanProgress) -> ScanStateEntry {
    let cursor_blob = progress
        .cursor()
        .map(|c| c.to_blob().expect("postcard encode"));
    ScanStateEntry {
        root_path: root_path.clone(),
        last_scan_at: FAKE_NOW,
        cursor: cursor_blob,
        files_seen: i64::try_from(progress.files_seen()).expect("files fit"),
        bytes_seen: i64::try_from(progress.bytes_seen()).expect("bytes fit"),
    }
}

#[test]
fn checkpoint_persists_across_database_reopen_and_resumes_to_completion() {
    let dir = tempdir().expect("tempdir");
    let scan_root = dir.path().join("media");
    fs::create_dir_all(&scan_root).expect("mkdir scan root");
    let baseline_paths = build_corpus(&scan_root);
    assert!(
        baseline_paths.len() > CRASH_AFTER,
        "corpus must be larger than the simulated crash point",
    );

    let db_path = dir.path().join("vidcull.db");
    let root_path_norm = NormalizedPath::new(&scan_root);
    let opts = ScanOptions::default();

    let mut visited: BTreeSet<NormalizedPath> = BTreeSet::new();
    {
        let db = vidcull_db::open_file(&db_path).expect("open db (run 1)");
        let scan = ResumableScan::new(opts.clone());
        let mut progress = ScanProgress::new(None);

        for (i, entry) in scan.iter(&scan_root).enumerate() {
            let entry = entry.expect("walk ok");
            visited.insert(entry.path.clone());
            progress.record(&entry);
            if i + 1 == CRASH_AFTER {
                break;
            }
        }

        let mut db = db;
        db.transaction(|conn| {
            ScanStateRepo::new(conn).upsert(&snapshot_state_entry(&root_path_norm, &progress))
        })
        .expect("checkpoint");
    }

    let reopened = vidcull_db::open_file(&db_path).expect("open db (run 2)");
    let persisted = ScanStateRepo::new(reopened.conn())
        .get(&root_path_norm)
        .expect("query scan_state")
        .expect("checkpoint row exists");
    assert_eq!(persisted.root_path, root_path_norm);
    assert_eq!(
        persisted.files_seen,
        i64::try_from(CRASH_AFTER).expect("crash count fits in i64"),
    );

    let restored_cursor =
        ScanCursor::from_blob(persisted.cursor.as_deref().expect("cursor blob present"))
            .expect("decode cursor blob");

    let scan = ResumableScan::resume_from(opts.clone(), restored_cursor.clone());
    let mut progress = ScanProgress::new(Some(&restored_cursor));
    for entry in scan.iter(&scan_root) {
        let entry = entry.expect("walk ok");
        assert!(
            visited.insert(entry.path.clone()),
            "{} visited twice — resume must skip already-processed paths",
            entry.path
        );
        progress.record(&entry);
    }

    let mut db = reopened;
    db.transaction(|conn| {
        ScanStateRepo::new(conn).upsert(&snapshot_state_entry(&root_path_norm, &progress))
    })
    .expect("final checkpoint");

    assert_eq!(visited.len(), baseline_paths.len());
    let baseline_set: BTreeSet<_> = baseline_paths.iter().cloned().collect();
    assert_eq!(visited, baseline_set);

    let final_state = ScanStateRepo::new(db.conn())
        .get(&root_path_norm)
        .expect("query")
        .expect("row");
    assert_eq!(
        final_state.files_seen,
        i64::try_from(baseline_paths.len()).expect("baseline count fits"),
    );

    let expected_bytes: u64 = walk(&scan_root, &ScanOptions::default())
        .map(|r| r.expect("ok").fingerprint.size_bytes)
        .sum();
    assert_eq!(
        final_state.bytes_seen,
        i64::try_from(expected_bytes).expect("bytes fit in i64"),
    );
}

#[test]
fn final_checkpoint_after_clean_scan_has_no_cursor_replay_remaining() {
    let dir = tempdir().expect("tempdir");
    let scan_root = dir.path().join("media");
    fs::create_dir_all(&scan_root).expect("mkdir scan root");
    let baseline = build_corpus(&scan_root);

    let opts = ScanOptions::default();
    let mut progress = ScanProgress::new(None);
    {
        let scan = ResumableScan::new(opts.clone());
        for entry in scan.iter(&scan_root) {
            progress.record(&entry.expect("walk ok"));
        }
    }
    assert_eq!(
        progress.files_seen(),
        u64::try_from(baseline.len()).expect("baseline count fits"),
    );

    let final_cursor = progress.cursor().expect("cursor after full scan");
    let scan = ResumableScan::resume_from(opts, final_cursor);
    let remaining: Vec<_> = scan.iter(&scan_root).map(|r| r.expect("ok")).collect();
    assert!(remaining.is_empty());
}
