use std::path::Path;

use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, FileId};

use crate::content_hash::hash_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub file_id: FileId,
    pub size_bytes: i64,
    pub mtime_ns: i64,
}

impl CacheKey {
    #[must_use]
    pub const fn new(file_id: FileId, size_bytes: i64, mtime_ns: i64) -> Self {
        Self {
            file_id,
            size_bytes,
            mtime_ns,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedHash {
    pub hash: Blake3Hash,
    pub from_cache: bool,
}

pub trait ContentHashCache {
    fn lookup(&self, key: CacheKey) -> Result<Option<Blake3Hash>>;
    fn store(&mut self, key: CacheKey, hash: Blake3Hash) -> Result<()>;
}

pub fn hash_file_cached<C, P>(cache: &mut C, key: CacheKey, path: P) -> Result<CachedHash>
where
    C: ContentHashCache,
    P: AsRef<Path>,
{
    if let Some(hash) = cache.lookup(key)? {
        return Ok(CachedHash {
            hash,
            from_cache: true,
        });
    }
    let hash = hash_file(path.as_ref())?;
    cache.store(key, hash)?;
    Ok(CachedHash {
        hash,
        from_cache: false,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct MemoryCache {
        records: HashMap<FileId, (i64, i64, Blake3Hash)>,
    }

    impl ContentHashCache for MemoryCache {
        fn lookup(&self, key: CacheKey) -> Result<Option<Blake3Hash>> {
            match self.records.get(&key.file_id) {
                Some((size, mtime, hash)) if *size == key.size_bytes && *mtime == key.mtime_ns => {
                    Ok(Some(*hash))
                }
                _ => Ok(None),
            }
        }

        fn store(&mut self, key: CacheKey, hash: Blake3Hash) -> Result<()> {
            self.records
                .insert(key.file_id, (key.size_bytes, key.mtime_ns, hash));
            Ok(())
        }
    }

    fn write_tmp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.bin");
        std::fs::write(&path, bytes).expect("write");
        (dir, path)
    }

    #[test]
    fn first_call_hashes_and_populates_the_cache() {
        let (_dir, path) = write_tmp(b"hello world");
        let mut cache = MemoryCache::default();
        let key = CacheKey::new(FileId(1), 11, 1_000);
        let out = hash_file_cached(&mut cache, key, &path).expect("hash");
        assert!(!out.from_cache, "first call must be a miss");
        let direct = crate::content_hash::hash_reader(&b"hello world"[..]).expect("direct");
        assert_eq!(out.hash, direct);
    }

    #[test]
    fn second_call_with_same_key_serves_from_cache() {
        let (dir, path) = write_tmp(b"second-call payload");
        let mut cache = MemoryCache::default();
        let key = CacheKey::new(FileId(7), 19, 42);
        let first = hash_file_cached(&mut cache, key, &path).expect("first");
        assert!(!first.from_cache);

        drop(path);
        drop(dir);

        let stale_path = std::path::PathBuf::from("definitely-does-not-exist.bin");
        let second = hash_file_cached(&mut cache, key, &stale_path).expect("cached");
        assert!(second.from_cache, "second call must be a hit");
        assert_eq!(first.hash, second.hash);
    }

    #[test]
    fn cache_miss_on_mtime_change() {
        let (_dir, path) = write_tmp(b"mtime-change payload");
        let mut cache = MemoryCache::default();
        let original = CacheKey::new(FileId(3), 21, 100);
        hash_file_cached(&mut cache, original, &path).expect("first");

        let bumped = CacheKey::new(FileId(3), 21, 200);
        let out = hash_file_cached(&mut cache, bumped, &path).expect("after mtime change");
        assert!(!out.from_cache, "mtime change must invalidate the cache");
    }

    #[test]
    fn cache_miss_on_size_change() {
        let (_dir, path) = write_tmp(b"size-change payload");
        let mut cache = MemoryCache::default();
        let original = CacheKey::new(FileId(4), 21, 100);
        hash_file_cached(&mut cache, original, &path).expect("first");

        let resized = CacheKey::new(FileId(4), 22, 100);
        let out = hash_file_cached(&mut cache, resized, &path).expect("after size change");
        assert!(!out.from_cache, "size change must invalidate the cache");
    }
}
