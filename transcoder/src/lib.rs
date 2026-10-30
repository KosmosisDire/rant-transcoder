//! The Rant transcoder: one node per host that republishes this host's video as H264 any
//! viewer can play, only where that changes it.

pub mod annexb;
pub mod av;
mod child;
pub mod hosts;
pub mod rules;
pub mod source;
pub mod stream;

#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unsafe_op_in_unsafe_fn, clippy::all)]
mod ffi;

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rant::{Entity, EntityKind, Node, NodeOptions};
use tracing::{info, warn};

use source::Locals;
use stream::{Origin, Parent, Stream};

/// The node's name. Every transcoder's node is skipped as a source, so two never feed each other.
pub const NODE: &str = "transcoder";

pub struct Options {
    /// 0 takes RANT_DOMAIN, else 0.
    pub domain: u16,
    /// One FFmpeg encoder to use, such as h264_nvenc. None takes the best that opens.
    pub encoder: Option<String>,
    /// The multicast interface: None for all, "127.0.0.1" for this host alone.
    pub interface: Option<String>,
}

/// Watches the mesh for video published on this host and keeps a Stream for each.
pub struct Transcoder {
    node: Arc<Node>,
    domain: u16,
    encoder: Option<String>,
    streams: HashMap<String, Stream>,
    locals: Locals,
    /// Counts live workers, each holding a clone.
    workers: Arc<()>,
    epoch: u32,
}

impl Transcoder {
    pub fn new(options: Options) -> Result<Transcoder, rant::Error> {
        let mut o = NodeOptions::new()
            .domain(options.domain)
            // schemas tell an Image from any other topic
            .fetch_details(true)
            // up to five per stream: two children, the parent, a descriptor
            .max_topics(512)
            .max_peers(256)
            // it logs to stdout, and the log topics of a peer on an older Rant never match
            .disable_logs(true);
        if let Some(ip) = &options.interface {
            o = o.multicast_interface(ip);
        }
        let node = Node::new(NODE, o)?;
        // Rant repeats an error for as long as its cause lasts, so each line shows once a minute
        let mut told: HashMap<String, Instant> = HashMap::new();
        node.on_event(move |e| {
            if !e.is_error() {
                return;
            }
            let (text, now) = (e.to_string(), Instant::now());
            if told.len() > 1000 {
                told.clear();
            }
            if told.get(&text).is_some_and(|&t| now - t < Duration::from_secs(60)) {
                return;
            }
            warn!("rant: {text}");
            told.insert(text, now);
        });
        Ok(Transcoder { node: Arc::new(node), domain: options.domain, encoder: options.encoder, streams: HashMap::new(), locals: Locals::default(), workers: Arc::new(()), epoch: 0 })
    }

    /// Runs until `stop` says so, then closes every stream.
    pub fn run(&mut self, stop: impl Fn() -> bool) {
        let Some(_host) = self.wait_for_host(&stop) else { return };
        let mut next_scan = Instant::now();
        while !stop() {
            let now = Instant::now();
            let epoch = self.node.reflection().epoch();
            // the epoch moves on every change, so a busy mesh still scans at most twice a second
            if (epoch != self.epoch && now + Duration::from_millis(1500) >= next_scan) || now >= next_scan {
                self.epoch = epoch;
                self.scan();
                next_scan = now + Duration::from_secs(2);
            }
            for s in self.streams.values_mut() {
                s.tick();
            }
            // everything here is quick, so a slow pass means a stream holds up the others
            let took = now.elapsed();
            if took > Duration::from_secs(1) {
                warn!("the main loop took {took:?}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.streams.clear();
        // workers end in their own time, and one stuck in a driver must not stop the process
        let from = Instant::now();
        while Arc::strong_count(&self.workers) > 1 && from.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until this transcoder is the one serving its domain on this host. None when told to
    /// stop first. The system frees the lock the moment its holder dies, however it dies.
    fn wait_for_host(&self, stop: impl Fn() -> bool) -> Option<Option<File>> {
        let file = match host_lock_file(self.domain) {
            Ok(file) => file,
            Err(e) => {
                warn!("cannot tell whether another transcoder runs on this host: {e}");
                return Some(None);
            }
        };
        let mut told = false;
        while !stop() {
            match file.try_lock() {
                Ok(()) => {
                    if told {
                        info!("the other transcoder went, this one takes over");
                    }
                    return Some(Some(file));
                }
                Err(TryLockError::WouldBlock) => {
                    if !told {
                        info!("another transcoder serves this domain on this host, waiting for it to go");
                    }
                    told = true;
                }
                Err(TryLockError::Error(e)) => {
                    warn!("cannot tell whether another transcoder runs on this host: {e}");
                    return Some(None);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        None
    }

    fn scan(&mut self) {
        let r = self.node.reflection();
        let hosts = hosts::host_addresses();
        let mut found: HashMap<String, Parent> = HashMap::new();
        let mut locals: HashSet<u32> = HashSet::new();
        let mut others: HashSet<String> = HashSet::new();
        for p in r.peers() {
            if !p.active || p.name == NODE {
                continue;
            }
            let local = hosts::address_ip(&p.address).is_some_and(|ip| hosts.contains(&ip));
            if local {
                locals.insert(p.id);
            }
            for e in r.entities(p.id) {
                if e.provides {
                    others.insert(e.name.clone());
                }
                if local && let Some(origin) = origin_of(&e) {
                    // the first local publisher wins
                    found.entry(e.name.clone()).or_insert(Parent { name: e.name, origin });
                }
            }
        }
        *self.locals.lock().unwrap_or_else(|e| e.into_inner()) = locals;
        self.streams.retain(|name, s| {
            let keep = found.get(name) == Some(s.parent());
            if !keep {
                info!("{name}: gone");
            }
            keep
        });
        for (name, parent) in found {
            if !self.streams.contains_key(&name) {
                info!("{name}: found on this host");
                let taken = taken_children(&name, &others);
                self.streams.insert(name, Stream::new(self.node.clone(), parent, self.encoder.clone(), self.locals.clone(), self.workers.clone(), taken));
            }
        }
        for (name, s) in &mut self.streams {
            s.set_taken(taken_children(name, &others));
        }
        // someone waits for a child that is not there, so its source may have changed
        for e in r.mesh() {
            if e.kind == EntityKind::Topic && e.consumers > 0 && e.providers == 0 {
                for suffix in ["/Video", "/Preview"] {
                    if let Some(s) = e.name.strip_suffix(suffix).and_then(|parent| self.streams.get_mut(parent)) {
                        s.asked();
                    }
                }
            }
        }
    }
}

/// Which of a parent's children another node on the mesh publishes.
fn taken_children(parent: &str, others: &HashSet<String>) -> [bool; 2] {
    ["/Video", "/Preview"].map(|suffix| others.contains(&format!("{parent}{suffix}")))
}

/// The file whose lock says who serves a domain on this host, in a folder every user shares, so
/// a service and someone's own run see the same one.
fn host_lock_file(domain: u16) -> std::io::Result<File> {
    let domain = if domain != 0 { domain } else { std::env::var("RANT_DOMAIN").ok().and_then(|d| d.parse().ok()).unwrap_or(0) };
    let dir = std::env::var_os("ProgramData").map_or_else(std::env::temp_dir, PathBuf::from);
    let path = dir.join(format!("rant-transcoder-{domain}.lock"));
    // another user's file opens to read only, which still locks
    OpenOptions::new().create(true).truncate(false).write(true).open(&path).or_else(|_| File::open(&path))
}

/// What an entity gives a stream, None when nothing.
fn origin_of(e: &Entity) -> Option<Origin> {
    if !e.provides || e.name.starts_with("@rant/") {
        return None;
    }
    let schema = e.schema.as_ref()?.name();
    match (e.kind, schema.as_str()) {
        (EntityKind::Topic, "Image") => Some(Origin::Image),
        (EntityKind::Topic, "VideoFrame") => Some(Origin::VideoFrame),
        (EntityKind::Topic, "ExternalVideoStream") => Some(Origin::StreamTopic),
        (EntityKind::Variable, "ExternalVideoStream") => Some(Origin::StreamVariable),
        _ => None,
    }
}
