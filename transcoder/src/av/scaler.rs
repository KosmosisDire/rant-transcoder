use std::os::raw::c_int;

use super::{Frame, PixelFormat};
use crate::ffi;

/// Converts pictures to one size and format in software.
#[derive(Default)]
pub struct Scaler {
    sws: Option<std::ptr::NonNull<ffi::SwsContext>>,
    out: Option<Frame>,
}

// SAFETY: the context belongs to this scaler alone
unsafe impl Send for Scaler {}

impl Scaler {
    /// `src` at `width` by `height` in `format`. The result lives until the next convert.
    pub fn convert(&mut self, src: &Frame, width: u32, height: u32, format: PixelFormat) -> Option<&mut Frame> {
        let fits = self.out.as_ref().is_some_and(|o| o.width() == width && o.height() == height && o.format() == format);
        // SAFETY: the output frame is ours, the source is only read, and a cached context
        // matches both or is made anew
        unsafe {
            if !fits {
                let mut out = Frame::new();
                let f = &mut *out.as_mut_ptr();
                f.width = width as c_int;
                f.height = height as c_int;
                f.format = format.0;
                if ffi::av_frame_get_buffer(f, 0) < 0 {
                    self.out = None;
                    return None;
                }
                self.out = Some(out);
            }
            let out = self.out.as_mut()?;
            // an encoder may still hold the last picture
            if ffi::av_frame_make_writable(out.as_mut_ptr()) < 0 {
                return None;
            }
            let s = &*src.as_ptr();
            let sws = ffi::sws_getCachedContext(
                self.sws.map_or(std::ptr::null_mut(), |p| p.as_ptr()),
                s.width,
                s.height,
                s.format,
                width as c_int,
                height as c_int,
                format.0,
                ffi::SWS_BILINEAR as c_int,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            self.sws = std::ptr::NonNull::new(sws);
            if sws.is_null() {
                return None;
            }
            let o = &mut *out.as_mut_ptr();
            ffi::sws_scale(sws, s.data.as_ptr() as *const *const u8, s.linesize.as_ptr(), 0, s.height, o.data.as_ptr(), o.linesize.as_ptr());
            Some(out)
        }
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        if let Some(sws) = self.sws {
            // SAFETY: ours alone
            unsafe { ffi::sws_freeContext(sws.as_ptr()) };
        }
    }
}
