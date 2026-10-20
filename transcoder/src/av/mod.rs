//! The safe layer over FFmpeg. Nothing outside this module touches `ffi`.

mod color;
mod decoder;
mod encoder;
mod frame;
mod hw;
mod input;
mod scaler;

pub use color::Colors;
pub use decoder::Decoder;
pub use encoder::Encoder;
pub use frame::{Frame, Layout, Pool, Wrapped};
pub use input::{Input, InputPacket};
pub use scaler::Scaler;

use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::c_int;
use std::ptr::NonNull;

use crate::ffi;

/// FFmpeg's own version, such as "n8.1.3-9-g29e619e767-20260930".
pub fn version() -> String {
    // SAFETY: a static string FFmpeg keeps for the life of the process
    unsafe { CStr::from_ptr(ffi::av_version_info()) }.to_string_lossy().into_owned()
}

/// Fails when a loaded FFmpeg library is not the one the transcoder was built against, as
/// when another FFmpeg comes first on the library path.
pub fn check() -> Result<(), String> {
    // SAFETY: each only reads a constant of its library
    let loaded = unsafe {
        [
            ("avutil", ffi::avutil_version(), ffi::LIBAVUTIL_VERSION_MAJOR),
            ("avcodec", ffi::avcodec_version(), ffi::LIBAVCODEC_VERSION_MAJOR),
            ("avformat", ffi::avformat_version(), ffi::LIBAVFORMAT_VERSION_MAJOR),
            ("avfilter", ffi::avfilter_version(), ffi::LIBAVFILTER_VERSION_MAJOR),
            ("swscale", ffi::swscale_version(), ffi::LIBSWSCALE_VERSION_MAJOR),
        ]
    };
    for (name, version, built) in loaded {
        if version >> 16 != built {
            return Err(format!("{name} {} is loaded, but rant-transcoder was built for {built}", version >> 16));
        }
    }
    Ok(())
}

/// What FFmpeg prints to stderr: warnings when `verbose`, else only fatal errors, since the
/// encoders it tries and skips would fill the log.
pub fn set_log_level(verbose: bool) {
    let level = if verbose { ffi::AV_LOG_WARNING } else { ffi::AV_LOG_FATAL };
    // SAFETY: sets a global FFmpeg reads atomically
    unsafe { ffi::av_log_set_level(level as c_int) };
}

/// An FFmpeg error code, shown as FFmpeg's words for it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Error(c_int);

impl Error {
    // AVERROR(EAGAIN) and AVERROR_EOF are macros bindgen cannot read
    const AGAIN: Error = Error(-if cfg!(target_os = "macos") { 35 } else { 11 });
    const EOF: Error = Error(-tag(b"EOF "));
}

/// AV_NOPTS_VALUE, a cast bindgen cannot read either.
const NO_PTS: i64 = i64::MIN;

const fn tag(t: &[u8; 4]) -> c_int {
    (t[0] as u32 | (t[1] as u32) << 8 | (t[2] as u32) << 16 | (t[3] as u32) << 24) as c_int
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; ffi::AV_ERROR_MAX_STRING_SIZE as usize];
        // SAFETY: the buffer is as long as said and FFmpeg ends the text with a nul
        unsafe { ffi::av_strerror(self.0, buf.as_mut_ptr().cast(), buf.len()) };
        let text = CStr::from_bytes_until_nul(&buf).map(CStr::to_string_lossy).unwrap_or_default();
        f.write_str(&text)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self} ({})", self.0)
    }
}

impl std::error::Error for Error {}

/// A picture's pixel layout.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat(ffi::AVPixelFormat);

impl PixelFormat {
    pub const NONE: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_NONE);
    pub const GRAY8: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_GRAY8);
    pub const GRAY16LE: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_GRAY16LE);
    pub const GRAYF32LE: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_GRAYF32LE);
    pub const RGB24: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_RGB24);
    pub const RGBA: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_RGBA);
    pub const BGR24: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_BGR24);
    pub const YUYV422: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_YUYV422);
    pub const NV12: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_NV12);
    pub const YUV420P: PixelFormat = PixelFormat(ffi::AV_PIX_FMT_YUV420P);

    pub fn name(self) -> &'static str {
        // SAFETY: null for an unknown format, else a static string
        let name = unsafe { ffi::av_get_pix_fmt_name(self.0) };
        if name.is_null() { "none" } else { unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("none") }
    }
}

impl fmt::Debug for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A bitstream format.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CodecId(ffi::AVCodecID);

impl CodecId {
    pub const NONE: CodecId = CodecId(ffi::AV_CODEC_ID_NONE);
    pub const MJPEG: CodecId = CodecId(ffi::AV_CODEC_ID_MJPEG);
    pub const PNG: CodecId = CodecId(ffi::AV_CODEC_ID_PNG);
    pub const H264: CodecId = CodecId(ffi::AV_CODEC_ID_H264);
    pub const HEVC: CodecId = CodecId(ffi::AV_CODEC_ID_HEVC);
    pub const AV1: CodecId = CodecId(ffi::AV_CODEC_ID_AV1);

    pub fn name(self) -> &'static str {
        // SAFETY: a static string, "unknown_codec" for an id it does not know
        unsafe { CStr::from_ptr(ffi::avcodec_get_name(self.0)) }.to_str().unwrap_or("unknown")
    }
}

impl fmt::Debug for CodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a demuxer says about its stream, which its decoder needs: the extradata above all.
pub struct CodecParameters {
    ptr: NonNull<ffi::AVCodecParameters>,
}

// SAFETY: plain data FFmpeg never shares
unsafe impl Send for CodecParameters {}

impl CodecParameters {
    /// A copy of FFmpeg's, which stays its own.
    fn copy_of(from: *const ffi::AVCodecParameters) -> CodecParameters {
        // SAFETY: a fresh allocation filled from a valid one
        unsafe {
            let ptr = NonNull::new(ffi::avcodec_parameters_alloc()).expect("FFmpeg could not allocate codec parameters");
            ffi::avcodec_parameters_copy(ptr.as_ptr(), from);
            CodecParameters { ptr }
        }
    }

    pub fn codec(&self) -> CodecId {
        CodecId(self.raw().codec_id)
    }

    pub fn width(&self) -> u32 {
        self.raw().width.max(0) as u32
    }

    pub fn height(&self) -> u32 {
        self.raw().height.max(0) as u32
    }

    fn raw(&self) -> &ffi::AVCodecParameters {
        // SAFETY: ours until drop
        unsafe { self.ptr.as_ref() }
    }
}

impl Clone for CodecParameters {
    fn clone(&self) -> CodecParameters {
        CodecParameters::copy_of(self.ptr.as_ptr())
    }
}

impl Drop for CodecParameters {
    fn drop(&mut self) {
        let mut p = self.ptr.as_ptr();
        // SAFETY: ours alone
        unsafe { ffi::avcodec_parameters_free(&mut p) };
    }
}

/// An AVPacket, freed on drop.
struct Packet {
    ptr: NonNull<ffi::AVPacket>,
}

impl Packet {
    fn new() -> Packet {
        // SAFETY: a plain allocation, null only when memory runs out
        Packet { ptr: NonNull::new(unsafe { ffi::av_packet_alloc() }).expect("FFmpeg could not allocate a packet") }
    }

    fn as_ptr(&self) -> *mut ffi::AVPacket {
        self.ptr.as_ptr()
    }

    fn raw(&self) -> &ffi::AVPacket {
        // SAFETY: ours until drop
        unsafe { self.ptr.as_ref() }
    }

    /// The bytes it holds now.
    fn data(&self) -> &[u8] {
        let p = self.raw();
        if p.data.is_null() || p.size <= 0 { &[] } else { unsafe { std::slice::from_raw_parts(p.data, p.size as usize) } }
    }

    fn unref(&mut self) {
        // SAFETY: drops what the packet held and resets it
        unsafe { ffi::av_packet_unref(self.as_ptr()) };
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        let mut p = self.ptr.as_ptr();
        // SAFETY: ours alone
        unsafe { ffi::av_packet_free(&mut p) };
    }
}

/// An AVDictionary of options, freed on drop. FFmpeg takes out the ones it used.
struct Dict(*mut ffi::AVDictionary);

impl Dict {
    fn of(pairs: &[(&str, &str)]) -> Dict {
        let mut d = Dict(std::ptr::null_mut());
        for (k, v) in pairs {
            let (k, v) = (CString::new(*k).unwrap(), CString::new(*v).unwrap());
            // SAFETY: FFmpeg copies both strings
            unsafe { ffi::av_dict_set(&mut d.0, k.as_ptr(), v.as_ptr(), 0) };
        }
        d
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        // SAFETY: ours alone, null is fine
        unsafe { ffi::av_dict_free(&mut self.0) };
    }
}

/// A name FFmpeg keeps for the life of the process, such as a codec's, "" when null.
fn static_text(p: *const std::os::raw::c_char) -> &'static str {
    if p.is_null() { "" } else { unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loaded_ffmpeg_is_the_pinned_one() {
        assert_eq!(check(), Ok(()));
        assert!(version().starts_with("n8.1"), "{}", version());
        assert_eq!(PixelFormat::NV12.name(), "nv12");
        assert_eq!(PixelFormat::NONE.name(), "none");
        assert_eq!(CodecId::HEVC.name(), "hevc");
    }

    #[test]
    fn errors_read_as_words() {
        assert_eq!(Error::EOF.to_string(), "End of file");
        assert_eq!(Error::AGAIN.to_string(), "Resource temporarily unavailable");
    }
}
