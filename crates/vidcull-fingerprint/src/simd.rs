use wide::f64x4;

// This crate is built under this workspace's .cargo/config.toml, which pins
// `-C target-cpu=x86-64-v3` for all x86_64 builds — that microarchitecture
// level guarantees hardware POPCNT, so plain `count_ones()` already compiles
// to a single POPCNT instruction. Four independent POPCNT calls (no data
// dependency between them, so the CPU can issue them back-to-back/in
// parallel) measurably beat one dependent ~15-op vectorized bit-trick chain
// per 4-wide group: `cargo bench -p vidcull-fingerprint -- hamming_corpus`
// showed the old u64x4 popcount_x4 version at ~0.8 Gelem/s vs ~1.6 Gelem/s
// for this plain scalar loop, on this build. Not worth hand-vectorizing.
pub(crate) fn hamming_batch(query: u64, hashes: &[u64], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(hashes.len());
    for &h in hashes {
        out.push((query ^ h).count_ones());
    }
}

fn dct_pass(
    in_: &[f64],
    rows: usize,
    n: usize,
    basis: &[f64],
    basis_stride: usize,
    out_dim: usize,
    out: &mut [f64],
) {
    debug_assert!(
        out_dim % 4 == 0,
        "out_dim must be a multiple of the lane width"
    );
    for r in 0..rows {
        let row = &in_[r * n..r * n + n];
        let dst = &mut out[r * out_dim..r * out_dim + out_dim];
        let mut vb = 0;
        while vb < out_dim {
            let mut acc = f64x4::new([0.0; 4]);
            for (k, &g) in row.iter().enumerate() {
                let bk = &basis[k * basis_stride..];
                let b = f64x4::new([bk[vb], bk[vb + 1], bk[vb + 2], bk[vb + 3]]);
                acc += f64x4::new([g; 4]) * b;
            }
            let a = acc.to_array();
            dst[vb..vb + 4].copy_from_slice(&a);
            vb += 4;
        }
    }
}

pub(crate) fn dct2d_lowblock(
    grid: &[f64],
    n: usize,
    basis: &[f64],
    out_dim: usize,
    tmp: &mut [f64],
    out: &mut [f64],
) {
    dct_pass(grid, n, n, basis, n, out_dim, tmp);
    debug_assert!(out_dim % 4 == 0);
    for u in 0..out_dim {
        let dst = &mut out[u * out_dim..u * out_dim + out_dim];
        let mut vb = 0;
        while vb < out_dim {
            let mut acc = f64x4::new([0.0; 4]);
            for x in 0..n {
                let t = &tmp[x * out_dim..];
                let tv = f64x4::new([t[vb], t[vb + 1], t[vb + 2], t[vb + 3]]);
                acc += f64x4::new([basis[x * n + u]; 4]) * tv;
            }
            let a = acc.to_array();
            dst[vb..vb + 4].copy_from_slice(&a);
            vb += 4;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[test]
    fn hamming_batch_matches_scalar_including_tail() {
        let mut s = 0x0123_4567_89AB_CDEF;
        for len in [0usize, 1, 3, 4, 7, 16, 33] {
            let hashes: Vec<u64> = (0..len).map(|_| splitmix64(&mut s)).collect();
            let query = splitmix64(&mut s);
            let mut out = Vec::new();
            hamming_batch(query, &hashes, &mut out);
            let want: Vec<u32> = hashes.iter().map(|&h| (query ^ h).count_ones()).collect();
            assert_eq!(out, want, "len={len}");
        }
    }
}
