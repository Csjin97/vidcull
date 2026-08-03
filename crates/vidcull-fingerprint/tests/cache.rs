use std::path::Path;

use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, FileId, NormalizedPath};
use vidcull_db::open_in_memory;
use vidcull_db::repo::{FilesRepo, NewFile};
use vidcull_fingerprint::cache::{CacheKey, ContentHashCache, hash_file_cached};
use vidcull_fingerprint::content_hash::hash_reader;

struct FilesRepoCache<'a> {
    repo: FilesRepo<'a>,
}

impl ContentHashCache for FilesRepoCache<'_> {
    fn lookup(&self, key: CacheKey) -> Result<Option<Blake3Hash>> {
        let Some(rec) = self.repo.get(key.file_id)? else {
            return Ok(None);
        };
        if rec.size_bytes != key.size_bytes || rec.mtime_ns != key.mtime_ns {
            return Ok(None);
        }
        Ok(rec.content_hash)
    }

    fn store(&mut self, key: CacheKey, hash: Blake3Hash) -> Result<()> {
        self.repo.set_content_hash(key.file_id, hash)
    }
}

fn fresh_row(path: &Path, size: i64, mtime: i64) -> NewFile {
    NewFile {
        path: NormalizedPath::new(path.to_string_lossy().into_owned()),
        size_bytes: size,
        mtime_ns: mtime,
        inode: None,
        content_hash: None,
        codec: None,
        container: None,
        duration: None,
        fps_x1000: None,
        bitrate_bps: None,
        resolution: None,
        first_seen_at: 0,
        last_seen_at: 0,
        laplacian_variance: None,
        dct_energy: None,
        bpp: None,
        encoder_tags: None,
    }
}

#[test]
fn fresh_row_misses_then_writes_back_to_files_content_hash() {
    let mut db = open_in_memory().expect("open db");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("video.mp4");
    std::fs::write(&path, b"raw bytes").expect("write");
    let size = i64::try_from(std::fs::metadata(&path).unwrap().len()).unwrap();

    let id: FileId = db
        .transaction(|conn| {
            let files = FilesRepo::new(conn);
            files.insert(&fresh_row(&path, size, 1_000))
        })
        .expect("insert");

    let outcome = db
        .transaction(|conn| {
            let mut cache = FilesRepoCache {
                repo: FilesRepo::new(conn),
            };
            hash_file_cached(&mut cache, CacheKey::new(id, size, 1_000), &path)
        })
        .expect("hash");

    assert!(!outcome.from_cache, "first call must miss");
    let expected = hash_reader(&b"raw bytes"[..]).expect("oracle hash");
    assert_eq!(outcome.hash, expected);

    let row = db
        .transaction(|conn| FilesRepo::new(conn).get(id))
        .expect("read back")
        .expect("row exists");
    assert_eq!(row.content_hash, Some(expected));
}

#[test]
fn second_call_serves_from_files_without_touching_disk() {
    let mut db = open_in_memory().expect("open db");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("video.mp4");
    let body = b"once-only payload";
    std::fs::write(&path, body).expect("write");
    let size = i64::try_from(body.len()).unwrap();

    let id: FileId = db
        .transaction(|conn| {
            let files = FilesRepo::new(conn);
            files.insert(&fresh_row(&path, size, 2_000))
        })
        .expect("insert");

    let first = db
        .transaction(|conn| {
            let mut cache = FilesRepoCache {
                repo: FilesRepo::new(conn),
            };
            hash_file_cached(&mut cache, CacheKey::new(id, size, 2_000), &path)
        })
        .expect("first");
    assert!(!first.from_cache);

    std::fs::remove_file(&path).expect("remove");
    assert!(!path.exists());

    let second = db
        .transaction(|conn| {
            let mut cache = FilesRepoCache {
                repo: FilesRepo::new(conn),
            };
            hash_file_cached(&mut cache, CacheKey::new(id, size, 2_000), &path)
        })
        .expect("second call must succeed from cache alone");

    assert!(second.from_cache, "second call must be a cache hit");
    assert_eq!(first.hash, second.hash);
}

#[test]
fn mtime_or_size_change_invalidates_the_cache() {
    let mut db = open_in_memory().expect("open db");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("video.mp4");
    std::fs::write(&path, b"baseline").expect("write");
    let size = i64::try_from(std::fs::metadata(&path).unwrap().len()).unwrap();

    let id: FileId = db
        .transaction(|conn| FilesRepo::new(conn).insert(&fresh_row(&path, size, 3_000)))
        .expect("insert");

    let primed = db
        .transaction(|conn| {
            let mut cache = FilesRepoCache {
                repo: FilesRepo::new(conn),
            };
            hash_file_cached(&mut cache, CacheKey::new(id, size, 3_000), &path)
        })
        .expect("first");
    assert!(!primed.from_cache);

    let after_mtime = db
        .transaction(|conn| {
            let mut cache = FilesRepoCache {
                repo: FilesRepo::new(conn),
            };
            hash_file_cached(&mut cache, CacheKey::new(id, size, 4_000), &path)
        })
        .expect("mtime-bumped");
    assert!(
        !after_mtime.from_cache,
        "mtime change must invalidate the cache"
    );

    let after_size = db
        .transaction(|conn| {
            let mut cache = FilesRepoCache {
                repo: FilesRepo::new(conn),
            };
            hash_file_cached(&mut cache, CacheKey::new(id, size + 1, 4_000), &path)
        })
        .expect("size-bumped");
    assert!(
        !after_size.from_cache,
        "size change must invalidate the cache"
    );
}
