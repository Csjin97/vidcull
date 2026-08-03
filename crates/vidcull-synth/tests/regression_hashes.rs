use std::fs;
use std::path::Path;
use vidcull_synth::{FfmpegBinaries, plan_regression_corpus, render, render_testsrc};

const SOURCE_DURATION_MS: u64 = 30_000;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const SEED: u64 = 0xDEAD_BEEF;

const GOLDEN_FFMPEG_BUILD_ID: &str = "baa9fccf8d";

fn get_vendored_binaries() -> Option<FfmpegBinaries> {
    if let Some(dir) = std::env::var_os("VIDCULL_FFMPEG_DIR") {
        let bins = FfmpegBinaries::from_dir(Path::new(&dir));
        if bins.ffmpeg().is_file() && bins.ffprobe().is_file() {
            return Some(bins);
        }
    }

    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let rel = Path::new("vendor").join("ffmpeg").join(&platform);
    if let Ok(mut cursor) = std::env::current_dir() {
        for _ in 0..12 {
            let candidate_dir = cursor.join(&rel);
            let bins = FfmpegBinaries::from_dir(&candidate_dir);
            if bins.ffmpeg().is_file() && bins.ffprobe().is_file() {
                return Some(bins);
            }
            if let Some(parent) = cursor.parent().map(Path::to_path_buf) {
                cursor = parent;
            } else {
                break;
            }
        }
    }
    None
}

fn ffmpeg_has_encoder(bins: &FfmpegBinaries, encoder: &str) -> bool {
    std::process::Command::new(bins.ffmpeg())
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(encoder))
}

fn ffmpeg_build_id(bins: &FfmpegBinaries) -> Option<String> {
    let out = std::process::Command::new(bins.ffmpeg())
        .args(["-version"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout.lines().next()?.trim();
    if let Some(pos) = first_line.find("git-") {
        let after = &first_line[pos + 4..];
        let hash: String = after.chars().take_while(char::is_ascii_hexdigit).collect();
        if !hash.is_empty() {
            return Some(hash);
        }
    }
    Some(first_line.to_owned())
}

fn sha256_digest(path: &Path) -> String {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("certutil")
            .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
            .output()
            .expect("run certutil");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        let hex_line = lines
            .get(1)
            .expect("certutil output must have at least 2 lines");
        let cleaned: String = hex_line
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        cleaned.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("sha256sum")
            .arg(path)
            .output()
            .expect("run sha256sum");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first_word = stdout
            .split_whitespace()
            .next()
            .expect("sha256sum output not empty");
        first_word.to_lowercase()
    }
}

#[allow(clippy::too_many_lines)]
#[test]
fn test_regression_corpus_hashes() {
    let Some(bins) = get_vendored_binaries() else {
        assert!(
            !(std::env::var("CI").is_ok() && cfg!(windows)),
            "Vendored FFmpeg not found on Windows CI but it is required!"
        );
        eprintln!("SKIP: Vendored FFmpeg binaries not found");
        return;
    };

    if !ffmpeg_has_encoder(&bins, "libx264") {
        eprintln!(
            "SKIP: vendored FFmpeg lacks libx264 (LGPL build); \
             H.264 corpus unrenderable"
        );
        return;
    }

    let running_id = ffmpeg_build_id(&bins).unwrap_or_default();
    if running_id != GOLDEN_FFMPEG_BUILD_ID {
        eprintln!(
            "SKIP: byte-golden pinned to ffmpeg build '{GOLDEN_FFMPEG_BUILD_ID}', \
             running '{running_id}' — see file header"
        );
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    let out_dir = dir.path().join("out");
    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::create_dir_all(&out_dir).expect("create out dir");

    let src = render_testsrc(&bins, &src_dir, 1, SOURCE_DURATION_MS, WIDTH, HEIGHT)
        .expect("render testsrc");

    let plan = plan_regression_corpus(&src, SOURCE_DURATION_MS, WIDTH, HEIGHT, SEED, &out_dir)
        .expect("plan regression corpus");

    let mut actual_hashes = Vec::new();
    for variant in &plan {
        let path = render(&bins, &variant.plan).expect("render variant");
        let hash = sha256_digest(&path);
        actual_hashes.push((
            variant
                .plan
                .output
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            hash,
        ));
    }

    let golden_hashes = vec![
        (
            "source.0000000000000001.remux.4adfb90f68c9eb9b.mkv",
            "43c9a1cf5c5f14515beb20d344b2c86990968ad6b0a20e6e65586796367f14e4",
        ),
        (
            "source.0000000000000001.hevc.de586a3141a10922.mp4",
            "938ef3ca702f6026f0e7b58c07160ec2bb46f7c3cd4a67a48da6d27719f211e3",
        ),
        (
            "source.0000000000000001.scale160x90_h264.021fbc2f8e1cfc1d.mp4",
            "2121dc62893e972c8b824ed0de23d37786d4324a8a9b820f847f6e099ffe515e",
        ),
        (
            "source.0000000000000001.wm_h264.7466ce737be16790.mp4",
            "81780ca12a8935f8565ac885b83b3f758f5097006123dcd1df3fd3675ca41385",
        ),
        (
            "source.0000000000000001.h264_500k.3bfa8764f685bd1c.mp4",
            "3c53cb87b9d95af69c014165c87459e46f9f6664bc479a1de806d45505e1ee6f",
        ),
        (
            "source.0000000000000001.fps29970_h264.ab203e503cb55b3f.mp4",
            "669bf7e69b6e40b536fc7f363eed7ac42c2f0d769d869b29e280a3d9e4614158",
        ),
        (
            "source.0000000000000001.clip10000-10000_h264.5a2fdc2bf68cedb3.mp4",
            "a9e394c38c8d273b42aba22b44877a60393ff5b1b27d007b72082b6ac2e75fb8",
        ),
        (
            "source.0000000000000001.sub_h264.b30a4ccf430b1b5a.mp4",
            "484453db36eb7f410e524db92fe5f0e171371f5852a2229b24f68721a264a781",
        ),
        (
            "source.0000000000000001.bri20_h264.0a90415039bd5985.mp4",
            "fc3a86818c6a9095991dbd6247f3439d1af7c2dea9114e277bfeadbe81d1a63e",
        ),
    ];

    println!("--- CALCULATED REGRESSION HASHES ---");
    for (filename, hash) in &actual_hashes {
        println!("(\"{filename}\", \"{hash}\"),");
    }
    println!("------------------------------------");

    for (i, (expected_name, expected_hash)) in golden_hashes.iter().enumerate() {
        let (actual_name, actual_hash) = &actual_hashes[i];
        assert_eq!(
            actual_name, expected_name,
            "Variant file name mismatch at index {i}"
        );
        assert_eq!(
            actual_hash, *expected_hash,
            "Hash mismatch for variant {expected_name}"
        );
    }
}
