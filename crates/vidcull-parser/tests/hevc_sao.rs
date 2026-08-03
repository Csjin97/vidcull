use std::path::Path;

use vidcull_parser::h264::{BitReader, rbsp_from_ebsp};
use vidcull_parser::hevc::ctu::decode_slice_to_luma;
use vidcull_parser::hevc::nal::{NalUnitType, split_annex_b};
use vidcull_parser::hevc::params::{parse_pps, parse_sps};
use vidcull_parser::hevc::parse_slice_segment_header;

const FIXTURES: &[(&[u8], &[u8], usize, usize)] = &[
    (
        include_bytes!("fixtures/hevc-sao/clip.hevc"),
        include_bytes!("fixtures/hevc-sao/clip.y8"),
        160,
        90,
    ),
    (
        include_bytes!("fixtures/hevc-sao/clip2.hevc"),
        include_bytes!("fixtures/hevc-sao/clip2.y8"),
        256,
        144,
    ),
];

#[test]
fn sao_matches_ffmpeg_luma() {
    for &(clip, golden, w, h) in FIXTURES {
        sao_one(clip, golden, w, h);
    }
}

fn sao_one(clip: &[u8], golden: &[u8], width: usize, height: usize) {
    let nals = split_annex_b(clip);
    let sps_nal = nals
        .iter()
        .find(|n| n.header.unit_type == NalUnitType::Sps)
        .expect("clip carries an SPS");
    let pps_nal = nals
        .iter()
        .find(|n| n.header.unit_type == NalUnitType::Pps)
        .expect("clip carries a PPS");
    let slice_nal = nals
        .iter()
        .find(|n| n.header.unit_type.is_irap())
        .expect("clip carries an IRAP slice");

    let sps =
        parse_sps(&mut BitReader::new(&rbsp_from_ebsp(&sps_nal.payload))).expect("SPS parses");
    let pps =
        parse_pps(&mut BitReader::new(&rbsp_from_ebsp(&pps_nal.payload))).expect("PPS parses");

    let slice_rbsp = rbsp_from_ebsp(&slice_nal.payload);
    let sh = parse_slice_segment_header(
        &mut BitReader::new(&slice_rbsp),
        &sps,
        &pps,
        &slice_nal.header,
    )
    .expect("slice header parses");

    assert!(sh.sao_luma, "fixture must enable luma SAO");

    let frame = decode_slice_to_luma(&sps, &pps, &sh, &slice_rbsp).expect("reconstruction runs");

    assert_eq!(
        (frame.width, frame.height),
        (width, height),
        "cropped luma size"
    );
    assert_eq!(frame.data.len(), golden.len(), "luma sample count");

    if frame.data != golden {
        let w = frame.width;
        let (i, (&got, &want)) = frame
            .data
            .iter()
            .zip(golden.iter())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .expect("vectors differ but no mismatch found");
        let mismatches = frame
            .data
            .iter()
            .zip(golden.iter())
            .filter(|(a, b)| a != b)
            .count();
        panic!(
            "luma diverged at ({x},{y}) [idx {i}]: got {got}, ffmpeg {want} \
             ({mismatches} total mismatches)",
            x = i % w,
            y = i / w,
        );
    }
}

#[test]
fn fixtures_are_present() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hevc-sao");
    for f in ["clip.hevc", "clip.y8", "clip2.hevc", "clip2.y8"] {
        assert!(dir.join(f).exists(), "missing fixture {f}");
    }
}
