use std::io::{Read, Seek, SeekFrom};

use vidcull_core::{Error, Result};

pub(crate) const MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) const READ_BUF_CAPACITY: usize = 1 << 20;

pub(crate) fn read_exact_bounded<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    ctx: &str,
) -> Result<Vec<u8>> {
    if len > MAX_ALLOC_BYTES {
        return Err(Error::Parse(format!(
            "{ctx}: declared size {len} exceeds the {MAX_ALLOC_BYTES}-byte allocation cap"
        )));
    }
    let pos = reader.stream_position()?;
    let end = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(pos))?;
    let remaining = end.saturating_sub(pos);
    if len > remaining {
        return Err(Error::Parse(format!(
            "{ctx}: declared size {len} exceeds {remaining} bytes remaining in file"
        )));
    }
    let len_usize = usize::try_from(len)
        .map_err(|_| Error::Parse(format!("{ctx}: size {len} exceeds usize")))?;
    let mut buf = vec![0u8; len_usize];
    reader
        .read_exact(&mut buf)
        .map_err(|_| Error::Parse(format!("{ctx}: truncated read of {len} bytes")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_exact_when_within_bounds() {
        let mut r = Cursor::new(vec![1u8, 2, 3, 4, 5]);
        let got = read_exact_bounded(&mut r, 3, "test").unwrap();
        assert_eq!(got, vec![1, 2, 3]);
        assert_eq!(r.stream_position().unwrap(), 3);
    }

    #[test]
    fn rejects_length_beyond_remaining_without_allocating() {
        let mut r = Cursor::new(vec![0u8; 5]);
        let err = read_exact_bounded(&mut r, 4 * 1024 * 1024 * 1024, "test").unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
        assert_eq!(r.stream_position().unwrap(), 0);
    }

    #[test]
    fn rejects_length_above_absolute_cap() {
        let mut r = Cursor::new(vec![0u8; 8]);
        let err = read_exact_bounded(&mut r, MAX_ALLOC_BYTES + 1, "test").unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn allows_full_remaining_length() {
        let mut r = Cursor::new(vec![7u8; 4]);
        let got = read_exact_bounded(&mut r, 4, "test").unwrap();
        assert_eq!(got, vec![7, 7, 7, 7]);
    }
}
