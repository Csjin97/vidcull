#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use wide::i32x8;

const COEFF_MIN: i32 = -32768;
const COEFF_MAX: i32 = 32767;

const LEVEL_SCALE: [i64; 6] = [40, 45, 51, 57, 64, 72];

#[inline]
fn clip_i16(x: i32) -> i32 {
    x.clamp(COEFF_MIN, COEFF_MAX)
}

pub fn dequant(coeffs: &mut [i32], log2_size: u32, qp: i32, bit_depth: u8) {
    let size = 1usize << log2_size;
    debug_assert_eq!(coeffs.len(), size * size);

    let shift = i64::from(bit_depth) + i64::from(log2_size) - 5;
    let add = 1i64 << (shift - 1);
    let scale = LEVEL_SCALE[(qp.rem_euclid(6)) as usize] << (qp / 6);
    let scale_m = 16i64;

    for c in coeffs.iter_mut() {
        let v = (i64::from(*c) * scale * scale_m + add) >> shift;
        *c = clip_i16(v.clamp(i64::from(COEFF_MIN), i64::from(COEFF_MAX)) as i32);
    }
}

pub fn inverse_transform(coeffs: &mut [i32], log2_size: u32, use_dst: bool, bit_depth: u8) {
    let n = 1usize << log2_size;
    debug_assert_eq!(coeffs.len(), n * n);

    let kernel: fn(&[i32], &mut [i32]) = if use_dst { dst7_4 } else { dct_1d_dispatch(n) };

    let mut tmp = [0i32; 32 * 32];
    let mut col_in = [0i32; 32];
    let mut col_out = [0i32; 32];
    for x in 0..n {
        for y in 0..n {
            col_in[y] = coeffs[y * n + x];
        }
        kernel(&col_in[..n], &mut col_out[..n]);
        for y in 0..n {
            tmp[y * n + x] = clip_i16((col_out[y] + 64) >> 7);
        }
    }

    let shift = 20 - i32::from(bit_depth);
    let add = 1i32 << (shift - 1);
    let mut row_out = [0i32; 32];
    for y in 0..n {
        let row = &tmp[y * n..y * n + n];
        kernel(row, &mut row_out[..n]);
        for x in 0..n {
            coeffs[y * n + x] = clip_i16((row_out[x] + add) >> shift);
        }
    }
}

pub fn transform_skip(coeffs: &mut [i32], log2_size: u32, bit_depth: u8) {
    let size = 1usize << log2_size;
    debug_assert_eq!(coeffs.len(), size * size);

    let ts_shift = 5 + log2_size;
    let bd_shift = 20 - i32::from(bit_depth);
    let add = 1i32 << (bd_shift - 1);
    for c in coeffs.iter_mut() {
        *c = clip_i16(((*c << ts_shift) + add) >> bd_shift);
    }
}

fn dst7_4(src: &[i32], out: &mut [i32]) {
    let c0 = src[0] + src[2];
    let c1 = src[2] + src[3];
    let c2 = src[0] - src[3];
    let c3 = 74 * src[1];
    out[0] = 29 * c0 + 55 * c1 + c3;
    out[1] = 55 * c2 - 29 * c1 + c3;
    out[2] = 74 * (src[0] - src[2] + src[3]);
    out[3] = 55 * c0 + 29 * c2 - c3;
}

fn dct_1d_dispatch(n: usize) -> fn(&[i32], &mut [i32]) {
    match n {
        4 => dct_1d::<4>,
        8 => dct_1d_simd_8,
        16 => dct_1d_simd_16,
        _ => dct_1d_simd_32,
    }
}

fn dct_1d<const N: usize>(src: &[i32], out: &mut [i32]) {
    let step = 32 / N;
    for (k, o) in out.iter_mut().enumerate().take(N) {
        let mut acc = 0i32;
        for (j, &s) in src.iter().enumerate().take(N) {
            acc += i32::from(TRANSFORM[step * j][k]) * s;
        }
        *o = acc;
    }
}

const fn transposed_mat<const N: usize>() -> [[i32; N]; N] {
    let step = 32 / N;
    let mut mat = [[0i32; N]; N];
    let mut k = 0;
    while k < N {
        let mut j = 0;
        while j < N {
            mat[k][j] = TRANSFORM[step * j][k] as i32;
            j += 1;
        }
        k += 1;
    }
    mat
}

const MAT8: [[i32; 8]; 8] = transposed_mat::<8>();
const MAT16: [[i32; 16]; 16] = transposed_mat::<16>();
const MAT32: [[i32; 32]; 32] = transposed_mat::<32>();

#[inline]
fn dot(a: &[i32], b: &[i32]) -> i32 {
    let mut acc = i32x8::from([0i32; 8]);
    let mut ai = a.chunks_exact(8);
    let mut bi = b.chunks_exact(8);
    for (ca, cb) in ai.by_ref().zip(bi.by_ref()) {
        let va = i32x8::from(<[i32; 8]>::try_from(ca).expect("chunk of 8"));
        let vb = i32x8::from(<[i32; 8]>::try_from(cb).expect("chunk of 8"));
        acc += va * vb;
    }
    let mut total: i32 = acc.to_array().iter().sum();
    for (&x, &y) in ai.remainder().iter().zip(bi.remainder()) {
        total += x * y;
    }
    total
}

#[inline]
fn dct_1d_simd_mat<const N: usize>(src: &[i32], out: &mut [i32], mat: &[[i32; N]; N]) {
    for (k, o) in out.iter_mut().enumerate().take(N) {
        *o = dot(&src[..N], &mat[k][..]);
    }
}

fn dct_1d_simd_8(src: &[i32], out: &mut [i32]) {
    dct_1d_simd_mat(src, out, &MAT8);
}
fn dct_1d_simd_16(src: &[i32], out: &mut [i32]) {
    dct_1d_simd_mat(src, out, &MAT16);
}
fn dct_1d_simd_32(src: &[i32], out: &mut [i32]) {
    dct_1d_simd_mat(src, out, &MAT32);
}

#[rustfmt::skip]
const TRANSFORM: [[i8; 32]; 32] = [
    [64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
     64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64],
    [90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4,
     -4, -13, -22, -31, -38, -46, -54, -61, -67, -73, -78, -82, -85, -88, -90, -90],
    [90, 87, 80, 70, 57, 43, 25, 9, -9, -25, -43, -57, -70, -80, -87, -90,
     -90, -87, -80, -70, -57, -43, -25, -9, 9, 25, 43, 57, 70, 80, 87, 90],
    [90, 82, 67, 46, 22, -4, -31, -54, -73, -85, -90, -88, -78, -61, -38, -13,
     13, 38, 61, 78, 88, 90, 85, 73, 54, 31, 4, -22, -46, -67, -82, -90],
    [89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89,
     89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89],
    [88, 67, 31, -13, -54, -82, -90, -78, -46, -4, 38, 73, 90, 85, 61, 22,
     -22, -61, -85, -90, -73, -38, 4, 46, 78, 90, 82, 54, 13, -31, -67, -88],
    [87, 57, 9, -43, -80, -90, -70, -25, 25, 70, 90, 80, 43, -9, -57, -87,
     -87, -57, -9, 43, 80, 90, 70, 25, -25, -70, -90, -80, -43, 9, 57, 87],
    [85, 46, -13, -67, -90, -73, -22, 38, 82, 88, 54, -4, -61, -90, -78, -31,
     31, 78, 90, 61, 4, -54, -88, -82, -38, 22, 73, 90, 67, 13, -46, -85],
    [83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83,
     83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83],
    [82, 22, -54, -90, -61, 13, 78, 85, 31, -46, -90, -67, 4, 73, 88, 38,
     -38, -88, -73, -4, 67, 90, 46, -31, -85, -78, -13, 61, 90, 54, -22, -82],
    [80, 9, -70, -87, -25, 57, 90, 43, -43, -90, -57, 25, 87, 70, -9, -80,
     -80, -9, 70, 87, 25, -57, -90, -43, 43, 90, 57, -25, -87, -70, 9, 80],
    [78, -4, -82, -73, 13, 85, 67, -22, -88, -61, 31, 90, 54, -38, -90, -46,
     46, 90, 38, -54, -90, -31, 61, 88, 22, -67, -85, -13, 73, 82, 4, -78],
    [75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75,
     75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75],
    [73, -31, -90, -22, 78, 67, -38, -90, -13, 82, 61, -46, -88, -4, 85, 54,
     -54, -85, 4, 88, 46, -61, -82, 13, 90, 38, -67, -78, 22, 90, 31, -73],
    [70, -43, -87, 9, 90, 25, -80, -57, 57, 80, -25, -90, -9, 87, 43, -70,
     -70, 43, 87, -9, -90, -25, 80, 57, -57, -80, 25, 90, 9, -87, -43, 70],
    [67, -54, -78, 38, 85, -22, -90, 4, 90, 13, -88, -31, 82, 46, -73, -61,
     61, 73, -46, -82, 31, 88, -13, -90, -4, 90, 22, -85, -38, 78, 54, -67],
    [64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64,
     64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64],
    [61, -73, -46, 82, 31, -88, -13, 90, -4, -90, 22, 85, -38, -78, 54, 67,
     -67, -54, 78, 38, -85, -22, 90, 4, -90, 13, 88, -31, -82, 46, 73, -61],
    [57, -80, -25, 90, -9, -87, 43, 70, -70, -43, 87, 9, -90, 25, 80, -57,
     -57, 80, 25, -90, 9, 87, -43, -70, 70, 43, -87, -9, 90, -25, -80, 57],
    [54, -85, -4, 88, -46, -61, 82, 13, -90, 38, 67, -78, -22, 90, -31, -73,
     73, 31, -90, 22, 78, -67, -38, 90, -13, -82, 61, 46, -88, 4, 85, -54],
    [50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50,
     50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50],
    [46, -90, 38, 54, -90, 31, 61, -88, 22, 67, -85, 13, 73, -82, 4, 78,
     -78, -4, 82, -73, -13, 85, -67, -22, 88, -61, -31, 90, -54, -38, 90, -46],
    [43, -90, 57, 25, -87, 70, 9, -80, 80, -9, -70, 87, -25, -57, 90, -43,
     -43, 90, -57, -25, 87, -70, -9, 80, -80, 9, 70, -87, 25, 57, -90, 43],
    [38, -88, 73, -4, -67, 90, -46, -31, 85, -78, 13, 61, -90, 54, 22, -82,
     82, -22, -54, 90, -61, -13, 78, -85, 31, 46, -90, 67, 4, -73, 88, -38],
    [36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36,
     36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36],
    [31, -78, 90, -61, 4, 54, -88, 82, -38, -22, 73, -90, 67, -13, -46, 85,
     -85, 46, 13, -67, 90, -73, 22, 38, -82, 88, -54, -4, 61, -90, 78, -31],
    [25, -70, 90, -80, 43, 9, -57, 87, -87, 57, -9, -43, 80, -90, 70, -25,
     -25, 70, -90, 80, -43, -9, 57, -87, 87, -57, 9, 43, -80, 90, -70, 25],
    [22, -61, 85, -90, 73, -38, -4, 46, -78, 90, -82, 54, -13, -31, 67, -88,
     88, -67, 31, 13, -54, 82, -90, 78, -46, 4, 38, -73, 90, -85, 61, -22],
    [18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18,
     18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18],
    [13, -38, 61, -78, 88, -90, 85, -73, 54, -31, 4, 22, -46, 67, -82, 90,
     -90, 82, -67, 46, -22, -4, 31, -54, 73, -85, 90, -88, 78, -61, 38, -13],
    [9, -25, 43, -57, 70, -80, 87, -90, 90, -87, 80, -70, 57, -43, 25, -9,
     -9, 25, -43, 57, -70, 80, -87, 90, -90, 87, -80, 70, -57, 43, -25, 9],
    [4, -13, 22, -31, 38, -46, 54, -61, 67, -73, 78, -82, 85, -88, 90, -90,
     90, -90, 88, -85, 82, -78, 73, -67, 61, -54, 46, -38, 31, -22, 13, -4],
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tr4_butterfly(s: &[i32; 4]) -> [i32; 4] {
        let e0 = 64 * s[0] + 64 * s[2];
        let e1 = 64 * s[0] - 64 * s[2];
        let o0 = 83 * s[1] + 36 * s[3];
        let o1 = 36 * s[1] - 83 * s[3];
        [e0 + o0, e1 + o1, e1 - o1, e0 - o0]
    }

    #[test]
    fn dct4_matrix_matches_butterfly() {
        for s in [
            [1, 0, 0, 0],
            [0, 7, 0, 0],
            [3, -5, 9, -2],
            [100, -64, 32, -16],
        ] {
            let mut out = [0i32; 4];
            dct_1d::<4>(&s, &mut out);
            assert_eq!(out, tr4_butterfly(&s), "matrix DCT4 vs butterfly for {s:?}");
        }
    }

    #[test]
    fn dct4_rows_are_canonical() {
        let rows: Vec<[i8; 4]> = (0..4)
            .map(|j| std::array::from_fn(|k| TRANSFORM[8 * j][k]))
            .collect();
        assert_eq!(rows[0], [64, 64, 64, 64]);
        assert_eq!(rows[1], [83, 36, -36, -83]);
        assert_eq!(rows[2], [64, -64, -64, 64]);
        assert_eq!(rows[3], [36, -83, 83, -36]);
    }

    #[test]
    fn dct8_first_odd_row() {
        let row: [i8; 8] = std::array::from_fn(|k| TRANSFORM[4][k]);
        assert_eq!(row, [89, 75, 50, 18, -18, -50, -75, -89]);
    }

    #[test]
    fn dct_pure_dc_is_uniform() {
        for &log2 in &[2u32, 3, 4, 5] {
            let n = 1usize << log2;
            let mut block = vec![0i32; n * n];
            block[0] = 256;
            inverse_transform(&mut block, log2, false, 8);
            let g = (64 * 256 + 64) >> 7;
            let expect = (64 * g + 2048) >> 12;
            assert!(
                block.iter().all(|&v| v == expect),
                "n={n}: not uniform {expect}"
            );
        }
    }

    #[test]
    fn dst_zero_and_dc() {
        let mut zero = [0i32; 16];
        inverse_transform(&mut zero, 2, true, 8);
        assert_eq!(zero, [0; 16]);

        let mut out = [0i32; 4];
        dst7_4(&[1, 0, 0, 0], &mut out);
        assert_eq!(out, [29, 55, 74, 84]);
    }

    #[test]
    fn dequant_hand_value() {
        let mut c = [0i32; 16];
        c[0] = 1;
        c[5] = -2;
        dequant(&mut c, 2, 24, 8);
        assert_eq!(c[0], (640 * 16 + 16) >> 5);
        assert_eq!(c[0], 320);
        assert_eq!(c[5], (-2 * 640 * 16 + 16) >> 5);
        assert_eq!(c[5], -640);
    }

    #[test]
    fn transform_skip_hand_value() {
        let mut c = [0i32; 16];
        c[0] = 320;
        c[5] = -640;
        transform_skip(&mut c, 2, 8);
        assert_eq!(c[0], ((320 << 7) + 2048) >> 12);
        assert_eq!(c[0], 10);
        assert_eq!(c[5], ((-640 << 7) + 2048) >> 12);
        assert_eq!(c[5], -20);
        assert_eq!(c[1], 0);
    }

    #[test]
    fn dequant_saturates_to_i16() {
        let mut c = [0i32; 16];
        c[0] = 30000;
        dequant(&mut c, 2, 51, 8);
        assert_eq!(c[0], COEFF_MAX);
        c[0] = -30000;
        dequant(&mut c, 2, 51, 8);
        assert_eq!(c[0], COEFF_MIN);
    }

    #[test]
    fn inverse_transform_zero_is_zero() {
        for &log2 in &[2u32, 3, 4, 5] {
            let n = 1usize << log2;
            let mut block = vec![0i32; n * n];
            inverse_transform(&mut block, log2, false, 8);
            assert!(block.iter().all(|&v| v == 0));
        }
    }

    #[test]
    fn dct_1d_simd_matches_scalar() {
        let mut state = 0x1234_5678u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 16) as i32) - 32768
        };
        for _ in 0..200 {
            let src: [i32; 32] = std::array::from_fn(|_| next());
            let (mut s, mut v) = ([0i32; 8], [0i32; 8]);
            dct_1d::<8>(&src[..8], &mut s);
            dct_1d_simd_8(&src[..8], &mut v);
            assert_eq!(s, v, "SIMD DCT8 != scalar");
            let (mut s, mut v) = ([0i32; 16], [0i32; 16]);
            dct_1d::<16>(&src[..16], &mut s);
            dct_1d_simd_16(&src[..16], &mut v);
            assert_eq!(s, v, "SIMD DCT16 != scalar");
            let (mut s, mut v) = ([0i32; 32], [0i32; 32]);
            dct_1d::<32>(&src, &mut s);
            dct_1d_simd_32(&src, &mut v);
            assert_eq!(s, v, "SIMD DCT32 != scalar");
        }
        for fill in [COEFF_MAX, COEFF_MIN] {
            let src = [fill; 32];
            let (mut s, mut v) = ([0i32; 32], [0i32; 32]);
            dct_1d::<32>(&src, &mut s);
            dct_1d_simd_32(&src, &mut v);
            assert_eq!(s, v, "SIMD DCT32 != scalar at extreme {fill}");
        }
    }

    #[test]
    #[ignore = "timing microbench; run with --ignored --nocapture"]
    fn idct_bench() {
        use std::time::Instant;
        let iters = 5_000_000u32;
        let bench = |name: &str,
                     scalar_fn: fn(&[i32], &mut [i32]),
                     simd_fn: fn(&[i32], &mut [i32]),
                     n: usize| {
            let src: Vec<i32> = (0..n)
                .map(|i| ((i as i32 * 1327) % 65_535) - 32_767)
                .collect();
            let mut out = vec![0i32; n];
            scalar_fn(std::hint::black_box(&src), &mut out);
            let t = Instant::now();
            for _ in 0..iters {
                scalar_fn(std::hint::black_box(&src), &mut out);
                std::hint::black_box(&out);
            }
            let scalar = t.elapsed();
            let t = Instant::now();
            for _ in 0..iters {
                simd_fn(std::hint::black_box(&src), &mut out);
                std::hint::black_box(&out);
            }
            let simd = t.elapsed();
            println!(
                "{name} x{iters}: scalar {:.2}ns/call, simd {:.2}ns/call, speedup {:.2}x",
                scalar.as_secs_f64() * 1e9 / f64::from(iters),
                simd.as_secs_f64() * 1e9 / f64::from(iters),
                scalar.as_secs_f64() / simd.as_secs_f64(),
            );
        };
        bench("dct_1d N=8 ", dct_1d::<8>, dct_1d_simd_8, 8);
        bench("dct_1d N=16", dct_1d::<16>, dct_1d_simd_16, 16);
        bench("dct_1d N=32", dct_1d::<32>, dct_1d_simd_32, 32);
    }
}
