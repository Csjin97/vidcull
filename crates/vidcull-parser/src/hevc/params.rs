use vidcull_core::{Error, Result};

use crate::h264::{BitReader, rbsp_from_ebsp};

use super::nal::{NalUnitType, parse_nal_header};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub sps_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub pic_width: u32,
    pub pic_height: u32,
    pub conf_win_left: u32,
    pub conf_win_right: u32,
    pub conf_win_top: u32,
    pub conf_win_bottom: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub log2_max_poc_lsb: u32,
    pub log2_min_cb_size: u32,
    pub log2_ctb_size: u32,
    pub log2_min_tb_size: u32,
    pub log2_max_tb_size: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub scaling_list_enabled: bool,
    pub sps_scaling_list_data_present: bool,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm: Option<PcmParams>,
    pub strong_intra_smoothing_enabled: bool,
    pub num_short_term_ref_pic_sets: u32,
    pub num_delta_pocs: Vec<u32>,
    pub long_term_ref_pics_present: bool,
    pub num_long_term_ref_pics_sps: u32,
    pub sps_temporal_mvp_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParams {
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub log2_min_pcm_cb_size: u32,
    pub log2_max_pcm_cb_size: u32,
    pub loop_filter_disabled: bool,
}

impl Sps {
    #[must_use]
    pub fn sub_width_c(&self) -> u32 {
        match self.chroma_format_idc {
            1 | 2 => 2,
            _ => 1,
        }
    }

    #[must_use]
    pub fn sub_height_c(&self) -> u32 {
        match self.chroma_format_idc {
            1 => 2,
            _ => 1,
        }
    }

    #[must_use]
    pub fn cropped_width(&self) -> u32 {
        self.pic_width
            .saturating_sub(self.sub_width_c() * (self.conf_win_left + self.conf_win_right))
    }

    #[must_use]
    pub fn cropped_height(&self) -> u32 {
        self.pic_height
            .saturating_sub(self.sub_height_c() * (self.conf_win_top + self.conf_win_bottom))
    }

    #[must_use]
    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_ctb_size
    }

    #[must_use]
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }

    #[must_use]
    pub fn pic_size_in_ctbs_y(&self) -> u32 {
        let ctb = self.ctb_size();
        let cols = self.pic_width.div_ceil(ctb);
        let rows = self.pic_height.div_ceil(ctb);
        cols * rows
    }
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub init_qp: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_override_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub pps_scaling_list_data_present: bool,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u32,
    pub slice_segment_header_extension_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcDecoderConfig {
    pub nal_length_size: usize,
    pub sps: Sps,
    pub pps: Pps,
}

pub fn parse_hvcc(data: &[u8]) -> Result<HevcDecoderConfig> {
    if data.len() < 23 {
        return Err(Error::Parse(format!(
            "hevc hvcC: record is {} bytes, need at least 23",
            data.len()
        )));
    }
    if data[0] != 1 {
        return Err(Error::Parse(format!(
            "hevc hvcC: unsupported configurationVersion {}",
            data[0]
        )));
    }
    let nal_length_size = usize::from(data[21] & 0x03) + 1;
    let num_arrays = data[22];

    let mut sps_rbsp: Option<Vec<u8>> = None;
    let mut pps_rbsp: Option<Vec<u8>> = None;

    let mut pos = 23usize;
    for _ in 0..num_arrays {
        if pos + 3 > data.len() {
            return Err(Error::Parse("hevc hvcC: truncated NAL array header".into()));
        }
        let nal_type = data[pos] & 0x3F;
        let num_nalus = (usize::from(data[pos + 1]) << 8) | usize::from(data[pos + 2]);
        pos += 3;

        for _ in 0..num_nalus {
            if pos + 2 > data.len() {
                return Err(Error::Parse("hevc hvcC: truncated NAL length".into()));
            }
            let nal_len = (usize::from(data[pos]) << 8) | usize::from(data[pos + 1]);
            pos += 2;
            let nal_end = pos + nal_len;
            if nal_end > data.len() {
                return Err(Error::Parse("hevc hvcC: NAL extends past record".into()));
            }
            let nal = &data[pos..nal_end];
            pos = nal_end;

            if nal.len() >= 2 {
                let payload_rbsp = rbsp_from_ebsp(&nal[2..]);
                match nal_type {
                    33 if sps_rbsp.is_none() => sps_rbsp = Some(payload_rbsp),
                    34 if pps_rbsp.is_none() => pps_rbsp = Some(payload_rbsp),
                    _ => {}
                }
            }
        }
    }

    let sps_rbsp = sps_rbsp
        .ok_or_else(|| Error::Unsupported("hevc hvcC: no SPS in configuration record".into()))?;
    let pps_rbsp = pps_rbsp
        .ok_or_else(|| Error::Unsupported("hevc hvcC: no PPS in configuration record".into()))?;

    let sps = parse_sps(&mut BitReader::new(&sps_rbsp))?;
    let pps = parse_pps(&mut BitReader::new(&pps_rbsp))?;

    Ok(HevcDecoderConfig {
        nal_length_size,
        sps,
        pps,
    })
}

pub fn parse_sps_nal(nal: &[u8]) -> Result<Sps> {
    if nal.len() < 2 {
        return Err(Error::Parse("hevc: SPS NAL too short".into()));
    }
    let header = parse_nal_header(nal[0], nal[1])?;
    if header.unit_type != NalUnitType::Sps {
        return Err(Error::Parse(format!(
            "hevc: expected SPS NAL, got {:?}",
            header.unit_type
        )));
    }
    let rbsp = rbsp_from_ebsp(&nal[2..]);
    parse_sps(&mut BitReader::new(&rbsp))
}

const MAX_LUMA_SAMPLES: u64 = 35_651_584;

#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn parse_sps(r: &mut BitReader) -> Result<Sps> {
    let _sps_video_parameter_set_id = r.read_bits(4)?;
    let sps_max_sub_layers_minus1 = r.read_bits(3)?;
    let _sps_temporal_id_nesting_flag = r.read_bit()?;

    profile_tier_level(r, sps_max_sub_layers_minus1)?;

    let sps_id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    let separate_colour_plane = if chroma_format_idc == 3 {
        r.read_flag()?
    } else {
        false
    };

    let pic_width = r.ue()?;
    let pic_height = r.ue()?;

    let (conf_win_left, conf_win_right, conf_win_top, conf_win_bottom) = if r.read_flag()? {
        (r.ue()?, r.ue()?, r.ue()?, r.ue()?)
    } else {
        (0, 0, 0, 0)
    };

    let bit_depth_luma = r.ue()? + 8;
    let bit_depth_chroma = r.ue()? + 8;
    let log2_max_poc_lsb = r.ue()? + 4;

    let sub_layer_ordering_present = r.read_flag()?;
    let first = if sub_layer_ordering_present {
        0
    } else {
        sps_max_sub_layers_minus1
    };
    for _ in first..=sps_max_sub_layers_minus1 {
        let _max_dec_pic_buffering_minus1 = r.ue()?;
        let _max_num_reorder_pics = r.ue()?;
        let _max_latency_increase_plus1 = r.ue()?;
    }

    let log2_min_cb_size = r.ue()? + 3;
    let log2_ctb_size = log2_min_cb_size + r.ue()?;
    let log2_min_tb_size = r.ue()? + 2;
    let log2_max_tb_size = log2_min_tb_size + r.ue()?;
    let max_transform_hierarchy_depth_inter = r.ue()?;
    let max_transform_hierarchy_depth_intra = r.ue()?;

    if pic_width == 0 || pic_height == 0 {
        return Err(Error::Parse(format!(
            "hevc sps: zero dimension {pic_width}x{pic_height}"
        )));
    }
    if u64::from(pic_width)
        .checked_mul(u64::from(pic_height))
        .is_none_or(|s| s > MAX_LUMA_SAMPLES)
    {
        return Err(Error::Unsupported(format!(
            "hevc sps: dimensions {pic_width}x{pic_height} exceed the native decoder limit"
        )));
    }
    if !(4..=6).contains(&log2_ctb_size) {
        return Err(Error::Parse(format!(
            "hevc sps: log2_ctb_size {log2_ctb_size} out of range [4,6]"
        )));
    }
    if !(3..=6).contains(&log2_min_cb_size) || log2_min_cb_size > log2_ctb_size {
        return Err(Error::Parse(format!(
            "hevc sps: log2_min_cb_size {log2_min_cb_size} invalid (ctb {log2_ctb_size})"
        )));
    }
    if !(2..=5).contains(&log2_min_tb_size) {
        return Err(Error::Parse(format!(
            "hevc sps: log2_min_tb_size {log2_min_tb_size} out of range [2,5]"
        )));
    }
    if !(2..=5).contains(&log2_max_tb_size)
        || log2_max_tb_size < log2_min_tb_size
        || log2_max_tb_size > log2_ctb_size
    {
        return Err(Error::Parse(format!(
            "hevc sps: log2_max_tb_size {log2_max_tb_size} invalid \
             (min {log2_min_tb_size}, ctb {log2_ctb_size})"
        )));
    }

    let scaling_list_enabled = r.read_flag()?;
    let sps_scaling_list_data_present = if scaling_list_enabled {
        let present = r.read_flag()?;
        if present {
            skip_scaling_list_data(r)?;
        }
        present
    } else {
        false
    };

    let amp_enabled = r.read_flag()?;
    let sao_enabled = r.read_flag()?;

    let pcm = if r.read_flag()? {
        let bit_depth_luma = r.read_bits(4)? + 1;
        let bit_depth_chroma = r.read_bits(4)? + 1;
        let log2_min_pcm_cb_size = r.ue()? + 3;
        let log2_max_pcm_cb_size = log2_min_pcm_cb_size + r.ue()?;
        let loop_filter_disabled = r.read_flag()?;
        Some(PcmParams {
            bit_depth_luma,
            bit_depth_chroma,
            log2_min_pcm_cb_size,
            log2_max_pcm_cb_size,
            loop_filter_disabled,
        })
    } else {
        None
    };

    let num_short_term_rps = r.ue()?;
    if num_short_term_rps > 64 {
        return Err(Error::Parse(format!(
            "hevc sps: num_short_term_ref_pic_sets {num_short_term_rps} > 64"
        )));
    }
    let mut num_delta_pocs = vec![0u32; num_short_term_rps as usize];
    for idx in 0..num_short_term_rps {
        num_delta_pocs[idx as usize] =
            parse_short_term_rps(r, idx, num_short_term_rps, &num_delta_pocs)?;
    }

    let long_term_ref_pics_present = r.read_flag()?;
    let num_long_term_ref_pics_sps = if long_term_ref_pics_present {
        let num_long_term = r.ue()?;
        for _ in 0..num_long_term {
            let _lt_ref_pic_poc_lsb = r.read_bits(log2_max_poc_lsb)?;
            let _used_by_curr_pic_lt = r.read_bit()?;
        }
        num_long_term
    } else {
        0
    };

    let sps_temporal_mvp_enabled = r.read_flag()?;
    let strong_intra_smoothing_enabled = r.read_flag()?;

    Ok(Sps {
        sps_id,
        chroma_format_idc,
        separate_colour_plane,
        pic_width,
        pic_height,
        conf_win_left,
        conf_win_right,
        conf_win_top,
        conf_win_bottom,
        bit_depth_luma,
        bit_depth_chroma,
        log2_max_poc_lsb,
        log2_min_cb_size,
        log2_ctb_size,
        log2_min_tb_size,
        log2_max_tb_size,
        max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra,
        scaling_list_enabled,
        sps_scaling_list_data_present,
        amp_enabled,
        sao_enabled,
        pcm,
        strong_intra_smoothing_enabled,
        num_short_term_ref_pic_sets: num_short_term_rps,
        num_delta_pocs,
        long_term_ref_pics_present,
        num_long_term_ref_pics_sps,
        sps_temporal_mvp_enabled,
    })
}

fn profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<()> {
    r.skip_bits(2 + 1 + 5 + 32 + 4 + 44 + 8)?;

    if max_sub_layers_minus1 > 0 {
        let n = max_sub_layers_minus1 as usize;
        let mut profile_present = [false; 8];
        let mut level_present = [false; 8];
        for i in 0..n {
            profile_present[i] = r.read_flag()?;
            level_present[i] = r.read_flag()?;
        }
        for _ in n..8 {
            r.skip_bits(2)?;
        }
        for i in 0..n {
            if profile_present[i] {
                r.skip_bits(2 + 1 + 5 + 32 + 4 + 44)?;
            }
            if level_present[i] {
                r.skip_bits(8)?;
            }
        }
    }
    Ok(())
}

fn skip_scaling_list_data(r: &mut BitReader) -> Result<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0u32;
        while matrix_id < 6 {
            let pred_mode = r.read_flag()?;
            if pred_mode {
                let coef_num = core::cmp::min(64u32, 1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    let _dc_coef_minus8 = r.se()?;
                }
                for _ in 0..coef_num {
                    let _delta_coef = r.se()?;
                }
            } else {
                let _pred_matrix_id_delta = r.ue()?;
            }
            matrix_id += step;
        }
    }
    Ok(())
}

pub(crate) fn parse_short_term_rps(
    r: &mut BitReader,
    idx: u32,
    num_rps: u32,
    prev: &[u32],
) -> Result<u32> {
    let inter_pred = if idx != 0 { r.read_flag()? } else { false };

    if inter_pred {
        let delta_idx_minus1 = if idx == num_rps { r.ue()? } else { 0 };
        let _delta_rps_sign = r.read_bit()?;
        let _abs_delta_rps_minus1 = r.ue()?;
        let ref_rps_idx = idx
            .checked_sub(delta_idx_minus1 + 1)
            .ok_or_else(|| Error::Parse("hevc rps: RefRpsIdx underflow".into()))?;
        let ref_num_delta = *prev.get(ref_rps_idx as usize).ok_or_else(|| {
            Error::Parse(format!("hevc rps: RefRpsIdx {ref_rps_idx} out of range"))
        })?;
        let mut count = 0u32;
        for _ in 0..=ref_num_delta {
            let used = r.read_flag()?;
            let use_delta = if used { true } else { r.read_flag()? };
            if used || use_delta {
                count += 1;
            }
        }
        Ok(count)
    } else {
        let num_negative = r.ue()?;
        let num_positive = r.ue()?;
        for _ in 0..num_negative {
            let _delta_poc_s0_minus1 = r.ue()?;
            let _used_by_curr_pic_s0 = r.read_bit()?;
        }
        for _ in 0..num_positive {
            let _delta_poc_s1_minus1 = r.ue()?;
            let _used_by_curr_pic_s1 = r.read_bit()?;
        }
        Ok(num_negative + num_positive)
    }
}

pub fn parse_pps(r: &mut BitReader) -> Result<Pps> {
    let pps_id = r.ue()?;
    let sps_id = r.ue()?;
    let dependent_slice_segments_enabled = r.read_flag()?;
    let output_flag_present = r.read_flag()?;
    let num_extra_slice_header_bits = r.read_bits(3)?;
    let sign_data_hiding_enabled = r.read_flag()?;
    let cabac_init_present = r.read_flag()?;
    let _num_ref_idx_l0_default_active_minus1 = r.ue()?;
    let _num_ref_idx_l1_default_active_minus1 = r.ue()?;
    let init_qp = r.se()? + 26;
    let constrained_intra_pred = r.read_flag()?;
    let transform_skip_enabled = r.read_flag()?;
    let cu_qp_delta_enabled = r.read_flag()?;
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled { r.ue()? } else { 0 };
    let cb_qp_offset = r.se()?;
    let cr_qp_offset = r.se()?;
    let slice_chroma_qp_offsets_present = r.read_flag()?;
    let _weighted_pred = r.read_bit()?;
    let _weighted_bipred = r.read_bit()?;
    let transquant_bypass_enabled = r.read_flag()?;
    let tiles_enabled = r.read_flag()?;
    let entropy_coding_sync_enabled = r.read_flag()?;

    if tiles_enabled {
        let num_tile_columns_minus1 = r.ue()?;
        let num_tile_rows_minus1 = r.ue()?;
        let uniform_spacing = r.read_flag()?;
        if !uniform_spacing {
            for _ in 0..num_tile_columns_minus1 {
                let _column_width_minus1 = r.ue()?;
            }
            for _ in 0..num_tile_rows_minus1 {
                let _row_height_minus1 = r.ue()?;
            }
        }
        let _loop_filter_across_tiles_enabled = r.read_bit()?;
    }

    let loop_filter_across_slices_enabled = r.read_flag()?;

    let (
        deblocking_filter_override_enabled,
        deblocking_filter_disabled,
        beta_offset_div2,
        tc_offset_div2,
    ) = if r.read_flag()? {
        let override_enabled = r.read_flag()?;
        let disabled = r.read_flag()?;
        let (beta, tc) = if disabled { (0, 0) } else { (r.se()?, r.se()?) };
        (override_enabled, disabled, beta, tc)
    } else {
        (false, false, 0, 0)
    };

    let pps_scaling_list_data_present = r.read_flag()?;
    if pps_scaling_list_data_present {
        skip_scaling_list_data(r)?;
    }

    let lists_modification_present = r.read_flag()?;
    let log2_parallel_merge_level = r.ue()? + 2;
    let slice_segment_header_extension_present = r.read_flag()?;

    Ok(Pps {
        pps_id,
        sps_id,
        dependent_slice_segments_enabled,
        output_flag_present,
        num_extra_slice_header_bits,
        sign_data_hiding_enabled,
        cabac_init_present,
        init_qp,
        constrained_intra_pred,
        transform_skip_enabled,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        transquant_bypass_enabled,
        tiles_enabled,
        entropy_coding_sync_enabled,
        loop_filter_across_slices_enabled,
        deblocking_filter_override_enabled,
        deblocking_filter_disabled,
        beta_offset_div2,
        tc_offset_div2,
        pps_scaling_list_data_present,
        lists_modification_present,
        log2_parallel_merge_level,
        slice_segment_header_extension_present,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::similar_names)]
    use super::*;
    use crate::h264::test_support::BitWriter;

    fn sps_prefix(
        pic_width: u32,
        pic_height: u32,
        log2_min_cb_minus3: u32,
        log2_ctb_diff: u32,
        log2_min_tb_minus2: u32,
        log2_max_tb_diff: u32,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(0, 4);
        w.bits(0, 3);
        w.bit(0);
        w.bits(0, 32);
        w.bits(0, 32);
        w.bits(0, 32);
        w.ue(0);
        w.ue(1);
        w.ue(pic_width);
        w.ue(pic_height);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(log2_min_cb_minus3);
        w.ue(log2_ctb_diff);
        w.ue(log2_min_tb_minus2);
        w.ue(log2_max_tb_diff);
        w.ue(0);
        w.ue(0);
        w.into_rbsp()
    }

    #[test]
    fn parse_sps_rejects_out_of_range_log2_ctb() {
        let rbsp = sps_prefix(64, 64, 0, 4, 0, 0);
        let err = parse_sps(&mut BitReader::new(&rbsp)).expect_err("must reject");
        assert!(err.to_string().contains("log2_ctb_size"), "got: {err}");
    }

    #[test]
    fn parse_sps_rejects_oversized_transform_block() {
        let rbsp = sps_prefix(64, 64, 0, 3, 0, 4);
        let err = parse_sps(&mut BitReader::new(&rbsp)).expect_err("must reject");
        assert!(err.to_string().contains("log2_max_tb_size"), "got: {err}");
    }

    #[test]
    fn parse_sps_rejects_zero_dimension() {
        let rbsp = sps_prefix(0, 64, 0, 3, 0, 0);
        let err = parse_sps(&mut BitReader::new(&rbsp)).expect_err("must reject");
        assert!(err.to_string().contains("zero dimension"), "got: {err}");
    }

    #[test]
    fn parse_sps_accepts_in_range_log2_sizes() {
        let rbsp = sps_prefix(64, 64, 0, 3, 0, 3);
        let err = parse_sps(&mut BitReader::new(&rbsp))
            .expect_err("prefix is truncated after the validated fields");
        let msg = err.to_string();
        assert!(
            !msg.contains("log2_") && !msg.contains("dimension"),
            "valid sizes must pass the guard; got: {msg}"
        );
    }
}
