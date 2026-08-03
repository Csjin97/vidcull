use vidcull_core::{Error, Result};

use super::bitstream::{BitReader, rbsp_from_ebsp};

const HIGH_PROFILE_IDCS: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub seq_parameter_set_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma: u32,
    pub log2_max_frame_num: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub max_num_ref_frames: u32,
    pub pic_width_in_mbs: u32,
    pub pic_height_in_map_units: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
}

impl Sps {
    #[must_use]
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    #[must_use]
    pub fn luma_dimensions(&self) -> (u32, u32) {
        let width = self.pic_width_in_mbs * 16;
        let frame_height_in_mbs =
            (2 - u32::from(self.frame_mbs_only_flag)) * self.pic_height_in_map_units;
        let height = frame_height_in_mbs * 16;

        let (crop_unit_x, crop_unit_y) = if self.chroma_array_type() == 0 {
            (1, 2 - u32::from(self.frame_mbs_only_flag))
        } else {
            let (sub_width_c, sub_height_c) = match self.chroma_format_idc {
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            (
                sub_width_c,
                sub_height_c * (2 - u32::from(self.frame_mbs_only_flag)),
            )
        };

        let cropped_width = width.saturating_sub(
            crop_unit_x * (self.frame_crop_left_offset + self.frame_crop_right_offset),
        );
        let cropped_height = height.saturating_sub(
            crop_unit_y * (self.frame_crop_top_offset + self.frame_crop_bottom_offset),
        );
        (cropped_width, cropped_height)
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub pic_parameter_set_id: u32,
    pub seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub weighted_pred_flag: bool,
    pub pic_init_qp: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
}

pub fn parse_sps(reader: &mut BitReader) -> Result<Sps> {
    let profile_idc = u8::try_from(reader.read_bits(8)?)
        .map_err(|_| Error::Parse("h264 sps: profile_idc out of range".into()))?;
    let _constraint_flags_and_reserved = reader.read_bits(8)?;
    let level_idc = u8::try_from(reader.read_bits(8)?)
        .map_err(|_| Error::Parse("h264 sps: level_idc out of range".into()))?;
    let seq_parameter_set_id = reader.ue()?;

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane_flag = false;
    let mut bit_depth_luma = 8;
    if HIGH_PROFILE_IDCS.contains(&profile_idc) {
        chroma_format_idc = reader.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = reader.read_flag()?;
        }
        bit_depth_luma = 8 + reader.ue()?;
        let _bit_depth_chroma = 8 + reader.ue()?;
        let _qpprime_y_zero_transform_bypass_flag = reader.read_flag()?;
        let seq_scaling_matrix_present = reader.read_flag()?;
        if seq_scaling_matrix_present {
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            consume_scaling_matrices(reader, count)?;
        }
    }

    let log2_max_frame_num = 4 + reader.ue()?;
    let pic_order_cnt_type = reader.ue()?;
    let mut log2_max_pic_order_cnt_lsb = 0;
    let mut delta_pic_order_always_zero_flag = false;
    if pic_order_cnt_type == 0 {
        log2_max_pic_order_cnt_lsb = 4 + reader.ue()?;
    } else if pic_order_cnt_type == 1 {
        delta_pic_order_always_zero_flag = reader.read_flag()?;
        let _offset_for_non_ref_pic = reader.se()?;
        let _offset_for_top_to_bottom_field = reader.se()?;
        let num_ref_frames_in_cycle = reader.ue()?;
        for _ in 0..num_ref_frames_in_cycle {
            let _offset_for_ref_frame = reader.se()?;
        }
    }

    let max_num_ref_frames = reader.ue()?;
    let _gaps_in_frame_num_allowed = reader.read_flag()?;
    let pic_width_in_mbs = reader.ue()? + 1;
    let pic_height_in_map_units = reader.ue()? + 1;
    let frame_mbs_only_flag = reader.read_flag()?;
    let mut mb_adaptive_frame_field_flag = false;
    if !frame_mbs_only_flag {
        mb_adaptive_frame_field_flag = reader.read_flag()?;
    }
    let direct_8x8_inference_flag = reader.read_flag()?;

    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if reader.read_flag()? {
        left = reader.ue()?;
        right = reader.ue()?;
        top = reader.ue()?;
        bottom = reader.ue()?;
    }

    Ok(Sps {
        profile_idc,
        level_idc,
        seq_parameter_set_id,
        chroma_format_idc,
        separate_colour_plane_flag,
        bit_depth_luma,
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb,
        delta_pic_order_always_zero_flag,
        max_num_ref_frames,
        pic_width_in_mbs,
        pic_height_in_map_units,
        frame_mbs_only_flag,
        mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag,
        frame_crop_left_offset: left,
        frame_crop_right_offset: right,
        frame_crop_top_offset: top,
        frame_crop_bottom_offset: bottom,
    })
}

pub fn parse_pps(reader: &mut BitReader, chroma_format_idc: u32) -> Result<Pps> {
    let pic_parameter_set_id = reader.ue()?;
    let seq_parameter_set_id = reader.ue()?;
    let entropy_coding_mode_flag = reader.read_flag()?;
    let bottom_field_pic_order_in_frame_present_flag = reader.read_flag()?;

    let num_slice_groups = reader.ue()? + 1;
    if num_slice_groups > 1 {
        return Err(Error::Unsupported(
            "h264: FMO (multiple slice groups) not supported".into(),
        ));
    }

    let num_ref_idx_l0_default_active = reader.ue()? + 1;
    let num_ref_idx_l1_default_active = reader.ue()? + 1;
    let weighted_pred_flag = reader.read_flag()?;
    let _weighted_bipred_idc = reader.read_bits(2)?;
    let pic_init_qp = 26 + reader.se()?;
    let _pic_init_qs = 26 + reader.se()?;
    let chroma_qp_index_offset = reader.se()?;
    let deblocking_filter_control_present_flag = reader.read_flag()?;
    let constrained_intra_pred_flag = reader.read_flag()?;
    let redundant_pic_cnt_present_flag = reader.read_flag()?;

    let mut transform_8x8_mode_flag = false;
    if reader.more_rbsp_data() {
        transform_8x8_mode_flag = reader.read_flag()?;
        if reader.read_flag()? {
            let extra = if chroma_format_idc == 3 { 6 } else { 2 };
            let count = 6 + extra * u32::from(transform_8x8_mode_flag);
            consume_scaling_matrices(reader, count)?;
        }
        let _second_chroma_qp_index_offset = reader.se()?;
    }

    Ok(Pps {
        pic_parameter_set_id,
        seq_parameter_set_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_ref_idx_l0_default_active,
        num_ref_idx_l1_default_active,
        weighted_pred_flag,
        pic_init_qp,
        chroma_qp_index_offset,
        deblocking_filter_control_present_flag,
        constrained_intra_pred_flag,
        redundant_pic_cnt_present_flag,
        transform_8x8_mode_flag,
    })
}

fn consume_scaling_matrices(reader: &mut BitReader, count: u32) -> Result<()> {
    for i in 0..count {
        if reader.read_flag()? {
            let size = if i < 6 { 16 } else { 64 };
            consume_scaling_list(reader, size)?;
        }
    }
    Ok(())
}

fn consume_scaling_list(reader: &mut BitReader, size: usize) -> Result<()> {
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta_scale = reader.se()?;
            next_scale = (last_scale + delta_scale + 256).rem_euclid(256);
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcDecoderConfig {
    pub sps: Sps,
    pub pps: Pps,
    pub nal_length_size: usize,
}

pub fn parse_avcc(data: &[u8]) -> Result<AvcDecoderConfig> {
    if data.len() < 6 {
        return Err(Error::Parse(format!(
            "avcc: record too short ({} bytes, need >= 6)",
            data.len()
        )));
    }
    let nal_length_size = usize::from(data[4] & 0x3) + 1;

    let mut pos = 5;
    let sps_count = data[pos] & 0x1F;
    pos += 1;
    if sps_count == 0 {
        return Err(Error::Parse("avcc: zero SPS NAL units".into()));
    }
    let sps_nal = read_length_prefixed_nal(data, &mut pos, "SPS")?;
    let sps = parse_sps(&mut BitReader::new(&nal_rbsp(sps_nal, "SPS")?))?;
    for _ in 1..sps_count {
        let _ = read_length_prefixed_nal(data, &mut pos, "SPS")?;
    }

    if pos >= data.len() {
        return Err(Error::Parse("avcc: truncated before PPS count".into()));
    }
    let pps_count = data[pos];
    pos += 1;
    if pps_count == 0 {
        return Err(Error::Parse("avcc: zero PPS NAL units".into()));
    }
    let pps_nal = read_length_prefixed_nal(data, &mut pos, "PPS")?;
    let pps = parse_pps(
        &mut BitReader::new(&nal_rbsp(pps_nal, "PPS")?),
        sps.chroma_format_idc,
    )?;

    Ok(AvcDecoderConfig {
        sps,
        pps,
        nal_length_size,
    })
}

fn read_length_prefixed_nal<'a>(data: &'a [u8], pos: &mut usize, what: &str) -> Result<&'a [u8]> {
    if *pos + 2 > data.len() {
        return Err(Error::Parse(format!(
            "avcc: truncated {what} length field at offset {pos}"
        )));
    }
    let len = (usize::from(data[*pos]) << 8) | usize::from(data[*pos + 1]);
    *pos += 2;
    let end = *pos + len;
    if end > data.len() {
        return Err(Error::Parse(format!(
            "avcc: {what} length {len} overruns buffer ({} bytes remain)",
            data.len() - *pos
        )));
    }
    let nal = &data[*pos..end];
    *pos = end;
    Ok(nal)
}

fn nal_rbsp(nal: &[u8], what: &str) -> Result<Vec<u8>> {
    let ebsp = nal
        .get(1..)
        .ok_or_else(|| Error::Parse(format!("avcc: empty {what} NAL unit")))?;
    Ok(rbsp_from_ebsp(ebsp))
}

#[cfg(test)]
mod tests {
    use super::super::bitstream::test_support::BitWriter;
    use super::*;

    fn baseline_sps(
        width_mbs_minus1: u32,
        height_map_units_minus1: u32,
        crop: Option<[u32; 4]>,
    ) -> Sps {
        let mut w = BitWriter::new();
        w.bits(66, 8);
        w.bits(0, 8);
        w.bits(30, 8);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(1);
        w.flag(false);
        w.ue(width_mbs_minus1);
        w.ue(height_map_units_minus1);
        w.flag(true);
        w.flag(true);
        match crop {
            None => w.flag(false),
            Some([left, right, top, bottom]) => {
                w.flag(true);
                w.ue(left);
                w.ue(right);
                w.ue(top);
                w.ue(bottom);
            }
        }
        w.flag(false);
        let rbsp = w.into_rbsp();
        parse_sps(&mut BitReader::new(&rbsp)).expect("baseline SPS parses")
    }

    #[test]
    fn baseline_sps_dimensions_176x144() {
        let sps = baseline_sps(10, 8, None);
        assert_eq!(sps.profile_idc, 66);
        assert_eq!(sps.level_idc, 30);
        assert_eq!(sps.chroma_format_idc, 1, "default 4:2:0");
        assert!(sps.frame_mbs_only_flag);
        assert_eq!(sps.pic_width_in_mbs, 11);
        assert_eq!(sps.pic_height_in_map_units, 9);
        assert_eq!(sps.luma_dimensions(), (176, 144));
    }

    #[test]
    fn frame_cropping_trims_luma_dimensions() {
        let sps = baseline_sps(119, 67, Some([0, 0, 0, 4]));
        assert_eq!(sps.luma_dimensions(), (1920, 1080));
    }

    #[test]
    fn baseline_sps_matches_hand_computed_byte_vector() {
        let bytes = [0x42, 0x00, 0x1E, 0xF4, 0x16, 0x27, 0x20];
        let sps = parse_sps(&mut BitReader::new(&bytes)).expect("hand vector parses");
        assert_eq!(sps.luma_dimensions(), (176, 144));
        assert_eq!(sps.max_num_ref_frames, 1);
    }

    #[test]
    fn high_profile_sps_parses_chroma_and_bit_depth() {
        let mut w = BitWriter::new();
        w.bits(100, 8);
        w.bits(0, 8);
        w.bits(41, 8);
        w.ue(0);
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(2);
        w.flag(false);
        w.ue(119);
        w.ue(67);
        w.flag(true);
        w.flag(true);
        w.flag(true);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4);
        w.flag(false);
        let rbsp = w.into_rbsp();
        let sps = parse_sps(&mut BitReader::new(&rbsp)).expect("high SPS parses");
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(sps.bit_depth_luma, 8);
        assert_eq!(sps.luma_dimensions(), (1920, 1080));
    }

    #[test]
    fn pps_distinguishes_cavlc_from_cabac() {
        let build = |cabac: bool| {
            let mut w = BitWriter::new();
            w.ue(0);
            w.ue(0);
            w.flag(cabac);
            w.flag(false);
            w.ue(0);
            w.ue(0);
            w.ue(0);
            w.flag(false);
            w.bits(0, 2);
            w.se(0);
            w.se(0);
            w.se(0);
            w.flag(true);
            w.flag(false);
            w.flag(false);
            let rbsp = w.into_rbsp();
            parse_pps(&mut BitReader::new(&rbsp), 1).expect("PPS parses")
        };
        let cavlc = build(false);
        assert!(!cavlc.entropy_coding_mode_flag);
        assert_eq!(cavlc.pic_init_qp, 26);
        assert!(cavlc.deblocking_filter_control_present_flag);
        assert!(!cavlc.transform_8x8_mode_flag);

        let cabac = build(true);
        assert!(cabac.entropy_coding_mode_flag);
    }

    #[test]
    fn pps_rejects_fmo() {
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.ue(1);
        let rbsp = w.into_rbsp();
        let err = parse_pps(&mut BitReader::new(&rbsp), 1).expect_err("FMO rejected");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    const SPS_176X144_RBSP: [u8; 7] = [0x42, 0x00, 0x1E, 0xF4, 0x16, 0x27, 0x20];

    fn cavlc_pps_rbsp() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.bits(0, 2);
        w.se(0);
        w.se(0);
        w.se(0);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.into_rbsp()
    }

    fn build_avcc(sps_rbsp: &[u8], pps_rbsp: &[u8]) -> Vec<u8> {
        let sps_nal: Vec<u8> = std::iter::once(0x67)
            .chain(sps_rbsp.iter().copied())
            .collect();
        let pps_nal: Vec<u8> = std::iter::once(0x68)
            .chain(pps_rbsp.iter().copied())
            .collect();
        let mut v = vec![1, sps_nal[1], sps_nal[2], sps_nal[3], 0xFF, 0xE1];
        v.extend_from_slice(&u16::try_from(sps_nal.len()).unwrap().to_be_bytes());
        v.extend_from_slice(&sps_nal);
        v.push(1);
        v.extend_from_slice(&u16::try_from(pps_nal.len()).unwrap().to_be_bytes());
        v.extend_from_slice(&pps_nal);
        v
    }

    #[test]
    fn parse_avcc_extracts_first_sps_pps_and_length_size() {
        let avcc = build_avcc(&SPS_176X144_RBSP, &cavlc_pps_rbsp());
        let cfg = parse_avcc(&avcc).expect("avcc parses");
        assert_eq!(cfg.nal_length_size, 4);
        assert_eq!(cfg.sps.luma_dimensions(), (176, 144));
        assert!(!cfg.pps.entropy_coding_mode_flag);
        assert_eq!(cfg.pps.pic_init_qp, 26);
    }

    #[test]
    fn parse_avcc_reads_length_size_from_byte4() {
        let mut avcc = build_avcc(&SPS_176X144_RBSP, &cavlc_pps_rbsp());
        avcc[4] = 0xFC;
        assert_eq!(parse_avcc(&avcc).unwrap().nal_length_size, 1);
        avcc[4] = 0xFD;
        assert_eq!(parse_avcc(&avcc).unwrap().nal_length_size, 2);
    }

    #[test]
    fn parse_avcc_rejects_truncated_record() {
        assert!(parse_avcc(&[1, 66, 0, 30, 0xFF]).is_err());
    }

    #[test]
    fn parse_avcc_rejects_zero_parameter_sets() {
        assert!(parse_avcc(&[1, 66, 0, 30, 0xFF, 0xE0, 0, 0]).is_err());
    }

    #[test]
    fn parse_avcc_rejects_sps_length_overrun() {
        let mut avcc = build_avcc(&SPS_176X144_RBSP, &cavlc_pps_rbsp());
        avcc[6] = 0xFF;
        avcc[7] = 0xFF;
        assert!(parse_avcc(&avcc).is_err());
    }
}
