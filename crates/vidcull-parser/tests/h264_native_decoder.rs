use vidcull_core::types::Codec;

use vidcull_parser::h264::NativeH264Decoder;
use vidcull_parser::h264::nal::{NalUnit, NalUnitType, split_annex_b};
use vidcull_parser::sparse::{SparseDecoder, SparseSample};

use std::path::{Path, PathBuf};

const FIXTURES: &[&str] = &["grad_48x32", "testsrc_64x64", "crop_160x90", "bars_176x144"];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264-conformance")
}

fn raw_nal(u: &NalUnit) -> Vec<u8> {
    let type_code = match u.header.unit_type {
        NalUnitType::NonIdrSlice => 1,
        NalUnitType::IdrSlice => 5,
        NalUnitType::Sei => 6,
        NalUnitType::Sps => 7,
        NalUnitType::Pps => 8,
        NalUnitType::AccessUnitDelimiter => 9,
        NalUnitType::Other(v) => v,
    };
    let header_byte = (u.header.ref_idc << 5) | type_code;
    let mut bytes = vec![header_byte];
    bytes.extend_from_slice(&u.payload);
    bytes
}

fn build_avcc(sps_nal: &[u8], pps_nal: &[u8]) -> Vec<u8> {
    let mut v = vec![1, sps_nal[1], sps_nal[2], sps_nal[3], 0xFF, 0xE1];
    v.extend_from_slice(
        &u16::try_from(sps_nal.len())
            .expect("SPS fits u16")
            .to_be_bytes(),
    );
    v.extend_from_slice(sps_nal);
    v.push(1);
    v.extend_from_slice(
        &u16::try_from(pps_nal.len())
            .expect("PPS fits u16")
            .to_be_bytes(),
    );
    v.extend_from_slice(pps_nal);
    v
}

fn build_avcc_sample(idr_nals: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in idr_nals {
        let len = u32::try_from(nal.len()).expect("NAL fits u32");
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.extend_from_slice(nal);
    }
    bytes
}

fn repackage_as_avcc(h264: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let units = split_annex_b(h264);

    let mut sps_nal = None;
    let mut pps_nal = None;
    let mut idr_nals = Vec::new();
    for u in &units {
        match u.header.unit_type {
            NalUnitType::Sps => sps_nal = Some(raw_nal(u)),
            NalUnitType::Pps => pps_nal = Some(raw_nal(u)),
            NalUnitType::IdrSlice => idr_nals.push(raw_nal(u)),
            _ => {}
        }
    }

    let sps_nal = sps_nal.expect("fixture carries an SPS");
    let pps_nal = pps_nal.expect("fixture carries a PPS");
    assert!(
        !idr_nals.is_empty(),
        "fixture carries at least one IDR slice"
    );

    (build_avcc(&sps_nal, &pps_nal), build_avcc_sample(&idr_nals))
}

#[test]
fn native_sparse_decoder_is_bit_exact_vs_committed_reference() {
    for &name in FIXTURES {
        let dir = fixture_dir();
        let h264 = std::fs::read(dir.join(format!("{name}.h264")))
            .unwrap_or_else(|e| panic!("read {name}.h264: {e}"));
        let reference = std::fs::read(dir.join(format!("{name}.gray8")))
            .unwrap_or_else(|e| panic!("read {name}.gray8: {e}"));

        let (avcc, sample_bytes) = repackage_as_avcc(&h264);
        let mut decoder = NativeH264Decoder::from_avcc(&avcc)
            .unwrap_or_else(|e| panic!("[{name}] build decoder from avcC: {e:?}"));
        let sample = SparseSample {
            timestamp_ms: 1234,
            bytes: sample_bytes,
        };
        let frame = decoder
            .decode_idr(&sample, &Codec::H264)
            .unwrap_or_else(|e| panic!("[{name}] native decode: {e:?}"));

        assert_eq!(
            frame.timestamp_ms, 1234,
            "[{name}] decoder must carry the sample timestamp forward"
        );
        assert_eq!(
            (frame.width as usize) * (frame.height as usize),
            frame.pixels.len(),
            "[{name}] grayscale buffer length must equal width*height"
        );
        assert_eq!(
            frame.pixels.len(),
            reference.len(),
            "[{name}] grayscale size {} != reference {} ({}x{})",
            frame.pixels.len(),
            reference.len(),
            frame.width,
            frame.height
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
                "[{name}] native SparseDecoder differs from ffmpeg: {diffs}/{} samples; \
                 first at (x={x}, y={y}) native={a} ref={b}",
                frame.pixels.len(),
            );
        }
    }
}
