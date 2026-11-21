//! The settle and motion-quality state Voxl keeps around its encoder: blocks that
//! changed are re-encoded at paint-over quality once they have been still for a
//! while (src/video.rs:422-518), and cheap motion gets x264's complexity estimate
//! taken back off the CRF (src/video.rs:952-1024).

use crate::damage::Band;

pub const REFINE_STILL_MS: u64 = 300;
pub const REFINE_EVERY_MS: u64 = 250;

pub struct Refiner {
    height: u32,
    cols: usize,
    changed_ms: Vec<u64>,
    pending: Vec<bool>,
    last_refine_ms: u64,
}

impl Refiner {
    pub fn new(blocks: usize, cols: usize, height: u32) -> Self {
        Refiner {
            height,
            cols,
            changed_ms: vec![0; blocks],
            pending: vec![false; blocks],
            last_refine_ms: 0,
        }
    }

    pub fn mark(&mut self, dirty: &[u8], now_ms: u64) {
        for (i, &d) in dirty.iter().enumerate() {
            if d != 0 {
                self.pending[i] = true;
                self.changed_ms[i] = now_ms;
            }
        }
    }

    pub fn mark_all(&mut self, now_ms: u64) {
        self.pending.fill(true);
        self.changed_ms.fill(now_ms);
    }

    pub fn clear_all(&mut self) {
        self.pending.fill(false);
    }

    pub fn pending(&self) -> usize {
        self.pending.iter().filter(|&&p| p).count()
    }

    /// Blocks still for `REFINE_STILL_MS`, as a map (1 = refine) and the pixel rows
    /// they span. Returns nothing more often than every `REFINE_EVERY_MS`.
    pub fn due(&mut self, now_ms: u64) -> Option<(Vec<u8>, Band)> {
        if now_ms < self.last_refine_ms.saturating_add(REFINE_EVERY_MS) {
            return None;
        }
        let mut map = vec![0u8; self.pending.len()];
        let (mut r0, mut r1, mut any) = (usize::MAX, 0usize, false);
        for (i, m) in map.iter_mut().enumerate() {
            if self.pending[i]
                && now_ms.saturating_sub(self.changed_ms[i]) >= REFINE_STILL_MS
            {
                *m = 1;
                self.pending[i] = false;
                let row = i / self.cols;
                r0 = r0.min(row);
                r1 = r1.max(row + 1);
                any = true;
            }
        }
        if !any {
            return None;
        }
        self.last_refine_ms = now_ms;
        Some((
            map,
            Band {
                y0: (r0 * 16) as u32,
                y1: ((r1 * 16) as u32).min(self.height),
            },
        ))
    }
}

pub const MAX_MOTION_EXCESS: f32 = 12.0;
pub const CHEAP_MB_BITS: f32 = 40.0;

#[derive(Clone, Copy)]
pub struct MotionSize {
    pub bits: usize,
    pub changed: usize,
    pub mbs: usize,
    pub crf: f32,
}

#[derive(Default)]
pub struct MotionQuality {
    complexity: f32,
    mb_bits_log2: f32,
    applied: f32,
}

impl MotionQuality {
    pub fn crf(&mut self, crf: f32) -> f32 {
        let want = if self.mb_bits_log2 < CHEAP_MB_BITS.log2() {
            self.complexity.clamp(0.0, MAX_MOTION_EXCESS)
        } else {
            0.0
        };
        if (want - self.applied).abs() >= 1.0 {
            self.applied = want.round();
        }
        (crf - self.applied).max(0.0)
    }

    pub fn note(
        &mut self,
        crf: f32,
        keyframe: bool,
        qp: Option<f32>,
        crf_avg: f32,
        size: Option<MotionSize>,
    ) {
        let (false, Some(qp)) = (keyframe, qp) else {
            return;
        };
        self.observe(crf, qp, crf_avg);
        if let Some(m) = size.filter(|m| m.changed * 16 >= m.mbs) {
            let b = (m.bits.max(1) as f32 / m.changed.max(1) as f32).log2() + (qp - m.crf) / 6.0;
            self.mb_bits_log2 = self.mb_bits_log2 * 0.7 + b * 0.3;
        }
    }

    fn observe(&mut self, crf: f32, qp: f32, crf_avg: f32) {
        let lo = qp - 0.5 - crf_avg;
        let hi = qp + 0.5 - crf;
        self.complexity = if hi - lo < 1.25 {
            (lo + hi) / 2.0
        } else {
            self.complexity.max(lo).min(hi)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_become_due_only_after_they_settle() {
        let mut refiner = Refiner::new(4, 2, 32);
        refiner.mark(&[1, 0, 0, 1], 0);
        assert_eq!(refiner.pending(), 2);
        assert!(refiner.due(200).is_none());
        assert!(refiner.due(299).is_none());
        let (map, band) = refiner.due(300).expect("settled blocks due at 300 ms");
        assert_eq!(map, vec![1, 0, 0, 1]);
        assert_eq!(band, Band { y0: 0, y1: 32 });
        assert_eq!(refiner.pending(), 0);
        assert!(refiner.due(900).is_none());
    }

    #[test]
    fn a_refine_waits_for_its_interval() {
        let mut refiner = Refiner::new(2, 1, 16);
        refiner.mark_all(0);
        assert!(refiner.due(250).is_none());
        assert!(refiner.due(300).is_some());
        refiner.mark_all(300);
        assert!(refiner.due(540).is_none());
        assert!(refiner.due(600).is_some());
    }

    #[test]
    fn repeated_motion_keeps_a_block_pending() {
        let mut refiner = Refiner::new(1, 1, 16);
        refiner.mark(&[1], 0);
        refiner.mark(&[1], 200);
        assert!(refiner.due(400).is_none());
        assert!(refiner.due(500).is_some());
    }
}
