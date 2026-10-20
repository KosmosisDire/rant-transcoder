//! The Rant transcoder: one node per host that republishes this host's video as H264 any
//! viewer can play, only where that changes it.

pub mod annexb;
pub mod av;
pub mod hosts;
pub mod rules;

#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unsafe_op_in_unsafe_fn, clippy::all)]
mod ffi;
