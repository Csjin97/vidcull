use std::f64::consts::PI;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use vidcull_core::Result;
use vidcull_core::binary;
use vidcull_core::types::{Codec, VideoDuration};

use crate::simd;

pub const PHASH_GRID: usize = 32;

pub const PHASH_LOWFREQ: usize = 8;

pub const REENCODE_STABILITY_THRESHOLD: f64 = 0.75;

#[derive(Debug, Clone, Copy)]
pub struct GrayFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GopSignature {
    pub keyframe_count: u32,
    pub mean_gop_ms: u32,
    pub max_gop_ms: u32,
}

impl GopSignature {
    #[must_use]
    pub fn from_durations(durations: &[u64]) -> Self {
        if durations.is_empty() {
            return Self {
                keyframe_count: 0,
                mean_gop_ms: 0,
                max_gop_ms: 0,
            };
        }
        let count = durations.len() as u64;
        let sum: u64 = durations.iter().copied().sum();
        let max = durations.iter().copied().max().unwrap_or(0);
        Self {
            keyframe_count: saturating_u32(count),
            mean_gop_ms: saturating_u32(sum / count),
            max_gop_ms: saturating_u32(max),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier1Fingerprint {
    pub duration_ms: u64,
    pub codec: Codec,
    pub gop: GopSignature,
    pub global_phash: u64,
}

impl Tier1Fingerprint {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        binary::encode(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        binary::decode(bytes)
    }
}

#[must_use]
pub fn build_tier1(
    duration: VideoDuration,
    codec: Codec,
    gop_durations_ms: &[u64],
    frames: &[GrayFrame<'_>],
) -> Tier1Fingerprint {
    Tier1Fingerprint {
        duration_ms: duration.as_millis(),
        codec,
        gop: GopSignature::from_durations(gop_durations_ms),
        global_phash: phash_frames(frames),
    }
}

#[derive(Debug, Clone)]
pub struct Tier1Builder {
    accum: Vec<f64>,
    used: u32,
}

impl Default for Tier1Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Tier1Builder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            accum: vec![0.0_f64; PHASH_GRID * PHASH_GRID],
            used: 0,
        }
    }

    pub fn push(&mut self, frame: &GrayFrame<'_>) {
        let _ = self.push_and_phash(frame);
    }

    /// Adds a frame to the global fingerprint and returns the same frame's
    /// pHash without performing a second full-resolution downscale pass.
    pub fn push_and_phash(&mut self, frame: &GrayFrame<'_>) -> Option<u64> {
        if frame.width == 0 || frame.height == 0 {
            return None;
        }
        let small = downscale(frame);
        for (a, &s) in self.accum.iter_mut().zip(&small) {
            *a += s;
        }
        self.used += 1;
        Some(phash_from_grid(&small))
    }

    /// Adds a frame and computes its pHash plus DCT energy from one downscale
    /// and one full DCT. The returned values are identical to calling the
    /// standalone pHash and DCT-energy paths separately.
    pub fn push_and_analyze(&mut self, frame: &GrayFrame<'_>) -> Option<(u64, f64)> {
        const N: usize = PHASH_GRID;
        if frame.width == 0 || frame.height == 0 {
            return None;
        }
        let grid = downscale(frame);
        for (a, &sample) in self.accum.iter_mut().zip(&grid) {
            *a += sample;
        }
        self.used += 1;

        let basis = dct_basis();
        let mut tmp = [0.0_f64; N * N];
        let mut full = [0.0_f64; N * N];
        simd::dct2d_lowblock(&grid, N, basis, N, &mut tmp, &mut full);
        Some((phash_from_full_dct(&full), dct_energy_from_full(&full)))
    }

    #[must_use]
    pub fn finish(self) -> u64 {
        if self.used == 0 {
            return 0;
        }
        let mut accum = self.accum;
        let inv = f64::from(self.used);
        for a in &mut accum {
            *a /= inv;
        }
        phash_from_grid(&accum)
    }
}

#[must_use]
pub fn phash_frames(frames: &[GrayFrame<'_>]) -> u64 {
    let mut builder = Tier1Builder::new();
    for f in frames {
        builder.push(f);
    }
    builder.finish()
}

#[must_use]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[must_use]
pub fn hamming_distance_batch(query: u64, hashes: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    simd::hamming_batch(query, hashes, &mut out);
    out
}

fn saturating_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn downscale(f: &GrayFrame<'_>) -> Vec<f64> {
    let w = f.width as usize;
    let h = f.height as usize;
    let len = f.pixels.len();
    let mut out = vec![0.0_f64; PHASH_GRID * PHASH_GRID];
    for ty in 0..PHASH_GRID {
        let y0 = ty * h / PHASH_GRID;
        let y1 = (((ty + 1) * h / PHASH_GRID).max(y0 + 1)).min(h);
        for tx in 0..PHASH_GRID {
            let x0 = tx * w / PHASH_GRID;
            let x1 = (((tx + 1) * w / PHASH_GRID).max(x0 + 1)).min(w);
            let mut sum = 0u32;
            for sy in y0..y1 {
                let lo = sy * w + x0;
                let hi = sy * w + x1;
                if hi <= len {
                    for &p in &f.pixels[lo..hi] {
                        sum += u32::from(p);
                    }
                } else {
                    for px in lo..hi {
                        sum += u32::from(f.pixels.get(px).copied().unwrap_or(0));
                    }
                }
            }
            let cnt = u32::try_from((y1 - y0) * (x1 - x0)).unwrap_or(u32::MAX);
            out[ty * PHASH_GRID + tx] = if cnt > 0 {
                f64::from(sum) / f64::from(cnt)
            } else {
                0.0
            };
        }
    }
    out
}

#[allow(clippy::cast_precision_loss)]
fn dct_basis() -> &'static [f64; PHASH_GRID * PHASH_GRID] {
    const N: usize = PHASH_GRID;
    static BASIS: OnceLock<[f64; N * N]> = OnceLock::new();

    BASIS.get_or_init(|| {
        let mut basis = [0.0_f64; N * N];
        for k in 0..N {
            for u in 0..N {
                basis[k * N + u] = ((2 * k + 1) as f64 * u as f64 * PI / (2.0 * N as f64)).cos();
            }
        }
        basis
    })
}

fn phash_from_grid(grid: &[f64]) -> u64 {
    const N: usize = PHASH_GRID;
    const LOW: usize = PHASH_LOWFREQ;

    let basis = dct_basis();
    let mut tmp = [0.0_f64; N * LOW];
    let mut low = [0.0_f64; LOW * LOW];
    simd::dct2d_lowblock(grid, N, basis, LOW, &mut tmp, &mut low);

    phash_from_low_coefficients(&low)
}

fn phash_from_low_coefficients(low: &[f64; PHASH_LOWFREQ * PHASH_LOWFREQ]) -> u64 {
    const LOW: usize = PHASH_LOWFREQ;

    if low.iter().skip(1).all(|c| c.abs() < 1e-6) {
        return 0;
    }

    let mut sorted = *low;
    sorted.sort_unstable_by(f64::total_cmp);
    let median = f64::midpoint(sorted[LOW * LOW / 2 - 1], sorted[LOW * LOW / 2]);

    let mut hash = 0u64;
    for (i, &c) in low.iter().enumerate() {
        if c > median {
            hash |= 1 << (63 - i);
        }
    }
    hash
}

fn phash_from_full_dct(full: &[f64; PHASH_GRID * PHASH_GRID]) -> u64 {
    const N: usize = PHASH_GRID;
    const LOW: usize = PHASH_LOWFREQ;
    let mut low = [0.0_f64; LOW * LOW];
    for u in 0..LOW {
        low[u * LOW..(u + 1) * LOW].copy_from_slice(&full[u * N..u * N + LOW]);
    }
    phash_from_low_coefficients(&low)
}

fn dct_energy_from_full(full: &[f64; PHASH_GRID * PHASH_GRID]) -> f64 {
    const N: usize = PHASH_GRID;
    let mut energy = 0.0_f64;
    for u in 0..N {
        for v in 0..N {
            if u >= 8 || v >= 8 {
                energy += full[u * N + v].powi(2);
            }
        }
    }
    energy
}

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn laplacian_variance(f: &GrayFrame<'_>) -> f64 {
    let w = f.width as usize;
    let h = f.height as usize;
    if w < 3 || h < 3 {
        return 0.0;
    }
    if w.checked_mul(h)
        .is_some_and(|required| f.pixels.len() >= required)
    {
        return laplacian_variance_complete(f.pixels, w, h);
    }

    // Two passes over the pixels directly instead of materializing a
    // Vec<f64> of every interior pixel's Laplacian value (2M+ f64s / 16MB+
    // for a 1080p frame) — same expression, same evaluation order as before
    // in both passes, so the sums are bit-identical to the single-Vec
    // version.
    let count = ((w - 2) * (h - 2)) as f64;

    let mut sum = 0.0_f64;
    for y in 1..(h - 1) {
        let prev_row = (y - 1) * w;
        let curr_row = y * w;
        let next_row = (y + 1) * w;
        for x in 1..(w - 1) {
            let center = f64::from(f.pixels.get(curr_row + x).copied().unwrap_or(0));
            let left = f64::from(f.pixels.get(curr_row + x - 1).copied().unwrap_or(0));
            let right = f64::from(f.pixels.get(curr_row + x + 1).copied().unwrap_or(0));
            let up = f64::from(f.pixels.get(prev_row + x).copied().unwrap_or(0));
            let down = f64::from(f.pixels.get(next_row + x).copied().unwrap_or(0));
            sum += left + right + up + down - 4.0 * center;
        }
    }
    let mean = sum / count;

    let mut variance_sum = 0.0_f64;
    for y in 1..(h - 1) {
        let prev_row = (y - 1) * w;
        let curr_row = y * w;
        let next_row = (y + 1) * w;
        for x in 1..(w - 1) {
            let center = f64::from(f.pixels.get(curr_row + x).copied().unwrap_or(0));
            let left = f64::from(f.pixels.get(curr_row + x - 1).copied().unwrap_or(0));
            let right = f64::from(f.pixels.get(curr_row + x + 1).copied().unwrap_or(0));
            let up = f64::from(f.pixels.get(prev_row + x).copied().unwrap_or(0));
            let down = f64::from(f.pixels.get(next_row + x).copied().unwrap_or(0));
            let val = left + right + up + down - 4.0 * center;
            variance_sum += (val - mean).powi(2);
        }
    }
    variance_sum / count
}

#[allow(clippy::cast_precision_loss)]
fn laplacian_variance_complete(pixels: &[u8], w: usize, h: usize) -> f64 {
    let count = ((w - 2) * (h - 2)) as f64;
    let mut sum = 0.0_f64;
    for y in 1..(h - 1) {
        let prev = &pixels[(y - 1) * w..y * w];
        let curr = &pixels[y * w..(y + 1) * w];
        let next = &pixels[(y + 1) * w..(y + 2) * w];
        for x in 1..(w - 1) {
            let center = f64::from(curr[x]);
            let left = f64::from(curr[x - 1]);
            let right = f64::from(curr[x + 1]);
            let up = f64::from(prev[x]);
            let down = f64::from(next[x]);
            sum += left + right + up + down - 4.0 * center;
        }
    }
    let mean = sum / count;

    let mut variance_sum = 0.0_f64;
    for y in 1..(h - 1) {
        let prev = &pixels[(y - 1) * w..y * w];
        let curr = &pixels[y * w..(y + 1) * w];
        let next = &pixels[(y + 1) * w..(y + 2) * w];
        for x in 1..(w - 1) {
            let center = f64::from(curr[x]);
            let left = f64::from(curr[x - 1]);
            let right = f64::from(curr[x + 1]);
            let up = f64::from(prev[x]);
            let down = f64::from(next[x]);
            let val = left + right + up + down - 4.0 * center;
            variance_sum += (val - mean).powi(2);
        }
    }
    variance_sum / count
}

#[must_use]
pub fn dct_energy(f: &GrayFrame<'_>) -> f64 {
    const N: usize = PHASH_GRID;
    if f.width == 0 || f.height == 0 {
        return 0.0;
    }
    let grid = downscale(f);

    let basis = dct_basis();
    let mut tmp = [0.0_f64; N * N];
    let mut full = [0.0_f64; N * N];
    simd::dct2d_lowblock(&grid, N, basis, N, &mut tmp, &mut full);

    dct_energy_from_full(&full)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[allow(clippy::needless_range_loop, clippy::cast_precision_loss)]
    fn dct_lowblock_scalar(grid: &[f64], out_dim: usize) -> Vec<f64> {
        const N: usize = PHASH_GRID;
        let mut basis = [[0.0_f64; N]; N];
        for k in 0..N {
            for u in 0..N {
                basis[k][u] = ((2 * k + 1) as f64 * u as f64 * PI / (2.0 * N as f64)).cos();
            }
        }
        let mut tmp = vec![0.0_f64; N * out_dim];
        for x in 0..N {
            let base = x * N;
            for v in 0..out_dim {
                let mut acc = 0.0_f64;
                for y in 0..N {
                    acc += grid[base + y] * basis[y][v];
                }
                tmp[x * out_dim + v] = acc;
            }
        }
        let mut low = vec![0.0_f64; out_dim * out_dim];
        for u in 0..out_dim {
            for v in 0..out_dim {
                let mut acc = 0.0_f64;
                for x in 0..N {
                    acc += tmp[x * out_dim + v] * basis[x][u];
                }
                low[u * out_dim + v] = acc;
            }
        }
        low
    }

    #[test]
    fn simd_dct_is_bit_identical_to_scalar_reference() {
        const N: usize = PHASH_GRID;
        let basis = dct_basis();
        let mut state = 0x00C0_FFEE_1234_5678;
        for _ in 0..200 {
            let grid: Vec<f64> = (0..N * N)
                .map(|_| f64::from((splitmix64(&mut state) & 0xFF) as u8))
                .collect();
            for out_dim in [PHASH_LOWFREQ, N] {
                let mut tmp = vec![0.0_f64; N * out_dim];
                let mut got = vec![0.0_f64; out_dim * out_dim];
                simd::dct2d_lowblock(&grid, N, basis, out_dim, &mut tmp, &mut got);
                let want = dct_lowblock_scalar(&grid, out_dim);
                for (g, w) in got.iter().zip(&want) {
                    assert_eq!(g.to_bits(), w.to_bits(), "out_dim={out_dim}");
                }
            }
        }
    }

    #[test]
    fn downscale_integer_matches_f64_accumulation() {
        fn downscale_f64(f: &GrayFrame<'_>) -> Vec<f64> {
            let w = f.width as usize;
            let h = f.height as usize;
            let mut out = vec![0.0_f64; PHASH_GRID * PHASH_GRID];
            for ty in 0..PHASH_GRID {
                let y0 = ty * h / PHASH_GRID;
                let y1 = (((ty + 1) * h / PHASH_GRID).max(y0 + 1)).min(h);
                for tx in 0..PHASH_GRID {
                    let x0 = tx * w / PHASH_GRID;
                    let x1 = (((tx + 1) * w / PHASH_GRID).max(x0 + 1)).min(w);
                    let mut sum = 0.0_f64;
                    let mut cnt = 0u32;
                    for sy in y0..y1 {
                        let row = sy * w;
                        for sx in x0..x1 {
                            sum += f64::from(f.pixels.get(row + sx).copied().unwrap_or(0));
                            cnt += 1;
                        }
                    }
                    out[ty * PHASH_GRID + tx] = if cnt > 0 { sum / f64::from(cnt) } else { 0.0 };
                }
            }
            out
        }

        let mut state = 0xFACE_0FF1_CE00_0001;
        for &(w, h) in &[(96u32, 72u32), (33, 31), (8, 8), (200, 1)] {
            let px: Vec<u8> = (0..w as usize * h as usize)
                .map(|_| (splitmix64(&mut state) & 0xFF) as u8)
                .collect();
            let f = GrayFrame {
                width: w,
                height: h,
                pixels: &px,
            };
            assert_eq!(downscale(&f), downscale_f64(&f), "{w}x{h}");
        }
        let short = vec![201u8; 500];
        let f = GrayFrame {
            width: 64,
            height: 64,
            pixels: &short,
        };
        assert_eq!(downscale(&f), downscale_f64(&f), "truncated");
    }

    #[test]
    fn hamming_distance_batch_matches_scalar_loop() {
        let mut state = 0x9E37_79B9_0000_0001;
        let query = splitmix64(&mut state);
        let hashes: Vec<u64> = (0..101).map(|_| splitmix64(&mut state)).collect();
        let got = hamming_distance_batch(query, &hashes);
        let want: Vec<u32> = hashes.iter().map(|&h| hamming_distance(query, h)).collect();
        assert_eq!(got, want);
        assert!(hamming_distance_batch(query, &[]).is_empty());
    }

    #[test]
    fn gop_signature_rounds_mean_via_integer_division() {
        let sig = GopSignature::from_durations(&[10, 11]);
        assert_eq!(sig.keyframe_count, 2);
        assert_eq!(sig.mean_gop_ms, 10);
        assert_eq!(sig.max_gop_ms, 11);
    }

    #[test]
    fn gop_signature_saturates_huge_durations() {
        let sig = GopSignature::from_durations(&[u64::MAX]);
        assert_eq!(sig.keyframe_count, 1);
        assert_eq!(sig.mean_gop_ms, u32::MAX);
        assert_eq!(sig.max_gop_ms, u32::MAX);
    }

    #[test]
    fn uniform_frame_hashes_to_zero() {
        let pixels = vec![128u8; 64 * 64];
        let f = GrayFrame {
            width: 64,
            height: 64,
            pixels: &pixels,
        };
        assert_eq!(phash_frames(&[f]), 0);
    }

    #[test]
    fn downscale_handles_dimensions_below_grid() {
        let pixels: Vec<u8> = (0u8..64).collect();
        let f = GrayFrame {
            width: 8,
            height: 8,
            pixels: &pixels,
        };
        let grid = downscale(&f);
        assert_eq!(grid.len(), PHASH_GRID * PHASH_GRID);
    }

    #[test]
    fn downscale_tolerates_short_pixel_buffer() {
        let pixels = vec![200u8; 100];
        let f = GrayFrame {
            width: 32,
            height: 32,
            pixels: &pixels,
        };
        let grid = downscale(&f);
        assert_eq!(grid.len(), PHASH_GRID * PHASH_GRID);
    }

    #[test]
    fn phash_is_deterministic() {
        let pixels: Vec<u8> = (0..(48u32 * 48)).map(|i| (i % 251) as u8).collect();
        let f = GrayFrame {
            width: 48,
            height: 48,
            pixels: &pixels,
        };
        assert_eq!(phash_frames(&[f]), phash_frames(&[f]));
    }

    #[test]
    fn uniform_brightness_shift_leaves_hash_unchanged() {
        let base: Vec<u8> = (0..(64u32 * 64)).map(|i| (i % 120) as u8).collect();
        let brighter: Vec<u8> = base.iter().map(|&p| p + 40).collect();
        let fb = GrayFrame {
            width: 64,
            height: 64,
            pixels: &base,
        };
        let fs = GrayFrame {
            width: 64,
            height: 64,
            pixels: &brighter,
        };
        assert_eq!(phash_frames(&[fb]), phash_frames(&[fs]));
    }

    #[test]
    fn test_laplacian_variance_and_dct_energy() {
        let pixels: Vec<u8> = (0..(64 * 64))
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let f = GrayFrame {
            width: 64,
            height: 64,
            pixels: &pixels,
        };
        let lv = laplacian_variance(&f);
        let de = dct_energy(&f);
        assert!(lv > 0.0);
        assert!(de > 0.0);
    }

    #[test]
    fn tier1_builder_is_byte_identical_to_build_tier1() {
        use crate::format::encode_tier1;
        use vidcull_core::types::{Codec, VideoDuration};

        let mut state = 0xDEAD_BEEF_1234_5678_u64;
        let (width, height) = (64_u32, 64_u32);
        let px_len = (width * height) as usize;
        let random = |state: &mut u64| -> Vec<u8> {
            (0..px_len)
                .map(|_| {
                    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                    let mut z = *state;
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                    ((z ^ (z >> 31)) & 0xFF) as u8
                })
                .collect()
        };
        let pix_a = random(&mut state);
        let pix_b = random(&mut state);
        let pix_c = random(&mut state);

        let empty: Vec<u8> = Vec::new();

        let frames = [
            GrayFrame {
                width,
                height,
                pixels: &pix_a,
            },
            GrayFrame {
                width,
                height,
                pixels: &pix_a,
            },
            GrayFrame {
                width,
                height,
                pixels: &pix_b,
            },
            GrayFrame {
                width: 0,
                height,
                pixels: &empty,
            },
            GrayFrame {
                width,
                height,
                pixels: &pix_c,
            },
        ];

        let duration = VideoDuration::from_millis(5000);
        let codec = Codec::H264;
        let gop_ms: &[u64] = &[1000, 1200, 800];
        let batch = build_tier1(duration, codec.clone(), gop_ms, &frames);

        let mut builder = Tier1Builder::new();
        for f in &frames {
            builder.push(f);
        }
        let streamed_phash = builder.finish();
        let streamed = crate::tier1::Tier1Fingerprint {
            duration_ms: duration.as_millis(),
            codec,
            gop: GopSignature::from_durations(gop_ms),
            global_phash: streamed_phash,
        };

        let batch_bytes = encode_tier1(&batch).unwrap();
        let streamed_bytes = encode_tier1(&streamed).unwrap();
        assert_eq!(
            batch_bytes, streamed_bytes,
            "Tier1Builder streaming fold must be byte-identical to build_tier1 batch"
        );
    }

    #[test]
    fn combined_tier1_push_reuses_the_byte_identical_frame_phash() {
        let pixels: Vec<u8> = (0..64 * 48)
            .map(|i| u8::try_from((i * 37) % 251).unwrap())
            .collect();
        let frame = GrayFrame {
            width: 64,
            height: 48,
            pixels: &pixels,
        };
        let expected = phash_frames(std::slice::from_ref(&frame));
        let expected_energy = dct_energy(&frame);
        let mut builder = Tier1Builder::new();
        let (reused, reused_energy) = builder
            .push_and_analyze(&frame)
            .expect("non-empty frame must produce analysis values");

        assert_eq!(reused, expected);
        assert_eq!(reused_energy.to_bits(), expected_energy.to_bits());
        assert_eq!(builder.finish(), expected);
    }

    #[test]
    fn laplacian_variance_and_dct_energy_monotonicity_and_edge_cases() {
        let pixels_blur = vec![128u8; 64 * 64];
        let f_blur = GrayFrame {
            width: 64,
            height: 64,
            pixels: &pixels_blur,
        };

        let mut pixels_sharp = vec![0u8; 64 * 64];
        for y in 0..64 {
            for x in 0..64 {
                if ((x / 8) + (y / 8)) % 2 == 0 {
                    pixels_sharp[y * 64 + x] = 255;
                }
            }
        }
        let f_sharp = GrayFrame {
            width: 64,
            height: 64,
            pixels: &pixels_sharp,
        };

        let lv_sharp = laplacian_variance(&f_sharp);
        let lv_blur = laplacian_variance(&f_blur);
        assert!(
            lv_sharp > lv_blur,
            "sharp frame variance ({lv_sharp}) must be greater than blur ({lv_blur})"
        );

        let de_sharp = dct_energy(&f_sharp);
        let de_blur = dct_energy(&f_blur);
        assert!(
            de_sharp > de_blur,
            "high frequency frame energy ({de_sharp}) must be greater than flat ({de_blur})"
        );

        let pixels_small = vec![128u8; 4];
        let f_small_w = GrayFrame {
            width: 2,
            height: 2,
            pixels: &pixels_small,
        };
        assert_eq!(laplacian_variance(&f_small_w), 0.0);

        let f_zero = GrayFrame {
            width: 0,
            height: 64,
            pixels: &pixels_blur,
        };
        assert_eq!(laplacian_variance(&f_zero), 0.0);
        assert_eq!(dct_energy(&f_zero), 0.0);

        let lv1 = laplacian_variance(&f_sharp);
        let lv2 = laplacian_variance(&f_sharp);
        assert_eq!(lv1.to_bits(), lv2.to_bits());

        let de1 = dct_energy(&f_sharp);
        let de2 = dct_energy(&f_sharp);
        assert_eq!(de1.to_bits(), de2.to_bits());
    }

    fn fnv1a_bits(values: &[f64]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &v in values {
            h ^= v.to_bits();
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    fn golden_frame_pixels() -> Vec<u8> {
        let mut state = 0x1357_9BDF_2468_ACE0u64;
        (0..80usize * 60)
            .map(|_| (splitmix64(&mut state) & 0xFF) as u8)
            .collect()
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn golden_downscale_to_bits() {
        let px = golden_frame_pixels();
        let f = GrayFrame {
            width: 80,
            height: 60,
            pixels: &px,
        };
        let bits = fnv1a_bits(&downscale(&f));
        assert_eq!(
            bits, 0x4bcd_ff19_5f9c_b093,
            "downscale golden drifted; actual = 0x{bits:016x}"
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn golden_dct2d_lowblock_to_bits() {
        const N: usize = PHASH_GRID;
        let basis = dct_basis();
        let mut state = 0x0F1E_2D3C_4B5A_6978u64;
        let grid: Vec<f64> = (0..N * N)
            .map(|_| f64::from((splitmix64(&mut state) & 0xFF) as u8))
            .collect();
        let mut tmp = vec![0.0_f64; N * N];
        let mut full = vec![0.0_f64; N * N];
        simd::dct2d_lowblock(&grid, N, basis, N, &mut tmp, &mut full);
        let bits = fnv1a_bits(&full);
        assert_eq!(
            bits, 0x2fd1_6c89_18bc_2c26,
            "dct2d_lowblock golden drifted; actual = 0x{bits:016x}"
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn golden_laplacian_variance_to_bits() {
        let px = golden_frame_pixels();
        let f = GrayFrame {
            width: 80,
            height: 60,
            pixels: &px,
        };
        let bits = laplacian_variance(&f).to_bits();
        assert_eq!(
            bits, 0x40fa_d9bb_ee2e_ee28,
            "laplacian_variance golden drifted; actual = 0x{bits:016x}"
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn golden_dct_energy_to_bits() {
        let px = golden_frame_pixels();
        let f = GrayFrame {
            width: 80,
            height: 60,
            pixels: &px,
        };
        let bits = dct_energy(&f).to_bits();
        assert_eq!(
            bits, 0x41b4_038d_bddf_800f,
            "dct_energy golden drifted; actual = 0x{bits:016x}"
        );
    }
}
