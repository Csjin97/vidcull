use std::path::Path;

use vidcull_parser::h264::{BitReader, rbsp_from_ebsp};
use vidcull_parser::hevc::ctu::{CtuStats, decode_slice_data, decode_slice_to_luma};
use vidcull_parser::hevc::nal::{NalUnitType, split_annex_b};
use vidcull_parser::hevc::params::{parse_pps, parse_sps};
use vidcull_parser::hevc::parse_slice_segment_header;

const FIXTURES: &[(&[u8], &[u8], usize, usize)] = &[
    (
        include_bytes!("fixtures/hevc-recon/clip.hevc"),
        include_bytes!("fixtures/hevc-recon/recon.y8"),
        160,
        90,
    ),
    (
        include_bytes!("fixtures/hevc-recon/clip2.hevc"),
        include_bytes!("fixtures/hevc-recon/recon2.y8"),
        256,
        144,
    ),
];

#[test]
fn reconstruct_matches_ffmpeg_luma() {
    for &(clip, golden, w, h) in FIXTURES {
        reconstruct_one(clip, golden, w, h);
    }
}

fn reconstruct_one(clip: &[u8], golden: &[u8], width: usize, height: usize) -> CtuStats {
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

    let frame = decode_slice_to_luma(&sps, &pps, &sh, &slice_rbsp).expect("reconstruction runs");

    assert_eq!(
        (frame.width, frame.height),
        (width, height),
        "cropped luma size"
    );
    assert_eq!(
        frame.data.len(),
        golden.len(),
        "decoded {} luma samples, golden has {}",
        frame.data.len(),
        golden.len()
    );

    if frame.data != golden {
        let w = frame.width;
        let (i, (&got, &want)) = frame
            .data
            .iter()
            .zip(golden.iter())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .expect("vectors differ but no mismatch found");
        panic!(
            "luma diverged at ({x},{y}) [idx {i}]: got {got}, ffmpeg {want}",
            x = i % w,
            y = i / w,
        );
    }

    decode_slice_data(&sps, &pps, &sh, &slice_rbsp).expect("slice data decodes")
}

#[test]
fn transform_skip_reconstruct_matches_ffmpeg() {
    let stats = reconstruct_one(
        include_bytes!("fixtures/hevc-recon-tskip/clip_tskip.hevc"),
        include_bytes!("fixtures/hevc-recon-tskip/recon_tskip.y8"),
        160,
        96,
    );
    assert!(
        stats.transform_skip_count >= 1,
        "fixture exercises no transform_skip TU — skip-path coverage is vacuous"
    );
}

#[test]
fn cu_qp_delta_reconstruct_matches_ffmpeg() {
    let stats = reconstruct_one(
        include_bytes!("fixtures/hevc-recon-cuqp/clip_cuqp.hevc"),
        include_bytes!("fixtures/hevc-recon-cuqp/recon_cuqp.y8"),
        160,
        96,
    );
    assert!(
        stats.cu_qp_delta_count >= 1,
        "fixture codes no cu_qp_delta (count=0) — adaptive-QP coverage is vacuous"
    );
}

#[test]
fn cu_qp_delta_deblock_matches_ffmpeg() {
    let stats = reconstruct_one(
        include_bytes!("fixtures/hevc-recon-cuqp/clip_cuqp_db.hevc"),
        include_bytes!("fixtures/hevc-recon-cuqp/recon_cuqp_db.y8"),
        160,
        96,
    );
    assert!(stats.cu_qp_delta_count >= 1, "fixture codes no cu_qp_delta");
}

#[test]
fn transform_skip_and_cu_qp_delta_reconstruct_matches_ffmpeg() {
    let stats = reconstruct_one(
        include_bytes!("fixtures/hevc-recon-combo/clip_combo.hevc"),
        include_bytes!("fixtures/hevc-recon-combo/recon_combo.y8"),
        160,
        96,
    );
    assert!(
        stats.transform_skip_count >= 1,
        "combo fixture codes no transform_skip"
    );
    assert!(
        stats.cu_qp_delta_count >= 1,
        "combo fixture codes no cu_qp_delta"
    );
}

#[test]
fn fixtures_are_present() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hevc-recon");
    for f in ["clip.hevc", "recon.y8", "clip2.hevc", "recon2.y8"] {
        assert!(dir.join(f).exists(), "missing fixture {f}");
    }
}
