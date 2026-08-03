use std::io::{Read, Seek, SeekFrom};

use vidcull_core::{Error, Result};

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum KeepLengthMarker {
    Keep,
    Strip,
}

pub(crate) fn read_element_header<R: Read>(reader: &mut R) -> Result<(u32, u64)> {
    let id = read_vint(reader, KeepLengthMarker::Keep)?;
    let id_u32 = u32::try_from(id)
        .map_err(|_| Error::Parse(format!("ebml: element ID 0x{id:X} exceeds u32 range")))?;
    let size = read_vint(reader, KeepLengthMarker::Strip)?;
    Ok((id_u32, size))
}

pub(crate) fn read_vint<R: Read>(reader: &mut R, marker: KeepLengthMarker) -> Result<u64> {
    let mut first = [0u8; 1];
    reader.read_exact(&mut first)?;
    let b = first[0];
    if b == 0 {
        return Err(Error::Parse("ebml: VINT first byte is zero".into()));
    }
    let length = (b.leading_zeros() as usize) + 1;
    if length > 8 {
        return Err(Error::Parse(format!(
            "ebml: VINT length {length} exceeds 8 bytes"
        )));
    }
    let mut value = match marker {
        KeepLengthMarker::Keep => u64::from(b),
        KeepLengthMarker::Strip => {
            let mask = if length >= 8 { 0u8 } else { 0xFFu8 >> length };
            u64::from(b & mask)
        }
    };
    if length > 1 {
        let mut rest = [0u8; 7];
        let n = length - 1;
        reader.read_exact(&mut rest[..n])?;
        for byte in &rest[..n] {
            value = (value << 8) | u64::from(*byte);
        }
    }
    Ok(value)
}

pub(crate) fn read_uint<R: Read>(reader: &mut R, size: u64) -> Result<u64> {
    if size > 8 {
        return Err(Error::Parse(format!(
            "ebml: uint size {size} exceeds 8 bytes"
        )));
    }
    #[allow(clippy::cast_possible_truncation)]
    let n = size as usize;
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf[..n])?;
    let mut value = 0u64;
    for byte in &buf[..n] {
        value = (value << 8) | u64::from(*byte);
    }
    Ok(value)
}

pub(crate) fn skip_bytes<R: Seek>(reader: &mut R, size: u64) -> Result<()> {
    if size == u64::MAX {
        return Err(Error::Parse(
            "ebml: cannot skip element with unknown size".into(),
        ));
    }
    let offset = i64::try_from(size)
        .map_err(|_| Error::Parse(format!("ebml: element size {size} exceeds seekable range")))?;
    reader.seek(SeekFrom::Current(offset))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn vint_decodes_single_byte_with_marker() {
        let mut r = Cursor::new(vec![0xB3]);
        assert_eq!(read_vint(&mut r, KeepLengthMarker::Keep).unwrap(), 0xB3);
    }

    #[test]
    fn vint_decodes_single_byte_size_strips_marker() {
        let mut r = Cursor::new(vec![0x82]);
        assert_eq!(read_vint(&mut r, KeepLengthMarker::Strip).unwrap(), 2);
    }

    #[test]
    fn vint_decodes_four_byte_id_matching_cues_signature() {
        let mut r = Cursor::new(vec![0x1C, 0x53, 0xBB, 0x6B]);
        assert_eq!(
            read_vint(&mut r, KeepLengthMarker::Keep).unwrap(),
            0x1C53_BB6B
        );
    }

    #[test]
    fn vint_rejects_zero_first_byte() {
        let mut r = Cursor::new(vec![0x00, 0x01]);
        assert!(read_vint(&mut r, KeepLengthMarker::Keep).is_err());
    }

    #[test]
    fn vint_decodes_eight_byte_size_without_shift_overflow() {
        let mut r = Cursor::new(vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x10]);
        assert_eq!(read_vint(&mut r, KeepLengthMarker::Strip).unwrap(), 2576);
    }

    #[test]
    fn vint_decodes_eight_byte_unknown_size_sentinel() {
        let mut r = Cursor::new(vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(
            read_vint(&mut r, KeepLengthMarker::Strip).unwrap(),
            0x00FF_FFFF_FFFF_FFFF
        );
    }

    #[test]
    fn read_uint_handles_one_to_eight_bytes() {
        let mut r = Cursor::new(vec![0x12, 0x34, 0x56, 0x78]);
        assert_eq!(read_uint(&mut r, 1).unwrap(), 0x12);
        assert_eq!(read_uint(&mut r, 2).unwrap(), 0x3456);
        assert_eq!(read_uint(&mut r, 1).unwrap(), 0x78);
    }

    #[test]
    fn read_uint_rejects_oversize() {
        let mut r = Cursor::new(vec![0u8; 16]);
        assert!(read_uint(&mut r, 9).is_err());
    }

    #[test]
    fn skip_bytes_refuses_unknown_size() {
        let mut r = Cursor::new(vec![0u8; 16]);
        assert!(skip_bytes(&mut r, u64::MAX).is_err());
    }
}
