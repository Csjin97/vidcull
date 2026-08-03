use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(value).map_err(Error::from)
}

pub fn encode_into<'a, T: Serialize>(value: &T, buf: &'a mut [u8]) -> Result<&'a mut [u8]> {
    postcard::to_slice(value, buf).map_err(Error::from)
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    postcard::from_bytes(bytes).map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_primitive() {
        let bytes = encode(&123_u32).expect("encode");
        let decoded: u32 = decode(&bytes).expect("decode");
        assert_eq!(decoded, 123);
    }

    #[test]
    fn round_trip_tuple_struct() {
        let v = (1_u8, -2_i16, "hello".to_string());
        let bytes = encode(&v).expect("encode");
        let decoded: (u8, i16, String) = decode(&bytes).expect("decode");
        assert_eq!(decoded, v);
    }

    #[test]
    fn decode_propagates_truncation_as_serialization_error() {
        let mut bytes = encode(&u32::MAX).expect("encode");
        bytes.truncate(2);
        let err = decode::<u32>(&bytes).expect_err("truncated decode must fail");
        match err {
            Error::Serialization(_) => {}
            other => panic!("expected Serialization variant, got {other:?}"),
        }
    }

    #[test]
    fn encode_into_writes_into_caller_buffer() {
        let mut buf = [0u8; 16];
        let written = encode_into(&7_u32, &mut buf).expect("encode_into");
        assert_eq!(written, [7u8].as_slice());
    }

    #[test]
    fn encode_into_reports_overflow_via_serialization_error() {
        let mut tiny = [0u8; 1];
        let err = encode_into(&u64::MAX, &mut tiny).expect_err("buffer too small must fail");
        match err {
            Error::Serialization(_) => {}
            other => panic!("expected Serialization variant, got {other:?}"),
        }
    }
}
