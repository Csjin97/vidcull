use vidcull_core::{Error, Result};

use super::bitstream::BitReader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidualBlock {
    pub coeffs: [i32; 16],
    pub total_coeff: usize,
}

pub fn residual_block(
    reader: &mut BitReader,
    max_num_coeff: usize,
    nc: i32,
) -> Result<ResidualBlock> {
    let (total_coeff, trailing_ones) = read_coeff_token(reader, nc)?;

    let mut coeffs = [0i32; 16];
    if total_coeff == 0 {
        return Ok(ResidualBlock {
            coeffs,
            total_coeff: 0,
        });
    }

    let level = read_levels(reader, total_coeff, trailing_ones)?;
    let total_zeros = read_total_zeros(reader, total_coeff, max_num_coeff)?;
    place_coefficients(&mut coeffs, &level, total_zeros, reader)?;

    Ok(ResidualBlock {
        coeffs,
        total_coeff,
    })
}

fn read_levels(
    reader: &mut BitReader,
    total_coeff: usize,
    trailing_ones: usize,
) -> Result<Vec<i32>> {
    let mut level = Vec::with_capacity(total_coeff);

    for _ in 0..trailing_ones {
        let sign = reader.read_bit()?;
        level.push(if sign == 0 { 1 } else { -1 });
    }

    let mut suffix_length: u32 = u32::from(total_coeff > 10 && trailing_ones < 3);

    for i in trailing_ones..total_coeff {
        let level_prefix = read_level_prefix(reader)?;

        let level_suffix_size: u32 = if level_prefix == 14 && suffix_length == 0 {
            4
        } else if level_prefix >= 15 {
            level_prefix - 3
        } else {
            suffix_length
        };

        let level_suffix = if level_suffix_size > 0 {
            reader.read_bits(level_suffix_size)?
        } else {
            0
        };

        let mut level_code = (level_prefix.min(15) << suffix_length) + level_suffix;

        if level_prefix >= 15 && suffix_length == 0 {
            level_code += 15;
        }
        if level_prefix >= 16 {
            level_code += (1 << (level_prefix - 3)) - 4096;
        }

        if i == trailing_ones && trailing_ones < 3 {
            level_code += 2;
        }

        let signed = if level_code % 2 == 0 {
            i32::try_from((level_code + 2) >> 1)
                .map_err(|_| Error::Parse("h264 cavlc: level overflow".into()))?
        } else {
            -i32::try_from((level_code + 1) >> 1)
                .map_err(|_| Error::Parse("h264 cavlc: level overflow".into()))?
        };
        level.push(signed);

        if suffix_length == 0 {
            suffix_length = 1;
        }
        let magnitude = signed.unsigned_abs();
        if suffix_length < 6 && magnitude > (3u32 << (suffix_length - 1)) {
            suffix_length += 1;
        }
    }

    Ok(level)
}

fn read_level_prefix(reader: &mut BitReader) -> Result<u32> {
    let mut prefix = 0u32;
    while reader.read_bit()? == 0 {
        prefix += 1;
        if prefix > 63 {
            return Err(Error::Parse("h264 cavlc: level_prefix too long".into()));
        }
    }
    Ok(prefix)
}

fn place_coefficients(
    coeffs: &mut [i32; 16],
    level: &[i32],
    total_zeros: usize,
    reader: &mut BitReader,
) -> Result<()> {
    let total_coeff = level.len();

    let mut run_val = vec![0usize; total_coeff];
    let mut zeros_left = total_zeros;
    for run_slot in run_val.iter_mut().take(total_coeff - 1) {
        if zeros_left == 0 {
            break;
        }
        let run_before = read_run_before(reader, zeros_left)?;
        *run_slot = run_before;
        zeros_left = zeros_left
            .checked_sub(run_before)
            .ok_or_else(|| Error::Parse("h264 cavlc: run_before exceeds zeros_left".into()))?;
    }
    run_val[total_coeff - 1] = zeros_left;

    let mut coeff_num: isize = -1;
    for i in (0..total_coeff).rev() {
        coeff_num += isize::try_from(run_val[i] + 1)
            .map_err(|_| Error::Parse("h264 cavlc: run_before overflow".into()))?;
        let idx = usize::try_from(coeff_num)
            .map_err(|_| Error::Parse("h264 cavlc: coeffNum out of range".into()))?;
        if idx >= 16 {
            return Err(Error::Parse("h264 cavlc: coeffNum exceeds block".into()));
        }
        coeffs[idx] = level[i];
    }

    Ok(())
}

struct CoeffTokenRow {
    code: u32,
    len: u32,
    trailing_ones: u8,
    total_coeff: u8,
}

fn read_coeff_token(reader: &mut BitReader, nc: i32) -> Result<(usize, usize)> {
    if (0..8).contains(&nc) {
        let table = if nc < 2 {
            &COEFF_TOKEN_0_2[..]
        } else if nc < 4 {
            &COEFF_TOKEN_2_4[..]
        } else {
            &COEFF_TOKEN_4_8[..]
        };
        return match_coeff_token(reader, table);
    }
    if nc >= 8 {
        return read_coeff_token_fixed(reader);
    }
    let table = if nc == -1 {
        &COEFF_TOKEN_CHROMA_DC_420[..]
    } else {
        &COEFF_TOKEN_CHROMA_DC_422[..]
    };
    match_coeff_token(reader, table)
}

fn match_coeff_token(reader: &mut BitReader, table: &[CoeffTokenRow]) -> Result<(usize, usize)> {
    let mut code: u32 = 0;
    let mut len: u32 = 0;
    loop {
        code = (code << 1) | reader.read_bit()?;
        len += 1;
        for row in table {
            if row.len == len && row.code == code {
                return Ok((row.total_coeff as usize, row.trailing_ones as usize));
            }
        }
        if len > 16 {
            return Err(Error::Parse("h264 cavlc: invalid coeff_token".into()));
        }
    }
}

fn read_coeff_token_fixed(reader: &mut BitReader) -> Result<(usize, usize)> {
    let bits = reader.read_bits(6)?;
    if bits == 0b00_0011 {
        return Ok((0, 0));
    }
    let total_coeff = ((bits >> 2) + 1) as usize;
    let trailing_ones = (bits & 0b11) as usize;
    if trailing_ones > total_coeff || total_coeff > 16 {
        return Err(Error::Parse("h264 cavlc: invalid fixed coeff_token".into()));
    }
    Ok((total_coeff, trailing_ones))
}

fn read_total_zeros(
    reader: &mut BitReader,
    total_coeff: usize,
    max_num_coeff: usize,
) -> Result<usize> {
    let max_total = max_num_coeff.saturating_sub(total_coeff);
    if max_total == 0 {
        return Ok(0);
    }
    let table: &[(u32, u32, u8)] = match max_num_coeff {
        4 => TOTAL_ZEROS_CHROMA_DC_2X2[total_coeff - 1],
        8 => TOTAL_ZEROS_CHROMA_DC_2X4[total_coeff - 1],
        _ => TOTAL_ZEROS_4X4[total_coeff - 1],
    };
    match_value_table(reader, table)
}

fn match_value_table(reader: &mut BitReader, table: &[(u32, u32, u8)]) -> Result<usize> {
    let mut code: u32 = 0;
    let mut len: u32 = 0;
    loop {
        code = (code << 1) | reader.read_bit()?;
        len += 1;
        for &(rcode, rlen, val) in table {
            if rlen == len && rcode == code {
                return Ok(val as usize);
            }
        }
        if len > 12 {
            return Err(Error::Parse("h264 cavlc: invalid total_zeros".into()));
        }
    }
}

fn read_run_before(reader: &mut BitReader, zeros_left: usize) -> Result<usize> {
    let idx = zeros_left.min(7) - 1;
    let table = RUN_BEFORE[idx];
    match_value_table(reader, table)
}

macro_rules! ct {
    ($code:expr, $len:expr, $t1:expr, $tc:expr) => {
        CoeffTokenRow {
            code: $code,
            len: $len,
            trailing_ones: $t1,
            total_coeff: $tc,
        }
    };
}

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static COEFF_TOKEN_0_2: [CoeffTokenRow; 62] = [
    ct!(0b1, 1, 0, 0),
    ct!(0b000101, 6, 0, 1),
    ct!(0b01, 2, 1, 1),
    ct!(0b00000111, 8, 0, 2),
    ct!(0b000100, 6, 1, 2),
    ct!(0b001, 3, 2, 2),
    ct!(0b000000111, 9, 0, 3),
    ct!(0b00000110, 8, 1, 3),
    ct!(0b0000101, 7, 2, 3),
    ct!(0b00011, 5, 3, 3),
    ct!(0b0000000111, 10, 0, 4),
    ct!(0b000000110, 9, 1, 4),
    ct!(0b00000101, 8, 2, 4),
    ct!(0b000011, 6, 3, 4),
    ct!(0b00000000111, 11, 0, 5),
    ct!(0b0000000110, 10, 1, 5),
    ct!(0b000000101, 9, 2, 5),
    ct!(0b0000100, 7, 3, 5),
    ct!(0b0000000001111, 13, 0, 6),
    ct!(0b00000000110, 11, 1, 6),
    ct!(0b0000000101, 10, 2, 6),
    ct!(0b00000100, 8, 3, 6),
    ct!(0b0000000001011, 13, 0, 7),
    ct!(0b0000000001110, 13, 1, 7),
    ct!(0b00000000101, 11, 2, 7),
    ct!(0b000000100, 9, 3, 7),
    ct!(0b0000000001000, 13, 0, 8),
    ct!(0b0000000001010, 13, 1, 8),
    ct!(0b0000000001101, 13, 2, 8),
    ct!(0b0000000100, 10, 3, 8),
    ct!(0b00000000001111, 14, 0, 9),
    ct!(0b00000000001110, 14, 1, 9),
    ct!(0b0000000001001, 13, 2, 9),
    ct!(0b00000000100, 11, 3, 9),
    ct!(0b00000000001011, 14, 0, 10),
    ct!(0b00000000001010, 14, 1, 10),
    ct!(0b00000000001101, 14, 2, 10),
    ct!(0b0000000001100, 13, 3, 10),
    ct!(0b000000000001111, 15, 0, 11),
    ct!(0b000000000001110, 15, 1, 11),
    ct!(0b00000000001001, 14, 2, 11),
    ct!(0b00000000001100, 14, 3, 11),
    ct!(0b000000000001011, 15, 0, 12),
    ct!(0b000000000001010, 15, 1, 12),
    ct!(0b000000000001101, 15, 2, 12),
    ct!(0b00000000001000, 14, 3, 12),
    ct!(0b0000000000001111, 16, 0, 13),
    ct!(0b000000000000001, 15, 1, 13),
    ct!(0b000000000001001, 15, 2, 13),
    ct!(0b000000000001100, 15, 3, 13),
    ct!(0b0000000000001011, 16, 0, 14),
    ct!(0b0000000000001110, 16, 1, 14),
    ct!(0b0000000000001101, 16, 2, 14),
    ct!(0b000000000001000, 15, 3, 14),
    ct!(0b0000000000000111, 16, 0, 15),
    ct!(0b0000000000001010, 16, 1, 15),
    ct!(0b0000000000001001, 16, 2, 15),
    ct!(0b0000000000001100, 16, 3, 15),
    ct!(0b0000000000000100, 16, 0, 16),
    ct!(0b0000000000000110, 16, 1, 16),
    ct!(0b0000000000000101, 16, 2, 16),
    ct!(0b0000000000001000, 16, 3, 16),
];

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static COEFF_TOKEN_2_4: [CoeffTokenRow; 62] = [
    ct!(0b11, 2, 0, 0),
    ct!(0b001011, 6, 0, 1),
    ct!(0b10, 2, 1, 1),
    ct!(0b000111, 6, 0, 2),
    ct!(0b00111, 5, 1, 2),
    ct!(0b011, 3, 2, 2),
    ct!(0b0000111, 7, 0, 3),
    ct!(0b001010, 6, 1, 3),
    ct!(0b001001, 6, 2, 3),
    ct!(0b0101, 4, 3, 3),
    ct!(0b00000111, 8, 0, 4),
    ct!(0b000110, 6, 1, 4),
    ct!(0b000101, 6, 2, 4),
    ct!(0b0100, 4, 3, 4),
    ct!(0b00000100, 8, 0, 5),
    ct!(0b0000110, 7, 1, 5),
    ct!(0b0000101, 7, 2, 5),
    ct!(0b00110, 5, 3, 5),
    ct!(0b000000111, 9, 0, 6),
    ct!(0b00000110, 8, 1, 6),
    ct!(0b00000101, 8, 2, 6),
    ct!(0b001000, 6, 3, 6),
    ct!(0b00000001111, 11, 0, 7),
    ct!(0b000000110, 9, 1, 7),
    ct!(0b000000101, 9, 2, 7),
    ct!(0b000100, 6, 3, 7),
    ct!(0b00000001011, 11, 0, 8),
    ct!(0b00000001110, 11, 1, 8),
    ct!(0b00000001101, 11, 2, 8),
    ct!(0b0000100, 7, 3, 8),
    ct!(0b000000001111, 12, 0, 9),
    ct!(0b00000001010, 11, 1, 9),
    ct!(0b00000001001, 11, 2, 9),
    ct!(0b000000100, 9, 3, 9),
    ct!(0b000000001011, 12, 0, 10),
    ct!(0b000000001110, 12, 1, 10),
    ct!(0b000000001101, 12, 2, 10),
    ct!(0b00000001100, 11, 3, 10),
    ct!(0b000000001000, 12, 0, 11),
    ct!(0b000000001010, 12, 1, 11),
    ct!(0b000000001001, 12, 2, 11),
    ct!(0b00000001000, 11, 3, 11),
    ct!(0b0000000001111, 13, 0, 12),
    ct!(0b0000000001110, 13, 1, 12),
    ct!(0b0000000001101, 13, 2, 12),
    ct!(0b000000001100, 12, 3, 12),
    ct!(0b0000000001011, 13, 0, 13),
    ct!(0b0000000001010, 13, 1, 13),
    ct!(0b0000000001001, 13, 2, 13),
    ct!(0b0000000001100, 13, 3, 13),
    ct!(0b0000000000111, 13, 0, 14),
    ct!(0b00000000001011, 14, 1, 14),
    ct!(0b0000000000110, 13, 2, 14),
    ct!(0b0000000001000, 13, 3, 14),
    ct!(0b00000000001001, 14, 0, 15),
    ct!(0b00000000001000, 14, 1, 15),
    ct!(0b00000000001010, 14, 2, 15),
    ct!(0b0000000000001, 13, 3, 15),
    ct!(0b00000000000111, 14, 0, 16),
    ct!(0b00000000000110, 14, 1, 16),
    ct!(0b00000000000101, 14, 2, 16),
    ct!(0b00000000000100, 14, 3, 16),
];

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static COEFF_TOKEN_4_8: [CoeffTokenRow; 62] = [
    ct!(0b1111, 4, 0, 0),
    ct!(0b001111, 6, 0, 1),
    ct!(0b1110, 4, 1, 1),
    ct!(0b001011, 6, 0, 2),
    ct!(0b01111, 5, 1, 2),
    ct!(0b1101, 4, 2, 2),
    ct!(0b001000, 6, 0, 3),
    ct!(0b01100, 5, 1, 3),
    ct!(0b01110, 5, 2, 3),
    ct!(0b1100, 4, 3, 3),
    ct!(0b0001111, 7, 0, 4),
    ct!(0b01010, 5, 1, 4),
    ct!(0b01011, 5, 2, 4),
    ct!(0b1011, 4, 3, 4),
    ct!(0b0001011, 7, 0, 5),
    ct!(0b01000, 5, 1, 5),
    ct!(0b01001, 5, 2, 5),
    ct!(0b1010, 4, 3, 5),
    ct!(0b0001001, 7, 0, 6),
    ct!(0b001110, 6, 1, 6),
    ct!(0b001101, 6, 2, 6),
    ct!(0b1001, 4, 3, 6),
    ct!(0b0001000, 7, 0, 7),
    ct!(0b001010, 6, 1, 7),
    ct!(0b001001, 6, 2, 7),
    ct!(0b1000, 4, 3, 7),
    ct!(0b00001111, 8, 0, 8),
    ct!(0b0001110, 7, 1, 8),
    ct!(0b0001101, 7, 2, 8),
    ct!(0b01101, 5, 3, 8),
    ct!(0b00001011, 8, 0, 9),
    ct!(0b00001110, 8, 1, 9),
    ct!(0b0001010, 7, 2, 9),
    ct!(0b001100, 6, 3, 9),
    ct!(0b000001111, 9, 0, 10),
    ct!(0b00001010, 8, 1, 10),
    ct!(0b00001101, 8, 2, 10),
    ct!(0b0001100, 7, 3, 10),
    ct!(0b000001011, 9, 0, 11),
    ct!(0b000001110, 9, 1, 11),
    ct!(0b00001001, 8, 2, 11),
    ct!(0b00001100, 8, 3, 11),
    ct!(0b000001000, 9, 0, 12),
    ct!(0b000001010, 9, 1, 12),
    ct!(0b000001101, 9, 2, 12),
    ct!(0b00001000, 8, 3, 12),
    ct!(0b0000001101, 10, 0, 13),
    ct!(0b000000111, 9, 1, 13),
    ct!(0b000001001, 9, 2, 13),
    ct!(0b000001100, 9, 3, 13),
    ct!(0b0000001001, 10, 0, 14),
    ct!(0b0000001100, 10, 1, 14),
    ct!(0b0000001011, 10, 2, 14),
    ct!(0b0000001010, 10, 3, 14),
    ct!(0b0000000101, 10, 0, 15),
    ct!(0b0000001000, 10, 1, 15),
    ct!(0b0000000111, 10, 2, 15),
    ct!(0b0000000110, 10, 3, 15),
    ct!(0b0000000001, 10, 0, 16),
    ct!(0b0000000100, 10, 1, 16),
    ct!(0b0000000011, 10, 2, 16),
    ct!(0b0000000010, 10, 3, 16),
];

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static COEFF_TOKEN_CHROMA_DC_420: [CoeffTokenRow; 14] = [
    ct!(0b01, 2, 0, 0),
    ct!(0b000111, 6, 0, 1),
    ct!(0b1, 1, 1, 1),
    ct!(0b000100, 6, 0, 2),
    ct!(0b000110, 6, 1, 2),
    ct!(0b001, 3, 2, 2),
    ct!(0b000011, 6, 0, 3),
    ct!(0b0000011, 7, 1, 3),
    ct!(0b0000010, 7, 2, 3),
    ct!(0b000101, 6, 3, 3),
    ct!(0b000010, 6, 0, 4),
    ct!(0b00000011, 8, 1, 4),
    ct!(0b00000010, 8, 2, 4),
    ct!(0b0000000, 7, 3, 4),
];

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static COEFF_TOKEN_CHROMA_DC_422: [CoeffTokenRow; 30] = [
    ct!(0b1, 1, 0, 0),
    ct!(0b0001111, 7, 0, 1),
    ct!(0b01, 2, 1, 1),
    ct!(0b0001110, 7, 0, 2),
    ct!(0b0001101, 7, 1, 2),
    ct!(0b001, 3, 2, 2),
    ct!(0b000000111, 9, 0, 3),
    ct!(0b0001100, 7, 1, 3),
    ct!(0b0001011, 7, 2, 3),
    ct!(0b00001, 5, 3, 3),
    ct!(0b000000110, 9, 0, 4),
    ct!(0b000000101, 9, 1, 4),
    ct!(0b0001010, 7, 2, 4),
    ct!(0b000001, 6, 3, 4),
    ct!(0b0000000111, 10, 0, 5),
    ct!(0b0000000110, 10, 1, 5),
    ct!(0b000000100, 9, 2, 5),
    ct!(0b0001001, 7, 3, 5),
    ct!(0b00000000111, 11, 0, 6),
    ct!(0b00000000110, 11, 1, 6),
    ct!(0b0000000101, 10, 2, 6),
    ct!(0b0001000, 7, 3, 6),
    ct!(0b000000000111, 12, 0, 7),
    ct!(0b000000000110, 12, 1, 7),
    ct!(0b00000000101, 11, 2, 7),
    ct!(0b0000000100, 10, 3, 7),
    ct!(0b0000000000111, 13, 0, 8),
    ct!(0b000000000101, 12, 1, 8),
    ct!(0b000000000100, 12, 2, 8),
    ct!(0b00000000100, 11, 3, 8),
];

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static TOTAL_ZEROS_4X4: [&[(u32, u32, u8)]; 15] = [
    &[(0b1,1,0),(0b011,3,1),(0b010,3,2),(0b0011,4,3),(0b0010,4,4),(0b00011,5,5),
      (0b00010,5,6),(0b000011,6,7),(0b000010,6,8),(0b0000011,7,9),(0b0000010,7,10),
      (0b00000011,8,11),(0b00000010,8,12),(0b000000011,9,13),(0b000000010,9,14),
      (0b000000001,9,15)],
    &[(0b111,3,0),(0b110,3,1),(0b101,3,2),(0b100,3,3),(0b011,3,4),(0b0101,4,5),
      (0b0100,4,6),(0b0011,4,7),(0b0010,4,8),(0b00011,5,9),(0b00010,5,10),
      (0b000011,6,11),(0b000010,6,12),(0b000001,6,13),(0b000000,6,14)],
    &[(0b0101,4,0),(0b111,3,1),(0b110,3,2),(0b101,3,3),(0b0100,4,4),(0b0011,4,5),
      (0b100,3,6),(0b011,3,7),(0b0010,4,8),(0b00011,5,9),(0b00010,5,10),
      (0b000001,6,11),(0b00001,5,12),(0b000000,6,13)],
    &[(0b00011,5,0),(0b111,3,1),(0b0101,4,2),(0b0100,4,3),(0b110,3,4),(0b101,3,5),
      (0b100,3,6),(0b0011,4,7),(0b011,3,8),(0b0010,4,9),(0b00010,5,10),
      (0b00001,5,11),(0b00000,5,12)],
    &[(0b0101,4,0),(0b0100,4,1),(0b0011,4,2),(0b111,3,3),(0b110,3,4),(0b101,3,5),
      (0b100,3,6),(0b011,3,7),(0b0010,4,8),(0b00001,5,9),(0b0001,4,10),
      (0b00000,5,11)],
    &[(0b000001,6,0),(0b00001,5,1),(0b111,3,2),(0b110,3,3),(0b101,3,4),(0b100,3,5),
      (0b011,3,6),(0b010,3,7),(0b0001,4,8),(0b001,3,9),(0b000000,6,10)],
    &[(0b000001,6,0),(0b00001,5,1),(0b101,3,2),(0b100,3,3),(0b011,3,4),(0b11,2,5),
      (0b010,3,6),(0b0001,4,7),(0b001,3,8),(0b000000,6,9)],
    &[(0b000001,6,0),(0b0001,4,1),(0b00001,5,2),(0b011,3,3),(0b11,2,4),(0b10,2,5),
      (0b010,3,6),(0b001,3,7),(0b000000,6,8)],
    &[(0b000001,6,0),(0b000000,6,1),(0b0001,4,2),(0b11,2,3),(0b10,2,4),(0b001,3,5),
      (0b01,2,6),(0b00001,5,7)],
    &[(0b00001,5,0),(0b00000,5,1),(0b001,3,2),(0b11,2,3),(0b10,2,4),(0b01,2,5),
      (0b0001,4,6)],
    &[(0b0000,4,0),(0b0001,4,1),(0b001,3,2),(0b010,3,3),(0b1,1,4),(0b011,3,5)],
    &[(0b0000,4,0),(0b0001,4,1),(0b01,2,2),(0b1,1,3),(0b001,3,4)],
    &[(0b000,3,0),(0b001,3,1),(0b1,1,2),(0b01,2,3)],
    &[(0b00,2,0),(0b01,2,1),(0b1,1,2)],
    &[(0b0,1,0),(0b1,1,1)],
];

#[rustfmt::skip]
static TOTAL_ZEROS_CHROMA_DC_2X2: [&[(u32, u32, u8)]; 3] = [
    &[(0b1,1,0),(0b01,2,1),(0b001,3,2),(0b000,3,3)],
    &[(0b1,1,0),(0b01,2,1),(0b00,2,2)],
    &[(0b1,1,0),(0b0,1,1)],
];

#[rustfmt::skip]
static TOTAL_ZEROS_CHROMA_DC_2X4: [&[(u32, u32, u8)]; 7] = [
    &[(0b1,1,0),(0b010,3,1),(0b011,3,2),(0b0010,4,3),(0b0011,4,4),(0b0001,4,5),
      (0b00001,5,6),(0b00000,5,7)],
    &[(0b000,3,0),(0b01,2,1),(0b001,3,2),(0b10,2,3),(0b110,3,4),(0b111,3,5),
      (0b0001,4,6)],
    &[(0b000,3,0),(0b001,3,1),(0b01,2,2),(0b10,2,3),(0b110,3,4),(0b111,3,5)],
    &[(0b110,3,0),(0b00,2,1),(0b01,2,2),(0b10,2,3),(0b111,3,4)],
    &[(0b00,2,0),(0b01,2,1),(0b10,2,2),(0b11,2,3)],
    &[(0b00,2,0),(0b01,2,1),(0b1,1,2)],
    &[(0b0,1,0),(0b1,1,1)],
];

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
static RUN_BEFORE: [&[(u32, u32, u8)]; 7] = [
    &[(0b1,1,0),(0b0,1,1)],
    &[(0b1,1,0),(0b01,2,1),(0b00,2,2)],
    &[(0b11,2,0),(0b10,2,1),(0b01,2,2),(0b00,2,3)],
    &[(0b11,2,0),(0b10,2,1),(0b01,2,2),(0b001,3,3),(0b000,3,4)],
    &[(0b11,2,0),(0b10,2,1),(0b011,3,2),(0b010,3,3),(0b001,3,4),(0b000,3,5)],
    &[(0b11,2,0),(0b000,3,1),(0b001,3,2),(0b011,3,3),(0b010,3,4),(0b101,3,5),
      (0b100,3,6)],
    &[(0b111,3,0),(0b110,3,1),(0b101,3,2),(0b100,3,3),(0b011,3,4),(0b010,3,5),
      (0b001,3,6),(0b0001,4,7),(0b00001,5,8),(0b000001,6,9),(0b0000001,7,10),
      (0b00000001,8,11),(0b000000001,9,12),(0b0000000001,10,13),(0b00000000001,11,14)],
];

#[cfg(test)]
#[allow(clippy::unreadable_literal)]
mod tests {
    use super::super::bitstream::{BitReader, test_support::BitWriter};
    use super::residual_block;

    fn decode(
        max_num_coeff: usize,
        nc: i32,
        f: impl FnOnce(&mut BitWriter),
    ) -> super::ResidualBlock {
        let mut w = BitWriter::new();
        f(&mut w);
        let rbsp = w.into_rbsp();
        let mut r = BitReader::new(&rbsp);
        residual_block(&mut r, max_num_coeff, nc).expect("decode should succeed")
    }

    #[test]
    fn coeff_token_0_2_band() {
        let blk = decode(16, 0, |w| {
            w.bits(0b00011, 5);
            w.bit(0);
            w.bit(0);
            w.bit(0);
            w.bits(0b0101, 4);
        });
        assert_eq!(blk.total_coeff, 3);
        assert_eq!(blk.coeffs[0], 1);
        assert_eq!(blk.coeffs[1], 1);
        assert_eq!(blk.coeffs[2], 1);
        let nonzero: usize = blk.coeffs.iter().filter(|&&c| c != 0).count();
        assert_eq!(nonzero, 3);
    }

    #[test]
    fn coeff_token_2_4_band() {
        let blk = decode(16, 2, |w| {
            w.bits(0b11, 2);
        });
        assert_eq!(blk.total_coeff, 0);
        assert_eq!(blk.coeffs, [0; 16]);
    }

    #[test]
    fn coeff_token_4_8_band() {
        let blk = decode(16, 4, |w| {
            w.bits(0b1110, 4);
            w.bit(1);
            w.bits(0b1, 1);
        });
        assert_eq!(blk.total_coeff, 1);
        assert_eq!(blk.coeffs[0], -1);
        let nonzero: usize = blk.coeffs.iter().filter(|&&c| c != 0).count();
        assert_eq!(nonzero, 1);
    }

    #[test]
    fn coeff_token_fixed_band_nc8() {
        let blk = decode(16, 8, |w| {
            w.bits(0b000001, 6);
            w.bit(0);
            w.bits(0b1, 1);
        });
        assert_eq!(blk.total_coeff, 1);
        assert_eq!(blk.coeffs[0], 1);
    }

    #[test]
    fn coeff_token_fixed_zero_special() {
        let blk = decode(16, 9, |w| {
            w.bits(0b000011, 6);
        });
        assert_eq!(blk.total_coeff, 0);
        assert_eq!(blk.coeffs, [0; 16]);
    }

    #[test]
    fn coeff_token_chroma_dc_420() {
        let blk = decode(4, -1, |w| {
            w.bits(0b1, 1);
            w.bit(1);
            w.bits(0b1, 1);
        });
        assert_eq!(blk.total_coeff, 1);
        assert_eq!(blk.coeffs[0], -1);
    }

    #[test]
    fn full_block_with_level_run_and_zeros() {
        let blk = decode(16, 0, |w| {
            w.bits(0b000100, 6);
            w.bit(0);
            w.bits(0b1, 1);
            w.bits(0b100, 3);
            w.bits(0b10, 2);
        });
        assert_eq!(blk.total_coeff, 2);
        assert_eq!(blk.coeffs[2], 2);
        assert_eq!(blk.coeffs[4], 1);
        let nonzero: usize = blk.coeffs.iter().filter(|&&c| c != 0).count();
        assert_eq!(nonzero, 2);
    }

    #[test]
    fn all_zero_block() {
        let blk = decode(16, 0, |w| {
            w.bits(0b1, 1);
        });
        assert_eq!(blk.total_coeff, 0);
        assert_eq!(blk.coeffs, [0; 16]);
    }

    #[test]
    fn level_escape_large_prefix() {
        let blk = decode(16, 0, |w| {
            w.bits(0b000101, 6);
            w.bits(0, 14);
            w.bit(1);
            w.bits(0b0000, 4);
            w.bits(0b1, 1);
        });
        assert_eq!(blk.total_coeff, 1);
        assert_eq!(blk.coeffs[0], 9);
    }

    #[test]
    fn truncated_bitstream_errs() {
        let mut w = BitWriter::new();
        w.bits(0b0000, 4);
        let rbsp = w.into_rbsp();
        let mut r = BitReader::new(&rbsp);
        let res = residual_block(&mut r, 16, 0);
        assert!(res.is_err(), "truncated bitstream must error, not panic");
    }

    #[test]
    fn run_before_exceeding_zeros_left_errs() {
        let mut w = BitWriter::new();
        w.bits(0b000100, 6);
        w.bit(0);
        w.bits(0b1, 1);
        w.bits(0b0011, 4);
        w.bits(0b00001, 5);
        let rbsp = w.into_rbsp();
        let mut r = BitReader::new(&rbsp);
        let res = residual_block(&mut r, 16, 0);
        assert!(
            res.is_err(),
            "run_before > zeros_left must error, not panic: {res:?}"
        );
    }
}
