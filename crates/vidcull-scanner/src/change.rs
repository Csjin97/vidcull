use std::collections::BTreeMap;

use vidcull_core::NormalizedPath;

use crate::fingerprint::FsFingerprint;
use crate::walk::ScanEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifiedEntry {
    pub previous: FsFingerprint,
    pub current: ScanEntry,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub added: Vec<ScanEntry>,
    pub modified: Vec<ModifiedEntry>,
    pub removed: Vec<NormalizedPath>,
    pub unchanged: Vec<NormalizedPath>,
}

impl ChangeSet {
    #[must_use]
    pub fn total(&self) -> usize {
        self.added.len() + self.modified.len() + self.removed.len() + self.unchanged.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.removed.is_empty()
            && self.unchanged.is_empty()
    }
}

pub fn diff<I>(mut previous: BTreeMap<NormalizedPath, FsFingerprint>, current: I) -> ChangeSet
where
    I: IntoIterator<Item = ScanEntry>,
{
    let mut out = ChangeSet::default();

    for entry in current {
        match previous.remove(&entry.path) {
            None => out.added.push(entry),
            Some(prev) => {
                if prev.matches(&entry.fingerprint) {
                    out.unchanged.push(entry.path);
                } else {
                    out.modified.push(ModifiedEntry {
                        previous: prev,
                        current: entry,
                    });
                }
            }
        }
    }

    for path in previous.into_keys() {
        out.removed.push(path);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, mtime: i128, inode: Option<u64>) -> ScanEntry {
        ScanEntry {
            path: NormalizedPath::new(path),
            fingerprint: FsFingerprint::new(size, mtime, inode),
        }
    }

    #[test]
    fn empty_inputs_yield_empty_changeset() {
        let cs = diff(BTreeMap::new(), Vec::<ScanEntry>::new());
        assert!(cs.is_empty());
        assert_eq!(cs.total(), 0);
    }

    #[test]
    fn fresh_entries_with_no_previous_become_added() {
        let cs = diff(BTreeMap::new(), vec![entry("/a.mp4", 1, 1, Some(1))]);
        assert_eq!(cs.added.len(), 1);
        assert!(cs.modified.is_empty() && cs.removed.is_empty() && cs.unchanged.is_empty());
    }

    #[test]
    fn matching_previous_becomes_unchanged() {
        let mut previous = BTreeMap::new();
        previous.insert(
            NormalizedPath::new("/a.mp4"),
            FsFingerprint::new(1, 1, Some(1)),
        );
        let cs = diff(previous, vec![entry("/a.mp4", 1, 1, Some(1))]);
        assert_eq!(cs.unchanged, vec![NormalizedPath::new("/a.mp4")]);
        assert!(cs.added.is_empty() && cs.modified.is_empty() && cs.removed.is_empty());
    }

    #[test]
    fn divergent_fingerprint_becomes_modified() {
        let mut previous = BTreeMap::new();
        previous.insert(
            NormalizedPath::new("/a.mp4"),
            FsFingerprint::new(1, 1, Some(1)),
        );
        let cs = diff(previous, vec![entry("/a.mp4", 999, 1, Some(1))]);
        assert_eq!(cs.modified.len(), 1);
        assert_eq!(cs.modified[0].previous.size_bytes, 1);
        assert_eq!(cs.modified[0].current.fingerprint.size_bytes, 999);
    }

    #[test]
    fn absent_current_becomes_removed() {
        let mut previous = BTreeMap::new();
        previous.insert(
            NormalizedPath::new("/a.mp4"),
            FsFingerprint::new(1, 1, Some(1)),
        );
        previous.insert(
            NormalizedPath::new("/b.mp4"),
            FsFingerprint::new(2, 2, Some(2)),
        );
        let cs = diff(previous, Vec::<ScanEntry>::new());
        assert_eq!(cs.removed.len(), 2);
    }

    #[test]
    fn all_four_buckets_in_one_diff() {
        let mut previous = BTreeMap::new();
        previous.insert(
            NormalizedPath::new("/unchanged.mp4"),
            FsFingerprint::new(100, 1000, Some(1)),
        );
        previous.insert(
            NormalizedPath::new("/modified.mp4"),
            FsFingerprint::new(200, 2000, Some(2)),
        );
        previous.insert(
            NormalizedPath::new("/removed.mp4"),
            FsFingerprint::new(300, 3000, Some(3)),
        );

        let current = vec![
            entry("/unchanged.mp4", 100, 1000, Some(1)),
            entry("/modified.mp4", 999, 2000, Some(2)),
            entry("/added.mp4", 400, 4000, Some(4)),
        ];

        let cs = diff(previous, current);

        assert_eq!(cs.added, vec![entry("/added.mp4", 400, 4000, Some(4))]);

        assert_eq!(cs.modified.len(), 1);
        assert_eq!(cs.modified[0].previous.size_bytes, 200);
        assert_eq!(
            cs.modified[0].current.path,
            NormalizedPath::new("/modified.mp4")
        );
        assert_eq!(cs.modified[0].current.fingerprint.size_bytes, 999);

        assert_eq!(cs.removed, vec![NormalizedPath::new("/removed.mp4")]);
        assert_eq!(cs.unchanged, vec![NormalizedPath::new("/unchanged.mp4")]);
    }

    #[test]
    fn change_set_total_equals_sum_of_buckets() {
        let mut previous = BTreeMap::new();
        previous.insert(
            NormalizedPath::new("/unchanged.mp4"),
            FsFingerprint::new(10, 10, None),
        );
        previous.insert(
            NormalizedPath::new("/modified.mp4"),
            FsFingerprint::new(20, 20, None),
        );
        previous.insert(
            NormalizedPath::new("/removed.mp4"),
            FsFingerprint::new(30, 30, None),
        );

        let current = vec![
            entry("/unchanged.mp4", 10, 10, None),
            entry("/modified.mp4", 25, 20, None),
            entry("/added.mp4", 40, 40, None),
        ];

        let cs = diff(previous, current);
        let sum = cs.added.len() + cs.modified.len() + cs.removed.len() + cs.unchanged.len();
        assert_eq!(cs.total(), sum);
        assert_eq!(cs.total(), 4);
    }
}
