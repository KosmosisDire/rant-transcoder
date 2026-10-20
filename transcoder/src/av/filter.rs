use std::ffi::CString;
use std::os::raw::c_int;

use super::hw::Frames;
use super::{Error, Frame};
use crate::ffi;

/// A chain of FFmpeg filters, such as a device's scaler, fed pictures of one size, format and
/// pool.
pub(super) struct Filter {
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    takes: Shape,
    out: Frame,
}

// SAFETY: the graph belongs to this filter alone
unsafe impl Send for Filter {}

/// What a picture must share with the first for the chain to take it.
#[derive(PartialEq, Eq)]
struct Shape {
    width: c_int,
    height: c_int,
    format: ffi::AVPixelFormat,
    range: ffi::AVColorRange,
    pool: usize,
}

impl Shape {
    fn of(f: &Frame) -> Shape {
        let r = f.raw();
        // SAFETY: a frame's pool reference is null or valid
        let pool = unsafe { r.hw_frames_ctx.as_ref() }.map_or(0, |b| b.data as usize);
        Shape { width: r.width, height: r.height, format: r.format, range: r.color_range, pool }
    }
}

impl Filter {
    /// `chain` in FFmpeg's filter graph syntax, fed pictures like `first`.
    pub(super) fn new(first: &Frame, chain: &str) -> Result<Filter, Error> {
        let chain = CString::new(chain).map_err(|_| Error::INVALID)?;
        // SAFETY: each context is made in the graph, which frees them all on drop, and the
        // in and out lists are freed here whatever the parse did with them
        unsafe {
            let mut f = Filter { graph: ffi::avfilter_graph_alloc(), src: std::ptr::null_mut(), sink: std::ptr::null_mut(), takes: Shape::of(first), out: Frame::new() };
            if f.graph.is_null() {
                return Err(Error::NO_MEMORY);
            }
            f.src = ffi::avfilter_graph_alloc_filter(f.graph, ffi::avfilter_get_by_name(c"buffer".as_ptr()), c"in".as_ptr());
            let params = ffi::av_buffersrc_parameters_alloc();
            if f.src.is_null() || params.is_null() {
                ffi::av_free(params.cast());
                return Err(Error::NO_MEMORY);
            }
            let r = first.raw();
            let p = &mut *params;
            (p.format, p.width, p.height) = (r.format, r.width, r.height);
            p.time_base = ffi::AVRational { num: 1, den: 1_000_000 };
            p.sample_aspect_ratio = r.sample_aspect_ratio;
            p.hw_frames_ctx = r.hw_frames_ctx;
            (p.color_space, p.color_range) = (r.colorspace, r.color_range);
            let set = ffi::av_buffersrc_parameters_set(f.src, params);
            ffi::av_free(params.cast());
            check(set)?;
            check(ffi::avfilter_init_dict(f.src, std::ptr::null_mut()))?;
            let buffersink = ffi::avfilter_get_by_name(c"buffersink".as_ptr());
            check(ffi::avfilter_graph_create_filter(&mut f.sink, buffersink, c"out".as_ptr(), std::ptr::null(), std::ptr::null_mut(), f.graph))?;
            // the chain reads from the source's pad and writes to the sink's
            let mut outputs = ffi::avfilter_inout_alloc();
            let mut inputs = ffi::avfilter_inout_alloc();
            let parsed = if outputs.is_null() || inputs.is_null() {
                Error::NO_MEMORY.0
            } else {
                ((*outputs).name, (*outputs).filter_ctx, (*outputs).pad_idx) = (ffi::av_strdup(c"in".as_ptr()), f.src, 0);
                ((*inputs).name, (*inputs).filter_ctx, (*inputs).pad_idx) = (ffi::av_strdup(c"out".as_ptr()), f.sink, 0);
                ffi::avfilter_graph_parse_ptr(f.graph, chain.as_ptr(), &mut inputs, &mut outputs, std::ptr::null_mut())
            };
            ffi::avfilter_inout_free(&mut inputs);
            ffi::avfilter_inout_free(&mut outputs);
            check(parsed)?;
            check(ffi::avfilter_graph_config(f.graph, std::ptr::null_mut()))?;
            Ok(f)
        }
    }

    /// The picture is like the first, so the chain takes it.
    pub(super) fn takes(&self, picture: &Frame) -> bool {
        Shape::of(picture) == self.takes
    }

    /// The pool of the pictures it gives out, None in system memory.
    pub(super) fn frames(&self) -> Option<Frames> {
        // SAFETY: a configured sink
        Frames::of(unsafe { ffi::av_buffersink_get_hw_frames_ctx(self.sink) })
    }

    /// Runs one picture through, and `out` gets each that comes out. False when the chain
    /// refuses it.
    pub(super) fn run(&mut self, picture: &Frame, mut out: impl FnMut(&mut Frame)) -> bool {
        // SAFETY: with KEEP_REF the source takes its own reference and leaves the picture as it was
        unsafe {
            if ffi::av_buffersrc_add_frame_flags(self.src, picture.as_ptr().cast_mut(), ffi::AV_BUFFERSRC_FLAG_KEEP_REF as c_int) < 0 {
                return false;
            }
            while ffi::av_buffersink_get_frame(self.sink, self.out.as_mut_ptr()) >= 0 {
                out(&mut self.out);
                self.out.unref();
            }
        }
        true
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        // SAFETY: ours alone, and it frees every filter in it
        unsafe { ffi::avfilter_graph_free(&mut self.graph) };
    }
}

fn check(r: c_int) -> Result<(), Error> {
    if r < 0 { Err(Error(r)) } else { Ok(()) }
}
