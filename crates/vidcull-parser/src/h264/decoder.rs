use vidcull_core::types::Codec;
use vidcull_core::{Error, Result};

use crate::sparse::{GrayscaleFrame, SparseDecoder, SparseSample};

use super::nal::{NalHeader, NalUnitType, split_avcc};
use super::params::{AvcDecoderConfig, parse_avcc, parse_pps, parse_sps};
use super::{BitReader, decode_intra_frame, rbsp_from_ebsp};

#[derive(Debug, Clone)]
pub struct NativeH264Decoder {
    config: AvcDecoderConfig,
}

impl NativeH264Decoder {
    pub fn from_avcc(avcc: &[u8]) -> Result<Self> {
        Ok(Self {
            config: parse_avcc(avcc)?,
        })
    }
}

impl SparseDecoder for NativeH264Decoder {
    fn decode_idr(&mut self, sample: &SparseSample, codec: &Codec) -> Result<GrayscaleFrame> {
        if !matches!(codec, Codec::H264) {
            return Err(Error::Unsupported(format!(
                "native h264 decoder invoked for {codec:?}; only H.264 is in scope"
            )));
        }

        let units = split_avcc(&sample.bytes, self.config.nal_length_size)?;

        let mut sps = self.config.sps.clone();
        let mut pps = self.config.pps.clone();
        let mut slice_rbsps: Vec<(NalHeader, Vec<u8>)> = Vec::new();

        for u in &units {
            match u.header.unit_type {
                NalUnitType::Sps => {
                    sps = parse_sps(&mut BitReader::new(&rbsp_from_ebsp(&u.payload)))?;
                }
                NalUnitType::Pps => {
                    pps = parse_pps(
                        &mut BitReader::new(&rbsp_from_ebsp(&u.payload)),
                        sps.chroma_format_idc,
                    )?;
                }
                NalUnitType::IdrSlice => {
                    slice_rbsps.push((u.header.clone(), rbsp_from_ebsp(&u.payload)));
                }
                _ => {}
            }
        }

        if slice_rbsps.is_empty() {
            return Err(Error::Parse(
                "native h264: sample carried no IDR slice NAL".into(),
            ));
        }

        let slices: Vec<(&NalHeader, &[u8])> =
            slice_rbsps.iter().map(|(h, r)| (h, r.as_slice())).collect();
        let luma = decode_intra_frame(&sps, &pps, &slices)?;

        let width = u32::try_from(luma.width)
            .map_err(|_| Error::Parse("native h264: luma width exceeds u32".into()))?;
        let height = u32::try_from(luma.height)
            .map_err(|_| Error::Parse("native h264: luma height exceeds u32".into()))?;

        let pixels = luma
            .data
            .iter()
            .map(|&y| expand_luma_full_range(y))
            .collect();

        Ok(GrayscaleFrame {
            width,
            height,
            timestamp_ms: sample.timestamp_ms,
            pixels,
        })
    }
}

#[must_use]
pub(crate) fn expand_luma_full_range(y: u8) -> u8 {
    let num = (i32::from(y) - 16) * 255;
    if num <= 0 {
        return 0;
    }
    u8::try_from(((num + 109) / 219).min(255)).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_avcc_rejects_garbage() {
        assert!(NativeH264Decoder::from_avcc(&[0x01, 0x42]).is_err());
    }

    #[test]
    fn decode_idr_rejects_non_h264_codec() {
        let avcc = minimal_avcc();
        let mut dec = NativeH264Decoder::from_avcc(&avcc).expect("avcc parses");
        let sample = SparseSample {
            timestamp_ms: 0,
            bytes: Vec::new(),
        };
        let err = dec
            .decode_idr(&sample, &Codec::H265)
            .expect_err("H.265 must be refused");
        assert!(matches!(err, Error::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn decode_idr_errors_when_sample_has_no_idr_slice() {
        let avcc = minimal_avcc();
        let mut dec = NativeH264Decoder::from_avcc(&avcc).expect("avcc parses");
        let sample = SparseSample {
            timestamp_ms: 0,
            bytes: vec![0x00, 0x00, 0x00, 0x01, 0x06],
        };
        let err = dec
            .decode_idr(&sample, &Codec::H264)
            .expect_err("no IDR slice must error");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }

    #[test]
    fn expand_luma_full_range_matches_ffmpeg_anchors() {
        assert_eq!(expand_luma_full_range(16), 0);
        assert_eq!(expand_luma_full_range(235), 255);
        assert_eq!(expand_luma_full_range(0), 0);
        assert_eq!(expand_luma_full_range(255), 255);
        assert_eq!(expand_luma_full_range(182), 193);
        assert_eq!(expand_luma_full_range(128), 130);
        assert_eq!(expand_luma_full_range(116), 116);
    }

    fn minimal_avcc() -> Vec<u8> {
        let sps_nal: &[u8] = &[0x67, 0x42, 0x00, 0x1E, 0xF4, 0x16, 0x27, 0x20];
        let pps_nal: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
        let mut v = vec![1, 0x42, 0x00, 0x1E, 0xFF, 0xE1];
        v.extend_from_slice(&u16::try_from(sps_nal.len()).unwrap().to_be_bytes());
        v.extend_from_slice(sps_nal);
        v.push(1);
        v.extend_from_slice(&u16::try_from(pps_nal.len()).unwrap().to_be_bytes());
        v.extend_from_slice(pps_nal);
        v
    }
}
