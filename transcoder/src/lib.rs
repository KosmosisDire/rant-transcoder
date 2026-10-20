//! The Rant transcoder: one node per host that republishes this host's video as H264 any
//! viewer can play, only where that changes it.

pub mod annexb;
pub mod av;
pub mod hosts;
pub mod rules;
pub mod source;
pub mod stream;

#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unsafe_op_in_unsafe_fn, clippy::all)]
mod ffi;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rant::{Entity, EntityKind, Node, NodeOptions};
use tracing::{info, warn};

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
    encoder: Option<String>,
    streams: HashMap<String, Stream>,
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
        Ok(Transcoder { node: Arc::new(node), encoder: options.encoder, streams: HashMap::new(), epoch: 0 })
    }

    /// Runs until `stop` says so, then closes every stream.
    pub fn run(&mut self, stop: impl Fn() -> bool) {
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
            std::thread::sleep(Duration::from_millis(100));
        }
        self.streams.clear();
    }

    fn scan(&mut self) {
        let r = self.node.reflection();
        let hosts = hosts::host_addresses();
        let mut found: HashMap<String, Parent> = HashMap::new();
        for p in r.peers() {
            let local = hosts::address_ip(&p.address).is_some_and(|ip| hosts.contains(&ip));
            if !p.active || p.name == NODE || !local {
                continue;
            }
            for e in r.entities(p.id) {
                if let Some(origin) = origin_of(&e) {
                    // the first local publisher wins
                    found.entry(e.name.clone()).or_insert(Parent { name: e.name, origin, peer: p.id });
                }
            }
        }
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
                self.streams.insert(name, Stream::new(self.node.clone(), parent, self.encoder.clone()));
            }
        }
    }
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
