use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FsFingerprint {
    pub size_bytes: u64,
    pub mtime_ns: i128,
    pub inode: Option<u64>,
}

impl FsFingerprint {
    #[must_use]
    pub const fn new(size_bytes: u64, mtime_ns: i128, inode: Option<u64>) -> Self {
        Self {
            size_bytes,
            mtime_ns,
            inode,
        }
    }

    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        if self.size_bytes != other.size_bytes || self.mtime_ns != other.mtime_ns {
            return false;
        }
        match (self.inode, other.inode) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(size: u64, mtime: i128, inode: Option<u64>) -> FsFingerprint {
        FsFingerprint::new(size, mtime, inode)
    }

    #[test]
    fn equal_triples_match() {
        assert!(fp(10, 100, Some(7)).matches(&fp(10, 100, Some(7))));
    }

    #[test]
    fn size_mismatch_does_not_match() {
        assert!(!fp(10, 100, Some(7)).matches(&fp(11, 100, Some(7))));
    }

    #[test]
    fn mtime_mismatch_does_not_match() {
        assert!(!fp(10, 100, Some(7)).matches(&fp(10, 200, Some(7))));
    }

    #[test]
    fn inode_mismatch_when_both_present_does_not_match() {
        assert!(!fp(10, 100, Some(7)).matches(&fp(10, 100, Some(8))));
    }

    #[test]
    fn inode_none_on_either_side_is_ignored() {
        assert!(fp(10, 100, None).matches(&fp(10, 100, Some(7))));
        assert!(fp(10, 100, Some(7)).matches(&fp(10, 100, None)));
        assert!(fp(10, 100, None).matches(&fp(10, 100, None)));
    }

    #[test]
    fn round_trip_via_postcard() {
        let original = fp(123_456_789, -42_000_000_000, Some(0xDEAD_BEEF));
        let bytes = postcard::to_allocvec(&original).expect("encode");
        let decoded: FsFingerprint = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(original, decoded);
    }
}
