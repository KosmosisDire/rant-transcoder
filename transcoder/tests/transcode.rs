//! The transcoder end to end in one process. A camera node publishes raw pictures, H264 and
//! MJPEG frames and a stream URL, and a viewer checks which children exist and what they carry.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use common::{CHILDREN, Camera, DOMAIN, Expect, LOOPBACK, MJPEG_CHILDREN, SoloCamera, Viewer};
use rant_transcoder::{Options, Transcoder, av};

/// The tests share one domain, so they take turns.
static TURN: Mutex<()> = Mutex::new(());

/// Taken before anything opens, since the tests' sources share their names.
fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts the transcoder, opened under a prefix of its own, which the names it reads from the
/// mesh skip. It runs until the flag is set.
fn transcoder() -> (Arc<AtomicBool>, JoinHandle<()>) {
    // the transcoder's own lines, shown when the test fails
    let _ = tracing_subscriber::fmt().with_env_filter("info").with_test_writer().try_init();
    av::set_log_level(false);
    // SAFETY: no other thread of this test reads the environment
    unsafe { std::env::set_var("RANT_PREFIX", "Elsewhere") };
    let mut t = Transcoder::new(Options { domain: DOMAIN, encoder: None, interface: Some(LOOPBACK.into()) }).unwrap();
    unsafe { std::env::remove_var("RANT_PREFIX") };
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    (done, std::thread::spawn(move || t.run(|| stop.load(Ordering::Relaxed))))
}

/// Watches children for a while, and checks each carries what it must or does not exist.
fn children_carry(children: &[Expect]) {
    let _turn = turn();
    let camera = Camera::start();
    let (done, transcoder) = transcoder();

    let viewer = Viewer::watch(&children.iter().map(|c| c.name).collect::<Vec<_>>());
    std::thread::sleep(Duration::from_secs(16));
    let mut failed = 0;
    for (e, w) in children.iter().zip(&viewer.watches) {
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

#[test]
fn h264_children_carry_what_changes() {
    children_carry(&CHILDREN);
}

#[test]
fn mjpeg_children_carry_what_changes() {
    children_carry(&MJPEG_CHILDREN);
}

#[test]
fn a_restarted_camera_comes_back() {
    let _turn = turn();
    let mut camera = Some(SoloCamera::start());
    let (done, transcoder) = transcoder();
    let viewer = Viewer::watch(&["Test/Solo/Video"]);
    std::thread::sleep(Duration::from_secs(6));
    // gone for longer than a scan, then back as a new node
    drop(camera.take());
    std::thread::sleep(Duration::from_secs(4));
    camera = Some(SoloCamera::start());
    std::thread::sleep(Duration::from_secs(10));

    let (size, fps) = {
        let w = viewer.watches[0].lock().unwrap();
        (w.size, w.fps(Duration::from_secs(5)))
    };
    done.store(true, Ordering::Relaxed);
    transcoder.join().unwrap();
    drop((viewer, camera));
    assert!(size == (640, 480) && (14.0..=22.0).contains(&fps), "Test/Solo/Video is {}x{} at {fps:.1} fps", size.0, size.1);
}
