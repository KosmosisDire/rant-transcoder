//! The safe layer over FFmpeg. Nothing outside this module touches `ffi`.

mod frame;

pub use frame::{Frame, Layout, Wrapped};

use std::ffi::CStr;
use std::fmt;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loaded_ffmpeg_is_the_pinned_one() {
        assert_eq!(check(), Ok(()));
        assert!(version().starts_with("n8.1"), "{}", version());
        assert_eq!(PixelFormat::NV12.name(), "nv12");
        assert_eq!(PixelFormat::NONE.name(), "none");
    }
}
