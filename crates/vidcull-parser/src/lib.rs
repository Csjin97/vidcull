#![allow(missing_docs)]
#![forbid(unsafe_code)]

mod bounded;
pub mod cancel;
pub mod decode;
mod ebml;
pub mod fallback;
pub mod h264;
pub mod hevc;
pub mod mkv;
pub mod mkv_index;
pub mod mp4;
pub mod mp4_index;
mod probe;
pub mod sparse;
pub mod sparse_mkv;
pub mod sparse_mp4;

pub use cancel::Cancel;
pub use decode::{
    DecodedVideo, probe_and_decode_sparse, probe_and_decode_sparse_budgets,
    probe_and_decode_sparse_budgets_streaming, probe_and_decode_sparse_budgets_streaming_preparsed,
};
pub use fallback::full_grid_len;
pub use mp4::PreParsedMp4;
pub use probe::{ContainerKind, VideoMetadata, container_kind_from_path, probe};
