use vidcull_core::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalUnitType {
    TrailN,
    TrailR,
    IdrWRadl,
    IdrNLp,
    CraNut,
    Vps,
    Sps,
    Pps,
    Aud,
    PrefixSei,
    SuffixSei,
    Other(u8),
}

impl NalUnitType {
    #[must_use]
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::TrailN,
            1 => Self::TrailR,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            32 => Self::Vps,
            33 => Self::Sps,
            34 => Self::Pps,
            35 => Self::Aud,
            39 => Self::PrefixSei,
            40 => Self::SuffixSei,
            other => Self::Other(other),
        }
    }

    #[must_use]
    pub fn is_irap(self) -> bool {
        matches!(self, Self::IdrWRadl | Self::IdrNLp | Self::CraNut)
            || matches!(self, Self::Other(t) if (16..=23).contains(&t))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    pub unit_type: NalUnitType,
    pub layer_id: u8,
    pub temporal_id: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalUnit {
    pub header: NalHeader,
    pub payload: Vec<u8>,
}

pub fn parse_nal_header(b0: u8, b1: u8) -> Result<NalHeader> {
    if b0 >> 7 != 0 {
        return Err(Error::Parse(format!(
            "hevc nal: forbidden_zero_bit set in header byte 0x{b0:02X}"
        )));
    }
    let unit_type = NalUnitType::from_raw((b0 >> 1) & 0x3F);
    let layer_id = ((b0 & 0x01) << 5) | (b1 >> 3);
    let temporal_id = (b1 & 0x07).saturating_sub(1);
    Ok(NalHeader {
        unit_type,
        layer_id,
        temporal_id,
    })
}

fn nal_from_raw(raw: &[u8]) -> Option<NalUnit> {
    let (&b0, rest) = raw.split_first()?;
    let (&b1, payload) = rest.split_first()?;
    let header = parse_nal_header(b0, b1).ok()?;
    Some(NalUnit {
        header,
        payload: payload.to_vec(),
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
        if let Some(unit) = nal_from_raw(&data[start..raw_end]) {
            units.push(unit);
        }
    }

    units
}

pub fn split_length_prefixed(data: &[u8], length_size: usize) -> Result<Vec<NalUnit>> {
    if !(1..=4).contains(&length_size) {
        return Err(Error::Parse(format!(
            "hevc: NAL length_size {length_size} is not in 1..=4"
        )));
    }

    let mut units = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let length_end = pos + length_size;
        if length_end > data.len() {
            return Err(Error::Parse(format!(
                "hevc: NAL length field at offset {pos} extends past buffer"
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
                "hevc: NAL at offset {pos} claims length {nal_len} but only {} bytes remain",
                data.len() - pos
            )));
        }
        let raw = &data[pos..nal_end];
        pos = nal_end;

        if raw.len() >= 2 {
            let header = parse_nal_header(raw[0], raw[1])?;
            units.push(NalUnit {
                header,
                payload: raw[2..].to_vec(),
            });
        }
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_vps() {
        let h = parse_nal_header(0x40, 0x01).expect("valid VPS header");
        assert_eq!(h.unit_type, NalUnitType::Vps);
        assert_eq!(h.layer_id, 0);
        assert_eq!(h.temporal_id, 0);
    }

    #[test]
    fn parse_header_sps_pps_idr() {
        assert_eq!(
            parse_nal_header(0x42, 0x01).unwrap().unit_type,
            NalUnitType::Sps
        );
        assert_eq!(
            parse_nal_header(0x44, 0x01).unwrap().unit_type,
            NalUnitType::Pps
        );
        assert_eq!(
            parse_nal_header(0x26, 0x01).unwrap().unit_type,
            NalUnitType::IdrWRadl
        );
        assert_eq!(
            parse_nal_header(0x28, 0x01).unwrap().unit_type,
            NalUnitType::IdrNLp
        );
    }

    #[test]
    fn parse_header_layer_and_temporal_id() {
        let h = parse_nal_header(0x41, 0x0B).expect("valid header");
        assert_eq!(h.layer_id, 33);
        assert_eq!(h.temporal_id, 2);
    }

    #[test]
    fn parse_header_forbidden_bit_errors() {
        assert!(parse_nal_header(0x80, 0x01).is_err());
    }

    #[test]
    fn is_irap_covers_idr_cra_bla() {
        assert!(NalUnitType::IdrWRadl.is_irap());
        assert!(NalUnitType::IdrNLp.is_irap());
        assert!(NalUnitType::CraNut.is_irap());
        assert!(NalUnitType::Other(18).is_irap());
        assert!(!NalUnitType::TrailR.is_irap());
        assert!(!NalUnitType::Vps.is_irap());
    }

    #[test]
    fn annex_b_splits_vps_sps_pps() {
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0xAA, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0xBB,
            0x00, 0x00, 0x00, 0x01, 0x44, 0x01, 0xCC,
        ];
        let units = split_annex_b(&data);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].header.unit_type, NalUnitType::Vps);
        assert_eq!(units[0].payload, vec![0xAA]);
        assert_eq!(units[1].header.unit_type, NalUnitType::Sps);
        assert_eq!(units[2].header.unit_type, NalUnitType::Pps);
        assert_eq!(units[2].payload, vec![0xCC]);
    }

    #[test]
    fn length_prefixed_round_trips() {
        let data = [
            0x00, 0x00, 0x00, 0x03, 0x40, 0x01, 0xAA, 0x00, 0x00, 0x00, 0x02, 0x26, 0x01,
        ];
        let units = split_length_prefixed(&data, 4).expect("valid length-prefixed");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].header.unit_type, NalUnitType::Vps);
        assert_eq!(units[0].payload, vec![0xAA]);
        assert_eq!(units[1].header.unit_type, NalUnitType::IdrWRadl);
        assert!(units[1].payload.is_empty());
    }

    #[test]
    fn length_prefixed_rejects_overrun_and_bad_length_size() {
        assert!(split_length_prefixed(&[0x00, 0x00, 0x00, 0x0A, 0x40, 0x01], 4).is_err());
        assert!(split_length_prefixed(&[], 0).is_err());
        assert!(split_length_prefixed(&[], 5).is_err());
    }
}
