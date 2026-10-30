//! A parent and the children that change it, as `rules.rs` decides them. A worker runs only
//! while a child has a subscriber, apart from a probe that measures the source.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rant::types::{ExternalVideoStream, Timestamp, VideoCodec, VideoFrame, VideoStreamKind};
use rant::{Node, Publisher, Qos, RemoteVariable, Subscriber, VariableOptions};
use tracing::{info, warn};

use crate::av::{Cancel, CodecId, CodecParameters, Decoder, Frame, Picture, Pool};
use crate::child::{Child, Job};
use crate::rules::{Busy, Coding, Kind, RateMeter, SIDE_MAX, name_fits, needs_child};
use crate::source::{Locals, Payload, Source, TopicSource, UrlSource};

/// What a parent carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Image,
    VideoFrame,
    StreamTopic,
    StreamVariable,
}

impl Origin {
    fn external(self) -> bool {
        matches!(self, Origin::StreamTopic | Origin::StreamVariable)
    }
}

/// One local entity the mesh scan found: a topic of pictures, or a stream descriptor. Its
/// publisher may change, as when a camera restarts, with no change to the stream.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Parent {
    pub name: String,
    pub origin: Origin,
}

/// How often a topic nobody watches is measured again, which only a size change needs.
const REMEASURE_TOPIC: Duration = Duration::from_secs(60);
/// How often a URL with no shape yet is tried, each a connection to its server.
const RETRY_URL: Duration = Duration::from_secs(30);
/// How soon a viewer asking for a child that is not there brings another measure of a topic.
const ASKED_AGAIN: Duration = Duration::from_secs(5);
/// A source that ends sooner than this waits as long before it opens again.
const SHORTEST_LIFE: Duration = Duration::from_secs(2);
/// An encoder nobody has fed for this long closes, since a GeForce counts its sessions.
const CHILD_IDLE: Duration = Duration::from_secs(5);

/// The worker's place among a stream's busy marks, after the four children's.
const WORKER: usize = 4;

/// Milliseconds on a clock that only rises.
pub(crate) fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A name read from the mesh, as given to a create. The '/' skips the RANT_PREFIX a launcher set.
fn absolute(name: &str) -> String {
    format!("/{name}")
}

/// What the source measured, which decides the children.
#[derive(Clone, Copy)]
struct Shape {
    width: u32,
    height: u32,
    fps: f64,
    coding: Coding,
}

/// What the main loop, the worker and the children share.
pub(crate) struct Shared {
    children: [Option<Publisher<VideoFrame>>; 4],
    /// Children another node publishes, left to it.
    taken: [bool; 4],
    /// Children whose publisher was refused, said once.
    refused: [bool; 4],
    shape: Option<Shape>,
    url: String,
    kind: VideoStreamKind,
}

impl Shared {
    fn matches(&self) -> [usize; 4] {
        Kind::ALL.map(|k| self.children[k as usize].as_ref().map_or(0, Publisher::match_count))
    }

    /// Makes or drops each child's publisher to match what is known of the source.
    fn reconcile(&mut self, node: &Node, parent: &Parent) {
        for k in Kind::ALL {
            let i = k as usize;
            let need = !self.taken[i] && name_fits(&parent.name, k) && self.needs(parent.origin, k);
            if need == self.children[i].is_some() {
                continue;
            }
            if !need {
                self.children[i] = None;
                continue;
            }
            match node.publisher::<VideoFrame>(&absolute(&format!("{}{}", parent.name, k.suffix())), Qos::new()) {
                Ok(p) => (self.children[i], self.refused[i]) = (Some(p), false),
                Err(e) => {
                    if !self.refused[i] {
                        warn!("{}{}: {e}", parent.name, k.suffix());
                    }
                    self.refused[i] = true;
                }
            }
        }
    }

    fn needs(&self, origin: Origin, k: Kind) -> bool {
        match self.shape {
            Some(s) => needs_child(k, s.coding, !origin.external(), s.width, s.height),
            // before a measure: pictures always change on the way to Video, a URL's first copy
            // brings it onto the mesh, a VideoFrame topic waits for its codec
            None => (origin == Origin::Image && k == Kind::Video) || (origin.external() && !k.is_preview()),
        }
    }
}

/// What the main loop tells the worker and the children.
#[derive(Default)]
pub(crate) struct Flags {
    stop: AtomicBool,
    probing: AtomicBool,
    /// A new viewer starts on a keyframe.
    keys: [AtomicBool; 4],
    /// Bumps with every new URL, which the worker reopens and the probe measures.
    generation: AtomicU32,
    /// The worker's step of work and each child's encode.
    pub(crate) working: [Busy; 5],
}

impl Flags {
    /// Whether the child's next picture must be a keyframe. Asking clears it.
    pub(crate) fn take_key(&self, kind: Kind) -> bool {
        self.keys[kind as usize].swap(false, Ordering::Relaxed)
    }
}

/// Held so the descriptor keeps arriving.
#[allow(dead_code)]
enum Descriptor {
    Topic(Subscriber<ExternalVideoStream>),
    Variable(RemoteVariable<ExternalVideoStream>),
}

pub struct Stream {
    node: Arc<Node>,
    parent: Parent,
    encoder: Option<String>,
    locals: Locals,
    /// Counts live workers, so the transcoder can wait for them at its end.
    workers: Arc<()>,
    /// No child's name fits.
    skip: bool,
    shared: Arc<Mutex<Shared>>,
    flags: Arc<Flags>,
    _descriptor: Option<Descriptor>,
    worker: Option<JoinHandle<()>>,
    seen: [usize; 4],
    seen_generation: u32,
    last_wanted: Instant,
    last_probe: Option<Instant>,
    next_probe: Instant,
    /// A viewer asks for a child that is not there.
    asked: bool,
}

impl Stream {
    /// `taken` says which of its children another node publishes already.
    pub fn new(node: Arc<Node>, parent: Parent, encoder: Option<String>, locals: Locals, workers: Arc<()>, taken: [bool; 4]) -> Stream {
        let shared = Shared { children: Default::default(), taken, refused: [false; 4], shape: None, url: String::new(), kind: VideoStreamKind::Rtsp };
        let mut s = Stream {
            node,
            skip: !Kind::ALL.iter().any(|&k| name_fits(&parent.name, k)),
            parent,
            encoder,
            locals,
            workers,
            shared: Arc::new(Mutex::new(shared)),
            flags: Arc::default(),
            _descriptor: None,
            worker: None,
            seen: [0; 4],
            seen_generation: 0,
            last_wanted: Instant::now(),
            last_probe: None,
            next_probe: Instant::now(),
            asked: false,
        };
        if s.skip {
            info!("{}: the name is too long to add a child, skipped", s.parent.name);
            return s;
        }
        lock(&s.shared).reconcile(&s.node, &s.parent);
        let name = absolute(&s.parent.name);
        let (to, flags) = (s.shared.clone(), s.flags.clone());
        let made = match s.parent.origin {
            Origin::StreamVariable => s.node.remote_variable::<ExternalVideoStream>(&name, VariableOptions::new()).map(|v| {
                if let Err(e) = v.on_change(move |d| set_descriptor(&to, &flags, &d)) {
                    warn!("{}: {e}", s.parent.name);
                }
                Some(Descriptor::Variable(v))
            }),
            Origin::StreamTopic => s
                .node
                .subscriber::<ExternalVideoStream>(&name, Qos::new().catch_up(1), move |d| set_descriptor(&to, &flags, &d))
                .map(|t| Some(Descriptor::Topic(t))),
            _ => Ok(None),
        };
        match made {
            Ok(d) => s._descriptor = d,
            Err(e) => warn!("{}: {e}", s.parent.name),
        }
        s
    }

    pub fn parent(&self) -> &Parent {
        &self.parent
    }

    /// Says which of its children another node publishes, whose names it leaves to them.
    pub fn set_taken(&mut self, taken: [bool; 4]) {
        let mut s = lock(&self.shared);
        if s.taken != taken {
            s.taken = taken;
            s.reconcile(&self.node, &self.parent);
        }
    }

    /// A viewer asks for a child that is not there: the source may have changed, so measure it.
    pub fn asked(&mut self) {
        // a viewer may ask for good for a child its source never needs, and a URL's measure is a connection
        let gap = if self.parent.origin.external() { RETRY_URL } else { ASKED_AGAIN };
        let again = self.last_probe.map_or_else(Instant::now, |at| at + gap);
        self.next_probe = self.next_probe.min(again);
        self.asked = true;
    }

    /// A thread of this stream has been at one step for longer than the limit.
    pub fn stuck(&self, now_ms: u64, limit_ms: u64) -> bool {
        self.flags.working.iter().any(|b| b.stuck(now_ms, limit_ms))
    }

    /// The main thread calls it about ten times a second.
    pub fn tick(&mut self) {
        if self.skip {
            return;
        }
        let now = Instant::now();
        let external = self.parent.origin.external();
        let (matches, shaped, openable) = {
            let s = lock(&self.shared);
            (s.matches(), s.shape.is_some(), !external || !s.url.is_empty())
        };
        let generation = self.flags.generation.load(Ordering::Relaxed);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            self.next_probe = now; // a new URL is probed at once
        }
        for k in Kind::ALL {
            let i = k as usize;
            if matches[i] > self.seen[i] {
                self.flags.keys[i].store(true, Ordering::Relaxed);
            }
        }
        self.seen = matches;

        let wanted = matches.iter().any(|&m| m > 0);
        if wanted {
            self.last_wanted = now;
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.worker.take().map(JoinHandle::join);
        }
        if self.worker.is_none() {
            if wanted && openable {
                self.start(false);
            } else if openable && now >= self.next_probe && (!external || !shaped || self.asked) {
                self.last_probe = Some(now);
                self.next_probe = now + if external { RETRY_URL } else { REMEASURE_TOPIC };
                self.asked = false;
                self.start(true);
            }
        } else if !wanted && !self.flags.probing.load(Ordering::Relaxed) && now - self.last_wanted > Duration::from_secs(2) {
            // nobody watched for 2 s: the parent subscription closes with the worker
            self.flags.stop.store(true, Ordering::Relaxed);
        }
    }

    fn start(&mut self, probe: bool) {
        self.flags.stop.store(false, Ordering::Relaxed);
        self.flags.probing.store(probe, Ordering::Relaxed);
        let w = Worker {
            node: self.node.clone(),
            parent: self.parent.clone(),
            encoder: self.encoder.clone(),
            locals: self.locals.clone(),
            shared: self.shared.clone(),
            flags: self.flags.clone(),
            out: VideoFrame::default(),
            _counted: self.workers.clone(),
        };
        self.worker = Some(std::thread::spawn(move || w.run()));
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.flags.stop.store(true, Ordering::Relaxed);
        // the names go free now for a stream that takes over, and the worker ends in its own
        // time, since one stuck in FFmpeg or a driver must not hold up the main thread
        lock(&self.shared).children = Default::default();
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_descriptor(shared: &Mutex<Shared>, flags: &Flags, d: &ExternalVideoStream) {
    let mut s = lock(shared);
    let url = d.url.value.as_str();
    if s.url == url && s.kind == d.kind {
        return;
    }
    s.url = url.to_owned();
    s.kind = d.kind;
    s.shape = None; // another stream: measure it again
    flags.generation.fetch_add(1, Ordering::Relaxed);
}

/// The children's encoders, started when a picture first comes for them, and the buffers of
/// the pictures handed to them.
#[derive(Default)]
struct Children {
    encoders: [Option<Child>; 4],
    pool: Pool,
}

impl Children {
    /// Closes each encoder nobody watches any more, so its session is free for another.
    fn close_idle(&mut self, wanted: [bool; 4]) {
        for k in Kind::ALL {
            let slot = &mut self.encoders[k as usize];
            if !wanted[k as usize] && slot.as_ref().is_some_and(|c| c.idle() > CHILD_IDLE) {
                *slot = None;
            }
        }
    }
}

/// The thread that pulls the parent, decodes it when it must and feeds each child.
struct Worker {
    node: Arc<Node>,
    parent: Parent,
    encoder: Option<String>,
    locals: Locals,
    shared: Arc<Mutex<Shared>>,
    flags: Arc<Flags>,
    /// The reused outgoing message.
    out: VideoFrame,
    _counted: Arc<()>,
}

impl Worker {
    fn run(mut self) {
        let flags = self.flags.clone();
        let probing = || flags.probing.load(Ordering::Relaxed);
        let stopped = || flags.stop.load(Ordering::Relaxed);
        let external = self.parent.origin.external();
        let mut src: Option<Box<dyn Source>> = None;
        let mut generation = 0;
        let mut params: Option<CodecParameters> = None;
        let mut stated_fps = 0.0;
        let mut dec: Option<Decoder> = None;
        let mut children = Children::default();
        let mut meter = RateMeter::default();
        let mut synced = false; // the decoder started on a keyframe and missed nothing since
        let (mut told_open, mut told_decode) = (false, false);
        let (mut width, mut height) = (0, 0); // the latest picture size, decoded or stated
        let mut decoded_size = false; // a decoder said the size, which outranks what is stated
        let mut coding = Coding::Raw;
        let began = Instant::now();
        let mut opened = began;
        let mut last_measure = began;
        let mut measured_as = None;
        let rate = |meter: &RateMeter, stated: f64| if meter.count() >= 2 { meter.fps() } else { stated };

        while !stopped() {
            self.flags.working[WORKER].begin(clock_ms());
            if probing() {
                // a URL's probe is a connection, so it ends by the clock. A topic's waits for a
                // first picture, which costs nothing, unless what comes never tells its size
                let done = if external { began.elapsed() > Duration::from_secs(3) } else { (width > 0 && began.elapsed() > Duration::from_secs(1)) || (meter.count() > 0 && began.elapsed() > Duration::from_secs(10)) };
                if done {
                    if width > 0 {
                        self.measured(width, height, rate(&meter, stated_fps), coding);
                    }
                    self.flags.probing.store(false, Ordering::Relaxed);
                    if lock(&self.shared).matches() == [0; 4] {
                        break;
                    }
                }
            }

            let moved = external && generation != self.flags.generation.load(Ordering::Relaxed);
            let ended = src.as_ref().is_some_and(|s| s.ended());
            if ended && !moved && opened.elapsed() < SHORTEST_LIFE {
                self.pause(SHORTEST_LIFE - opened.elapsed(), generation);
                continue;
            }
            if src.is_none() || ended || moved {
                src = None;
                dec = None;
                synced = false;
                decoded_size = false;
                match self.open(&mut generation) {
                    Ok(s) => {
                        params = s.parameters();
                        stated_fps = s.stated_fps();
                        src = Some(s);
                        opened = Instant::now();
                        told_open = false;
                    }
                    Err(why) => {
                        if !told_open && !stopped() {
                            warn!("{}: cannot open: {why}", self.parent.name);
                        }
                        told_open = true;
                        self.pause(Duration::from_secs(2), generation);
                        if probing() {
                            break;
                        }
                        continue;
                    }
                }
            }

            let Some(u) = src.as_mut().and_then(|s| s.read(Duration::from_millis(200))) else { continue };
            meter.add(u.time_us);
            let (w, h) = match &u.payload {
                Payload::Picture(p) => (p.width(), p.height()),
                Payload::Coded { .. } => (u.width, u.height),
            };
            let stated = w > 0 && h > 0 && w <= SIDE_MAX && h <= SIDE_MAX;
            if stated && !decoded_size {
                (width, height) = (w, h);
            }
            let fps = Some(rate(&meter, stated_fps)).filter(|&r| r > 0.0).unwrap_or(30.0);
            let wanted = lock(&self.shared).matches().map(|m| m > 0);

            match u.payload {
                Payload::Picture(p) => {
                    coding = Coding::Raw;
                    decoded_size = false;
                    if wanted.contains(&true) {
                        self.feed(&mut children, &p, wanted, fps, u.time_us);
                    }
                }
                Payload::Coded { codec, data, key } => {
                    coding = coding_of(codec);
                    // a URL's H264 and MJPEG come onto the mesh as they are
                    let copied = |k: Kind| matches!((k, coding), (Kind::Video, Coding::H264) | (Kind::Mjpeg, Coding::Mjpeg));
                    for k in [Kind::Video, Kind::Mjpeg] {
                        if wanted[k as usize] && copied(k) {
                            self.publish(k, data, key, width, height, u.time_us);
                        }
                    }
                    let encoded = Kind::ALL.map(|k| wanted[k as usize] && !copied(k));
                    if encoded.contains(&true) || width == 0 {
                        // a topic may change its codec at any frame
                        if dec.as_ref().is_some_and(|d| d.codec() != codec) {
                            dec = None;
                            synced = false;
                            decoded_size = false;
                        }
                        if !dec.as_ref().is_some_and(Decoder::is_open) {
                            dec = Decoder::open(codec, params.as_ref().filter(|p| p.codec() == codec), self.encoder.as_deref());
                            match &dec {
                                Some(d) => {
                                    info!("{}: decoding with {}", self.parent.name, d.describe());
                                    told_decode = false;
                                }
                                None => {
                                    if !told_decode {
                                        warn!("{}: no decoder for {}", self.parent.name, codec.name());
                                    }
                                    told_decode = true;
                                    continue;
                                }
                            }
                        }
                        if !synced && !key {
                            continue;
                        }
                        let d = dec.as_mut().unwrap();
                        synced = d.decode(data, |f| {
                            (width, height) = (f.width(), f.height());
                            decoded_size = true;
                            self.feed(&mut children, f, encoded, fps, u.time_us);
                        });
                    } else {
                        synced = false; // frames go by undecoded, so the next decode starts on a keyframe
                        decoded_size = false;
                    }
                }
            }
            children.close_idle(wanted);

            // a new size or codec shows at once, but a source flipping between them never makes a storm
            let since = last_measure.elapsed();
            let shape = (width, height, coding);
            if !probing() && width > 0 && (since > Duration::from_secs(2) || (measured_as != Some(shape) && since > Duration::from_millis(500))) {
                self.measured(width, height, rate(&meter, stated_fps), coding);
                last_measure = Instant::now();
                measured_as = Some(shape);
            }
        }
        self.flags.working[WORKER].end();
    }

    /// Waits, until the stream stops or its URL moves on.
    fn pause(&self, how_long: Duration, generation: u32) {
        let from = Instant::now();
        while from.elapsed() < how_long && !self.flags.stop.load(Ordering::Relaxed) && self.flags.generation.load(Ordering::Relaxed) == generation {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The parent's source: its topic, or the URL its descriptor names now.
    fn open(&self, generation: &mut u32) -> Result<Box<dyn Source>, String> {
        if !self.parent.origin.external() {
            let name = absolute(&self.parent.name);
            let s = TopicSource::open(&self.node, &name, self.parent.origin == Origin::Image, self.locals.clone());
            return s.map(|s| Box::new(s) as Box<dyn Source>).map_err(|e| e.to_string());
        }
        // the generation first, so a URL set meanwhile reopens rather than goes unseen
        *generation = self.flags.generation.load(Ordering::Relaxed);
        let (url, kind) = {
            let s = lock(&self.shared);
            (s.url.clone(), s.kind)
        };
        if url.is_empty() {
            return Err("no URL yet".into());
        }
        let (flags, at) = (self.flags.clone(), *generation);
        let cancel: Cancel = Arc::new(move || flags.stop.load(Ordering::Relaxed) || flags.generation.load(Ordering::Relaxed) != at);
        UrlSource::open(&url, kind, cancel).map(|s| Box::new(s) as Box<dyn Source>)
    }

    /// Hands one picture to the children that take it, each encoding it on its own thread.
    fn feed(&self, children: &mut Children, picture: &Frame, wanted: [bool; 4], fps: f64, time_us: u64) {
        // the children read it after the source has moved on, so it needs a frame of its own
        let Some(picture) = children.pool.own(picture).map(|f| Arc::new(Picture::new(f))) else { return };
        for k in Kind::ALL {
            let slot = &mut children.encoders[k as usize];
            if slot.as_ref().is_some_and(Child::died) {
                warn!("{}{}: its encoder thread died, starting another", self.parent.name, k.suffix());
                *slot = None;
            }
            if wanted[k as usize] {
                let c = slot.get_or_insert_with(|| Child::start(&self.parent.name, k, self.encoder.clone(), self.shared.clone(), self.flags.clone()));
                c.give(Job { picture: picture.clone(), time_us, fps });
            }
        }
    }

    fn publish(&mut self, kind: Kind, data: &[u8], key: bool, width: u32, height: u32, time_us: u64) {
        publish(&self.shared, &mut self.out, kind, data, key, width, height, time_us);
    }

    /// Records the source's shape and makes or drops the children it calls for.
    fn measured(&self, width: u32, height: u32, fps: f64, coding: Coding) {
        let mut s = lock(&self.shared);
        let changed = s.shape.is_none_or(|o| o.width != width || o.height != height || o.coding != coding || (fps - o.fps).abs() > o.fps * 0.2);
        s.shape = Some(Shape { width, height, fps, coding });
        s.reconcile(&self.node, &self.parent);
        if !changed {
            return;
        }
        let made: Vec<&str> = Kind::ALL.iter().filter(|&&k| s.children[k as usize].is_some()).map(|k| &k.suffix()[1..]).collect();
        let children = if made.is_empty() { "nothing to republish".to_owned() } else { format!("republished as {}", made.join(", ")) };
        info!("{}: {width}x{height} at {fps:.1} fps {coding:?}, {children}", self.parent.name);
    }
}

fn coding_of(codec: CodecId) -> Coding {
    match codec {
        CodecId::H264 => Coding::H264,
        CodecId::MJPEG => Coding::Mjpeg,
        _ => Coding::Other,
    }
}

/// Sends one access unit on a child, when someone watches it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish(shared: &Mutex<Shared>, out: &mut VideoFrame, kind: Kind, data: &[u8], key: bool, width: u32, height: u32, time_us: u64) {
    out.codec = if kind.is_mjpeg() { VideoCodec::Mjpeg } else { VideoCodec::H264 };
    out.width = width;
    out.height = height;
    out.keyframe = key;
    out.pts = Timestamp { us: time_us as i64 };
    out.data.clear();
    out.data.extend_from_slice(data);
    let s = lock(shared);
    if let Some(p) = s.children[kind as usize].as_ref().filter(|p| p.match_count() > 0) {
        let _ = p.send_captured(out, out.pts);
    }
}
