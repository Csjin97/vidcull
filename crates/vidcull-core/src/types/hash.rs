use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const HASH_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Blake3Hash([u8; HASH_LEN]);

impl Blake3Hash {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(HASH_LEN * 2);
        for byte in self.0 {
            out.push(hex_nibble(byte >> 4));
            out.push(hex_nibble(byte & 0x0f));
        }
        out
    }

    pub fn from_hex(input: &str) -> Result<Self> {
        if input.len() != HASH_LEN * 2 {
            return Err(Error::InvalidHash(format!(
                "expected {} hex chars, got {}",
                HASH_LEN * 2,
                input.len()
            )));
        }
        let bytes = input.as_bytes();
        let mut out = [0u8; HASH_LEN];
        for i in 0..HASH_LEN {
            let hi = decode_nibble(bytes[i * 2])?;
            let lo = decode_nibble(bytes[i * 2 + 1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

const fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => '?',
    }
}

fn decode_nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(Error::InvalidHash(format!(
            "non-hex character: {:?}",
            other as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Blake3Hash {
        let mut bytes = [0u8; HASH_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::try_from(i).expect("0..32 fits in u8");
        }
        Blake3Hash::from_bytes(bytes)
    }

    #[test]
    fn hex_round_trip_lowercase() {
        let h = sample();
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        let parsed = Blake3Hash::from_hex(&hex).expect("parse");
        assert_eq!(h, parsed);
    }

    #[test]
    fn from_hex_accepts_uppercase() {
        let h = sample();
        let upper = h.to_hex().to_uppercase();
        let parsed = Blake3Hash::from_hex(&upper).expect("parse uppercase");
        assert_eq!(h, parsed);
    }

    #[test]
    fn from_hex_rejects_short_input() {
        let err = Blake3Hash::from_hex("abc").expect_err("short input must fail");
        match err {
            Error::InvalidHash(msg) => {
                assert!(
                    msg.contains("64"),
                    "msg should mention expected length: {msg}"
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_hex_rejects_non_hex_chars() {
        let bad = "zz".repeat(32);
        let err = Blake3Hash::from_hex(&bad).expect_err("non-hex must fail");
        match err {
            Error::InvalidHash(msg) => assert!(msg.contains("non-hex"), "{msg}"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn postcard_round_trip_is_fixed_32_bytes() {
        let h = sample();
        let bytes = postcard::to_allocvec(&h).expect("encode");
        assert_eq!(
            bytes.len(),
            HASH_LEN,
            "serde(transparent) on [u8; 32] must serialize as 32 raw bytes"
        );
        let decoded: Blake3Hash = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(h, decoded);
    }
}
