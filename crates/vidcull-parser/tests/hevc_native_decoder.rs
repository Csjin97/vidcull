use vidcull_core::types::Codec;

use vidcull_parser::hevc::NativeH265Decoder;
use vidcull_parser::hevc::nal::{NalUnit, NalUnitType, split_annex_b};
use vidcull_parser::sparse::{SparseDecoder, SparseSample};

use std::path::{Path, PathBuf};

const FIXTURES: &[(&str, usize, usize)] = &[("clip", 160, 90), ("clip2", 256, 144)];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hevc-sao")
}

fn nal_type_code(t: NalUnitType) -> u8 {
    match t {
        NalUnitType::TrailN => 0,
        NalUnitType::TrailR => 1,
        NalUnitType::IdrWRadl => 19,
        NalUnitType::IdrNLp => 20,
        NalUnitType::CraNut => 21,
        NalUnitType::Vps => 32,
        NalUnitType::Sps => 33,
        NalUnitType::Pps => 34,
        NalUnitType::Aud => 35,
        NalUnitType::PrefixSei => 39,
        NalUnitType::SuffixSei => 40,
        NalUnitType::Other(v) => v,
    }
}

fn raw_nal(u: &NalUnit) -> Vec<u8> {
    let t = nal_type_code(u.header.unit_type);
    let b0 = (t << 1) | (u.header.layer_id >> 5);
    let b1 = ((u.header.layer_id & 0x1F) << 3) | (u.header.temporal_id + 1);
    let mut bytes = vec![b0, b1];
    bytes.extend_from_slice(&u.payload);
    bytes
}

fn build_hvcc(sps_nal: &[u8], pps_nal: &[u8]) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&[0u8; 20]);
    v.push(0xFF);
    v.push(2);
    for (nal_type, nal) in [(33u8, sps_nal), (34u8, pps_nal)] {
        v.push(nal_type);
        v.extend_from_slice(&1u16.to_be_bytes());
        v.extend_from_slice(
            &u16::try_from(nal.len())
                .expect("NAL fits u16")
                .to_be_bytes(),
        );
        v.extend_from_slice(nal);
    }
    v
}

fn build_sample(slice_nals: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in slice_nals {
        let len = u32::try_from(nal.len()).expect("NAL fits u32");
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.extend_from_slice(nal);
    }
    bytes
}

fn repackage(hevc: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let units = split_annex_b(hevc);
    let mut sps_nal = None;
    let mut pps_nal = None;
    let mut slice_nals = Vec::new();
    for u in &units {
        match u.header.unit_type {
            NalUnitType::Sps => sps_nal = Some(raw_nal(u)),
            NalUnitType::Pps => pps_nal = Some(raw_nal(u)),
            t if t.is_irap() => slice_nals.push(raw_nal(u)),
            _ => {}
        }
    }
    let sps_nal = sps_nal.expect("fixture carries an SPS");
    let pps_nal = pps_nal.expect("fixture carries a PPS");
    assert!(!slice_nals.is_empty(), "fixture carries an IRAP slice");
    (build_hvcc(&sps_nal, &pps_nal), build_sample(&slice_nals))
}

#[test]
fn native_sparse_decoder_is_bit_exact_vs_committed_reference() {
    for &(name, width, height) in FIXTURES {
        let dir = fixture_dir();
        let hevc = std::fs::read(dir.join(format!("{name}.hevc")))
            .unwrap_or_else(|e| panic!("read {name}.hevc: {e}"));
        let reference = std::fs::read(dir.join(format!("{name}.gray8")))
            .unwrap_or_else(|e| panic!("read {name}.gray8: {e}"));

        let (hvcc, sample_bytes) = repackage(&hevc);
        let mut decoder = NativeH265Decoder::from_hvcc(&hvcc)
            .unwrap_or_else(|e| panic!("[{name}] build decoder from hvcC: {e:?}"));

        let guard = SparseSample {
            timestamp_ms: 0,
            bytes: sample_bytes.clone(),
        };
        assert!(
            matches!(
                decoder.decode_idr(&guard, &Codec::H264),
                Err(vidcull_core::Error::Unsupported(_))
            ),
            "[{name}] H.264 codec must be refused by the H.265 decoder"
        );

        let sample = SparseSample {
            timestamp_ms: 1234,
            bytes: sample_bytes,
        };
        let frame = decoder
            .decode_idr(&sample, &Codec::H265)
            .unwrap_or_else(|e| panic!("[{name}] native decode: {e:?}"));

        assert_eq!(
            frame.timestamp_ms, 1234,
            "[{name}] decoder must carry the sample timestamp forward"
        );
        assert_eq!(
            (frame.width as usize, frame.height as usize),
            (width, height),
            "[{name}] cropped luma dimensions"
        );
        assert_eq!(
            (frame.width as usize) * (frame.height as usize),
            frame.pixels.len(),
            "[{name}] grayscale buffer length must equal width*height"
        );
        assert_eq!(
            frame.pixels.len(),
            reference.len(),
            "[{name}] grayscale size {} != reference {}",
            frame.pixels.len(),
            reference.len()
        );

        if let Some((i, (&a, &b))) = frame
            .pixels
            .iter()
            .zip(reference.iter())
            .enumerate()
            .find(|(_, (a, b))| a != b)
        {
            let (x, y) = (i % frame.width as usize, i / frame.width as usize);
            let diffs = frame
                .pixels
                .iter()
                .zip(reference.iter())
                .filter(|(a, b)| a != b)
                .count();
            panic!(
                "[{name}] native SparseDecoder differs from ffmpeg gray: {diffs}/{} samples; \
                 first at (x={x}, y={y}) native={a} ref={b}",
                frame.pixels.len(),
            );
        }
    }
}
