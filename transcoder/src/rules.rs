//! Which children a source gets, each one's size, and the rates they are judged by.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

/// The preview: the source scaled to fit 1280x720, at most 24 fps.
pub const PREVIEW_WIDTH: u32 = 1280;
pub const PREVIEW_HEIGHT: u32 = 720;
pub const PREVIEW_FPS: f64 = 24.0;

/// The MJPEG preview, which fits this on its longest side at the same rate. JPEG costs far more
/// bytes a pixel than H264, so a low stream of it has to be smaller.
pub const MJPEG_PREVIEW_SIDE: u32 = 640;

/// H264's largest picture at level 5.2, the most players are held to decode: 4096 a side and
/// 36864 blocks of 16x16, which 4096x2304 fills.
pub const H264_MAX_SIDE: u32 = 4096;
pub const H264_MAX_BLOCKS: u32 = 36864;

/// The smallest side every encoder takes: one block. openh264, the last tried, needs it.
pub const H264_MIN_SIDE: u32 = 16;

/// Rant's name limit, which a child's name must also fit.
const NAME_MAX: usize = 64;

/// A source's size past this is no real size: JPEG and H264 stop here.
pub const SIDE_MAX: u32 = 65535;

/// A stream's children: H264 and MJPEG, each at the source's size and as a smaller preview.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Video,
    Preview,
    Mjpeg,
    MjpegPreview,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Video, Kind::Preview, Kind::Mjpeg, Kind::MjpegPreview];

    pub fn suffix(self) -> &'static str {
        match self {
            Kind::Video => "/Video",
            Kind::Preview => "/Preview",
            Kind::Mjpeg => "/Mjpeg",
            Kind::MjpegPreview => "/MjpegPreview",
        }
    }

    pub fn is_preview(self) -> bool {
        matches!(self, Kind::Preview | Kind::MjpegPreview)
    }

    pub fn is_mjpeg(self) -> bool {
        matches!(self, Kind::Mjpeg | Kind::MjpegPreview)
    }

    /// The child's picture for a source of this size.
    pub fn size(self, width: u32, height: u32) -> (u32, u32) {
        match self {
            Kind::Video | Kind::Mjpeg => video_size(width, height),
            Kind::Preview => preview_size(width, height),
            Kind::MjpegPreview => fit(width, height, MJPEG_PREVIEW_SIDE, MJPEG_PREVIEW_SIDE),
        }
    }

    /// FFmpeg's JPEG scale for an MJPEG child, 2 best to 31 worst. The preview's is where going
    /// coarser starts to show blocks and save little.
    pub fn jpeg_quantizer(self) -> u8 {
        if self.is_preview() { 15 } else { 5 }
    }
}

/// How a source arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Coding {
    Raw,
    H264,
    Mjpeg,
    Other,
}

/// Whether a child changes its source. A copy of what is on the mesh already never does, nor
/// does a preview the size of its source. `on_mesh` is false for a URL, which a copy brings there.
pub fn needs_child(kind: Kind, coding: Coding, on_mesh: bool, width: u32, height: u32) -> bool {
    match kind {
        Kind::Video => !(on_mesh && coding == Coding::H264),
        Kind::Mjpeg => !(on_mesh && coding == Coding::Mjpeg),
        Kind::Preview => needs_preview(width, height),
        Kind::MjpegPreview => width > MJPEG_PREVIEW_SIDE || height > MJPEG_PREVIEW_SIDE,
    }
}

/// The child's full name fits Rant's limit.
pub fn name_fits(parent: &str, kind: Kind) -> bool {
    parent.len() + kind.suffix().len() <= NAME_MAX
}

/// Video keeps its source's size with both sides even, grown to one block a side or shrunk to
/// fit H264's largest picture.
pub fn video_size(width: u32, height: u32) -> (u32, u32) {
    let (wf, hf) = (width.max(1) as f64, height.max(1) as f64);
    let side = H264_MAX_SIDE as f64;
    let up = (H264_MIN_SIDE as f64 / wf.min(hf)).max(1.0);
    let mut s = up.min(side / wf).min(side / hf).min((H264_MAX_BLOCKS as f64 * 256.0 / (wf * hf)).sqrt());
    loop {
        let even = |v: f64| ((v * s).round() as u32 & !1).max(H264_MIN_SIDE);
        let (w, h) = (even(wf), even(hf));
        if w <= H264_MAX_SIDE && h <= H264_MAX_SIDE && w.div_ceil(16) * h.div_ceil(16) <= H264_MAX_BLOCKS {
            return (w, h);
        }
        s *= 0.995;
    }
}

/// A preview exists only when it would be smaller than its source.
pub fn needs_preview(width: u32, height: u32) -> bool {
    width > PREVIEW_WIDTH || height > PREVIEW_HEIGHT
}

/// The source scaled to fit 1280x720, never up, both sides even.
pub fn preview_size(width: u32, height: u32) -> (u32, u32) {
    fit(width, height, PREVIEW_WIDTH, PREVIEW_HEIGHT)
}

/// A picture scaled to fit a box, never up, both sides even.
fn fit(width: u32, height: u32, box_width: u32, box_height: u32) -> (u32, u32) {
    let s = 1f64.min(box_width as f64 / width as f64).min(box_height as f64 / height as f64);
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
            // the source's clock went back, so what passed last says nothing
            Some(next) if next - t > 1e6 => t + self.period,
            // an eighth of a period of slack, so a source at exactly the rate never loses a frame to jitter
            Some(next) if t + self.period / 8.0 < next => return false,
            Some(next) if t < next + self.period => next + self.period,
            _ => t + self.period,
        };
        self.next = Some(next);
        true
    }
}

/// How long a thread has been at one step of its work, for spotting one stuck in a driver,
/// which no timeout reaches. Times are milliseconds on any clock that only rises, 0 for idle.
#[derive(Default)]
pub struct Busy(AtomicU64);

impl Busy {
    pub fn begin(&self, now_ms: u64) {
        self.0.store(now_ms.max(1), Ordering::Relaxed);
    }

    pub fn end(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    pub fn stuck(&self, now_ms: u64, limit_ms: u64) -> bool {
        let since = self.0.load(Ordering::Relaxed);
        since != 0 && now_ms.saturating_sub(since) > limit_ms
    }
}

/// A gap in source time past this is a jump of its clock, not a slow source.
const CLOCK_JUMP_US: u64 = 30_000_000;

/// Pictures a second over the last two seconds of source time.
#[derive(Default)]
pub struct RateMeter {
    times: VecDeque<u64>,
}

impl RateMeter {
    pub fn add(&mut self, time_us: u64) {
        if self.times.back().is_some_and(|&last| time_us < last || time_us - last > CLOCK_JUMP_US) {
            self.times.clear(); // the clock jumped: start over
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
    fn video_is_even_and_fits_h264() {
        assert_eq!(video_size(1920, 1080), (1920, 1080));
        assert_eq!(video_size(1921, 1081), (1920, 1080));
        assert_eq!(video_size(7680, 4320), (4096, 2304));
        assert_eq!(video_size(4096, 4096), (3072, 3072));
        assert_eq!(video_size(8192, 1024), (4096, 512));
        assert_eq!(video_size(1, 1), (16, 16));
        assert_eq!(video_size(8, 6), (20, 16));
        assert_eq!(video_size(32, 24), (32, 24));
        for (w, h) in [(5000, 3000), (4097, 2305), (3000, 9000), (12345, 678), (3, 100_000)] {
            let (vw, vh) = video_size(w, h);
            assert!(vw % 2 == 0 && vh % 2 == 0 && (16..=4096).contains(&vw) && (16..=4096).contains(&vh), "{w}x{h} gives {vw}x{vh}");
            assert!(vw.div_ceil(16) * vh.div_ceil(16) <= H264_MAX_BLOCKS, "{w}x{h} gives {vw}x{vh}");
        }
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
    fn children_only_where_they_change_the_source() {
        let needs = |kind, coding, on_mesh, w, h| needs_child(kind, coding, on_mesh, w, h);
        // a topic that is H264 already gets no H264 copy, one that is MJPEG no MJPEG copy
        assert!(!needs(Kind::Video, Coding::H264, true, 640, 480));
        assert!(needs(Kind::Mjpeg, Coding::H264, true, 640, 480));
        assert!(needs(Kind::Video, Coding::Mjpeg, true, 640, 480));
        assert!(!needs(Kind::Mjpeg, Coding::Mjpeg, true, 640, 480));
        // raw pictures and other codecs get both
        for coding in [Coding::Raw, Coding::Other] {
            assert!(needs(Kind::Video, coding, true, 640, 480) && needs(Kind::Mjpeg, coding, true, 640, 480));
        }
        // a URL's copy brings it onto the mesh
        assert!(needs(Kind::Video, Coding::H264, false, 640, 480));
        assert!(needs(Kind::Mjpeg, Coding::Mjpeg, false, 640, 480));
        // the H264 preview only above 720p, the MJPEG one only above 640 a side
        assert!(!needs(Kind::Preview, Coding::Raw, true, 1280, 720));
        assert!(needs(Kind::Preview, Coding::H264, true, 1920, 1080));
        assert!(needs(Kind::MjpegPreview, Coding::Raw, true, 1280, 720));
        assert!(needs(Kind::MjpegPreview, Coding::Mjpeg, true, 641, 360));
        assert!(needs(Kind::MjpegPreview, Coding::H264, true, 360, 641));
        assert!(!needs(Kind::MjpegPreview, Coding::Raw, true, 640, 480));
        assert!(!needs(Kind::MjpegPreview, Coding::Raw, true, 360, 640));
    }

    #[test]
    fn the_mjpeg_preview_is_smaller_and_coarser() {
        for ((w, h), want) in [((1920, 1080), (640, 360)), ((1280, 720), (640, 360)), ((4000, 3000), (640, 480)), ((1080, 1920), (360, 640))] {
            assert_eq!(Kind::MjpegPreview.size(w, h), want, "{w}x{h}");
        }
        assert_eq!(Kind::Mjpeg.size(1920, 1080), (1920, 1080));
        assert!(Kind::MjpegPreview.jpeg_quantizer() > Kind::Mjpeg.jpeg_quantizer());
    }

    #[test]
    fn a_long_name_loses_only_the_children_that_do_not_fit() {
        let name = "n".repeat(58);
        assert!(name_fits(&name, Kind::Video) && name_fits(&name, Kind::Mjpeg));
        assert!(!name_fits(&name, Kind::Preview) && !name_fits(&name, Kind::MjpegPreview));
    }

    #[test]
    fn busy_says_stuck_only_past_the_limit() {
        let b = Busy::default();
        assert!(!b.stuck(1_000_000, 1000));
        b.begin(5000);
        assert!(!b.stuck(5500, 1000));
        assert!(b.stuck(6001, 1000));
        b.end();
        assert!(!b.stuck(6001, 1000));
    }

    #[test]
    fn gate_starts_over_when_time_goes_back() {
        let mut g = RateGate::new(PREVIEW_FPS);
        let hour = 3_600_000_000u64;
        assert!(g.pass(hour));
        assert!(!g.pass(hour + 1_000));
        assert!(g.pass(1_000)); // back an hour
        assert!(!g.pass(2_000));
        assert!(g.pass(1_000 + 50_000));
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
        // a jump ahead starts over too, a slow source does not
        m.add(10_000_000);
        assert_eq!(m.count(), 2);
        m.add(3_700_000_000);
        assert_eq!((m.count(), m.fps()), (1, 0.0));
    }
}
