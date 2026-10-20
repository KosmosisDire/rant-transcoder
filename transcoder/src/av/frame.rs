use std::marker::PhantomData;
use std::ops::Deref;
use std::os::raw::c_int;
use std::ptr::NonNull;

use super::PixelFormat;
use crate::ffi;

/// A picture: FFmpeg's AVFrame, freed on drop.
pub struct Frame {
    ptr: NonNull<ffi::AVFrame>,
}

// SAFETY: a frame belongs to whoever holds it, FFmpeg keeps no hidden thread ties
unsafe impl Send for Frame {}

impl Frame {
    pub fn new() -> Frame {
        // SAFETY: a plain allocation, null only when memory runs out
        let ptr = NonNull::new(unsafe { ffi::av_frame_alloc() }).expect("FFmpeg could not allocate a frame");
        Frame { ptr }
    }

    pub fn width(&self) -> u32 {
        self.raw().width as u32
    }

    pub fn height(&self) -> u32 {
        self.raw().height as u32
    }

    pub fn format(&self) -> PixelFormat {
        PixelFormat(self.raw().format)
    }

    fn raw(&self) -> &ffi::AVFrame {
        // SAFETY: the frame lives as long as self
        unsafe { self.ptr.as_ref() }
    }

    pub(super) fn as_ptr(&self) -> *const ffi::AVFrame {
        self.ptr.as_ptr()
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.ptr.as_ptr()
    }

    /// Drops the buffers the frame holds and resets its fields.
    pub(super) fn unref(&mut self) {
        // SAFETY: the frame is ours, and a borrowed picture is never in it here since Wrapped holds it mutably
        unsafe { ffi::av_frame_unref(self.ptr.as_ptr()) };
    }

    /// Points the frame at raw pixels without copying them, for as long as the borrow lasts.
    /// None when the layout is not one FFmpeg reads or `data` is too short to hold it.
    pub fn wrap<'a>(&'a mut self, data: &'a [u8], layout: Layout) -> Option<Wrapped<'a>> {
        let Layout { width, height, stride, format } = layout;
        let (w, h) = (c_int::try_from(width).ok()?, height as usize);
        if w == 0 || h == 0 {
            return None;
        }
        // NV12's chroma plane follows its luma plane, rows as far apart, and an odd width makes its rows a byte longer
        let planes = if format == PixelFormat::NV12 { 2 } else { 1 };
        let mut line = 0;
        for plane in 0..planes {
            // SAFETY: a pure computation, negative for a format it does not know
            let l = unsafe { ffi::av_image_get_linesize(format.0, w, plane) };
            if l <= 0 {
                return None;
            }
            line = line.max(l as usize);
        }
        let stride = if stride == 0 { line } else { stride as usize };
        let rows = if planes == 2 { h + h.div_ceil(2) } else { h };
        // the last row needs its pixels, not its padding
        if stride < line || c_int::try_from(stride).is_err() || data.len() < stride * (rows - 1) + line {
            return None;
        }
        let pixels = data.as_ptr() as *mut u8;
        // SAFETY: the pixels outlive the Wrapped that clears these pointers, and FFmpeg only reads
        // them, since buf[0] stays null and so marks the frame as not its own
        unsafe {
            let f = self.ptr.as_mut();
            f.data = [std::ptr::null_mut(); 8];
            f.linesize = [0; 8];
            f.data[0] = pixels;
            f.linesize[0] = stride as c_int;
            if planes == 2 {
                f.data[1] = pixels.add(stride * h);
                f.linesize[1] = stride as c_int;
            }
            f.extended_data = f.data.as_mut_ptr();
            f.width = w;
            f.height = h as c_int;
            f.format = format.0;
        }
        Some(Wrapped { frame: self, _pixels: PhantomData })
    }
}

impl Default for Frame {
    fn default() -> Frame {
        Frame::new()
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let mut p = self.ptr.as_ptr();
        // SAFETY: the frame is ours alone
        unsafe { ffi::av_frame_free(&mut p) };
    }
}

/// Raw pixels as a Rant Image lays them out: rows from the top, `stride` bytes apart or packed
/// when 0, and NV12's chroma plane straight after its luma plane.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}

/// A frame pointing at borrowed pixels, which it lets go on drop.
pub struct Wrapped<'a> {
    frame: &'a mut Frame,
    _pixels: PhantomData<&'a [u8]>,
}

impl Deref for Wrapped<'_> {
    type Target = Frame;

    fn deref(&self) -> &Frame {
        self.frame
    }
}

impl Drop for Wrapped<'_> {
    fn drop(&mut self) {
        // SAFETY: the frame owns no buffers, so this only resets its fields
        unsafe { ffi::av_frame_unref(self.frame.ptr.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: u32, height: u32, stride: u32, format: PixelFormat) -> Layout {
        Layout { width, height, stride, format }
    }

    #[test]
    fn wraps_padded_rows() {
        let mut f = Frame::new();
        let bgr = vec![0u8; 64 * 3 + 16 + 64 * 3];
        let w = f.wrap(&bgr, layout(64, 2, 64 * 3 + 16, PixelFormat::BGR24)).unwrap();
        assert_eq!((w.width(), w.height(), w.format()), (64, 2, PixelFormat::BGR24));
        assert_eq!(w.raw().linesize[0], 64 * 3 + 16);
        drop(w);
        assert!(f.wrap(&bgr[1..], layout(64, 2, 64 * 3 + 16, PixelFormat::BGR24)).is_none());
        assert!(f.wrap(&bgr, layout(64, 2, 64 * 3 - 1, PixelFormat::BGR24)).is_none());
        assert!(f.wrap(&bgr, layout(0, 2, 0, PixelFormat::BGR24)).is_none());
        assert!(f.wrap(&bgr, layout(64, 2, 0, PixelFormat::NONE)).is_none());
    }

    #[test]
    fn wraps_both_nv12_planes() {
        let mut f = Frame::new();
        let nv12 = vec![0u8; 16 * 16 * 3 / 2];
        let w = f.wrap(&nv12, layout(16, 16, 0, PixelFormat::NV12)).unwrap();
        assert_eq!(w.raw().data[1] as *const u8, nv12[256..].as_ptr());
        assert_eq!(w.raw().linesize[1], 16);
        drop(w);
        // an odd width's chroma rows are a byte longer than its luma rows
        let odd = vec![0u8; 15 * 16 * 3 / 2];
        assert!(f.wrap(&odd, layout(15, 16, 15, PixelFormat::NV12)).is_none());
    }

    #[test]
    fn letting_go_clears_the_pointers() {
        let mut f = Frame::new();
        let gray = vec![0u8; 8 * 8];
        drop(f.wrap(&gray, layout(8, 8, 0, PixelFormat::GRAY8)).unwrap());
        assert!(f.raw().data[0].is_null());
        assert_eq!(f.width(), 0);
    }
}
