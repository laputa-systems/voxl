//! BGRX to I420 for the rows the parent marked damaged. The BT.601 limited-range
//! coefficients match libyuv's `ARGBToI420`, which Voxl uses for the same bytes on
//! Linux and Windows (src/convert.rs there); macOS has no libyuv, so this path is
//! plain Rust.

pub struct I420Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        I420Frame {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    pub fn stride_uv(&self) -> usize {
        self.width.div_ceil(2)
    }
}

pub fn convert_bgrx_band(
    src: &[u8],
    src_stride: usize,
    dst: &mut I420Frame,
    y0: usize,
    y1: usize,
) -> Result<(), String> {
    let (w, h) = (dst.width, dst.height);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err("I420 needs a non-zero even width and height".into());
    }
    if y0 % 2 != 0 || y1 % 2 != 0 || y0 > y1 || y1 > h {
        return Err(format!("row band {y0}..{y1} is not an even-aligned range in 0..{h}"));
    }
    if src_stride < w * 4 {
        return Err(format!("stride {src_stride} is under width * 4 = {}", w * 4));
    }
    let need = (y1 - y0) * src_stride;
    if src.len() < need {
        return Err(format!(
            "{} source bytes cannot hold rows {y0}..{y1} at stride {src_stride} ({need} bytes)",
            src.len()
        ));
    }
    if y0 == y1 {
        return Ok(());
    }
    for row in y0..y1 {
        let local = row - y0;
        let line = &src[local * src_stride..local * src_stride + w * 4];
        let out = &mut dst.y[row * w..(row + 1) * w];
        for (px, y) in line.chunks_exact(4).zip(out.iter_mut()) {
            *y = luma(px[2], px[1], px[0]);
        }
    }
    let cw = dst.stride_uv();
    for cy in y0 / 2..y1 / 2 {
        let top_row = 2 * cy - y0;
        let top = &src[top_row * src_stride..top_row * src_stride + w * 4];
        let bottom = &src[(top_row + 1) * src_stride..(top_row + 1) * src_stride + w * 4];
        let urow = &mut dst.u[cy * cw..(cy + 1) * cw];
        let vrow = &mut dst.v[cy * cw..(cy + 1) * cw];
        for cx in 0..cw {
            let x = cx * 2;
            let (r, g, b) = average_2x2(top, bottom, x);
            let (u, v) = chroma(r, g, b);
            urow[cx] = u;
            vrow[cx] = v;
        }
    }
    Ok(())
}

fn average_2x2(top: &[u8], bottom: &[u8], x: usize) -> (i32, i32, i32) {
    let a = &top[x * 4..x * 4 + 4];
    let b = &top[x * 4 + 4..x * 4 + 8];
    let c = &bottom[x * 4..x * 4 + 4];
    let d = &bottom[x * 4 + 4..x * 4 + 8];
    let r = (a[2] as i32 + b[2] as i32 + c[2] as i32 + d[2] as i32 + 2) / 4;
    let g = (a[1] as i32 + b[1] as i32 + c[1] as i32 + d[1] as i32 + 2) / 4;
    let bl = (a[0] as i32 + b[0] as i32 + c[0] as i32 + d[0] as i32 + 2) / 4;
    (r, g, bl)
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                v.extend_from_slice(&f(x, y));
            }
        }
        v
    }

    #[test]
    fn black_and_white_land_on_limited_range_ends() {
        let src = frame(2, 2, |x, y| {
            if x == 0 && y == 0 {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            }
        });
        let mut dst = I420Frame::new(2, 2);
        convert_bgrx_band(&src, 8, &mut dst, 0, 2).unwrap();
        assert_eq!(dst.y, vec![16, 235, 235, 235]);
        assert_eq!(dst.u, vec![128]);
        assert_eq!(dst.v, vec![128]);
    }

    #[test]
    fn a_band_leaves_the_rest_of_the_frame_alone() {
        let (w, h) = (32, 32);
        let src = frame(w, h, |x, y| [x as u8, y as u8, (x + y) as u8, 255]);
        let mut dst = I420Frame::new(w, h);
        dst.y.fill(7);
        dst.u.fill(9);
        dst.v.fill(11);
        convert_bgrx_band(&src[16 * w * 4..], w * 4, &mut dst, 16, 32).unwrap();
        assert!(dst.y[..16 * w].iter().all(|&p| p == 7));
        assert!(dst.y[16 * w..].iter().all(|&p| p != 7));
        assert!(dst.u[..8 * dst.stride_uv()].iter().all(|&p| p == 9));
        assert!(dst.u[8 * dst.stride_uv()..].iter().all(|&p| p != 9));
    }

    #[test]
    fn a_band_matches_the_same_rows_of_a_full_conversion() {
        let (w, h) = (64, 48);
        let src = frame(w, h, |x, y| {
            [(x * 3) as u8, (y * 5) as u8, (x ^ y) as u8, 255]
        });
        let mut full = I420Frame::new(w, h);
        convert_bgrx_band(&src, w * 4, &mut full, 0, h).unwrap();
        let mut band = I420Frame::new(w, h);
        band.y.fill(16);
        band.u.fill(128);
        band.v.fill(128);
        convert_bgrx_band(&src[16 * w * 4..32 * w * 4], w * 4, &mut band, 16, 32).unwrap();
        assert_eq!(band.y[16 * w..32 * w], full.y[16 * w..32 * w]);
        let suv = band.stride_uv();
        assert_eq!(band.u[8 * suv..16 * suv], full.u[8 * suv..16 * suv]);
        assert_eq!(band.v[8 * suv..16 * suv], full.v[8 * suv..16 * suv]);
    }

    #[test]
    fn odd_bands_and_short_buffers_are_rejected() {
        let src = vec![0u8; 32 * 32 * 4];
        let mut dst = I420Frame::new(32, 32);
        assert!(convert_bgrx_band(&src, 32 * 4, &mut dst, 1, 16).is_err());
        assert!(convert_bgrx_band(&src, 32 * 3, &mut dst, 0, 16).is_err());
        assert!(convert_bgrx_band(&src[..100], 32 * 4, &mut dst, 0, 16).is_err());
    }
}
