use std::path::{Path, PathBuf};

use vidcull_fingerprint::GrayFrame;
use vidcull_fingerprint::tier1::phash_frames;
use vidcull_parser::fallback::{DecodePath, FfmpegBinaries};
use vidcull_parser::probe_and_decode_sparse;

const FIXTURES: &[(&str, usize, usize)] = &[("wpp", 256, 192), ("wpp2", 320, 256)];

const EXPECTED_TIMESTAMPS: &[u64] = &[0, 2500, 5000];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hevc-wpp-e2e")
}

#[test]
fn wpp_native_mkv_path_is_taken_and_phash_matches_ffmpeg_gray() {
    let bins = FfmpegBinaries::new(
        PathBuf::from("/nonexistent/ffmpeg"),
        PathBuf::from("/nonexistent/ffprobe"),
    );

    for &(name, width, height) in FIXTURES {
        let dir = fixture_dir();
        let reference = std::fs::read(dir.join(format!("{name}.gray8")))
            .unwrap_or_else(|e| panic!("read {name}.gray8: {e}"));
        let mkv = dir.join(format!("{name}.mkv"));
        let label = format!("{name} mkv");

        let decoded = probe_and_decode_sparse(&bins, &mkv, 4)
            .unwrap_or_else(|e| panic!("[{label}] probe_and_decode_sparse: {e:?}"));

        assert_eq!(
            decoded.decode_path,
            DecodePath::Native,
            "[{label}] WPP H.265 MKV must decode on the native path, not the ffmpeg fallback"
        );

        let frame_len = width * height;
        assert_eq!(
            reference.len(),
            frame_len * EXPECTED_TIMESTAMPS.len(),
            "[{label}] reference holds {} plan frames",
            EXPECTED_TIMESTAMPS.len()
        );
        assert_eq!(
            decoded.frames.len(),
            EXPECTED_TIMESTAMPS.len(),
            "[{label}] native produced {} frames, expected {}",
            decoded.frames.len(),
            EXPECTED_TIMESTAMPS.len()
        );

        for (i, frame) in decoded.frames.iter().enumerate() {
            assert_eq!(
                frame.timestamp_ms, EXPECTED_TIMESTAMPS[i],
                "[{label}] frame {i} stamped {} ms, expected {} (grid parity with ffmpeg)",
                frame.timestamp_ms, EXPECTED_TIMESTAMPS[i]
            );
            assert_eq!(
                (frame.width as usize, frame.height as usize),
                (width, height),
                "[{label}] frame {i} dimensions"
            );
            assert_eq!(frame.pixels.len(), frame_len, "[{label}] frame {i} size");

            let want = &reference[i * frame_len..(i + 1) * frame_len];
            let native_phash = phash_frames(&[GrayFrame {
                width: frame.width,
                height: frame.height,
                pixels: &frame.pixels,
            }]);
            let golden_phash = phash_frames(&[GrayFrame {
                width: u32::try_from(width).unwrap(),
                height: u32::try_from(height).unwrap(),
                pixels: want,
            }]);
            assert_eq!(
                native_phash, golden_phash,
                "[{label}] native WPP frame {i} (t={} ms) pHash {native_phash:#018x} \
                 differs from ffmpeg golden pHash {golden_phash:#018x}",
                frame.timestamp_ms,
            );
        }
    }
}
