use vidcull_parser::h264::{BitReader, rbsp_from_ebsp};
use vidcull_parser::hevc::nal::parse_nal_header;
use vidcull_parser::hevc::{SliceType, parse_hvcc, parse_slice_segment_header};

const VPS_NAL: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x03, 0x00, 0x1e, 0x95, 0x98, 0x09,
];
const SPS_NAL: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x1e, 0xa0, 0x14, 0x20, 0x61, 0xf2, 0x65, 0x95, 0x9a, 0x49, 0x32, 0xbc, 0x05, 0xa0, 0x20,
    0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x03, 0x03, 0xc1,
];
const PPS_NAL: &[u8] = &[0x44, 0x01, 0xc1, 0x71, 0xa3, 0x12];
const IDR_SLICE_NAL: &[u8] = &[
    0x28, 0x01, 0xaf, 0x2d, 0x0c, 0x81, 0x58, 0x27, 0xe8, 0xaf, 0xf0, 0x5c, 0xb1, 0xc7, 0x63, 0xaa,
    0x6a, 0x28, 0xea, 0x3f, 0xd5, 0x22, 0xc3, 0xf4, 0x7f, 0xc2, 0xaf, 0x86, 0x1f, 0xcd, 0x79, 0x9f,
];
const CRA_SLICE_NAL: &[u8] = &[
    0x2a, 0x01, 0xac, 0x3c, 0x5a, 0x22, 0x11, 0xca, 0x43, 0x23, 0x76, 0x94, 0xa6, 0x5e, 0x66, 0x9c,
    0xe3, 0x80, 0x62, 0xaf, 0xac, 0xef, 0xbb, 0xa3, 0x58, 0x61, 0xb1, 0xd9, 0x75, 0xee, 0x28, 0x45,
];

fn build_hvcc() -> Vec<u8> {
    let mut v = vec![
        0x01, 0x01, 0x60, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0xf0, 0x00,
        0xfc, 0xfd, 0xf8, 0xf8, 0x00, 0x00, 0x0f, 0x03,
    ];
    for (nal_type, nal) in [(32u8, VPS_NAL), (33u8, SPS_NAL), (34u8, PPS_NAL)] {
        v.push(nal_type);
        v.extend_from_slice(&1u16.to_be_bytes());
        v.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
        v.extend_from_slice(nal);
    }
    v
}

fn parse_slice(nal: &[u8]) -> vidcull_parser::hevc::SliceSegmentHeader {
    let config = parse_hvcc(&build_hvcc()).expect("hvcC parses");
    let header = parse_nal_header(nal[0], nal[1]).expect("NAL header parses");
    let rbsp = rbsp_from_ebsp(&nal[2..]);
    parse_slice_segment_header(
        &mut BitReader::new(&rbsp),
        &config.sps,
        &config.pps,
        &header,
    )
    .expect("I-slice header parses")
}

#[test]
fn parses_idr_slice_header() {
    let sh = parse_slice(IDR_SLICE_NAL);
    assert!(sh.first_slice_segment_in_pic);
    assert_eq!(sh.slice_type, SliceType::I);
    assert_eq!(sh.slice_pic_parameter_set_id, 0);
    assert_eq!(sh.slice_segment_address, 0, "single-slice picture");
    assert_eq!(sh.slice_qp, 24, "init_qp 26 + slice_qp_delta -2");
    assert!(sh.sao_luma, "x265 default SAO on");
    assert!(sh.sao_chroma);
    assert!(!sh.deblocking_filter_disabled);
    assert_eq!(sh.data_byte_offset, 5, "CABAC data starts at byte 5");
}

#[test]
fn parses_cra_slice_header_with_poc_and_rps() {
    let sh = parse_slice(CRA_SLICE_NAL);
    assert!(sh.first_slice_segment_in_pic);
    assert_eq!(sh.slice_type, SliceType::I);
    assert_eq!(
        sh.slice_qp, 24,
        "constant QP carries through the CRA header"
    );
    assert!(sh.sao_luma);
    assert!(sh.sao_chroma);
    assert_eq!(sh.data_byte_offset, 9, "CABAC data starts at byte 9");
}

#[test]
fn idr_and_cra_agree_on_qp_and_sao() {
    let idr = parse_slice(IDR_SLICE_NAL);
    let cra = parse_slice(CRA_SLICE_NAL);
    assert_eq!(idr.slice_qp, cra.slice_qp);
    assert_eq!(idr.sao_luma, cra.sao_luma);
    assert_eq!(idr.sao_chroma, cra.sao_chroma);
}
