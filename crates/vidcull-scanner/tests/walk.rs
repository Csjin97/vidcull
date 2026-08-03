use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use filetime::{FileTime, set_file_mtime};
use tempfile::tempdir;
use vidcull_core::NormalizedPath;
use vidcull_scanner::{FsFingerprint, ScanEntry, ScanOptions, diff, walk};

fn touch(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    let mut f = File::create(path).expect("create file");
    f.write_all(body).expect("write body");
    f.sync_all().expect("sync");
}

fn set_mtime_secs(path: &Path, seconds_from_epoch: i64) {
    let t = FileTime::from_unix_time(seconds_from_epoch, 0);
    set_file_mtime(path, t).expect("set mtime");
}

fn collect(root: &Path, opts: &ScanOptions) -> Vec<ScanEntry> {
    walk(root, opts)
        .collect::<Result<Vec<_>, _>>()
        .expect("walk should succeed on a controlled tempdir")
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .expect("path has file name")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn walk_yields_only_whitelisted_video_extensions() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    touch(&root.join("video.mp4"), b"a");
    touch(&root.join("clip.mkv"), b"a");
    touch(&root.join("readme.txt"), b"a");
    touch(&root.join("cover.jpg"), b"a");
    touch(&root.join("no_extension"), b"a");

    let entries = collect(root, &ScanOptions::default());
    let names: Vec<_> = entries.iter().map(|e| basename(e.path.as_str())).collect();
    assert_eq!(names, vec!["clip.mkv", "video.mp4"]);
}

#[test]
fn extension_whitelist_is_case_insensitive() {
    let dir = tempdir().expect("tempdir");
    touch(&dir.path().join("UPPER.MP4"), b"x");
    touch(&dir.path().join("Mixed.MkV"), b"x");
    touch(&dir.path().join("skip.JPG"), b"x");

    let entries = collect(dir.path(), &ScanOptions::default());
    assert_eq!(
        entries.len(),
        2,
        "case-insensitive match failed: {entries:?}"
    );
}

#[test]
fn extension_whitelist_can_be_customised() {
    let dir = tempdir().expect("tempdir");
    touch(&dir.path().join("a.mp4"), b"x");
    touch(&dir.path().join("b.weird"), b"x");

    let opts = ScanOptions::default().with_extensions(["weird"]);
    let entries = collect(dir.path(), &opts);
    let names: Vec<_> = entries.iter().map(|e| basename(e.path.as_str())).collect();
    assert_eq!(names, vec!["b.weird"]);
}

#[test]
fn extension_whitelist_strips_leading_dot() {
    let dir = tempdir().expect("tempdir");
    touch(&dir.path().join("a.mp4"), b"x");

    let opts = ScanOptions::default().with_extensions([".mp4"]);
    let entries = collect(dir.path(), &opts);
    assert_eq!(entries.len(), 1);
}

#[test]
fn walk_recurses_into_subdirectories() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    touch(&root.join("a/one.mp4"), b"1");
    touch(&root.join("a/b/two.mp4"), b"22");
    touch(&root.join("c/three.mkv"), b"333");

    let entries = collect(root, &ScanOptions::default());
    assert_eq!(entries.len(), 3, "expected 3 videos, got {entries:?}");
}

#[test]
fn walk_returns_siblings_in_lexical_order_for_hdd_friendliness() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    touch(&root.join("z.mp4"), b"x");
    touch(&root.join("a.mp4"), b"x");
    touch(&root.join("m.mp4"), b"x");

    let entries = collect(root, &ScanOptions::default());
    let names: Vec<_> = entries.iter().map(|e| basename(e.path.as_str())).collect();
    assert_eq!(names, vec!["a.mp4", "m.mp4", "z.mp4"]);
}

#[test]
fn walk_normalizes_paths_to_forward_slashes() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    touch(&root.join("nested/dir/clip.mp4"), b"x");

    let entries = collect(root, &ScanOptions::default());
    assert_eq!(entries.len(), 1);
    let p = entries[0].path.as_str();
    assert!(!p.contains('\\'), "expected forward slashes, got {p}");
    assert!(p.ends_with("nested/dir/clip.mp4"), "got {p}");
}

#[test]
fn walk_respects_max_depth() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    touch(&root.join("top.mp4"), b"x");
    touch(&root.join("level1/deep.mp4"), b"x");
    touch(&root.join("level1/level2/deeper.mp4"), b"x");

    let opts = ScanOptions::default().with_max_depth(1);
    let entries = collect(root, &opts);
    let names: Vec<_> = entries.iter().map(|e| basename(e.path.as_str())).collect();
    assert_eq!(names, vec!["top.mp4"]);
}

#[test]
fn walk_skips_directories_themselves() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    fs::create_dir(root.join("looks-like-a-video.mp4")).expect("mkdir");
    touch(&root.join("real.mp4"), b"x");

    let entries = collect(root, &ScanOptions::default());
    let names: Vec<_> = entries.iter().map(|e| basename(e.path.as_str())).collect();
    assert_eq!(names, vec!["real.mp4"]);
}

#[test]
fn fingerprint_changes_when_size_changes() {
    let dir = tempdir().expect("tempdir");
    let p = dir.path().join("a.mp4");
    touch(&p, b"hello");
    set_mtime_secs(&p, 1_000_000_000);
    let opts = ScanOptions::default();
    let before = collect(dir.path(), &opts)[0].fingerprint;

    {
        let mut f = File::options().append(true).open(&p).expect("reopen");
        f.write_all(b" world").expect("append");
        f.sync_all().expect("sync");
    }
    set_mtime_secs(&p, 2_000_000_000);

    let after = collect(dir.path(), &opts)[0].fingerprint;
    assert_ne!(before.size_bytes, after.size_bytes);
    assert!(!before.matches(&after));
}

#[test]
fn fingerprint_changes_when_mtime_changes() {
    let dir = tempdir().expect("tempdir");
    let p = dir.path().join("a.mp4");
    touch(&p, b"static");
    set_mtime_secs(&p, 1_000_000_000);
    let before = collect(dir.path(), &ScanOptions::default())[0].fingerprint;

    set_mtime_secs(&p, 1_000_001_000);
    let after = collect(dir.path(), &ScanOptions::default())[0].fingerprint;
    assert_eq!(before.size_bytes, after.size_bytes);
    assert_ne!(before.mtime_ns, after.mtime_ns);
    assert!(!before.matches(&after));
}

#[test]
fn fingerprint_unchanged_when_nothing_mutates() {
    let dir = tempdir().expect("tempdir");
    let p = dir.path().join("a.mp4");
    touch(&p, b"static");
    set_mtime_secs(&p, 1_000_000_000);

    let first = collect(dir.path(), &ScanOptions::default())[0].fingerprint;
    let second = collect(dir.path(), &ScanOptions::default())[0].fingerprint;
    assert!(
        first.matches(&second),
        "identical reads must produce matching fingerprints"
    );
}

#[test]
fn diff_classifies_added_modified_removed_unchanged() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    touch(&root.join("kept.mp4"), b"same");
    set_mtime_secs(&root.join("kept.mp4"), 1_000_000_000);
    touch(&root.join("changed.mp4"), b"old");
    set_mtime_secs(&root.join("changed.mp4"), 1_000_000_000);

    let initial = collect(root, &ScanOptions::default());
    let mut previous: BTreeMap<NormalizedPath, FsFingerprint> = BTreeMap::new();
    for e in &initial {
        previous.insert(e.path.clone(), e.fingerprint);
    }
    let gone_path = NormalizedPath::new(root.join("gone.mp4"));
    previous.insert(
        gone_path.clone(),
        FsFingerprint::new(7, 999_999_999_000_000_000_i128, Some(123)),
    );

    touch(&root.join("brand_new.mp4"), b"hello");
    {
        let mut f = File::options()
            .write(true)
            .truncate(true)
            .open(root.join("changed.mp4"))
            .expect("reopen");
        f.write_all(b"NEW LARGER CONTENT").expect("write");
        f.sync_all().expect("sync");
    }
    set_mtime_secs(&root.join("changed.mp4"), 1_000_500_000);

    let current = collect(root, &ScanOptions::default());
    let changes = diff(previous, current);

    let added_names: Vec<_> = changes
        .added
        .iter()
        .map(|e| basename(e.path.as_str()))
        .collect();
    assert_eq!(added_names, vec!["brand_new.mp4"]);

    let modified_names: Vec<_> = changes
        .modified
        .iter()
        .map(|m| basename(m.current.path.as_str()))
        .collect();
    assert_eq!(modified_names, vec!["changed.mp4"]);

    let removed_names: Vec<_> = changes
        .removed
        .iter()
        .map(|p| basename(p.as_str()))
        .collect();
    assert_eq!(removed_names, vec!["gone.mp4"]);

    let unchanged_names: Vec<_> = changes
        .unchanged
        .iter()
        .map(|p| basename(p.as_str()))
        .collect();
    assert_eq!(unchanged_names, vec!["kept.mp4"]);

    assert_eq!(changes.total(), 4);
}

#[test]
fn diff_handles_empty_previous_snapshot() {
    let dir = tempdir().expect("tempdir");
    touch(&dir.path().join("a.mp4"), b"a");
    touch(&dir.path().join("b.mkv"), b"b");

    let current = collect(dir.path(), &ScanOptions::default());
    let changes = diff(BTreeMap::new(), current);
    assert_eq!(changes.added.len(), 2);
    assert!(changes.modified.is_empty());
    assert!(changes.removed.is_empty());
    assert!(changes.unchanged.is_empty());
}

#[test]
fn diff_handles_completely_removed_tree() {
    let mut previous: BTreeMap<NormalizedPath, FsFingerprint> = BTreeMap::new();
    previous.insert(
        NormalizedPath::new("/a/b/c.mp4"),
        FsFingerprint::new(10, 100, Some(1)),
    );
    previous.insert(
        NormalizedPath::new("/x/y/z.mp4"),
        FsFingerprint::new(20, 200, Some(2)),
    );

    let empty: Vec<ScanEntry> = Vec::new();
    let changes = diff(previous, empty);
    assert_eq!(changes.removed.len(), 2);
    assert!(changes.added.is_empty());
    assert!(changes.unchanged.is_empty());
}
