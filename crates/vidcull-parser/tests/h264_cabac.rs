mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::ffmpeg_or_skip;
use vidcull_parser::h264::nal::{NalHeader, NalUnitType, split_annex_b};
use vidcull_parser::h264::params::{parse_pps, parse_sps};
use vidcull_parser::h264::{BitReader, LumaFrame, decode_intra_frame, rbsp_from_ebsp};

const FIXTURES: &[&str] = &[
    "main_testsrc_64x64",
    "main_bars_176x144",
    "main_crop_160x90",
    "high_testsrc_96x64",
    "high_bars_176x144",
];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264-cabac")
}

fn decode_native(h264: &[u8]) -> LumaFrame {
    let units = split_annex_b(h264);

    let mut sps = None;
    let mut pps = None;
    let mut slice_rbsps: Vec<(NalHeader, Vec<u8>)> = Vec::new();

    for u in &units {
        let rbsp = rbsp_from_ebsp(&u.payload);
        match u.header.unit_type {
            NalUnitType::Sps => {
                let mut r = BitReader::new(&rbsp);
                sps = Some(parse_sps(&mut r).expect("parse SPS"));
            }
            NalUnitType::Pps => {
                let s = sps.as_ref().expect("SPS precedes PPS");
                let mut r = BitReader::new(&rbsp);
                pps = Some(parse_pps(&mut r, s.chroma_format_idc).expect("parse PPS"));
            }
            NalUnitType::IdrSlice => slice_rbsps.push((u.header.clone(), rbsp)),
            _ => {}
        }
    }

    let sps = sps.expect("stream carries an SPS");
    let pps = pps.expect("stream carries a PPS");
    assert!(
        pps.entropy_coding_mode_flag,
        "CABAC corpus fixture must be CABAC-coded"
    );
    let slices: Vec<(&NalHeader, &[u8])> =
        slice_rbsps.iter().map(|(h, r)| (h, r.as_slice())).collect();

    decode_intra_frame(&sps, &pps, &slices).expect("native CABAC decode")
}

fn assert_bit_exact(name: &str, frame: &LumaFrame, reference: &[u8]) {
    assert_eq!(
        frame.data.len(),
        reference.len(),
        "[{name}] luma size {} != reference {} ({}x{})",
        frame.data.len(),
        reference.len(),
        frame.width,
        frame.height
    );
    if let Some((i, (&a, &b))) = frame
        .data
        .iter()
        .zip(reference.iter())
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        let (x, y) = (i % frame.width, i / frame.width);
        let diffs = frame
            .data
            .iter()
            .zip(reference.iter())
            .filter(|(a, b)| a != b)
            .count();
        panic!(
            "[{name}] native CABAC decode differs from ffmpeg: {diffs}/{} samples; \
             first at (x={x}, y={y}) native={a} ref={b} \
             [x%4={} y%4={} x%16={} y%16={}]",
            frame.data.len(),
            x % 4,
            y % 4,
            x % 16,
            y % 16,
        );
    }
}

#[test]
fn native_cabac_decode_is_bit_exact_vs_committed_reference() {
    for &name in FIXTURES {
        let dir = fixture_dir();
        let h264 = std::fs::read(dir.join(format!("{name}.h264")))
            .unwrap_or_else(|e| panic!("read {name}.h264: {e}"));
        let reference = std::fs::read(dir.join(format!("{name}.y8")))
            .unwrap_or_else(|e| panic!("read {name}.y8: {e}"));

        let frame = decode_native(&h264);
        assert_bit_exact(name, &frame, &reference);
    }
}

#[test]
fn committed_references_match_ffmpeg() {
    let Some(ffmpeg) = ffmpeg_or_skip("committed_references_match_ffmpeg") else {
        return;
    };
    let tmp = std::env::temp_dir();

    for &name in FIXTURES {
        let dir = fixture_dir();
        let h264 = dir.join(format!("{name}.h264"));
        let committed = std::fs::read(dir.join(format!("{name}.y8")))
            .unwrap_or_else(|e| panic!("read {name}.y8: {e}"));

        let out = tmp.join(format!("vidcull_cabac_{name}.y8"));
        let status = Command::new(&ffmpeg)
            .args(["-y", "-i"])
            .arg(&h264)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "extractplanes=y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "gray",
            ])
            .arg(&out)
            .output()
            .expect("run ffmpeg")
            .status;
        assert!(status.success(), "[{name}] ffmpeg reference decode failed");

        let fresh = std::fs::read(&out).expect("read fresh ffmpeg reference");
        let _ = std::fs::remove_file(&out);
        assert_eq!(
            committed, fresh,
            "[{name}] committed .y8 is stale vs ffmpeg — regenerate with scripts/gen-h264-cabac.ps1"
        );
    }
}
