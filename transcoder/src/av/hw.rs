//! The hardware devices pictures are decoded and converted on, one per kind for the whole
//! process.

use std::os::raw::c_int;
use std::ptr::NonNull;
use std::sync::Mutex;

use super::{Dict, Frame, static_text};
use crate::ffi;

/// A device context FFmpeg made, kept until the process ends.
#[derive(Clone, Copy)]
pub(super) struct Device(NonNull<ffi::AVBufferRef>);

// SAFETY: FFmpeg's device contexts take references from any thread
unsafe impl Send for Device {}

impl Device {
    pub(super) fn as_ptr(self) -> *mut ffi::AVBufferRef {
        self.0.as_ptr()
    }
}

/// A device type, on an adapter of one PCI vendor or on the system's default (vendor 0).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Kind {
    pub(super) kind: ffi::AVHWDeviceType,
    vendor: u32,
}

impl Kind {
    pub(super) const CUDA: Kind = Kind { kind: ffi::AV_HWDEVICE_TYPE_CUDA, vendor: 0 };
    /// Intel's, whose pictures Quick Sync encodes in place.
    #[cfg(windows)]
    pub(super) const D3D11_INTEL: Kind = Kind { kind: ffi::AV_HWDEVICE_TYPE_D3D11VA, vendor: 0x8086 };
    #[cfg(windows)]
    const D3D11: Kind = Kind { kind: ffi::AV_HWDEVICE_TYPE_D3D11VA, vendor: 0 };
    #[cfg(target_os = "macos")]
    const VIDEOTOOLBOX: Kind = Kind { kind: ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, vendor: 0 };
    #[cfg(not(any(windows, target_os = "macos")))]
    pub(super) const VAAPI: Kind = Kind { kind: ffi::AV_HWDEVICE_TYPE_VAAPI, vendor: 0 };
}

/// The devices tried for decoding, best first: those an encoder takes pictures from in place,
/// then the platform's own.
#[cfg(windows)]
pub(super) const DECODE: &[Kind] = &[Kind::CUDA, Kind::D3D11_INTEL, Kind::D3D11];
#[cfg(target_os = "macos")]
pub(super) const DECODE: &[Kind] = &[Kind::VIDEOTOOLBOX];
#[cfg(not(any(windows, target_os = "macos")))]
pub(super) const DECODE: &[Kind] = &[Kind::CUDA, Kind::VAAPI];

/// The device of a kind, made on first ask. None when this machine has none.
pub(super) fn device(kind: Kind) -> Option<Device> {
    static MADE: Mutex<Vec<(Kind, Option<Device>)>> = Mutex::new(Vec::new());
    if kind.kind == ffi::AV_HWDEVICE_TYPE_VAAPI && !have_vaapi() {
        return None;
    }
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, d)) = made.iter().find(|(k, _)| *k == kind) {
        return *d;
    }
    let vendor = format!("{:#x}", kind.vendor);
    let options = if kind.vendor == 0 { Dict::of(&[]) } else { Dict::of(&[("vendor_id", &vendor)]) };
    let mut p = std::ptr::null_mut();
    // SAFETY: on failure FFmpeg leaves p null, and it only reads the options
    let ok = unsafe { ffi::av_hwdevice_ctx_create(&mut p, kind.kind, std::ptr::null(), options.0, 0) } >= 0;
    let d = if ok { NonNull::new(p).map(Device) } else { None };
    made.push((kind, d));
    d
}

/// A pool of pictures on a device, which an encoder reads.
pub(super) struct Frames(NonNull<ffi::AVBufferRef>);

// SAFETY: FFmpeg's pools lock themselves
unsafe impl Send for Frames {}

impl Frames {
    /// A pool of NV12 pictures of a size on a device, for pictures uploaded from system memory.
    /// None when the device makes none.
    pub(super) fn new(device: Device, width: u32, height: u32) -> Option<Frames> {
        // SAFETY: the pool is set up before its init and freed when the init fails, and the
        // constraints are freed once read
        unsafe {
            let mut constraints = ffi::av_hwdevice_get_hwframe_constraints(device.as_ptr(), std::ptr::null());
            let format = constraints.as_ref().and_then(|c| c.valid_hw_formats.as_ref()).map_or(ffi::AV_PIX_FMT_NONE, |f| *f);
            ffi::av_hwframe_constraints_free(&mut constraints);
            let mut pool = ffi::av_hwframe_ctx_alloc(device.as_ptr());
            if pool.is_null() || format == ffi::AV_PIX_FMT_NONE {
                ffi::av_buffer_unref(&mut pool);
                return None;
            }
            let c = &mut *((*pool).data as *mut ffi::AVHWFramesContext);
            (c.format, c.sw_format, c.width, c.height) = (format, ffi::AV_PIX_FMT_NV12, width as c_int, height as c_int);
            if ffi::av_hwframe_ctx_init(pool) < 0 {
                ffi::av_buffer_unref(&mut pool);
                return None;
            }
            NonNull::new(pool).map(Frames)
        }
    }

    /// Another reference to a pool FFmpeg made, such as a filter's, None for null.
    pub(super) fn of(pool: *mut ffi::AVBufferRef) -> Option<Frames> {
        // SAFETY: a new reference to a valid pool, or null for null
        NonNull::new(unsafe { ffi::av_buffer_ref(pool) }).map(Frames)
    }

    pub(super) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.0.as_ptr()
    }

    /// A picture of the pool holding `src`, which is in system memory at the pool's size and format.
    pub(super) fn upload(&self, src: &Frame) -> Option<Frame> {
        let mut f = Frame::new();
        // SAFETY: a fresh frame filled from the pool, then from a valid source
        let ok = unsafe {
            ffi::av_hwframe_get_buffer(self.as_ptr(), f.as_mut_ptr(), 0) >= 0
                && ffi::av_hwframe_transfer_data(f.as_mut_ptr(), src.as_ptr(), 0) >= 0
                && ffi::av_frame_copy_props(f.as_mut_ptr(), src.as_ptr()) >= 0
        };
        ok.then_some(f)
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        let mut p = self.0.as_ptr();
        // SAFETY: our reference alone
        unsafe { ffi::av_buffer_unref(&mut p) };
    }
}

/// The type of device a picture is on, None for system memory.
pub(super) fn device_of(frame: &Frame) -> Option<ffi::AVHWDeviceType> {
    let frames = frame.raw().hw_frames_ctx;
    // SAFETY: a hardware frame's pool reference holds the pool's context, which holds its device's
    unsafe { frames.as_ref().and_then(|f| type_of((*(f.data as *const ffi::AVHWFramesContext)).device_ref)) }
}

/// The type of a device a context reference holds, None for null.
pub(super) fn type_of(device: *const ffi::AVBufferRef) -> Option<ffi::AVHWDeviceType> {
    // SAFETY: a device reference's data is its device context
    unsafe { device.as_ref().map(|d| (*(d.data as *const ffi::AVHWDeviceContext)).type_) }
}

/// FFmpeg's name for a device type, such as cuda.
pub(super) fn name(kind: ffi::AVHWDeviceType) -> &'static str {
    // SAFETY: a static name, null for an unknown type
    static_text(unsafe { ffi::av_hwdevice_get_type_name(kind) })
}

/// VAAPI, which Quick Sync also runs on under Linux, is usable. The FFmpeg build binds libva
/// lazily and aborts the process when a call finds it missing, so look before asking.
pub(super) fn have_vaapi() -> bool {
    #[cfg(target_os = "linux")]
    {
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // SAFETY: loading libva runs only its own initialisers
        *OK.get_or_init(|| ["libva.so.2", "libva-drm.so.2"].into_iter().all(|lib| unsafe { libloading::Library::new(lib) }.is_ok()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}
