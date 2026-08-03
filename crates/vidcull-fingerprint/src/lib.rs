#![allow(missing_docs)]
#![forbid(unsafe_code)]

pub mod cache;
pub mod content_hash;
pub mod format;
pub mod normalize;
mod simd;
pub mod tier1;
pub mod tier2;

pub use cache::{CacheKey, CachedHash, ContentHashCache, hash_file_cached};
pub use content_hash::{
    CHUNK_SIZE, hash_file, hash_file_cancellable, hash_reader, hash_reader_cancellable,
};
pub use format::{FORMAT_VERSION, Fingerprint, HEADER_LEN, Header, MAGIC, PayloadKind};
pub use normalize::{DEFAULT_BAR_LIMIT, trim_uniform_borders};
pub use tier1::{
    GopSignature, GrayFrame, Tier1Builder, Tier1Fingerprint, build_tier1, dct_energy,
    hamming_distance, hamming_distance_batch, laplacian_variance,
};
pub use tier2::{
    SEQUENCE_STABILITY_THRESHOLD, SceneHash, TIER2_BUDGET_BYTES, Tier2Builder, Tier2Fingerprint,
    TimedFrame, build_tier2, sequence_similarity,
};
