use vidcull_core::{Error, Result};

#[must_use]
pub fn rbsp_from_ebsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0u32;
    let mut i = 0;
    while i < ebsp.len() {
        let b = ebsp[i];
        if zeros >= 2 && b == 0x03 && ebsp.get(i + 1).is_some_and(|&n| n <= 0x03) {
            zeros = 0;
            i += 1;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    out
}

#[must_use]
pub fn rbsp_from_ebsp_tracked(ebsp: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut skipped = Vec::new();
    let mut zeros = 0u32;
    let mut i = 0;
    while i < ebsp.len() {
        let b = ebsp[i];
        if zeros >= 2 && b == 0x03 && ebsp.get(i + 1).is_some_and(|&n| n <= 0x03) {
            skipped.push(i);
            zeros = 0;
            i += 1;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    (out, skipped)
}

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub fn new(rbsp: &'a [u8]) -> Self {
        Self { data: rbsp, pos: 0 }
    }

    #[must_use]
    pub fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    #[must_use]
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    #[must_use]
    pub fn bits_remaining(&self) -> usize {
        self.total_bits().saturating_sub(self.pos)
    }

    #[must_use]
    pub fn is_byte_aligned(&self) -> bool {
        self.pos % 8 == 0
    }

    pub fn read_bit(&mut self) -> Result<u32> {
        if self.pos >= self.total_bits() {
            return Err(Error::Parse("h264 bitstream: read past end".into()));
        }
        let byte = self.data[self.pos / 8];
        let shift = 7 - (self.pos % 8);
        self.pos += 1;
        Ok(u32::from((byte >> shift) & 1))
    }

    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::Parse(format!("h264 bitstream: read_bits({n}) > 32")));
        }
        if (n as usize) > self.bits_remaining() {
            return Err(Error::Parse("h264 bitstream: read_bits past end".into()));
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()?;
        }
        Ok(value)
    }

    pub fn read_flag(&mut self) -> Result<bool> {
        Ok(self.read_bit()? == 1)
    }

    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        if n > self.bits_remaining() {
            return Err(Error::Parse("h264 bitstream: skip past end".into()));
        }
        self.pos += n;
        Ok(())
    }

    pub fn align_to_byte(&mut self) {
        let rem = self.pos % 8;
        if rem != 0 {
            self.pos += 8 - rem;
        }
    }

    pub fn ue(&mut self) -> Result<u32> {
        let mut leading_zeros = 0u32;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(Error::Parse(
                    "h264 bitstream: exp-golomb prefix too long".into(),
                ));
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Ok((1u32 << leading_zeros) - 1 + suffix)
    }

    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()?;
        let magnitude = i32::try_from((u64::from(k) + 1) >> 1)
            .map_err(|_| Error::Parse("h264 bitstream: se magnitude out of range".into()))?;
        Ok(if k & 1 == 1 { magnitude } else { -magnitude })
    }

    #[must_use]
    pub fn more_rbsp_data(&self) -> bool {
        if self.pos >= self.total_bits() {
            return false;
        }
        for byte_idx in (0..self.data.len()).rev() {
            let byte = self.data[byte_idx];
            if byte != 0 {
                let bit_in_byte = 7 - byte.trailing_zeros() as usize;
                let last_one = byte_idx * 8 + bit_in_byte;
                return self.pos < last_one;
            }
        }
        false
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        pub(crate) fn new() -> Self {
            Self { bits: Vec::new() }
        }

        pub(crate) fn bit(&mut self, value: u32) {
            self.bits.push((value & 1) as u8);
        }

        pub(crate) fn bits(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                self.bit(value >> i);
            }
        }

        pub(crate) fn flag(&mut self, value: bool) {
            self.bit(u32::from(value));
        }

        pub(crate) fn bit_len(&self) -> usize {
            self.bits.len()
        }

        pub(crate) fn ue(&mut self, value: u32) {
            let nbits = 32 - (value + 1).leading_zeros();
            let leading_zeros = nbits - 1;
            for _ in 0..leading_zeros {
                self.bit(0);
            }
            self.bits(value + 1, nbits);
        }

        pub(crate) fn se(&mut self, value: i32) {
            let v = i64::from(value);
            let code = if v > 0 { 2 * v - 1 } else { -2 * v };
            let code = u32::try_from(code).expect("se code fits u32");
            self.ue(code);
        }

        pub(crate) fn into_rbsp(mut self) -> Vec<u8> {
            self.bit(1);
            while self.bits.len() % 8 != 0 {
                self.bit(0);
            }
            self.pack()
        }

        fn pack(&self) -> Vec<u8> {
            let mut out = vec![0u8; self.bits.len().div_ceil(8)];
            for (i, &b) in self.bits.iter().enumerate() {
                if b == 1 {
                    out[i / 8] |= 1 << (7 - (i % 8));
                }
            }
            out
        }
    }

    #[test]
    fn writer_round_trips_through_reader() {
        use super::BitReader;
        let mut w = BitWriter::new();
        w.bits(0x42, 8);
        w.ue(10);
        w.se(-3);
        w.flag(true);
        let rbsp = w.into_rbsp();
        let mut r = BitReader::new(&rbsp);
        assert_eq!(r.read_bits(8).unwrap(), 0x42);
        assert_eq!(r.ue().unwrap(), 10);
        assert_eq!(r.se().unwrap(), -3);
        assert!(r.read_flag().unwrap());
        assert!(!r.more_rbsp_data(), "only the stop bit remains");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbsp_strips_emulation_prevention_byte() {
        assert_eq!(
            rbsp_from_ebsp(&[0x00, 0x00, 0x03, 0x00]),
            vec![0x00, 0x00, 0x00]
        );
        assert_eq!(
            rbsp_from_ebsp(&[0x00, 0x00, 0x03, 0x01]),
            vec![0x00, 0x00, 0x01]
        );
    }

    #[test]
    fn rbsp_keeps_legitimate_03_and_non_emulation_runs() {
        assert_eq!(rbsp_from_ebsp(&[0xAB, 0x03, 0xCD]), vec![0xAB, 0x03, 0xCD]);
        assert_eq!(
            rbsp_from_ebsp(&[0x00, 0x00, 0x03, 0xFF]),
            vec![0x00, 0x00, 0x03, 0xFF]
        );
        assert_eq!(rbsp_from_ebsp(&[0x00, 0x00, 0x03]), vec![0x00, 0x00, 0x03]);
    }

    #[test]
    fn rbsp_handles_consecutive_emulation_bytes() {
        assert_eq!(
            rbsp_from_ebsp(&[0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00]),
            vec![0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn rbsp_tracked_records_epb_positions() {
        let (rbsp, skipped) = rbsp_from_ebsp_tracked(&[0x00, 0x00, 0x03, 0x00]);
        assert_eq!(rbsp, vec![0x00, 0x00, 0x00]);
        assert_eq!(skipped, vec![2]);
        let (rbsp, skipped) = rbsp_from_ebsp_tracked(&[0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00]);
        assert_eq!(rbsp, vec![0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(skipped, vec![2, 5]);
        let (rbsp, skipped) = rbsp_from_ebsp_tracked(&[0xAB, 0x03, 0xCD]);
        assert_eq!(rbsp, vec![0xAB, 0x03, 0xCD]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn read_bits_is_msb_first() {
        let mut r = BitReader::new(&[0xA6]);
        assert_eq!(r.read_bits(1).unwrap(), 1);
        assert_eq!(r.read_bits(2).unwrap(), 0b01);
        assert_eq!(r.read_bits(3).unwrap(), 0b001);
        assert_eq!(r.read_bits(2).unwrap(), 0b10);
        assert!(r.read_bit().is_err(), "buffer exhausted");
    }

    #[test]
    fn read_bits_spans_byte_boundary() {
        let mut r = BitReader::new(&[0xDE, 0xAD]);
        assert_eq!(r.read_bits(12).unwrap(), 0xDEA);
        assert_eq!(r.read_bits(4).unwrap(), 0xD);
    }

    #[test]
    fn read_bits_zero_and_full_width() {
        let mut r = BitReader::new(&[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(
            r.read_bits(0).unwrap(),
            0,
            "zero-width read does not advance"
        );
        assert_eq!(r.bit_pos(), 0);
        assert_eq!(r.read_bits(32).unwrap(), 0x1234_5678);
        assert!(r.read_bits(33).is_err(), "n > 32 rejected");
    }

    #[test]
    fn ue_decodes_known_codewords() {
        let bytes = [0b1010_0110, 0b0100_0011, 0b1000_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 6);
    }

    #[test]
    fn se_maps_unsigned_to_signed() {
        let bytes = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.se().unwrap(), 0);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
        assert_eq!(r.se().unwrap(), 2);
        assert_eq!(r.se().unwrap(), -2);
    }

    #[test]
    fn align_to_byte_advances_to_boundary() {
        let mut r = BitReader::new(&[0xFF, 0x0F]);
        r.read_bits(3).unwrap();
        r.align_to_byte();
        assert_eq!(r.bit_pos(), 8);
        r.align_to_byte();
        assert_eq!(r.bit_pos(), 8, "already aligned is a no-op");
        assert_eq!(r.read_bits(8).unwrap(), 0x0F);
    }

    #[test]
    fn more_rbsp_data_tracks_stop_bit() {
        let mut r = BitReader::new(&[0b1011_1000]);
        assert!(r.more_rbsp_data());
        r.read_bits(4).unwrap();
        assert!(!r.more_rbsp_data(), "now sitting on the stop bit");
    }

    #[test]
    fn more_rbsp_data_false_on_empty_or_zero_buffer() {
        assert!(!BitReader::new(&[]).more_rbsp_data());
        assert!(!BitReader::new(&[0x00, 0x00]).more_rbsp_data());
    }
}
