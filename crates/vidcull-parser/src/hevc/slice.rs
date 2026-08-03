use vidcull_core::{Error, Result};

use crate::h264::BitReader;

use super::nal::{NalHeader, NalUnitType};
use super::params::{Pps, Sps, parse_short_term_rps};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

impl SliceType {
    fn from_raw(raw: u32) -> Result<Self> {
        match raw {
            0 => Ok(Self::B),
            1 => Ok(Self::P),
            2 => Ok(Self::I),
            other => Err(Error::Parse(format!("hevc: invalid slice_type {other}"))),
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceSegmentHeader {
    pub first_slice_segment_in_pic: bool,
    pub slice_type: SliceType,
    pub slice_pic_parameter_set_id: u32,
    pub slice_segment_address: u32,
    pub slice_qp: i32,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices_enabled: bool,
    pub data_byte_offset: usize,
    pub entry_point_offsets: Vec<u32>,
}

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

#[allow(clippy::too_many_lines)]
pub fn parse_slice_segment_header(
    r: &mut BitReader,
    sps: &Sps,
    pps: &Pps,
    nal: &NalHeader,
) -> Result<SliceSegmentHeader> {
    let nal_type = nal.unit_type;
    let is_irap = nal_type.is_irap();
    let is_idr = matches!(nal_type, NalUnitType::IdrWRadl | NalUnitType::IdrNLp);

    let first_slice_segment_in_pic = r.read_flag()?;
    if is_irap {
        let _no_output_of_prior_pics_flag = r.read_flag()?;
    }
    let slice_pic_parameter_set_id = r.ue()?;

    let mut dependent_slice_segment = false;
    let mut slice_segment_address = 0;
    if !first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled {
            dependent_slice_segment = r.read_flag()?;
        }
        let addr_bits = ceil_log2(sps.pic_size_in_ctbs_y());
        slice_segment_address = r.read_bits(addr_bits)?;
    }

    if dependent_slice_segment {
        return Err(Error::Unsupported(
            "hevc: dependent slice segments are not supported".into(),
        ));
    }

    for _ in 0..pps.num_extra_slice_header_bits {
        let _slice_reserved_flag = r.read_bit()?;
    }

    let slice_type = SliceType::from_raw(r.ue()?)?;
    if slice_type != SliceType::I {
        return Err(Error::Unsupported(format!(
            "hevc: only I slices are decodable, got {slice_type:?}"
        )));
    }

    if pps.output_flag_present {
        let _pic_output_flag = r.read_bit()?;
    }
    if sps.separate_colour_plane {
        let _colour_plane_id = r.read_bits(2)?;
    }

    if !is_idr {
        let _slice_pic_order_cnt_lsb = r.read_bits(sps.log2_max_poc_lsb)?;
        let short_term_ref_pic_set_sps_flag = r.read_flag()?;
        if short_term_ref_pic_set_sps_flag {
            if sps.num_short_term_ref_pic_sets > 1 {
                let idx_bits = ceil_log2(sps.num_short_term_ref_pic_sets);
                let _short_term_ref_pic_set_idx = r.read_bits(idx_bits)?;
            }
        } else {
            let idx = sps.num_short_term_ref_pic_sets;
            parse_short_term_rps(r, idx, idx, &sps.num_delta_pocs)?;
        }
        if sps.long_term_ref_pics_present {
            parse_slice_long_term_refs(r, sps)?;
        }
        if sps.sps_temporal_mvp_enabled {
            let _slice_temporal_mvp_enabled_flag = r.read_bit()?;
        }
    }

    let (mut sao_luma, mut sao_chroma) = (false, false);
    if sps.sao_enabled {
        sao_luma = r.read_flag()?;
        if sps.chroma_array_type() != 0 {
            sao_chroma = r.read_flag()?;
        }
    }

    let slice_qp_delta = r.se()?;
    let slice_qp = pps.init_qp + slice_qp_delta;

    if pps.slice_chroma_qp_offsets_present {
        let _slice_cb_qp_offset = r.se()?;
        let _slice_cr_qp_offset = r.se()?;
    }

    let mut deblocking_filter_disabled = pps.deblocking_filter_disabled;
    let mut beta_offset_div2 = pps.beta_offset_div2;
    let mut tc_offset_div2 = pps.tc_offset_div2;
    if pps.deblocking_filter_override_enabled && r.read_flag()? {
        deblocking_filter_disabled = r.read_flag()?;
        if !deblocking_filter_disabled {
            beta_offset_div2 = r.se()?;
            tc_offset_div2 = r.se()?;
        }
    }

    let mut loop_filter_across_slices_enabled = pps.loop_filter_across_slices_enabled;
    if pps.loop_filter_across_slices_enabled
        && (sao_luma || sao_chroma || !deblocking_filter_disabled)
    {
        loop_filter_across_slices_enabled = r.read_flag()?;
    }

    let mut entry_point_offsets = Vec::new();
    if pps.tiles_enabled || pps.entropy_coding_sync_enabled {
        let num_entry_point_offsets = r.ue()?;
        if num_entry_point_offsets > 0 {
            let offset_len = r.ue()? + 1;
            entry_point_offsets.reserve(num_entry_point_offsets as usize);
            for _ in 0..num_entry_point_offsets {
                entry_point_offsets.push(r.read_bits(offset_len)? + 1);
            }
        }
    }

    if pps.slice_segment_header_extension_present {
        let ext_len = r.ue()?;
        for _ in 0..ext_len {
            let _ext_data_byte = r.read_bits(8)?;
        }
    }

    let _alignment_bit_equal_to_one = r.read_bit()?;
    r.align_to_byte();
    let data_byte_offset = r.bit_pos() / 8;

    Ok(SliceSegmentHeader {
        first_slice_segment_in_pic,
        slice_type,
        slice_pic_parameter_set_id,
        slice_segment_address,
        slice_qp,
        sao_luma,
        sao_chroma,
        deblocking_filter_disabled,
        beta_offset_div2,
        tc_offset_div2,
        loop_filter_across_slices_enabled,
        data_byte_offset,
        entry_point_offsets,
    })
}

fn parse_slice_long_term_refs(r: &mut BitReader, sps: &Sps) -> Result<()> {
    let num_long_term_sps = if sps.num_long_term_ref_pics_sps > 0 {
        r.ue()?
    } else {
        0
    };
    let num_long_term_pics = r.ue()?;
    let lt_idx_bits = ceil_log2(sps.num_long_term_ref_pics_sps);
    for i in 0..(num_long_term_sps + num_long_term_pics) {
        if i < num_long_term_sps {
            if sps.num_long_term_ref_pics_sps > 1 {
                let _lt_idx_sps = r.read_bits(lt_idx_bits)?;
            }
        } else {
            let _poc_lsb_lt = r.read_bits(sps.log2_max_poc_lsb)?;
            let _used_by_curr_pic_lt_flag = r.read_bit()?;
        }
        let delta_poc_msb_present = r.read_flag()?;
        if delta_poc_msb_present {
            let _delta_poc_msb_cycle_lt = r.ue()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::rbsp_from_ebsp;
    use crate::h264::test_support::BitWriter;
    use crate::hevc::params::{Pps, Sps};

    #[test]
    fn ceil_log2_matches_uv_field_width() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(8), 3);
        assert_eq!(ceil_log2(9), 4);
    }

    fn test_sps() -> Sps {
        Sps {
            sps_id: 0,
            chroma_format_idc: 1,
            separate_colour_plane: false,
            pic_width: 160,
            pic_height: 96,
            conf_win_left: 0,
            conf_win_right: 0,
            conf_win_top: 0,
            conf_win_bottom: 0,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            log2_max_poc_lsb: 8,
            log2_min_cb_size: 3,
            log2_ctb_size: 6,
            log2_min_tb_size: 2,
            log2_max_tb_size: 5,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            scaling_list_enabled: false,
            sps_scaling_list_data_present: false,
            amp_enabled: false,
            sao_enabled: true,
            pcm: None,
            strong_intra_smoothing_enabled: true,
            num_short_term_ref_pic_sets: 0,
            num_delta_pocs: Vec::new(),
            long_term_ref_pics_present: false,
            num_long_term_ref_pics_sps: 0,
            sps_temporal_mvp_enabled: false,
        }
    }

    fn test_pps() -> Pps {
        Pps {
            pps_id: 0,
            sps_id: 0,
            dependent_slice_segments_enabled: false,
            output_flag_present: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding_enabled: true,
            cabac_init_present: false,
            init_qp: 26,
            constrained_intra_pred: false,
            transform_skip_enabled: false,
            cu_qp_delta_enabled: false,
            diff_cu_qp_delta_depth: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            slice_chroma_qp_offsets_present: false,
            transquant_bypass_enabled: false,
            tiles_enabled: false,
            entropy_coding_sync_enabled: false,
            loop_filter_across_slices_enabled: true,
            deblocking_filter_override_enabled: false,
            deblocking_filter_disabled: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            pps_scaling_list_data_present: false,
            lists_modification_present: false,
            log2_parallel_merge_level: 2,
            slice_segment_header_extension_present: false,
        }
    }

    fn idr_header() -> NalHeader {
        NalHeader {
            unit_type: NalUnitType::IdrNLp,
            layer_id: 0,
            temporal_id: 0,
        }
    }

    fn parse(rbsp: &[u8], sps: &Sps, pps: &Pps, nal: NalHeader) -> Result<SliceSegmentHeader> {
        let bytes = rbsp_from_ebsp(rbsp);
        parse_slice_segment_header(&mut BitReader::new(&bytes), sps, pps, &nal)
    }

    #[test]
    fn parses_synthetic_idr_i_slice() {
        let mut w = BitWriter::new();
        w.flag(true);
        w.flag(false);
        w.ue(0);
        w.ue(2);
        w.flag(true);
        w.flag(false);
        w.se(3);
        w.flag(true);
        let rbsp = w.into_rbsp();

        let sh = parse(&rbsp, &test_sps(), &test_pps(), idr_header()).expect("I slice parses");
        assert_eq!(sh.slice_type, SliceType::I);
        assert_eq!(sh.slice_qp, 29);
        assert!(sh.sao_luma);
        assert!(!sh.sao_chroma);
        assert!(sh.loop_filter_across_slices_enabled);
    }

    #[test]
    fn collects_wpp_entry_point_offsets() {
        let mut pps = test_pps();
        pps.entropy_coding_sync_enabled = true;
        let mut w = BitWriter::new();
        w.flag(true);
        w.flag(false);
        w.ue(0);
        w.ue(2);
        w.flag(true);
        w.flag(false);
        w.se(3);
        w.flag(true);
        w.ue(1);
        w.ue(7);
        w.bits(41, 8);
        let rbsp = w.into_rbsp();

        let sh = parse(&rbsp, &test_sps(), &pps, idr_header()).expect("WPP I slice parses");
        assert_eq!(sh.slice_qp, 29);
        assert_eq!(
            sh.entry_point_offsets,
            vec![42],
            "stored as substream byte length (minus1 + 1)"
        );
    }

    #[test]
    fn no_entry_points_when_wpp_off() {
        let mut w = BitWriter::new();
        w.flag(true);
        w.flag(false);
        w.ue(0);
        w.ue(2);
        w.flag(true);
        w.flag(false);
        w.se(3);
        w.flag(true);
        let rbsp = w.into_rbsp();
        let sh = parse(&rbsp, &test_sps(), &test_pps(), idr_header()).expect("I slice parses");
        assert!(sh.entry_point_offsets.is_empty());
    }

    #[test]
    fn rejects_p_slice() {
        let mut w = BitWriter::new();
        w.flag(true);
        w.flag(false);
        w.ue(0);
        w.ue(1);
        let rbsp = w.into_rbsp();
        let err =
            parse(&rbsp, &test_sps(), &test_pps(), idr_header()).expect_err("P slice rejected");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn rejects_dependent_slice_segment() {
        let mut pps = test_pps();
        pps.dependent_slice_segments_enabled = true;
        let mut w = BitWriter::new();
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.flag(true);
        w.bits(1, 3);
        let rbsp = w.into_rbsp();
        let err =
            parse(&rbsp, &test_sps(), &pps, idr_header()).expect_err("dependent slice rejected");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }
}
