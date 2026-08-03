pub const DEFAULT_BAR_LIMIT: u8 = 24;

fn active_bounds(
    width: usize,
    height: usize,
    pixels: &[u8],
    limit: u8,
) -> (usize, usize, usize, usize) {
    if width == 0 || height == 0 {
        return (0, 0, width, height);
    }
    let px =
        |x: usize, y: usize| -> u64 { u64::from(pixels.get(y * width + x).copied().unwrap_or(0)) };
    let lim = u64::from(limit);
    let row_is_bar =
        |y: usize| -> bool { (0..width).map(|x| px(x, y)).sum::<u64>() <= lim * width as u64 };
    let col_is_bar =
        |x: usize| -> bool { (0..height).map(|y| px(x, y)).sum::<u64>() <= lim * height as u64 };

    let mut y0 = 0;
    while y0 < height && row_is_bar(y0) {
        y0 += 1;
    }
    if y0 == height {
        return (0, 0, width, height);
    }
    let mut y1 = height;
    while y1 > y0 && row_is_bar(y1 - 1) {
        y1 -= 1;
    }
    let mut x0 = 0;
    while x0 < width && col_is_bar(x0) {
        x0 += 1;
    }
    let mut x1 = width;
    while x1 > x0 && col_is_bar(x1 - 1) {
        x1 -= 1;
    }
    (x0, y0, x1, y1)
}

#[allow(clippy::cast_possible_truncation)]
#[must_use]
pub fn trim_uniform_borders(
    width: u32,
    height: u32,
    pixels: &[u8],
    limit: u8,
) -> (u32, u32, Vec<u8>) {
    let w = width as usize;
    let h = height as usize;
    let (x0, y0, x1, y1) = active_bounds(w, h, pixels, limit);
    let nw = x1 - x0;
    let nh = y1 - y0;
    if nw == 0 || nh == 0 || (nw == w && nh == h) {
        return (
            width,
            height,
            pixels.get(..w * h).unwrap_or(pixels).to_vec(),
        );
    }
    let mut out = Vec::with_capacity(nw * nh);
    for y in y0..y1 {
        let base = y * w;
        for x in x0..x1 {
            out.push(pixels.get(base + x).copied().unwrap_or(0));
        }
    }
    (nw as u32, nh as u32, out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation)]
    use super::*;

    fn frame(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                v.push(f(x, y));
            }
        }
        v
    }

    #[test]
    fn trims_pillarbox_to_centered_content() {
        let w = 10;
        let h = 4;
        let px = frame(w, h, |x, _| if (3..7).contains(&x) { 200 } else { 0 });
        let (nw, nh, out) = trim_uniform_borders(w as u32, h as u32, &px, DEFAULT_BAR_LIMIT);
        assert_eq!((nw, nh), (4, 4), "kept only the 4-wide content band");
        assert!(
            out.iter().all(|&p| p == 200),
            "content preserved, bars gone"
        );
    }

    #[test]
    fn trims_letterbox_top_and_bottom() {
        let w = 4;
        let h = 4;
        let px = frame(w, h, |_, y| if (1..3).contains(&y) { 180 } else { 10 });
        let (nw, nh, _) = trim_uniform_borders(w as u32, h as u32, &px, DEFAULT_BAR_LIMIT);
        assert_eq!((nw, nh), (4, 2));
    }

    #[test]
    fn no_bars_is_a_noop_full_copy() {
        let w = 5;
        let h = 5;
        let px = frame(w, h, |x, y| (x + y) as u8 + 50);
        let (nw, nh, out) = trim_uniform_borders(w as u32, h as u32, &px, DEFAULT_BAR_LIMIT);
        assert_eq!((nw, nh), (5, 5));
        assert_eq!(out, px);
    }

    #[test]
    fn entirely_dark_frame_is_kept_whole() {
        let w = 4;
        let h = 4;
        let px = vec![0u8; w * h];
        let (nw, nh, _) = trim_uniform_borders(w as u32, h as u32, &px, DEFAULT_BAR_LIMIT);
        assert_eq!((nw, nh), (4, 4), "no content anchor → keep the frame");
    }

    #[test]
    fn deterministic_same_input_same_crop() {
        let w = 8;
        let h = 4;
        let px = frame(w, h, |x, _| if (2..6).contains(&x) { 120 } else { 5 });
        let a = trim_uniform_borders(w as u32, h as u32, &px, DEFAULT_BAR_LIMIT);
        let b = trim_uniform_borders(w as u32, h as u32, &px, DEFAULT_BAR_LIMIT);
        assert_eq!(a, b);
    }

    #[test]
    fn zero_dimension_is_safe() {
        let (nw, nh, out) = trim_uniform_borders(0, 5, &[], DEFAULT_BAR_LIMIT);
        assert_eq!((nw, nh), (0, 5));
        assert!(out.is_empty());
    }
}
