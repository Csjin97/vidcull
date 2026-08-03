use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use vidcull_core::{Error, Result};

#[cfg(windows)]
pub(crate) const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
pub(crate) const EXE_SUFFIX: &str = "";

const ENV_FFMPEG: &str = "VIDCULL_FFMPEG";
const ENV_FFPROBE: &str = "VIDCULL_FFPROBE";
const ENV_DIR: &str = "VIDCULL_FFMPEG_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegBinaries {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl FfmpegBinaries {
    #[must_use]
    pub fn new(ffmpeg: PathBuf, ffprobe: PathBuf) -> Self {
        Self { ffmpeg, ffprobe }
    }

    #[must_use]
    pub fn from_dir(dir: &Path) -> Self {
        Self {
            ffmpeg: dir.join(format!("ffmpeg{EXE_SUFFIX}")),
            ffprobe: dir.join(format!("ffprobe{EXE_SUFFIX}")),
        }
    }

    pub fn resolve() -> Result<Self> {
        let dir = std::env::var_os(ENV_DIR);
        let path = std::env::var_os("PATH");
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        resolve_from(
            std::env::var_os(ENV_FFMPEG),
            std::env::var_os(ENV_FFPROBE),
            dir.as_deref(),
            exe_dir.as_deref().map(Path::as_os_str),
            path.as_deref(),
        )
    }

    #[must_use]
    pub fn ffmpeg(&self) -> &Path {
        &self.ffmpeg
    }

    #[must_use]
    pub fn ffprobe(&self) -> &Path {
        &self.ffprobe
    }
}

fn resolve_from(
    ffmpeg_env: Option<OsString>,
    ffprobe_env: Option<OsString>,
    dir: Option<&OsStr>,
    exe_dir: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Result<FfmpegBinaries> {
    Ok(FfmpegBinaries {
        ffmpeg: pick(ffmpeg_env, "ffmpeg", dir, exe_dir, path)?,
        ffprobe: pick(ffprobe_env, "ffprobe", dir, exe_dir, path)?,
    })
}

fn pick(
    explicit: Option<OsString>,
    stem: &str,
    dir: Option<&OsStr>,
    exe_dir: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        let candidate = PathBuf::from(explicit);
        if candidate.is_file() {
            return Ok(candidate);
        }
        let shown = candidate.file_name().unwrap_or(std::ffi::OsStr::new("?"));
        return Err(Error::Unsupported(format!(
            "{stem}: configured override path does not exist (file {shown:?}); check \
             {ENV_FFMPEG}/{ENV_FFPROBE}/{ENV_DIR}"
        )));
    }
    for base in [dir, exe_dir].into_iter().flatten() {
        let candidate = Path::new(base).join(format!("{stem}{EXE_SUFFIX}"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(found) = find_on_path(stem, path) {
        return Ok(found);
    }
    Err(Error::Unsupported(format!(
        "{stem}: not found via {ENV_FFMPEG}/{ENV_FFPROBE}, {ENV_DIR}, exe dir, or PATH"
    )))
}

fn find_on_path(stem: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    let file = format!("{stem}{EXE_SUFFIX}");
    std::env::split_paths(path)
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"#!stub").expect("write stub binary");
        p
    }

    #[test]
    fn from_dir_appends_platform_suffix() {
        let bins = FfmpegBinaries::from_dir(Path::new("/opt/av"));
        assert_eq!(
            bins.ffmpeg(),
            Path::new(&format!("/opt/av/ffmpeg{EXE_SUFFIX}"))
        );
        assert_eq!(
            bins.ffprobe(),
            Path::new(&format!("/opt/av/ffprobe{EXE_SUFFIX}"))
        );
    }

    #[test]
    fn explicit_override_wins_when_it_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ff = touch(tmp.path(), &format!("custom-ffmpeg{EXE_SUFFIX}"));
        let fp = touch(tmp.path(), &format!("custom-ffprobe{EXE_SUFFIX}"));
        let bins = resolve_from(
            Some(ff.clone().into_os_string()),
            Some(fp.clone().into_os_string()),
            None,
            None,
            None,
        )
        .expect("resolve");
        assert_eq!(bins.ffmpeg(), ff);
        assert_eq!(bins.ffprobe(), fp);
    }

    #[test]
    fn explicit_override_that_is_missing_is_a_hard_error() {
        let err = resolve_from(
            Some(OsString::from("/no/such/ffmpeg")),
            Some(OsString::from("/no/such/ffprobe")),
            None,
            None,
            None,
        )
        .expect_err("missing explicit path must error");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn bundled_dir_is_used_before_path() {
        let bundled = tempfile::tempdir().expect("bundled dir");
        let on_path = tempfile::tempdir().expect("path dir");
        touch(bundled.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(bundled.path(), &format!("ffprobe{EXE_SUFFIX}"));
        touch(on_path.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(on_path.path(), &format!("ffprobe{EXE_SUFFIX}"));

        let bins = resolve_from(
            None,
            None,
            Some(bundled.path().as_os_str()),
            None,
            Some(on_path.path().as_os_str()),
        )
        .expect("resolve");
        assert!(bins.ffmpeg().starts_with(bundled.path()));
        assert!(bins.ffprobe().starts_with(bundled.path()));
    }

    #[test]
    fn exe_dir_is_used_before_path() {
        let exe_dir = tempfile::tempdir().expect("exe dir");
        let on_path = tempfile::tempdir().expect("path dir");
        touch(exe_dir.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(exe_dir.path(), &format!("ffprobe{EXE_SUFFIX}"));
        touch(on_path.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(on_path.path(), &format!("ffprobe{EXE_SUFFIX}"));
        let bins = resolve_from(
            None,
            None,
            None,
            Some(exe_dir.path().as_os_str()),
            Some(on_path.path().as_os_str()),
        )
        .expect("resolve");
        assert!(bins.ffmpeg().starts_with(exe_dir.path()));
        assert!(bins.ffprobe().starts_with(exe_dir.path()));
    }

    #[test]
    fn bundled_dir_wins_over_exe_dir() {
        let bundled = tempfile::tempdir().expect("bundled dir");
        let exe_dir = tempfile::tempdir().expect("exe dir");
        touch(bundled.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(bundled.path(), &format!("ffprobe{EXE_SUFFIX}"));
        touch(exe_dir.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(exe_dir.path(), &format!("ffprobe{EXE_SUFFIX}"));
        let bins = resolve_from(
            None,
            None,
            Some(bundled.path().as_os_str()),
            Some(exe_dir.path().as_os_str()),
            None,
        )
        .expect("resolve");
        assert!(bins.ffmpeg().starts_with(bundled.path()));
    }

    #[test]
    fn falls_back_to_path_search() {
        let on_path = tempfile::tempdir().expect("path dir");
        touch(on_path.path(), &format!("ffmpeg{EXE_SUFFIX}"));
        touch(on_path.path(), &format!("ffprobe{EXE_SUFFIX}"));
        let bins = resolve_from(None, None, None, None, Some(on_path.path().as_os_str()))
            .expect("resolve");
        assert!(bins.ffmpeg().starts_with(on_path.path()));
    }

    #[test]
    fn unresolvable_is_unsupported_not_panic() {
        let empty = tempfile::tempdir().expect("empty dir");
        let err = resolve_from(None, None, None, None, Some(empty.path().as_os_str()))
            .expect_err("nothing on PATH must error");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn find_on_path_returns_none_when_path_unset() {
        assert_eq!(find_on_path("ffmpeg", None), None);
    }
}
