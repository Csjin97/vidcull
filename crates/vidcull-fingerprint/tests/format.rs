use vidcull_core::{Codec, VideoDuration};
use vidcull_fingerprint::format::{
    self, FORMAT_VERSION, Fingerprint, HEADER_LEN, MAGIC, PayloadKind,
};
use vidcull_fingerprint::tier1::{Tier1Fingerprint, build_tier1};
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};

fn sample_tier1() -> Tier1Fingerprint {
    build_tier1(
        VideoDuration::from_millis(1_480),
        Codec::H264,
        &[500, 500, 480],
        &[],
    )
}

fn sample_tier2() -> Tier2Fingerprint {
    Tier2Fingerprint {
        scenes: vec![
            SceneHash {
                timestamp_ms: 0,
                phash: 0xDEAD_BEEF_0000_0001,
            },
            SceneHash {
                timestamp_ms: 500,
                phash: 0x0102_0304_0506_0708,
            },
        ],
    }
}

#[test]
fn tier1_round_trips_through_envelope() {
    let fp = sample_tier1();
    let bytes = format::encode_tier1(&fp).unwrap();
    let decoded = format::decode_tier1(&bytes).unwrap();
    assert_eq!(fp, decoded);
}

#[test]
fn tier2_round_trips_through_envelope() {
    let fp = sample_tier2();
    let bytes = format::encode_tier2(&fp).unwrap();
    let decoded = format::decode_tier2(&bytes).unwrap();
    assert_eq!(fp, decoded);
}

#[test]
fn header_carries_magic_version_and_kind() {
    let bytes = format::encode_tier1(&sample_tier1()).unwrap();
    assert!(bytes.len() >= HEADER_LEN);
    assert_eq!(&bytes[0..4], &MAGIC);
    assert_eq!(bytes[4], FORMAT_VERSION);
    assert_eq!(bytes[5], PayloadKind::Tier1Global as u8);

    let bytes2 = format::encode_tier2(&sample_tier2()).unwrap();
    assert_eq!(bytes2[5], PayloadKind::Tier2Temporal as u8);
}

#[test]
fn envelope_overhead_is_exactly_the_header() {
    let fp = sample_tier1();
    let payload = fp.to_bytes().unwrap();
    let enveloped = format::encode_tier1(&fp).unwrap();
    assert_eq!(enveloped.len(), HEADER_LEN + payload.len());
    assert_eq!(&enveloped[HEADER_LEN..], payload.as_slice());
}

#[test]
fn peek_header_reports_kind_and_version_without_decoding() {
    let bytes = format::encode_tier2(&sample_tier2()).unwrap();
    let header = format::peek_header(&bytes).unwrap();
    assert_eq!(header.version, FORMAT_VERSION);
    assert_eq!(header.kind, PayloadKind::Tier2Temporal);
}

#[test]
fn generic_decode_dispatches_to_the_right_variant() {
    let t1 = format::encode_tier1(&sample_tier1()).unwrap();
    let t2 = format::encode_tier2(&sample_tier2()).unwrap();

    match format::decode(&t1).unwrap() {
        Fingerprint::Tier1(fp) => assert_eq!(fp, sample_tier1()),
        Fingerprint::Tier2(_) => panic!("tier1 blob decoded as tier2"),
    }
    match format::decode(&t2).unwrap() {
        Fingerprint::Tier2(fp) => assert_eq!(fp, sample_tier2()),
        Fingerprint::Tier1(_) => panic!("tier2 blob decoded as tier1"),
    }
}

#[test]
fn typed_decode_rejects_the_other_kind() {
    let t2 = format::encode_tier2(&sample_tier2()).unwrap();
    assert!(format::decode_tier1(&t2).is_err());

    let t1 = format::encode_tier1(&sample_tier1()).unwrap();
    assert!(format::decode_tier2(&t1).is_err());
}

#[test]
fn rejects_bad_magic() {
    let mut bytes = format::encode_tier1(&sample_tier1()).unwrap();
    bytes[0] ^= 0xFF;
    assert!(format::decode(&bytes).is_err());
    assert!(format::peek_header(&bytes).is_err());
}

#[test]
fn rejects_blob_shorter_than_header() {
    for n in 0..HEADER_LEN {
        let short = vec![0u8; n];
        assert!(
            format::peek_header(&short).is_err(),
            "len {n} should be too short for a header"
        );
        assert!(format::decode(&short).is_err());
    }
}

#[test]
fn rejects_future_format_version() {
    let mut bytes = format::encode_tier1(&sample_tier1()).unwrap();
    bytes[4] = FORMAT_VERSION + 1;
    let err = format::decode(&bytes).expect_err("future version must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains(&(FORMAT_VERSION + 1).to_string()),
        "error should name the unsupported version: {msg}"
    );
    assert!(format::peek_header(&bytes).is_err());
}

#[test]
fn rejects_zero_version() {
    let mut bytes = format::encode_tier1(&sample_tier1()).unwrap();
    bytes[4] = 0;
    assert!(format::decode(&bytes).is_err());
}

#[test]
fn rejects_unknown_payload_kind() {
    let mut bytes = format::encode_tier1(&sample_tier1()).unwrap();
    bytes[5] = 0xEE;
    assert!(format::decode(&bytes).is_err());
    assert!(format::peek_header(&bytes).is_err());
}

#[test]
fn corrupt_payload_surfaces_an_error() {
    let mut bytes = format::encode_tier2(&sample_tier2()).unwrap();
    bytes.truncate(HEADER_LEN + 1);
    assert!(format::decode_tier2(&bytes).is_err());
}

#[test]
fn golden_empty_tier1_bytes() {
    let fp = build_tier1(VideoDuration::from_millis(0), Codec::H264, &[], &[]);
    let bytes = format::encode_tier1(&fp).unwrap();
    let expected = [
        0x41, 0x56, 0x53, 0x46, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    assert_eq!(bytes, expected);
    assert_eq!(format::decode_tier1(&bytes).unwrap(), fp);
}

#[test]
fn golden_empty_tier2_bytes() {
    let fp = Tier2Fingerprint::default();
    let bytes = format::encode_tier2(&fp).unwrap();
    let expected = [0x41, 0x56, 0x53, 0x46, 0x01, 0x02, 0x00];
    assert_eq!(bytes, expected);
    assert_eq!(format::decode_tier2(&bytes).unwrap(), fp);
}
