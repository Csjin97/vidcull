use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use tempfile::tempdir;
use vidcull_core::NormalizedPath;
use vidcull_scanner::{ResumableScan, ScanCursor, ScanOptions, ScanProgress, walk};

fn touch(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    let mut f = File::create(path).expect("create file");
    f.write_all(body).expect("write body");
    f.sync_all().expect("sync");
}

fn basename(p: &NormalizedPath) -> String {
    Path::new(p.as_str())
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned()
}

fn build_corpus(root: &Path) {
    touch(&root.join("a.mp4"), b"a---");
    touch(&root.join("a-dir/file1.mp4"), b"d1");
    touch(&root.join("a-dir/file2.mp4"), b"d22");
    touch(&root.join("b.mp4"), b"bb");
    touch(&root.join("c.mp4"), b"ccc");
}

#[test]
fn scan_cursor_round_trips_via_postcard() {
    let original = ScanCursor {
        last_completed_path: NormalizedPath::new("D:/library/subdir/file.mp4"),
        files_seen: 1234,
        bytes_seen: 9_876_543_210,
    };
    let blob = original.to_blob().expect("encode");
    let decoded = ScanCursor::from_blob(&blob).expect("decode");
    assert_eq!(original, decoded);
}

#[test]
fn scan_cursor_from_blob_rejects_garbage() {
    let bytes = [0xFF_u8; 4];
    let err = ScanCursor::from_blob(&bytes).expect_err("postcard must reject");
    let msg = err.to_string();
    assert!(
        matches!(err, vidcull_core::Error::Serialization(_)),
        "expected Serialization variant: {msg}",
    );
}

#[test]
fn progress_starts_at_zero_when_no_prior_cursor() {
    let p = ScanProgress::new(None);
    assert_eq!(p.files_seen(), 0);
    assert_eq!(p.bytes_seen(), 0);
    assert!(p.cursor().is_none());
}

#[test]
fn progress_resumes_counters_from_prior_cursor() {
    let prior = ScanCursor {
        last_completed_path: NormalizedPath::new("/x.mp4"),
        files_seen: 7,
        bytes_seen: 4096,
    };
    let p = ScanProgress::new(Some(&prior));
    assert_eq!(p.files_seen(), 7);
    assert_eq!(p.bytes_seen(), 4096);
    let restored = p.cursor().expect("cursor present after resume");
    assert_eq!(restored, prior);
}

#[test]
fn progress_record_accumulates_files_and_bytes() {
    let dir = tempdir().expect("tempdir");
    touch(&dir.path().join("one.mp4"), b"AAA");
    touch(&dir.path().join("two.mp4"), b"BBBBB");

    let entries: Vec<_> = walk(dir.path(), &ScanOptions::default())
        .map(|r| r.expect("walk ok"))
        .collect();
    assert_eq!(entries.len(), 2);

    let mut progress = ScanProgress::new(None);
    progress.record(&entries[0]);
    assert_eq!(progress.files_seen(), 1);
    assert_eq!(progress.bytes_seen(), 3);
    assert_eq!(
        progress.cursor().expect("cursor").last_completed_path,
        entries[0].path
    );

    progress.record(&entries[1]);
    assert_eq!(progress.files_seen(), 2);
    assert_eq!(progress.bytes_seen(), 8);
    assert_eq!(
        progress.cursor().expect("cursor").last_completed_path,
        entries[1].path
    );
}

#[test]
fn resumable_scan_with_no_cursor_yields_every_entry() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    build_corpus(root);

    let baseline: Vec<_> = walk(root, &ScanOptions::default())
        .map(|r| r.expect("walk ok"))
        .collect();

    let scan = ResumableScan::new(ScanOptions::default());
    let resumed: Vec<_> = scan.iter(root).map(|r| r.expect("resume ok")).collect();

    assert_eq!(resumed.len(), baseline.len());
    let baseline_names: Vec<_> = baseline.iter().map(|e| basename(&e.path)).collect();
    let resumed_names: Vec<_> = resumed.iter().map(|e| basename(&e.path)).collect();
    assert_eq!(resumed_names, baseline_names);
}

#[test]
fn resumable_scan_skips_through_sentinel_then_yields_rest() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    build_corpus(root);

    let all: Vec<_> = walk(root, &ScanOptions::default())
        .map(|r| r.expect("walk ok"))
        .collect();
    let sentinel = all[1].path.clone();

    let cursor = ScanCursor {
        last_completed_path: sentinel.clone(),
        files_seen: 2,
        bytes_seen: all[0].fingerprint.size_bytes + all[1].fingerprint.size_bytes,
    };
    let scan = ResumableScan::resume_from(ScanOptions::default(), cursor);
    let yielded: Vec<_> = scan.iter(root).map(|r| r.expect("ok")).collect();

    let expected: Vec<_> = all.iter().skip(2).cloned().collect();
    assert_eq!(
        yielded.len(),
        expected.len(),
        "resumed iter must drop exactly the prefix up to and including the sentinel",
    );
    for (got, want) in yielded.iter().zip(&expected) {
        assert_eq!(got.path, want.path);
    }
    assert!(
        yielded.iter().all(|e| e.path != sentinel),
        "sentinel must not be re-yielded after resume",
    );
}

#[test]
fn resumable_scan_when_sentinel_is_first_entry_yields_remainder() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    build_corpus(root);

    let all: Vec<_> = walk(root, &ScanOptions::default())
        .map(|r| r.expect("walk ok"))
        .collect();
    let first = &all[0];
    let cursor = ScanCursor {
        last_completed_path: first.path.clone(),
        files_seen: 1,
        bytes_seen: first.fingerprint.size_bytes,
    };

    let scan = ResumableScan::resume_from(ScanOptions::default(), cursor);
    let yielded: Vec<_> = scan.iter(root).map(|r| r.expect("ok")).collect();
    assert_eq!(yielded.len(), all.len() - 1);
    assert_eq!(yielded[0].path, all[1].path);
}

#[test]
fn resumable_scan_when_sentinel_is_last_entry_yields_nothing() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    build_corpus(root);

    let all: Vec<_> = walk(root, &ScanOptions::default())
        .map(|r| r.expect("walk ok"))
        .collect();
    let last = all.last().expect("non-empty").clone();
    let cursor = ScanCursor {
        last_completed_path: last.path.clone(),
        files_seen: u64::try_from(all.len()).expect("fits"),
        bytes_seen: all.iter().map(|e| e.fingerprint.size_bytes).sum(),
    };

    let scan = ResumableScan::resume_from(ScanOptions::default(), cursor);
    let yielded: Vec<_> = scan.iter(root).map(|r| r.expect("ok")).collect();
    assert!(yielded.is_empty(), "should be drained: {yielded:?}");
}

#[test]
fn resumable_scan_with_missing_sentinel_yields_nothing_and_reports_unanchored() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    build_corpus(root);

    let cursor = ScanCursor {
        last_completed_path: NormalizedPath::new(root.join("ghost.mp4")),
        files_seen: 99,
        bytes_seen: 99,
    };
    let scan = ResumableScan::resume_from(ScanOptions::default(), cursor);
    let mut iter = scan.iter(root);
    let yielded: Vec<_> = (&mut iter).map(|r| r.expect("ok")).collect();
    assert!(yielded.is_empty(), "ghost sentinel should not match");
    assert!(
        !iter.sentinel_seen(),
        "unanchored resume must surface sentinel_seen() == false",
    );
}

#[test]
fn interrupted_then_resumed_visits_each_path_exactly_once() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    build_corpus(root);
    let opts = ScanOptions::default();

    let mut first_round: Vec<NormalizedPath> = Vec::new();
    let mut progress = ScanProgress::new(None);
    {
        let scan = ResumableScan::new(opts.clone());
        for (i, entry) in scan.iter(root).enumerate() {
            let entry = entry.expect("walk ok");
            first_round.push(entry.path.clone());
            progress.record(&entry);
            if i + 1 == 3 {
                break;
            }
        }
    }
    assert_eq!(first_round.len(), 3);
    let crash_cursor = progress.cursor().expect("cursor after partial work");

    let mut second_round: Vec<NormalizedPath> = Vec::new();
    {
        let scan = ResumableScan::resume_from(opts, crash_cursor);
        for entry in scan.iter(root) {
            let entry = entry.expect("walk ok");
            second_round.push(entry.path.clone());
            progress.record(&entry);
        }
    }

    let total: Vec<NormalizedPath> = first_round
        .iter()
        .chain(second_round.iter())
        .cloned()
        .collect();
    let mut sorted = total.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        total.len(),
        "duplicate detected across resume boundary: {total:?}",
    );

    let baseline: Vec<NormalizedPath> = walk(root, &ScanOptions::default())
        .map(|r| r.expect("walk ok").path)
        .collect();
    let mut combined = total;
    combined.sort();
    let mut baseline_sorted = baseline.clone();
    baseline_sorted.sort();
    assert_eq!(combined, baseline_sorted);

    let total_bytes: u64 = walk(root, &ScanOptions::default())
        .map(|r| r.expect("ok").fingerprint.size_bytes)
        .sum();
    let expected_files = u64::try_from(baseline.len()).expect("file count fits in u64");
    assert_eq!(progress.files_seen(), expected_files);
    assert_eq!(progress.bytes_seen(), total_bytes);
}

#[test]
fn walkdir_emission_order_is_stable_across_runs() {
    let dir = tempdir().expect("tempdir");
    build_corpus(dir.path());

    let first: Vec<_> = walk(dir.path(), &ScanOptions::default())
        .map(|r| r.expect("ok").path)
        .collect();
    let second: Vec<_> = walk(dir.path(), &ScanOptions::default())
        .map(|r| r.expect("ok").path)
        .collect();
    assert_eq!(first, second);
}
