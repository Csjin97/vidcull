use vidcull_parser::hevc::parse_hvcc;

const VPS_NAL: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x03, 0x00, 0x1e, 0x92, 0x80, 0x90,
];
const SPS_NAL: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x1e, 0xa0, 0x14, 0x20, 0x61, 0xf2, 0x65, 0x92, 0xa4, 0x93, 0x2b, 0xc0, 0x5a, 0x02, 0x00,
    0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x10,
];
const PPS_NAL: &[u8] = &[0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40];

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

#[test]
fn parse_hvcc_extracts_geometry_and_flags() {
    let hvcc = build_hvcc();
    let config = parse_hvcc(&hvcc).expect("real libx265 hvcC must parse");

    assert_eq!(config.nal_length_size, 4, "lengthSizeMinusOne + 1");

    let sps = &config.sps;
    assert_eq!(sps.cropped_width(), 160, "displayed width");
    assert_eq!(sps.cropped_height(), 90, "displayed height");
    assert!(
        sps.pic_height >= 90 && sps.pic_height % 8 == 0,
        "coded height {} should be CB-aligned and cover 90",
        sps.pic_height
    );

    assert_eq!(sps.chroma_format_idc, 1, "4:2:0");
    assert_eq!(sps.bit_depth_luma, 8);
    assert_eq!(sps.bit_depth_chroma, 8);
    assert_eq!(sps.log2_max_poc_lsb, 8, "log2-max-poc-lsb=8");

    assert_eq!(sps.log2_ctb_size, 6, "CTU 64");
    assert_eq!(sps.ctb_size(), 64);
    assert_eq!(sps.log2_min_cb_size, 3, "min-cu-size=8");
    assert_eq!(sps.log2_min_tb_size, 2, "min TU 4");
    assert_eq!(sps.log2_max_tb_size, 5, "max-tu-size=32");
    assert_eq!(
        sps.max_transform_hierarchy_depth_intra, 0,
        "tu-intra-depth=1"
    );

    assert!(sps.sao_enabled, "sao");
    assert!(sps.strong_intra_smoothing_enabled, "strong-intra-smoothing");
    assert!(!sps.amp_enabled, "no-amp");
    assert!(sps.pcm.is_none(), "PCM not enabled");

    let pps = &config.pps;
    assert!(pps.sign_data_hiding_enabled, "signhide");
    assert!(!pps.transform_skip_enabled, "no-tskip");
    assert!(!pps.constrained_intra_pred, "no-constrained-intra");
    assert!(!pps.tiles_enabled, "single tile");
    assert_eq!(pps.beta_offset_div2, 0, "deblock=0:0");
    assert_eq!(pps.tc_offset_div2, 0, "deblock=0:0");
    assert_eq!(pps.sps_id, sps.sps_id, "PPS references the SPS");
    assert!(
        (1..=51).contains(&pps.init_qp),
        "init_qp {} in valid range",
        pps.init_qp
    );
}

#[test]
fn parse_hvcc_rejects_truncated_record() {
    assert!(parse_hvcc(&[0x01, 0x01, 0x60]).is_err());
}

#[test]
fn parse_hvcc_rejects_wrong_version() {
    let mut hvcc = build_hvcc();
    hvcc[0] = 0x02;
    assert!(parse_hvcc(&hvcc).is_err());
}
