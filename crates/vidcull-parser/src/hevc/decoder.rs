use vidcull_core::types::Codec;
use vidcull_core::{Error, Result};

use crate::h264::decoder::expand_luma_full_range;
use crate::h264::{BitReader, LumaFrame, rbsp_from_ebsp, rbsp_from_ebsp_tracked};
use crate::sparse::{GrayscaleFrame, SparseDecoder, SparseSample};

use super::nal::{NalHeader, NalUnitType, split_length_prefixed};
use super::params::{HevcDecoderConfig, parse_hvcc, parse_pps, parse_sps};
use super::{Pps, Sps, decode_slice_to_luma_tracked, parse_slice_segment_header};

#[derive(Debug, Clone)]
pub struct NativeH265Decoder {
    config: HevcDecoderConfig,
}

impl NativeH265Decoder {
    pub fn from_hvcc(hvcc: &[u8]) -> Result<Self> {
        Ok(Self {
            config: parse_hvcc(hvcc)?,
        })
    }
}

impl SparseDecoder for NativeH265Decoder {
    fn decode_idr(&mut self, sample: &SparseSample, codec: &Codec) -> Result<GrayscaleFrame> {
        if !matches!(codec, Codec::H265) {
            return Err(Error::Unsupported(format!(
                "native h265 decoder invoked for {codec:?}; only H.265 is in scope"
            )));
        }

        let units = split_length_prefixed(&sample.bytes, self.config.nal_length_size)?;

        let mut sps = self.config.sps.clone();
        let mut pps = self.config.pps.clone();
        let mut slice: Option<(NalHeader, Vec<u8>, Vec<usize>)> = None;

        for u in &units {
            match u.header.unit_type {
                NalUnitType::Sps => {
                    sps = parse_sps(&mut BitReader::new(&rbsp_from_ebsp(&u.payload)))?;
                }
                NalUnitType::Pps => {
                    pps = parse_pps(&mut BitReader::new(&rbsp_from_ebsp(&u.payload)))?;
                }
                t if t.is_irap() && slice.is_none() => {
                    let (rbsp, skipped) = rbsp_from_ebsp_tracked(&u.payload);
                    slice = Some((u.header, rbsp, skipped));
                }
                _ => {}
            }
        }

        let (header, rbsp, skipped) = slice
            .ok_or_else(|| Error::Parse("native h265: sample carried no IRAP slice NAL".into()))?;

        let luma = reconstruct(&sps, &pps, header, &rbsp, &skipped)?;

        let width = u32::try_from(luma.width)
            .map_err(|_| Error::Parse("native h265: luma width exceeds u32".into()))?;
        let height = u32::try_from(luma.height)
            .map_err(|_| Error::Parse("native h265: luma height exceeds u32".into()))?;

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

fn reconstruct(
    sps: &Sps,
    pps: &Pps,
    header: NalHeader,
    rbsp: &[u8],
    skipped: &[usize],
) -> Result<LumaFrame> {
    let sh = parse_slice_segment_header(&mut BitReader::new(rbsp), sps, pps, &header)?;
    decode_slice_to_luma_tracked(sps, pps, &sh, rbsp, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_hvcc_rejects_garbage() {
        assert!(NativeH265Decoder::from_hvcc(&[0x01, 0x42]).is_err());
    }

    #[test]
    fn from_hvcc_rejects_short_record() {
        let err = NativeH265Decoder::from_hvcc(&[1u8; 22]).expect_err("short record must error");
        assert!(matches!(err, Error::Parse(_)), "got {err:?}");
    }
}
