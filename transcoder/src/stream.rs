//! A parent and the children that change it: `Parent/Video` at the source's own size and
//! rate unless the parent is an H264 topic already, `Parent/Preview` when it would be
//! smaller. A worker runs only while a child has a subscriber, apart from a probe that
//! measures the source.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rant::types::{ExternalVideoStream, Timestamp, VideoCodec, VideoFrame, VideoStreamKind};
use rant::{Node, Publisher, Qos, RemoteVariable, Subscriber, VariableOptions};
use tracing::{info, warn};

use crate::av::{Cancel, CodecId, CodecParameters, Decoder, Frame, Picture, Pool};
use crate::child::{Child, Job};
use crate::rules::{RateMeter, needs_preview};
use crate::source::{Payload, Source, TopicSource, UrlSource};

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

/// One local entity the mesh scan found: a topic of pictures, or a stream descriptor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Parent {
    pub name: String,
    pub origin: Origin,
    /// The local publisher or owner.
    pub peer: u32,
}

/// How often a topic nobody watches is measured again, which only a size change needs.
const REMEASURE_TOPIC: Duration = Duration::from_secs(60);
/// How often a URL with no shape yet is tried, each a connection to its server.
const RETRY_URL: Duration = Duration::from_secs(30);
/// How soon a viewer asking for a child that is not there brings another measure of a topic.
const ASKED_AGAIN: Duration = Duration::from_secs(5);
/// A source that ends sooner than this waits as long before it opens again.
const SHORTEST_LIFE: Duration = Duration::from_secs(2);

/// Rant's name limit, which a child's name must also fit.
const NAME_MAX: usize = 64;

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
    h264: bool,
}

/// What the main loop, the worker and the children share.
pub(crate) struct Shared {
    video: Option<Publisher<VideoFrame>>,
    preview: Option<Publisher<VideoFrame>>,
    shape: Option<Shape>,
    url: String,
    kind: VideoStreamKind,
}

impl Shared {
    fn matches(&self) -> (usize, usize) {
        let count = |p: &Option<Publisher<VideoFrame>>| p.as_ref().map_or(0, Publisher::match_count);
        (count(&self.video), count(&self.preview))
    }
}

/// What the main loop tells the worker and the children.
#[derive(Default)]
pub(crate) struct Flags {
    stop: AtomicBool,
    probing: AtomicBool,
    /// A new viewer starts on a keyframe.
    key_video: AtomicBool,
    key_preview: AtomicBool,
    /// Bumps with every new URL, which the worker reopens and the probe measures.
    generation: AtomicU32,
}

impl Flags {
    /// Whether the child's next picture must be a keyframe. Asking clears it.
    pub(crate) fn take_key(&self, preview: bool) -> bool {
        if preview { &self.key_preview } else { &self.key_video }.swap(false, Ordering::Relaxed)
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
    /// Counts live workers, so the transcoder can wait for them at its end.
    workers: Arc<()>,
    /// The name leaves no room for a child's.
    skip: bool,
    shared: Arc<Mutex<Shared>>,
    flags: Arc<Flags>,
    _descriptor: Option<Descriptor>,
    worker: Option<JoinHandle<()>>,
    seen: (usize, usize),
    seen_generation: u32,
    last_wanted: Instant,
    last_probe: Option<Instant>,
    next_probe: Instant,
    /// A viewer asks for a child that is not there.
    asked: bool,
}

impl Stream {
    pub fn new(node: Arc<Node>, parent: Parent, encoder: Option<String>, workers: Arc<()>) -> Stream {
        let shared = Arc::new(Mutex::new(Shared { video: None, preview: None, shape: None, url: String::new(), kind: VideoStreamKind::Rtsp }));
        let mut s = Stream {
            node,
            skip: parent.name.len() + "/Preview".len() > NAME_MAX,
            parent,
            encoder,
            workers,
            shared,
            flags: Arc::default(),
            _descriptor: None,
            worker: None,
            seen: (0, 0),
            seen_generation: 0,
            last_wanted: Instant::now(),
            last_probe: None,
            next_probe: Instant::now(),
            asked: false,
        };
        if s.skip {
            info!("{}: the name is too long to add /Video and /Preview, skipped", s.parent.name);
            return s;
        }
        // pictures and URLs always change on the way to Video, a VideoFrame topic waits for its codec
        if s.parent.origin != Origin::VideoFrame {
            child(&s.node, &mut lock(&s.shared).video, true, &s.parent.name, "/Video");
        }
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

    /// A viewer asks for a child that is not there: the source may have changed, so measure it.
    pub fn asked(&mut self) {
        // a viewer may ask for good for a child its source never needs, and a URL's measure is a connection
        let gap = if self.parent.origin.external() { RETRY_URL } else { ASKED_AGAIN };
        let again = self.last_probe.map_or_else(Instant::now, |at| at + gap);
        self.next_probe = self.next_probe.min(again);
        self.asked = true;
    }

    /// The main thread calls it about ten times a second.
    pub fn tick(&mut self) {
        if self.skip {
            return;
        }
        let now = Instant::now();
        let external = self.parent.origin.external();
        let ((video, preview), shaped, openable) = {
            let s = lock(&self.shared);
            (s.matches(), s.shape.is_some(), !self.parent.origin.external() || !s.url.is_empty())
        };
        let generation = self.flags.generation.load(Ordering::Relaxed);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            self.next_probe = now; // a new URL is probed at once
        }
        if video > self.seen.0 {
            self.flags.key_video.store(true, Ordering::Relaxed);
        }
        if preview > self.seen.1 {
            self.flags.key_preview.store(true, Ordering::Relaxed);
        }
        self.seen = (video, preview);

        let wanted = video > 0 || preview > 0;
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
            shared: self.shared.clone(),
            flags: self.flags.clone(),
            out: VideoFrame { codec: VideoCodec::H264, ..VideoFrame::default() },
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
        let mut s = lock(&self.shared);
        (s.video, s.preview) = (None, None);
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

/// Makes or drops one child's publisher to match `need`.
fn child(node: &Node, slot: &mut Option<Publisher<VideoFrame>>, need: bool, parent: &str, suffix: &str) {
    if need == slot.is_some() {
        return;
    }
    if !need {
        *slot = None;
        return;
    }
    match node.publisher::<VideoFrame>(&absolute(&format!("{parent}{suffix}")), Qos::new()) {
        Ok(p) => *slot = Some(p),
        Err(e) => warn!("{parent}{suffix}: {e}"),
    }
}

/// The children's encoders, started when a picture first comes for them, and the buffers of
/// the pictures handed to them.
#[derive(Default)]
struct Children {
    video: Option<Child>,
    preview: Option<Child>,
    pool: Pool,
}

/// The thread that pulls the parent, decodes it when it must and feeds each child.
struct Worker {
    node: Arc<Node>,
    parent: Parent,
    encoder: Option<String>,
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
        let mut h264 = false; // the source arrives as H264
        let began = Instant::now();
        let mut opened = began;
        let mut last_measure = began;
        let mut measured_as = None;
        let external = self.parent.origin.external();
        let rate = |meter: &RateMeter, stated: f64| if meter.count() >= 2 { meter.fps() } else { stated };

        while !self.flags.stop.load(Ordering::Relaxed) {
            if probing() {
                // a URL's probe is a connection, so it ends by the clock. A topic's waits for a
                // first picture, which costs nothing, unless what comes never tells its size
                let done = if external { began.elapsed() > Duration::from_secs(3) } else { (width > 0 && began.elapsed() > Duration::from_secs(1)) || (meter.count() > 0 && began.elapsed() > Duration::from_secs(10)) };
                if done {
                    if width > 0 {
                        self.measured(width, height, rate(&meter, stated_fps), h264);
                    }
                    self.flags.probing.store(false, Ordering::Relaxed);
                    if lock(&self.shared).matches() == (0, 0) {
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
                match self.open(&mut generation) {
                    Ok(s) => {
                        params = s.parameters();
                        stated_fps = s.stated_fps();
                        src = Some(s);
                        opened = Instant::now();
                        told_open = false;
                    }
                    Err(why) => {
                        if !told_open && !self.flags.stop.load(Ordering::Relaxed) {
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
            if w > 0 && h > 0 {
                (width, height) = (w, h);
            }
            let fps = Some(rate(&meter, stated_fps)).filter(|&r| r > 0.0).unwrap_or(30.0);
            let (video, preview) = {
                let (v, p) = lock(&self.shared).matches();
                (v > 0, p > 0)
            };

            match u.payload {
                Payload::Picture(p) => {
                    h264 = false;
                    if video || preview {
                        self.feed(&mut children, &p, video, preview, fps, u.time_us);
                    }
                }
                Payload::Coded { codec, data, key } => {
                    h264 = codec == CodecId::H264;
                    // only a URL has a Video then, which carries the same bytes onto the mesh
                    let pass = h264;
                    if video && pass {
                        self.publish(false, data, key, width, height, u.time_us);
                    }
                    if (video && !pass) || preview || width == 0 {
                        // a topic may change its codec at any frame
                        if dec.as_ref().is_some_and(|d| d.codec() != codec) {
                            dec = None;
                            synced = false;
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
                            self.feed(&mut children, f, video && !pass, preview, fps, u.time_us);
                        });
                    } else {
                        synced = false; // frames go by undecoded, so the next decode starts on a keyframe
                    }
                }
            }

            // a new size or codec shows at once, but a source flipping between them never makes a storm
            let since = last_measure.elapsed();
            let shape = (width, height, h264);
            if !probing() && width > 0 && (since > Duration::from_secs(2) || (measured_as != Some(shape) && since > Duration::from_millis(500))) {
                self.measured(width, height, rate(&meter, stated_fps), h264);
                last_measure = Instant::now();
                measured_as = Some(shape);
            }
        }
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
            let s = TopicSource::open(&self.node, &name, self.parent.origin == Origin::Image, self.parent.peer);
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
    fn feed(&self, children: &mut Children, picture: &Frame, to_video: bool, preview: bool, fps: f64, time_us: u64) {
        // the children read it after the source has moved on, so it needs a frame of its own
        let Some(picture) = children.pool.own(picture).map(|f| Arc::new(Picture::new(f))) else { return };
        for (wanted, slot, is_preview) in [(to_video, &mut children.video, false), (preview, &mut children.preview, true)] {
            if wanted {
                let c = slot.get_or_insert_with(|| Child::start(&self.parent.name, is_preview, self.encoder.clone(), self.shared.clone(), self.flags.clone()));
                c.give(Job { picture: picture.clone(), time_us, fps });
            }
        }
    }

    fn publish(&mut self, preview: bool, data: &[u8], key: bool, width: u32, height: u32, time_us: u64) {
        publish(&self.shared, &mut self.out, preview, data, key, width, height, time_us);
    }

    /// Records the source's shape and makes or drops the children it calls for.
    fn measured(&self, width: u32, height: u32, fps: f64, h264: bool) {
        let mut s = lock(&self.shared);
        let changed = s.shape.is_none_or(|o| o.width != width || o.height != height || o.h264 != h264 || (fps - o.fps).abs() > o.fps * 0.2);
        s.shape = Some(Shape { width, height, fps, h264 });
        // a copy of what is on the mesh already changes nothing, so it is never published
        let video = self.parent.origin != Origin::VideoFrame || !h264;
        let preview = needs_preview(width, height);
        let name = &self.parent.name;
        child(&self.node, &mut s.video, video, name, "/Video");
        child(&self.node, &mut s.preview, preview, name, "/Preview");
        if !changed {
            return;
        }
        let children = match (video, preview) {
            (true, true) => "republished as Video and Preview",
            (true, false) => "republished as Video",
            (false, true) => "republished as Preview",
            (false, false) => "nothing to republish",
        };
        info!("{name}: {width}x{height} at {fps:.1} fps{}, {children}", if h264 { " H264" } else { "" });
    }
}

/// Sends one H264 access unit on a child, when someone watches it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish(shared: &Mutex<Shared>, out: &mut VideoFrame, preview: bool, data: &[u8], key: bool, width: u32, height: u32, time_us: u64) {
    out.width = width;
    out.height = height;
    out.keyframe = key;
    out.pts = Timestamp { us: time_us as i64 };
    out.data.clear();
    out.data.extend_from_slice(data);
    let s = lock(shared);
    let child = if preview { &s.preview } else { &s.video };
    if let Some(p) = child.as_ref().filter(|p| p.match_count() > 0) {
        let _ = p.send_captured(out, out.pts);
    }
}
