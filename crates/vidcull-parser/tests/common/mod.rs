#![allow(dead_code)]

use std::path::{Path, PathBuf};

use vidcull_parser::fallback::FfmpegBinaries;

fn require_ffmpeg_gate(test: &str) {
    assert!(
        std::env::var_os("VIDCULL_REQUIRE_FFMPEG").is_none(),
        "VIDCULL_REQUIRE_FFMPEG set but ffmpeg/ffprobe unresolved \
         (PATH/VIDCULL_FFMPEG_DIR) — refusing to silent-skip (test: {test})"
    );
}

pub fn ffmpeg_or_skip(test: &str) -> Option<PathBuf> {
    let candidate = match std::env::var_os("VIDCULL_FFMPEG_DIR") {
        Some(dir) => {
            let exe = if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            };
            Path::new(&dir).join(exe)
        }
        None => PathBuf::from("ffmpeg"),
    };
    match std::process::Command::new(&candidate)
        .arg("-version")
        .output()
    {
        Ok(out) if out.status.success() => Some(candidate),
        _ => {
            require_ffmpeg_gate(test);
            eprintln!(
                "SKIP {test}: ffmpeg not resolvable; set VIDCULL_FFMPEG_DIR or install on PATH"
            );
            None
        }
    }
}

pub fn binaries_or_skip(test: &str) -> Option<FfmpegBinaries> {
    match FfmpegBinaries::resolve() {
        Ok(bins) => Some(bins),
        Err(e) => {
            require_ffmpeg_gate(test);
            eprintln!(
                "SKIP {test}: ffmpeg/ffprobe not resolvable ({e}); \
                 set VIDCULL_FFMPEG_DIR or install on PATH"
            );
            None
        }
    }
}
