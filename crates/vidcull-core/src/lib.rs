#![allow(missing_docs)]
#![forbid(unsafe_code)]

pub mod binary;
pub mod error;
pub mod tracing_init;
pub mod types;

pub use binary::{decode, encode, encode_into};
pub use error::{Error, Result};
pub use tracing_init::init_tracing;
pub use types::{Blake3Hash, Codec, FileId, HASH_LEN, NormalizedPath, Resolution, VideoDuration};

pub const SPARSE_GRID_INTERVAL_MS: u64 = 2_500;

#[cfg(test)]
mod grid_const_tests {
    #[test]
    fn sparse_grid_interval_is_pinned_at_2500ms() {
        assert_eq!(super::SPARSE_GRID_INTERVAL_MS, 2_500);
    }
}
