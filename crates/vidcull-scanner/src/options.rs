use std::collections::BTreeSet;

#[must_use]
pub fn default_video_extensions() -> BTreeSet<String> {
    [
        "mp4", "mkv", "m4v", "mov", "avi", "webm", "ts", "m2ts", "mts", "flv", "wmv", "mpg",
        "mpeg", "ogv", "3gp",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub extensions: BTreeSet<String>,
    pub follow_symlinks: bool,
    pub max_depth: Option<usize>,
    pub exclude_dirs: BTreeSet<String>,
}

#[must_use]
pub fn default_excluded_dirs() -> BTreeSet<String> {
    [
        "$recycle.bin",
        "system volume information",
        "$winreagent",
        "$windows.~bt",
        "$windows.~ws",
        "recovery",
        "config.msi",
        "node_modules",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            extensions: default_video_extensions(),
            follow_symlinks: false,
            max_depth: None,
            exclude_dirs: default_excluded_dirs(),
        }
    }
}

impl ScanOptions {
    #[must_use]
    pub fn with_extensions<I, S>(mut self, exts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.extensions = exts
            .into_iter()
            .map(|e| e.as_ref().trim_start_matches('.').to_ascii_lowercase())
            .collect();
        self
    }

    #[must_use]
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    #[must_use]
    pub fn following_symlinks(mut self) -> Self {
        self.follow_symlinks = true;
        self
    }

    #[must_use]
    pub fn accepts_extension(&self, ext_lower: &str) -> bool {
        self.extensions.contains(ext_lower)
    }

    #[must_use]
    pub fn is_excluded_dir_name(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.exclude_dirs.contains(&lower) || lower.starts_with("found.")
    }

    #[must_use]
    pub fn with_excludes<I, S>(mut self, rules: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for rule in rules {
            let r = rule.as_ref().trim().to_ascii_lowercase();
            if !r.is_empty() {
                self.exclude_dirs.insert(r);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_includes_primary_native_codecs() {
        let opts = ScanOptions::default();
        assert!(opts.accepts_extension("mp4"));
        assert!(opts.accepts_extension("mkv"));
    }

    #[test]
    fn with_extensions_strips_leading_dot_and_lowercases() {
        let opts = ScanOptions::default().with_extensions([".MP4", "WEIRD"]);
        assert!(opts.accepts_extension("mp4"));
        assert!(opts.accepts_extension("weird"));
        assert!(!opts.accepts_extension("mkv"));
    }

    #[test]
    fn with_max_depth_is_recorded() {
        let opts = ScanOptions::default().with_max_depth(3);
        assert_eq!(opts.max_depth, Some(3));
    }

    #[test]
    fn following_symlinks_opt_in() {
        assert!(!ScanOptions::default().follow_symlinks);
        assert!(ScanOptions::default().following_symlinks().follow_symlinks);
    }
}
