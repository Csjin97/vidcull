use vidcull_core::{Error, Result};

use crate::h264::cabac::{RANGE_TAB_LPS, TRANS_IDX_LPS, TRANS_IDX_MPS};

pub const NUM_CTX: usize = 179;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SyntaxElement {
    SaoMergeFlag,
    SaoTypeIdx,
    SplitCodingUnitFlag,
    CuTransquantBypassFlag,
    SkipFlag,
    CuQpDelta,
    PredModeFlag,
    PartMode,
    PrevIntraLumaPredFlag,
    IntraChromaPredMode,
    MergeFlag,
    MergeIdx,
    InterPredIdc,
    RefIdxL0,
    RefIdxL1,
    AbsMvdGreater0Flag,
    AbsMvdGreater1Flag,
    MvpLxFlag,
    NoResidualDataFlag,
    SplitTransformFlag,
    CbfLuma,
    CbfChroma,
    TransformSkipFlag,
    ExplicitRdpcmFlag,
    ExplicitRdpcmDirFlag,
    LastSignificantCoeffXPrefix,
    LastSignificantCoeffYPrefix,
    SignificantCoeffGroupFlag,
    SignificantCoeffFlag,
    CoeffAbsLevelGreater1Flag,
    CoeffAbsLevelGreater2Flag,
    Log2ResScaleAbs,
    ResScaleSignFlag,
    CuChromaQpOffsetFlag,
    CuChromaQpOffsetIdx,
}

impl SyntaxElement {
    #[must_use]
    pub const fn offset(self) -> usize {
        use SyntaxElement::{
            AbsMvdGreater0Flag, AbsMvdGreater1Flag, CbfChroma, CbfLuma, CoeffAbsLevelGreater1Flag,
            CoeffAbsLevelGreater2Flag, CuChromaQpOffsetFlag, CuChromaQpOffsetIdx, CuQpDelta,
            CuTransquantBypassFlag, ExplicitRdpcmDirFlag, ExplicitRdpcmFlag, InterPredIdc,
            IntraChromaPredMode, LastSignificantCoeffXPrefix, LastSignificantCoeffYPrefix,
            Log2ResScaleAbs, MergeFlag, MergeIdx, MvpLxFlag, NoResidualDataFlag, PartMode,
            PredModeFlag, PrevIntraLumaPredFlag, RefIdxL0, RefIdxL1, ResScaleSignFlag,
            SaoMergeFlag, SaoTypeIdx, SignificantCoeffFlag, SignificantCoeffGroupFlag, SkipFlag,
            SplitCodingUnitFlag, SplitTransformFlag, TransformSkipFlag,
        };
        match self {
            SaoMergeFlag => 0,
            SaoTypeIdx => 1,
            SplitCodingUnitFlag => 2,
            CuTransquantBypassFlag => 5,
            SkipFlag => 6,
            CuQpDelta => 9,
            PredModeFlag => 12,
            PartMode => 13,
            PrevIntraLumaPredFlag => 17,
            IntraChromaPredMode => 18,
            MergeFlag => 20,
            MergeIdx => 21,
            InterPredIdc => 22,
            RefIdxL0 => 27,
            RefIdxL1 => 29,
            AbsMvdGreater0Flag => 31,
            AbsMvdGreater1Flag => 33,
            MvpLxFlag => 35,
            NoResidualDataFlag => 36,
            SplitTransformFlag => 37,
            CbfLuma => 40,
            CbfChroma => 42,
            TransformSkipFlag => 47,
            ExplicitRdpcmFlag => 49,
            ExplicitRdpcmDirFlag => 51,
            LastSignificantCoeffXPrefix => 53,
            LastSignificantCoeffYPrefix => 71,
            SignificantCoeffGroupFlag => 89,
            SignificantCoeffFlag => 93,
            CoeffAbsLevelGreater1Flag => 137,
            CoeffAbsLevelGreater2Flag => 161,
            Log2ResScaleAbs => 167,
            ResScaleSignFlag => 175,
            CuChromaQpOffsetFlag => 177,
            CuChromaQpOffsetIdx => 178,
        }
    }
}

#[rustfmt::skip]
static INIT_VALUES_I: [u8; NUM_CTX] = [
    153,
    200,
    139, 141, 157,
    154,
    154, 154, 154,
    154, 154, 154,
    154,
    184, 154, 154, 154,
    184,
    63, 139,
    154,
    154,
    154, 154, 154, 154, 154,
    154, 154,
    154, 154,
    154, 154,
    154, 154,
    154,
    154,
    153, 138, 138,
    111, 141,
    94, 138, 182, 154, 154,
    139, 139,
    139, 139,
    139, 139,
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111,
     79, 108, 123,  63,
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111,
     79, 108, 123,  63,
    91, 171, 134, 141,
    111, 111, 125, 110, 110,  94, 124, 108, 124, 107, 125, 141, 179, 153,
    125, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140,
    139, 182, 182, 152, 136, 152, 136, 153, 136, 139, 111, 136, 139, 111,
    141, 111,
    140,  92, 137, 138, 140, 152, 138, 139, 153,  74, 149,  92, 139, 107,
    122, 152, 140, 179, 166, 182, 140, 227, 122, 197,
    138, 153, 136, 167, 152, 152,
    154, 154, 154, 154, 154, 154, 154, 154,
    154, 154,
    154,
    154,
];

pub struct CabacDecoder<'a> {
    data: &'a [u8],
    bit_pos: usize,
    range: u32,
    offset: u32,
    state: [u8; NUM_CTX],
    mps: [u8; NUM_CTX],
}

impl<'a> CabacDecoder<'a> {
    pub fn new(rbsp: &'a [u8], data_byte_offset: usize, slice_qp: i32) -> Result<Self> {
        let start_bit = data_byte_offset * 8;
        if start_bit + 9 > rbsp.len() * 8 {
            return Err(Error::Parse(
                "hevc cabac: bitstream too short for arithmetic init".into(),
            ));
        }
        let mut dec = Self {
            data: rbsp,
            bit_pos: start_bit,
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

    pub fn reinit_contexts(&mut self, slice_qp: i32) {
        self.init_contexts(slice_qp);
    }

    fn init_contexts(&mut self, slice_qp: i32) {
        let qp = slice_qp.clamp(0, 51);
        for (idx, &init_value) in INIT_VALUES_I.iter().enumerate() {
            let m = (i32::from(init_value) >> 4) * 5 - 45;
            let n = ((i32::from(init_value) & 15) << 3) - 16;
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
    pub fn decode_bin(&mut self, ctx_idx: usize) -> u32 {
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
    pub fn decode_bin_se(&mut self, se: SyntaxElement, ctx_idx_inc: usize) -> u32 {
        self.decode_bin(se.offset() + ctx_idx_inc)
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
    pub fn decode_bypass_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.decode_bypass();
        }
        v
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

    #[must_use]
    pub fn bit_pos(&self) -> usize {
        self.bit_pos
    }

    #[must_use]
    pub fn save_contexts(&self) -> ([u8; NUM_CTX], [u8; NUM_CTX]) {
        (self.state, self.mps)
    }

    pub fn restore_contexts(&mut self, snap: &([u8; NUM_CTX], [u8; NUM_CTX])) {
        self.state = snap.0;
        self.mps = snap.1;
    }

    pub fn reinit_substream(&mut self, byte_off: usize) {
        self.bit_pos = byte_off * 8;
        self.range = 510;
        self.offset = 0;
        for _ in 0..9 {
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_formula_matches_spec_worked_values() {
        for qp in [0, 26, 27, 51] {
            let (state, mps) = init_one(154, qp);
            assert_eq!((state, mps), (0, 1), "CNU at qp {qp}");
        }
        assert_eq!(init_one(200, 27), (9, 1));
        assert_eq!(init_one(63, 27), (10, 0));
    }

    fn init_one(init_value: u8, qp: i32) -> (u8, u8) {
        let m = (i32::from(init_value) >> 4) * 5 - 45;
        let n = ((i32::from(init_value) & 15) << 3) - 16;
        let pre = (((m * qp.clamp(0, 51)) >> 4) + n).clamp(1, 126);
        if pre <= 63 {
            (u8::try_from(63 - pre).unwrap(), 0)
        } else {
            (u8::try_from(pre - 64).unwrap(), 1)
        }
    }

    #[test]
    fn context_layout_is_consistent() {
        assert_eq!(INIT_VALUES_I.len(), NUM_CTX);
        assert_eq!(SyntaxElement::SaoMergeFlag.offset(), 0);
        assert_eq!(INIT_VALUES_I[SyntaxElement::SaoTypeIdx.offset()], 200);
        assert_eq!(
            INIT_VALUES_I[SyntaxElement::SplitTransformFlag.offset()],
            153
        );
        assert_eq!(INIT_VALUES_I[SyntaxElement::CbfLuma.offset()], 111);
        assert_eq!(
            INIT_VALUES_I[SyntaxElement::SignificantCoeffFlag.offset()],
            111
        );
        assert_eq!(
            INIT_VALUES_I[SyntaxElement::CoeffAbsLevelGreater2Flag.offset()],
            138
        );
        assert_eq!(SyntaxElement::CuChromaQpOffsetIdx.offset(), NUM_CTX - 1);
    }

    fn engine(bytes: &[u8]) -> CabacDecoder<'_> {
        CabacDecoder::new(bytes, 0, 26).expect("engine seeds")
    }

    #[test]
    fn bypass_stream_matches_h264_engine() {
        let bytes: &[u8] = &[
            0x3D, 0xA1, 0x07, 0xFF, 0x00, 0x55, 0xC3, 0x9E, 0x12, 0x80, 0x6B, 0xF4,
        ];
        let mut h264 = crate::h264::cabac::CabacDecoder::new(bytes, 0, 26).expect("h264 seeds");
        let mut hevc = engine(bytes);
        for i in 0..40 {
            assert_eq!(
                hevc.decode_bypass(),
                h264.decode_bypass(),
                "bypass bin {i} diverged from the H.264 engine"
            );
        }
    }

    #[test]
    fn terminate_matches_h264_engine() {
        let bytes: &[u8] = &[0x7E, 0x44, 0x91, 0xC0, 0x2B, 0xFD, 0x10, 0x88];
        let mut h264 = crate::h264::cabac::CabacDecoder::new(bytes, 0, 26).expect("h264 seeds");
        let mut hevc = engine(bytes);
        for i in 0..6 {
            assert_eq!(hevc.decode_terminate(), h264.decode_terminate(), "term {i}");
            assert_eq!(
                hevc.decode_bypass(),
                h264.decode_bypass(),
                "bypass after {i}"
            );
        }
    }

    #[test]
    fn wpp_context_save_restore_round_trips() {
        let bytes: &[u8] = &[
            0x3D, 0xA1, 0x07, 0xFF, 0x00, 0x55, 0xC3, 0x9E, 0x12, 0x80, 0x6B, 0xF4,
        ];
        let mut dec = engine(bytes);
        let snap = dec.save_contexts();
        for ctx in 0..8 {
            dec.decode_bin(ctx);
        }
        assert_ne!(
            (dec.state, dec.mps),
            snap,
            "decoding context bins must move the models"
        );
        dec.restore_contexts(&snap);
        assert_eq!((dec.state, dec.mps), snap, "restore returns the snapshot");
    }

    #[test]
    fn reinit_substream_matches_fresh_seed() {
        let bytes: &[u8] = &[
            0x3D, 0xA1, 0x07, 0xFF, 0x00, 0x55, 0xC3, 0x9E, 0x12, 0x80, 0x6B, 0xF4,
        ];
        let target = 4usize;
        let fresh = CabacDecoder::new(bytes, target, 26).expect("fresh seed");
        let mut dec = engine(bytes);
        let ctx_before = (dec.state, dec.mps);
        for _ in 0..20 {
            dec.decode_bypass();
        }
        dec.reinit_substream(target);
        assert_eq!(dec.range, fresh.range, "range reset to 510");
        assert_eq!(dec.offset, fresh.offset, "9-bit offset re-read at target");
        assert_eq!(dec.bit_pos, target * 8 + 9, "cursor sits past the seed");
        assert_eq!(
            (dec.state, dec.mps),
            ctx_before,
            "reinit must not touch the context models"
        );
    }

    #[test]
    fn rejects_truncated_init() {
        match CabacDecoder::new(&[0x00], 0, 26) {
            Err(Error::Parse(_)) => {}
            other => panic!(
                "one byte cannot seed a 9-bit offset, got {:?}",
                other.is_ok()
            ),
        }
    }
}
