use std::io::BufReader;
use std::path::{Path, PathBuf};

use vidcull_core::Error;

use vidcull_fingerprint::GrayFrame;
use vidcull_fingerprint::tier1::phash_frames;
use vidcull_parser::ContainerKind;
use vidcull_parser::fallback::{DecodePath, FfmpegBinaries};
use vidcull_parser::mp4::{extract_avcc, probe_mp4, read_mp4_tolerant};
use vidcull_parser::mp4_index::index_mp4;
use vidcull_parser::probe_and_decode_sparse;
use vidcull_parser::sparse_mp4::Mp4SampleSource;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn box_header(size: u32, fourcc: &[u8]) -> Vec<u8> {
    let mut v = size.to_be_bytes().to_vec();
    v.extend_from_slice(fourcc);
    v
}

fn with_overshoot_garbage(base: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    v.extend_from_slice(&box_header(500_000_000, b"junk"));
    v
}

fn write_mp4(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clip.mp4");
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

fn raw_read(bytes: &[u8]) -> Result<mp4parse::MediaContext, mp4parse::Error> {
    mp4parse::read_mp4(&mut BufReader::new(std::io::Cursor::new(bytes.to_vec())))
}

#[test]
fn t1_overshoot_garbage_all_four_parsers_recover() {
    let base = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let craft = with_overshoot_garbage(&base);

    assert!(
        raw_read(&craft).is_err(),
        "crafted overshoot fixture must reproduce the raw mp4parse failure"
    );

    let (_dir, path) = write_mp4(&craft);

    assert!(
        read_mp4_tolerant(&path).is_ok(),
        "read_mp4_tolerant must recover the clip past trailing garbage"
    );

    probe_mp4(&path, ContainerKind::Mp4).expect("probe_mp4 recovers metadata");
    extract_avcc(&path).expect("extract_avcc recovers the avcC");
    index_mp4(&path).expect("index_mp4 recovers the keyframe/GOP index");
    Mp4SampleSource::open(&path).expect("Mp4SampleSource::open builds the IDR table");
}

#[test]
fn t5_garbage_before_moov_stays_error() {
    let base = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let mut craft = base[..32].to_vec();
    craft.extend_from_slice(&box_header(500_000_000, b"junk"));
    craft.extend_from_slice(&base[32..]);

    let (_dir, path) = write_mp4(&craft);

    let err = read_mp4_tolerant(&path).expect_err("trimmed-away moov must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "expected Parse, got {err:?}"
    );
    assert!(probe_mp4(&path, ContainerKind::Mp4).is_err());
    assert!(index_mp4(&path).is_err());
}

#[test]
fn t6_fit_declaring_trailing_garbage_returns_original_error() {
    let base = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let body = [0u8; 40];
    let mut craft = base.clone();
    craft.extend_from_slice(&box_header(8 + u32::try_from(body.len()).unwrap(), b"moov"));
    craft.extend_from_slice(&body);

    let raw_err = raw_read(&craft).expect_err("malformed fit-declaring moov must fail raw");

    let (_dir, path) = write_mp4(&craft);
    let err = read_mp4_tolerant(&path).expect_err("fit-declaring garbage is not trimmed");
    assert_eq!(
        format!("{err:?}"),
        format!("{:?}", Error::Parse(format!("mp4parse: {raw_err}"))),
        "fit-declaring residual must surface the ORIGINAL error unchanged"
    );
}

#[test]
fn t7_legit_fitting_trailing_box_is_not_trimmed() {
    let mut craft = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let pad = [0u8; 8];
    craft.extend_from_slice(&box_header(8 + u32::try_from(pad.len()).unwrap(), b"free"));
    craft.extend_from_slice(&pad);

    let raw = raw_read(&craft).expect("mp4parse tolerates a fitting trailing free box");
    let (_dir, path) = write_mp4(&craft);
    let tol = read_mp4_tolerant(&path).expect("tolerant reader is a no-op on a valid file");
    assert_eq!(
        tol.tracks.len(),
        raw.tracks.len(),
        "no-op: tolerant parse must equal raw parse for a legit fitting tail"
    );
}

#[test]
fn t8_never_masks_original_error_with_trim_derived_one() {
    let base = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let mut craft = base[..32].to_vec();
    craft.extend_from_slice(&box_header(500_000_000, b"junk"));
    craft.extend_from_slice(&base[32..]);

    let raw_err = raw_read(&craft).expect_err("crafted file must fail raw");
    let raw_msg = format!("{raw_err}");

    let (_dir, path) = write_mp4(&craft);
    let err = read_mp4_tolerant(&path).expect_err("must stay an error");
    let Error::Parse(msg) = &err else {
        panic!("expected Parse, got {err:?}");
    };
    assert_eq!(msg, &format!("mp4parse: {raw_msg}"));
    assert!(
        !msg.contains("MoovMissing"),
        "helper masked the original error with the trim-derived retry error: {msg}"
    );
}

fn native_decode(path: &Path) -> Vec<vidcull_parser::sparse::GrayscaleFrame> {
    let bins = FfmpegBinaries::new(
        PathBuf::from("/nonexistent/ffmpeg"),
        PathBuf::from("/nonexistent/ffprobe"),
    );
    let decoded = probe_and_decode_sparse(&bins, path, 4)
        .unwrap_or_else(|e| panic!("probe_and_decode_sparse({}): {e:?}", path.display()));
    assert_eq!(
        decoded.decode_path,
        DecodePath::Native,
        "H.264 must decode on the native path (see h264_native_e2e::assert_native_phash_matches)"
    );
    decoded.frames
}

#[test]
fn ac3_trailing_garbage_preserves_native_fingerprint() {
    let clean_path = fixture("h264-native-e2e/testsrc2_160_90.mp4");
    let clean_frames = native_decode(&clean_path);

    let craft = with_overshoot_garbage(&std::fs::read(&clean_path).unwrap());
    assert!(
        raw_read(&craft).is_err(),
        "crafted clip must fail raw mp4parse"
    );
    let (_dir, garbage_path) = write_mp4(&craft);
    let garbage_frames = native_decode(&garbage_path);

    assert_eq!(
        garbage_frames.len(),
        clean_frames.len(),
        "garbage-appended clip produced a different frame count"
    );
    for (i, (g, c)) in garbage_frames.iter().zip(clean_frames.iter()).enumerate() {
        assert_eq!(g.timestamp_ms, c.timestamp_ms, "frame {i} timestamp drift");
        assert_eq!((g.width, g.height), (c.width, c.height), "frame {i} dims");
        assert_eq!(
            g.pixels, c.pixels,
            "frame {i} pixels differ under trailing garbage"
        );
        let g_hash = phash_frames(&[GrayFrame {
            width: g.width,
            height: g.height,
            pixels: &g.pixels,
        }]);
        let c_hash = phash_frames(&[GrayFrame {
            width: c.width,
            height: c.height,
            pixels: &c.pixels,
        }]);
        assert_eq!(
            g_hash, c_hash,
            "frame {i} pHash {g_hash:#018x} != clean {c_hash:#018x} under trailing garbage"
        );
    }
}
