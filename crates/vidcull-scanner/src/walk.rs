use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

use vidcull_core::NormalizedPath;
use walkdir::{DirEntry, WalkDir};

use crate::fingerprint::FsFingerprint;
use crate::options::ScanOptions;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub path: NormalizedPath,
    pub fingerprint: FsFingerprint,
}

impl ScanEntry {
    fn from_dir_entry(entry: &DirEntry) -> io::Result<Self> {
        let meta = entry.metadata().map_err(walkdir_to_io)?;
        let mtime_ns = mtime_nanos(&meta)?;
        let inode = file_inode(&meta);
        Ok(Self {
            path: NormalizedPath::new(entry.path()),
            fingerprint: FsFingerprint {
                size_bytes: meta.len(),
                mtime_ns,
                inode,
            },
        })
    }
}

pub struct ScanIter {
    inner: walkdir::IntoIter,
    options: ScanOptions,
}

impl Iterator for ScanIter {
    type Item = io::Result<ScanEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let raw = self.inner.next()?;
            let entry = match raw {
                Ok(e) => e,
                Err(err) => return Some(Err(walkdir_to_io(err))),
            };
            if entry.file_type().is_dir() {
                if is_excluded_dir(&entry, &self.options) {
                    self.inner.skip_current_dir();
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if !has_whitelisted_extension(entry.path(), &self.options) {
                continue;
            }
            return Some(ScanEntry::from_dir_entry(&entry));
        }
    }
}

pub fn walk<P: AsRef<Path>>(root: P, options: &ScanOptions) -> ScanIter {
    let mut wd = WalkDir::new(root)
        .sort_by_file_name()
        .follow_links(options.follow_symlinks);
    if let Some(depth) = options.max_depth {
        wd = wd.max_depth(depth);
    }
    ScanIter {
        inner: wd.into_iter(),
        options: options.clone(),
    }
}

fn has_whitelisted_extension(path: &Path, options: &ScanOptions) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| options.accepts_extension(&ext.to_ascii_lowercase()))
}

fn is_excluded_dir(entry: &DirEntry, options: &ScanOptions) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| options.is_excluded_dir_name(name))
}

fn walkdir_to_io(err: walkdir::Error) -> io::Error {
    err.into_io_error()
        .unwrap_or_else(|| io::Error::other("walkdir cycle or non-io failure"))
}

fn mtime_nanos(meta: &fs::Metadata) -> io::Result<i128> {
    let modified = meta.modified()?;
    let nanos = match modified.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i128::try_from(d.as_nanos())
            .map_err(|_| io::Error::other("mtime exceeds i128 nanosecond range"))?,
        Err(e) => {
            let n = i128::try_from(e.duration().as_nanos())
                .map_err(|_| io::Error::other("pre-epoch mtime exceeds i128 nanosecond range"))?;
            -n
        }
    };
    Ok(nanos)
}

#[cfg(unix)]
fn file_inode(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn file_inode(_meta: &fs::Metadata) -> Option<u64> {
    None
}
