//! The end to end test's camera and viewer, for measuring a transcoder run as a process of its
//! own with `--domain 151 --interface 127.0.0.1`. It watches every child that must exist for
//! the given seconds, 30 by default, then prints the rate each one reached. A second argument
//! `mjpeg` watches the MJPEG children instead of the H264 ones.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Duration;

use common::{CHILDREN, Camera, MJPEG_CHILDREN, Viewer};

fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let camera = Camera::start();
    let family = if std::env::args().nth(2).is_some_and(|a| a == "mjpeg") { &MJPEG_CHILDREN } else { &CHILDREN };
    let watched: Vec<_> = family.iter().filter(|c| c.size.0 > 0).collect();
    let viewer = Viewer::watch(&watched.iter().map(|c| c.name).collect::<Vec<_>>());
    println!("watching {} children for {seconds} s", watched.len());
    std::thread::sleep(Duration::from_secs(seconds));
    for (c, w) in watched.iter().zip(&viewer.watches) {
        let w = w.lock().unwrap();
        println!("{:<22} {:>4}x{:<4} {:5.1} fps, {} received", c.name, w.size.0, w.size.1, w.fps(Duration::from_secs(10)), w.received);
    }
    drop((viewer, camera));
}
