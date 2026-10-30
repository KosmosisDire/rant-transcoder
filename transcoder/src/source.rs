//! Where a stream's units come from: a Rant topic on this host, or a URL through FFmpeg.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rant::types::{ImageFormat, VideoCodec, VideoStreamKind};
use rant::{Bytes, MessageView, Node, Qos, Subscriber};

use crate::annexb;
use crate::av::{Cancel, CodecId, CodecParameters, Frame, Input, Layout, PixelFormat, Wrapped};

/// One step of input: a raw picture, or one coded access unit.
pub struct Unit<'a> {
    pub payload: Payload<'a>,
    /// The coded size the source states, 0 when unstated.
    pub width: u32,
    pub height: u32,
    /// When it was true, UTC: capture, else written, else arrival.
    pub time_us: u64,
}

pub enum Payload<'a> {
    /// Wraps the message's bytes until the next read.
    Picture(Wrapped<'a>),
    Coded { codec: CodecId, data: &'a [u8], key: bool },
}

pub trait Source: Send {
    /// The next unit, None when none came in time. A URL blocks until a unit, a cancel or a
    /// stall, whatever `timeout` says.
    fn read(&mut self, timeout: Duration) -> Option<Unit<'_>>;

    /// A URL that ended or failed, to be opened again.
    fn ended(&self) -> bool {
        false
    }

    /// A demuxed stream's parameters for its decoder, None for a Rant topic.
    fn parameters(&self) -> Option<CodecParameters> {
        None
    }

    /// The stream's own frame rate claim, 0 when it makes none.
    fn stated_fps(&self) -> f64 {
        0.0
    }
}

/// UTC microseconds now.
pub fn wall_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

/// Rant's raw picture formats as FFmpeg names them.
const RAW: [(ImageFormat, PixelFormat); 8] = [
    (ImageFormat::Mono8, PixelFormat::GRAY8),
    (ImageFormat::Mono16, PixelFormat::GRAY16LE),
    (ImageFormat::Rgb8, PixelFormat::RGB24),
    (ImageFormat::Rgba8, PixelFormat::RGBA),
    (ImageFormat::Bgr8, PixelFormat::BGR24),
    (ImageFormat::Yuyv, PixelFormat::YUYV422),
    (ImageFormat::Nv12, PixelFormat::NV12),
    (ImageFormat::Monof32, PixelFormat::GRAYF32LE),
];

/// An Image or VideoFrame topic, taking messages from one publisher only. An Image source
/// reads the newest picture, a VideoFrame one every frame.
pub struct TopicSource {
    sub: Subscriber<Bytes>,
    image: bool,
    publisher: u32,
    picture: Frame,
}

impl TopicSource {
    pub fn open(node: &Node, name: &str, image: bool, publisher: u32) -> Result<TopicSource, rant::Error> {
        // every coded frame matters to the decoder, so hold a burst. A raw reader keeps only the newest
        let qos = Qos::new().reflect_from_mesh(true).queue_bytes(if image { 0 } else { 16 << 20 });
        Ok(TopicSource { sub: node.pulled_subscriber::<Bytes>(name, qos)?, image, publisher, picture: Frame::new() })
    }
}

impl Source for TopicSource {
    fn read(&mut self, timeout: Duration) -> Option<Unit<'_>> {
        let m = if self.image { self.sub.take_latest(timeout) } else { self.sub.take(timeout) }.ok().flatten()?;
        // a remote publisher of the same name is skipped, only this host's is ours
        if m.publisher_id() != self.publisher {
            return None;
        }
        let time_us = [m.capture_us(), m.written_us()].into_iter().find(|&t| t != 0).unwrap_or_else(wall_us);
        let (width, height) = (m.get_uint("width") as u32, m.get_uint("height") as u32);
        let data = m.get_array("data");
        if data.is_empty() {
            return None;
        }
        let payload = if self.image { image(&m, data, width, height, &mut self.picture)? } else { video(&m, data)? };
        // a frame's pts is when it was true, unless the message says so itself
        let pts = m.get_int("pts");
        let time_us = if !self.image && m.capture_us() == 0 && pts > 0 { pts as u64 } else { time_us };
        Some(Unit { payload, width, height, time_us })
    }
}

fn image<'a>(m: &MessageView<'a>, data: &'a [u8], width: u32, height: u32, picture: &'a mut Frame) -> Option<Payload<'a>> {
    let format = m.get_uint("format");
    let codec = match format {
        f if f == ImageFormat::Jpeg as u64 => CodecId::MJPEG,
        f if f == ImageFormat::Png as u64 => CodecId::PNG,
        _ => {
            let (_, pixels) = RAW.iter().find(|(f, _)| *f as u64 == format)?;
            let layout = Layout { width, height, stride: m.get_uint("stride") as u32, format: *pixels };
            return picture.wrap(data, layout).map(Payload::Picture);
        }
    };
    Some(Payload::Coded { codec, data, key: true })
}

fn video<'a>(m: &MessageView<'a>, data: &'a [u8]) -> Option<Payload<'a>> {
    let codec = match m.get_uint("codec") {
        c if c == VideoCodec::Mjpeg as u64 => CodecId::MJPEG,
        c if c == VideoCodec::H264 as u64 => CodecId::H264,
        c if c == VideoCodec::H265 as u64 => CodecId::HEVC,
        c if c == VideoCodec::Av1 as u64 => CodecId::AV1,
        _ => return None,
    };
    let idr = match codec {
        CodecId::H264 => annexb::has_idr(annexb::Codec::H264, data),
        CodecId::HEVC => annexb::has_idr(annexb::Codec::H265, data),
        _ => false,
    };
    Some(Payload::Coded { codec, data, key: m.get_bool("keyframe") || codec == CodecId::MJPEG || idr })
}

/// An ExternalVideoStream's URL, demuxed by FFmpeg and paced to the stream's own clock.
pub struct UrlSource {
    input: Input,
    width: u32,
    height: u32,
    ended: bool,
    /// The wall clock and stream time of the first unit, which later units are paced by.
    anchor: Option<(i64, i64)>,
    last_us: i64,
    cancel: Cancel,
}

impl UrlSource {
    /// `cancel` interrupts a blocking open or read.
    pub fn open(url: &str, kind: VideoStreamKind, cancel: Cancel) -> Result<UrlSource, String> {
        let mut options = vec![("fflags", "nobuffer"), ("probesize", "1000000"), ("analyzeduration", "1000000")];
        match kind {
            VideoStreamKind::WebrtcWhep => return Err("WHEP is not supported yet".into()),
            VideoStreamKind::Rtsp => options.push(("rtsp_transport", "tcp")),
            VideoStreamKind::Rtp => options.push(("protocol_whitelist", "file,udp,rtp,crypto,data")),
            _ => {}
        }
        let format = (kind == VideoStreamKind::HttpMjpeg).then_some("mpjpeg");
        let input = Input::open(url, format, &options, cancel.clone())?;
        let p = input.parameters();
        Ok(UrlSource { width: p.width(), height: p.height(), input, ended: false, anchor: None, last_us: 0, cancel })
    }
}

/// Sleeps, in steps short enough that a cancel never waits on a sparse stream's long gaps.
fn pace(how_long: Duration, cancel: &Cancel) {
    let from = Instant::now();
    while let Some(left) = how_long.checked_sub(from.elapsed()) {
        if left.is_zero() || cancel() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

impl Source for UrlSource {
    fn read(&mut self, _timeout: Duration) -> Option<Unit<'_>> {
        if self.ended {
            return None;
        }
        let codec = self.input.codec();
        let p = match self.input.read() {
            Ok(p) => p,
            Err(why) => {
                tracing::debug!("stream ended: {why}");
                self.ended = true;
                return None;
            }
        };
        // maps stream time onto the wall clock from the first unit, and sleeps when a file or
        // a burst runs ahead of it, so units leave at the stream's own pace
        let now = wall_us() as i64;
        let time_us = match p.time_us {
            None => now,
            Some(us) => {
                let drift = self.anchor.map_or(0, |(wall, base)| wall + (us - base) - now);
                if self.anchor.is_none() || us < self.last_us || drift.abs() > 2_000_000 {
                    self.anchor = Some((now, us));
                } else if drift > 0 {
                    pace(Duration::from_micros(drift as u64), &self.cancel);
                }
                self.last_us = us;
                let (wall, base) = self.anchor.unwrap();
                wall + (us - base)
            }
        };
        let key = p.key || codec == CodecId::MJPEG;
        Some(Unit { payload: Payload::Coded { codec, data: p.data, key }, width: self.width, height: self.height, time_us: time_us as u64 })
    }

    fn ended(&self) -> bool {
        self.ended
    }

    fn parameters(&self) -> Option<CodecParameters> {
        Some(self.input.parameters())
    }

    fn stated_fps(&self) -> f64 {
        self.input.stated_fps()
    }
}
