//! The hardware devices decoders run on, one per type for the whole process.

use std::ptr::NonNull;
use std::sync::Mutex;

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

#[cfg(windows)]
const PLATFORM: ffi::AVHWDeviceType = ffi::AV_HWDEVICE_TYPE_D3D11VA;
#[cfg(target_os = "macos")]
const PLATFORM: ffi::AVHWDeviceType = ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
#[cfg(not(any(windows, target_os = "macos")))]
const PLATFORM: ffi::AVHWDeviceType = ffi::AV_HWDEVICE_TYPE_VAAPI;

/// The device types tried for decoding, best first.
pub(super) const DECODE: [ffi::AVHWDeviceType; 2] = [ffi::AV_HWDEVICE_TYPE_CUDA, PLATFORM];

/// The device of a type, made on first ask. None when this machine has none.
pub(super) fn device(kind: ffi::AVHWDeviceType) -> Option<Device> {
    static MADE: Mutex<Vec<(ffi::AVHWDeviceType, Option<Device>)>> = Mutex::new(Vec::new());
    if kind == ffi::AV_HWDEVICE_TYPE_VAAPI && !have_vaapi() {
        return None;
    }
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, d)) = made.iter().find(|(k, _)| *k == kind) {
        return *d;
    }
    let mut p = std::ptr::null_mut();
    // SAFETY: on failure FFmpeg leaves p null
    let ok = unsafe { ffi::av_hwdevice_ctx_create(&mut p, kind, std::ptr::null(), std::ptr::null_mut(), 0) } >= 0;
    let d = if ok { NonNull::new(p).map(Device) } else { None };
    made.push((kind, d));
    d
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
