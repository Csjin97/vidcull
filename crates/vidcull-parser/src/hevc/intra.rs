#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::needless_range_loop
)]

const INTRA_PLANAR: u8 = 0;
const INTRA_DC: u8 = 1;
const INTRA_ANGULAR_10: u8 = 10;
const INTRA_ANGULAR_18: u8 = 18;
const INTRA_ANGULAR_26: u8 = 26;

#[rustfmt::skip]
const INTRA_PRED_ANGLE: [i32; 35] = [
    0, 0,
    32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26,
    -32, -26, -21, -17, -13, -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
];

#[rustfmt::skip]
const INV_ANGLE: [i32; 35] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096,
    0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const HOR_VER_DIST_THRES: [i32; 6] = [0, 0, 0, 7, 1, 0];

#[derive(Debug, Clone)]
pub struct RefSamples {
    pub n: usize,
    corner: i32,
    top: Vec<i32>,
    left: Vec<i32>,
}

impl RefSamples {
    #[inline]
    fn p_top(&self, x: i32) -> i32 {
        if x < 0 {
            self.corner
        } else {
            self.top[x as usize]
        }
    }

    #[inline]
    fn p_left(&self, y: i32) -> i32 {
        if y < 0 {
            self.corner
        } else {
            self.left[y as usize]
        }
    }
}

#[inline]
fn clip1(v: i32, bit_depth: u8) -> i32 {
    v.clamp(0, (1 << bit_depth) - 1)
}

#[must_use]
pub fn build_references(
    n: usize,
    corner: Option<u8>,
    top: &[Option<u8>],
    left: &[Option<u8>],
    bit_depth: u8,
) -> RefSamples {
    assert_eq!(top.len(), 2 * n, "top must hold 2·n samples");
    assert_eq!(left.len(), 2 * n, "left must hold 2·n samples");

    let scan: Vec<Option<u8>> = left
        .iter()
        .rev()
        .copied()
        .chain(core::iter::once(corner))
        .chain(top.iter().copied())
        .collect();

    let first_avail = scan.iter().flatten().next().copied();
    let Some(first) = first_avail else {
        let mid = 1i32 << (bit_depth - 1);
        return RefSamples {
            n,
            corner: mid,
            top: vec![mid; 2 * n],
            left: vec![mid; 2 * n],
        };
    };

    let mut filled = Vec::with_capacity(scan.len());
    let mut prev = i32::from(first);
    for s in &scan {
        if let Some(v) = s {
            prev = i32::from(*v);
        }
        filled.push(prev);
    }

    let left_rev = &filled[..2 * n];
    let corner = filled[2 * n];
    let top_vals = &filled[2 * n + 1..];
    let mut left = vec![0i32; 2 * n];
    for (i, v) in left_rev.iter().rev().enumerate() {
        left[i] = *v;
    }
    RefSamples {
        n,
        corner,
        top: top_vals.to_vec(),
        left,
    }
}

#[must_use]
pub fn filter_flag(mode: u8, n: usize) -> bool {
    if mode == INTRA_DC || n == 4 {
        return false;
    }
    let log2 = n.trailing_zeros() as usize;
    let min_dist = (i32::from(mode) - i32::from(INTRA_ANGULAR_26))
        .abs()
        .min((i32::from(mode) - i32::from(INTRA_ANGULAR_10)).abs());
    min_dist > HOR_VER_DIST_THRES[log2]
}

pub fn filter_references(
    refs: &mut RefSamples,
    mode: u8,
    is_luma: bool,
    strong_smoothing: bool,
    bit_depth: u8,
) {
    let n = refs.n;
    if !filter_flag(mode, n) {
        return;
    }

    let thres = 1i32 << (i32::from(bit_depth) - 5);
    let bi_int = strong_smoothing
        && is_luma
        && n == 32
        && (refs.corner + refs.top[2 * n - 1] - 2 * refs.top[n - 1]).abs() < thres
        && (refs.corner + refs.left[2 * n - 1] - 2 * refs.left[n - 1]).abs() < thres;

    if bi_int {
        let corner = refs.corner;
        let top_far = refs.top[2 * n - 1];
        let left_far = refs.left[2 * n - 1];
        let mut top = vec![0i32; 2 * n];
        let mut left = vec![0i32; 2 * n];
        for x in 0..2 * n - 1 {
            let k = x as i32;
            top[x] = ((63 - k) * corner + (k + 1) * top_far + 32) >> 6;
            left[x] = ((63 - k) * corner + (k + 1) * left_far + 32) >> 6;
        }
        top[2 * n - 1] = top_far;
        left[2 * n - 1] = left_far;
        refs.top = top;
        refs.left = left;
        return;
    }

    let old_corner = refs.corner;
    let old_top = refs.top.clone();
    let old_left = refs.left.clone();

    refs.corner = (old_left[0] + 2 * old_corner + old_top[0] + 2) >> 2;

    for x in 0..2 * n - 1 {
        let prev = if x == 0 { old_corner } else { old_top[x - 1] };
        refs.top[x] = (prev + 2 * old_top[x] + old_top[x + 1] + 2) >> 2;
    }
    for y in 0..2 * n - 1 {
        let prev = if y == 0 { old_corner } else { old_left[y - 1] };
        refs.left[y] = (prev + 2 * old_left[y] + old_left[y + 1] + 2) >> 2;
    }
}

#[must_use]
pub fn predict(mode: u8, refs: &RefSamples, is_luma: bool, bit_depth: u8) -> Vec<i32> {
    assert!(mode <= 34, "intra mode {mode} out of range");
    match mode {
        INTRA_PLANAR => predict_planar(refs),
        INTRA_DC => predict_dc(refs, is_luma),
        _ => predict_angular(mode, refs, is_luma, bit_depth),
    }
}

fn predict_planar(refs: &RefSamples) -> Vec<i32> {
    let n = refs.n;
    let log2 = n.trailing_zeros();
    let ni = n as i32;
    let mut out = vec![0i32; n * n];
    let top_ne = refs.top[n];
    let left_sw = refs.left[n];
    for y in 0..n {
        let yi = y as i32;
        for x in 0..n {
            let xi = x as i32;
            let v = (ni - 1 - xi) * refs.left[y]
                + (xi + 1) * top_ne
                + (ni - 1 - yi) * refs.top[x]
                + (yi + 1) * left_sw
                + ni;
            out[y * n + x] = v >> (log2 + 1);
        }
    }
    out
}

fn predict_dc(refs: &RefSamples, is_luma: bool) -> Vec<i32> {
    let n = refs.n;
    let log2 = n.trailing_zeros();
    let mut sum = n as i32;
    for i in 0..n {
        sum += refs.top[i] + refs.left[i];
    }
    let dc = sum >> (log2 + 1);

    let mut out = vec![dc; n * n];
    if is_luma && n < 32 {
        out[0] = (refs.left[0] + 2 * dc + refs.top[0] + 2) >> 2;
        for x in 1..n {
            out[x] = (refs.top[x] + 3 * dc + 2) >> 2;
        }
        for y in 1..n {
            out[y * n] = (refs.left[y] + 3 * dc + 2) >> 2;
        }
    }
    out
}

fn predict_angular(mode: u8, refs: &RefSamples, is_luma: bool, bit_depth: u8) -> Vec<i32> {
    let n = refs.n;
    let ni = n as i32;
    let angle = INTRA_PRED_ANGLE[mode as usize];
    let inv_angle = INV_ANGLE[mode as usize];
    let mut out = vec![0i32; n * n];

    let off = ni;
    let mut r = vec![0i32; 3 * n + 2];
    let idx = |i: i32| (i + off) as usize;

    if mode >= INTRA_ANGULAR_18 {
        for x in 0..=ni {
            r[idx(x)] = refs.p_top(x - 1);
        }
        if angle < 0 {
            let min = (ni * angle) >> 5;
            if min < -1 {
                for x in (min..=-1).rev() {
                    let p = -1 + ((x * inv_angle + 128) >> 8);
                    r[idx(x)] = refs.p_left(p);
                }
            }
        } else {
            for x in ni + 1..=2 * ni {
                r[idx(x)] = refs.p_top(x - 1);
            }
        }
        for y in 0..n {
            let pos = (y as i32 + 1) * angle;
            let i_idx = pos >> 5;
            let i_fact = pos & 31;
            for x in 0..n {
                let base = x as i32 + i_idx;
                let v = ((32 - i_fact) * r[idx(base + 1)] + i_fact * r[idx(base + 2)] + 16) >> 5;
                out[y * n + x] = v;
            }
        }
        if mode == INTRA_ANGULAR_26 && is_luma && n < 32 {
            for y in 0..n {
                let v = refs.top[0] + ((refs.left[y] - refs.corner) >> 1);
                out[y * n] = clip1(v, bit_depth);
            }
        }
    } else {
        for y in 0..=ni {
            r[idx(y)] = refs.p_left(y - 1);
        }
        if angle < 0 {
            let min = (ni * angle) >> 5;
            if min < -1 {
                for y in (min..=-1).rev() {
                    let p = -1 + ((y * inv_angle + 128) >> 8);
                    r[idx(y)] = refs.p_top(p);
                }
            }
        } else {
            for y in ni + 1..=2 * ni {
                r[idx(y)] = refs.p_left(y - 1);
            }
        }
        for x in 0..n {
            let pos = (x as i32 + 1) * angle;
            let i_idx = pos >> 5;
            let i_fact = pos & 31;
            for y in 0..n {
                let base = y as i32 + i_idx;
                let v = ((32 - i_fact) * r[idx(base + 1)] + i_fact * r[idx(base + 2)] + 16) >> 5;
                out[y * n + x] = v;
            }
        }
        if mode == INTRA_ANGULAR_10 && is_luma && n < 32 {
            for x in 0..n {
                let v = refs.left[0] + ((refs.top[x] - refs.corner) >> 1);
                out[x] = clip1(v, bit_depth);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_refs(n: usize, v: u8) -> RefSamples {
        let edge = vec![Some(v); 2 * n];
        build_references(n, Some(v), &edge, &edge, 8)
    }

    #[test]
    fn substitution_all_unavailable_is_mid_grey() {
        let refs = build_references(4, None, &[None; 8], &[None; 8], 8);
        assert_eq!(refs.corner, 128);
        assert!(refs.top.iter().all(|&v| v == 128));
        assert!(refs.left.iter().all(|&v| v == 128));
    }

    #[test]
    fn substitution_propagates_from_first_available() {
        let mut left = [None; 8];
        left[7] = Some(50);
        let refs = build_references(4, None, &[None; 8], &left, 8);
        assert!(refs.left.iter().all(|&v| v == 50));
        assert_eq!(refs.corner, 50);
        assert!(refs.top.iter().all(|&v| v == 50));
    }

    #[test]
    fn substitution_fills_gaps() {
        let left = [Some(10), None, Some(30), None, None, None, None, Some(80)];
        let corner = None;
        let top = [Some(200), None, None, Some(150), None, None, None, None];
        let refs = build_references(4, corner, &top, &left, 8);
        assert_eq!(refs.left, [10, 30, 30, 80, 80, 80, 80, 80]);
        assert_eq!(refs.corner, 10);
        assert_eq!(refs.top, [200, 200, 200, 150, 150, 150, 150, 150]);
    }

    #[test]
    fn filter_flag_decision() {
        assert!(!filter_flag(0, 4));
        assert!(!filter_flag(18, 4));
        assert!(!filter_flag(INTRA_DC, 32));
        assert!(!filter_flag(26, 8));
        assert!(!filter_flag(10, 16));
        assert!(filter_flag(0, 8));
        assert!(filter_flag(0, 16));
        assert!(filter_flag(0, 32));
        assert!(filter_flag(18, 8));
        assert!(!filter_flag(9, 8));
        assert!(!filter_flag(9, 16));
        assert!(filter_flag(8, 16));
        assert!(filter_flag(9, 32));
    }

    #[test]
    fn three_tap_filter_hand_value() {
        let mut left = [None; 16];
        let mut top = [None; 16];
        for i in 0..16 {
            left[i] = Some((i * 10) as u8);
            top[i] = Some(100);
        }
        let mut refs = build_references(8, Some(60), &top, &left, 8);
        filter_references(&mut refs, 0, true, false, 8);
        assert_eq!(refs.left[0], 18);
        assert_eq!(refs.left[1], 10);
        assert_eq!(refs.left[15], 150);
        assert_eq!(refs.corner, 55);
    }

    #[test]
    fn dc_flat_is_flat() {
        let refs = flat_refs(8, 120);
        let out = predict(INTRA_DC, &refs, true, 8);
        assert!(out.iter().all(|&v| v == 120), "{out:?}");
    }

    #[test]
    fn dc_average() {
        let refs = build_references(8, Some(0), &[Some(100); 16], &[Some(200); 16], 8);
        let out = predict(INTRA_DC, &refs, false, 8);
        assert!(out.iter().all(|&v| v == 150), "dc={}", out[0]);
    }

    #[test]
    fn planar_flat_is_flat() {
        let refs = flat_refs(16, 77);
        let out = predict(INTRA_PLANAR, &refs, true, 8);
        assert!(out.iter().all(|&v| v == 77));
    }

    #[test]
    fn vertical_copies_top() {
        let n = 32;
        let mut top = vec![None; 2 * n];
        for (x, t) in top.iter_mut().enumerate().take(n) {
            *t = Some((x as u8).wrapping_mul(3));
        }
        for t in top.iter_mut().skip(n) {
            *t = Some(0);
        }
        let refs = build_references(n, Some(0), &top, &vec![Some(0); 2 * n], 8);
        let out = predict(26, &refs, true, 8);
        for y in 0..n {
            for x in 0..n {
                assert_eq!(out[y * n + x], refs.top[x], "({x},{y})");
            }
        }
    }

    #[test]
    fn horizontal_copies_left() {
        let n = 32;
        let mut left = vec![None; 2 * n];
        for (y, l) in left.iter_mut().enumerate().take(n) {
            *l = Some((y as u8).wrapping_mul(5));
        }
        for l in left.iter_mut().skip(n) {
            *l = Some(0);
        }
        let refs = build_references(n, Some(0), &vec![Some(0); 2 * n], &left, 8);
        let out = predict(10, &refs, true, 8);
        for y in 0..n {
            for x in 0..n {
                assert_eq!(out[y * n + x], refs.left[y], "({x},{y})");
            }
        }
    }

    #[test]
    fn angular_34_diagonal() {
        let n = 4;
        let top: Vec<Option<u8>> = (0..2 * n).map(|x| Some(x as u8)).collect();
        let refs = build_references(n, Some(0), &top, &vec![Some(0); 2 * n], 8);
        let out = predict(34, &refs, true, 8);
        for y in 0..n {
            for x in 0..n {
                let expect = refs.top[x + y + 1];
                assert_eq!(out[y * n + x], expect, "({x},{y})");
            }
        }
    }

    #[test]
    fn predictions_in_range() {
        for &n in &[4usize, 8, 16, 32] {
            let top: Vec<Option<u8>> = (0..2 * n).map(|x| Some((x * 7 % 256) as u8)).collect();
            let left: Vec<Option<u8>> = (0..2 * n).map(|y| Some((y * 13 % 256) as u8)).collect();
            for mode in 0u8..=34 {
                let mut refs = build_references(n, Some(64), &top, &left, 8);
                filter_references(&mut refs, mode, true, true, 8);
                let out = predict(mode, &refs, true, 8);
                assert!(
                    out.iter().all(|&v| (0..=255).contains(&v)),
                    "mode {mode} n {n} out of range"
                );
                assert_eq!(out.len(), n * n);
            }
        }
    }
}
