use std::cell::Cell;
use std::ffi::{CString, c_void};
use std::os::raw::c_int;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{CodecId, CodecParameters, Dict, Error, Packet};
use crate::ffi;

/// How long an open may go without an answer.
const OPEN_STALL: Duration = Duration::from_secs(10);
/// How long a live stream may go without a picture before it counts as hung.
const READ_STALL: Duration = Duration::from_secs(5);
/// How long closing may take, which tells the server the session is over.
const GOODBYE: Duration = Duration::from_secs(1);

/// Asked often during a blocking call, which ends once it says true.
pub type Cancel = Arc<dyn Fn() -> bool + Send + Sync>;

/// A demuxed URL's video stream, H264 and H265 turned into Annex B as Rant carries them.
pub struct Input {
    fmt: *mut ffi::AVFormatContext,
    bsf: *mut ffi::AVBSFContext,
    packet: Packet,
    index: c_int,
    codec: CodecId,
    fps: f64,
    /// Boxed, since FFmpeg holds a pointer to it.
    wait: Box<Wait>,
}

// SAFETY: the contexts belong to this input alone, and the wait is touched only by calls on it
unsafe impl Send for Input {}

/// One unit read from an input.
pub struct InputPacket<'a> {
    pub data: &'a [u8],
    pub key: bool,
    /// The stream's own time in microseconds, None when the packet has none.
    pub time_us: Option<i64>,
}

/// What FFmpeg's interrupt callback reads: the caller's cancel and the stall deadline.
struct Wait {
    cancel: Cancel,
    deadline: Cell<Instant>,
    timed_out: Cell<bool>,
    closing: Cell<bool>,
}

impl Wait {
    fn arm(&self, stall: Duration) {
        self.deadline.set(Instant::now() + stall);
    }

    /// FFmpeg reports our own deadline as an exit request, which says nothing to a reader.
    fn why(&self, code: c_int) -> String {
        if self.timed_out.get() { "no answer in time".to_owned() } else { Error(code).to_string() }
    }
}

unsafe extern "C" fn interrupted(opaque: *mut c_void) -> c_int {
    // SAFETY: opaque is the input's boxed Wait, alive while any call on the input runs
    let w = unsafe { &*(opaque as *const Wait) };
    // a cancel must not cut off the goodbye, or a camera holds the session until it times out
    if w.closing.get() {
        return (Instant::now() > w.deadline.get()) as c_int;
    }
    if (w.cancel)() {
        return 1;
    }
    w.timed_out.set(Instant::now() > w.deadline.get());
    w.timed_out.get() as c_int
}

impl Input {
    /// Opens `url` with demuxer options, `format` naming the demuxer when probing cannot
    /// tell. `cancel` interrupts a blocking open or read.
    pub fn open(url: &str, format: Option<&str>, options: &[(&str, &str)], cancel: Cancel) -> Result<Input, String> {
        let wait = Box::new(Wait { cancel, deadline: Cell::new(Instant::now() + OPEN_STALL), timed_out: Cell::new(false), closing: Cell::new(false) });
        let curl = CString::new(url).map_err(|_| "the URL holds a nul byte".to_string())?;
        // SAFETY: a fresh context whose interrupt points at the boxed wait, which the Input keeps
        unsafe {
            let mut fmt = ffi::avformat_alloc_context();
            (*fmt).interrupt_callback = ffi::AVIOInterruptCB { callback: Some(interrupted), opaque: &*wait as *const Wait as *mut c_void };
            let demuxer = match format {
                Some(f) => ffi::av_find_input_format(CString::new(f).unwrap().as_ptr()),
                None => std::ptr::null(),
            };
            let mut o = Dict::of(options);
            let r = ffi::avformat_open_input(&mut fmt, curl.as_ptr(), demuxer, &mut o.0);
            if r < 0 {
                return Err(wait.why(r)); // the failed open freed the context
            }
            let mut input = Input { fmt, bsf: std::ptr::null_mut(), packet: Packet::new(), index: -1, codec: CodecId::NONE, fps: 0.0, wait };
            input.wait.arm(OPEN_STALL);
            let r = ffi::avformat_find_stream_info(fmt, std::ptr::null_mut());
            if r < 0 {
                return Err(input.wait.why(r));
            }
            input.index = ffi::av_find_best_stream(fmt, ffi::AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
            if input.index < 0 {
                return Err("no video stream".into());
            }
            let st = &**(*fmt).streams.add(input.index as usize);
            input.codec = CodecId((*st.codecpar).codec_id);
            let rate = if st.avg_frame_rate.num != 0 { st.avg_frame_rate } else { st.r_frame_rate };
            input.fps = if rate.den != 0 { rate.num as f64 / rate.den as f64 } else { 0.0 };
            // MP4, HLS and SRT carry H264 and H265 length prefixed. Annex B passes through
            let filter = match input.codec {
                CodecId::H264 => Some(c"h264_mp4toannexb"),
                CodecId::HEVC => Some(c"hevc_mp4toannexb"),
                _ => None,
            };
            if let Some(name) = filter {
                let r = ffi::av_bsf_alloc(ffi::av_bsf_get_by_name(name.as_ptr()), &mut input.bsf);
                if r < 0 {
                    return Err(Error(r).to_string());
                }
                ffi::avcodec_parameters_copy((*input.bsf).par_in, st.codecpar);
                (*input.bsf).time_base_in = st.time_base;
                let r = ffi::av_bsf_init(input.bsf);
                if r < 0 {
                    return Err(Error(r).to_string());
                }
            }
            Ok(input)
        }
    }

    pub fn codec(&self) -> CodecId {
        self.codec
    }

    /// The stream's own frame rate claim, 0 when it makes none.
    pub fn stated_fps(&self) -> f64 {
        self.fps
    }

    /// What the decoder needs, as the packets read come out.
    pub fn parameters(&self) -> CodecParameters {
        // SAFETY: both live as long as the input
        CodecParameters::copy_of(unsafe { if self.bsf.is_null() { (*self.stream()).codecpar } else { (*self.bsf).par_out } })
    }

    fn stream(&self) -> *const ffi::AVStream {
        // SAFETY: the index was checked at open
        unsafe { *(*self.fmt).streams.add(self.index as usize) }
    }

    /// The next unit of the video stream. Err when the stream ended or failed, or stalled.
    pub fn read(&mut self) -> Result<InputPacket<'_>, String> {
        self.packet.unref();
        // the read a stall ended gave the frame a parser held and said nothing was wrong
        if self.wait.timed_out.get() {
            return Err(self.wait.why(0));
        }
        // SAFETY: the packet is ours between reads, and the filter takes its reference on a send
        unsafe {
            loop {
                if !self.bsf.is_null() {
                    let r = ffi::av_bsf_receive_packet(self.bsf, self.packet.as_ptr());
                    if r == 0 {
                        break;
                    }
                    if Error(r) != Error::AGAIN {
                        return Err(self.wait.why(r));
                    }
                }
                self.wait.arm(READ_STALL);
                let r = ffi::av_read_frame(self.fmt, self.packet.as_ptr());
                if r < 0 {
                    return Err(self.wait.why(r));
                }
                if self.packet.raw().stream_index != self.index {
                    self.packet.unref();
                    continue;
                }
                if self.bsf.is_null() {
                    break;
                }
                let r = ffi::av_bsf_send_packet(self.bsf, self.packet.as_ptr());
                if r < 0 {
                    return Err(Error(r).to_string());
                }
            }
            let p = self.packet.raw();
            let ts = if p.dts != super::NO_PTS { p.dts } else { p.pts };
            let base = if self.bsf.is_null() { (*self.stream()).time_base } else { (*self.bsf).time_base_out };
            let time_us = (ts != super::NO_PTS).then(|| ffi::av_rescale_q(ts, base, ffi::AVRational { num: 1, den: 1_000_000 }));
            Ok(InputPacket { data: self.packet.data(), key: p.flags & ffi::AV_PKT_FLAG_KEY as c_int != 0, time_us })
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.wait.closing.set(true);
        self.wait.deadline.set(Instant::now() + GOODBYE);
        // SAFETY: ours alone, the filter's null is fine
        unsafe {
            ffi::av_bsf_free(&mut self.bsf);
            ffi::avformat_close_input(&mut self.fmt);
        }
    }
}
