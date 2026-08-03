use serde::{Deserialize, Serialize};
use vidcull_core::{
    Blake3Hash, Codec, FileId, NormalizedPath, Resolution, VideoDuration, decode, encode,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileRecord {
    id: FileId,
    path: NormalizedPath,
    content_hash: Blake3Hash,
    duration: VideoDuration,
    resolution: Resolution,
    codec: Codec,
}

fn sample_record() -> FileRecord {
    let mut hash_bytes = [0u8; 32];
    for (i, b) in hash_bytes.iter_mut().enumerate() {
        *b = u8::try_from(i * 7 % 251).expect("fits in u8");
    }
    FileRecord {
        id: FileId(987_654),
        path: NormalizedPath::new(r"D:\library\holiday\beach.mp4"),
        content_hash: Blake3Hash::from_bytes(hash_bytes),
        duration: VideoDuration::from_secs_f64(125.5),
        resolution: Resolution::new(3840, 2160),
        codec: Codec::H265,
    }
}

#[test]
fn composite_record_round_trips_through_public_api() {
    let original = sample_record();
    let bytes = encode(&original).expect("encode");
    let decoded: FileRecord = decode(&bytes).expect("decode");
    assert_eq!(decoded, original);
}

#[test]
fn composite_record_encoding_is_deterministic() {
    let a = encode(&sample_record()).expect("encode a");
    let b = encode(&sample_record()).expect("encode b");
    assert_eq!(
        a, b,
        "postcard output must be byte-identical for equal inputs"
    );
}

#[test]
fn fallback_codec_round_trips_with_label_preserved() {
    let mut record = sample_record();
    record.codec = Codec::Other("prores_hq".into());
    let bytes = encode(&record).expect("encode");
    let decoded: FileRecord = decode(&bytes).expect("decode");
    assert_eq!(decoded.codec, Codec::Other("prores_hq".into()));
}

#[test]
fn decode_rejects_truncated_payload() {
    let bytes = encode(&sample_record()).expect("encode");
    let truncated = &bytes[..bytes.len() / 2];
    let err = decode::<FileRecord>(truncated).expect_err("truncated payload must fail");
    let rendered = err.to_string();
    assert!(
        rendered.contains("serialization") || rendered.contains("Serialization"),
        "expected a serialization-category error, got: {rendered}"
    );
}
