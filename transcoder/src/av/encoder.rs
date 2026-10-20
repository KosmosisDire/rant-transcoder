use std::ffi::{CString, c_void};
use std::os::raw::c_int;

use super::{Colors, Dict, Frame, Packet, PixelFormat, hw, static_text};
use crate::annexb::ParameterSets;
use crate::ffi;

/// The H264 encoders tried, best first: the hardware ones, then openh264. A full NVENC
/// falls through to the next.
#[cfg(windows)]
const ORDER: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "libopenh264"];
#[cfg(target_os = "macos")]
const ORDER: &[&str] = &["h264_videotoolbox", "libopenh264"];
#[cfg(not(any(windows, target_os = "macos")))]
const ORDER: &[&str] = &["h264_nvenc", "h264_qsv", "libopenh264"];

/// An H264 encoder set for low latency: no B frames, no lookahead, constrained baseline,
/// parameter sets in front of every keyframe.
pub struct Encoder {
    ctx: *mut ffi::AVCodecContext,
    colors: Colors,
    packet: Packet,
    last_pts: i64,
    params: ParameterSets,
}

// SAFETY: the context belongs to this encoder alone
unsafe impl Send for Encoder {}

impl Encoder {
    /// The first encoder that opens at this size, or only the one named. None when none does.
    pub fn open(width: u32, height: u32, fps: f64, colors: Colors, only: Option<&str>) -> Option<Encoder> {
        let fps = if fps > 0.0 { fps } else { 30.0 };
        let names: Vec<&str> = only.map_or(ORDER.to_vec(), |n| vec![n]);
        for name in names {
            if name == "h264_qsv" && cfg!(target_os = "linux") && !hw::have_vaapi() {
                continue;
            }
            let Ok(cname) = CString::new(name) else { continue };
            // SAFETY: a lookup in FFmpeg's static tables, then a fresh context set up before
            // its open and freed when the open fails
            unsafe {
                let codec = ffi::avcodec_find_encoder_by_name(cname.as_ptr());
                if codec.is_null() {
                    continue;
                }
                let mut ctx = ffi::avcodec_alloc_context3(codec);
                let c = &mut *ctx;
                c.width = width as c_int;
                c.height = height as c_int;
                c.time_base = ffi::AVRational { num: 1, den: 1_000_000 };
                c.framerate = ffi::av_d2q(fps, 1000);
                c.pix_fmt = input_format(ctx, codec);
                c.bit_rate = bitrate_for(width, height, fps);
                c.gop_size = ((fps * 2.0).round() as c_int).max(1);
                c.max_b_frames = 0;
                colors.stamp_encoder(c);
                let mut options = Dict::of(tune(name));
                if ffi::avcodec_open2(ctx, codec, &mut options.0) == 0 {
                    return Some(Encoder { ctx, colors, packet: Packet::new(), last_pts: i64::MIN, params: ParameterSets::default() });
                }
                ffi::avcodec_free_context(&mut ctx);
            }
        }
        None
    }

    fn raw(&self) -> &ffi::AVCodecContext {
        // SAFETY: open until drop
        unsafe { &*self.ctx }
    }

    pub fn width(&self) -> u32 {
        self.raw().width as u32
    }

    pub fn height(&self) -> u32 {
        self.raw().height as u32
    }

    /// The colours its stream signals.
    pub fn colors(&self) -> Colors {
        self.colors
    }

    /// The picture format it takes.
    pub fn format(&self) -> PixelFormat {
        PixelFormat(self.raw().pix_fmt)
    }

    pub fn name(&self) -> &'static str {
        // SAFETY: an open context names its codec
        static_text(unsafe { (*self.raw().codec).name })
    }

    /// Encodes a picture at the encoder's size and format. `keyframe` forces an IDR. `out`
    /// gets each access unit in Annex B and whether it is a keyframe.
    pub fn encode(&mut self, picture: &mut Frame, time_us: u64, keyframe: bool, mut out: impl FnMut(&[u8], bool)) {
        let pts = (time_us as i64).max(self.last_pts.saturating_add(1));
        self.last_pts = pts;
        // SAFETY: the picture is ours to stamp, and each packet is ours between receive and unref
        unsafe {
            let p = &mut *picture.as_mut_ptr();
            p.pts = pts;
            p.pict_type = if keyframe { ffi::AV_PICTURE_TYPE_I } else { ffi::AV_PICTURE_TYPE_NONE };
            if ffi::avcodec_send_frame(self.ctx, p) < 0 {
                return;
            }
            while ffi::avcodec_receive_packet(self.ctx, self.packet.as_ptr()) == 0 {
                let key = self.packet.raw().flags & ffi::AV_PKT_FLAG_KEY as c_int != 0;
                out(&self.params.complete(self.packet.data(), key), key);
                self.packet.unref();
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: ours alone
        unsafe { ffi::avcodec_free_context(&mut self.ctx) };
    }
}

/// About 0.08 bits a pixel.
fn bitrate_for(width: u32, height: u32, fps: f64) -> i64 {
    ((width as f64 * height as f64 * fps.max(1.0) * 0.08) as i64).clamp(200_000, 40_000_000)
}

/// Each encoder's low latency settings. Constrained baseline decodes in every browser.
fn tune(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "h264_nvenc" => &[("preset", "p1"), ("tune", "ull"), ("zerolatency", "1"), ("delay", "0"), ("rc", "cbr"), ("forced-idr", "1"), ("profile", "baseline")],
        "h264_qsv" => &[("preset", "veryfast"), ("async_depth", "1"), ("look_ahead", "0"), ("forced_idr", "1"), ("profile", "baseline")],
        "h264_amf" => &[("usage", "ultralowlatency"), ("quality", "speed"), ("profile", "constrained_baseline")],
        "h264_videotoolbox" => &[("realtime", "1"), ("profile", "baseline")],
        "libopenh264" => &[("profile", "constrained_baseline"), ("allow_skip_frames", "0"), ("rc_mode", "bitrate")],
        _ => &[],
    }
}

/// NV12 when the encoder takes it, since hardware encoders read it natively, else 4:2:0 planar.
unsafe fn input_format(ctx: *const ffi::AVCodecContext, codec: *const ffi::AVCodec) -> ffi::AVPixelFormat {
    let mut list: *const c_void = std::ptr::null();
    let mut n: c_int = 0;
    // SAFETY: FFmpeg points list at a static array of n formats
    let formats: &[ffi::AVPixelFormat] = unsafe {
        if ffi::avcodec_get_supported_config(ctx, codec, ffi::AV_CODEC_CONFIG_PIX_FORMAT, 0, &mut list, &mut n) < 0 || list.is_null() || n <= 0 {
            return ffi::AV_PIX_FMT_YUV420P;
        }
        std::slice::from_raw_parts(list.cast(), n as usize)
    };
    [ffi::AV_PIX_FMT_NV12, ffi::AV_PIX_FMT_YUV420P].into_iter().find(|f| formats.contains(f)).unwrap_or(formats[0])
}
