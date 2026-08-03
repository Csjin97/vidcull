use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct FileId(pub i64);

impl FileId {
    pub const UNASSIGNED: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for FileId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<FileId> for i64 {
    fn from(value: FileId) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_postcard() {
        let original = FileId(42);
        let bytes = postcard::to_allocvec(&original).expect("encode");
        let decoded: FileId = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(original, decoded);
    }

    #[test]
    fn ordering_matches_inner_i64() {
        let mut ids = vec![FileId(5), FileId(1), FileId(3)];
        ids.sort();
        assert_eq!(ids, vec![FileId(1), FileId(3), FileId(5)]);
    }

    #[test]
    fn transparent_serialization_matches_raw_i64() {
        let id = FileId(7);
        let id_bytes = postcard::to_allocvec(&id).expect("encode FileId");
        let raw_bytes = postcard::to_allocvec(&7_i64).expect("encode i64");
        assert_eq!(
            id_bytes, raw_bytes,
            "FileId must be wire-compatible with its inner i64 thanks to serde(transparent)"
        );
    }

    #[test]
    fn unassigned_sentinel_is_zero() {
        assert_eq!(FileId::UNASSIGNED.get(), 0);
    }

    #[test]
    fn from_i64_creates_file_id() {
        let id = FileId::from(12345_i64);
        assert_eq!(id.get(), 12345);
    }

    #[test]
    fn into_i64_extracts_inner() {
        let id = FileId(54321);
        let raw: i64 = i64::from(id);
        assert_eq!(raw, 54321);
    }
}
