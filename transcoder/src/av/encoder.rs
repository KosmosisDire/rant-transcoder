use std::ffi::{CString, c_void};
use std::os::raw::c_int;
use std::sync::Mutex;

use super::hw::{self, Frames, Kind};
use super::{Colors, Dict, Error, Frame, Packet, PixelFormat, static_text};
use crate::annexb::ParameterSets;
use crate::ffi;

/// FFmpeg filters that bring a device's pictures to a width and height, NV12 and limited
/// range, told whether they are full range now.
pub(super) type Chain = fn(u32, u32, bool) -> String;

/// What the transcoder knows of one H264 encoder.
pub(super) struct Spec {
    pub(super) name: &'static str,
    /// Its low latency settings, constrained baseline among them, which every browser decodes.
    tune: &'static [(&'static str, &'static str)],
    /// The device whose pictures it takes in place, and the chain that brings them there.
    pub(super) device: Option<(Kind, Chain)>,
    /// It takes that device's pictures alone, so pictures in system memory go up to it first.
    pub(super) device_only: bool,
}

#[cfg(not(target_os = "macos"))]
const NVENC: Spec = Spec {
    name: "h264_nvenc",
    tune: &[("preset", "p1"), ("tune", "ull"), ("zerolatency", "1"), ("delay", "0"), ("rc", "cbr"), ("forced-idr", "1"), ("profile", "baseline")],
    device: Some((Kind::CUDA, |w, h, full| format!("{}scale_cuda=w={w}:h={h}:format=nv12", if full { "colorspace_cuda=range=tv," } else { "" }))),
    device_only: false,
};
#[cfg(not(target_os = "macos"))]
const QSV: Spec = Spec {
    name: "h264_qsv",
    // Intel numbers constrained baseline 578, as FFmpeg does, and its own "baseline" is plain
    tune: &[("preset", "veryfast"), ("async_depth", "1"), ("look_ahead", "0"), ("forced_idr", "1"), ("profile", "578")],
    device: Some((QSV_FROM, |w, h, _| format!("hwmap=derive_device=qsv,format=qsv,vpp_qsv=w={w}:h={h}:format=nv12:out_range=limited"))),
    device_only: false,
};
/// The device Quick Sync maps pictures from.
#[cfg(windows)]
const QSV_FROM: Kind = Kind::D3D11_INTEL;
#[cfg(not(any(windows, target_os = "macos")))]
const QSV_FROM: Kind = Kind::VAAPI;
#[cfg(windows)]
const AMF: Spec = Spec { name: "h264_amf", tune: &[("usage", "ultralowlatency"), ("quality", "speed"), ("profile", "constrained_baseline")], device: None, device_only: false };
/// Media Foundation, held to a hardware encoder, since openh264 does software better.
#[cfg(windows)]
const MF: Spec = Spec { name: "h264_mf", tune: &[("hw_encoding", "1"), ("scenario", "display_remoting"), ("rate_control", "cbr")], device: None, device_only: false };
#[cfg(not(any(windows, target_os = "macos")))]
const VAAPI: Spec = Spec {
    name: "h264_vaapi",
    tune: &[("profile", "constrained_baseline"), ("coder", "cavlc"), ("rc_mode", "CBR"), ("async_depth", "1")],
    device: Some((Kind::VAAPI, |w, h, _| format!("scale_vaapi=w={w}:h={h}:format=nv12:out_range=limited"))),
    device_only: true,
};
/// The Raspberry Pi 4's encoder, which takes FFmpeg's profile number.
#[cfg(not(any(windows, target_os = "macos")))]
const V4L2: Spec = Spec { name: "h264_v4l2m2m", tune: &[("profile", "578")], device: None, device_only: false };
#[cfg(target_os = "macos")]
const VIDEOTOOLBOX: Spec = Spec { name: "h264_videotoolbox", tune: &[("realtime", "1"), ("profile", "baseline")], device: None, device_only: false };
const OPENH264: Spec = Spec {
    name: "libopenh264",
    tune: &[("profile", "constrained_baseline"), ("allow_skip_frames", "0"), ("rc_mode", "bitrate")],
    device: None,
    device_only: false,
};

/// The H264 encoders tried, best first. One that is full or missing falls through to the next.
/// Vulkan's is left out: NVIDIA's makes High only, not the constrained baseline browsers decode.
#[cfg(windows)]
pub(super) const ORDER: &[Spec] = &[NVENC, QSV, AMF, MF, OPENH264];
#[cfg(target_os = "macos")]
pub(super) const ORDER: &[Spec] = &[VIDEOTOOLBOX, OPENH264];
#[cfg(not(any(windows, target_os = "macos")))]
pub(super) const ORDER: &[Spec] = &[NVENC, QSV, VAAPI, V4L2, OPENH264];

/// The encoders to try: all of them, or only the one named.
pub(super) fn specs(only: Option<&str>) -> impl Iterator<Item = &'static Spec> {
    ORDER.iter().filter(move |s| only.is_none_or(|n| n == s.name))
}

/// An H264 encoder set for low latency: no B frames, no lookahead, constrained baseline,
/// parameter sets in front of every keyframe.
pub struct Encoder {
    ctx: *mut ffi::AVCodecContext,
    colors: Colors,
    packet: Packet,
    last_pts: i64,
    /// Added to source times, so the times it gets rise whatever the source's clock does.
    shift: i64,
    /// H264's, which a JPEG has no need of.
    params: Option<ParameterSets>,
    /// The quantizer a JPEG picture is told, in FFmpeg's lambda.
    quality: c_int,
}

// SAFETY: the context belongs to this encoder alone
unsafe impl Send for Encoder {}

/// What an encoder's picture times step by when the source's clock goes back.
const PTS_STEP: i64 = 33_333;

/// Held while an encoder opens or closes. Media Foundation unloads the GPU driver on close,
/// which crashes or hangs another thread opening one at the same time.
static OPENING: Mutex<()> = Mutex::new(());

impl Encoder {
    /// Opens one encoder at a size, for pictures in system memory or, given a device's pool,
    /// for that pool's pictures. None when it does not open.
    pub(super) fn open(spec: &Spec, width: u32, height: u32, fps: f64, colors: Colors, frames: Option<&Frames>) -> Option<Encoder> {
        // a source's clock can claim any rate, and encoders refuse the extremes. NVENC under WSL
        // crashes at exactly 1 fps on a busy picture, 9 runs of 10, and never at 2
        let fps = if fps > 0.0 { fps.clamp(2.0, 240.0) } else { 30.0 };
        if spec.name == "h264_qsv" && cfg!(target_os = "linux") && !hw::have_vaapi() {
            return None;
        }
        let cname = CString::new(spec.name).ok()?;
        let _one = OPENING.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: a lookup in FFmpeg's static tables, then a fresh context set up before its
        // open and freed when the open fails. A pool reference is a hardware frames context
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                return None;
            }
            let mut ctx = ffi::avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.width = width as c_int;
            c.height = height as c_int;
            c.time_base = ffi::AVRational { num: 1, den: 1_000_000 };
            c.framerate = ffi::av_d2q(fps, 1000);
            if let Some(pool) = frames {
                let pool_ctx = &*((*pool.as_ptr()).data as *const ffi::AVHWFramesContext);
                (c.pix_fmt, c.sw_pix_fmt) = (pool_ctx.format, pool_ctx.sw_format);
                c.hw_frames_ctx = ffi::av_buffer_ref(pool.as_ptr());
            } else {
                c.pix_fmt = input_format(ctx, codec);
            }
            c.bit_rate = bitrate_for(width, height, fps);
            c.gop_size = ((fps * 2.0).round() as c_int).max(1);
            c.max_b_frames = 0;
            colors.stamp_encoder(c);
            let mut options = Dict::of(spec.tune);
            if ffi::avcodec_open2(ctx, codec, &mut options.0) == 0 {
                return Some(Encoder { ctx, colors, packet: Packet::new(), last_pts: i64::MIN, shift: 0, params: Some(ParameterSets::default()), quality: 0 });
            }
            ffi::avcodec_free_context(&mut ctx);
        }
        None
    }

    /// Opens FFmpeg's own JPEG encoder at a fixed quantizer, 2 best to 31 worst. It stays on one
    /// thread, since its slices need restart markers that the simplest JPEG readers lack.
    pub(super) fn open_jpeg(width: u32, height: u32, quantizer: u8) -> Option<Encoder> {
        let colors = Colors::JFIF;
        // SAFETY: a lookup in FFmpeg's static tables, then a fresh context set up before its
        // open and freed when the open fails
        unsafe {
            let codec = ffi::avcodec_find_encoder(ffi::AV_CODEC_ID_MJPEG);
            if codec.is_null() {
                return None;
            }
            let mut ctx = ffi::avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.width = width as c_int;
            c.height = height as c_int;
            c.time_base = ffi::AVRational { num: 1, den: 1_000_000 };
            c.pix_fmt = ffi::AV_PIX_FMT_YUV420P;
            c.thread_count = 1;
            c.flags |= ffi::AV_CODEC_FLAG_QSCALE as c_int;
            c.qmin = quantizer as c_int;
            c.qmax = quantizer as c_int;
            colors.stamp_encoder(c);
            if ffi::avcodec_open2(ctx, codec, std::ptr::null_mut()) == 0 {
                let quality = quantizer as c_int * ffi::FF_QP2LAMBDA as c_int;
                return Some(Encoder { ctx, colors, packet: Packet::new(), last_pts: i64::MIN, shift: 0, params: None, quality });
            }
            ffi::avcodec_free_context(&mut ctx);
        }
        None
    }

    pub(super) fn is_jpeg(&self) -> bool {
        self.params.is_none()
    }

    fn raw(&self) -> &ffi::AVCodecContext {
        // SAFETY: open until drop
        unsafe { &*self.ctx }
    }

    pub(super) fn width(&self) -> u32 {
        self.raw().width as u32
    }

    pub(super) fn height(&self) -> u32 {
        self.raw().height as u32
    }

    /// The colours its stream signals.
    pub(super) fn colors(&self) -> Colors {
        self.colors
    }

    /// The picture format it takes.
    pub(super) fn format(&self) -> PixelFormat {
        PixelFormat(self.raw().pix_fmt)
    }

    pub(super) fn name(&self) -> &'static str {
        // SAFETY: an open context names its codec
        static_text(unsafe { (*self.raw().codec).name })
    }

    /// Encodes a picture at the encoder's size and format. `keyframe` forces an IDR. `out`
    /// gets each access unit in Annex B and whether it is a keyframe. False when it fails.
    pub(super) fn encode(&mut self, picture: &mut Frame, time_us: u64, keyframe: bool, mut out: impl FnMut(&[u8], bool)) -> bool {
        let mut pts = (time_us as i64).saturating_add(self.shift);
        if pts <= self.last_pts {
            let rising = self.last_pts.saturating_add(PTS_STEP);
            self.shift = self.shift.saturating_add(rising - pts);
            pts = rising;
        }
        self.last_pts = pts;
        // SAFETY: the picture is ours to stamp, and each packet is ours between receive and unref
        unsafe {
            let p = &mut *picture.as_mut_ptr();
            p.pts = pts;
            p.pict_type = if keyframe { ffi::AV_PICTURE_TYPE_I } else { ffi::AV_PICTURE_TYPE_NONE };
            p.quality = self.quality;
            if ffi::avcodec_send_frame(self.ctx, p) < 0 {
                return false;
            }
            loop {
                let r = ffi::avcodec_receive_packet(self.ctx, self.packet.as_ptr());
                if r < 0 {
                    return Error(r) == Error::AGAIN || Error(r) == Error::EOF;
                }
                let key = self.packet.raw().flags & ffi::AV_PKT_FLAG_KEY as c_int != 0;
                match &mut self.params {
                    Some(params) => out(&params.complete(self.packet.data(), key), key),
                    None => out(self.packet.data(), key),
                }
                self.packet.unref();
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // FFmpeg's JPEG encoder touches no device, so it never waits on one that does
        let _one = (!self.is_jpeg()).then(|| OPENING.lock().unwrap_or_else(|e| e.into_inner()));
        // SAFETY: ours alone
        unsafe { ffi::avcodec_free_context(&mut self.ctx) };
    }
}

/// About 0.08 bits a pixel.
fn bitrate_for(width: u32, height: u32, fps: f64) -> i64 {
    ((width as f64 * height as f64 * fps.max(1.0) * 0.08) as i64).clamp(200_000, 40_000_000)
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
