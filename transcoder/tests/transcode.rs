//! The transcoder end to end in one process. A camera node publishes raw pictures, H264 and
//! MJPEG frames and a stream URL, and a viewer checks which children exist and what they carry.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{CHILDREN, Camera, DOMAIN, LOOPBACK, Viewer};
use rant_transcoder::{Options, Transcoder, av};

#[test]
fn children_carry_what_changes() {
    // the transcoder's own lines, shown when the test fails
    let _ = tracing_subscriber::fmt().with_env_filter("info").with_test_writer().try_init();
    av::set_log_level(false);
    let camera = Camera::start();

    // the transcoder, opened under a prefix of its own, which the names it reads from the mesh skip
    // SAFETY: no other thread of this test reads the environment
    unsafe { std::env::set_var("RANT_PREFIX", "Elsewhere") };
    let mut t = Transcoder::new(Options { domain: DOMAIN, encoder: None, interface: Some(LOOPBACK.into()) }).unwrap();
    unsafe { std::env::remove_var("RANT_PREFIX") };
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let transcoder = std::thread::spawn(move || t.run(|| stop.load(Ordering::Relaxed)));

    let viewer = Viewer::watch(&CHILDREN.map(|c| c.name));
    std::thread::sleep(Duration::from_secs(16));
    let mut failed = 0;
    for (e, w) in CHILDREN.iter().zip(&viewer.watches) {
        let w = w.lock().unwrap();
        let fps = w.fps(Duration::from_secs(5));
        let ok = if e.size.0 > 0 {
            w.size == e.size && (e.fps.0..=e.fps.1).contains(&fps)
        } else {
            w.fps(Duration::from_secs(16)) == 0.0 && viewer.find(e.name).is_none_or(|f| f.providers == 0)
        };
        println!("{:<4} {:<22} {:>4}x{:<4} {fps:5.1} fps, {} received", if ok { "ok" } else { "FAIL" }, e.name, w.size.0, w.size.1, w.received);
        failed += !ok as u32;
    }

    done.store(true, Ordering::Relaxed);
    transcoder.join().unwrap();
    drop((viewer, camera));
    assert_eq!(failed, 0, "{failed} children did not carry what they must");
}
