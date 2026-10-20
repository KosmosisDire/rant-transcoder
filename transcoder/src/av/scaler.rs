use std::os::raw::c_int;
use std::ptr::NonNull;

use super::{Colors, Frame, PixelFormat};
use crate::ffi;

/// Converts pictures to one size and format in software. The source's range and matrix come
/// from its frame, the output's from the Colors asked for.
#[derive(Default)]
pub struct Scaler {
    sws: Option<NonNull<ffi::SwsContext>>,
    out: Option<Frame>,
}

// SAFETY: the context belongs to this scaler alone
unsafe impl Send for Scaler {}

impl Scaler {
    /// `src` at `width` by `height` in `format`. The result lives until the next convert.
    pub fn convert(&mut self, src: &Frame, width: u32, height: u32, format: PixelFormat, colors: Colors) -> Option<&mut Frame> {
        let fits = self.out.as_ref().is_some_and(|o| o.width() == width && o.height() == height && o.format() == format);
        // SAFETY: the output frame is ours, the source is only read, and the context adapts
        // to whatever frames it is given
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
            if self.sws.is_none() {
                let sws = NonNull::new(ffi::sws_alloc_context())?;
                (*sws.as_ptr()).flags = ffi::SWS_BILINEAR;
                self.sws = Some(sws);
            }
            let out = self.out.as_mut()?;
            // an encoder may still hold the last picture
            if ffi::av_frame_make_writable(out.as_mut_ptr()) < 0 {
                return None;
            }
            colors.stamp(&mut *out.as_mut_ptr());
            if ffi::sws_scale_frame(self.sws?.as_ptr(), out.as_mut_ptr(), src.as_ptr()) < 0 {
                return None;
            }
            Some(out)
        }
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        if let Some(sws) = self.sws {
            let mut p = sws.as_ptr();
            // SAFETY: ours alone
            unsafe { ffi::sws_free_context(&mut p) };
        }
    }
}
