#[derive(Debug, Clone, Default)]
pub(super) struct MbDeblockInfo {
    pub present: bool,
    pub qp: i32,
    pub transform_8x8: bool,
    pub disable_idc: u8,
    pub alpha_off: i32,
    pub beta_off: i32,
    pub slice_id: u32,
}

const ALPHA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20,
    22, 25, 28, 32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, 203, 226,
    255, 255,
];

const BETA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
];

const TC0: [[u8; 52]; 3] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 7, 8, 8, 10, 11, 12, 13, 15, 17,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2,
        2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 23, 25,
    ],
];

fn clip3(a: i32, b: i32, x: i32) -> i32 {
    x.clamp(a, b)
}

fn clip1(x: i32) -> u8 {
    u8::try_from(x.clamp(0, 255)).expect("value clamped into 0..=255")
}

#[derive(Clone, Copy)]
struct EdgeFilter {
    bs4: bool,
    alpha: i32,
    beta: i32,
    tc0: i32,
}

fn filter_luma_line(s: &mut [i32; 8], f: EdgeFilter) {
    let EdgeFilter {
        bs4,
        alpha,
        beta,
        tc0,
    } = f;
    let [p3, p2, p1, p0, q0, q1, q2, q3] = *s;

    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }

    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();

    if bs4 {
        let strong = (p0 - q0).abs() < (alpha >> 2) + 2;
        if strong && ap < beta {
            s[3] = (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3;
            s[2] = (p2 + p1 + p0 + q0 + 2) >> 2;
            s[1] = (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3;
        } else {
            s[3] = (2 * p1 + p0 + q1 + 2) >> 2;
        }
        if strong && aq < beta {
            s[4] = (q2 + 2 * q1 + 2 * q0 + 2 * p0 + p1 + 4) >> 3;
            s[5] = (q2 + q1 + q0 + p0 + 2) >> 2;
            s[6] = (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3;
        } else {
            s[4] = (2 * q1 + q0 + p1 + 2) >> 2;
        }
    } else {
        let tc = tc0 + i32::from(ap < beta) + i32::from(aq < beta);
        let delta = clip3(-tc, tc, ((q0 - p0) * 4 + (p1 - q1) + 4) >> 3);
        s[3] = i32::from(clip1(p0 + delta));
        s[4] = i32::from(clip1(q0 - delta));
        if ap < beta {
            s[2] = p1 + clip3(-tc0, tc0, (p2 + ((p0 + q0 + 1) >> 1) - 2 * p1) >> 1);
        }
        if aq < beta {
            s[5] = q1 + clip3(-tc0, tc0, (q2 + ((p0 + q0 + 1) >> 1) - 2 * q1) >> 1);
        }
    }
}

fn edge_thresholds(
    qp_p: i32,
    qp_q: i32,
    bs4: bool,
    alpha_off: i32,
    beta_off: i32,
) -> Option<EdgeFilter> {
    let qp_av = (qp_p + qp_q + 1) >> 1;
    let index_a = usize::try_from((qp_av + alpha_off).clamp(0, 51)).expect("indexA in 0..=51");
    let index_b = usize::try_from((qp_av + beta_off).clamp(0, 51)).expect("indexB in 0..=51");
    let alpha = i32::from(ALPHA[index_a]);
    let beta = i32::from(BETA[index_b]);
    if alpha == 0 || beta == 0 {
        return None;
    }
    let tc0 = if bs4 { 0 } else { i32::from(TC0[2][index_a]) };
    Some(EdgeFilter {
        bs4,
        alpha,
        beta,
        tc0,
    })
}

fn filter_vertical_edge(luma: &mut [u8], pw: usize, xc: usize, oy: usize, f: EdgeFilter) {
    for row in oy..oy + 16 {
        let base = row * pw;
        let mut s = [
            i32::from(luma[base + xc - 4]),
            i32::from(luma[base + xc - 3]),
            i32::from(luma[base + xc - 2]),
            i32::from(luma[base + xc - 1]),
            i32::from(luma[base + xc]),
            i32::from(luma[base + xc + 1]),
            i32::from(luma[base + xc + 2]),
            i32::from(luma[base + xc + 3]),
        ];
        filter_luma_line(&mut s, f);
        luma[base + xc - 3] = clip1(s[1]);
        luma[base + xc - 2] = clip1(s[2]);
        luma[base + xc - 1] = clip1(s[3]);
        luma[base + xc] = clip1(s[4]);
        luma[base + xc + 1] = clip1(s[5]);
        luma[base + xc + 2] = clip1(s[6]);
    }
}

fn filter_horizontal_edge(luma: &mut [u8], pw: usize, ox: usize, yc: usize, f: EdgeFilter) {
    for col in ox..ox + 16 {
        let mut s = [
            i32::from(luma[(yc - 4) * pw + col]),
            i32::from(luma[(yc - 3) * pw + col]),
            i32::from(luma[(yc - 2) * pw + col]),
            i32::from(luma[(yc - 1) * pw + col]),
            i32::from(luma[yc * pw + col]),
            i32::from(luma[(yc + 1) * pw + col]),
            i32::from(luma[(yc + 2) * pw + col]),
            i32::from(luma[(yc + 3) * pw + col]),
        ];
        filter_luma_line(&mut s, f);
        luma[(yc - 3) * pw + col] = clip1(s[1]);
        luma[(yc - 2) * pw + col] = clip1(s[2]);
        luma[(yc - 1) * pw + col] = clip1(s[3]);
        luma[yc * pw + col] = clip1(s[4]);
        luma[(yc + 1) * pw + col] = clip1(s[5]);
        luma[(yc + 2) * pw + col] = clip1(s[6]);
    }
}

pub(super) fn deblock_luma(
    luma: &mut [u8],
    width_mbs: usize,
    height_mbs: usize,
    mbs: &[MbDeblockInfo],
) {
    let pw = width_mbs * 16;
    for addr in 0..width_mbs * height_mbs {
        let info = &mbs[addr];
        if !info.present || info.disable_idc == 1 {
            continue;
        }
        let mb_x = addr % width_mbs;
        let mb_y = addr / width_mbs;
        let ox = mb_x * 16;
        let oy = mb_y * 16;

        for edge in 0..4usize {
            if info.transform_8x8 && (edge == 1 || edge == 3) {
                continue;
            }
            let xc = ox + edge * 4;
            let (bs4, qp_p) = if edge == 0 {
                if mb_x == 0 {
                    continue;
                }
                let left = &mbs[addr - 1];
                if !left.present || (info.disable_idc == 2 && left.slice_id != info.slice_id) {
                    continue;
                }
                (true, left.qp)
            } else {
                (false, info.qp)
            };
            if let Some(f) = edge_thresholds(qp_p, info.qp, bs4, info.alpha_off, info.beta_off) {
                filter_vertical_edge(luma, pw, xc, oy, f);
            }
        }

        for edge in 0..4usize {
            if info.transform_8x8 && (edge == 1 || edge == 3) {
                continue;
            }
            let yc = oy + edge * 4;
            let (bs4, qp_p) = if edge == 0 {
                if mb_y == 0 {
                    continue;
                }
                let top = &mbs[addr - width_mbs];
                if !top.present || (info.disable_idc == 2 && top.slice_id != info.slice_id) {
                    continue;
                }
                (true, top.qp)
            } else {
                (false, info.qp)
            };
            if let Some(f) = edge_thresholds(qp_p, info.qp, bs4, info.alpha_off, info.beta_off) {
                filter_horizontal_edge(luma, pw, ox, yc, f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_filter_smooths_small_step_within_tc() {
        let mut s = [100, 100, 100, 100, 110, 110, 110, 110];
        filter_luma_line(
            &mut s,
            EdgeFilter {
                bs4: false,
                alpha: 255,
                beta: 18,
                tc0: 4,
            },
        );
        assert_eq!(s, [100, 100, 102, 104, 106, 107, 110, 110]);
    }

    #[test]
    fn strong_filter_spreads_large_intra_step() {
        let mut s = [100, 100, 100, 100, 160, 160, 160, 160];
        filter_luma_line(
            &mut s,
            EdgeFilter {
                bs4: true,
                alpha: 255,
                beta: 18,
                tc0: 0,
            },
        );
        assert_eq!(s, [100, 108, 115, 123, 138, 145, 153, 160]);
    }

    #[test]
    fn gate_blocks_filtering_when_step_exceeds_alpha() {
        let mut s = [100, 100, 100, 100, 160, 160, 160, 160];
        let before = s;
        filter_luma_line(
            &mut s,
            EdgeFilter {
                bs4: true,
                alpha: 16,
                beta: 18,
                tc0: 0,
            },
        );
        assert_eq!(s, before, "edge left untouched when gate fails");
    }

    #[test]
    fn zero_alpha_yields_no_thresholds() {
        assert!(edge_thresholds(0, 0, true, 0, 0).is_none());
        assert!(edge_thresholds(40, 40, false, 0, 0).is_some());
    }

    #[test]
    fn vertical_edge_only_touches_six_columns() {
        let pw = 32usize;
        let mut luma = vec![0u8; pw * 16];
        for row in 0..16 {
            for x in 0..16 {
                luma[row * pw + x] = 100;
            }
            for x in 16..32 {
                luma[row * pw + x] = 160;
            }
        }
        let snapshot = luma.clone();
        filter_vertical_edge(
            &mut luma,
            pw,
            16,
            0,
            EdgeFilter {
                bs4: true,
                alpha: 255,
                beta: 18,
                tc0: 0,
            },
        );
        assert_eq!(luma[12], snapshot[12]);
        assert_eq!(luma[19], snapshot[19]);
        assert_ne!(luma[15], snapshot[15]);
        assert_ne!(luma[16], snapshot[16]);
    }

    fn mb(qp: i32, transform_8x8: bool) -> MbDeblockInfo {
        MbDeblockInfo {
            present: true,
            qp,
            transform_8x8,
            disable_idc: 0,
            alpha_off: 0,
            beta_off: 0,
            slice_id: 0,
        }
    }

    fn stepped_1mb() -> Vec<u8> {
        let mut p = vec![0u8; 16 * 16];
        for row in 0..16 {
            for x in 0..16 {
                p[row * 16 + x] = if x < 4 { 100 } else { 120 };
            }
        }
        p
    }

    #[test]
    fn filters_4x4_internal_edge_but_skips_it_under_8x8_transform() {
        let mut p4 = stepped_1mb();
        deblock_luma(&mut p4, 1, 1, &[mb(40, false)]);
        assert_eq!((p4[3], p4[4]), (108, 112), "4×4: internal edge filtered");

        let mut p8 = stepped_1mb();
        deblock_luma(&mut p8, 1, 1, &[mb(40, true)]);
        assert_eq!(
            (p8[3], p8[4]),
            (100, 120),
            "8×8: internal 4-pel edge skipped"
        );
    }

    #[test]
    fn disable_idc_1_leaves_plane_unchanged() {
        let mut p = stepped_1mb();
        let before = p.clone();
        let info = MbDeblockInfo {
            disable_idc: 1,
            ..mb(40, false)
        };
        deblock_luma(&mut p, 1, 1, &[info]);
        assert_eq!(
            p, before,
            "disable_deblocking_filter_idc = 1 ⇒ no filtering"
        );
    }

    #[test]
    fn idc2_skips_boundary_between_different_slices() {
        let plane = || {
            let mut p = vec![0u8; 32 * 16];
            for row in 0..16 {
                for x in 0..32 {
                    p[row * 32 + x] = if x < 16 { 100 } else { 120 };
                }
            }
            p
        };
        let mb_in_slice = |slice_id| MbDeblockInfo {
            present: true,
            qp: 40,
            transform_8x8: false,
            disable_idc: 2,
            alpha_off: 0,
            beta_off: 0,
            slice_id,
        };

        let mut across = plane();
        deblock_luma(&mut across, 2, 1, &[mb_in_slice(0), mb_in_slice(1)]);
        assert_eq!(
            (across[15], across[16]),
            (100, 120),
            "idc=2: cross-slice boundary untouched"
        );

        let mut within = plane();
        deblock_luma(&mut within, 2, 1, &[mb_in_slice(0), mb_in_slice(0)]);
        assert_eq!(within[15], 108, "same-slice boundary filtered");
    }
}
