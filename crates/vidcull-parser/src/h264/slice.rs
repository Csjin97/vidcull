use vidcull_core::{Error, Result};

use super::bitstream::BitReader;
use super::nal::{NalHeader, NalUnitType};
use super::params::{Pps, Sps};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn from_raw(raw: u32) -> Result<Self> {
        match raw % 5 {
            0 => Ok(Self::P),
            1 => Ok(Self::B),
            2 => Ok(Self::I),
            3 => Ok(Self::Sp),
            4 => Ok(Self::Si),
            _ => Err(Error::Parse("h264: impossible slice_type".into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pic_parameter_set_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: Option<u32>,
    pub pic_order_cnt_lsb: Option<u32>,
    pub slice_qp: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset: i32,
    pub slice_beta_offset: i32,
}

pub fn parse_slice_header(
    reader: &mut BitReader,
    sps: &Sps,
    pps: &Pps,
    nal: &NalHeader,
) -> Result<SliceHeader> {
    let first_mb_in_slice = reader.ue()?;
    let slice_type_raw = reader.ue()?;
    let slice_type = SliceType::from_raw(slice_type_raw)?;
    if slice_type != SliceType::I {
        return Err(Error::Unsupported(format!(
            "h264: only I slices are decodable, got slice_type {slice_type_raw}"
        )));
    }
    let pic_parameter_set_id = reader.ue()?;

    if sps.separate_colour_plane_flag {
        let _colour_plane_id = reader.read_bits(2)?;
    }

    let frame_num = reader.read_bits(sps.log2_max_frame_num)?;

    let mut field_pic_flag = false;
    if !sps.frame_mbs_only_flag {
        field_pic_flag = reader.read_flag()?;
        if field_pic_flag {
            let _bottom_field_flag = reader.read_flag()?;
        }
    }

    let idr_pic_flag = matches!(nal.unit_type, NalUnitType::IdrSlice);
    let idr_pic_id = if idr_pic_flag {
        Some(reader.ue()?)
    } else {
        None
    };

    let mut pic_order_cnt_lsb = None;
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = Some(reader.read_bits(sps.log2_max_pic_order_cnt_lsb)?);
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            let _delta_pic_order_cnt_bottom = reader.se()?;
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
        let _delta_pic_order_cnt_0 = reader.se()?;
        if pps.bottom_field_pic_order_in_frame_present_flag && !field_pic_flag {
            let _delta_pic_order_cnt_1 = reader.se()?;
        }
    }

    if pps.redundant_pic_cnt_present_flag {
        let _redundant_pic_cnt = reader.ue()?;
    }

    if nal.ref_idc != 0 {
        parse_dec_ref_pic_marking(reader, idr_pic_flag)?;
    }

    let slice_qp_delta = reader.se()?;
    let slice_qp = pps.pic_init_qp + slice_qp_delta;

    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset = 0;
    let mut slice_beta_offset = 0;
    if pps.deblocking_filter_control_present_flag {
        disable_deblocking_filter_idc = reader.ue()?;
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset = reader.se()? * 2;
            slice_beta_offset = reader.se()? * 2;
        }
    }

    Ok(SliceHeader {
        first_mb_in_slice,
        slice_type,
        pic_parameter_set_id,
        frame_num,
        idr_pic_id,
        pic_order_cnt_lsb,
        slice_qp,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset,
        slice_beta_offset,
    })
}

fn parse_dec_ref_pic_marking(reader: &mut BitReader, idr_pic_flag: bool) -> Result<()> {
    if idr_pic_flag {
        let _no_output_of_prior_pics_flag = reader.read_flag()?;
        let _long_term_reference_flag = reader.read_flag()?;
        return Ok(());
    }
    let adaptive_ref_pic_marking_mode_flag = reader.read_flag()?;
    if !adaptive_ref_pic_marking_mode_flag {
        return Ok(());
    }
    loop {
        let memory_management_control_operation = reader.ue()?;
        match memory_management_control_operation {
            0 => break,
            1 | 2 | 4 | 6 => {
                let _arg = reader.ue()?;
            }
            3 => {
                let _difference_of_pic_nums_minus1 = reader.ue()?;
                let _long_term_frame_idx = reader.ue()?;
            }
            5 => {}
            other => {
                return Err(Error::Parse(format!("h264: invalid mmco {other}")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::bitstream::test_support::BitWriter;
    use super::*;

    fn test_sps() -> Sps {
        Sps {
            profile_idc: 66,
            level_idc: 30,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            bit_depth_luma: 8,
            log2_max_frame_num: 4,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb: 4,
            delta_pic_order_always_zero_flag: false,
            max_num_ref_frames: 1,
            pic_width_in_mbs: 11,
            pic_height_in_map_units: 9,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
        }
    }

    fn test_pps() -> Pps {
        Pps {
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            entropy_coding_mode_flag: false,
            bottom_field_pic_order_in_frame_present_flag: false,
            num_ref_idx_l0_default_active: 1,
            num_ref_idx_l1_default_active: 1,
            weighted_pred_flag: false,
            pic_init_qp: 26,
            chroma_qp_index_offset: 0,
            deblocking_filter_control_present_flag: true,
            constrained_intra_pred_flag: false,
            redundant_pic_cnt_present_flag: false,
            transform_8x8_mode_flag: false,
        }
    }

    fn idr_header() -> NalHeader {
        NalHeader {
            ref_idc: 3,
            unit_type: NalUnitType::IdrSlice,
        }
    }

    #[test]
    fn parses_idr_i_slice_header() {
        let sps = test_sps();
        let pps = test_pps();
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(7);
        w.ue(0);
        w.bits(5, 4);
        w.ue(0);
        w.bits(0, 4);
        w.flag(false);
        w.flag(false);
        w.se(2);
        w.ue(0);
        w.se(-1);
        w.se(3);
        let rbsp = w.into_rbsp();

        let sh = parse_slice_header(&mut BitReader::new(&rbsp), &sps, &pps, &idr_header())
            .expect("I-slice header parses");
        assert_eq!(sh.first_mb_in_slice, 0);
        assert_eq!(sh.slice_type, SliceType::I);
        assert_eq!(sh.frame_num, 5);
        assert_eq!(sh.idr_pic_id, Some(0));
        assert_eq!(sh.pic_order_cnt_lsb, Some(0));
        assert_eq!(sh.slice_qp, 28);
        assert_eq!(sh.disable_deblocking_filter_idc, 0);
        assert_eq!(sh.slice_alpha_c0_offset, -2);
        assert_eq!(sh.slice_beta_offset, 6);
    }

    #[test]
    fn disable_deblocking_skips_offset_reads() {
        let sps = test_sps();
        let pps = test_pps();
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(2);
        w.ue(0);
        w.bits(0, 4);
        w.ue(3);
        w.bits(1, 4);
        w.flag(false);
        w.flag(false);
        w.se(0);
        w.ue(1);
        let rbsp = w.into_rbsp();

        let sh = parse_slice_header(&mut BitReader::new(&rbsp), &sps, &pps, &idr_header())
            .expect("parses with deblocking disabled");
        assert_eq!(sh.slice_qp, 26);
        assert_eq!(sh.disable_deblocking_filter_idc, 1);
        assert_eq!(sh.slice_alpha_c0_offset, 0, "offsets untouched");
        assert_eq!(sh.slice_beta_offset, 0);
    }

    #[test]
    fn rejects_non_intra_slice() {
        let sps = test_sps();
        let pps = test_pps();
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.ue(0);
        let rbsp = w.into_rbsp();
        let err = parse_slice_header(&mut BitReader::new(&rbsp), &sps, &pps, &idr_header())
            .expect_err("P slice rejected");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn non_reference_i_slice_skips_marking() {
        let sps = test_sps();
        let pps = test_pps();
        let nal = NalHeader {
            ref_idc: 0,
            unit_type: NalUnitType::NonIdrSlice,
        };
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(2);
        w.ue(0);
        w.bits(9, 4);
        w.bits(2, 4);
        w.se(-4);
        w.ue(2);
        w.se(0);
        w.se(0);
        let rbsp = w.into_rbsp();

        let sh = parse_slice_header(&mut BitReader::new(&rbsp), &sps, &pps, &nal)
            .expect("non-ref I slice parses");
        assert_eq!(sh.idr_pic_id, None);
        assert_eq!(sh.frame_num, 9);
        assert_eq!(sh.slice_qp, 22);
        assert_eq!(sh.disable_deblocking_filter_idc, 2);
    }
}
