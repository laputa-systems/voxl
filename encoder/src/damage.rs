//! Damage rectangles to x264's per-macroblock map. Voxl builds the same map from X
//! damage (src/capture.rs:241-244) and turns it into `X264_MBINFO_CONSTANT` for the
//! macroblocks that did not change (src/video.rs:1089-1096).

pub const MBINFO_UNCHANGED: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

pub struct MacroblockMap {
    cols: usize,
    rows: usize,
    height: u32,
    dirty: Vec<u8>,
}

impl MacroblockMap {
    pub fn new(width: u32, height: u32) -> Self {
        let cols = width.div_ceil(16) as usize;
        let rows = height.div_ceil(16) as usize;
        MacroblockMap {
            cols,
            rows,
            height,
            dirty: vec![0; cols * rows],
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn len(&self) -> usize {
        self.dirty.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }

    pub fn dirty(&self) -> &[u8] {
        &self.dirty
    }

    pub fn clear(&mut self) {
        self.dirty.fill(0);
    }

    pub fn mark_all(&mut self) {
        self.dirty.fill(1);
    }

    pub fn mark_rect(&mut self, r: Rect, width: u32, height: u32) -> Result<(), String> {
        if r.w == 0 || r.h == 0 {
            return Err(format!("damage rect {:?} is empty", r));
        }
        let right = r
            .x
            .checked_add(r.w)
            .ok_or_else(|| format!("damage rect {:?} overflows x", r))?;
        let bottom = r
            .y
            .checked_add(r.h)
            .ok_or_else(|| format!("damage rect {:?} overflows y", r))?;
        if right > width || bottom > height {
            return Err(format!(
                "damage rect {:?} exceeds the {width}x{height} frame",
                r
            ));
        }
        let x0 = (r.x as usize / 16).min(self.cols);
        let x1 = (right as usize).div_ceil(16).min(self.cols);
        let y0 = (r.y as usize / 16).min(self.rows);
        let y1 = (bottom as usize).div_ceil(16).min(self.rows);
        for row in y0..y1 {
            self.dirty[row * self.cols + x0..row * self.cols + x1].fill(1);
        }
        Ok(())
    }

    pub fn changed(&self) -> usize {
        self.dirty.iter().filter(|&&d| d != 0).count()
    }

    /// One byte per macroblock for x264: 1 marks a macroblock it may copy from the
    /// reference without analysis.
    pub fn unchanged_info(&self) -> Vec<u8> {
        self.dirty
            .iter()
            .map(|&d| (d == 0) as u8 * MBINFO_UNCHANGED)
            .collect()
    }

    /// The pixel rows, aligned to 16, spanned by the dirty macroblock rows.
    pub fn required_row_band(&self) -> Option<Band> {
        let mut r0 = usize::MAX;
        let mut r1 = 0usize;
        for row in 0..self.rows {
            if self.dirty[row * self.cols..(row + 1) * self.cols]
                .iter()
                .any(|&d| d != 0)
            {
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
            }
        }
        (r1 > r0).then(|| Band {
            y0: (r0 * 16) as u32,
            y1: ((r1 * 16) as u32).min(self.height),
        })
    }
}

impl std::ops::BitOrAssign<&[u8]> for MacroblockMap {
    fn bitor_assign(&mut self, rhs: &[u8]) {
        for (d, r) in self.dirty.iter_mut().zip(rhs) {
            *d |= *r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1280;
    const H: u32 = 800;

    #[test]
    fn one_macroblock_row_is_marked_by_a_16_pixel_rect() {
        let mut map = MacroblockMap::new(W, H);
        map.mark_rect(
            Rect {
                x: 0,
                y: 320,
                w: W,
                h: 16,
            },
            W,
            H,
        )
        .unwrap();
        assert_eq!(map.cols(), 80);
        assert_eq!(map.rows(), 50);
        assert_eq!(map.changed(), 80);
        for row in 0..map.rows() {
            let expected = u8::from(row == 20);
            assert!(map.dirty()[row * 80..(row + 1) * 80]
                .iter()
                .all(|&d| d == expected));
        }
        assert_eq!(
            map.required_row_band(),
            Some(Band { y0: 320, y1: 336 })
        );
    }

    #[test]
    fn unchanged_macroblocks_are_flagged_and_changed_ones_are_not() {
        let mut map = MacroblockMap::new(W, H);
        map.mark_rect(
            Rect {
                x: 100,
                y: 100,
                w: 20,
                h: 20,
            },
            W,
            H,
        )
        .unwrap();
        let info = map.unchanged_info();
        assert_eq!(info.len(), 80 * 50);
        assert_eq!(info[0], MBINFO_UNCHANGED);
        assert_eq!(info[6 * 80 + 7], 0);
        assert_eq!(info[7 * 80 + 7], 0);
        assert_eq!(map.changed(), 4);
    }

    #[test]
    fn a_rect_rounds_outward_to_macroblock_bounds() {
        let mut map = MacroblockMap::new(W, H);
        map.mark_rect(
            Rect {
                x: 17,
                y: 33,
                w: 1,
                h: 1,
            },
            W,
            H,
        )
        .unwrap();
        assert_eq!(map.changed(), 1);
        assert_eq!(map.dirty()[2 * 80 + 1], 1);
    }

    #[test]
    fn a_partial_last_macroblock_row_stops_at_the_frame() {
        let mut map = MacroblockMap::new(64, 20);
        map.mark_rect(
            Rect {
                x: 0,
                y: 16,
                w: 64,
                h: 4,
            },
            64,
            20,
        )
        .unwrap();
        assert_eq!(
            map.required_row_band(),
            Some(Band { y0: 16, y1: 20 })
        );
    }

    #[test]
    fn out_of_frame_and_empty_rects_are_rejected() {
        let mut map = MacroblockMap::new(W, H);
        assert!(map
            .mark_rect(
                Rect {
                    x: W - 1,
                    y: 0,
                    w: 2,
                    h: 1
                },
                W,
                H
            )
            .is_err());
        assert!(map
            .mark_rect(
                Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 1
                },
                W,
                H
            )
            .is_err());
    }
}
