use std::ffi::c_void;
use std::os::raw::c_int;

use super::hw::{self, Device};
use super::{CodecId, CodecParameters, Error, Frame, Packet, Dict, static_text};
use crate::ffi;

/// Decodes one codec, on a hardware device when one takes it, whose pictures stay there.
pub struct Decoder {
    ctx: *mut ffi::AVCodecContext,
    codec: CodecId,
    params: Option<CodecParameters>,
    packet: Packet,
    frame: Frame,
    /// The device's picture format, NONE in software.
    hw: ffi::AVPixelFormat,
    errors: u32,
}

// SAFETY: the context belongs to this decoder alone
unsafe impl Send for Decoder {}

impl Decoder {
    /// `params` carries a demuxed stream's extradata, None for a Rant topic. The device the
    /// `encoder` takes pictures from in place comes first. None when no decoder opens.
    pub fn open(codec: CodecId, params: Option<&CodecParameters>, encoder: Option<&str>) -> Option<Decoder> {
        let mut d = Decoder::closed(codec, params);
        // SAFETY: a lookup in FFmpeg's static tables
        let native = unsafe { ffi::avcodec_find_decoder(codec.0) };
        let first = encoder.and_then(super::output::device_for);
        if !native.is_null() {
            for kind in first.into_iter().chain(hw::DECODE.iter().copied().filter(|&k| Some(k) != first)) {
                let format = device_format(native, kind.kind);
                if format == ffi::AV_PIX_FMT_NONE {
                    continue;
                }
                if let Some(device) = hw::device(kind)
                    && d.open_with(native, Some((&device, format)))
                {
                    return Some(d);
                }
            }
        }
        d.open_software().then_some(d)
    }

    /// The same in software only.
    pub fn software(codec: CodecId, params: Option<&CodecParameters>) -> Option<Decoder> {
        let mut d = Decoder::closed(codec, params);
        d.open_software().then_some(d)
    }

    fn closed(codec: CodecId, params: Option<&CodecParameters>) -> Decoder {
        Decoder {
            ctx: std::ptr::null_mut(),
            codec,
            params: params.cloned(),
            packet: Packet::new(),
            frame: Frame::new(),
            hw: ffi::AV_PIX_FMT_NONE,
            errors: 0,
        }
    }

    fn open_software(&mut self) -> bool {
        // FFmpeg's own AV1 decoder needs a device, dav1d decodes in software
        // SAFETY: lookups in FFmpeg's static tables
        let codec = unsafe {
            if self.codec == CodecId::AV1 { ffi::avcodec_find_decoder_by_name(c"libdav1d".as_ptr()) } else { ffi::avcodec_find_decoder(self.codec.0) }
        };
        !codec.is_null() && self.open_with(codec, None)
    }

    fn open_with(&mut self, codec: *const ffi::AVCodec, device: Option<(&Device, ffi::AVPixelFormat)>) -> bool {
        // SAFETY: a fresh context set up before its open, freed on any failure
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            self.ctx = ffi::avcodec_alloc_context3(codec);
            let c = &mut *self.ctx;
            if let Some(p) = &self.params {
                ffi::avcodec_parameters_to_context(c, p.ptr.as_ptr());
            }
            // frame threads hold pictures back a frame each, slice threads add no delay
            c.thread_type = ffi::FF_THREAD_SLICE as c_int;
            c.flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
            self.hw = ffi::AV_PIX_FMT_NONE;
            if let Some((device, format)) = device {
                c.hw_device_ctx = ffi::av_buffer_ref(device.as_ptr());
                // the format itself rides in opaque, so the callback needs no pointer back to us
                c.opaque = format as isize as *mut c_void;
                c.get_format = Some(pick_format);
                // a device pool can be fixed, and each child holds a picture or two from it
                c.extra_hw_frames = 8;
                self.hw = format;
            }
            // dav1d's own low latency switch
            let mut options = Dict::of(&[("max_frame_delay", "1")]);
            if ffi::avcodec_open2(self.ctx, codec, &mut options.0) < 0 {
                ffi::avcodec_free_context(&mut self.ctx);
                self.hw = ffi::AV_PIX_FMT_NONE;
                return false;
            }
        }
        self.errors = 0;
        true
    }

    /// False once the decoder has given up, after which it must be opened again.
    pub fn is_open(&self) -> bool {
        !self.ctx.is_null()
    }

    /// Feeds one access unit. `out` runs for each picture it completes, which may be on a
    /// device. False on a decode error, so the caller waits for the next keyframe.
    pub fn decode(&mut self, data: &[u8], mut out: impl FnMut(&Frame)) -> bool {
        if self.ctx.is_null() || data.is_empty() {
            return false;
        }
        // SAFETY: the packet points at data only for the send, which copies what it is not
        // given a reference to, and each frame is ours between receive and unref
        unsafe {
            let p = &mut *self.packet.as_ptr();
            p.data = data.as_ptr() as *mut u8;
            p.size = data.len() as c_int;
            let r = ffi::avcodec_send_packet(self.ctx, p);
            p.data = std::ptr::null_mut();
            p.size = 0;
            if r < 0 && Error(r) != Error::AGAIN {
                return self.fail();
            }
            loop {
                let r = ffi::avcodec_receive_frame(self.ctx, self.frame.as_mut_ptr());
                if Error(r) == Error::AGAIN || Error(r) == Error::EOF {
                    break;
                }
                if r < 0 {
                    return self.fail();
                }
                self.errors = 0;
                out(&self.frame);
                self.frame.unref();
            }
        }
        true
    }

    fn fail(&mut self) -> bool {
        // a device that keeps failing, such as on a profile it lacks, gives way to software
        if self.hw != ffi::AV_PIX_FMT_NONE {
            self.errors += 1;
            if self.errors >= 30 {
                tracing::info!("decoder {} keeps failing, switching to software", self.describe());
                // SAFETY: an open context's device reference is null or valid
                if let Some(device) = hw::type_of(unsafe { (*self.ctx).hw_device_ctx }) {
                    hw::forget(device);
                }
                if !self.open_software() {
                    // SAFETY: ours alone
                    unsafe { ffi::avcodec_free_context(&mut self.ctx) };
                }
            }
        }
        false
    }

    /// The decoder's name, and the device when in hardware.
    pub fn describe(&self) -> String {
        if self.ctx.is_null() {
            return "closed".into();
        }
        // SAFETY: an open context names its codec
        let name = static_text(unsafe { (*(*self.ctx).codec).name });
        // SAFETY: an open context's device reference is null or valid
        match hw::type_of(unsafe { (*self.ctx).hw_device_ctx }) {
            Some(device) if self.hw != ffi::AV_PIX_FMT_NONE => format!("{name} on {}", hw::name(device)),
            _ => name.into(),
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: ours alone, null is fine
        unsafe { ffi::avcodec_free_context(&mut self.ctx) };
    }
}

/// The picture format a codec decodes to on a device of this type, NONE when it cannot.
fn device_format(codec: *const ffi::AVCodec, kind: ffi::AVHWDeviceType) -> ffi::AVPixelFormat {
    for i in 0.. {
        // SAFETY: FFmpeg's static table, null past its end
        let Some(c) = (unsafe { ffi::avcodec_get_hw_config(codec, i).as_ref() }) else { break };
        if c.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0 && c.device_type == kind {
            return c.pix_fmt;
        }
    }
    ffi::AV_PIX_FMT_NONE
}

/// The device's format when offered, else the first in system memory.
unsafe extern "C" fn pick_format(ctx: *mut ffi::AVCodecContext, list: *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg hands its context and a list ended by NONE
    unsafe {
        let wanted = (*ctx).opaque as isize as ffi::AVPixelFormat;
        let mut offered = (0..).map(|i| *list.add(i)).take_while(|&f| f != ffi::AV_PIX_FMT_NONE);
        if let Some(f) = offered.clone().find(|&f| f == wanted) {
            return f;
        }
        offered
            .find(|&f| ffi::av_pix_fmt_desc_get(f).as_ref().is_some_and(|d| d.flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 == 0))
            .unwrap_or(ffi::AV_PIX_FMT_NONE)
    }
}
