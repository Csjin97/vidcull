use std::path::Path;

use vidcull_parser::h264::{BitReader, rbsp_from_ebsp};
use vidcull_parser::hevc::ctu::decode_slice_data_traced;
use vidcull_parser::hevc::nal::{NalUnitType, split_annex_b};
use vidcull_parser::hevc::params::{parse_pps, parse_sps};
use vidcull_parser::hevc::parse_slice_segment_header;

const CLIP: &[u8] = include_bytes!("fixtures/hevc-ctu/clip.hevc");
const GOLDEN: &str = include_str!("fixtures/hevc-ctu/trace.golden.txt");

#[derive(Debug, PartialEq, Eq)]
struct Op {
    kind: u8,
    ctx: i32,
    bin: u8,
}

fn parse_golden(text: &str) -> Vec<Op> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace();
            let kind = it.next().unwrap().as_bytes()[0];
            if kind == b'D' {
                let ctx: i32 = it.next().unwrap().parse().unwrap();
                let bin: u8 = it.next().unwrap().parse().unwrap();
                Op { kind, ctx, bin }
            } else {
                let bin: u8 = it.next().unwrap().parse().unwrap();
                Op { kind, ctx: -1, bin }
            }
        })
        .collect()
}

#[test]
fn ctu_parse_matches_ffmpeg_bin_trace() {
    let nals = split_annex_b(CLIP);
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

    let (stats, trace) =
        decode_slice_data_traced(&sps, &pps, &sh, &slice_rbsp).expect("CTU data parses");

    let expected = parse_golden(GOLDEN);

    for (i, (got, want)) in trace.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            (got.kind as char, got.ctx, got.bin),
            (want.kind as char, want.ctx, want.bin),
            "CABAC op #{i} diverged from the ffmpeg trace (got vs want)"
        );
    }
    assert_eq!(
        trace.len(),
        expected.len(),
        "decoded {} CABAC ops, ffmpeg trace has {}",
        trace.len(),
        expected.len()
    );

    assert_eq!(stats.ctb_count, 6, "3×2 CTBs at CTB size 64");
    assert_eq!(trace.last().map(|o| (o.kind, o.bin)), Some((b'T', 1)));
}

#[test]
fn fixture_is_present() {
    assert!(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hevc-ctu/clip.hevc")
            .exists()
    );
}
