#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::if_not_else
)]

use vidcull_core::{Error, Result};

use crate::h264::LumaFrame;

use super::cabac::{CabacDecoder, NUM_CTX, SyntaxElement};
use super::params::{Pps, Sps};
use super::slice::SliceSegmentHeader;
use super::{intra, transform};

const INTRA_PLANAR: u8 = 0;
const INTRA_DC: u8 = 1;
const INTRA_ANGULAR_26: u8 = 26;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CabacTraceOp {
    pub kind: u8,
    pub ctx: i32,
    pub bin: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CtuStats {
    pub ctb_count: u32,
    pub cu_count: u32,
    pub tu_count: u32,
    pub coeff_count: u64,
    pub transform_skip_count: u32,
    pub cu_qp_delta_count: u32,
    pub end_bit_pos: usize,
}

fn diag_scan(n: usize) -> Vec<(u8, u8)> {
    let mut out = Vec::with_capacity(n * n);
    let (mut x, mut y) = (0i32, 0i32);
    loop {
        while y >= 0 {
            if (x as usize) < n && (y as usize) < n {
                out.push((x as u8, y as u8));
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
        if out.len() >= n * n {
            break;
        }
    }
    out
}

fn horiz_scan(n: usize) -> Vec<(u8, u8)> {
    let mut out = Vec::with_capacity(n * n);
    for y in 0..n {
        for x in 0..n {
            out.push((x as u8, y as u8));
        }
    }
    out
}

fn vert_scan(n: usize) -> Vec<(u8, u8)> {
    let mut out = Vec::with_capacity(n * n);
    for x in 0..n {
        for y in 0..n {
            out.push((x as u8, y as u8));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanType {
    Diag,
    Horiz,
    Vert,
}

struct Scan {
    within: Vec<(u8, u8)>,
    groups: Vec<(u8, u8)>,
    within_inv: [[u8; 4]; 4],
    cg_side: usize,
    group_inv: Vec<u16>,
}

impl Scan {
    fn build(scan_type: ScanType, log2_size: u32) -> Self {
        let size = 1usize << log2_size;
        let cg_side = (size / 4).max(1);
        let (within, groups) = match scan_type {
            ScanType::Diag => (diag_scan(4), diag_scan(cg_side)),
            ScanType::Horiz => (horiz_scan(4), horiz_scan(cg_side)),
            ScanType::Vert => (vert_scan(4), vert_scan(cg_side)),
        };
        let mut within_inv = [[0u8; 4]; 4];
        for (i, &(x, y)) in within.iter().enumerate() {
            within_inv[y as usize][x as usize] = i as u8;
        }
        let mut group_inv = vec![0u16; cg_side * cg_side];
        for (i, &(x, y)) in groups.iter().enumerate() {
            group_inv[y as usize * cg_side + x as usize] = i as u16;
        }
        Self {
            within,
            groups,
            within_inv,
            cg_side,
            group_inv,
        }
    }
}

#[rustfmt::skip]
const SIG_CTX_IDX_MAP: [[u8; 16]; 5] = [
    [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8], 
    [1, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0], 
    [2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0], 
    [2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0], 
    [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2], 
];

const CABAC_MAX_BIN: u32 = 31;

#[rustfmt::skip]
const BETA_TABLE: [u8; 52] = [
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  6,  7,  8,
     9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36,
    38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64,
];

#[rustfmt::skip]
const TC_TABLE: [u8; 54] = [
    0, 0, 0, 0, 0, 0, 0,  0,  0,  0,  0,  0,  0,  0,  0,  0, 0, 0, 1,
    1, 1, 1, 1, 1, 1, 1,  1,  2,  2,  2,  2,  3,  3,  3,  3, 4, 4, 4,
    5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24,
];

const DEFAULT_INTRA_TC_OFFSET: i32 = 2;
const MAX_QP: i32 = 51;

#[derive(Clone, Copy, Default)]
struct SaoLuma {
    type_idx: u8,
    offset_val: [i32; 5],
    band_position: u8,
    eo_class: u8,
}

const SAO_EDGE_IDX: [usize; 5] = [1, 2, 0, 3, 4];

const SAO_EO_POS: [[(i32, i32); 2]; 4] = [
    [(-1, 0), (1, 0)],
    [(0, -1), (0, 1)],
    [(-1, -1), (1, 1)],
    [(1, -1), (-1, 1)],
];

struct CtuDecoder<'a> {
    cabac: CabacDecoder<'a>,
    sps: &'a Sps,
    sh: &'a SliceSegmentHeader,
    trace: Option<Vec<CabacTraceOp>>,
    sign_data_hiding: bool,
    transform_skip_enabled: bool,
    tu_transform_skip: bool,

    cu_qp_delta_enabled: bool,
    log2_min_cu_qp_delta_size: u32,
    is_cu_qp_delta_coded: bool,
    cu_qp_delta_val: i32,
    qg_x: u32,
    qg_y: u32,
    qp_y_prev: i32,
    last_qp_y: i32,
    cur_cu_qp_y: i32,
    cur_cu_qp_set: bool,
    tab_qp_y: Vec<i8>,

    log2_ctb: u32,
    log2_min_cb: u32,
    log2_min_pu: u32,
    log2_min_tb: u32,
    log2_max_tb: u32,
    max_trafo_depth_intra: u32,
    width: u32,
    height: u32,
    bit_depth: u8,
    min_cb_width: usize,
    min_pu_width: usize,

    luma: Vec<u8>,
    coeff_buf: Vec<i32>,

    bs_w: usize,
    vert_bs: Vec<u8>,
    horiz_bs: Vec<u8>,

    ctb_cols: usize,
    ctb_rows: usize,
    sao_luma_ctb: Vec<SaoLuma>,

    wpp: bool,
    wpp_saved_ctx: Option<([u8; NUM_CTX], [u8; NUM_CTX])>,
    wpp_row_offsets: Vec<usize>,

    tab_ct_depth: Vec<u8>,
    tab_ipm: Vec<u8>,
    ctb_left: bool,
    ctb_up: bool,

    pu_intra_mode: [u8; 4],
    pu_intra_mode_c: u8,
    cu_intra_split: bool,
    cu_max_trafo_depth: u32,
    tu_intra_mode: u8,
    tu_intra_mode_c: u8,

    stats: CtuStats,
}

impl<'a> CtuDecoder<'a> {
    fn new(
        cabac: CabacDecoder<'a>,
        sps: &'a Sps,
        sh: &'a SliceSegmentHeader,
        wpp: bool,
        wpp_row_offsets: Vec<usize>,
        trace: Option<Vec<CabacTraceOp>>,
    ) -> Self {
        let log2_min_cb = sps.log2_min_cb_size;
        let log2_min_pu = log2_min_cb - 1;
        let min_cb_width = (sps.pic_width >> log2_min_cb) as usize;
        let min_cb_height = (sps.pic_height >> log2_min_cb) as usize;
        let min_pu_width = (sps.pic_width >> log2_min_pu) as usize;
        let min_pu_height = (sps.pic_height >> log2_min_pu) as usize;
        Self {
            cabac,
            sps,
            sh,
            trace,
            sign_data_hiding: false,
            transform_skip_enabled: false,
            tu_transform_skip: false,
            cu_qp_delta_enabled: false,
            log2_min_cu_qp_delta_size: sps.log2_ctb_size,
            is_cu_qp_delta_coded: false,
            cu_qp_delta_val: 0,
            qg_x: 0,
            qg_y: 0,
            qp_y_prev: sh.slice_qp,
            last_qp_y: sh.slice_qp,
            cur_cu_qp_y: sh.slice_qp,
            cur_cu_qp_set: false,
            tab_qp_y: vec![sh.slice_qp as i8; (sps.pic_width / 4 * (sps.pic_height / 4)) as usize],
            log2_ctb: sps.log2_ctb_size,
            log2_min_cb,
            log2_min_pu,
            log2_min_tb: sps.log2_min_tb_size,
            log2_max_tb: sps.log2_max_tb_size,
            max_trafo_depth_intra: sps.max_transform_hierarchy_depth_intra,
            width: sps.pic_width,
            height: sps.pic_height,
            bit_depth: sps.bit_depth_luma as u8,
            min_cb_width,
            min_pu_width,
            luma: vec![0u8; (sps.pic_width * sps.pic_height) as usize],
            coeff_buf: vec![0i32; 32 * 32],
            bs_w: (sps.pic_width / 4) as usize,
            vert_bs: vec![0u8; (sps.pic_width / 4 * (sps.pic_height / 4)) as usize],
            horiz_bs: vec![0u8; (sps.pic_width / 4 * (sps.pic_height / 4)) as usize],
            ctb_cols: sps.pic_width.div_ceil(1 << sps.log2_ctb_size) as usize,
            ctb_rows: sps.pic_height.div_ceil(1 << sps.log2_ctb_size) as usize,
            sao_luma_ctb: vec![
                SaoLuma::default();
                (sps.pic_width.div_ceil(1 << sps.log2_ctb_size)
                    * sps.pic_height.div_ceil(1 << sps.log2_ctb_size))
                    as usize
            ],
            wpp,
            wpp_saved_ctx: None,
            wpp_row_offsets,
            tab_ct_depth: vec![0u8; min_cb_width * min_cb_height],
            tab_ipm: vec![INTRA_DC; min_pu_width * min_pu_height],
            ctb_left: false,
            ctb_up: false,
            pu_intra_mode: [INTRA_DC; 4],
            pu_intra_mode_c: INTRA_DC,
            cu_intra_split: false,
            cu_max_trafo_depth: 0,
            tu_intra_mode: INTRA_DC,
            tu_intra_mode_c: INTRA_DC,
            stats: CtuStats::default(),
        }
    }

    #[inline]
    fn rec(&mut self, kind: u8, ctx: i32, bin: u8) {
        if let Some(t) = &mut self.trace {
            t.push(CabacTraceOp { kind, ctx, bin });
        }
    }

    #[inline]
    fn d(&mut self, ctx: usize) -> u32 {
        let bin = self.cabac.decode_bin(ctx);
        self.rec(b'D', ctx as i32, bin as u8);
        bin
    }

    #[inline]
    fn d_se(&mut self, se: SyntaxElement, inc: usize) -> u32 {
        self.d(se.offset() + inc)
    }

    #[inline]
    fn b(&mut self) -> u32 {
        let bit = self.cabac.decode_bypass();
        self.rec(b'B', -1, bit as u8);
        bit
    }

    #[inline]
    fn b_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.b();
        }
        v
    }

    #[inline]
    fn term(&mut self) -> bool {
        let end = self.cabac.decode_terminate();
        self.rec(b'T', -1, u8::from(end));
        end
    }

    fn decode(&mut self) -> Result<()> {
        self.reject_unsupported()?;
        let ctb_size = 1u32 << self.log2_ctb;
        let ctb_cols = self.width.div_ceil(ctb_size);
        let ctb_rows = self.height.div_ceil(ctb_size);
        let total = ctb_cols * ctb_rows;

        for ctb_addr in 0..total {
            let col = ctb_addr % ctb_cols;
            let row = (ctb_addr / ctb_cols) as usize;
            let x_ctb = col << self.log2_ctb;
            let y_ctb = (ctb_addr / ctb_cols) << self.log2_ctb;

            if self.wpp && col == 0 && ctb_addr != 0 {
                let _end_of_subset_one_bit = self.term();
                let byte_off = self.wpp_row_offsets.get(row).copied().ok_or_else(|| {
                    Error::Parse("hevc ctu: WPP entry point missing for row".into())
                })?;
                self.cabac.reinit_substream(byte_off);
                if let Some(snap) = self.wpp_saved_ctx {
                    self.cabac.restore_contexts(&snap);
                } else {
                    self.cabac.reinit_contexts(self.sh.slice_qp);
                }
                self.last_qp_y = self.sh.slice_qp;
            }

            self.ctb_left = x_ctb > 0;
            self.ctb_up = y_ctb > 0;

            self.decode_sao(x_ctb, y_ctb);

            self.stats.ctb_count += 1;
            let more = self.coding_quadtree(x_ctb, y_ctb, self.log2_ctb, 0)?;

            if self.wpp && col == 1 {
                self.wpp_saved_ctx = Some(self.cabac.save_contexts());
            }

            if !more {
                break;
            }
        }
        self.stats.end_bit_pos = self.cabac.bit_pos();
        self.deblock_luma();
        self.apply_sao_luma();
        Ok(())
    }

    fn reject_unsupported(&self) -> Result<()> {
        if self.sps.pcm.is_some() {
            return Err(Error::Unsupported(
                "hevc ctu: PCM coding not supported".into(),
            ));
        }
        Ok(())
    }

    fn decode_sao(&mut self, x_ctb: u32, y_ctb: u32) {
        if !self.sh.sao_luma && !self.sh.sao_chroma {
            return;
        }
        let rx = (x_ctb >> self.log2_ctb) as usize;
        let ry = (y_ctb >> self.log2_ctb) as usize;
        let addr = ry * self.ctb_cols + rx;

        let mut merge_left = false;
        if rx > 0 && self.ctb_left {
            merge_left = self.d_se(SyntaxElement::SaoMergeFlag, 0) == 1;
        }
        let mut merge_up = false;
        if ry > 0 && !merge_left && self.ctb_up {
            merge_up = self.d_se(SyntaxElement::SaoMergeFlag, 0) == 1;
        }
        if merge_left {
            self.sao_luma_ctb[addr] = self.sao_luma_ctb[addr - 1];
            return;
        }
        if merge_up {
            self.sao_luma_ctb[addr] = self.sao_luma_ctb[addr - self.ctb_cols];
            return;
        }

        let n_channels = if self.sps.chroma_array_type() != 0 {
            3
        } else {
            1
        };
        let sao_len = (1u32 << (self.sps.bit_depth_luma.min(10) - 5)) - 1;
        let mut type_idx_luma_chroma = [0u32; 2];
        for c_idx in 0..n_channels {
            let enabled = if c_idx == 0 {
                self.sh.sao_luma
            } else {
                self.sh.sao_chroma
            };
            if !enabled {
                continue;
            }
            let type_idx = if c_idx == 2 {
                type_idx_luma_chroma[1]
            } else {
                let t = if self.d_se(SyntaxElement::SaoTypeIdx, 0) == 0 {
                    0
                } else if self.b() == 0 {
                    1
                } else {
                    2
                };
                type_idx_luma_chroma[usize::from(c_idx == 1 || c_idx == 2)] = t;
                if c_idx == 0 {
                    type_idx_luma_chroma[0] = t;
                }
                t
            };
            if type_idx == 0 {
                continue;
            }

            let mut offset_abs = [0u32; 4];
            for o in &mut offset_abs {
                let mut i = 0;
                while i < sao_len && self.b() == 1 {
                    i += 1;
                }
                *o = i;
            }
            let mut sign = [false; 4];
            let mut band_position = 0u32;
            let mut eo_class = 0u32;
            if type_idx == 1 {
                for (s, &abs) in sign.iter_mut().zip(&offset_abs) {
                    if abs != 0 {
                        *s = self.b() == 1;
                    }
                }
                band_position = self.b_bits(5);
            } else if c_idx != 2 {
                eo_class = self.b_bits(2);
            }

            if c_idx == 0 {
                let mut offset_val = [0i32; 5];
                for i in 0..4 {
                    let v = offset_abs[i] as i32;
                    offset_val[i + 1] = if type_idx == 1 {
                        if sign[i] { -v } else { v }
                    } else if i > 1 {
                        -v
                    } else {
                        v
                    };
                }
                self.sao_luma_ctb[addr] = SaoLuma {
                    type_idx: type_idx as u8,
                    offset_val,
                    band_position: band_position as u8,
                    eo_class: eo_class as u8,
                };
            }
        }
    }

    fn apply_sao_luma(&mut self) {
        if !self.sh.sao_luma {
            return;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let bd = i32::from(self.bit_depth);
        let max = (1i32 << bd) - 1;
        let shift = bd - 5;
        let ctb_size = 1usize << self.log2_ctb;
        let src = self.luma.clone();
        let dst = &mut self.luma;

        for ry in 0..self.ctb_rows {
            for rx in 0..self.ctb_cols {
                let sao = self.sao_luma_ctb[ry * self.ctb_cols + rx];
                if sao.type_idx == 0 {
                    continue;
                }
                let x0 = rx * ctb_size;
                let y0 = ry * ctb_size;
                let cw = ctb_size.min(w - x0);
                let ch = ctb_size.min(h - y0);

                if sao.type_idx == 1 {
                    let mut table = [0i32; 32];
                    for band in 0..4 {
                        table[(band + sao.band_position as usize) & 31] = sao.offset_val[band + 1];
                    }
                    for yy in 0..ch {
                        for xx in 0..cw {
                            let idx = (y0 + yy) * w + (x0 + xx);
                            let sample = i32::from(src[idx]);
                            dst[idx] = (sample + table[((sample >> shift) & 31) as usize])
                                .clamp(0, max) as u8;
                        }
                    }
                } else {
                    let [(adx, ady), (bdx, bdy)] = SAO_EO_POS[sao.eo_class as usize];
                    for yy in 0..ch {
                        for xx in 0..cw {
                            let (x, y) = ((x0 + xx) as i32, (y0 + yy) as i32);
                            let (ax, ay) = (x + adx, y + ady);
                            let (bx, by) = (x + bdx, y + bdy);
                            if ax < 0
                                || ay < 0
                                || bx < 0
                                || by < 0
                                || ax >= w as i32
                                || ay >= h as i32
                                || bx >= w as i32
                                || by >= h as i32
                            {
                                continue;
                            }
                            let idx = (y as usize) * w + x as usize;
                            let center = i32::from(src[idx]);
                            let na = i32::from(src[(ay as usize) * w + ax as usize]);
                            let nb = i32::from(src[(by as usize) * w + bx as usize]);
                            let diff0 = i32::from(center > na) - i32::from(center < na);
                            let diff1 = i32::from(center > nb) - i32::from(center < nb);
                            let ov = sao.offset_val[SAO_EDGE_IDX[(2 + diff0 + diff1) as usize]];
                            dst[idx] = (center + ov).clamp(0, max) as u8;
                        }
                    }
                }
            }
        }
    }

    fn coding_quadtree(&mut self, x0: u32, y0: u32, log2_cb_size: u32, depth: u32) -> Result<bool> {
        let cb_size = 1u32 << log2_cb_size;

        if self.cu_qp_delta_enabled && log2_cb_size >= self.log2_min_cu_qp_delta_size {
            self.qp_y_prev = self.last_qp_y;
            self.is_cu_qp_delta_coded = false;
            self.cu_qp_delta_val = 0;
            self.qg_x = x0;
            self.qg_y = y0;
        }

        let split = if x0 + cb_size <= self.width
            && y0 + cb_size <= self.height
            && log2_cb_size > self.log2_min_cb
        {
            self.split_cu_flag(depth, x0, y0) == 1
        } else {
            log2_cb_size > self.log2_min_cb
        };

        if split {
            let half = cb_size >> 1;
            let (x1, y1) = (x0 + half, y0 + half);
            let mut more = self.coding_quadtree(x0, y0, log2_cb_size - 1, depth + 1)?;
            if more && x1 < self.width {
                more = self.coding_quadtree(x1, y0, log2_cb_size - 1, depth + 1)?;
            }
            if more && y1 < self.height {
                more = self.coding_quadtree(x0, y1, log2_cb_size - 1, depth + 1)?;
            }
            if more && x1 < self.width && y1 < self.height {
                more = self.coding_quadtree(x1, y1, log2_cb_size - 1, depth + 1)?;
            }
            if more {
                Ok(x0 + cb_size < self.width || y0 + cb_size < self.height)
            } else {
                Ok(false)
            }
        } else {
            self.coding_unit(x0, y0, log2_cb_size, depth)?;
            let ctb_size = 1u32 << self.log2_ctb;
            let at_right = (x0 + cb_size) % ctb_size == 0 || x0 + cb_size >= self.width;
            let at_bottom = (y0 + cb_size) % ctb_size == 0 || y0 + cb_size >= self.height;
            if at_right && at_bottom {
                Ok(!self.term())
            } else {
                Ok(true)
            }
        }
    }

    fn split_cu_flag(&mut self, ct_depth: u32, x0: u32, y0: u32) -> u32 {
        let x0b = x0 & ((1 << self.log2_ctb) - 1);
        let y0b = y0 & ((1 << self.log2_ctb) - 1);
        let x_cb = (x0 >> self.log2_min_cb) as usize;
        let y_cb = (y0 >> self.log2_min_cb) as usize;
        let mut inc = 0;
        if self.ctb_left || x0b != 0 {
            let d = self.tab_ct_depth[y_cb * self.min_cb_width + x_cb - 1];
            inc += usize::from(u32::from(d) > ct_depth);
        }
        if self.ctb_up || y0b != 0 {
            let d = self.tab_ct_depth[(y_cb - 1) * self.min_cb_width + x_cb];
            inc += usize::from(u32::from(d) > ct_depth);
        }
        self.d_se(SyntaxElement::SplitCodingUnitFlag, inc)
    }

    fn coding_unit(&mut self, x0: u32, y0: u32, log2_cb_size: u32, depth: u32) -> Result<()> {
        self.stats.cu_count += 1;
        self.cur_cu_qp_set = false;
        let part_nxn = if log2_cb_size == self.log2_min_cb {
            self.d_se(SyntaxElement::PartMode, 0) == 0
        } else {
            false
        };
        self.cu_intra_split = part_nxn;

        self.intra_prediction_unit(x0, y0, log2_cb_size, part_nxn);

        self.cu_max_trafo_depth = self.max_trafo_depth_intra + u32::from(self.cu_intra_split);
        self.transform_tree(x0, y0, x0, y0, log2_cb_size, 0, 0, [0, 0], [0, 0])?;

        if self.cu_qp_delta_enabled {
            let qp_y = self.current_cu_qp_y();
            self.write_cu_qp_y(x0, y0, log2_cb_size, qp_y);
        }

        self.set_ct_depth(x0, y0, log2_cb_size, depth);
        Ok(())
    }

    fn current_cu_qp_y(&mut self) -> i32 {
        if !self.cur_cu_qp_set {
            self.cur_cu_qp_y = if self.cu_qp_delta_enabled {
                self.derive_qp_y()
            } else {
                self.sh.slice_qp
            };
            self.cur_cu_qp_set = true;
            self.last_qp_y = self.cur_cu_qp_y;
        }
        self.cur_cu_qp_y
    }

    fn derive_qp_y(&self) -> i32 {
        let qp_a = self
            .neighbour_qp_y(i64::from(self.qg_x) - 1, i64::from(self.qg_y))
            .unwrap_or(self.qp_y_prev);
        let qp_b = self
            .neighbour_qp_y(i64::from(self.qg_x), i64::from(self.qg_y) - 1)
            .unwrap_or(self.qp_y_prev);
        let qp_pred = (qp_a + qp_b + 1) >> 1;
        let qp_bd_offset = 6 * (i32::from(self.bit_depth) - 8);
        let range = 52 + qp_bd_offset;
        (qp_pred + self.cu_qp_delta_val + 52 + 2 * qp_bd_offset).rem_euclid(range) - qp_bd_offset
    }

    fn neighbour_qp_y(&self, nx: i64, ny: i64) -> Option<i32> {
        if nx < 0 || ny < 0 {
            return None;
        }
        let (nx, ny) = (nx as u32, ny as u32);
        if nx >= self.width || ny >= self.height {
            return None;
        }
        if (nx >> self.log2_ctb) != (self.qg_x >> self.log2_ctb)
            || (ny >> self.log2_ctb) != (self.qg_y >> self.log2_ctb)
        {
            return None;
        }
        if self.min_tb_addr_zs(nx, ny) > self.min_tb_addr_zs(self.qg_x, self.qg_y) {
            return None;
        }
        Some(i32::from(self.tab_qp_y[self.qp_cell(nx, ny)]))
    }

    fn qp_cell(&self, x: u32, y: u32) -> usize {
        (y as usize / 4) * self.bs_w + (x as usize / 4)
    }

    fn write_cu_qp_y(&mut self, x0: u32, y0: u32, log2_cb_size: u32, qp_y: i32) {
        let cells = (1usize << log2_cb_size) / 4;
        let (xi, yi) = (x0 as usize / 4, y0 as usize / 4);
        for j in 0..cells {
            for i in 0..cells {
                if (yi + j) * 4 < self.height as usize && (xi + i) * 4 < self.width as usize {
                    self.tab_qp_y[(yi + j) * self.bs_w + (xi + i)] = qp_y as i8;
                }
            }
        }
    }

    fn cu_qp_delta_abs(&mut self) -> i32 {
        let mut prefix = 0i32;
        while prefix < 5 && self.d_se(SyntaxElement::CuQpDelta, usize::from(prefix > 0)) == 1 {
            prefix += 1;
        }
        if prefix < 5 {
            prefix
        } else {
            5 + self.bypass_egk(0)
        }
    }

    fn bypass_egk(&mut self, mut k: u32) -> i32 {
        let mut value = 0i32;
        while self.b() == 1 {
            value += 1 << k;
            k += 1;
        }
        while k > 0 {
            k -= 1;
            value += (self.b() as i32) << k;
        }
        value
    }

    fn set_ct_depth(&mut self, x0: u32, y0: u32, log2_cb_size: u32, depth: u32) {
        let x_cb = (x0 >> self.log2_min_cb) as usize;
        let y_cb = (y0 >> self.log2_min_cb) as usize;
        let n = (1u32 << (log2_cb_size - self.log2_min_cb)) as usize;
        for i in 0..n {
            for j in 0..n {
                self.tab_ct_depth[(y_cb + i) * self.min_cb_width + x_cb + j] = depth as u8;
            }
        }
    }

    fn intra_prediction_unit(&mut self, x0: u32, y0: u32, log2_cb_size: u32, split: bool) {
        let side = if split { 2 } else { 1 };
        let pb_size = (1u32 << log2_cb_size) >> u32::from(split);

        let mut prev_flag = [false; 4];
        for f in prev_flag.iter_mut().take(side * side) {
            *f = self.d_se(SyntaxElement::PrevIntraLumaPredFlag, 0) == 1;
        }

        for i in 0..side {
            for j in 0..side {
                let blk = 2 * i + j;
                let (mpm_idx, rem_mode) = if prev_flag[blk] {
                    let mut idx = 0u8;
                    while idx < 2 && self.b() == 1 {
                        idx += 1;
                    }
                    (idx, 0u8)
                } else {
                    (0u8, self.b_bits(5) as u8)
                };
                let mode = self.luma_intra_pred_mode(
                    x0 + pb_size * j as u32,
                    y0 + pb_size * i as u32,
                    pb_size,
                    prev_flag[blk],
                    mpm_idx,
                    rem_mode,
                );
                self.pu_intra_mode[blk] = mode;
            }
        }

        if self.sps.chroma_array_type() != 0 {
            const TABLE: [u8; 4] = [INTRA_PLANAR, INTRA_ANGULAR_26, 10, INTRA_DC];
            let chroma_mode = if self.d_se(SyntaxElement::IntraChromaPredMode, 0) == 0 {
                4
            } else {
                self.b_bits(2)
            };
            let luma0 = self.pu_intra_mode[0];
            self.pu_intra_mode_c = if chroma_mode != 4 {
                let cand = TABLE[chroma_mode as usize];
                if luma0 == cand { 34 } else { cand }
            } else {
                luma0
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn luma_intra_pred_mode(
        &mut self,
        x0: u32,
        y0: u32,
        pu_size: u32,
        prev_flag: bool,
        mpm_idx: u8,
        rem_mode: u8,
    ) -> u8 {
        let x_pu = (x0 >> self.log2_min_pu) as usize;
        let y_pu = (y0 >> self.log2_min_pu) as usize;
        let x0b = x0 & ((1 << self.log2_ctb) - 1);
        let y0b = y0 & ((1 << self.log2_ctb) - 1);

        let mut cand_up = if self.ctb_up || y0b != 0 {
            self.tab_ipm[(y_pu - 1) * self.min_pu_width + x_pu]
        } else {
            INTRA_DC
        };
        let cand_left = if self.ctb_left || x0b != 0 {
            self.tab_ipm[y_pu * self.min_pu_width + x_pu - 1]
        } else {
            INTRA_DC
        };
        let y_ctb = (y0 >> self.log2_ctb) << self.log2_ctb;
        if (y0 as i64 - 1) < y_ctb as i64 {
            cand_up = INTRA_DC;
        }

        let mut cand = [0u8; 3];
        if cand_left == cand_up {
            if cand_left < 2 {
                cand = [INTRA_PLANAR, INTRA_DC, INTRA_ANGULAR_26];
            } else {
                cand[0] = cand_left;
                cand[1] = 2 + ((cand_left as i32 - 2 - 1 + 32) & 31) as u8;
                cand[2] = 2 + ((cand_left as i32 - 2 + 1) & 31) as u8;
            }
        } else {
            cand[0] = cand_left;
            cand[1] = cand_up;
            cand[2] = if cand_left != INTRA_PLANAR && cand_up != INTRA_PLANAR {
                INTRA_PLANAR
            } else if cand_left != INTRA_DC && cand_up != INTRA_DC {
                INTRA_DC
            } else {
                INTRA_ANGULAR_26
            };
        }

        let mode = if prev_flag {
            cand[mpm_idx as usize]
        } else {
            cand.sort_unstable();
            let mut m = rem_mode;
            for &c in &cand {
                if m >= c {
                    m += 1;
                }
            }
            m
        };

        let size_in_pus = ((pu_size >> self.log2_min_pu) as usize).max(1);
        for i in 0..size_in_pus {
            for j in 0..size_in_pus {
                self.tab_ipm[(y_pu + i) * self.min_pu_width + x_pu + j] = mode;
            }
        }
        mode
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_trafo_size: u32,
        trafo_depth: u32,
        blk_idx: u32,
        base_cbf_cb: [u8; 2],
        base_cbf_cr: [u8; 2],
    ) -> Result<()> {
        let mut cbf_cb = base_cbf_cb;
        let mut cbf_cr = base_cbf_cr;

        if self.cu_intra_split {
            if trafo_depth == 1 {
                self.tu_intra_mode = self.pu_intra_mode[blk_idx as usize];
                self.tu_intra_mode_c = self.pu_intra_mode_c;
            }
        } else {
            self.tu_intra_mode = self.pu_intra_mode[0];
            self.tu_intra_mode_c = self.pu_intra_mode_c;
        }

        let split_transform = if log2_trafo_size <= self.log2_max_tb
            && log2_trafo_size > self.log2_min_tb
            && trafo_depth < self.cu_max_trafo_depth
            && !(self.cu_intra_split && trafo_depth == 0)
        {
            self.split_transform_flag(log2_trafo_size) == 1
        } else {
            log2_trafo_size > self.log2_max_tb || (self.cu_intra_split && trafo_depth == 0)
        };

        let idc = self.sps.chroma_format_idc;
        let chroma_422 = idc == 2;
        if idc != 0 && (log2_trafo_size > 2 || idc == 3) {
            if trafo_depth == 0 || cbf_cb[0] != 0 {
                cbf_cb[0] = self.cbf_chroma(trafo_depth) as u8;
                if chroma_422 && (!split_transform || log2_trafo_size == 3) {
                    cbf_cb[1] = self.cbf_chroma(trafo_depth) as u8;
                }
            }
            if trafo_depth == 0 || cbf_cr[0] != 0 {
                cbf_cr[0] = self.cbf_chroma(trafo_depth) as u8;
                if chroma_422 && (!split_transform || log2_trafo_size == 3) {
                    cbf_cr[1] = self.cbf_chroma(trafo_depth) as u8;
                }
            }
        }

        if split_transform {
            let half = 1u32 << (log2_trafo_size - 1);
            let (x1, y1) = (x0 + half, y0 + half);
            self.transform_tree(
                x0,
                y0,
                x0,
                y0,
                log2_trafo_size - 1,
                trafo_depth + 1,
                0,
                cbf_cb,
                cbf_cr,
            )?;
            self.transform_tree(
                x1,
                y0,
                x0,
                y0,
                log2_trafo_size - 1,
                trafo_depth + 1,
                1,
                cbf_cb,
                cbf_cr,
            )?;
            self.transform_tree(
                x0,
                y1,
                x0,
                y0,
                log2_trafo_size - 1,
                trafo_depth + 1,
                2,
                cbf_cb,
                cbf_cr,
            )?;
            self.transform_tree(
                x1,
                y1,
                x0,
                y0,
                log2_trafo_size - 1,
                trafo_depth + 1,
                3,
                cbf_cb,
                cbf_cr,
            )?;
        } else {
            let cbf_luma = self.cbf_luma(trafo_depth) == 1;
            self.transform_unit(
                x0,
                y0,
                x_base,
                y_base,
                log2_trafo_size,
                blk_idx,
                cbf_luma,
                cbf_cb,
                cbf_cr,
            );
        }
        Ok(())
    }

    fn split_transform_flag(&mut self, log2_trafo_size: u32) -> u32 {
        self.d_se(
            SyntaxElement::SplitTransformFlag,
            (5 - log2_trafo_size) as usize,
        )
    }

    fn cbf_chroma(&mut self, trafo_depth: u32) -> u32 {
        self.d_se(SyntaxElement::CbfChroma, trafo_depth as usize)
    }

    fn cbf_luma(&mut self, trafo_depth: u32) -> u32 {
        self.d_se(SyntaxElement::CbfLuma, usize::from(trafo_depth == 0))
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2_trafo_size: u32,
        blk_idx: u32,
        cbf_luma: bool,
        cbf_cb: [u8; 2],
        cbf_cr: [u8; 2],
    ) {
        self.stats.tu_count += 1;
        let idc = self.sps.chroma_format_idc;
        let chroma_422 = idc == 2;
        let cbf_chroma =
            cbf_cb[0] != 0 || cbf_cr[0] != 0 || (chroma_422 && (cbf_cb[1] != 0 || cbf_cr[1] != 0));

        let (mut scan_luma, mut scan_chroma) = (ScanType::Diag, ScanType::Diag);
        if log2_trafo_size < 4 {
            scan_luma = scan_for_mode(self.tu_intra_mode);
            scan_chroma = scan_for_mode(self.tu_intra_mode_c);
        }

        if (cbf_luma || cbf_chroma) && self.cu_qp_delta_enabled && !self.is_cu_qp_delta_coded {
            let abs = self.cu_qp_delta_abs();
            self.cu_qp_delta_val = if abs != 0 && self.b() == 1 { -abs } else { abs };
            self.is_cu_qp_delta_coded = true;
            self.stats.cu_qp_delta_count += 1;
        }

        if cbf_luma {
            self.residual_coding(x0, y0, log2_trafo_size, scan_luma, 0);
        }
        if cbf_chroma {
            let log2_c = log2_trafo_size - self.sps.sub_width_c().trailing_zeros();
            let n = if chroma_422 { 2 } else { 1 };
            if log2_trafo_size > 2 || idc == 3 {
                self.chroma_residuals(x0, y0, log2_c, scan_chroma, n, cbf_cb, cbf_cr);
            } else if blk_idx == 3 {
                self.chroma_residuals(
                    x_base,
                    y_base,
                    log2_trafo_size,
                    scan_chroma,
                    n,
                    cbf_cb,
                    cbf_cr,
                );
            }
        }

        self.reconstruct_luma(x0, y0, log2_trafo_size, cbf_luma);
    }

    fn reconstruct_luma(&mut self, x0: u32, y0: u32, log2_size: u32, has_residual: bool) {
        let n = 1usize << log2_size;
        let bd = self.bit_depth;
        let mode = self.tu_intra_mode;

        let (corner, top, left) = self.gather_luma_refs(x0, y0, n);
        let mut refs = intra::build_references(n, corner, &top, &left, bd);
        intra::filter_references(
            &mut refs,
            mode,
            true,
            self.sps.strong_intra_smoothing_enabled,
            bd,
        );
        let pred = intra::predict(mode, &refs, true, bd);

        if has_residual {
            let qp = self.current_cu_qp_y() + 6 * (i32::from(bd) - 8);
            transform::dequant(&mut self.coeff_buf[..n * n], log2_size, qp, bd);
            if self.tu_transform_skip {
                transform::transform_skip(&mut self.coeff_buf[..n * n], log2_size, bd);
            } else {
                let use_dst = log2_size == 2;
                transform::inverse_transform(&mut self.coeff_buf[..n * n], log2_size, use_dst, bd);
            }
        }

        let w = self.width as usize;
        let max = (1i32 << bd) - 1;
        for y in 0..n {
            for x in 0..n {
                let res = if has_residual {
                    self.coeff_buf[y * n + x]
                } else {
                    0
                };
                let v = (pred[y * n + x] + res).clamp(0, max);
                self.luma[(y0 as usize + y) * w + (x0 as usize + x)] = v as u8;
            }
        }

        self.mark_deblock_edges(x0, y0, n);
    }

    fn mark_deblock_edges(&mut self, x0: u32, y0: u32, n: usize) {
        let xi = (x0 / 4) as usize;
        let yi = (y0 / 4) as usize;
        let cells = n / 4;
        for c in 0..cells {
            self.vert_bs[(yi + c) * self.bs_w + xi] = 2;
            self.horiz_bs[yi * self.bs_w + (xi + c)] = 2;
        }
    }

    fn gather_luma_refs(
        &self,
        x0: u32,
        y0: u32,
        n: usize,
    ) -> (Option<u8>, Vec<Option<u8>>, Vec<Option<u8>>) {
        let w = self.width as i64;
        let h = self.height as i64;
        let cur_z = self.min_tb_addr_zs(x0, y0);
        let sample = |px: i64, py: i64| -> Option<u8> {
            if px < 0 || py < 0 || px >= w || py >= h {
                return None;
            }
            if self.min_tb_addr_zs(px as u32, py as u32) > cur_z {
                return None;
            }
            Some(self.luma[(py as usize) * (self.width as usize) + px as usize])
        };
        let (x0i, y0i) = (i64::from(x0), i64::from(y0));
        let corner = sample(x0i - 1, y0i - 1);
        let top = (0..2 * n)
            .map(|i| sample(x0i + i as i64, y0i - 1))
            .collect();
        let left = (0..2 * n)
            .map(|j| sample(x0i - 1, y0i + j as i64))
            .collect();
        (corner, top, left)
    }

    fn min_tb_addr_zs(&self, x: u32, y: u32) -> u64 {
        let m = self.log2_ctb - self.log2_min_tb;
        let tb_x = x >> self.log2_min_tb;
        let tb_y = y >> self.log2_min_tb;
        let ctb_cols = self.width.div_ceil(1 << self.log2_ctb);
        let ctb_rs = u64::from((tb_y >> m) * ctb_cols + (tb_x >> m));
        let mask = (1u32 << m) - 1;
        let (lx, ly) = (tb_x & mask, tb_y & mask);
        let mut z = 0u64;
        for i in 0..m {
            z |= u64::from((lx >> i) & 1) << (2 * i);
            z |= u64::from((ly >> i) & 1) << (2 * i + 1);
        }
        (ctb_rs << (2 * m)) + z
    }

    fn deblock_luma(&mut self) {
        if self.sh.deblocking_filter_disabled {
            return;
        }
        let beta_offset = self.sh.beta_offset_div2 * 2;
        let tc_offset = self.sh.tc_offset_div2 * 2;
        let beta_raw = |qp_l: i32| -> i32 {
            i32::from(BETA_TABLE[(qp_l + beta_offset).clamp(0, MAX_QP) as usize])
        };
        let tc_raw = |qp_l: i32, bs: u8| -> i32 {
            if bs == 0 {
                return 0;
            }
            let idx = (qp_l + DEFAULT_INTRA_TC_OFFSET * (i32::from(bs) - 1) + (tc_offset & !1))
                .clamp(0, MAX_QP + DEFAULT_INTRA_TC_OFFSET);
            i32::from(TC_TABLE[idx as usize])
        };
        let (w, h) = (self.width as usize, self.height as usize);

        let mut y = 0;
        while y < h {
            let mut x = 8;
            while x < w {
                let bs0 = self.vert_bs[(y / 4) * self.bs_w + x / 4];
                let bs1 = self.vert_bs[(y / 4 + 1) * self.bs_w + x / 4];
                if bs0 != 0 || bs1 != 0 {
                    let qpl0 = (self.qp_y_at(x - 1, y) + self.qp_y_at(x, y) + 1) >> 1;
                    let qpl1 = (self.qp_y_at(x - 1, y + 4) + self.qp_y_at(x, y + 4) + 1) >> 1;
                    let beta = [beta_raw(qpl0), beta_raw(qpl1)];
                    let tc = [tc_raw(qpl0, bs0), tc_raw(qpl1, bs1)];
                    self.filter_luma_edge(x as i32, y as i32, 1, 0, 0, 1, beta, tc);
                }
                x += 8;
            }
            y += 8;
        }

        let mut y = 8;
        while y < h {
            let mut x = 0;
            while x < w {
                let bs0 = self.horiz_bs[(y / 4) * self.bs_w + x / 4];
                let bs1 = self.horiz_bs[(y / 4) * self.bs_w + (x / 4 + 1)];
                if bs0 != 0 || bs1 != 0 {
                    let qpl0 = (self.qp_y_at(x, y - 1) + self.qp_y_at(x, y) + 1) >> 1;
                    let qpl1 = (self.qp_y_at(x + 4, y - 1) + self.qp_y_at(x + 4, y) + 1) >> 1;
                    let beta = [beta_raw(qpl0), beta_raw(qpl1)];
                    let tc = [tc_raw(qpl0, bs0), tc_raw(qpl1, bs1)];
                    self.filter_luma_edge(x as i32, y as i32, 0, 1, 1, 0, beta, tc);
                }
                x += 8;
            }
            y += 8;
        }
    }

    fn qp_y_at(&self, x: usize, y: usize) -> i32 {
        i32::from(self.tab_qp_y[self.qp_cell(x as u32, y as u32)])
    }

    #[allow(clippy::too_many_arguments)]
    fn filter_luma_edge(
        &mut self,
        px: i32,
        py: i32,
        ax: i32,
        ay: i32,
        lx: i32,
        ly: i32,
        beta_tab: [i32; 2],
        tc_tab: [i32; 2],
    ) {
        let w = self.width as i32;
        let bd = i32::from(self.bit_depth);
        let max = (1i32 << bd) - 1;
        let idx = move |line: i32, a: i32| -> usize {
            ((py + ly * line + ay * a) * w + (px + lx * line + ax * a)) as usize
        };

        for j in 0..2i32 {
            let (l0, l3) = (j * 4, j * 4 + 3);
            let s = |b: &[u8], line: i32, a: i32| -> i32 { i32::from(b[idx(line, a)]) };
            let dp0 =
                (s(&self.luma, l0, -3) - 2 * s(&self.luma, l0, -2) + s(&self.luma, l0, -1)).abs();
            let dq0 =
                (s(&self.luma, l0, 0) - 2 * s(&self.luma, l0, 1) + s(&self.luma, l0, 2)).abs();
            let dp3 =
                (s(&self.luma, l3, -3) - 2 * s(&self.luma, l3, -2) + s(&self.luma, l3, -1)).abs();
            let dq3 =
                (s(&self.luma, l3, 0) - 2 * s(&self.luma, l3, 1) + s(&self.luma, l3, 2)).abs();
            let d0 = dp0 + dq0;
            let d3 = dp3 + dq3;
            let beta = beta_tab[j as usize] << (bd - 8);
            let tc = tc_tab[j as usize] << (bd - 8);

            if d0 + d3 >= beta {
                continue;
            }
            let beta_3 = beta >> 3;
            let beta_2 = beta >> 2;
            let tc25 = (tc * 5 + 1) >> 1;
            let strong = (s(&self.luma, l0, -4) - s(&self.luma, l0, -1)).abs()
                + (s(&self.luma, l0, 3) - s(&self.luma, l0, 0)).abs()
                < beta_3
                && (s(&self.luma, l0, -1) - s(&self.luma, l0, 0)).abs() < tc25
                && (s(&self.luma, l3, -4) - s(&self.luma, l3, -1)).abs()
                    + (s(&self.luma, l3, 3) - s(&self.luma, l3, 0)).abs()
                    < beta_3
                && (s(&self.luma, l3, -1) - s(&self.luma, l3, 0)).abs() < tc25
                && (d0 << 1) < beta_2
                && (d3 << 1) < beta_2;

            if strong {
                let tc2 = tc << 1;
                for d in 0..4 {
                    let line = j * 4 + d;
                    let (p3, p2, p1, p0) = (
                        s(&self.luma, line, -4),
                        s(&self.luma, line, -3),
                        s(&self.luma, line, -2),
                        s(&self.luma, line, -1),
                    );
                    let (q0, q1, q2, q3) = (
                        s(&self.luma, line, 0),
                        s(&self.luma, line, 1),
                        s(&self.luma, line, 2),
                        s(&self.luma, line, 3),
                    );
                    let np0 = p0
                        + (((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) - p0).clamp(-tc2, tc2);
                    let np1 = p1 + (((p2 + p1 + p0 + q0 + 2) >> 2) - p1).clamp(-tc2, tc2);
                    let np2 =
                        p2 + (((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) - p2).clamp(-tc2, tc2);
                    let nq0 = q0
                        + (((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) - q0).clamp(-tc2, tc2);
                    let nq1 = q1 + (((p0 + q0 + q1 + q2 + 2) >> 2) - q1).clamp(-tc2, tc2);
                    let nq2 =
                        q2 + (((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) - q2).clamp(-tc2, tc2);
                    self.luma[idx(line, -3)] = np2.clamp(0, max) as u8;
                    self.luma[idx(line, -2)] = np1.clamp(0, max) as u8;
                    self.luma[idx(line, -1)] = np0.clamp(0, max) as u8;
                    self.luma[idx(line, 0)] = nq0.clamp(0, max) as u8;
                    self.luma[idx(line, 1)] = nq1.clamp(0, max) as u8;
                    self.luma[idx(line, 2)] = nq2.clamp(0, max) as u8;
                }
            } else {
                let tc_2 = tc >> 1;
                let nd_thr = (beta + (beta >> 1)) >> 3;
                let nd_p = i32::from(dp0 + dp3 < nd_thr);
                let nd_q = i32::from(dq0 + dq3 < nd_thr);
                for d in 0..4 {
                    let line = j * 4 + d;
                    let (p2, p1, p0) = (
                        s(&self.luma, line, -3),
                        s(&self.luma, line, -2),
                        s(&self.luma, line, -1),
                    );
                    let (q0, q1, q2) = (
                        s(&self.luma, line, 0),
                        s(&self.luma, line, 1),
                        s(&self.luma, line, 2),
                    );
                    let delta0 = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
                    if delta0.abs() < 10 * tc {
                        let delta0 = delta0.clamp(-tc, tc);
                        self.luma[idx(line, -1)] = (p0 + delta0).clamp(0, max) as u8;
                        self.luma[idx(line, 0)] = (q0 - delta0).clamp(0, max) as u8;
                        if nd_p != 0 {
                            let dp1 =
                                ((((p2 + p0 + 1) >> 1) - p1 + delta0) >> 1).clamp(-tc_2, tc_2);
                            self.luma[idx(line, -2)] = (p1 + dp1).clamp(0, max) as u8;
                        }
                        if nd_q != 0 {
                            let dq1 =
                                ((((q2 + q0 + 1) >> 1) - q1 - delta0) >> 1).clamp(-tc_2, tc_2);
                            self.luma[idx(line, 1)] = (q1 + dq1).clamp(0, max) as u8;
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn chroma_residuals(
        &mut self,
        x: u32,
        y: u32,
        log2_c: u32,
        scan: ScanType,
        n: usize,
        cbf_cb: [u8; 2],
        cbf_cr: [u8; 2],
    ) {
        for (i, &cbf) in cbf_cb.iter().enumerate().take(n) {
            if cbf != 0 {
                self.residual_coding(x, y + (i as u32) * (1 << log2_c), log2_c, scan, 1);
            }
        }
        for (i, &cbf) in cbf_cr.iter().enumerate().take(n) {
            if cbf != 0 {
                self.residual_coding(x, y + (i as u32) * (1 << log2_c), log2_c, scan, 2);
            }
        }
    }

    fn residual_coding(
        &mut self,
        _x0: u32,
        _y0: u32,
        log2_size: u32,
        scan_type: ScanType,
        c_idx: u32,
    ) {
        debug_assert!(
            log2_size <= 5,
            "hevc residual_coding: log2_size {log2_size} > 5 exceeds the 32×32 coeff_buf"
        );
        let n = 1usize << log2_size;
        if c_idx == 0 {
            self.coeff_buf[..n * n].fill(0);
        }

        let transform_skip = self.transform_skip_enabled
            && log2_size == 2
            && self.d_se(SyntaxElement::TransformSkipFlag, usize::from(c_idx != 0)) == 1;
        if c_idx == 0 {
            self.tu_transform_skip = transform_skip;
            if transform_skip {
                self.stats.transform_skip_count += 1;
            }
        }

        let max = (log2_size << 1) - 1;
        let (ctx_offset, ctx_shift) = if c_idx == 0 {
            (
                3 * (log2_size - 2) + ((log2_size - 1) >> 2),
                (log2_size + 1) >> 2,
            )
        } else {
            (15, log2_size - 2)
        };
        let mut last_x = 0u32;
        while last_x < max
            && self.d_se(
                SyntaxElement::LastSignificantCoeffXPrefix,
                ((last_x >> ctx_shift) + ctx_offset) as usize,
            ) == 1
        {
            last_x += 1;
        }
        let mut last_y = 0u32;
        while last_y < max
            && self.d_se(
                SyntaxElement::LastSignificantCoeffYPrefix,
                ((last_y >> ctx_shift) + ctx_offset) as usize,
            ) == 1
        {
            last_y += 1;
        }
        let mut last_x = self.last_sig_value(last_x);
        let mut last_y = self.last_sig_value(last_y);

        if scan_type == ScanType::Vert {
            core::mem::swap(&mut last_x, &mut last_y);
        }

        let scan = Scan::build(scan_type, log2_size);
        let x_cg_last = (last_x >> 2) as usize;
        let y_cg_last = (last_y >> 2) as usize;
        let within_last = scan.within_inv[(last_y & 3) as usize][(last_x & 3) as usize] as u32;
        let group_last = u32::from(scan.group_inv[y_cg_last * scan.cg_side + x_cg_last]);
        let num_coeff = (group_last << 4) + within_last + 1;
        let num_last_subset = ((num_coeff - 1) >> 4) as i32;

        let cg_grid = (1usize << log2_size) / 4;
        let mut sig_group = vec![false; cg_grid.max(1) * cg_grid.max(1)];
        let cg_w = cg_grid.max(1);
        let sig_group_get = |g: &[bool], x: usize, y: usize| -> bool { g[y * cg_w + x] };

        let mut greater1_ctx = 1u32;

        for i in (0..=num_last_subset).rev() {
            let offset = (i << 4) as u32;
            let (x_cg, y_cg) = scan.groups[i as usize];
            let (x_cg, y_cg) = (x_cg as usize, y_cg as usize);

            let mut implicit_nz = false;
            let coded_group;
            if i < num_last_subset && i > 0 {
                let mut ctx_cg = 0u32;
                if x_cg + 1 < cg_w {
                    ctx_cg += u32::from(sig_group_get(&sig_group, x_cg + 1, y_cg));
                }
                if y_cg + 1 < cg_w {
                    ctx_cg += u32::from(sig_group_get(&sig_group, x_cg, y_cg + 1));
                }
                let inc = ctx_cg.min(1) + if c_idx > 0 { 2 } else { 0 };
                coded_group =
                    self.d_se(SyntaxElement::SignificantCoeffGroupFlag, inc as usize) == 1;
                sig_group[y_cg * cg_w + x_cg] = coded_group;
                implicit_nz = true;
            } else {
                coded_group = (x_cg == x_cg_last && y_cg == y_cg_last) || (x_cg == 0 && y_cg == 0);
                sig_group[y_cg * cg_w + x_cg] = coded_group;
            }

            let last_scan_pos = (num_coeff - offset - 1) as i32;
            let mut n_end;
            let mut sig_idx: Vec<u8> = Vec::with_capacity(16);
            if i == num_last_subset {
                n_end = last_scan_pos - 1;
                sig_idx.push(last_scan_pos as u8);
            } else {
                n_end = 15;
            }

            let mut prev_sig = 0u32;
            if x_cg + 1 < cg_w {
                prev_sig = u32::from(sig_group_get(&sig_group, x_cg + 1, y_cg));
            }
            if y_cg + 1 < cg_w {
                prev_sig += u32::from(sig_group_get(&sig_group, x_cg, y_cg + 1)) << 1;
            }

            if coded_group && n_end >= 0 {
                let (ctx_row, mut scf_offset) =
                    sig_ctx_base(c_idx, log2_size, scan_type, prev_sig, x_cg, y_cg);

                let mut n = n_end;
                while n > 0 {
                    let (x_c, y_c) = scan.within[n as usize];
                    let inc = SIG_CTX_IDX_MAP[ctx_row][(y_c << 2 | x_c) as usize] as usize
                        + scf_offset as usize;
                    if self.d_se(SyntaxElement::SignificantCoeffFlag, inc) == 1 {
                        sig_idx.push(n as u8);
                        implicit_nz = false;
                    }
                    n -= 1;
                }
                if !implicit_nz {
                    scf_offset = if i == 0 {
                        if c_idx == 0 { 0 } else { 27 }
                    } else {
                        2 + scf_offset
                    };
                    if self.d_se(SyntaxElement::SignificantCoeffFlag, scf_offset as usize) == 1 {
                        sig_idx.push(0);
                    }
                } else {
                    sig_idx.push(0);
                }
            }

            n_end = sig_idx.len() as i32;
            if n_end == 0 {
                continue;
            }

            let mut ctx_set = if i > 0 && c_idx == 0 { 2u32 } else { 0 };
            if i != num_last_subset && greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;

            let n_g1 = (n_end as usize).min(8);
            let mut g1 = [0u8; 8];
            let mut first_g1_idx: i32 = -1;
            for (m, slot) in g1.iter_mut().enumerate().take(n_g1) {
                let inc = (ctx_set << 2) + greater1_ctx + if c_idx > 0 { 16 } else { 0 };
                let bit = self.d_se(SyntaxElement::CoeffAbsLevelGreater1Flag, inc as usize);
                *slot = bit as u8;
                if bit == 1 {
                    greater1_ctx = 0;
                    if first_g1_idx == -1 {
                        first_g1_idx = m as i32;
                    }
                } else if greater1_ctx > 0 && greater1_ctx < 3 {
                    greater1_ctx += 1;
                }
            }

            let last_nz = i32::from(sig_idx[0]);
            let first_nz = i32::from(sig_idx[(n_end - 1) as usize]);
            let sign_hidden = last_nz - first_nz >= 4;

            if first_g1_idx != -1 {
                let inc = ctx_set + if c_idx > 0 { 4 } else { 0 };
                g1[first_g1_idx as usize] +=
                    self.d_se(SyntaxElement::CoeffAbsLevelGreater2Flag, inc as usize) as u8;
            }

            let nb = n_end as u32;
            let n_signs = if self.pps_sign_hiding() && sign_hidden {
                nb - 1
            } else {
                nb
            };
            let mut signs = [0u8; 16];
            for s in signs.iter_mut().take(n_signs as usize) {
                *s = self.b() as u8;
            }

            let mut c_rice = 0u32;
            let mut sum_abs = 0u32;
            let mut hidden_idx: Option<usize> = None;
            #[allow(clippy::needless_range_loop)]
            for m in 0..n_end as usize {
                let level = if m < 8 {
                    let base = 1 + u32::from(g1[m]);
                    let thr = if m as i32 == first_g1_idx { 3 } else { 2 };
                    if base == thr {
                        let rem = self.coeff_abs_level_remaining(c_rice);
                        let level = base + rem;
                        if level > (3 << c_rice) {
                            c_rice = (c_rice + 1).min(4);
                        }
                        level
                    } else {
                        base
                    }
                } else {
                    let rem = self.coeff_abs_level_remaining(c_rice);
                    let level = 1 + rem;
                    if level > (3 << c_rice) {
                        c_rice = (c_rice + 1).min(4);
                    }
                    level
                };
                sum_abs = sum_abs.wrapping_add(level);
                self.stats.coeff_count += 1;

                if c_idx == 0 {
                    let (wx, wy) = scan.within[sig_idx[m] as usize];
                    let cx = x_cg * 4 + wx as usize;
                    let cy = y_cg * 4 + wy as usize;
                    let buf_idx = cy * n + cx;
                    self.coeff_buf[buf_idx] = if m < n_signs as usize {
                        if signs[m] == 1 {
                            -(level as i32)
                        } else {
                            level as i32
                        }
                    } else {
                        hidden_idx = Some(buf_idx);
                        level as i32
                    };
                }
            }
            if let Some(buf_idx) = hidden_idx {
                if sum_abs & 1 == 1 {
                    self.coeff_buf[buf_idx] = -self.coeff_buf[buf_idx];
                }
            }
        }
    }

    fn last_sig_value(&mut self, prefix: u32) -> u32 {
        if prefix > 3 {
            let length = (prefix >> 1) - 1;
            let suffix = self.b_bits(length);
            (1 << ((prefix >> 1) - 1)) * (2 + (prefix & 1)) + suffix
        } else {
            prefix
        }
    }

    fn coeff_abs_level_remaining(&mut self, rice: u32) -> u32 {
        let mut prefix = 0u32;
        while prefix < CABAC_MAX_BIN && self.b() == 1 {
            prefix += 1;
        }
        if prefix < 3 {
            let suffix = self.b_bits(rice);
            (prefix << rice) + suffix
        } else {
            let prefix_minus3 = prefix - 3;
            let suffix = self.b_bits(prefix_minus3 + rice);
            (((1 << prefix_minus3) + 3 - 1) << rice) + suffix
        }
    }

    fn pps_sign_hiding(&self) -> bool {
        self.sign_data_hiding
    }
}

fn sig_ctx_base(
    c_idx: u32,
    log2_size: u32,
    scan_type: ScanType,
    prev_sig: u32,
    x_cg: usize,
    y_cg: usize,
) -> (usize, u32) {
    let mut scf_offset = if c_idx != 0 { 27 } else { 0 };
    if log2_size == 2 {
        return (0, scf_offset);
    }
    let row = (prev_sig + 1) as usize;
    if c_idx == 0 {
        if x_cg > 0 || y_cg > 0 {
            scf_offset += 3;
        }
        if log2_size == 3 {
            scf_offset += if scan_type == ScanType::Diag { 9 } else { 15 };
        } else {
            scf_offset += 21;
        }
    } else if log2_size == 3 {
        scf_offset += 9;
    } else {
        scf_offset += 12;
    }
    (row, scf_offset)
}

fn scan_for_mode(mode: u8) -> ScanType {
    if (6..=14).contains(&mode) {
        ScanType::Vert
    } else if (22..=30).contains(&mode) {
        ScanType::Horiz
    } else {
        ScanType::Diag
    }
}

pub fn decode_slice_data(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceSegmentHeader,
    rbsp: &[u8],
) -> Result<CtuStats> {
    let (stats, _, _) = decode_inner(sps, pps, sh, rbsp, &[], false)?;
    Ok(stats)
}

pub fn decode_slice_to_luma(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceSegmentHeader,
    rbsp: &[u8],
) -> Result<LumaFrame> {
    decode_slice_to_luma_tracked(sps, pps, sh, rbsp, &[])
}

pub fn decode_slice_to_luma_tracked(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceSegmentHeader,
    rbsp: &[u8],
    skipped_bytes: &[usize],
) -> Result<LumaFrame> {
    let (_, luma, _) = decode_inner(sps, pps, sh, rbsp, skipped_bytes, false)?;
    Ok(crop_luma(sps, &luma))
}

fn crop_luma(sps: &Sps, coded: &[u8]) -> LumaFrame {
    let pic_w = sps.pic_width as usize;
    let cw = sps.cropped_width() as usize;
    let ch = sps.cropped_height() as usize;
    let left = (sps.sub_width_c() * sps.conf_win_left) as usize;
    let top = (sps.sub_height_c() * sps.conf_win_top) as usize;
    let mut data = Vec::with_capacity(cw * ch);
    for y in 0..ch {
        let start = (top + y) * pic_w + left;
        data.extend_from_slice(&coded[start..start + cw]);
    }
    LumaFrame {
        width: cw,
        height: ch,
        data,
    }
}

#[doc(hidden)]
pub fn decode_slice_data_traced(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceSegmentHeader,
    rbsp: &[u8],
) -> Result<(CtuStats, Vec<CabacTraceOp>)> {
    let (stats, _, trace) = decode_inner(sps, pps, sh, rbsp, &[], true)?;
    Ok((stats, trace.unwrap_or_default()))
}

fn wpp_row_offsets(sh: &SliceSegmentHeader, skipped_bytes: &[usize]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(sh.entry_point_offsets.len() + 1);
    let mut rbsp_off = sh.data_byte_offset;
    let mut ebsp_off = sh.data_byte_offset + skipped_before(skipped_bytes, sh.data_byte_offset);
    offsets.push(rbsp_off);
    for &len_ebsp in &sh.entry_point_offsets {
        let next_ebsp = ebsp_off + len_ebsp as usize;
        let cmpt = skipped_bytes
            .iter()
            .filter(|&&p| p >= ebsp_off && p < next_ebsp)
            .count();
        rbsp_off += len_ebsp as usize - cmpt;
        ebsp_off = next_ebsp;
        offsets.push(rbsp_off);
    }
    offsets
}

fn skipped_before(skipped_bytes: &[usize], ebsp_off: usize) -> usize {
    skipped_bytes
        .iter()
        .enumerate()
        .filter(|&(i, &p)| p - i < ebsp_off)
        .count()
}

fn decode_inner(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceSegmentHeader,
    rbsp: &[u8],
    skipped_bytes: &[usize],
    record_trace: bool,
) -> Result<(CtuStats, Vec<u8>, Option<Vec<CabacTraceOp>>)> {
    if pps.transquant_bypass_enabled {
        return Err(Error::Unsupported(
            "hevc ctu: transquant_bypass not supported".into(),
        ));
    }
    if pps.tiles_enabled {
        return Err(Error::Unsupported("hevc ctu: tiles not supported".into()));
    }

    let max_luma_samples: u64 = 35_651_584;
    let luma_samples = u64::from(sps.pic_width).checked_mul(u64::from(sps.pic_height));
    if luma_samples.is_none_or(|px| px > max_luma_samples) {
        return Err(Error::Unsupported(
            "hevc: frame dimensions exceed the native decoder limit".into(),
        ));
    }

    let wpp = pps.entropy_coding_sync_enabled;
    let row_offsets = if wpp {
        wpp_row_offsets(sh, skipped_bytes)
    } else {
        Vec::new()
    };

    let cabac = CabacDecoder::new(rbsp, sh.data_byte_offset, sh.slice_qp)?;
    let trace = if record_trace { Some(Vec::new()) } else { None };
    let mut dec = CtuDecoder::new(cabac, sps, sh, wpp, row_offsets, trace);
    dec.sign_data_hiding = pps.sign_data_hiding_enabled;
    dec.transform_skip_enabled = pps.transform_skip_enabled;
    dec.cu_qp_delta_enabled = pps.cu_qp_delta_enabled;
    dec.log2_min_cu_qp_delta_size = sps.log2_ctb_size - pps.diff_cu_qp_delta_depth;
    dec.decode()?;
    Ok((dec.stats, dec.luma, dec.trace))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_with_entry_points(data_byte_offset: usize, offsets: Vec<u32>) -> SliceSegmentHeader {
        SliceSegmentHeader {
            first_slice_segment_in_pic: true,
            slice_type: crate::hevc::SliceType::I,
            slice_pic_parameter_set_id: 0,
            slice_segment_address: 0,
            slice_qp: 26,
            sao_luma: false,
            sao_chroma: false,
            deblocking_filter_disabled: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            loop_filter_across_slices_enabled: true,
            data_byte_offset,
            entry_point_offsets: offsets,
        }
    }

    #[test]
    fn wpp_row_offsets_without_epb_are_cumulative() {
        let sh = sh_with_entry_points(5, vec![10, 7, 20]);
        assert_eq!(wpp_row_offsets(&sh, &[]), vec![5, 15, 22, 42]);
    }

    #[test]
    fn wpp_row_offsets_subtract_epb_inside_substream() {
        let sh = sh_with_entry_points(0, vec![10, 7]);
        assert_eq!(wpp_row_offsets(&sh, &[4]), vec![0, 9, 16]);
    }

    #[test]
    fn skipped_before_counts_header_epb() {
        assert_eq!(skipped_before(&[2, 6], 4), 1);
        assert_eq!(skipped_before(&[2, 6], 6), 2);
    }

    #[test]
    fn diag_scan4x4_matches_reference() {
        let s = diag_scan(4);
        let xs: Vec<u8> = s.iter().map(|&(x, _)| x).collect();
        let ys: Vec<u8> = s.iter().map(|&(_, y)| y).collect();
        assert_eq!(xs, [0, 0, 1, 0, 1, 2, 0, 1, 2, 3, 1, 2, 3, 2, 3, 3]);
        assert_eq!(ys, [0, 1, 0, 2, 1, 0, 3, 2, 1, 0, 3, 2, 1, 3, 2, 3]);
    }

    #[test]
    fn diag_scan8x8_anchors() {
        let s = diag_scan(8);
        assert_eq!(s.len(), 64);
        assert_eq!(s[0], (0, 0));
        assert_eq!(s[1], (0, 1));
        assert_eq!(s[2], (1, 0));
        assert_eq!(s[10], (0, 4));
        assert_eq!(s[63], (7, 7));
    }

    #[test]
    fn horiz_vert_scan4x4() {
        let h = horiz_scan(4);
        let hx: Vec<u8> = h.iter().map(|&(x, _)| x).collect();
        let hy: Vec<u8> = h.iter().map(|&(_, y)| y).collect();
        assert_eq!(hx, [0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3]);
        assert_eq!(hy, [0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
        let v = vert_scan(4);
        assert_eq!(v[0], (0, 0));
        assert_eq!(v[1], (0, 1));
        assert_eq!(v[4], (1, 0));
    }

    #[test]
    fn diag_within_inverse() {
        let scan = Scan::build(ScanType::Diag, 4);
        assert_eq!(scan.within_inv[0], [0, 2, 5, 9]);
        assert_eq!(scan.within_inv[1], [1, 4, 8, 12]);
        assert_eq!(scan.within_inv[2], [3, 7, 11, 14]);
        assert_eq!(scan.within_inv[3], [6, 10, 13, 15]);
    }

    #[test]
    fn scan_selection() {
        assert_eq!(scan_for_mode(0), ScanType::Diag);
        assert_eq!(scan_for_mode(10), ScanType::Vert);
        assert_eq!(scan_for_mode(26), ScanType::Horiz);
        assert_eq!(scan_for_mode(34), ScanType::Diag);
    }

    #[test]
    fn deblock_tables_match_spec() {
        assert_eq!(BETA_TABLE.len(), 52);
        assert_eq!(BETA_TABLE[15], 0);
        assert_eq!(BETA_TABLE[16], 6);
        assert_eq!(BETA_TABLE[28], 18);
        assert_eq!(BETA_TABLE[29], 20);
        assert_eq!(BETA_TABLE[51], 64);
        assert_eq!(TC_TABLE.len(), 54);
        assert_eq!(TC_TABLE[17], 0);
        assert_eq!(TC_TABLE[18], 1);
        assert_eq!(TC_TABLE[37], 4);
        assert_eq!(TC_TABLE[38], 5);
        assert_eq!(TC_TABLE[53], 24);
    }
}
