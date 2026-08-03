use vidcull_core::error::{Error, Result};

use crate::tier1::Tier1Fingerprint;
use crate::tier2::Tier2Fingerprint;

pub const MAGIC: [u8; 4] = *b"AVSF";

pub const FORMAT_VERSION: u8 = 1;

pub const HEADER_LEN: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PayloadKind {
    Tier1Global = 1,
    Tier2Temporal = 2,
}

impl PayloadKind {
    fn from_byte(b: u8) -> Result<Self> {
        match b {
            1 => Ok(Self::Tier1Global),
            2 => Ok(Self::Tier2Temporal),
            other => Err(Error::Serialization(format!(
                "unknown fingerprint payload kind: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub kind: PayloadKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fingerprint {
    Tier1(Tier1Fingerprint),
    Tier2(Tier2Fingerprint),
}

pub fn encode_tier1(fp: &Tier1Fingerprint) -> Result<Vec<u8>> {
    Ok(wrap(PayloadKind::Tier1Global, &fp.to_bytes()?))
}

pub fn encode_tier2(fp: &Tier2Fingerprint) -> Result<Vec<u8>> {
    Ok(wrap(PayloadKind::Tier2Temporal, &fp.to_bytes()?))
}

pub fn peek_header(bytes: &[u8]) -> Result<Header> {
    split(bytes).map(|(header, _)| header)
}

pub fn decode(bytes: &[u8]) -> Result<Fingerprint> {
    let (header, payload) = split(bytes)?;
    match header.kind {
        PayloadKind::Tier1Global => Ok(Fingerprint::Tier1(Tier1Fingerprint::from_bytes(payload)?)),
        PayloadKind::Tier2Temporal => {
            Ok(Fingerprint::Tier2(Tier2Fingerprint::from_bytes(payload)?))
        }
    }
}

pub fn decode_tier1(bytes: &[u8]) -> Result<Tier1Fingerprint> {
    let (_, payload) = expect(bytes, PayloadKind::Tier1Global)?;
    Tier1Fingerprint::from_bytes(payload)
}

pub fn decode_tier2(bytes: &[u8]) -> Result<Tier2Fingerprint> {
    let (_, payload) = expect(bytes, PayloadKind::Tier2Temporal)?;
    Tier2Fingerprint::from_bytes(payload)
}

fn wrap(kind: PayloadKind, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    out.push(kind as u8);
    out.extend_from_slice(payload);
    out
}

fn split(bytes: &[u8]) -> Result<(Header, &[u8])> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Serialization(format!(
            "fingerprint blob too short: {} byte(s), need at least {HEADER_LEN}",
            bytes.len()
        )));
    }
    if bytes[0..4] != MAGIC {
        return Err(Error::Serialization(format!(
            "bad fingerprint magic: {:02x?}",
            &bytes[0..4]
        )));
    }
    let version = bytes[4];
    if version == 0 || version > FORMAT_VERSION {
        return Err(Error::Unsupported(format!(
            "fingerprint format version {version} is not supported by this build (max {FORMAT_VERSION})"
        )));
    }
    let kind = PayloadKind::from_byte(bytes[5])?;
    Ok((Header { version, kind }, &bytes[HEADER_LEN..]))
}

fn expect(bytes: &[u8], want: PayloadKind) -> Result<(Header, &[u8])> {
    let (header, payload) = split(bytes)?;
    if header.kind != want {
        return Err(Error::Serialization(format!(
            "expected {want:?} payload, found {:?}",
            header.kind
        )));
    }
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier1::GopSignature;

    #[test]
    fn payload_kind_round_trips_through_byte() {
        for kind in [PayloadKind::Tier1Global, PayloadKind::Tier2Temporal] {
            assert_eq!(PayloadKind::from_byte(kind as u8).unwrap(), kind);
        }
    }

    #[test]
    fn payload_kind_rejects_unknown_byte() {
        assert!(PayloadKind::from_byte(0).is_err());
        assert!(PayloadKind::from_byte(3).is_err());
    }

    #[test]
    fn wrap_produces_exactly_header_plus_payload() {
        let wrapped = wrap(PayloadKind::Tier1Global, &[0xAA, 0xBB]);
        assert_eq!(wrapped, [0x41, 0x56, 0x53, 0x46, 0x01, 0x01, 0xAA, 0xBB]);
    }

    #[test]
    fn split_round_trips_wrap() {
        let wrapped = wrap(PayloadKind::Tier2Temporal, &[1, 2, 3]);
        let (header, payload) = split(&wrapped).unwrap();
        assert_eq!(header.version, FORMAT_VERSION);
        assert_eq!(header.kind, PayloadKind::Tier2Temporal);
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn encode_decode_tier1_round_trip() {
        use vidcull_core::types::Codec;
        let fp = Tier1Fingerprint {
            duration_ms: 1000,
            codec: Codec::H264,
            gop: GopSignature {
                keyframe_count: 5,
                mean_gop_ms: 200,
                max_gop_ms: 300,
            },
            global_phash: 0x1234_5678_9ABC_DEF0,
        };
        let encoded = encode_tier1(&fp).unwrap();

        let header = peek_header(&encoded).unwrap();
        assert_eq!(header.version, FORMAT_VERSION);
        assert_eq!(header.kind, PayloadKind::Tier1Global);

        let decoded = decode(&encoded).unwrap();
        match decoded {
            Fingerprint::Tier1(decoded_fp) => assert_eq!(decoded_fp, fp),
            Fingerprint::Tier2(_) => panic!("Expected Tier1 fingerprint"),
        }

        let decoded_tier1 = decode_tier1(&encoded).unwrap();
        assert_eq!(decoded_tier1, fp);
    }

    #[test]
    fn encode_decode_tier2_round_trip() {
        use crate::tier2::SceneHash;
        let fp = Tier2Fingerprint {
            scenes: vec![
                SceneHash {
                    timestamp_ms: 100,
                    phash: 1,
                },
                SceneHash {
                    timestamp_ms: 200,
                    phash: 2,
                },
            ],
        };
        let encoded = encode_tier2(&fp).unwrap();

        let header = peek_header(&encoded).unwrap();
        assert_eq!(header.version, FORMAT_VERSION);
        assert_eq!(header.kind, PayloadKind::Tier2Temporal);

        let decoded = decode(&encoded).unwrap();
        match decoded {
            Fingerprint::Tier2(decoded_fp) => assert_eq!(decoded_fp.scenes.len(), 2),
            Fingerprint::Tier1(_) => panic!("Expected Tier2 fingerprint"),
        }

        let decoded_tier2 = decode_tier2(&encoded).unwrap();
        assert_eq!(decoded_tier2.scenes.len(), 2);
    }

    #[test]
    fn peek_header_error_paths() {
        assert!(peek_header(&[0; 5]).is_err());

        let bad_magic = [0x00, 0x00, 0x00, 0x00, 0x01, 0x01];
        assert!(peek_header(&bad_magic).is_err());

        let zero_ver = [0x41, 0x56, 0x53, 0x46, 0x00, 0x01];
        assert!(peek_header(&zero_ver).is_err());

        let future_ver = [0x41, 0x56, 0x53, 0x46, 0x02, 0x01];
        assert!(peek_header(&future_ver).is_err());

        let bad_kind = [0x41, 0x56, 0x53, 0x46, 0x01, 0x03];
        assert!(peek_header(&bad_kind).is_err());
    }

    #[test]
    fn decode_rejects_mismatched_kind() {
        use vidcull_core::types::Codec;
        let fp = Tier1Fingerprint {
            duration_ms: 1000,
            codec: Codec::H264,
            gop: GopSignature {
                keyframe_count: 5,
                mean_gop_ms: 200,
                max_gop_ms: 300,
            },
            global_phash: 0x1234_5678_9ABC_DEF0,
        };
        let encoded = encode_tier1(&fp).unwrap();
        assert!(decode_tier2(&encoded).is_err());
    }
}
