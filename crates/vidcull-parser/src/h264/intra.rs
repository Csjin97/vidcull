#[derive(Debug, Clone, Copy, Default)]
pub struct Neighbors4x4 {
    pub top: Option<[u8; 4]>,
    pub top_right: Option<[u8; 4]>,
    pub left: Option<[u8; 4]>,
    pub top_left: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Neighbors8x8 {
    pub top: Option<[u8; 8]>,
    pub top_right: Option<[u8; 8]>,
    pub left: Option<[u8; 8]>,
    pub top_left: Option<u8>,
}

fn clamp_u8(v: i32) -> u8 {
    u8::try_from(v.clamp(0, 255)).expect("value clamped into 0..=255")
}

#[must_use]
pub fn predict_4x4(mode: u8, n: &Neighbors4x4) -> [u8; 16] {
    let top = n.top.unwrap_or_default();
    let tr = n.top_right.unwrap_or([top[3]; 4]);
    let p_top: [i32; 8] = [
        i32::from(top[0]),
        i32::from(top[1]),
        i32::from(top[2]),
        i32::from(top[3]),
        i32::from(tr[0]),
        i32::from(tr[1]),
        i32::from(tr[2]),
        i32::from(tr[3]),
    ];
    let left = n.left.unwrap_or_default();
    let p_left: [i32; 4] = [
        i32::from(left[0]),
        i32::from(left[1]),
        i32::from(left[2]),
        i32::from(left[3]),
    ];
    let p_tl = i32::from(n.top_left.unwrap_or(0));
    let top_avail = n.top.is_some();
    let left_avail = n.left.is_some();
    predict_4x4_mode(mode, &p_top, &p_left, p_tl, top_avail, left_avail)
}

#[allow(clippy::many_single_char_names)]
#[allow(clippy::too_many_lines)]
fn predict_4x4_mode(
    mode: u8,
    p_top: &[i32; 8],
    p_left: &[i32; 4],
    p_tl: i32,
    top_avail: bool,
    left_avail: bool,
) -> [u8; 16] {
    let t = |x: i32| {
        if x < 0 {
            p_tl
        } else {
            p_top[usize::try_from(x).expect("x>=0")]
        }
    };
    let l = |y: i32| {
        if y < 0 {
            p_tl
        } else {
            p_left[usize::try_from(y).expect("y>=0")]
        }
    };

    let mut pred = [0u8; 16];
    let mut put = |x: i32, y: i32, v: i32| {
        let idx = usize::try_from(y * 4 + x).expect("0..16");
        pred[idx] = clamp_u8(v);
    };

    match mode {
        0 => {
            for y in 0..4 {
                for x in 0..4 {
                    put(x, y, t(x));
                }
            }
        }
        1 => {
            for y in 0..4 {
                for x in 0..4 {
                    put(x, y, l(y));
                }
            }
        }
        2 => {
            let dc = dc_4x4(top_avail, p_top, left_avail, p_left);
            for y in 0..4 {
                for x in 0..4 {
                    put(x, y, dc);
                }
            }
        }
        3 => {
            for y in 0..4 {
                for x in 0..4 {
                    let v = if x == 3 && y == 3 {
                        (t(6) + 3 * t(7) + 2) >> 2
                    } else {
                        let i = x + y;
                        (t(i) + 2 * t(i + 1) + t(i + 2) + 2) >> 2
                    };
                    put(x, y, v);
                }
            }
        }
        4 => {
            for y in 0..4 {
                for x in 0..4 {
                    let v = match x - y {
                        z if z > 0 => (t(x - y - 2) + 2 * t(x - y - 1) + t(x - y) + 2) >> 2,
                        z if z < 0 => (l(y - x - 2) + 2 * l(y - x - 1) + l(y - x) + 2) >> 2,
                        _ => (t(0) + 2 * p_tl + l(0) + 2) >> 2,
                    };
                    put(x, y, v);
                }
            }
        }
        5 => {
            for y in 0..4 {
                for x in 0..4 {
                    let z = 2 * x - y;
                    let xh = x - (y >> 1);
                    let v = match z {
                        0 | 2 | 4 | 6 => (t(xh - 1) + t(xh) + 1) >> 1,
                        1 | 3 | 5 => (t(xh - 2) + 2 * t(xh - 1) + t(xh) + 2) >> 2,
                        -1 => (l(0) + 2 * p_tl + t(0) + 2) >> 2,
                        _ => (l(y - 2 * x - 1) + 2 * l(y - 2 * x - 2) + l(y - 2 * x - 3) + 2) >> 2,
                    };
                    put(x, y, v);
                }
            }
        }
        6 => {
            for y in 0..4 {
                for x in 0..4 {
                    let z = 2 * y - x;
                    let yh = y - (x >> 1);
                    let v = match z {
                        0 | 2 | 4 | 6 => (l(yh - 1) + l(yh) + 1) >> 1,
                        1 | 3 | 5 => (l(yh - 2) + 2 * l(yh - 1) + l(yh) + 2) >> 2,
                        -1 => (l(0) + 2 * p_tl + t(0) + 2) >> 2,
                        _ => (t(x - 2 * y - 1) + 2 * t(x - 2 * y - 2) + t(x - 2 * y - 3) + 2) >> 2,
                    };
                    put(x, y, v);
                }
            }
        }
        7 => {
            for y in 0..4 {
                for x in 0..4 {
                    let xh = x + (y >> 1);
                    let v = if y % 2 == 0 {
                        (t(xh) + t(xh + 1) + 1) >> 1
                    } else {
                        (t(xh) + 2 * t(xh + 1) + t(xh + 2) + 2) >> 2
                    };
                    put(x, y, v);
                }
            }
        }
        8 => {
            for y in 0..4 {
                for x in 0..4 {
                    let z = x + 2 * y;
                    let yh = y + (x >> 1);
                    let v = if z < 5 && z % 2 == 0 {
                        (l(yh) + l(yh + 1) + 1) >> 1
                    } else if z < 5 {
                        (l(yh) + 2 * l(yh + 1) + l(yh + 2) + 2) >> 2
                    } else if z == 5 {
                        (l(2) + 3 * l(3) + 2) >> 2
                    } else {
                        l(3)
                    };
                    put(x, y, v);
                }
            }
        }
        _ => {}
    }
    pred
}

fn dc_4x4(top_avail: bool, p_top: &[i32; 8], left_avail: bool, p_left: &[i32; 4]) -> i32 {
    let sum_top: i32 = p_top[0..4].iter().sum();
    let sum_left: i32 = p_left.iter().sum();
    match (top_avail, left_avail) {
        (true, true) => (sum_top + sum_left + 4) >> 3,
        (true, false) => (sum_top + 2) >> 2,
        (false, true) => (sum_left + 2) >> 2,
        (false, false) => 128,
    }
}

#[must_use]
pub fn predict_8x8(mode: u8, n: &Neighbors8x8) -> [u8; 64] {
    let refs = Refs8x8::filtered(n);
    let top_avail = n.top.is_some();
    let left_avail = n.left.is_some();
    predict_8x8_mode(mode, &refs, top_avail, left_avail)
}

#[allow(clippy::many_single_char_names)]
#[allow(clippy::too_many_lines)]
fn predict_8x8_mode(mode: u8, refs: &Refs8x8, top_avail: bool, left_avail: bool) -> [u8; 64] {
    let mut pred = [0u8; 64];
    let mut put = |x: i32, y: i32, v: i32| {
        let idx = usize::try_from(y * 8 + x).expect("0..64");
        pred[idx] = clamp_u8(v);
    };

    let t = |x: i32| {
        if x < 0 {
            refs.top_left
        } else {
            refs.top[usize::try_from(x).expect("x>=0")]
        }
    };
    let l = |y: i32| {
        if y < 0 {
            refs.top_left
        } else {
            refs.left[usize::try_from(y).expect("y>=0")]
        }
    };
    let tl = refs.top_left;

    match mode {
        0 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, t(x));
                }
            }
        }
        1 => {
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, l(y));
                }
            }
        }
        2 => {
            let sum_top: i32 = refs.top[0..8].iter().sum();
            let sum_left: i32 = refs.left.iter().sum();
            let dc = match (top_avail, left_avail) {
                (true, true) => (sum_top + sum_left + 8) >> 4,
                (true, false) => (sum_top + 4) >> 3,
                (false, true) => (sum_left + 4) >> 3,
                (false, false) => 128,
            };
            for y in 0..8 {
                for x in 0..8 {
                    put(x, y, dc);
                }
            }
        }
        3 => {
            for y in 0..8 {
                for x in 0..8 {
                    let v = if x == 7 && y == 7 {
                        (t(14) + 3 * t(15) + 2) >> 2
                    } else {
                        let i = x + y;
                        (t(i) + 2 * t(i + 1) + t(i + 2) + 2) >> 2
                    };
                    put(x, y, v);
                }
            }
        }
        4 => {
            for y in 0..8 {
                for x in 0..8 {
                    let v = match x - y {
                        z if z > 0 => (t(x - y - 2) + 2 * t(x - y - 1) + t(x - y) + 2) >> 2,
                        z if z < 0 => (l(y - x - 2) + 2 * l(y - x - 1) + l(y - x) + 2) >> 2,
                        _ => (t(0) + 2 * tl + l(0) + 2) >> 2,
                    };
                    put(x, y, v);
                }
            }
        }
        5 => {
            for y in 0..8 {
                for x in 0..8 {
                    let z = 2 * x - y;
                    let xh = x - (y >> 1);
                    let v = if z >= 0 && z % 2 == 0 {
                        (t(xh - 1) + t(xh) + 1) >> 1
                    } else if z >= 0 {
                        (t(xh - 2) + 2 * t(xh - 1) + t(xh) + 2) >> 2
                    } else if z == -1 {
                        (l(0) + 2 * tl + t(0) + 2) >> 2
                    } else {
                        (l(y - 2 * x - 1) + 2 * l(y - 2 * x - 2) + l(y - 2 * x - 3) + 2) >> 2
                    };
                    put(x, y, v);
                }
            }
        }
        6 => {
            for y in 0..8 {
                for x in 0..8 {
                    let z = 2 * y - x;
                    let yh = y - (x >> 1);
                    let v = if z >= 0 && z % 2 == 0 {
                        (l(yh - 1) + l(yh) + 1) >> 1
                    } else if z >= 0 {
                        (l(yh - 2) + 2 * l(yh - 1) + l(yh) + 2) >> 2
                    } else if z == -1 {
                        (l(0) + 2 * tl + t(0) + 2) >> 2
                    } else {
                        (t(x - 2 * y - 1) + 2 * t(x - 2 * y - 2) + t(x - 2 * y - 3) + 2) >> 2
                    };
                    put(x, y, v);
                }
            }
        }
        7 => {
            for y in 0..8 {
                for x in 0..8 {
                    let xh = x + (y >> 1);
                    let v = if y % 2 == 0 {
                        (t(xh) + t(xh + 1) + 1) >> 1
                    } else {
                        (t(xh) + 2 * t(xh + 1) + t(xh + 2) + 2) >> 2
                    };
                    put(x, y, v);
                }
            }
        }
        8 => {
            for y in 0..8 {
                for x in 0..8 {
                    let z = x + 2 * y;
                    let yh = y + (x >> 1);
                    let v = if z < 13 && z % 2 == 0 {
                        (l(yh) + l(yh + 1) + 1) >> 1
                    } else if z < 13 {
                        (l(yh) + 2 * l(yh + 1) + l(yh + 2) + 2) >> 2
                    } else if z == 13 {
                        (l(6) + 3 * l(7) + 2) >> 2
                    } else {
                        l(7)
                    };
                    put(x, y, v);
                }
            }
        }
        _ => {}
    }
    pred
}

struct Refs8x8 {
    top: [i32; 16],
    left: [i32; 8],
    top_left: i32,
}

impl Refs8x8 {
    fn filtered(n: &Neighbors8x8) -> Self {
        let top = n.top.unwrap_or_default();
        let tr = n.top_right.unwrap_or([top[7]; 8]);
        let mut rt = [0i32; 16];
        for x in 0..8 {
            rt[x] = i32::from(top[x]);
        }
        for x in 0..8 {
            rt[8 + x] = i32::from(tr[x]);
        }
        let left = n.left.unwrap_or_default();
        let mut rl = [0i32; 8];
        for (y, s) in left.iter().enumerate() {
            rl[y] = i32::from(*s);
        }
        let rtl = i32::from(n.top_left.unwrap_or(0));

        let top_avail = n.top.is_some();
        let left_avail = n.left.is_some();
        let tl_avail = n.top_left.is_some();

        let mut ft = [0i32; 16];
        if top_avail {
            ft[0] = if tl_avail {
                (rtl + 2 * rt[0] + rt[1] + 2) >> 2
            } else {
                (3 * rt[0] + rt[1] + 2) >> 2
            };
            for x in 1..15 {
                ft[x] = (rt[x - 1] + 2 * rt[x] + rt[x + 1] + 2) >> 2;
            }
            ft[15] = (rt[14] + 3 * rt[15] + 2) >> 2;
        }

        let mut fl = [0i32; 8];
        if left_avail {
            fl[0] = if tl_avail {
                (rtl + 2 * rl[0] + rl[1] + 2) >> 2
            } else {
                (3 * rl[0] + rl[1] + 2) >> 2
            };
            for y in 1..7 {
                fl[y] = (rl[y - 1] + 2 * rl[y] + rl[y + 1] + 2) >> 2;
            }
            fl[7] = (rl[6] + 3 * rl[7] + 2) >> 2;
        }

        let ftl = if tl_avail {
            let a = if top_avail { rt[0] } else { rtl };
            let b = if left_avail { rl[0] } else { rtl };
            (a + 2 * rtl + b + 2) >> 2
        } else {
            rtl
        };

        Refs8x8 {
            top: ft,
            left: fl,
            top_left: ftl,
        }
    }
}

#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn predict_16x16(
    mode: u8,
    top: Option<[u8; 16]>,
    left: Option<[u8; 16]>,
    top_left: Option<u8>,
) -> [u8; 256] {
    let rt = top.unwrap_or_default();
    let rl = left.unwrap_or_default();
    let p_top: [i32; 16] = core::array::from_fn(|i| i32::from(rt[i]));
    let p_left: [i32; 16] = core::array::from_fn(|i| i32::from(rl[i]));
    let p_tl = i32::from(top_left.unwrap_or(0));

    let mut pred = [0u8; 256];
    let mut put = |x: usize, y: usize, v: i32| pred[y * 16 + x] = clamp_u8(v);

    match mode {
        0 => {
            for y in 0..16 {
                for (x, &tv) in p_top.iter().enumerate() {
                    put(x, y, tv);
                }
            }
        }
        1 => {
            for (y, &lv) in p_left.iter().enumerate() {
                for x in 0..16 {
                    put(x, y, lv);
                }
            }
        }
        2 => {
            let sum_top: i32 = p_top.iter().sum();
            let sum_left: i32 = p_left.iter().sum();
            let dc = match (top.is_some(), left.is_some()) {
                (true, true) => (sum_top + sum_left + 16) >> 5,
                (true, false) => (sum_top + 8) >> 4,
                (false, true) => (sum_left + 8) >> 4,
                (false, false) => 128,
            };
            for y in 0..16 {
                for x in 0..16 {
                    put(x, y, dc);
                }
            }
        }
        3 => {
            let mut h = 0i32;
            for xp in 0..8usize {
                let right = p_top[8 + xp];
                let left_s = if xp == 7 { p_tl } else { p_top[6 - xp] };
                h += i32::try_from(xp + 1).expect("0..8 fits i32") * (right - left_s);
            }
            let mut v = 0i32;
            for yp in 0..8usize {
                let down = p_left[8 + yp];
                let up = if yp == 7 { p_tl } else { p_left[6 - yp] };
                v += i32::try_from(yp + 1).expect("0..8 fits i32") * (down - up);
            }
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            let a = 16 * (p_left[15] + p_top[15]);
            for y in 0..16i32 {
                for x in 0..16i32 {
                    let val = (a + b * (x - 7) + c * (y - 7) + 16) >> 5;
                    put(
                        usize::try_from(x).expect("0..16"),
                        usize::try_from(y).expect("0..16"),
                        val,
                    );
                }
            }
        }
        _ => {}
    }
    pred
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_4x4_replicates_top() {
        let n = Neighbors4x4 {
            top: Some([10, 20, 30, 40]),
            ..Default::default()
        };
        let p = predict_4x4(0, &n);
        for y in 0..4 {
            assert_eq!(&p[y * 4..y * 4 + 4], &[10, 20, 30, 40]);
        }
    }

    #[test]
    fn horizontal_4x4_replicates_left() {
        let n = Neighbors4x4 {
            left: Some([1, 2, 3, 4]),
            ..Default::default()
        };
        let p = predict_4x4(1, &n);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(p[y * 4 + x], u8::try_from(y + 1).unwrap());
            }
        }
    }

    #[test]
    fn dc_4x4_all_availability_cases() {
        let top = [10, 20, 30, 40];
        let left = [4, 8, 12, 16];

        let both = Neighbors4x4 {
            top: Some(top),
            left: Some(left),
            ..Default::default()
        };
        assert_eq!(predict_4x4(2, &both)[0], 18);

        let top_only = Neighbors4x4 {
            top: Some(top),
            ..Default::default()
        };
        assert_eq!(predict_4x4(2, &top_only)[0], 25);

        let left_only = Neighbors4x4 {
            left: Some(left),
            ..Default::default()
        };
        assert_eq!(predict_4x4(2, &left_only)[0], 10);

        let none = Neighbors4x4::default();
        assert_eq!(predict_4x4(2, &none)[0], 128);
    }

    #[test]
    fn diag_down_left_4x4_hand_computed() {
        let n = Neighbors4x4 {
            top: Some([0, 1, 2, 3]),
            top_right: Some([4, 5, 6, 7]),
            ..Default::default()
        };
        let p = predict_4x4(3, &n);
        assert_eq!(p[0], 1);
        assert_eq!(p[1], 2);
        assert_eq!(p[4], 2);
        assert_eq!(p[15], 7);
    }

    #[test]
    fn diag_down_right_4x4_hand_computed() {
        let n = Neighbors4x4 {
            top: Some([10, 20, 30, 40]),
            left: Some([50, 60, 70, 80]),
            top_left: Some(100),
            ..Default::default()
        };
        let p = predict_4x4(4, &n);
        assert_eq!(p[0], 65);
        assert_eq!(p[1], 35);
        assert_eq!(p[4], 65);
    }

    #[test]
    fn vertical_left_4x4_hand_computed() {
        let n = Neighbors4x4 {
            top: Some([0, 1, 2, 3]),
            top_right: Some([4, 5, 6, 7]),
            ..Default::default()
        };
        let p = predict_4x4(7, &n);
        assert_eq!(&p[0..4], &[1, 2, 3, 4]);
        assert_eq!(p[4], 1);
        assert_eq!(p[5], 2);
    }

    #[test]
    fn horizontal_up_4x4_hand_computed() {
        let n = Neighbors4x4 {
            left: Some([10, 20, 30, 40]),
            ..Default::default()
        };
        let p = predict_4x4(8, &n);
        assert_eq!(p[0], 15);
        assert_eq!(p[1], 20);
        assert_eq!(p[15], 40);
        assert_eq!(p[7], 38);
    }

    #[test]
    fn vertical_16x16_replicates_top() {
        let mut top = [0u8; 16];
        for (i, t) in top.iter_mut().enumerate() {
            *t = u8::try_from(i * 4).unwrap();
        }
        let p = predict_16x16(0, Some(top), None, None);
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(p[y * 16 + x], top[x]);
            }
        }
    }

    #[test]
    fn dc_16x16_all_cases() {
        let top = [4u8; 16];
        let left = [8u8; 16];

        assert_eq!(predict_16x16(2, Some(top), Some(left), None)[0], 6);
        assert_eq!(predict_16x16(2, Some(top), None, None)[0], 4);
        assert_eq!(predict_16x16(2, None, Some(left), None)[0], 8);
        assert_eq!(predict_16x16(2, None, None, None)[0], 128);
    }

    #[test]
    fn plane_16x16_flat_neighbours_is_constant() {
        let top = [100u8; 16];
        let left = [100u8; 16];
        let p = predict_16x16(3, Some(top), Some(left), Some(100));
        for &px in &p {
            assert_eq!(px, 100);
        }
    }

    #[test]
    fn plane_16x16_gradient_corner_pixels() {
        let mut top = [0u8; 16];
        for (i, t) in top.iter_mut().enumerate() {
            *t = u8::try_from(i * 8).unwrap();
        }
        let left = [60u8; 16];
        let tl = 60u8;

        let p = predict_16x16(3, Some(top), Some(left), Some(tl));
        assert_eq!(p[7 * 16 + 7], 90);
        assert_eq!(p[0], 43);
        assert_eq!(p[15], 143);
    }

    #[test]
    fn filter_8x8_boundary_value() {
        let n = Neighbors8x8 {
            top: Some([10, 20, 30, 40, 50, 60, 70, 80]),
            left: Some([5, 5, 5, 5, 5, 5, 5, 5]),
            top_left: Some(100),
            ..Default::default()
        };
        let p = predict_8x8(0, &n);
        assert_eq!(p[0], 35);
        assert_eq!(p[1], 20);
    }

    #[test]
    fn dc_8x8_flat_neighbours() {
        let n = Neighbors8x8 {
            top: Some([40; 8]),
            top_right: Some([40; 8]),
            left: Some([40; 8]),
            top_left: Some(40),
        };
        let p = predict_8x8(2, &n);
        for &px in &p {
            assert_eq!(px, 40);
        }
    }

    #[test]
    fn dc_8x8_neither_available() {
        let n = Neighbors8x8::default();
        let p = predict_8x8(2, &n);
        assert_eq!(p[0], 128);
    }

    #[test]
    fn vertical_8x8_filtered_constant_top() {
        let n = Neighbors8x8 {
            top: Some([50; 8]),
            top_right: Some([50; 8]),
            left: Some([50; 8]),
            top_left: Some(50),
        };
        let p = predict_8x8(0, &n);
        for &px in &p {
            assert_eq!(px, 50);
        }
    }

    #[test]
    fn horizontal_8x8_filtered_left() {
        let n = Neighbors8x8 {
            left: Some([70; 8]),
            top: Some([70; 8]),
            top_right: Some([70; 8]),
            top_left: Some(70),
        };
        let p = predict_8x8(1, &n);
        for &px in &p {
            assert_eq!(px, 70);
        }
    }
}
