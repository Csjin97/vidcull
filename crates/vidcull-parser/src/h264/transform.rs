const RASTER_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

const RASTER_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const V4: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

const V8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

const fn class_4x4(i: usize, j: usize) -> usize {
    if i % 2 == 0 && j % 2 == 0 {
        0
    } else if i % 2 == 1 && j % 2 == 1 {
        1
    } else {
        2
    }
}

const fn level_scale_4x4(m: usize, i: usize, j: usize) -> i32 {
    16 * V4[m][class_4x4(i, j)]
}

const fn class_8x8(i: usize, j: usize) -> usize {
    if i % 4 == 0 && j % 4 == 0 {
        0
    } else if i % 2 == 1 && j % 2 == 1 {
        1
    } else if i % 4 == 2 && j % 4 == 2 {
        2
    } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
        3
    } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
        4
    } else {
        5
    }
}

const fn level_scale_8x8(m: usize, i: usize, j: usize) -> i32 {
    16 * V8[m][class_8x8(i, j)]
}

fn qp_mod6(qp: i32) -> usize {
    usize::try_from(qp.rem_euclid(6)).unwrap_or(0)
}

#[must_use]
pub fn inverse_scan_4x4(scanned: &[i32; 16]) -> [i32; 16] {
    let mut out = [0; 16];
    for (i, &v) in scanned.iter().enumerate() {
        out[RASTER_4X4[i]] = v;
    }
    out
}

#[must_use]
pub fn inverse_scan_8x8(scanned: &[i32; 64]) -> [i32; 64] {
    let mut out = [0; 64];
    for (i, &v) in scanned.iter().enumerate() {
        out[RASTER_8X8[i]] = v;
    }
    out
}

#[must_use]
pub fn dequant_4x4(coeffs: &[i32; 16], qp: i32, skip_dc: bool) -> [i32; 16] {
    let m = qp_mod6(qp);
    let shift = qp / 6;
    let mut out = [0; 16];
    for idx in 0..16 {
        if skip_dc && idx == 0 {
            out[0] = coeffs[0];
            continue;
        }
        let i = idx / 4;
        let j = idx % 4;
        let ls = level_scale_4x4(m, i, j);
        let c = coeffs[idx];
        out[idx] = if qp >= 24 {
            (c * ls) << (shift - 4)
        } else {
            (c * ls + (1 << (3 - shift))) >> (4 - shift)
        };
    }
    out
}

#[must_use]
pub fn dequant_8x8(coeffs: &[i32; 64], qp: i32) -> [i32; 64] {
    let m = qp_mod6(qp);
    let shift = qp / 6;
    let mut out = [0; 64];
    for idx in 0..64 {
        let i = idx / 8;
        let j = idx % 8;
        let ls = level_scale_8x8(m, i, j);
        let c = coeffs[idx];
        out[idx] = if qp >= 36 {
            (c * ls) << (shift - 6)
        } else {
            (c * ls + (1 << (5 - shift))) >> (6 - shift)
        };
    }
    out
}

#[inline]
fn idct_4x4_1d(z: [i32; 4]) -> [i32; 4] {
    let [z0, z1, z2, z3] = z;
    let e0 = z0 + z2;
    let e1 = z0 - z2;
    let e2 = (z1 >> 1) - z3;
    let e3 = z1 + (z3 >> 1);
    [e0 + e3, e1 + e2, e1 - e2, e0 - e3]
}

#[must_use]
pub fn idct_4x4(block: &[i32; 16]) -> [i32; 16] {
    let mut inter = [0i32; 16];
    for i in 0..4 {
        let row = [
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ];
        let o = idct_4x4_1d(row);
        for j in 0..4 {
            inter[i * 4 + j] = o[j];
        }
    }
    let mut out = [0i32; 16];
    for j in 0..4 {
        let col = [inter[j], inter[4 + j], inter[8 + j], inter[12 + j]];
        let o = idct_4x4_1d(col);
        for i in 0..4 {
            out[i * 4 + j] = (o[i] + 32) >> 6;
        }
    }
    out
}

#[inline]
fn idct_8x8_1d(z: [i32; 8]) -> [i32; 8] {
    let [z0, z1, z2, z3, z4, z5, z6, z7] = z;
    let a0 = z0 + z4;
    let a4 = z0 - z4;
    let a2 = (z2 >> 1) - z6;
    let a6 = z2 + (z6 >> 1);
    let b0 = a0 + a6;
    let b2 = a4 + a2;
    let b4 = a4 - a2;
    let b6 = a0 - a6;
    let a1 = -z3 + z5 - z7 - (z7 >> 1);
    let a3 = z1 + z7 - z3 - (z3 >> 1);
    let a5 = -z1 + z7 + z5 + (z5 >> 1);
    let a7 = z3 + z5 + z1 + (z1 >> 1);
    let b1 = a1 + (a7 >> 2);
    let b7 = a7 - (a1 >> 2);
    let b3 = a3 + (a5 >> 2);
    let b5 = (a3 >> 2) - a5;
    [
        b0 + b7,
        b2 + b5,
        b4 + b3,
        b6 + b1,
        b6 - b1,
        b4 - b3,
        b2 - b5,
        b0 - b7,
    ]
}

#[must_use]
pub fn idct_8x8(block: &[i32; 64]) -> [i32; 64] {
    let mut inter = [0i32; 64];
    for i in 0..8 {
        let mut row = [0i32; 8];
        for j in 0..8 {
            row[j] = block[i * 8 + j];
        }
        let o = idct_8x8_1d(row);
        for j in 0..8 {
            inter[i * 8 + j] = o[j];
        }
    }
    let mut out = [0i32; 64];
    for j in 0..8 {
        let mut col = [0i32; 8];
        for i in 0..8 {
            col[i] = inter[i * 8 + j];
        }
        let o = idct_8x8_1d(col);
        for i in 0..8 {
            out[i * 8 + j] = (o[i] + 32) >> 6;
        }
    }
    out
}

#[inline]
fn hadamard_4x4_1d(z: [i32; 4]) -> [i32; 4] {
    let [z0, z1, z2, z3] = z;
    let a0 = z0 + z2;
    let a1 = z0 - z2;
    let a2 = z1 - z3;
    let a3 = z1 + z3;
    [a0 + a3, a1 + a2, a1 - a2, a0 - a3]
}

#[must_use]
pub fn luma_dc_transform(dc_scanned: &[i32; 16], qp: i32) -> [i32; 16] {
    let c = inverse_scan_4x4(dc_scanned);

    let mut inter = [0i32; 16];
    for i in 0..4 {
        let row = [c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3]];
        let o = hadamard_4x4_1d(row);
        for j in 0..4 {
            inter[i * 4 + j] = o[j];
        }
    }
    let mut f = [0i32; 16];
    for j in 0..4 {
        let col = [inter[j], inter[4 + j], inter[8 + j], inter[12 + j]];
        let o = hadamard_4x4_1d(col);
        for i in 0..4 {
            f[i * 4 + j] = o[i];
        }
    }

    let m = qp_mod6(qp);
    let shift = qp / 6;
    let ls = level_scale_4x4(m, 0, 0);
    let mut out = [0i32; 16];
    for idx in 0..16 {
        out[idx] = if qp >= 36 {
            (f[idx] * ls) << (shift - 6)
        } else {
            (f[idx] * ls + (1 << (5 - shift))) >> (6 - shift)
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_scan_4x4_matches_table() {
        let scanned: [i32; 16] = std::array::from_fn(|i| i32::try_from(i).unwrap());
        let out = inverse_scan_4x4(&scanned);
        let expected = [0, 1, 5, 6, 2, 4, 7, 12, 3, 8, 11, 13, 9, 10, 14, 15];
        assert_eq!(out, expected);
    }

    #[test]
    fn inverse_scan_8x8_matches_table() {
        let scanned: [i32; 64] = std::array::from_fn(|i| i32::try_from(i).unwrap());
        let out = inverse_scan_8x8(&scanned);
        let expected = [
            0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43,
            9, 11, 18, 24, 31, 40, 44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51,
            55, 60, 21, 34, 37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn level_scale_4x4_hand_values() {
        assert_eq!(level_scale_4x4(0, 0, 0), 160);
        assert_eq!(level_scale_4x4(0, 1, 1), 256);
        assert_eq!(level_scale_4x4(0, 0, 1), 208);
    }

    #[test]
    fn dequant_4x4_low_qp_round_branch() {
        let mut coeffs = [0i32; 16];
        coeffs[0] = 1;
        coeffs[1] = 3;
        let out = dequant_4x4(&coeffs, 0, false);
        assert_eq!(out[0], (160 + 8) >> 4);
        assert_eq!(out[0], 10);
        assert_eq!(out[1], (3 * 208 + 8) >> 4);
    }

    #[test]
    fn dequant_4x4_high_qp_shift_branch() {
        let mut coeffs = [0i32; 16];
        coeffs[0] = 2;
        let out = dequant_4x4(&coeffs, 30, false);
        assert_eq!(out[0], (2 * 160) << 1);
    }

    #[test]
    fn dequant_4x4_skip_dc_leaves_index_zero() {
        let mut coeffs = [0i32; 16];
        coeffs[0] = 7;
        coeffs[5] = 1;
        let out = dequant_4x4(&coeffs, 0, true);
        assert_eq!(out[0], 7, "skip_dc must leave index 0 untouched");
        assert_eq!(out[5], (256 + 8) >> 4);
    }

    #[test]
    fn idct_4x4_all_zero() {
        assert_eq!(idct_4x4(&[0; 16]), [0; 16]);
    }

    #[test]
    fn idct_4x4_pure_dc() {
        let mut block = [0i32; 16];
        block[0] = 64;
        assert_eq!(idct_4x4(&block), [1; 16]);
        block[0] = 256;
        assert_eq!(idct_4x4(&block), [4; 16]);
    }

    #[test]
    fn idct_8x8_all_zero() {
        assert_eq!(idct_8x8(&[0; 64]), [0; 64]);
    }

    #[test]
    fn idct_8x8_pure_dc() {
        let mut block = [0i32; 64];
        block[0] = 64;
        assert_eq!(idct_8x8(&block), [1; 64]);
        block[0] = 256;
        assert_eq!(idct_8x8(&block), [4; 64]);
    }

    #[test]
    fn dequant_8x8_low_qp_round_branch() {
        let mut coeffs = [0i32; 64];
        coeffs[0] = 1;
        let out = dequant_8x8(&coeffs, 20);
        assert_eq!(out[0], (416 + (1 << 2)) >> 3);
        assert_eq!(out[0], 52);
    }

    #[test]
    fn dequant_8x8_high_qp_shift_branch() {
        let mut coeffs = [0i32; 64];
        coeffs[0] = 1;
        let out = dequant_8x8(&coeffs, 42);
        assert_eq!(out[0], 320 << 1);
    }

    #[test]
    fn luma_dc_transform_all_zero() {
        assert_eq!(luma_dc_transform(&[0; 16], 30), [0; 16]);
    }

    #[test]
    fn luma_dc_transform_single_dc() {
        let mut dc = [0i32; 16];
        dc[0] = 2;
        let out = luma_dc_transform(&dc, 12);
        let expected = (2 * 160 + (1 << 3)) >> 4;
        assert_eq!(out, [expected; 16]);
        assert_eq!(out, [20; 16]);
    }

    #[test]
    fn luma_dc_transform_high_qp_shift_branch() {
        let mut dc = [0i32; 16];
        dc[0] = 1;
        let out = luma_dc_transform(&dc, 42);
        assert_eq!(out, [160 << 1; 16]);
    }
}
