use vidcull_core::{Error, Result};

#[rustfmt::skip]
pub(crate) static RANGE_TAB_LPS: [[u16; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [ 95, 116, 137, 158], [ 90, 110, 130, 150], [ 85, 104, 123, 142], [ 81,  99, 117, 135],
    [ 77,  94, 111, 128], [ 73,  89, 105, 122], [ 69,  85, 100, 116], [ 66,  80,  95, 110],
    [ 62,  76,  90, 104], [ 59,  72,  86,  99], [ 56,  69,  81,  94], [ 53,  65,  77,  89],
    [ 51,  62,  73,  85], [ 48,  59,  69,  80], [ 46,  56,  66,  76], [ 43,  53,  63,  72],
    [ 41,  50,  59,  69], [ 39,  48,  56,  65], [ 37,  45,  54,  62], [ 35,  43,  51,  59],
    [ 33,  41,  48,  56], [ 32,  39,  46,  53], [ 30,  37,  43,  50], [ 29,  35,  41,  48],
    [ 27,  33,  39,  45], [ 26,  31,  37,  43], [ 24,  30,  35,  41], [ 23,  28,  33,  39],
    [ 22,  27,  32,  37], [ 21,  26,  30,  35], [ 20,  24,  29,  33], [ 19,  23,  27,  31],
    [ 18,  22,  26,  30], [ 17,  21,  25,  28], [ 16,  20,  23,  27], [ 15,  19,  22,  25],
    [ 14,  18,  21,  24], [ 14,  17,  20,  23], [ 13,  16,  19,  22], [ 12,  15,  18,  21],
    [ 12,  14,  17,  20], [ 11,  14,  16,  19], [ 11,  13,  15,  18], [ 10,  12,  15,  17],
    [ 10,  12,  14,  16], [  9,  11,  13,  15], [  9,  11,  12,  14], [  8,  10,  12,  14],
    [  8,   9,  11,  13], [  7,   9,  11,  12], [  7,   9,  10,  12], [  7,   8,  10,  11],
    [  6,   8,   9,  11], [  6,   7,   9,  10], [  6,   7,   8,   9], [  2,   2,   2,   2],
];

#[rustfmt::skip]
pub(crate) static TRANS_IDX_LPS: [u8; 64] = [
     0,  0,  1,  2,  2,  4,  4,  5,  6,  7,  8,  9,  9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

#[rustfmt::skip]
pub(crate) static TRANS_IDX_MPS: [u8; 64] = [
     1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11, 12, 13, 14, 15, 16,
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
    33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
    49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

#[rustfmt::skip]
static SIG_8X8_FRAME: [u8; 63] = [
     0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5,
     4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9,10, 9, 8, 7,
     7, 6,11,12,13,11, 6, 7, 8, 9,14,10, 9, 8, 6,11,
    12,13,11, 6, 9,14,10, 9,11,12,13,11,14,10,12,
];

#[rustfmt::skip]
static LAST_8X8_FRAME: [u8; 63] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8,
];

static COEFF_ABS_LEVEL1_CTX: [usize; 8] = [1, 2, 3, 4, 0, 0, 0, 0];
static COEFF_ABS_LEVELGT1_CTX: [[usize; 8]; 2] =
    [[5, 5, 5, 5, 6, 7, 8, 9], [5, 5, 5, 5, 6, 7, 8, 8]];
static COEFF_ABS_TRANS: [[usize; 8]; 2] = [[1, 2, 3, 3, 4, 5, 6, 7], [4, 4, 4, 4, 5, 6, 7, 7]];

#[rustfmt::skip]
static INIT_I: [(i32, i32); 460] = [
    (20, -15), (2, 54), (3, 74), (20, -15), (2, 54), (3, 74), (-28, 127), (-23, 104),
    (-6, 53), (-1, 54), (7, 51),
    (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0),
    (0, 0), (0, 0),
    (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0),
    (0, 0), (0, 0), (0, 0), (0, 0), (0, 0),
    (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0),
    (0, 0), (0, 0), (0, 0),
    (0, 0), (0, 0), (0, 0), (0, 0), (0, 0), (0, 0),
    (0, 41), (0, 63), (0, 63), (0, 63), (-9, 83), (4, 86), (0, 97), (-7, 72), (13, 41), (3, 62),
    (0, 11), (1, 55), (0, 69), (-17, 127), (-13, 102), (0, 82), (-7, 74), (-21, 107), (-27, 127),
    (-31, 127), (-24, 127), (-18, 95), (-27, 127), (-21, 114), (-30, 127), (-17, 123), (-12, 115),
    (-16, 122),
    (-11, 115), (-12, 63), (-2, 68), (-15, 84), (-13, 104), (-3, 70), (-8, 93), (-10, 90),
    (-30, 127), (-1, 74), (-6, 97), (-7, 91), (-20, 127), (-4, 56), (-5, 82), (-7, 76), (-22, 125),
    (-7, 93), (-11, 87), (-3, 77), (-5, 71), (-4, 63), (-4, 68), (-12, 84), (-7, 62), (-7, 65),
    (8, 61), (5, 56), (-2, 66), (1, 64), (0, 61), (-2, 78), (1, 50), (7, 52), (10, 35), (0, 44),
    (11, 38), (1, 45), (0, 46), (5, 44), (31, 17), (1, 51), (7, 50), (28, 19), (16, 33), (14, 62),
    (-13, 108), (-15, 100),
    (-13, 101), (-13, 91), (-12, 94), (-10, 88), (-16, 84), (-10, 86), (-7, 83), (-13, 87),
    (-19, 94), (1, 70), (0, 72), (-5, 74), (18, 59), (-8, 102), (-15, 100), (0, 95), (-4, 75),
    (2, 72), (-11, 75), (-3, 71), (15, 46), (-13, 69), (0, 62), (0, 65), (21, 37), (-15, 72),
    (9, 57), (16, 54), (0, 62), (12, 72),
    (24, 0), (15, 9), (8, 25), (13, 18), (15, 9), (13, 19), (10, 37), (12, 18), (6, 29), (20, 33),
    (15, 30), (4, 45), (1, 58), (0, 62), (7, 61), (12, 38), (11, 45), (15, 39), (11, 42), (13, 44),
    (16, 45), (12, 41), (10, 49), (30, 34), (18, 42), (10, 55), (17, 51), (17, 46), (0, 89),
    (26, -19), (22, -17),
    (26, -17), (30, -25), (28, -20), (33, -23), (37, -27), (33, -23), (40, -28), (38, -17),
    (33, -11), (40, -15), (41, -6), (38, 1), (41, 17), (30, -6), (27, 3), (26, 22), (37, -16),
    (35, -4), (38, -8), (38, -3), (37, 3), (38, 5), (42, 0), (35, 16), (39, 22), (14, 48),
    (27, 37), (21, 60), (12, 68), (2, 97),
    (-3, 71), (-6, 42), (-5, 50), (-3, 54), (-2, 62), (0, 58), (1, 63), (-2, 72), (-1, 74),
    (-9, 91), (-5, 67), (-5, 27), (-3, 39), (-2, 44), (0, 46), (-16, 64), (-8, 68), (-10, 78),
    (-6, 77), (-10, 86), (-12, 92), (-15, 55), (-10, 60), (-6, 62), (-4, 65),
    (-12, 73), (-8, 76), (-7, 80), (-9, 88), (-17, 110), (-11, 97), (-20, 84), (-11, 79), (-6, 73),
    (-4, 74), (-13, 86), (-13, 96), (-11, 97), (-19, 117), (-8, 78), (-5, 33), (-4, 48), (-2, 53),
    (-3, 62), (-13, 71), (-10, 79), (-12, 86), (-13, 90), (-14, 97),
    (0, 0),
    (-6, 93), (-6, 84), (-8, 79), (0, 66), (-1, 71), (0, 62), (-2, 60), (-2, 59), (-5, 75),
    (-3, 62), (-4, 58), (-9, 66), (-1, 79), (0, 71), (3, 68), (10, 44), (-7, 62), (15, 36),
    (14, 40), (16, 27), (12, 29), (1, 44), (20, 36), (18, 32), (5, 42), (1, 48), (10, 62),
    (17, 46), (9, 64), (-12, 104), (-11, 97),
    (-16, 96), (-7, 88), (-8, 85), (-7, 85), (-9, 85), (-13, 88), (4, 66), (-3, 77), (-3, 76),
    (-6, 76), (10, 58), (-1, 76), (-1, 83), (-7, 99), (-14, 95), (2, 95), (0, 76), (-5, 74),
    (0, 70), (-11, 75), (1, 68), (0, 65), (-14, 73), (3, 62), (4, 62), (-1, 68), (-13, 75),
    (11, 55), (5, 64), (12, 70),
    (15, 6), (6, 19), (7, 16), (12, 14), (18, 13), (13, 11), (13, 15), (15, 16), (12, 23),
    (13, 23), (15, 20), (14, 26), (14, 44), (17, 40), (17, 47), (24, 17), (21, 21), (25, 22),
    (31, 27), (22, 29), (19, 35), (14, 50), (10, 57), (7, 63), (-2, 77), (-4, 82), (-3, 94),
    (9, 69), (-12, 109), (36, -35), (36, -34),
    (32, -26), (37, -30), (44, -32), (34, -18), (34, -15), (40, -15), (33, -7), (35, -5), (33, 0),
    (38, 2), (33, 13), (23, 35), (13, 58), (29, -3), (26, 0), (22, 30), (31, -7), (35, -15),
    (34, -3), (34, 3), (36, -1), (34, 5), (32, 11), (35, 5), (34, 12), (39, 11), (30, 29),
    (34, 26), (29, 39), (19, 66),
    (31, 21), (31, 31), (25, 50), (-17, 120), (-20, 112), (-18, 114), (-11, 85), (-15, 92),
    (-14, 89), (-26, 71), (-15, 81), (-14, 80), (0, 68), (-14, 70), (-24, 56), (-23, 68),
    (-24, 50), (-11, 74), (23, -13), (26, -13), (40, -15), (49, -14), (44, 3), (45, 6), (44, 34),
    (33, 54), (19, 82), (-3, 75), (-1, 23), (1, 34), (1, 43), (0, 54), (-2, 55), (0, 61), (1, 64),
    (0, 68), (-9, 92),
    (-14, 106), (-13, 97), (-15, 90), (-12, 90), (-18, 88), (-10, 73), (-9, 79), (-14, 86),
    (-10, 73), (-10, 70), (-10, 69), (-5, 66), (-9, 64), (-5, 58), (2, 59), (21, -10), (24, -11),
    (28, -8), (28, -1), (29, 3), (29, 9), (35, 20), (29, 36), (14, 67),
];

const NUM_CTX: usize = 460;

#[derive(Debug, Clone, Copy)]
pub struct ResidualCat {
    sig_base: usize,
    last_base: usize,
    abs_base: usize,
    max_coeff: usize,
    is_8x8: bool,
}

impl ResidualCat {
    pub const LUMA_DC: Self = Self::new(0, 16, false);
    pub const LUMA_AC: Self = Self::new(1, 15, false);
    pub const LUMA_4X4: Self = Self::new(2, 16, false);
    pub const CHROMA_DC: Self = Self::new(3, 4, false);
    pub const CHROMA_AC: Self = Self::new(4, 15, false);
    pub const LUMA_8X8: Self = Self::new(5, 64, true);

    const fn new(cat: usize, max_coeff: usize, is_8x8: bool) -> Self {
        const SIG: [usize; 6] = [105, 120, 134, 149, 152, 402];
        const LAST: [usize; 6] = [166, 181, 195, 210, 213, 417];
        const ABS: [usize; 6] = [227, 237, 247, 257, 266, 426];
        Self {
            sig_base: SIG[cat],
            last_base: LAST[cat],
            abs_base: ABS[cat],
            max_coeff,
            is_8x8,
        }
    }
}

pub struct CabacDecoder<'a> {
    data: &'a [u8],
    bit_pos: usize,
    range: u32,
    offset: u32,
    state: [u8; NUM_CTX],
    mps: [u8; NUM_CTX],
}

impl<'a> CabacDecoder<'a> {
    pub fn new(rbsp: &'a [u8], start_bit: usize, slice_qp: i32) -> Result<Self> {
        let aligned = start_bit.div_ceil(8) * 8;
        if aligned + 9 > rbsp.len() * 8 {
            return Err(Error::Parse(
                "h264 cabac: bitstream too short for arithmetic init".into(),
            ));
        }
        let mut dec = Self {
            data: rbsp,
            bit_pos: aligned,
            range: 510,
            offset: 0,
            state: [0; NUM_CTX],
            mps: [0; NUM_CTX],
        };
        for _ in 0..9 {
            dec.offset = (dec.offset << 1) | dec.read_bit();
        }
        dec.init_contexts(slice_qp);
        Ok(dec)
    }

    fn init_contexts(&mut self, slice_qp: i32) {
        let qp = slice_qp.clamp(0, 51);
        for (idx, &(m, n)) in INIT_I.iter().enumerate() {
            let pre = (((m * qp) >> 4) + n).clamp(1, 126);
            if pre <= 63 {
                self.state[idx] = u8::try_from(63 - pre).expect("0..=62 fits u8");
                self.mps[idx] = 0;
            } else {
                self.state[idx] = u8::try_from(pre - 64).expect("0..=62 fits u8");
                self.mps[idx] = 1;
            }
        }
    }

    #[inline]
    fn read_bit(&mut self) -> u32 {
        let byte_idx = self.bit_pos / 8;
        let bit = if byte_idx < self.data.len() {
            let shift = 7 - (self.bit_pos % 8);
            u32::from((self.data[byte_idx] >> shift) & 1)
        } else {
            0
        };
        self.bit_pos += 1;
        bit
    }

    #[inline]
    fn renorm(&mut self) {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }

    #[inline]
    pub fn decode_decision(&mut self, ctx_idx: usize) -> u32 {
        let state = usize::from(self.state[ctx_idx]);
        let mps = self.mps[ctx_idx];
        let q = ((self.range >> 6) & 3) as usize;
        let lps = u32::from(RANGE_TAB_LPS[state][q]);
        self.range -= lps;
        let bin;
        if self.offset >= self.range {
            bin = u32::from(1 - mps);
            self.offset -= self.range;
            self.range = lps;
            if state == 0 {
                self.mps[ctx_idx] = 1 - mps;
            }
            self.state[ctx_idx] = TRANS_IDX_LPS[state];
        } else {
            bin = u32::from(mps);
            self.state[ctx_idx] = TRANS_IDX_MPS[state];
        }
        self.renorm();
        bin
    }

    #[inline]
    pub fn decode_bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | self.read_bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    #[inline]
    pub fn decode_terminate(&mut self) -> bool {
        self.range -= 2;
        if self.offset >= self.range {
            true
        } else {
            self.renorm();
            false
        }
    }

    #[inline]
    pub fn decode_bypass_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.decode_bypass();
        }
        v
    }

    #[must_use]
    pub fn bit_pos(&self) -> usize {
        self.bit_pos
    }

    pub fn decode_residual(&mut self, cat: &ResidualCat, out: &mut [i32]) -> usize {
        let n = cat.max_coeff;
        let mut index = [0usize; 64];
        let mut count = 0usize;

        let mut last = 0usize;
        while last < n - 1 {
            let sig_inc = if cat.is_8x8 {
                usize::from(SIG_8X8_FRAME[last])
            } else {
                last
            };
            if self.decode_decision(cat.sig_base + sig_inc) == 1 {
                index[count] = last;
                count += 1;
                let last_inc = if cat.is_8x8 {
                    usize::from(LAST_8X8_FRAME[last])
                } else {
                    last
                };
                if self.decode_decision(cat.last_base + last_inc) == 1 {
                    last = n;
                    break;
                }
            }
            last += 1;
        }
        if last == n - 1 {
            index[count] = n - 1;
            count += 1;
        }

        let mut node_ctx = 0usize;
        for k in (0..count).rev() {
            let pos = index[k];
            let level = if self.decode_decision(cat.abs_base + COEFF_ABS_LEVEL1_CTX[node_ctx]) == 0
            {
                node_ctx = COEFF_ABS_TRANS[0][node_ctx];
                1
            } else {
                let ctxg = cat.abs_base + COEFF_ABS_LEVELGT1_CTX[0][node_ctx];
                node_ctx = COEFF_ABS_TRANS[1][node_ctx];
                let mut coeff_abs = 2u32;
                while coeff_abs < 15 && self.decode_decision(ctxg) == 1 {
                    coeff_abs += 1;
                }
                if coeff_abs >= 15 {
                    let mut j = 0u32;
                    while self.decode_bypass() == 1 && j < 16 + 7 {
                        j += 1;
                    }
                    coeff_abs = 1;
                    while j > 0 {
                        coeff_abs = coeff_abs * 2 + self.decode_bypass();
                        j -= 1;
                    }
                    coeff_abs += 14;
                }
                i32::try_from(coeff_abs).expect("coeff magnitude fits i32")
            };
            out[pos] = if self.decode_bypass() == 1 {
                -level
            } else {
                level
            };
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_tables_well_formed() {
        assert_eq!(TRANS_IDX_MPS[0], 1);
        assert_eq!(TRANS_IDX_MPS[62], 62, "state 62 is the MPS terminal");
        assert_eq!(TRANS_IDX_MPS[63], 63);
        assert_eq!(TRANS_IDX_LPS[0], 0, "LPS at state 0 stays but flips MPS");
        assert_eq!(TRANS_IDX_LPS[63], 63);
    }

    #[test]
    fn context_init_matches_spec_formula() {
        let buf = [0xFFu8; 10];
        let dec = CabacDecoder::new(&buf, 0, 26).expect("init");
        assert_eq!(dec.state[0], 46);
        assert_eq!(dec.mps[0], 0);
        assert_eq!(dec.state[6], 17);
        assert_eq!(dec.mps[6], 1);
    }

    #[test]
    fn engine_init_reads_nine_bit_offset() {
        let buf = [0b1010_1010, 0b1000_0000, 0, 0];
        let dec = CabacDecoder::new(&buf, 0, 26).expect("init");
        assert_eq!(dec.range, 510);
        assert_eq!(dec.offset, 0b1_0101_0101);
    }

    #[test]
    fn engine_aligns_before_init() {
        let buf = [0xFF, 0b1100_0000, 0b1000_0000, 0, 0];
        let dec = CabacDecoder::new(&buf, 3, 26).expect("init");
        assert_eq!(dec.offset, 0b1_1000_0001);
    }
}
