use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NormalizedPath(String);

impl NormalizedPath {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let raw = path.as_ref().to_string_lossy();
        Self(raw.replace('\\', "/"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn as_display_path(&self) -> &Path {
        Path::new(&self.0)
    }

    #[deprecated(note = "use to_native_path() for I/O or as_display_path() for display")]
    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    #[must_use]
    pub fn to_native_path(&self) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(self.0.replace('/', "\\"))
        }
        #[cfg(not(windows))]
        {
            self.to_path_buf()
        }
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl AsRef<Path> for NormalizedPath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl std::fmt::Display for NormalizedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslashes_become_forward_slashes() {
        let p = NormalizedPath::new(r"C:\Users\Example\Desktop\video.mp4");
        assert_eq!(p.as_str(), "C:/Users/Example/Desktop/video.mp4");
    }

    #[test]
    fn unix_paths_pass_through_unchanged() {
        let p = NormalizedPath::new("/mnt/nas/clips/foo.mkv");
        assert_eq!(p.as_str(), "/mnt/nas/clips/foo.mkv");
    }

    #[test]
    fn round_trip_via_postcard() {
        let p = NormalizedPath::new(r"D:\library\subdir\file.mp4");
        let bytes = postcard::to_allocvec(&p).expect("encode");
        let decoded: NormalizedPath = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(p, decoded);
        assert_eq!(decoded.as_str(), "D:/library/subdir/file.mp4");
    }

    #[test]
    fn as_display_path_returns_forward_slash_view() {
        let p = NormalizedPath::new("/a/b/c.mp4");
        assert_eq!(p.as_display_path(), Path::new("/a/b/c.mp4"));
    }

    #[test]
    #[allow(deprecated)]
    fn as_path_still_returns_the_forward_slash_view() {
        let p = NormalizedPath::new("/a/b/c.mp4");
        assert_eq!(p.as_path(), Path::new("/a/b/c.mp4"));
    }

    #[cfg(windows)]
    #[test]
    fn to_native_path_restores_backslashes_on_windows() {
        for (stored, native) in [
            ("C:/Users/x", r"C:\Users\x"),
            ("D:/lib/a.mp4", r"D:\lib\a.mp4"),
            ("//server/share/f", r"\\server\share\f"),
        ] {
            let p = NormalizedPath::new(stored);
            assert_eq!(p.as_str(), stored, "storage representation is unchanged");
            assert_eq!(
                p.to_native_path(),
                PathBuf::from(native),
                "native I/O path restores the OS separator: {stored}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn to_native_path_best_effort_round_trips_a_verbatim_prefix() {
        let p = NormalizedPath::new("//?/C:/x");
        assert_eq!(p.as_str(), "//?/C:/x");
        assert_eq!(p.to_native_path(), PathBuf::from(r"\\?\C:\x"));
    }

    #[cfg(not(windows))]
    #[test]
    fn to_native_path_is_identity_off_windows() {
        for stored in ["/a/b/c.mp4", "//server/share/f", "relative/x.mp4"] {
            let p = NormalizedPath::new(stored);
            assert_eq!(p.to_native_path(), PathBuf::from(stored));
            assert_eq!(p.as_str(), stored);
        }
    }

    #[test]
    fn empty_path_is_valid() {
        let p = NormalizedPath::new("");
        assert_eq!(p.as_str(), "");
    }

    #[test]
    fn display_matches_as_str() {
        let p = NormalizedPath::new("/a/b/c.mp4");
        assert_eq!(format!("{p}"), p.as_str());
    }

    #[test]
    fn to_path_buf_creates_owned_copy() {
        let p = NormalizedPath::new("/a/b/c.mp4");
        let pb = p.to_path_buf();
        assert_eq!(pb, std::path::PathBuf::from("/a/b/c.mp4"));
    }
}
