//! A child's encoder on a thread of its own, fed the newest picture. A child still busy with
//! one picture skips those that came meanwhile, so a slow child slows no other.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rant::types::VideoFrame;
use tracing::{info, warn};

use crate::av::{Output, Picture};
use crate::rules::{Kind, PREVIEW_FPS, RateGate};
use crate::stream::{Flags, Shared, clock_ms, publish};

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
    given: Instant,
}

impl Child {
    /// `name` is the parent's. The kind says the child's size, rate and codec.
    pub fn start(name: &str, kind: Kind, only: Option<String>, shared: Arc<Mutex<Shared>>, flags: Arc<Flags>) -> Child {
        let e = Encoding {
            name: format!("{name}{}", kind.suffix()),
            kind,
            only,
            shared,
            flags,
            output: None,
            errors: 0,
            refused: Vec::new(),
            failed: None,
            gate: kind.is_preview().then(|| RateGate::new(PREVIEW_FPS)),
            out: VideoFrame::default(),
        };
        let slot = Arc::new(Slot::default());
        let s = slot.clone();
        Child { slot, thread: Some(std::thread::spawn(move || e.run(&s))), given: Instant::now() }
    }

    /// Hands over the newest picture, in place of any the child has not started.
    pub fn give(&mut self, job: Job) {
        self.given = Instant::now();
        self.slot.lock().0 = Some(job);
        self.slot.ready.notify_one();
    }

    /// How long since a picture came, so an unwatched child can give up its encoder.
    pub fn idle(&self) -> Duration {
        self.given.elapsed()
    }

    /// Its thread ended, which only a panic does, so it must be started again.
    pub fn died(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
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
    kind: Kind,
    only: Option<String>,
    shared: Arc<Mutex<Shared>>,
    flags: Arc<Flags>,
    output: Option<Output>,
    /// Pictures the output failed in a row.
    errors: u32,
    /// Encoders that kept failing, left out until a minute has passed.
    refused: Vec<(&'static str, Instant)>,
    /// The size no encoder opened at, and when, so it waits before the next try.
    failed: Option<((u32, u32), Instant)>,
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
                let flags = self.flags.clone();
                let busy = &flags.working[self.kind as usize];
                busy.begin(clock_ms());
                self.encode(&job);
                busy.end();
            }
        }
    }

    fn encode(&mut self, job: &Job) {
        if self.gate.as_mut().is_some_and(|g| !g.pass(job.time_us)) {
            return;
        }
        let p = &*job.picture;
        let kind = self.kind;
        let (w, h) = kind.size(p.frame().width(), p.frame().height());
        let fps = if kind.is_preview() { job.fps.min(PREVIEW_FPS) } else { job.fps };
        if !self.ready(p, w, h, fps) {
            return;
        }
        let Some(o) = &mut self.output else { return };
        let (shared, out, t) = (&self.shared, &mut self.out, job.time_us);
        if o.encode(p, t, self.flags.take_key(kind), |data, key| publish(shared, out, kind, data, key, w, h, t)) {
            self.errors = 0;
            return;
        }
        // a failure can kill an encoder for good, as a picture that overflows OpenH264 does, so
        // it opens again at once. One that keeps failing gives way to the next
        if self.errors == 0 {
            info!("{}: {} failed a picture, opening it again", self.name, o.describe());
        }
        self.errors += 1;
        if self.errors.is_multiple_of(30) {
            warn!("{}: {} keeps failing, trying the next encoder", self.name, o.describe());
            self.refused.push((o.encoder(), Instant::now()));
        }
        self.output = None;
    }

    /// Opens the output for pictures like this one at a size when it is not open for them already.
    fn ready(&mut self, picture: &Picture, width: u32, height: u32, fps: f64) -> bool {
        if self.output.as_ref().is_some_and(|o| o.fits(picture, width, height)) {
            return true;
        }
        let now = Instant::now();
        let size = (width, height);
        // a full NVENC or a busy device may free up, so a failed size is tried again
        if self.failed.is_some_and(|(s, at)| s == size && now - at < Duration::from_secs(10)) {
            return false;
        }
        self.refused.retain(|(_, at)| now - *at < Duration::from_secs(60));
        let refused: Vec<&str> = self.refused.iter().map(|(name, _)| *name).collect();
        self.output = None; // a GeForce counts the open sessions
        self.output = if self.kind.is_mjpeg() {
            Output::open_jpeg(picture, width, height, self.kind.jpeg_quantizer())
        } else {
            let only = self.only.as_deref();
            // on a host with one encoder, a refused one still beats none
            Output::open(picture, width, height, fps, only, &refused).or_else(|| if refused.is_empty() { None } else { Output::open(picture, width, height, fps, only, &[]) })
        };
        match &self.output {
            Some(o) => {
                info!("{}: {width}x{height} at {fps:.1} fps with {}", self.name, o.describe());
                self.failed = None;
            }
            None => {
                if self.failed.is_none_or(|(s, _)| s != size) {
                    warn!("{}: no encoder opens at {width}x{height}", self.name);
                }
                self.failed = Some((size, now));
            }
        }
        self.output.is_some()
    }
}
