//! When a preview exists, its size, and the rates both children are judged by.

use std::collections::VecDeque;

/// The preview: the source scaled to fit 1280x720, at most 24 fps.
pub const PREVIEW_WIDTH: u32 = 1280;
pub const PREVIEW_HEIGHT: u32 = 720;
pub const PREVIEW_FPS: f64 = 24.0;

/// A preview exists only when it would be smaller than its source.
pub fn needs_preview(width: u32, height: u32) -> bool {
    width > PREVIEW_WIDTH || height > PREVIEW_HEIGHT
}

/// The source scaled to fit 1280x720, never up, both sides even.
pub fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let s = 1f64.min(PREVIEW_WIDTH as f64 / width as f64).min(PREVIEW_HEIGHT as f64 / height as f64);
    let side = |v: u32| ((v as f64 * s).round() as u32 & !1).max(2);
    (side(width), side(height))
}

/// Passes at most `fps` pictures a second, judged by their source times.
pub struct RateGate {
    period: f64,
    next: Option<f64>,
}

impl RateGate {
    pub fn new(fps: f64) -> RateGate {
        RateGate { period: 1e6 / fps, next: None }
    }

    pub fn pass(&mut self, time_us: u64) -> bool {
        let t = time_us as f64;
        let next = match self.next {
            // an eighth of a period of slack, so a source at exactly the rate never loses a frame to jitter
            Some(next) if t + self.period / 8.0 < next => return false,
            Some(next) if t < next + self.period => next + self.period,
            _ => t + self.period,
        };
        self.next = Some(next);
        true
    }
}

/// Pictures a second over the last two seconds of source time.
#[derive(Default)]
pub struct RateMeter {
    times: VecDeque<u64>,
}

impl RateMeter {
    pub fn add(&mut self, time_us: u64) {
        if self.times.back().is_some_and(|&last| time_us < last) {
            self.times.clear(); // the clock went back: start over
        }
        self.times.push_back(time_us);
        while self.times.len() > 2 && self.times.back().unwrap() - self.times.front().unwrap() > 2_000_000 {
            self.times.pop_front();
        }
    }

    /// 0 until two pictures came at different times.
    pub fn fps(&self) -> f64 {
        match (self.times.front(), self.times.back()) {
            (Some(&first), Some(&last)) if last > first => (self.times.len() - 1) as f64 * 1e6 / (last - first) as f64,
            _ => 0.0,
        }
    }

    pub fn count(&self) -> usize {
        self.times.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many of `n` pictures at `fps` the gate passes.
    fn gated(fps: f64, n: u32) -> u32 {
        let mut g = RateGate::new(PREVIEW_FPS);
        (0..n).filter(|&i| g.pass((i as f64 * 1e6 / fps) as u64)).count() as u32
    }

    #[test]
    fn a_preview_only_when_smaller() {
        assert!(needs_preview(1920, 1080));
        assert!(needs_preview(1440, 1080));
        assert!(needs_preview(1080, 1920));
        assert!(needs_preview(1281, 720));
        assert!(!needs_preview(1280, 720));
        assert!(!needs_preview(640, 480));
    }

    #[test]
    fn preview_fits_720p() {
        assert_eq!(preview_size(1920, 1080), (1280, 720));
        assert_eq!(preview_size(3840, 2160), (1280, 720));
        assert_eq!(preview_size(1440, 1080), (960, 720));
        assert_eq!(preview_size(1080, 1920), (404, 720));
        assert_eq!(preview_size(640, 480), (640, 480));
    }

    #[test]
    fn gate_keeps_24_a_second() {
        // ten seconds at each rate: a slower source keeps every picture
        assert!((238..=242).contains(&gated(30.0, 300)));
        assert!((238..=242).contains(&gated(60.0, 600)));
        assert_eq!(gated(24.0, 240), 240);
        assert_eq!(gated(15.0, 150), 150);
    }

    #[test]
    fn meter_measures_the_rate() {
        let mut m = RateMeter::default();
        for i in 0..90u64 {
            m.add(i * 1_000_000 / 30);
        }
        assert!((29.5..30.5).contains(&m.fps()));
        m.add(0);
        assert_eq!((m.count(), m.fps()), (1, 0.0));
    }
}
