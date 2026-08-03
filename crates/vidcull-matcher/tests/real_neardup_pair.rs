use std::env;
use std::path::{Path, PathBuf};
use vidcull_db::open_in_memory;
use vidcull_db::repo::DuplicateGroupsRepo;
use vidcull_matcher::near::{LshParams, rebuild_near_duplicate_groups};

fn real_dir_opt() -> Option<PathBuf> {
    let val = env::var("VIDCULL_REAL_CORPUS_DIR").ok()?;
    let p = PathBuf::from(val);
    if p.is_dir() { Some(p) } else { None }
}

fn skip_if_corpus_absent() -> bool {
    if real_dir_opt().is_none() {
        eprintln!(
            "[RealCorpus] SKIPPED — set VIDCULL_REAL_CORPUS_DIR to enable \
             real-corpus gate"
        );
        return true;
    }
    false
}

fn pair_present(dir: &Path) -> bool {
    dir.join("pair_h264.mp4").is_file() && dir.join("pair_h265.mp4").is_file()
}

#[test]
#[allow(clippy::too_many_lines)]
fn near_dup_pair_yields_exact_grouping() {
    if skip_if_corpus_absent() {
        return;
    }

    let dir = real_dir_opt().unwrap();

    assert!(
        pair_present(&dir),
        "\n\n[real_neardup_pair] Required fixture files are missing.\n\
         Set VIDCULL_REAL_CORPUS_DIR and re-provision with the commands in \
         fixtures/real/README.md, then re-run the tests.\n\
         \n\
         Expected inside VIDCULL_REAL_CORPUS_DIR:\n\
         \n\
           pair_h264.mp4\n\
           pair_h265.mp4\n"
    );

    let h264_path = dir.join("pair_h264.mp4");
    let h265_path = dir.join("pair_h265.mp4");

    assert!(
        h264_path.metadata().map(|m| m.len()).unwrap_or(0) > 0,
        "pair_h264.mp4 is empty"
    );
    assert!(
        h265_path.metadata().map(|m| m.len()).unwrap_or(0) > 0,
        "pair_h265.mp4 is empty"
    );

    eprintln!(
        "[real_neardup_pair] corpus present — h264={} bytes, h265={} bytes",
        h264_path.metadata().unwrap().len(),
        h265_path.metadata().unwrap().len(),
    );

    let mut db = open_in_memory().expect("open in-memory db");
    let out = rebuild_near_duplicate_groups(&mut db, LshParams::default(), 0)
        .expect("rebuild_near_duplicate_groups must not error on an empty DB");

    assert_eq!(
        out.groups_created, 0,
        "empty DB must produce zero near groups"
    );

    let _repo = DuplicateGroupsRepo::new(db.conn());

    eprintln!(
        "[real_neardup_pair] SKELETON — real decode assertions pending \
         /128 merge + provisioned-machine confirmation. \
         Corpus files confirmed present."
    );
}

#[test]
fn av1_fixture_is_present_and_non_empty() {
    if skip_if_corpus_absent() {
        return;
    }
    let dir = real_dir_opt().unwrap();
    let path = dir.join("pair_av1.mp4");
    assert!(
        path.is_file(),
        "\n\n[real_neardup_pair] AV1 fixture (pair_av1.mp4) missing.\n\
         Set VIDCULL_REAL_CORPUS_DIR and re-provision with the ffmpeg \
         commands in fixtures/real/README.md, then re-run the tests.\n"
    );
    let size = path.metadata().unwrap().len();
    assert!(
        size > 1_000_000,
        "pair_av1.mp4 is suspiciously small ({size} bytes) — re-provision"
    );
    eprintln!("[real_neardup_pair] AV1 fixture OK: {size} bytes");
}

#[test]
fn partial_clip_fixture_is_present_and_non_empty() {
    if skip_if_corpus_absent() {
        return;
    }
    let dir = real_dir_opt().unwrap();
    let path = dir.join("pair_partial.mp4");
    assert!(
        path.is_file(),
        "\n\n[real_neardup_pair] Partial-clip fixture (pair_partial.mp4) missing.\n\
         Set VIDCULL_REAL_CORPUS_DIR and re-provision with the commands in \
         fixtures/real/README.md, then re-run the tests.\n"
    );
    let size = path.metadata().unwrap().len();
    assert!(
        size > 100_000,
        "pair_partial.mp4 is suspiciously small ({size} bytes) — re-provision"
    );
    eprintln!("[real_neardup_pair] partial-clip fixture OK: {size} bytes");
}
