use vidcull_core::{Error, Result};

use super::bitstream::BitReader;
use super::cabac::{CabacDecoder, ResidualCat};
use super::deblock::{self, MbDeblockInfo};
use super::nal::NalHeader;
use super::params::{Pps, Sps};
use super::slice::{SliceHeader, SliceType, parse_slice_header};
use super::{cavlc, intra, transform};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LumaFrame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

const BLK_POS: [(usize, usize); 16] = [
    (0, 0),
    (1, 0),
    (0, 1),
    (1, 1),
    (2, 0),
    (3, 0),
    (2, 1),
    (3, 1),
    (0, 2),
    (1, 2),
    (0, 3),
    (1, 3),
    (2, 2),
    (3, 2),
    (2, 3),
    (3, 3),
];

const BLK_INV: [usize; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

const CBP_INTRA: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

fn clamp_u8(v: i32) -> u8 {
    u8::try_from(v.clamp(0, 255)).expect("value clamped into 0..=255")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IMbType {
    NxN {
        transform_8x8: bool,
    },
    I16x16 {
        pred_mode: u8,
        cbp_luma: u8,
        cbp_chroma: u8,
    },
    Pcm,
}

struct FrameDecoder<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    pic_w_mbs: usize,
    pic_h_mbs: usize,
    luma: Vec<u8>,
    luma_nnz: Vec<u8>,
    chroma_nnz: [Vec<u8>; 2],
    pred_mode: Vec<i8>,
    decoded: Vec<bool>,
    qp_prev: i32,
    mb_db: Vec<MbDeblockInfo>,
    slice_disable_idc: u8,
    slice_alpha_off: i32,
    slice_beta_off: i32,
    cur_slice_id: u32,
    cbp_full: Vec<u16>,
    mb_i16_or_pcm: Vec<bool>,
    mb_transform8x8: Vec<bool>,
    mb_chroma_pred: Vec<u8>,
    last_qscale_diff: i32,
}

impl<'a> FrameDecoder<'a> {
    fn new(sps: &'a Sps, pps: &'a Pps) -> Self {
        let pic_w_mbs = sps.pic_width_in_mbs as usize;
        let frame_h_mbs =
            (2 - usize::from(sps.frame_mbs_only_flag)) * sps.pic_height_in_map_units as usize;
        let luma_w = pic_w_mbs * 16;
        let luma_h = frame_h_mbs * 16;
        let total_mbs = pic_w_mbs * frame_h_mbs;
        Self {
            sps,
            pps,
            pic_w_mbs,
            pic_h_mbs: frame_h_mbs,
            luma: vec![0u8; luma_w * luma_h],
            luma_nnz: vec![0u8; (pic_w_mbs * 4) * (frame_h_mbs * 4)],
            chroma_nnz: [
                vec![0u8; (pic_w_mbs * 2) * (frame_h_mbs * 2)],
                vec![0u8; (pic_w_mbs * 2) * (frame_h_mbs * 2)],
            ],
            pred_mode: vec![-1i8; (pic_w_mbs * 4) * (frame_h_mbs * 4)],
            decoded: vec![false; total_mbs],
            qp_prev: 0,
            mb_db: vec![MbDeblockInfo::default(); total_mbs],
            slice_disable_idc: 0,
            slice_alpha_off: 0,
            slice_beta_off: 0,
            cur_slice_id: 0,
            cbp_full: vec![0u16; total_mbs],
            mb_i16_or_pcm: vec![false; total_mbs],
            mb_transform8x8: vec![false; total_mbs],
            mb_chroma_pred: vec![0u8; total_mbs],
            last_qscale_diff: 0,
        }
    }

    fn luma_w(&self) -> usize {
        self.pic_w_mbs * 16
    }

    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.luma_w() + x
    }

    fn get(&self, x: usize, y: usize) -> u8 {
        let i = self.idx(x, y);
        self.luma[i]
    }

    fn set(&mut self, x: usize, y: usize, v: u8) {
        let i = self.idx(x, y);
        self.luma[i] = v;
    }

    fn sample_available(&self, px: i32, py: i32, cur_addr: usize, cur_scan: usize) -> bool {
        if px < 0 || py < 0 {
            return false;
        }
        let (px, py) = (
            usize::try_from(px).expect("px non-negative after guard"),
            usize::try_from(py).expect("py non-negative after guard"),
        );
        if px >= self.luma_w() || py >= self.pic_h_mbs * 16 {
            return false;
        }
        let nb = (py / 16) * self.pic_w_mbs + (px / 16);
        if nb == cur_addr {
            let bx = (px % 16) / 4;
            let by = (py % 16) / 4;
            BLK_INV[by * 4 + bx] < cur_scan
        } else {
            nb < cur_addr && self.decoded[nb]
        }
    }

    fn mb_available(&self, cur_addr: usize, off_x: isize, off_y: isize) -> bool {
        let base_x = isize::try_from(cur_addr % self.pic_w_mbs).expect("mb col fits isize") + off_x;
        let base_y = isize::try_from(cur_addr / self.pic_w_mbs).expect("mb row fits isize") + off_y;
        let pic_w = isize::try_from(self.pic_w_mbs).expect("pic_w_mbs fits isize");
        let pic_h = isize::try_from(self.pic_h_mbs).expect("pic_h_mbs fits isize");
        if base_x < 0 || base_y < 0 || base_x >= pic_w || base_y >= pic_h {
            return false;
        }
        let nb = usize::try_from(base_y).expect("base_y non-negative after guard") * self.pic_w_mbs
            + usize::try_from(base_x).expect("base_x non-negative after guard");
        nb < cur_addr && self.decoded[nb]
    }

    fn luma_nc(&self, bx: usize, by: usize, cur_addr: usize, cur_scan: usize) -> i32 {
        let gw = self.pic_w_mbs * 4;
        let gh = self.pic_h_mbs * 4;
        let avail = |nx: isize, ny: isize| -> Option<u8> {
            let nx = usize::try_from(nx).ok()?;
            let ny = usize::try_from(ny).ok()?;
            if nx >= gw || ny >= gh {
                return None;
            }
            let nb = (ny / 4) * self.pic_w_mbs + (nx / 4);
            let ok = if nb == cur_addr {
                BLK_INV[(ny % 4) * 4 + (nx % 4)] < cur_scan
            } else {
                nb < cur_addr && self.decoded[nb]
            };
            ok.then(|| self.luma_nnz[ny * gw + nx])
        };
        let ibx = isize::try_from(bx).expect("bx fits isize");
        let iby = isize::try_from(by).expect("by fits isize");
        nc_from(avail(ibx - 1, iby), avail(ibx, iby - 1))
    }

    fn chroma_nc(
        &self,
        comp: usize,
        bx: usize,
        by: usize,
        cur_addr: usize,
        cur_scan: usize,
    ) -> i32 {
        let gw = self.pic_w_mbs * 2;
        let gh = self.pic_h_mbs * 2;
        let avail = |nx: isize, ny: isize| -> Option<u8> {
            let nx = usize::try_from(nx).ok()?;
            let ny = usize::try_from(ny).ok()?;
            if nx >= gw || ny >= gh {
                return None;
            }
            let nb = (ny / 2) * self.pic_w_mbs + (nx / 2);
            let ok = if nb == cur_addr {
                ((ny % 2) * 2 + (nx % 2)) < cur_scan
            } else {
                nb < cur_addr && self.decoded[nb]
            };
            ok.then(|| self.chroma_nnz[comp][ny * gw + nx])
        };
        let ibx = isize::try_from(bx).expect("bx fits isize");
        let iby = isize::try_from(by).expect("by fits isize");
        nc_from(avail(ibx - 1, iby), avail(ibx, iby - 1))
    }

    fn predicted_intra_mode(&self, bx: usize, by: usize, cur_addr: usize) -> u8 {
        let gw = self.pic_w_mbs * 4;
        let gh = self.pic_h_mbs * 4;
        let mode_at = |nx: isize, ny: isize| -> Option<i8> {
            let nx = usize::try_from(nx).ok()?;
            let ny = usize::try_from(ny).ok()?;
            if nx >= gw || ny >= gh {
                return None;
            }
            let nb = (ny / 4) * self.pic_w_mbs + (nx / 4);
            if nb == cur_addr || (nb < cur_addr && self.decoded[nb]) {
                Some(self.pred_mode[ny * gw + nx])
            } else {
                None
            }
        };
        let ibx = isize::try_from(bx).expect("bx fits isize");
        let iby = isize::try_from(by).expect("by fits isize");
        let left = mode_at(ibx - 1, iby);
        let top = mode_at(ibx, iby - 1);
        if left.is_none() || top.is_none() {
            return 2;
        }
        let m = |o: Option<i8>| -> u8 {
            match o {
                Some(v) if v >= 0 => u8::try_from(v).expect("intra mode 0..=8 fits u8"),
                _ => 2,
            }
        };
        m(left).min(m(top))
    }
}

fn nc_from(a: Option<u8>, b: Option<u8>) -> i32 {
    match (a, b) {
        (Some(na), Some(nb)) => (i32::from(na) + i32::from(nb) + 1) >> 1,
        (Some(na), None) => i32::from(na),
        (None, Some(nb)) => i32::from(nb),
        (None, None) => 0,
    }
}

pub fn decode_intra_frame(
    sps: &Sps,
    pps: &Pps,
    slices: &[(&NalHeader, &[u8])],
) -> Result<LumaFrame> {
    if sps.chroma_array_type() != 1 {
        return Err(Error::Unsupported(
            "h264: native decoder supports 4:2:0 (ChromaArrayType 1) only".into(),
        ));
    }
    if !sps.frame_mbs_only_flag {
        return Err(Error::Unsupported(
            "h264: field/MBAFF coding not supported".into(),
        ));
    }

    let max_frame_mbs: u64 = 139_264;
    let frame_mbs =
        u64::from(sps.pic_width_in_mbs).checked_mul(u64::from(sps.pic_height_in_map_units));
    if frame_mbs.is_none_or(|mbs| mbs > max_frame_mbs) {
        return Err(Error::Unsupported(
            "h264: frame dimensions exceed the native decoder limit".into(),
        ));
    }

    let mut dec = FrameDecoder::new(sps, pps);
    for (nal, rbsp) in slices {
        let mut reader = BitReader::new(rbsp);
        let sh = parse_slice_header(&mut reader, sps, pps, nal)?;
        if sh.slice_type != SliceType::I {
            return Err(Error::Unsupported(
                "h264: only I slices are decodable".into(),
            ));
        }
        if pps.entropy_coding_mode_flag {
            dec.decode_slice_cabac(rbsp, reader.bit_pos(), &sh)?;
        } else {
            dec.decode_slice(&mut reader, &sh)?;
        }
    }
    dec.deblock();
    Ok(dec.crop())
}

impl FrameDecoder<'_> {
    fn decode_slice(&mut self, reader: &mut BitReader, sh: &SliceHeader) -> Result<()> {
        self.qp_prev = sh.slice_qp;
        self.slice_disable_idc = u8::try_from(sh.disable_deblocking_filter_idc.min(2))
            .expect("disable_deblocking_filter_idc clamped to 0..=2");
        self.slice_alpha_off = sh.slice_alpha_c0_offset;
        self.slice_beta_off = sh.slice_beta_offset;
        let total = self.pic_w_mbs * self.pic_h_mbs;
        let mut addr = usize::try_from(sh.first_mb_in_slice).expect("first_mb_in_slice fits usize");
        loop {
            if addr >= total {
                return Err(Error::Parse("h264: macroblock address past frame".into()));
            }
            self.decode_macroblock(reader, addr)?;
            self.decoded[addr] = true;
            addr += 1;
            if !reader.more_rbsp_data() {
                break;
            }
        }
        self.cur_slice_id += 1;
        Ok(())
    }

    fn record_mb(&mut self, addr: usize, qp: i32, transform_8x8: bool) {
        self.mb_db[addr] = MbDeblockInfo {
            present: true,
            qp,
            transform_8x8,
            disable_idc: self.slice_disable_idc,
            alpha_off: self.slice_alpha_off,
            beta_off: self.slice_beta_off,
            slice_id: self.cur_slice_id,
        };
    }

    fn deblock(&mut self) {
        deblock::deblock_luma(&mut self.luma, self.pic_w_mbs, self.pic_h_mbs, &self.mb_db);
    }

    fn decode_macroblock(&mut self, reader: &mut BitReader, addr: usize) -> Result<()> {
        let raw = reader.ue()?;
        let mb_type = classify_i_mb_type(raw)?;
        match mb_type {
            IMbType::Pcm => self.decode_pcm(reader, addr),
            IMbType::NxN { .. } => {
                let transform_8x8 = if self.pps.transform_8x8_mode_flag {
                    reader.read_flag()?
                } else {
                    false
                };
                self.decode_nxn(reader, addr, transform_8x8)
            }
            IMbType::I16x16 {
                pred_mode,
                cbp_luma,
                cbp_chroma,
            } => self.decode_16x16(reader, addr, pred_mode, cbp_luma, cbp_chroma),
        }
    }

    fn read_qp(&mut self, reader: &mut BitReader) -> Result<i32> {
        let delta = reader.se()?;
        let qp = (self.qp_prev + delta + 52).rem_euclid(52);
        self.qp_prev = qp;
        Ok(qp)
    }

    fn decode_16x16(
        &mut self,
        reader: &mut BitReader,
        addr: usize,
        pred_mode: u8,
        cbp_luma: u8,
        cbp_chroma: u8,
    ) -> Result<()> {
        let _intra_chroma_pred_mode = reader.ue()?;
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        let (ox, oy) = (mb_x * 16, mb_y * 16);

        self.set_mb_pred_modes(addr, &[-1; 16]);

        self.predict_16x16_into(addr, pred_mode);

        let qp = self.read_qp(reader)?;
        self.record_mb(addr, qp, false);

        let dc_nc = self.luma_nc(mb_x * 4, mb_y * 4, addr, 0);
        let dc_block = cavlc::residual_block(reader, 16, dc_nc)?;
        let dc_values = transform::luma_dc_transform(&dc_block.coeffs, qp);

        for (scan, &(bx, by)) in BLK_POS.iter().enumerate() {
            let gx = mb_x * 4 + bx;
            let gy = mb_y * 4 + by;
            let ac = if cbp_luma & (1 << (scan / 4)) != 0 {
                let nc = self.luma_nc(gx, gy, addr, scan);
                let blk = cavlc::residual_block(reader, 15, nc)?;
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] =
                    u8::try_from(blk.total_coeff).expect("TotalCoeff 0..=16 fits u8");
                blk.coeffs
            } else {
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
                [0; 16]
            };
            let dc = dc_values[by * 4 + bx];
            self.add_ac_residual(ox + bx * 4, oy + by * 4, &ac, dc, qp);
        }

        self.consume_chroma(reader, addr, cbp_chroma)
    }

    fn predict_16x16_into(&mut self, addr: usize, mode: u8) {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        let (ox, oy) = (mb_x * 16, mb_y * 16);

        let top = self.mb_available(addr, 0, -1).then(|| {
            let mut t = [0u8; 16];
            for (i, s) in t.iter_mut().enumerate() {
                *s = self.get(ox + i, oy - 1);
            }
            t
        });
        let left = self.mb_available(addr, -1, 0).then(|| {
            let mut l = [0u8; 16];
            for (i, s) in l.iter_mut().enumerate() {
                *s = self.get(ox - 1, oy + i);
            }
            l
        });
        let top_left = self
            .mb_available(addr, -1, -1)
            .then(|| self.get(ox - 1, oy - 1));

        let pred = intra::predict_16x16(mode, top, left, top_left);
        for y in 0..16 {
            for x in 0..16 {
                self.set(ox + x, oy + y, pred[y * 16 + x]);
            }
        }
    }

    fn add_ac_residual(&mut self, ax: usize, ay: usize, ac_scan: &[i32; 16], dc: i32, qp: i32) {
        let mut full = [0i32; 16];
        full[1..16].copy_from_slice(&ac_scan[0..15]);
        let raster = transform::inverse_scan_4x4(&full);
        let mut deq = transform::dequant_4x4(&raster, qp, true);
        deq[0] = dc;
        let res = transform::idct_4x4(&deq);
        for i in 0..4 {
            for j in 0..4 {
                let v = i32::from(self.get(ax + j, ay + i)) + res[i * 4 + j];
                self.set(ax + j, ay + i, clamp_u8(v));
            }
        }
    }

    fn decode_nxn(
        &mut self,
        reader: &mut BitReader,
        addr: usize,
        transform_8x8: bool,
    ) -> Result<()> {
        if transform_8x8 {
            self.decode_8x8(reader, addr)
        } else {
            self.decode_4x4(reader, addr)
        }
    }

    fn decode_4x4(&mut self, reader: &mut BitReader, addr: usize) -> Result<()> {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);

        let mut modes = [0u8; 16];
        for (scan, mode_slot) in modes.iter_mut().enumerate() {
            let (bx, by) = BLK_POS[scan];
            let pred = self.predicted_intra_mode(mb_x * 4 + bx, mb_y * 4 + by, addr);
            *mode_slot = Self::read_intra4x4_mode(reader, pred)?;
            self.pred_mode[(mb_y * 4 + by) * (self.pic_w_mbs * 4) + (mb_x * 4 + bx)] =
                i8::try_from(*mode_slot).expect("intra mode 0..=8 fits i8");
        }
        let _intra_chroma_pred_mode = reader.ue()?;

        let cbp = Self::read_cbp(reader)?;
        let cbp_luma = cbp & 0x0F;
        let cbp_chroma = cbp >> 4;

        let qp = if cbp_luma != 0 || cbp_chroma != 0 {
            self.read_qp(reader)?
        } else {
            self.qp_prev
        };
        self.record_mb(addr, qp, false);

        for scan in 0..16 {
            let (bx, by) = BLK_POS[scan];
            let gx = mb_x * 4 + bx;
            let gy = mb_y * 4 + by;
            let (ax, ay) = (gx * 4, gy * 4);

            self.predict_4x4_into(addr, scan, ax, ay, modes[scan]);

            if cbp_luma & (1 << (scan / 4)) != 0 {
                let nc = self.luma_nc(gx, gy, addr, scan);
                let blk = cavlc::residual_block(reader, 16, nc)?;
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] =
                    u8::try_from(blk.total_coeff).expect("TotalCoeff 0..=16 fits u8");
                self.add_residual_4x4(ax, ay, &blk.coeffs, qp);
            } else {
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
            }
        }

        self.consume_chroma(reader, addr, cbp_chroma)
    }

    fn read_intra4x4_mode(reader: &mut BitReader, predicted: u8) -> Result<u8> {
        if reader.read_flag()? {
            Ok(predicted)
        } else {
            let rem = u8::try_from(reader.read_bits(3)?)
                .map_err(|_| Error::Parse("h264: rem_intra4x4_pred_mode out of range".into()))?;
            Ok(if rem < predicted { rem } else { rem + 1 })
        }
    }

    fn predict_4x4_into(&mut self, addr: usize, scan: usize, ax: usize, ay: usize, mode: u8) {
        let iax = i32::try_from(ax).expect("ax fits i32");
        let iay = i32::try_from(ay).expect("ay fits i32");
        let s =
            |dx: i32, dy: i32| -> bool { self.sample_available(iax + dx, iay + dy, addr, scan) };
        let top = s(0, -1).then(|| {
            [
                self.get(ax, ay - 1),
                self.get(ax + 1, ay - 1),
                self.get(ax + 2, ay - 1),
                self.get(ax + 3, ay - 1),
            ]
        });
        let top_right = s(4, -1).then(|| {
            [
                self.get(ax + 4, ay - 1),
                self.get(ax + 5, ay - 1),
                self.get(ax + 6, ay - 1),
                self.get(ax + 7, ay - 1),
            ]
        });
        let left = s(-1, 0).then(|| {
            [
                self.get(ax - 1, ay),
                self.get(ax - 1, ay + 1),
                self.get(ax - 1, ay + 2),
                self.get(ax - 1, ay + 3),
            ]
        });
        let top_left = s(-1, -1).then(|| self.get(ax - 1, ay - 1));

        let n = intra::Neighbors4x4 {
            top,
            top_right,
            left,
            top_left,
        };
        let pred = intra::predict_4x4(mode, &n);
        for i in 0..4 {
            for j in 0..4 {
                self.set(ax + j, ay + i, pred[i * 4 + j]);
            }
        }
    }

    fn add_residual_4x4(&mut self, ax: usize, ay: usize, scan_coeffs: &[i32; 16], qp: i32) {
        let raster = transform::inverse_scan_4x4(scan_coeffs);
        let deq = transform::dequant_4x4(&raster, qp, false);
        let res = transform::idct_4x4(&deq);
        for i in 0..4 {
            for j in 0..4 {
                let v = i32::from(self.get(ax + j, ay + i)) + res[i * 4 + j];
                self.set(ax + j, ay + i, clamp_u8(v));
            }
        }
    }

    fn decode_8x8(&mut self, reader: &mut BitReader, addr: usize) -> Result<()> {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);

        let mut modes = [0u8; 4];
        for (blk8, mode_slot) in modes.iter_mut().enumerate() {
            let (b8x, b8y) = (blk8 % 2, blk8 / 2);
            let pred = self.predicted_intra_mode(mb_x * 4 + b8x * 2, mb_y * 4 + b8y * 2, addr);
            let mode = Self::read_intra4x4_mode(reader, pred)?;
            *mode_slot = mode;
            for dy in 0..2 {
                for dx in 0..2 {
                    let gx = mb_x * 4 + b8x * 2 + dx;
                    let gy = mb_y * 4 + b8y * 2 + dy;
                    self.pred_mode[gy * (self.pic_w_mbs * 4) + gx] =
                        i8::try_from(mode).expect("intra mode 0..=8 fits i8");
                }
            }
        }
        let _intra_chroma_pred_mode = reader.ue()?;

        let cbp = Self::read_cbp(reader)?;
        let cbp_luma = cbp & 0x0F;
        let cbp_chroma = cbp >> 4;

        let qp = if cbp_luma != 0 || cbp_chroma != 0 {
            self.read_qp(reader)?
        } else {
            self.qp_prev
        };
        self.record_mb(addr, qp, true);

        for (blk8, &mode) in modes.iter().enumerate() {
            let (b8x, b8y) = (blk8 % 2, blk8 / 2);
            let (ax, ay) = ((mb_x * 16) + b8x * 8, (mb_y * 16) + b8y * 8);
            self.predict_8x8_into(addr, blk8, ax, ay, mode);

            if cbp_luma & (1 << blk8) != 0 {
                let mut level8x8 = [0i32; 64];
                for i4 in 0..4 {
                    let cx = b8x * 2 + (i4 % 2);
                    let cy = b8y * 2 + (i4 / 2);
                    let gx = mb_x * 4 + cx;
                    let gy = mb_y * 4 + cy;
                    let nc = self.luma_nc(gx, gy, addr, BLK_INV[cy * 4 + cx]);
                    let blk = cavlc::residual_block(reader, 16, nc)?;
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] =
                        u8::try_from(blk.total_coeff).expect("TotalCoeff 0..=16 fits u8");
                    for k in 0..16 {
                        level8x8[4 * k + i4] = blk.coeffs[k];
                    }
                }
                self.add_residual_8x8(ax, ay, &level8x8, qp);
            } else {
                for i4 in 0..4 {
                    let gx = mb_x * 4 + b8x * 2 + (i4 % 2);
                    let gy = mb_y * 4 + b8y * 2 + (i4 / 2);
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
                }
            }
        }

        self.consume_chroma(reader, addr, cbp_chroma)
    }

    fn predict_8x8_into(&mut self, addr: usize, blk8: usize, ax: usize, ay: usize, mode: u8) {
        let (b8x, b8y) = (blk8 % 2, blk8 / 2);
        let scan = BLK_INV[(b8y * 2) * 4 + (b8x * 2)];
        let iax = i32::try_from(ax).expect("ax fits i32");
        let iay = i32::try_from(ay).expect("ay fits i32");
        let s =
            |dx: i32, dy: i32| -> bool { self.sample_available(iax + dx, iay + dy, addr, scan) };
        let row = |this: &Self, x0: usize, y: usize| -> [u8; 8] {
            let mut r = [0u8; 8];
            for (i, v) in r.iter_mut().enumerate() {
                *v = this.get(x0 + i, y);
            }
            r
        };
        let top = s(0, -1).then(|| row(self, ax, ay - 1));
        let top_right = s(8, -1).then(|| row(self, ax + 8, ay - 1));
        let left = s(-1, 0).then(|| {
            let mut l = [0u8; 8];
            for (i, v) in l.iter_mut().enumerate() {
                *v = self.get(ax - 1, ay + i);
            }
            l
        });
        let top_left = s(-1, -1).then(|| self.get(ax - 1, ay - 1));

        let n = intra::Neighbors8x8 {
            top,
            top_right,
            left,
            top_left,
        };
        let pred = intra::predict_8x8(mode, &n);
        for y in 0..8 {
            for x in 0..8 {
                self.set(ax + x, ay + y, pred[y * 8 + x]);
            }
        }
    }

    fn add_residual_8x8(&mut self, ax: usize, ay: usize, scan_coeffs: &[i32; 64], qp: i32) {
        let raster = transform::inverse_scan_8x8(scan_coeffs);
        let deq = transform::dequant_8x8(&raster, qp);
        let res = transform::idct_8x8(&deq);
        for i in 0..8 {
            for j in 0..8 {
                let v = i32::from(self.get(ax + j, ay + i)) + res[i * 8 + j];
                self.set(ax + j, ay + i, clamp_u8(v));
            }
        }
    }

    fn decode_pcm(&mut self, reader: &mut BitReader, addr: usize) -> Result<()> {
        reader.align_to_byte();
        self.qp_prev = 0;
        self.record_mb(addr, 0, false);
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        let (ox, oy) = (mb_x * 16, mb_y * 16);
        for y in 0..16 {
            for x in 0..16 {
                let s = u8::try_from(reader.read_bits(8)?)
                    .map_err(|_| Error::Parse("h264: pcm luma sample".into()))?;
                self.set(ox + x, oy + y, s);
            }
        }
        for _ in 0..128 {
            let _ = reader.read_bits(8)?;
        }
        self.set_mb_pred_modes(addr, &[-1; 16]);
        for by in 0..4 {
            for bx in 0..4 {
                let gx = mb_x * 4 + bx;
                let gy = mb_y * 4 + by;
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 16;
            }
        }
        for comp in 0..2 {
            for by in 0..2 {
                for bx in 0..2 {
                    let gx = mb_x * 2 + bx;
                    let gy = mb_y * 2 + by;
                    self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] = 16;
                }
            }
        }
        Ok(())
    }

    fn read_cbp(reader: &mut BitReader) -> Result<u8> {
        let code = usize::try_from(reader.ue()?).expect("ue() fits usize");
        CBP_INTRA.get(code).copied().ok_or_else(|| {
            Error::Parse(format!("h264: coded_block_pattern codeNum {code} invalid"))
        })
    }

    fn set_mb_pred_modes(&mut self, addr: usize, modes: &[i8; 16]) {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        for by in 0..4 {
            for bx in 0..4 {
                let gx = mb_x * 4 + bx;
                let gy = mb_y * 4 + by;
                self.pred_mode[gy * (self.pic_w_mbs * 4) + gx] = modes[by * 4 + bx];
            }
        }
    }

    fn consume_chroma(
        &mut self,
        reader: &mut BitReader,
        addr: usize,
        cbp_chroma: u8,
    ) -> Result<()> {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        if cbp_chroma == 0 {
            for comp in 0..2 {
                for by in 0..2 {
                    for bx in 0..2 {
                        let gx = mb_x * 2 + bx;
                        let gy = mb_y * 2 + by;
                        self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] = 0;
                    }
                }
            }
            return Ok(());
        }
        for _comp in 0..2 {
            let _dc = cavlc::residual_block(reader, 4, -1)?;
        }
        for comp in 0..2 {
            for by in 0..2 {
                for bx in 0..2 {
                    let gx = mb_x * 2 + bx;
                    let gy = mb_y * 2 + by;
                    if cbp_chroma == 2 {
                        let scan = by * 2 + bx;
                        let nc = self.chroma_nc(comp, gx, gy, addr, scan);
                        let blk = cavlc::residual_block(reader, 15, nc)?;
                        self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] =
                            u8::try_from(blk.total_coeff).expect("TotalCoeff 0..=16 fits u8");
                    } else {
                        self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] = 0;
                    }
                }
            }
        }
        Ok(())
    }

    fn crop(&self) -> LumaFrame {
        let (w, h) = self.sps.luma_dimensions();
        let (w, h) = (w as usize, h as usize);
        let (crop_x, crop_y) = self.crop_origin();
        let mut data = Vec::with_capacity(w * h);
        for y in 0..h {
            let base = self.idx(crop_x, crop_y + y);
            data.extend_from_slice(&self.luma[base..base + w]);
        }
        LumaFrame {
            width: w,
            height: h,
            data,
        }
    }

    fn crop_origin(&self) -> (usize, usize) {
        let crop_unit_x = 2usize;
        let crop_unit_y = 2usize;
        (
            crop_unit_x * self.sps.frame_crop_left_offset as usize,
            crop_unit_y * self.sps.frame_crop_top_offset as usize,
        )
    }
}

impl FrameDecoder<'_> {
    fn decode_slice_cabac(
        &mut self,
        rbsp: &[u8],
        start_bit: usize,
        sh: &SliceHeader,
    ) -> Result<()> {
        let mut eng = CabacDecoder::new(rbsp, start_bit, sh.slice_qp)?;
        self.qp_prev = sh.slice_qp;
        self.last_qscale_diff = 0;
        self.slice_disable_idc = u8::try_from(sh.disable_deblocking_filter_idc.min(2))
            .expect("disable_deblocking_filter_idc clamped to 0..=2");
        self.slice_alpha_off = sh.slice_alpha_c0_offset;
        self.slice_beta_off = sh.slice_beta_offset;
        let total = self.pic_w_mbs * self.pic_h_mbs;
        let mut addr = usize::try_from(sh.first_mb_in_slice).expect("first_mb_in_slice fits usize");
        loop {
            if addr >= total {
                return Err(Error::Parse(
                    "h264 cabac: macroblock address past frame".into(),
                ));
            }
            self.decode_macroblock_cabac(&mut eng, addr)?;
            self.decoded[addr] = true;
            if eng.decode_terminate() {
                break;
            }
            addr += 1;
        }
        self.cur_slice_id += 1;
        Ok(())
    }

    fn decode_macroblock_cabac(&mut self, eng: &mut CabacDecoder, addr: usize) -> Result<()> {
        let raw = self.decode_mb_type_cabac(eng, addr);
        let mb_type = classify_i_mb_type(raw)?;
        self.cbp_full[addr] = 0;
        self.mb_i16_or_pcm[addr] = matches!(mb_type, IMbType::I16x16 { .. } | IMbType::Pcm);
        self.mb_transform8x8[addr] = false;
        self.mb_chroma_pred[addr] = 0;
        match mb_type {
            IMbType::Pcm => {
                return Err(Error::Unsupported(
                    "h264 cabac: I_PCM not implemented (native CABAC falls back)".into(),
                ));
            }
            IMbType::NxN { .. } => {
                let transform_8x8 = if self.pps.transform_8x8_mode_flag {
                    let ctx = 399 + self.neighbour_transform_size(addr);
                    eng.decode_decision(ctx) == 1
                } else {
                    false
                };
                self.mb_transform8x8[addr] = transform_8x8;
                if transform_8x8 {
                    self.decode_8x8_cabac(eng, addr);
                } else {
                    self.decode_4x4_cabac(eng, addr);
                }
            }
            IMbType::I16x16 {
                pred_mode,
                cbp_luma,
                cbp_chroma,
            } => self.decode_16x16_cabac(eng, addr, pred_mode, cbp_luma, cbp_chroma),
        }
        Ok(())
    }

    fn decode_mb_type_cabac(&mut self, eng: &mut CabacDecoder, addr: usize) -> u32 {
        let mut ctx = 0usize;
        if let Some(l) = self.nb_mb(addr, -1, 0) {
            ctx += usize::from(self.mb_i16_or_pcm[l]);
        }
        if let Some(t) = self.nb_mb(addr, 0, -1) {
            ctx += usize::from(self.mb_i16_or_pcm[t]);
        }
        if eng.decode_decision(3 + ctx) == 0 {
            return 0;
        }
        if eng.decode_terminate() {
            return 25;
        }
        let mut t = 1u32;
        t += 12 * eng.decode_decision(6);
        if eng.decode_decision(7) == 1 {
            t += 4 + 4 * eng.decode_decision(8);
        }
        t += 2 * eng.decode_decision(9);
        t += eng.decode_decision(10);
        t
    }

    fn decode_16x16_cabac(
        &mut self,
        eng: &mut CabacDecoder,
        addr: usize,
        pred_mode: u8,
        cbp_luma: u8,
        cbp_chroma: u8,
    ) {
        self.mb_chroma_pred[addr] = self.decode_chroma_pred_mode_cabac(eng, addr);
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        let (ox, oy) = (mb_x * 16, mb_y * 16);

        self.set_mb_pred_modes(addr, &[-1; 16]);
        self.predict_16x16_into(addr, pred_mode);

        let qp = self.read_qp_cabac(eng);
        self.record_mb(addr, qp, false);

        let mut dc_scan = [0i32; 16];
        if eng.decode_decision(self.luma_dc_cbf_ctx(addr)) == 1 {
            eng.decode_residual(&ResidualCat::LUMA_DC, &mut dc_scan);
            self.cbp_full[addr] |= 0x100;
        }
        let dc_values = transform::luma_dc_transform(&dc_scan, qp);

        let luma_cbp_set = cbp_luma != 0;
        for (scan, &(bx, by)) in BLK_POS.iter().enumerate() {
            let gx = mb_x * 4 + bx;
            let gy = mb_y * 4 + by;
            let ac = if luma_cbp_set {
                let ctx = self.luma_ac_cbf_ctx(85 + 4, gx, gy, addr, scan);
                if eng.decode_decision(ctx) == 1 {
                    let mut ac = [0i32; 16];
                    let count = eng.decode_residual(&ResidualCat::LUMA_AC, &mut ac[..15]);
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] =
                        u8::try_from(count).expect("coeff_count fits u8");
                    ac
                } else {
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
                    [0; 16]
                }
            } else {
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
                [0; 16]
            };
            let dc = dc_values[by * 4 + bx];
            self.add_ac_residual(ox + bx * 4, oy + by * 4, &ac, dc, qp);
        }
        if luma_cbp_set {
            self.cbp_full[addr] |= 0x0F;
        }
        self.consume_chroma_cabac(eng, addr, cbp_chroma);
    }

    fn decode_4x4_cabac(&mut self, eng: &mut CabacDecoder, addr: usize) {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);

        let mut modes = [0u8; 16];
        for (scan, mode_slot) in modes.iter_mut().enumerate() {
            let (bx, by) = BLK_POS[scan];
            let pred = self.predicted_intra_mode(mb_x * 4 + bx, mb_y * 4 + by, addr);
            *mode_slot = Self::read_intra4x4_mode_cabac(eng, pred);
            self.pred_mode[(mb_y * 4 + by) * (self.pic_w_mbs * 4) + (mb_x * 4 + bx)] =
                i8::try_from(*mode_slot).expect("intra mode 0..=8 fits i8");
        }
        self.mb_chroma_pred[addr] = self.decode_chroma_pred_mode_cabac(eng, addr);

        let cbp = self.decode_cbp_cabac(eng, addr);
        let cbp_luma = cbp & 0x0F;
        let cbp_chroma = cbp >> 4;

        let qp = if cbp_luma != 0 || cbp_chroma != 0 {
            self.read_qp_cabac(eng)
        } else {
            self.last_qscale_diff = 0;
            self.qp_prev
        };
        self.record_mb(addr, qp, false);

        for scan in 0..16 {
            let (bx, by) = BLK_POS[scan];
            let gx = mb_x * 4 + bx;
            let gy = mb_y * 4 + by;
            let (ax, ay) = (gx * 4, gy * 4);
            self.predict_4x4_into(addr, scan, ax, ay, modes[scan]);

            if cbp_luma & (1 << (scan / 4)) != 0 {
                let ctx = self.luma_ac_cbf_ctx(85 + 8, gx, gy, addr, scan);
                if eng.decode_decision(ctx) == 1 {
                    let mut coeffs = [0i32; 16];
                    let count = eng.decode_residual(&ResidualCat::LUMA_4X4, &mut coeffs);
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] =
                        u8::try_from(count).expect("coeff_count fits u8");
                    self.add_residual_4x4(ax, ay, &coeffs, qp);
                } else {
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
                }
            } else {
                self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
            }
        }

        self.cbp_full[addr] |= u16::from(cbp_luma);
        self.consume_chroma_cabac(eng, addr, cbp_chroma);
    }

    fn decode_8x8_cabac(&mut self, eng: &mut CabacDecoder, addr: usize) {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);

        let mut modes = [0u8; 4];
        for (blk8, mode_slot) in modes.iter_mut().enumerate() {
            let (b8x, b8y) = (blk8 % 2, blk8 / 2);
            let pred = self.predicted_intra_mode(mb_x * 4 + b8x * 2, mb_y * 4 + b8y * 2, addr);
            let mode = Self::read_intra4x4_mode_cabac(eng, pred);
            *mode_slot = mode;
            for dy in 0..2 {
                for dx in 0..2 {
                    let gx = mb_x * 4 + b8x * 2 + dx;
                    let gy = mb_y * 4 + b8y * 2 + dy;
                    self.pred_mode[gy * (self.pic_w_mbs * 4) + gx] =
                        i8::try_from(mode).expect("intra mode 0..=8 fits i8");
                }
            }
        }
        self.mb_chroma_pred[addr] = self.decode_chroma_pred_mode_cabac(eng, addr);

        let cbp = self.decode_cbp_cabac(eng, addr);
        let cbp_luma = cbp & 0x0F;
        let cbp_chroma = cbp >> 4;

        let qp = if cbp_luma != 0 || cbp_chroma != 0 {
            self.read_qp_cabac(eng)
        } else {
            self.last_qscale_diff = 0;
            self.qp_prev
        };
        self.record_mb(addr, qp, true);

        for (blk8, &mode) in modes.iter().enumerate() {
            let (b8x, b8y) = (blk8 % 2, blk8 / 2);
            let (ax, ay) = ((mb_x * 16) + b8x * 8, (mb_y * 16) + b8y * 8);
            self.predict_8x8_into(addr, blk8, ax, ay, mode);

            if cbp_luma & (1 << blk8) != 0 {
                let mut coeffs = [0i32; 64];
                let count = eng.decode_residual(&ResidualCat::LUMA_8X8, &mut coeffs);
                let nnz = u8::try_from(count).expect("coeff_count fits u8");
                for i4 in 0..4 {
                    let gx = mb_x * 4 + b8x * 2 + (i4 % 2);
                    let gy = mb_y * 4 + b8y * 2 + (i4 / 2);
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = nnz;
                }
                self.add_residual_8x8(ax, ay, &coeffs, qp);
            } else {
                for i4 in 0..4 {
                    let gx = mb_x * 4 + b8x * 2 + (i4 % 2);
                    let gy = mb_y * 4 + b8y * 2 + (i4 / 2);
                    self.luma_nnz[gy * (self.pic_w_mbs * 4) + gx] = 0;
                }
            }
        }

        self.cbp_full[addr] |= u16::from(cbp_luma);
        self.consume_chroma_cabac(eng, addr, cbp_chroma);
    }

    fn consume_chroma_cabac(&mut self, eng: &mut CabacDecoder, addr: usize, cbp_chroma: u8) {
        let (mb_x, mb_y) = (addr % self.pic_w_mbs, addr / self.pic_w_mbs);
        let clear_chroma_nnz = |dec: &mut Self| {
            for comp in 0..2 {
                for by in 0..2 {
                    for bx in 0..2 {
                        let gx = mb_x * 2 + bx;
                        let gy = mb_y * 2 + by;
                        dec.chroma_nnz[comp][gy * (dec.pic_w_mbs * 2) + gx] = 0;
                    }
                }
            }
        };
        if cbp_chroma == 0 {
            clear_chroma_nnz(self);
            return;
        }
        self.cbp_full[addr] |= u16::from(cbp_chroma) << 4;

        for comp in 0..2 {
            let ctx = self.chroma_dc_cbf_ctx(addr, comp);
            if eng.decode_decision(ctx) == 1 {
                let mut dc = [0i32; 4];
                eng.decode_residual(&ResidualCat::CHROMA_DC, &mut dc);
                self.cbp_full[addr] |= 0x40 << comp;
            }
        }
        for comp in 0..2 {
            for by in 0..2 {
                for bx in 0..2 {
                    let gx = mb_x * 2 + bx;
                    let gy = mb_y * 2 + by;
                    if cbp_chroma == 2 {
                        let scan = by * 2 + bx;
                        let ctx = self.chroma_ac_cbf_ctx(comp, gx, gy, addr, scan);
                        if eng.decode_decision(ctx) == 1 {
                            let mut ac = [0i32; 16];
                            let count = eng.decode_residual(&ResidualCat::CHROMA_AC, &mut ac[..15]);
                            self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] =
                                u8::try_from(count).expect("coeff_count fits u8");
                        } else {
                            self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] = 0;
                        }
                    } else {
                        self.chroma_nnz[comp][gy * (self.pic_w_mbs * 2) + gx] = 0;
                    }
                }
            }
        }
    }

    fn read_qp_cabac(&mut self, eng: &mut CabacDecoder) -> i32 {
        let delta = if eng.decode_decision(60 + usize::from(self.last_qscale_diff != 0)) == 1 {
            let mut val = 1u32;
            let mut ctx = 2usize;
            while eng.decode_decision(60 + ctx) == 1 {
                ctx = 3;
                val += 1;
                if val > 256 {
                    break;
                }
            }
            if val & 1 == 1 {
                i32::try_from((val + 1) >> 1).expect("qp delta fits i32")
            } else {
                -i32::try_from((val + 1) >> 1).expect("qp delta fits i32")
            }
        } else {
            0
        };
        self.last_qscale_diff = delta;
        let qp = (self.qp_prev + delta + 52).rem_euclid(52);
        self.qp_prev = qp;
        qp
    }

    fn read_intra4x4_mode_cabac(eng: &mut CabacDecoder, predicted: u8) -> u8 {
        if eng.decode_decision(68) == 1 {
            return predicted;
        }
        let mut rem = eng.decode_decision(69);
        rem |= eng.decode_decision(69) << 1;
        rem |= eng.decode_decision(69) << 2;
        let rem = u8::try_from(rem).expect("rem 0..=7 fits u8");
        if rem < predicted { rem } else { rem + 1 }
    }

    fn decode_chroma_pred_mode_cabac(&self, eng: &mut CabacDecoder, addr: usize) -> u8 {
        let mut ctx = 0usize;
        if let Some(l) = self.nb_mb(addr, -1, 0) {
            ctx += usize::from(self.mb_chroma_pred[l] != 0);
        }
        if let Some(t) = self.nb_mb(addr, 0, -1) {
            ctx += usize::from(self.mb_chroma_pred[t] != 0);
        }
        if eng.decode_decision(64 + ctx) == 0 {
            return 0;
        }
        if eng.decode_decision(64 + 3) == 0 {
            return 1;
        }
        if eng.decode_decision(64 + 3) == 0 {
            return 2;
        }
        3
    }

    fn decode_cbp_cabac(&self, eng: &mut CabacDecoder, addr: usize) -> u8 {
        let cbp_a = self.nb_mb(addr, -1, 0).map_or(0x7CF, |l| self.cbp_full[l]);
        let cbp_b = self.nb_mb(addr, 0, -1).map_or(0x7CF, |t| self.cbp_full[t]);

        let mut cbp = 0u16;
        let ctx = usize::from(cbp_a & 0x02 == 0) + 2 * usize::from(cbp_b & 0x04 == 0);
        cbp |= u16::from(eng.decode_decision(73 + ctx) == 1);
        let ctx = usize::from(cbp & 0x01 == 0) + 2 * usize::from(cbp_b & 0x08 == 0);
        cbp |= u16::from(eng.decode_decision(73 + ctx) == 1) << 1;
        let ctx = usize::from(cbp_a & 0x08 == 0) + 2 * usize::from(cbp & 0x01 == 0);
        cbp |= u16::from(eng.decode_decision(73 + ctx) == 1) << 2;
        let ctx = usize::from(cbp & 0x04 == 0) + 2 * usize::from(cbp & 0x02 == 0);
        cbp |= u16::from(eng.decode_decision(73 + ctx) == 1) << 3;

        let chroma_left = (cbp_a >> 4) & 0x03;
        let chroma_top = (cbp_b >> 4) & 0x03;
        let ctx = usize::from(chroma_left > 0) + 2 * usize::from(chroma_top > 0);
        if eng.decode_decision(77 + ctx) == 1 {
            let ctx = 4 + usize::from(chroma_left == 2) + 2 * usize::from(chroma_top == 2);
            let suffix = 1 + eng.decode_decision(77 + ctx);
            cbp |= u16::try_from(suffix).expect("0..=2 fits u16") << 4;
        }
        u8::try_from(cbp).expect("cbp 0..=0x2F fits u8")
    }

    fn neighbour_transform_size(&self, addr: usize) -> usize {
        let mut n = 0;
        if let Some(l) = self.nb_mb(addr, -1, 0) {
            n += usize::from(self.mb_transform8x8[l]);
        }
        if let Some(t) = self.nb_mb(addr, 0, -1) {
            n += usize::from(self.mb_transform8x8[t]);
        }
        n
    }

    fn luma_ac_cbf_ctx(
        &self,
        base: usize,
        gx: usize,
        gy: usize,
        addr: usize,
        scan: usize,
    ) -> usize {
        let gx = isize::try_from(gx).expect("gx fits isize");
        let gy = isize::try_from(gy).expect("gy fits isize");
        let nza = self.luma_cbf_neighbour(gx - 1, gy, addr, scan);
        let nzb = self.luma_cbf_neighbour(gx, gy - 1, addr, scan);
        base + usize::from(nza > 0) + 2 * usize::from(nzb > 0)
    }

    fn luma_cbf_neighbour(&self, nx: isize, ny: isize, addr: usize, scan: usize) -> u8 {
        if nx < 0 || ny < 0 {
            return 64;
        }
        let gw = self.pic_w_mbs * 4;
        let gh = self.pic_h_mbs * 4;
        let (nx, ny) = (
            usize::try_from(nx).expect("nx non-negative after guard"),
            usize::try_from(ny).expect("ny non-negative after guard"),
        );
        if nx >= gw || ny >= gh {
            return 64;
        }
        let nb = (ny / 4) * self.pic_w_mbs + (nx / 4);
        let avail = if nb == addr {
            BLK_INV[(ny % 4) * 4 + (nx % 4)] < scan
        } else {
            self.decoded[nb]
        };
        if avail {
            self.luma_nnz[ny * gw + nx]
        } else {
            64
        }
    }

    fn luma_dc_cbf_ctx(&self, addr: usize) -> usize {
        let a = self.nb_mb(addr, -1, 0).map_or(0x7CF, |l| self.cbp_full[l]);
        let b = self.nb_mb(addr, 0, -1).map_or(0x7CF, |t| self.cbp_full[t]);
        85 + usize::from(a & 0x100 != 0) + 2 * usize::from(b & 0x100 != 0)
    }

    fn chroma_dc_cbf_ctx(&self, addr: usize, comp: usize) -> usize {
        let a = self.nb_mb(addr, -1, 0).map_or(0x7CF, |l| self.cbp_full[l]);
        let b = self.nb_mb(addr, 0, -1).map_or(0x7CF, |t| self.cbp_full[t]);
        let bit = 6 + comp;
        97 + usize::from((a >> bit) & 1 != 0) + 2 * usize::from((b >> bit) & 1 != 0)
    }

    fn chroma_ac_cbf_ctx(
        &self,
        comp: usize,
        gx: usize,
        gy: usize,
        addr: usize,
        scan: usize,
    ) -> usize {
        let gx = isize::try_from(gx).expect("gx fits isize");
        let gy = isize::try_from(gy).expect("gy fits isize");
        let nza = self.chroma_cbf_neighbour(comp, gx - 1, gy, addr, scan);
        let nzb = self.chroma_cbf_neighbour(comp, gx, gy - 1, addr, scan);
        101 + usize::from(nza > 0) + 2 * usize::from(nzb > 0)
    }

    fn chroma_cbf_neighbour(
        &self,
        comp: usize,
        nx: isize,
        ny: isize,
        addr: usize,
        scan: usize,
    ) -> u8 {
        if nx < 0 || ny < 0 {
            return 64;
        }
        let gw = self.pic_w_mbs * 2;
        let gh = self.pic_h_mbs * 2;
        let (nx, ny) = (
            usize::try_from(nx).expect("nx non-negative after guard"),
            usize::try_from(ny).expect("ny non-negative after guard"),
        );
        if nx >= gw || ny >= gh {
            return 64;
        }
        let nb = (ny / 2) * self.pic_w_mbs + (nx / 2);
        let avail = if nb == addr {
            ((ny % 2) * 2 + (nx % 2)) < scan
        } else {
            self.decoded[nb]
        };
        if avail {
            self.chroma_nnz[comp][ny * gw + nx]
        } else {
            64
        }
    }

    fn nb_mb(&self, addr: usize, off_x: isize, off_y: isize) -> Option<usize> {
        let x = isize::try_from(addr % self.pic_w_mbs).expect("mb col fits isize") + off_x;
        let y = isize::try_from(addr / self.pic_w_mbs).expect("mb row fits isize") + off_y;
        let pic_w = isize::try_from(self.pic_w_mbs).expect("pic_w fits isize");
        let pic_h = isize::try_from(self.pic_h_mbs).expect("pic_h fits isize");
        if x < 0 || y < 0 || x >= pic_w || y >= pic_h {
            return None;
        }
        let nb = usize::try_from(y).expect("y non-negative") * self.pic_w_mbs
            + usize::try_from(x).expect("x non-negative");
        self.decoded[nb].then_some(nb)
    }
}

fn classify_i_mb_type(raw: u32) -> Result<IMbType> {
    match raw {
        0 => Ok(IMbType::NxN {
            transform_8x8: false,
        }),
        25 => Ok(IMbType::Pcm),
        1..=24 => {
            let n = raw - 1;
            let pred_mode = u8::try_from(n % 4).expect("0..4");
            let group = n / 4;
            let cbp_chroma = u8::try_from(group % 3).expect("0..3");
            let cbp_luma = if group / 3 == 1 { 15 } else { 0 };
            Ok(IMbType::I16x16 {
                pred_mode,
                cbp_luma,
                cbp_chroma,
            })
        }
        other => Err(Error::Unsupported(format!(
            "h264: mb_type {other} is not an I-slice macroblock type"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::bitstream::test_support::BitWriter;
    use super::super::nal::NalUnitType;
    use super::*;

    fn sps_1mb() -> Sps {
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
            pic_width_in_mbs: 1,
            pic_height_in_map_units: 1,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
        }
    }

    fn pps_cavlc() -> Pps {
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
            deblocking_filter_control_present_flag: false,
            constrained_intra_pred_flag: false,
            redundant_pic_cnt_present_flag: false,
            transform_8x8_mode_flag: false,
        }
    }

    fn idr_nal() -> NalHeader {
        NalHeader {
            ref_idc: 3,
            unit_type: NalUnitType::IdrSlice,
        }
    }

    fn write_i_slice_header(w: &mut BitWriter) {
        w.ue(0);
        w.ue(2);
        w.ue(0);
        w.bits(0, 4);
        w.ue(0);
        w.bits(0, 4);
        w.flag(false);
        w.flag(false);
        w.se(0);
    }

    fn decode_one(rbsp: &[u8]) -> LumaFrame {
        let sps = sps_1mb();
        let pps = pps_cavlc();
        let nal = idr_nal();
        decode_intra_frame(&sps, &pps, &[(&nal, rbsp)]).expect("frame decodes")
    }

    #[test]
    fn oversized_frame_dimensions_route_to_fallback() {
        let sps = Sps {
            pic_width_in_mbs: 4096,
            pic_height_in_map_units: 4096,
            ..sps_1mb()
        };
        let pps = pps_cavlc();
        let nal = idr_nal();
        let err = decode_intra_frame(&sps, &pps, &[(&nal, &[0u8][..])])
            .expect_err("oversized dimensions must be rejected");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn first_mb_in_slice_past_frame_is_rejected() {
        let mut w = BitWriter::new();
        w.ue(5);
        w.ue(2);
        w.ue(0);
        w.bits(0, 4);
        w.ue(0);
        w.bits(0, 4);
        w.flag(false);
        w.flag(false);
        w.se(0);
        let rbsp = w.into_rbsp();

        let sps = sps_1mb();
        let pps = pps_cavlc();
        let nal = idr_nal();
        let err = decode_intra_frame(&sps, &pps, &[(&nal, &rbsp)])
            .expect_err("out-of-range first_mb_in_slice must be rejected");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }

    fn sps_2mb() -> Sps {
        Sps {
            pic_width_in_mbs: 2,
            ..sps_1mb()
        }
    }

    #[test]
    fn i16x16_horizontal_predicts_from_left_neighbour() {
        let mut w = BitWriter::new();
        write_i_slice_header(&mut w);
        w.ue(3);
        w.ue(0);
        w.se(0);
        w.bits(0b01, 2);
        w.bits(0, 1);
        w.bits(0b1, 1);
        w.ue(2);
        w.ue(0);
        w.se(0);
        w.bits(0b1, 1);
        let rbsp = w.into_rbsp();

        let sps = sps_2mb();
        let pps = pps_cavlc();
        let nal = idr_nal();
        let frame = decode_intra_frame(&sps, &pps, &[(&nal, &rbsp)]).expect("two-MB frame decodes");
        assert_eq!((frame.width, frame.height), (32, 16));
        assert!(
            frame.data.iter().all(|&p| p == 129),
            "MB1 horizontal-predicts MB0's 129 column across the whole frame"
        );
    }

    #[test]
    fn i16x16_dc_no_residual_is_flat_128() {
        let mut w = BitWriter::new();
        write_i_slice_header(&mut w);
        w.ue(3);
        w.ue(0);
        w.se(0);
        w.bits(0b1, 1);
        let rbsp = w.into_rbsp();

        let frame = decode_one(&rbsp);
        assert_eq!((frame.width, frame.height), (16, 16));
        assert!(frame.data.iter().all(|&p| p == 128), "all DC-predicted 128");
    }

    #[test]
    fn i16x16_dc_single_coefficient_adds_uniform_offset() {
        let mut w = BitWriter::new();
        write_i_slice_header(&mut w);
        w.ue(3);
        w.ue(0);
        w.se(0);
        w.bits(0b01, 2);
        w.bits(0, 1);
        w.bits(0b1, 1);
        let rbsp = w.into_rbsp();

        let frame = decode_one(&rbsp);
        assert!(
            frame.data.iter().all(|&p| p == 129),
            "uniform +1 residual on 128 prediction"
        );
    }

    #[test]
    fn i4x4_all_dc_no_residual_is_flat_128() {
        let mut w = BitWriter::new();
        write_i_slice_header(&mut w);
        w.ue(0);
        for _ in 0..16 {
            w.flag(true);
        }
        w.ue(0);
        w.ue(3);
        let rbsp = w.into_rbsp();

        let frame = decode_one(&rbsp);
        assert!(frame.data.iter().all(|&p| p == 128), "flat DC 128");
    }

    #[test]
    fn i_pcm_passes_raw_samples_through() {
        let mut w = BitWriter::new();
        write_i_slice_header(&mut w);
        w.ue(25);
        while w_len_bits(&w) % 8 != 0 {
            w.bit(0);
        }
        for i in 0..256u32 {
            w.bits(i % 256, 8);
        }
        for _ in 0..128 {
            w.bits(0, 8);
        }
        let rbsp = w.into_rbsp();

        let frame = decode_one(&rbsp);
        for x in 0..16usize {
            assert_eq!(
                frame.data[x],
                u8::try_from(x).expect("x in 0..16"),
                "pcm luma ramp row 0"
            );
        }
        assert_eq!(frame.data[16], 16, "pcm luma sample (0,1)");
    }

    fn w_len_bits(w: &BitWriter) -> usize {
        w.bit_len()
    }

    #[test]
    fn routes_cabac_to_native_decode() {
        let sps = sps_1mb();
        let mut pps = pps_cavlc();
        pps.entropy_coding_mode_flag = true;
        let nal = idr_nal();
        let err = decode_intra_frame(&sps, &pps, &[(&nal, &[0x80])]).expect_err("truncated slice");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }

    #[test]
    fn classify_i16x16_packing() {
        assert_eq!(
            classify_i_mb_type(1).unwrap(),
            IMbType::I16x16 {
                pred_mode: 0,
                cbp_luma: 0,
                cbp_chroma: 0
            }
        );
        assert_eq!(
            classify_i_mb_type(13).unwrap(),
            IMbType::I16x16 {
                pred_mode: 0,
                cbp_luma: 15,
                cbp_chroma: 0
            }
        );
        assert!(matches!(
            classify_i_mb_type(0).unwrap(),
            IMbType::NxN { .. }
        ));
        assert!(matches!(classify_i_mb_type(25).unwrap(), IMbType::Pcm));
    }
}
