use std::fs::File;
use std::io::Read;
use std::path::Path;

use vidcull_core::Result;
use vidcull_core::types::{Blake3Hash, HASH_LEN};

pub const CHUNK_SIZE: usize = 64 * 1024;

pub fn hash_file(path: &Path) -> Result<Blake3Hash> {
    hash_file_cancellable(path, || false)
}

pub fn hash_file_cancellable(path: &Path, should_cancel: impl Fn() -> bool) -> Result<Blake3Hash> {
    let file = File::open(path)?;
    hash_reader_cancellable(file, should_cancel)
}

pub fn hash_reader<R: Read>(reader: R) -> Result<Blake3Hash> {
    hash_reader_cancellable(reader, || false)
}

pub fn hash_reader_cancellable<R: Read>(
    mut reader: R,
    should_cancel: impl Fn() -> bool,
) -> Result<Blake3Hash> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        if should_cancel() {
            return Err(vidcull_core::Error::Cancelled);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest: [u8; HASH_LEN] = *hasher.finalize().as_bytes();
    Ok(Blake3Hash::from_bytes(digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_matches_blake3_reference_digest() {
        let h = hash_reader(std::io::empty()).expect("empty hash");
        let reference = blake3::hash(b"");
        assert_eq!(h.as_bytes(), reference.as_bytes());
    }

    #[test]
    fn single_chunk_matches_one_shot_blake3() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let streamed = hash_reader(&data[..]).expect("hash");
        let reference = blake3::hash(&data);
        assert_eq!(streamed.as_bytes(), reference.as_bytes());
    }

    #[test]
    fn multi_chunk_matches_one_shot_blake3() {
        let data: Vec<u8> = (0..(CHUNK_SIZE * 3 + 17))
            .map(|i| u8::try_from(i % 251).expect("0..251 fits in u8"))
            .collect();
        let streamed = hash_reader(&data[..]).expect("hash");
        let reference = blake3::hash(&data);
        assert_eq!(streamed.as_bytes(), reference.as_bytes());
    }

    #[test]
    fn cancellable_aborts_before_first_chunk_when_cancel_is_preset() {
        let data = vec![0u8; CHUNK_SIZE + 1];
        let result = hash_reader_cancellable(&data[..], || true);
        assert!(
            matches!(result, Err(vidcull_core::Error::Cancelled)),
            "pre-set should_cancel must return Err(Cancelled), got {result:?}",
        );
    }

    #[test]
    fn cancellable_with_false_matches_hash_reader() {
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 13))
            .map(|i| u8::try_from(i % 251).expect("fits"))
            .collect();
        let expected = hash_reader(&data[..]).expect("hash_reader");
        let got = hash_reader_cancellable(&data[..], || false)
            .expect("hash_reader_cancellable(|| false)");
        assert_eq!(
            expected.as_bytes(),
            got.as_bytes(),
            "|| false must produce identical digest to hash_reader",
        );
    }
}
