use vidcull_core::Result;
use vidcull_core::types::NormalizedPath;

use vidcull_parser::{VideoMetadata, probe};

use crate::fingerprint::FsFingerprint;
use crate::walk::ScanEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedFile {
    pub path: NormalizedPath,
    pub fingerprint: FsFingerprint,
    pub video: VideoMetadata,
}

pub fn collect(entry: ScanEntry) -> Result<CollectedFile> {
    let video = probe(entry.path.to_native_path())?;
    Ok(CollectedFile {
        path: entry.path,
        fingerprint: entry.fingerprint,
        video,
    })
}
