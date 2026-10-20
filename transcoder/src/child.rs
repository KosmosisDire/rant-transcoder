//! A child's encoder on a thread of its own, fed the newest picture. A child still busy with
//! one picture skips those that came meanwhile, so a slow child slows no other.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use rant::types::{VideoCodec, VideoFrame};
use tracing::{info, warn};

use crate::av::{Output, Picture};
use crate::rules::{PREVIEW_FPS, RateGate, preview_size};
use crate::stream::{Flags, Shared, publish};

/// A picture for a child, with the source's rate when it came.
pub struct Job {
    pub picture: Arc<Picture>,
    pub time_us: u64,
    pub fps: f64,
}

/// The newest job not started yet, and whether the child must stop.
#[derive(Default)]
struct Slot {
    state: Mutex<(Option<Job>, bool)>,
    ready: Condvar,
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, (Option<Job>, bool)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Child {
    slot: Arc<Slot>,
    thread: Option<JoinHandle<()>>,
}

impl Child {
    /// `name` is the parent's. The preview child keeps its own size and rate.
    pub fn start(name: &str, preview: bool, only: Option<String>, shared: Arc<Mutex<Shared>>, flags: Arc<Flags>) -> Child {
        let e = Encoding {
            name: format!("{name}{}", if preview { "/Preview" } else { "/Video" }),
            preview,
            only,
            shared,
            flags,
            output: None,
            failed: None,
            gate: preview.then(|| RateGate::new(PREVIEW_FPS)),
            out: VideoFrame { codec: VideoCodec::H264, ..VideoFrame::default() },
        };
        let slot = Arc::new(Slot::default());
        let s = slot.clone();
        Child { slot, thread: Some(std::thread::spawn(move || e.run(&s))) }
    }

    /// Hands over the newest picture, in place of any the child has not started.
    pub fn give(&self, job: Job) {
        self.slot.lock().0 = Some(job);
        self.slot.ready.notify_one();
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.slot.lock().1 = true;
        self.slot.ready.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The child's thread: its encoder and what feeds it.
struct Encoding {
    name: String,
    preview: bool,
    only: Option<String>,
    shared: Arc<Mutex<Shared>>,
    flags: Arc<Flags>,
    output: Option<Output>,
    /// The size no encoder opened at, not retried.
    failed: Option<(u32, u32)>,
    gate: Option<RateGate>,
    /// The reused outgoing message.
    out: VideoFrame,
}

impl Encoding {
    fn run(mut self, slot: &Slot) {
        loop {
            let job = {
                let state = slot.ready.wait_while(slot.lock(), |(job, stop)| job.is_none() && !*stop);
                let mut state = state.unwrap_or_else(|e| e.into_inner());
                if state.1 {
                    return;
                }
                state.0.take()
            };
            if let Some(job) = job {
                self.encode(&job);
            }
        }
    }

    fn encode(&mut self, job: &Job) {
        if self.gate.as_mut().is_some_and(|g| !g.pass(job.time_us)) {
            return;
        }
        let p = &*job.picture;
        let (width, height) = (p.frame().width(), p.frame().height());
        let (w, h, fps) = if self.preview {
            let (w, h) = preview_size(width, height);
            (w, h, job.fps.min(PREVIEW_FPS))
        } else {
            (width, height, job.fps)
        };
        if !self.ready(p, w, h, fps) {
            return;
        }
        let Some(o) = &mut self.output else { return };
        let (shared, out, preview, t) = (&self.shared, &mut self.out, self.preview, job.time_us);
        o.encode(p, t, self.flags.take_key(preview), |data, key| publish(shared, out, preview, data, key, w, h, t));
    }

    /// Opens the output for pictures like this one at a size when it is not open for them already.
    fn ready(&mut self, picture: &Picture, width: u32, height: u32, fps: f64) -> bool {
        if self.output.as_ref().is_some_and(|o| o.fits(picture, width, height)) {
            return true;
        }
        if self.failed == Some((width, height)) {
            return false;
        }
        self.output = None; // a GeForce counts the open sessions
        self.output = Output::open(picture, width, height, fps, self.only.as_deref());
        match &self.output {
            Some(o) => info!("{}: {width}x{height} at {fps:.1} fps with {}", self.name, o.describe()),
            None => {
                self.failed = Some((width, height));
                warn!("{}: no H264 encoder opens at {width}x{height}", self.name);
            }
        }
        self.output.is_some()
    }
}
