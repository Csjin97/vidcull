use vidcull_core::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NalUnitType {
    NonIdrSlice,
    IdrSlice,
    Sei,
    Sps,
    Pps,
    AccessUnitDelimiter,
    Other(u8),
}

impl NalUnitType {
    #[must_use]
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::NonIdrSlice,
            5 => Self::IdrSlice,
            6 => Self::Sei,
            7 => Self::Sps,
            8 => Self::Pps,
            9 => Self::AccessUnitDelimiter,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalHeader {
    pub ref_idc: u8,
    pub unit_type: NalUnitType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnit {
    pub header: NalHeader,
    pub payload: Vec<u8>,
}

pub fn parse_nal_header(byte: u8) -> Result<NalHeader> {
    if byte >> 7 != 0 {
        return Err(Error::Parse(format!(
            "h264 nal: forbidden_zero_bit set in header byte 0x{byte:02X}"
        )));
    }
    Ok(NalHeader {
        ref_idc: (byte >> 5) & 0x3,
        unit_type: NalUnitType::from_raw(byte & 0x1F),
    })
}

#[must_use]
pub fn split_annex_b(data: &[u8]) -> Vec<NalUnit> {
    let mut nal_starts: Vec<usize> = Vec::new();
    let len = data.len();

    let mut i = 0;
    while i + 2 < len {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if data[i + 2] == 0x01 {
                nal_starts.push(i + 3);
                i += 3;
                continue;
            } else if i + 3 < len && data[i + 2] == 0x00 && data[i + 3] == 0x01 {
                nal_starts.push(i + 4);
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    if nal_starts.is_empty() {
        return Vec::new();
    }

    let mut units = Vec::with_capacity(nal_starts.len());

    for (idx, &start) in nal_starts.iter().enumerate() {
        let raw_end = if idx + 1 < nal_starts.len() {
            let next = nal_starts[idx + 1];
            let mut end = next;
            if end > 0 && data[end - 1] == 0x01 {
                end -= 1;
            }
            while end > start && data[end - 1] == 0x00 {
                end -= 1;
            }
            end
        } else {
            len
        };

        if start >= raw_end {
            continue;
        }

        let raw = &data[start..raw_end];
        if raw.is_empty() {
            continue;
        }

        let Ok(header) = parse_nal_header(raw[0]) else {
            continue;
        };

        units.push(NalUnit {
            header,
            payload: raw[1..].to_vec(),
        });
    }

    units
}

pub fn split_avcc(data: &[u8], length_size: usize) -> Result<Vec<NalUnit>> {
    if !(1..=4).contains(&length_size) {
        return Err(Error::Parse(format!(
            "h264 avcc: length_size {length_size} is not in 1..=4"
        )));
    }

    let mut units = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let length_end = pos + length_size;
        if length_end > data.len() {
            return Err(Error::Parse(format!(
                "h264 avcc: length field at offset {pos} extends past buffer (need {length_end}, \
                 have {})",
                data.len()
            )));
        }

        let mut nal_len: usize = 0;
        for &b in &data[pos..length_end] {
            nal_len = (nal_len << 8) | usize::from(b);
        }
        pos = length_end;

        let nal_end = pos + nal_len;
        if nal_end > data.len() {
            return Err(Error::Parse(format!(
                "h264 avcc: NAL at offset {pos} claims length {nal_len} but only {} bytes remain",
                data.len() - pos
            )));
        }

        let raw = &data[pos..nal_end];
        pos = nal_end;

        if raw.is_empty() {
            continue;
        }

        let header = parse_nal_header(raw[0])?;
        units.push(NalUnit {
            header,
            payload: raw[1..].to_vec(),
        });
    }

    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_sps() {
        let h = parse_nal_header(0x67).expect("0x67 should be valid");
        assert_eq!(h.ref_idc, 3);
        assert_eq!(h.unit_type, NalUnitType::Sps);
    }

    #[test]
    fn parse_header_idr_slice() {
        let h = parse_nal_header(0x65).expect("0x65 should be valid");
        assert_eq!(h.ref_idc, 3);
        assert_eq!(h.unit_type, NalUnitType::IdrSlice);
    }

    #[test]
    fn parse_header_forbidden_bit_set_errors() {
        assert!(parse_nal_header(0x80).is_err());
        assert!(parse_nal_header(0xE7).is_err());
    }

    #[test]
    fn parse_header_other_type() {
        let h = parse_nal_header(0x0C).expect("0x0C should be valid");
        assert_eq!(h.ref_idc, 0);
        assert_eq!(h.unit_type, NalUnitType::Other(12));
    }

    #[test]
    fn annex_b_single_nal_3byte_start_code() {
        let data = [0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, 0xCC];
        let units = split_annex_b(&data);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].header.unit_type, NalUnitType::Sps);
        assert_eq!(units[0].payload, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn annex_b_single_nal_4byte_start_code() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0xDE, 0xAD];
        let units = split_annex_b(&data);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].header.unit_type, NalUnitType::IdrSlice);
        assert_eq!(units[0].payload, vec![0xDE, 0xAD]);
    }

    #[test]
    fn annex_b_multiple_nals_mixed_start_codes() {
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x01, 0x00, 0x00, 0x01, 0x68, 0x02, 0x03, 0x00, 0x00,
            0x00, 0x01, 0x65,
        ];
        let units = split_annex_b(&data);
        assert_eq!(units.len(), 3, "expected 3 NAL units");
        assert_eq!(units[0].header.unit_type, NalUnitType::Sps);
        assert_eq!(units[0].payload, vec![0x01]);
        assert_eq!(units[1].header.unit_type, NalUnitType::Pps);
        assert_eq!(units[1].payload, vec![0x02, 0x03]);
        assert_eq!(units[2].header.unit_type, NalUnitType::IdrSlice);
        assert!(units[2].payload.is_empty());
    }

    #[test]
    fn annex_b_empty_input() {
        assert!(split_annex_b(&[]).is_empty());
    }

    #[test]
    fn annex_b_no_start_code() {
        assert!(split_annex_b(&[0x67, 0xAA, 0xBB]).is_empty());
    }

    #[test]
    fn annex_b_leading_zeros_tolerated() {
        let data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x68, 0xFF];
        let units = split_annex_b(&data);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].header.unit_type, NalUnitType::Pps);
        assert_eq!(units[0].payload, vec![0xFF]);
    }

    #[test]
    fn avcc_single_nal_length4() {
        let data = [0x00, 0x00, 0x00, 0x02, 0x67, 0xAA];
        let units = split_avcc(&data, 4).expect("valid AVCC");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].header.unit_type, NalUnitType::Sps);
        assert_eq!(units[0].payload, vec![0xAA]);
    }

    #[test]
    fn avcc_multi_nal_length4() {
        let data = [
            0x00, 0x00, 0x00, 0x03, 0x67, 0xAA, 0xBB, 0x00, 0x00, 0x00, 0x01, 0x65,
        ];
        let units = split_avcc(&data, 4).expect("valid AVCC");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].header.unit_type, NalUnitType::Sps);
        assert_eq!(units[0].payload, vec![0xAA, 0xBB]);
        assert_eq!(units[1].header.unit_type, NalUnitType::IdrSlice);
        assert!(units[1].payload.is_empty());
    }

    #[test]
    fn avcc_truncated_length_errors() {
        let data = [0x00, 0x00, 0x01];
        assert!(split_avcc(&data, 4).is_err());
    }

    #[test]
    fn avcc_length_overruns_buffer_errors() {
        let data = [0x00, 0x00, 0x00, 0x0A, 0x67, 0xAA];
        assert!(split_avcc(&data, 4).is_err());
    }

    #[test]
    fn avcc_invalid_length_size_errors() {
        assert!(split_avcc(&[], 0).is_err());
        assert!(split_avcc(&[], 5).is_err());
    }

    #[test]
    fn avcc_empty_input_ok() {
        let units = split_avcc(&[], 4).expect("empty AVCC is valid");
        assert!(units.is_empty());
    }
}
