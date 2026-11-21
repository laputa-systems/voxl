//! One encoder for one subscription: damage map in, one Annex-B access unit out.

use crate::convert::{convert_bgrx_band, I420Frame};
use crate::damage::{MacroblockMap, MBINFO_UNCHANGED};
use crate::protocol::{Configure, Frame, PROTOCOL_VERSION};
use crate::settle::{MotionQuality, MotionSize, Refiner};
use crate::x264_ffi::{self, Encoder, Planes};
use std::time::Instant;

pub const PAINT_OVER_CRF: f32 = 16.0;

#[derive(Clone, Copy, Debug)]
pub struct SessionConfig {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fps: u32,
    pub crf: f32,
    pub vbv_kbps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Protocol(String),
    Config(String),
    Encode(String),
}

impl SessionError {
    pub fn code(&self) -> u16 {
        match self {
            SessionError::Protocol(_) => crate::protocol::ERR_PROTOCOL,
            SessionError::Config(_) => crate::protocol::ERR_CONFIG,
            SessionError::Encode(_) => crate::protocol::ERR_ENCODE,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            SessionError::Protocol(m) | SessionError::Config(m) | SessionError::Encode(m) => m,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncodeOutput {
    pub generation: u64,
    pub keyframe: bool,
    pub paint_over: bool,
    pub crf: f32,
    pub qp: Option<f32>,
    pub bytes: Vec<u8>,
}

impl SessionConfig {
    pub fn from_configure(c: &Configure) -> Result<Self, SessionError> {
        if c.version != PROTOCOL_VERSION {
            return Err(SessionError::Config(format!(
                "protocol version {} is not {PROTOCOL_VERSION}",
                c.version
            )));
        }
        let cfg = SessionConfig {
            width: c.width,
            height: c.height,
            stride: c.stride,
            fps: c.fps,
            crf: c.crf,
            vbv_kbps: c.vbv_kbps,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), SessionError> {
        let bad = |m: String| SessionError::Config(m);
        if self.width == 0 || self.height == 0 {
            return Err(bad("width and height must be non-zero".into()));
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(bad(format!(
                "I420 needs an even size, got {}x{}",
                self.width, self.height
            )));
        }
        if self.stride < self.width.saturating_mul(4) {
            return Err(bad(format!(
                "stride {} cannot hold {} BGRX pixels",
                self.stride, self.width
            )));
        }
        if self.stride as u64 > i32::MAX as u64 || self.height as u64 > i32::MAX as u64 {
            return Err(bad("frame geometry exceeds x264's limits".into()));
        }
        if self.fps == 0 {
            return Err(bad("fps must be non-zero".into()));
        }
        if !self.crf.is_finite() || !(0.0..=51.0).contains(&self.crf) {
            return Err(bad(format!("CRF {} is outside 0..=51", self.crf)));
        }
        Ok(())
    }
}

pub struct Session {
    cfg: SessionConfig,
    enc: Encoder,
    i420: I420Frame,
    map: MacroblockMap,
    refiner: Refiner,
    motion: MotionQuality,
    last_generation: Option<u64>,
    request_keyframe: bool,
    frames: u64,
    applied_crf: Option<f32>,
    started: Instant,
}

impl Session {
    pub fn new(cfg: SessionConfig) -> Result<Self, SessionError> {
        cfg.validate()?;
        let enc = Encoder::new(&x264_ffi::Config {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            crf: cfg.crf,
            max_kbps: cfg.vbv_kbps,
        })
        .map_err(SessionError::Config)?;
        let cols = cfg.width.div_ceil(16) as usize;
        let rows = cfg.height.div_ceil(16) as usize;
        Ok(Session {
            cfg,
            enc,
            i420: I420Frame::new(cfg.width as usize, cfg.height as usize),
            map: MacroblockMap::new(cfg.width, cfg.height),
            refiner: Refiner::new(cols * rows, cols, cfg.height),
            motion: MotionQuality::default(),
            last_generation: None,
            request_keyframe: false,
            frames: 0,
            applied_crf: None,
            started: Instant::now(),
        })
    }

    pub fn width(&self) -> u32 {
        self.cfg.width
    }

    pub fn height(&self) -> u32 {
        self.cfg.height
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn request_keyframe(&mut self) {
        self.request_keyframe = true;
    }

    pub fn handle_frame(
        &mut self,
        frame: &Frame,
    ) -> Result<Option<EncodeOutput>, SessionError> {
        let now = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.handle_frame_at(frame, now)
    }

    /// `now_ms` is the session clock the settle refiner compares against.
    pub fn handle_frame_at(
        &mut self,
        frame: &Frame,
        now_ms: u64,
    ) -> Result<Option<EncodeOutput>, SessionError> {
        self.accept_generation(frame.generation)?;
        self.validate_frame(frame)?;
        self.update_pixels(frame)?;

        let keyframe = self.frames == 0 || std::mem::take(&mut self.request_keyframe);
        let is_motion = frame.full || !frame.damage.is_empty();
        if keyframe {
            self.refiner.mark_all(now_ms);
        } else if is_motion {
            let (refiner, map) = (&mut self.refiner, &self.map);
            refiner.mark(map.dirty(), now_ms);
        }

        let mut paint_over = false;
        let mut changed = 0;
        let crf;
        let mut info;
        if keyframe {
            crf = self.cfg.crf;
            info = None;
        } else if is_motion {
            crf = self.motion.crf(self.cfg.crf);
            let mut final_map = self.map.dirty().to_vec();
            if let Some((rmap, _)) = self.refiner.due(now_ms) {
                for (d, r) in final_map.iter_mut().zip(rmap.iter()) {
                    *d |= *r;
                }
            }
            changed = final_map.iter().filter(|&&d| d != 0).count();
            info = Some(unchanged(&final_map));
        } else if let Some((rmap, _)) = self.refiner.due(now_ms) {
            paint_over = true;
            crf = PAINT_OVER_CRF;
            changed = rmap.iter().filter(|&&d| d != 0).count();
            info = Some(unchanged(&rmap));
        } else {
            self.frames += 1;
            self.last_generation = Some(frame.generation);
            return Ok(None);
        }

        if keyframe {
            let boost = self.cfg.vbv_kbps.saturating_mul(4).max(16_000);
            self.enc
                .set_rate_buf(self.cfg.crf, boost, boost / self.cfg.fps.max(1) * 8);
            self.applied_crf = None;
        } else if self.applied_crf != Some(crf) {
            self.enc.set_rate(crf, self.cfg.vbv_kbps);
            self.applied_crf = Some(crf);
        }

        let planes = Planes {
            y: &self.i420.y,
            u: &self.i420.u,
            v: &self.i420.v,
            stride_y: self.cfg.width as usize,
            stride_uv: self.i420.stride_uv(),
        };
        let ts_ms = self.frames as i64 * 1000 / self.cfg.fps.max(1) as i64;
        let out = self
            .enc
            .encode(&planes, ts_ms, keyframe, info.as_deref_mut())
            .map_err(SessionError::Encode)?;
        self.motion.note(
            crf,
            out.keyframe,
            out.qp,
            out.crf_avg,
            (is_motion && !keyframe).then(|| MotionSize {
                bits: out.bytes.len() * 8,
                changed,
                mbs: self.map.len(),
                crf: self.cfg.crf,
            }),
        );
        self.frames += 1;
        self.last_generation = Some(frame.generation);
        let bytes = out.bytes;
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodeOutput {
            generation: frame.generation,
            keyframe: out.keyframe,
            paint_over,
            crf,
            qp: out.qp,
            bytes,
        }))
    }

    fn accept_generation(&self, generation: u64) -> Result<(), SessionError> {
        if let Some(last) = self.last_generation {
            if generation <= last {
                return Err(SessionError::Protocol(format!(
                    "generation {generation} does not follow {last}"
                )));
            }
        }
        Ok(())
    }

    fn validate_frame(&self, frame: &Frame) -> Result<(), SessionError> {
        let bad = |m: String| SessionError::Protocol(m);
        if frame.rows.row_bytes != self.cfg.stride {
            return Err(bad(format!(
                "row bytes {} are not the configured stride {}",
                frame.rows.row_bytes, self.cfg.stride
            )));
        }
        let height = self.cfg.height;
        if frame.full {
            if !frame.damage.is_empty() {
                return Err(bad("a full frame carries no damage rects".into()));
            }
            if frame.rows.y0 != 0 || frame.rows.y1 != height {
                return Err(bad(format!(
                    "a full frame needs rows 0..{height}, got {}..{}",
                    frame.rows.y0, frame.rows.y1
                )));
            }
            let need = self.cfg.stride as usize * height as usize;
            if frame.pixels.len() != need {
                return Err(bad(format!(
                    "a full frame needs {need} pixel bytes, got {}",
                    frame.pixels.len()
                )));
            }
        } else {
            let (y0, y1) = (frame.rows.y0, frame.rows.y1);
            if y0 % 2 != 0 || y1 % 2 != 0 || y0 > y1 || y1 > height {
                return Err(bad(format!(
                    "pixel rows {y0}..{y1} are not an even-aligned range in 0..{height}"
                )));
            }
            let need = (y1 - y0) as usize * frame.rows.row_bytes as usize;
            if frame.pixels.len() != need {
                return Err(bad(format!(
                    "rows {y0}..{y1} need {need} pixel bytes, got {}",
                    frame.pixels.len()
                )));
            }
        }
        Ok(())
    }

    fn update_pixels(&mut self, frame: &Frame) -> Result<(), SessionError> {
        let width = self.cfg.width;
        let height = self.cfg.height;
        self.map.clear();
        if frame.full {
            self.map.mark_all();
        } else {
            for r in &frame.damage {
                self.map
                    .mark_rect(*r, width, height)
                    .map_err(SessionError::Protocol)?;
            }
            match self.map.required_row_band() {
                Some(band) => {
                    if frame.rows.y0 > band.y0 || frame.rows.y1 < band.y1 {
                        return Err(SessionError::Protocol(format!(
                            "pixel rows {}..{} do not cover damaged macroblock rows {}..{}",
                            frame.rows.y0, frame.rows.y1, band.y0, band.y1
                        )));
                    }
                }
                None => {
                    if frame.rows.y0 != frame.rows.y1 {
                        return Err(SessionError::Protocol(
                            "pixel rows sent for a frame with no damage".into(),
                        ));
                    }
                }
            }
        }
        let (y0, y1) = if frame.full {
            (0, height as usize)
        } else {
            (frame.rows.y0 as usize, frame.rows.y1 as usize)
        };
        convert_bgrx_band(
            &frame.pixels,
            self.cfg.stride as usize,
            &mut self.i420,
            y0,
            y1,
        )
        .map_err(SessionError::Protocol)
    }
}

fn unchanged(dirty: &[u8]) -> Vec<u8> {
    dirty
        .iter()
        .map(|&d| (d == 0) as u8 * MBINFO_UNCHANGED)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::damage::Rect;
    use crate::protocol::Rows;

    const W: u32 = 1280;
    const H: u32 = 800;
    const STRIDE: u32 = W * 4;

    fn config() -> SessionConfig {
        SessionConfig {
            width: W,
            height: H,
            stride: STRIDE,
            fps: 30,
            crf: 24.0,
            vbv_kbps: 8000,
        }
    }

    fn uncapped_config() -> SessionConfig {
        SessionConfig {
            vbv_kbps: 0,
            ..config()
        }
    }

    fn background() -> Vec<u8> {
        let mut px = vec![0u8; STRIDE as usize * H as usize];
        for y in 0..H as usize {
            for x in 0..W as usize {
                let i = y * STRIDE as usize + x * 4;
                let v = ((x / 7 + y / 5) % 3) as u8 * 40 + 40;
                px[i] = v;
                px[i + 1] = 255 - v;
                px[i + 2] = (x + y) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    fn noise(px: &mut [u8], rect: Rect, seed: u64) {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for y in rect.y as usize..(rect.y + rect.h) as usize {
            for x in rect.x as usize..(rect.x + rect.w) as usize {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let i = y * STRIDE as usize + x * 4;
                px[i] = state as u8;
                px[i + 1] = (state >> 8) as u8;
                px[i + 2] = (state >> 16) as u8;
                px[i + 3] = 255;
            }
        }
    }

    fn full_frame(generation: u64, px: &[u8]) -> Frame {
        Frame {
            generation,
            full: true,
            damage: Vec::new(),
            rows: Rows {
                y0: 0,
                y1: H,
                row_bytes: STRIDE,
            },
            pixels: px.to_vec(),
        }
    }

    fn band_for(rect: Rect) -> Rows {
        Rows {
            y0: (rect.y / 16) * 16,
            y1: (((rect.y + rect.h).div_ceil(16)) * 16).min(H),
            row_bytes: STRIDE,
        }
    }

    fn partial_frame(generation: u64, px: &[u8], rect: Rect) -> Frame {
        let rows = band_for(rect);
        let start = rows.y0 as usize * STRIDE as usize;
        let end = rows.y1 as usize * STRIDE as usize;
        Frame {
            generation,
            full: false,
            damage: vec![rect],
            rows,
            pixels: px[start..end].to_vec(),
        }
    }

    fn empty_frame(generation: u64) -> Frame {
        Frame {
            generation,
            full: false,
            damage: Vec::new(),
            rows: Rows {
                y0: 0,
                y1: 0,
                row_bytes: STRIDE,
            },
            pixels: Vec::new(),
        }
    }

    fn nal_types(au: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 3 < au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
                if i + 3 < au.len() {
                    types.push(au[i + 3] & 0x1f);
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        types
    }

    #[test]
    fn the_first_access_unit_is_an_annex_b_idr() {
        let mut s = Session::new(config()).unwrap();
        let px = background();
        let out = s.handle_frame(&full_frame(1, &px)).unwrap().unwrap();
        assert!(out.keyframe);
        assert!(out.bytes.starts_with(&[0, 0, 0, 1]) || out.bytes.starts_with(&[0, 0, 1]));
        let types = nal_types(&out.bytes);
        assert!(types.contains(&7), "no SPS in {types:?}");
        assert!(types.contains(&8), "no PPS in {types:?}");
        assert!(types.contains(&5), "no IDR slice in {types:?}");
        assert_eq!(out.generation, 1);
        assert_eq!(out.crf, 24.0);
        println!("first access unit: {} bytes, NAL types {types:?}", out.bytes.len());
    }

    #[test]
    fn a_one_macroblock_row_change_is_much_smaller_than_a_full_change() {
        let mut s = Session::new(uncapped_config()).unwrap();
        let mut px = background();
        s.handle_frame(&full_frame(1, &px)).unwrap().unwrap();

        let row = Rect {
            x: 0,
            y: 320,
            w: W,
            h: 16,
        };
        noise(&mut px, row, 1);
        let one_row = s.handle_frame(&partial_frame(2, &px, row)).unwrap().unwrap();

        let full = Rect {
            x: 0,
            y: 0,
            w: W,
            h: H,
        };
        noise(&mut px, full, 2);
        let full_change = s.handle_frame(&full_frame(3, &px)).unwrap().unwrap();

        assert!(!one_row.keyframe);
        assert!(!full_change.keyframe);
        let ratio = full_change.bytes.len() as f64 / one_row.bytes.len() as f64;
        println!(
            "one macroblock row: {} bytes; full change: {} bytes; ratio {ratio:.1}x",
            one_row.bytes.len(),
            full_change.bytes.len()
        );
        assert!(
            ratio >= 4.0,
            "one-row update was {} bytes, full change {} bytes (ratio {ratio:.1})",
            one_row.bytes.len(),
            full_change.bytes.len()
        );
    }

    #[test]
    fn an_unchanged_frame_produces_nothing_or_a_minimal_p_frame() {
        let mut s = Session::new(config()).unwrap();
        let px = background();
        s.handle_frame_at(&full_frame(1, &px), 0).unwrap().unwrap();
        let out = s.handle_frame_at(&empty_frame(2), 50).unwrap();
        assert!(out.is_none(), "an unchanged frame produced an access unit");
        println!("unchanged frame: no access unit");
    }

    #[test]
    fn a_refinement_paints_over_blocks_after_motion_settles() {
        let mut s = Session::new(config()).unwrap();
        let mut px = background();
        s.handle_frame_at(&full_frame(1, &px), 0).unwrap().unwrap();
        let rect = Rect {
            x: 300,
            y: 300,
            w: 200,
            h: 120,
        };
        noise(&mut px, rect, 9);
        let motion = s.handle_frame_at(&partial_frame(2, &px, rect), 0).unwrap().unwrap();
        assert!(!motion.paint_over);
        assert!(s.handle_frame_at(&empty_frame(3), 100).unwrap().is_none());
        let refine = s
            .handle_frame_at(&empty_frame(4), 350)
            .unwrap()
            .expect("settled blocks produce a paint-over frame");
        assert!(refine.paint_over);
        assert_eq!(refine.crf, PAINT_OVER_CRF);
        assert!(!refine.keyframe);
        println!(
            "motion {} bytes at CRF {:.0}; paint-over {} bytes at CRF {:.0}",
            motion.bytes.len(),
            motion.crf,
            refine.bytes.len(),
            refine.crf
        );
    }

    #[test]
    fn a_requested_keyframe_makes_the_next_frame_an_idr() {
        let mut s = Session::new(config()).unwrap();
        let px = background();
        s.handle_frame(&full_frame(1, &px)).unwrap().unwrap();
        s.handle_frame(&empty_frame(2)).unwrap();
        s.request_keyframe();
        let out = s.handle_frame(&empty_frame(3)).unwrap().unwrap();
        assert!(out.keyframe);
        assert!(nal_types(&out.bytes).contains(&5));
    }

    #[test]
    fn pixel_rows_must_cover_the_damaged_macroblock_rows() {
        let mut s = Session::new(config()).unwrap();
        let px = background();
        s.handle_frame(&full_frame(1, &px)).unwrap().unwrap();
        let rect = Rect {
            x: 0,
            y: 320,
            w: 64,
            h: 16,
        };
        let mut f = partial_frame(2, &px, rect);
        f.rows.y0 = 0;
        f.rows.y1 = 16;
        f.pixels = px[..16 * STRIDE as usize].to_vec();
        assert!(matches!(
            s.handle_frame(&f),
            Err(SessionError::Protocol(_))
        ));
    }

    #[test]
    fn generations_must_advance() {
        let mut s = Session::new(config()).unwrap();
        let px = background();
        s.handle_frame(&full_frame(2, &px)).unwrap().unwrap();
        assert!(matches!(
            s.handle_frame(&full_frame(2, &px)),
            Err(SessionError::Protocol(_))
        ));
    }

    #[test]
    fn a_full_frame_with_rects_or_the_wrong_size_is_rejected() {
        let mut s = Session::new(config()).unwrap();
        let px = background();
        let mut f = full_frame(1, &px);
        f.damage.push(Rect {
            x: 0,
            y: 0,
            w: 16,
            h: 16,
        });
        assert!(matches!(
            s.handle_frame(&f),
            Err(SessionError::Protocol(_))
        ));
        let mut f = full_frame(1, &px);
        f.rows.y1 = H - 16;
        f.pixels.truncate((H - 16) as usize * STRIDE as usize);
        assert!(matches!(
            s.handle_frame(&f),
            Err(SessionError::Protocol(_))
        ));
    }
}
