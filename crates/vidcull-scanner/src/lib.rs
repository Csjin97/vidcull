#![allow(missing_docs)]
#![forbid(unsafe_code)]

mod change;
mod fingerprint;
pub mod metadata;
mod options;
mod resume;
mod walk;

pub use change::{ChangeSet, ModifiedEntry, diff};
pub use fingerprint::FsFingerprint;
pub use metadata::{CollectedFile, collect};
pub use options::{ScanOptions, default_excluded_dirs, default_video_extensions};
pub use resume::{ResumableScan, ResumeIter, ScanCursor, ScanProgress};
pub use walk::{ScanEntry, ScanIter, walk};
