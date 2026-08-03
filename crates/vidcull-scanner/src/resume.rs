use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use vidcull_core::types::NormalizedPath;
use vidcull_core::{Error, Result};

use crate::options::ScanOptions;
use crate::walk::{ScanEntry, ScanIter, walk};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCursor {
    pub last_completed_path: NormalizedPath,
    pub files_seen: u64,
    pub bytes_seen: u64,
}

impl ScanCursor {
    pub fn to_blob(&self) -> Result<Vec<u8>> {
        postcard::to_allocvec(self).map_err(Error::from)
    }

    pub fn from_blob(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).map_err(Error::from)
    }
}

#[derive(Debug, Clone)]
pub struct ScanProgress {
    files_seen: u64,
    bytes_seen: u64,
    last_completed_path: Option<NormalizedPath>,
}

impl ScanProgress {
    #[must_use]
    pub fn new(prior: Option<&ScanCursor>) -> Self {
        match prior {
            Some(c) => Self {
                files_seen: c.files_seen,
                bytes_seen: c.bytes_seen,
                last_completed_path: Some(c.last_completed_path.clone()),
            },
            None => Self {
                files_seen: 0,
                bytes_seen: 0,
                last_completed_path: None,
            },
        }
    }

    pub fn record(&mut self, entry: &ScanEntry) {
        self.files_seen += 1;
        self.bytes_seen += entry.fingerprint.size_bytes;
        self.last_completed_path = Some(entry.path.clone());
    }

    #[must_use]
    pub fn files_seen(&self) -> u64 {
        self.files_seen
    }

    #[must_use]
    pub fn bytes_seen(&self) -> u64 {
        self.bytes_seen
    }

    #[must_use]
    pub fn cursor(&self) -> Option<ScanCursor> {
        self.last_completed_path.as_ref().map(|p| ScanCursor {
            last_completed_path: p.clone(),
            files_seen: self.files_seen,
            bytes_seen: self.bytes_seen,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResumableScan {
    options: ScanOptions,
    cursor: Option<ScanCursor>,
}

impl ResumableScan {
    #[must_use]
    pub fn new(options: ScanOptions) -> Self {
        Self {
            options,
            cursor: None,
        }
    }

    #[must_use]
    pub fn resume_from(options: ScanOptions, cursor: ScanCursor) -> Self {
        Self {
            options,
            cursor: Some(cursor),
        }
    }

    #[must_use]
    pub fn cursor(&self) -> Option<&ScanCursor> {
        self.cursor.as_ref()
    }

    pub fn iter<P: AsRef<Path>>(&self, root: P) -> ResumeIter {
        ResumeIter {
            inner: walk(root, &self.options),
            sentinel: self.cursor.as_ref().map(|c| c.last_completed_path.clone()),
            sentinel_seen: self.cursor.is_none(),
        }
    }
}

pub struct ResumeIter {
    inner: ScanIter,
    sentinel: Option<NormalizedPath>,
    sentinel_seen: bool,
}

impl ResumeIter {
    #[must_use]
    pub fn sentinel_seen(&self) -> bool {
        self.sentinel_seen
    }
}

impl Iterator for ResumeIter {
    type Item = io::Result<ScanEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.inner.next()? {
                Ok(e) => e,
                Err(err) => return Some(Err(err)),
            };

            if self.sentinel_seen {
                return Some(Ok(entry));
            }

            if let Some(s) = &self.sentinel
                && entry.path == *s
            {
                self.sentinel_seen = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::FsFingerprint;

    fn entry(path: &str, size: u64) -> ScanEntry {
        ScanEntry {
            path: NormalizedPath::new(path),
            fingerprint: FsFingerprint::new(size, 0, None),
        }
    }

    #[test]
    fn progress_record_advances_sentinel() {
        let mut p = ScanProgress::new(None);
        p.record(&entry("/a.mp4", 10));
        p.record(&entry("/b.mp4", 32));
        let cur = p.cursor().expect("cursor");
        assert_eq!(cur.last_completed_path, NormalizedPath::new("/b.mp4"));
        assert_eq!(cur.files_seen, 2);
        assert_eq!(cur.bytes_seen, 42);
    }

    #[test]
    fn progress_seeded_from_cursor_keeps_running_totals() {
        let prior = ScanCursor {
            last_completed_path: NormalizedPath::new("/x.mp4"),
            files_seen: 5,
            bytes_seen: 1024,
        };
        let mut p = ScanProgress::new(Some(&prior));
        assert_eq!(p.files_seen(), 5);
        assert_eq!(p.bytes_seen(), 1024);
        p.record(&entry("/y.mp4", 100));
        assert_eq!(p.files_seen(), 6);
        assert_eq!(p.bytes_seen(), 1124);
        assert_eq!(
            p.cursor().expect("cursor").last_completed_path,
            NormalizedPath::new("/y.mp4"),
        );
    }

    #[test]
    fn cursor_postcard_round_trip() {
        let c = ScanCursor {
            last_completed_path: NormalizedPath::new("/a/b/c.mp4"),
            files_seen: 42,
            bytes_seen: 100_000,
        };
        let bytes = c.to_blob().expect("encode");
        let decoded = ScanCursor::from_blob(&bytes).expect("decode");
        assert_eq!(c, decoded);
    }

    #[test]
    fn progress_cursor_is_none_before_first_record() {
        let p = ScanProgress::new(None);
        assert!(p.cursor().is_none());
    }

    #[test]
    fn from_blob_rejects_corrupt_data() {
        let res = ScanCursor::from_blob(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(res.is_err());
    }
}
