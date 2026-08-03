use std::env;
use std::path::PathBuf;

fn real_dir_opt() -> Option<PathBuf> {
    let val = env::var("VIDCULL_REAL_CORPUS_DIR").ok()?;
    let p = PathBuf::from(val);
    if p.is_dir() { Some(p) } else { None }
}

fn skip_if_corpus_absent() -> bool {
    if real_dir_opt().is_none() {
        eprintln!(
            "[RealCorpus] SKIPPED — set VIDCULL_REAL_CORPUS_DIR to enable \
             real-corpus gate"
        );
        return true;
    }
    false
}

pub struct RealCorpus {
    dir: PathBuf,
}

impl RealCorpus {
    #[track_caller]
    #[must_use]
    pub fn open() -> Self {
        let dir = real_dir_opt().expect(
            "RealCorpus::open called but VIDCULL_REAL_CORPUS_DIR is not set \
             or does not exist — call skip_if_corpus_absent() first",
        );
        let c = Self { dir };
        let mut missing: Vec<String> = Vec::new();
        for (name, regen) in Self::required_files() {
            if !c.dir.join(name).is_file() {
                missing.push(format!("  {name}\n    regen: {regen}"));
            }
        }
        assert!(
            missing.is_empty(),
            "\n\n[RealCorpus] Required fixture files are missing.\n\
             Set VIDCULL_REAL_CORPUS_DIR and run the ffmpeg commands below \
             to re-provision them, then re-run the tests.\n\n\
             Missing files:\n{}\n",
            missing.join("\n")
        );
        c
    }

    #[must_use]
    pub fn pair_h264(&self) -> PathBuf {
        self.dir.join("pair_h264.mp4")
    }

    #[must_use]
    pub fn pair_h265(&self) -> PathBuf {
        self.dir.join("pair_h265.mp4")
    }

    #[must_use]
    pub fn pair_av1(&self) -> PathBuf {
        self.dir.join("pair_av1.mp4")
    }

    #[must_use]
    pub fn pair_partial(&self) -> PathBuf {
        self.dir.join("pair_partial.mp4")
    }

    fn required_files() -> &'static [(&'static str, &'static str)] {
        &[
            (
                "pair_h264.mp4",
                r#"ffmpeg -y -ss 00:01:00 -t 8 -i "<YOUR_SOURCE_DIR>/<source_h264>.mp4" -c copy fixtures\real\pair_h264.mp4"#,
            ),
            (
                "pair_h265.mp4",
                r#"ffmpeg -y -ss 00:01:00 -t 8 -i "<YOUR_SOURCE_DIR>/<source_h265>.mp4" -c copy fixtures\real\pair_h265.mp4"#,
            ),
            (
                "pair_av1.mp4",
                r#"ffmpeg -y -ss 00:01:00 -t 8 -i "<YOUR_SOURCE_DIR>/<source_av1>.mp4" -c copy fixtures\real\pair_av1.mp4"#,
            ),
            (
                "pair_partial.mp4",
                r#"copy "<YOUR_SOURCE_DIR>\<source_partial>.mp4" fixtures\real\pair_partial.mp4"#,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_required_fixtures_are_present() {
        if skip_if_corpus_absent() {
            return;
        }
        let dir = real_dir_opt().unwrap();
        let mut missing: Vec<String> = Vec::new();
        for (name, regen) in RealCorpus::required_files() {
            if !dir.join(name).is_file() {
                missing.push(format!("  {name}  (regen: {regen})"));
            }
        }
        assert!(
            missing.is_empty(),
            "\n[RealCorpus] Missing fixture files:\n{}\n\
             Run the ffmpeg commands above to provision them.\n\
             (Set VIDCULL_REAL_CORPUS_DIR to the corpus directory.)",
            missing.join("\n")
        );
    }

    #[test]
    fn fixture_files_have_expected_extensions() {
        if skip_if_corpus_absent() {
            return;
        }
        let dir = real_dir_opt().unwrap();
        for (name, _) in RealCorpus::required_files() {
            let path = dir.join(name);
            if path.is_file() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                assert_eq!(
                    ext, "mp4",
                    "fixture {name} must be an .mp4 container (got .{ext})"
                );
            }
        }
    }
}
