//! Tries to break the transcoder, run as a process of its own, so a crash or a hang shows. Each
//! test does one thing a long running service meets, then checks video still plays in time.

mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{DOMAIN, LOOPBACK, Reader, Viewer, ffmpeg, local};
use rant::types::{ExternalVideoStream, Image, ImageFormat, Timestamp, Uri, VideoCodec, VideoFrame, VideoStreamKind};
use rant::{Node, Publisher, Qos, VariableDefinition, VariableOptions};
use rant_transcoder::av::Input;
use rant_transcoder::rules::{needs_preview, video_size};
use rant_transcoder::source::wall_us;

/// The tests share one domain, so they take turns.
static TURN: Mutex<()> = Mutex::new(());

/// How long video may take to appear or come back after a change, on a slow shared machine.
const SOON: Duration = Duration::from_secs(8);

/// The transcoder as a process, killed on drop.
struct Service {
    child: Child,
    log: PathBuf,
}

impl Service {
    fn start() -> Service {
        let log = std::env::temp_dir().join(format!("rant-transcoder-survive-{}-{}.log", std::process::id(), wall_us()));
        let file = std::fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_rant-transcoder"))
            .args(["--domain", &DOMAIN.to_string(), "--interface", LOOPBACK])
            .env("NO_COLOR", "1")
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        Service { child, log }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Fails the test when the process died.
    fn alive(&mut self) {
        if let Some(status) = self.child.try_wait().unwrap() {
            panic!("the transcoder exited with {status}\n{}", self.log());
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Resident memory in MB, threads and handles or open files.
    fn usage(&self) -> (u64, u64, u64) {
        let pid = self.child.id();
        if cfg!(windows) {
            let script = format!("$p=Get-Process -Id {pid}; \"$($p.WorkingSet64) $($p.Threads.Count) $($p.HandleCount)\"");
            let out = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let v: Vec<u64> = text.split_whitespace().map(|n| n.parse().unwrap_or(0)).collect();
            (v[0] >> 20, v[1], v[2])
        } else {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
            let field = |name: &str| status.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(1)).and_then(|n| n.parse().ok()).unwrap_or(0u64);
            (field("VmRSS:") >> 10, field("Threads:"), std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap().count() as u64)
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.kill();
        if std::thread::panicking() {
            eprintln!("--- the transcoder's log\n{}", self.log());
        }
        let _ = std::fs::remove_file(&self.log);
    }
}

fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

type Clip = Vec<(Vec<u8>, bool)>;

/// A folder of clips, removed on drop.
struct Clips(PathBuf);

impl Clips {
    fn new() -> Clips {
        let dir = std::env::temp_dir().join(format!("rant-transcoder-clips-{}-{}", std::process::id(), wall_us()));
        std::fs::create_dir_all(&dir).unwrap();
        Clips(dir)
    }

    /// Two seconds of a moving pattern coded by one encoder, one unit a frame.
    fn coded(&self, encoder: &[&str], width: u32, height: u32) -> Clip {
        let path = self.0.join(format!("{}-{width}x{height}.mkv", encoder[0]));
        let source = format!("testsrc2=size={width}x{height}:rate=30");
        let p = path.to_string_lossy();
        let mut args = vec!["-f", "lavfi", "-i", &source, "-t", "2", "-c:v"];
        args.extend_from_slice(encoder);
        args.push(&p);
        ffmpeg(&args);
        read_clip(&path)
    }

    fn h264(&self, width: u32, height: u32) -> Clip {
        self.coded(&["libopenh264", "-b:v", "2M", "-g", "30"], width, height)
    }

    fn mjpeg(&self, width: u32, height: u32) -> Clip {
        self.coded(&["mjpeg", "-q:v", "5"], width, height)
    }

    fn av1(&self, width: u32, height: u32) -> Clip {
        self.coded(&["libsvtav1", "-preset", "12", "-g", "30"], width, height)
    }

    /// AV1 of 10 bits, whose pictures come out in another layout from every decoder.
    fn av1_10bit(&self, width: u32, height: u32) -> Clip {
        self.coded(&["libsvtav1", "-preset", "12", "-g", "30", "-pix_fmt", "yuv420p10le"], width, height)
    }

    /// One picture as a file of a format, such as png or jpg.
    fn picture(&self, extension: &str, width: u32, height: u32) -> Vec<u8> {
        self.picture_as(extension, "yuvj420p", width, height)
    }

    /// One picture as a file of a format, with its pixels laid out one way.
    fn picture_as(&self, extension: &str, pixels: &str, width: u32, height: u32) -> Vec<u8> {
        let path = self.0.join(format!("picture-{pixels}-{width}x{height}.{extension}"));
        let source = format!("testsrc2=size={width}x{height}:rate=30");
        let pixels = if extension == "png" && pixels == "yuvj420p" { "rgb24" } else { pixels };
        ffmpeg(&["-f", "lavfi", "-i", &source, "-frames:v", "1", "-update", "1", "-pix_fmt", pixels, &path.to_string_lossy()]);
        std::fs::read(path).unwrap()
    }

    /// A transport stream file the transcoder opens as a URL.
    fn ts(&self, name: &str, source: &str, seconds: &str) -> String {
        let path = self.0.join(name);
        ffmpeg(&["-f", "lavfi", "-i", source, "-t", seconds, "-c:v", "libopenh264", "-b:v", "1M", "-g", "30", "-f", "mpegts", &path.to_string_lossy()]);
        format!("file:{}", path.to_string_lossy().replace('\\', "/"))
    }
}

impl Drop for Clips {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_clip(path: &Path) -> Clip {
    let mut input = Input::open(&path.to_string_lossy(), None, &[], Arc::new(|| false)).unwrap();
    let mut clip = Vec::new();
    while let Ok(p) = input.read() {
        clip.push((p.data.to_vec(), p.key));
    }
    assert!(clip.len() > 30, "a clip of {} frames", clip.len());
    clip
}

/// Runs a step per tick on a thread, until dropped.
struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    fn every(period: Duration, mut step: impl FnMut(usize) + Send + 'static) -> Ticker {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut next = Instant::now();
            for i in 0.. {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                step(i);
                next += period;
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Ticker { stop, thread: Some(thread) }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const FRAME: Duration = Duration::from_millis(33);

fn now() -> Timestamp {
    Timestamp { us: wall_us() as i64 }
}

fn raw(width: u32, height: u32) -> Image {
    Image { width, height, stride: 0, format: ImageFormat::Bgr8, data: vec![80; (width * height * 3) as usize] }
}

/// Publishes a raw camera at 30 fps until dropped.
fn raw_camera(node: &Node, name: &str, width: u32, height: u32) -> Ticker {
    let p = node.publisher::<Image>(name, Qos::new()).unwrap();
    let picture = raw(width, height);
    Ticker::every(FRAME, move |_| {
        let _ = p.send_captured(&picture, now());
    })
}

fn frame(clip: &Clip, i: usize, codec: VideoCodec, width: u32, height: u32) -> VideoFrame {
    let (data, keyframe) = &clip[i % clip.len()];
    VideoFrame { codec, width, height, keyframe: *keyframe, pts: now(), data: data.clone() }
}

fn stream(url: &str, kind: VideoStreamKind) -> ExternalVideoStream {
    ExternalVideoStream { kind, codec: VideoCodec::Unknown, width: 0, height: 0, url: Uri { value: url.try_into().unwrap() }, name: "survive".try_into().unwrap() }
}

fn stream_variable(node: &Node, name: &str, url: &str, kind: VideoStreamKind) -> VariableDefinition<ExternalVideoStream> {
    node.variable_definition::<ExternalVideoStream>(name, VariableOptions::new().initial(stream(url, kind))).unwrap()
}

/// How long until pictures come, failing with `what` when they do not come soon.
fn plays(r: &Reader, what: &str) -> Duration {
    r.plays(5, SOON).unwrap_or_else(|| panic!("{}: nothing decoded within {SOON:?} after {what}", r.name))
}

/// An MJPEG over HTTP server that counts connections, as a camera's web server is.
struct JpegServer {
    port: u16,
    stop: Arc<AtomicBool>,
    accepted: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl JpegServer {
    /// Serves `parts` pictures to each connection and closes it, or goes on forever when 0.
    fn start(port: u16, jpeg: Vec<u8>, parts: usize) -> JpegServer {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, accepted, open) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (stopped, a, o) = (stop.clone(), accepted.clone(), open.clone());
        let thread = std::thread::spawn(move || {
            let mut conns = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                let Ok((c, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                a.fetch_add(1, Ordering::Relaxed);
                o.fetch_add(1, Ordering::Relaxed);
                let (jpeg, stopped, o) = (jpeg.clone(), stopped.clone(), o.clone());
                conns.push(std::thread::spawn(move || {
                    serve_jpegs(c, &jpeg, parts, &stopped);
                    o.fetch_sub(1, Ordering::Relaxed);
                }));
            }
            for c in conns {
                let _ = c.join();
            }
        });
        JpegServer { port, stop, accepted, open, thread: Some(thread) }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/video", self.port)
    }

    fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }
}

impl Drop for JpegServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve_jpegs(mut c: TcpStream, jpeg: &[u8], parts: usize, stop: &AtomicBool) {
    let _ = c.set_nonblocking(false);
    let _ = c.set_read_timeout(Some(Duration::from_millis(500)));
    let mut request = [0u8; 2048];
    let _ = c.read(&mut request);
    if c.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n").is_err() {
        return;
    }
    let mut sent = 0;
    while !stop.load(Ordering::Relaxed) && (parts == 0 || sent < parts) {
        let head = format!("--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len());
        if c.write_all(head.as_bytes()).is_err() || c.write_all(jpeg).is_err() || c.write_all(b"\r\n").is_err() {
            return;
        }
        sent += 1;
        std::thread::sleep(FRAME);
    }
}

/// Takes connections and never answers, as a hung camera does.
struct BlackHole {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl BlackHole {
    fn start() -> BlackHole {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut held = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                if let Ok((c, _)) = listener.accept() {
                    held.push(c);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        BlackHole { port, stop, thread: Some(thread) }
    }
}

impl Drop for BlackHole {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sends one phase's items at 30 fps, moved on by the store.
fn phases<T: Send + 'static>(items: Vec<T>, mut send: impl FnMut(&T, usize) + Send + 'static) -> (Arc<AtomicUsize>, Ticker) {
    let phase = Arc::new(AtomicUsize::new(0));
    let at = phase.clone();
    (phase, Ticker::every(FRAME, move |i| send(&items[at.load(Ordering::Relaxed) % items.len()], i)))
}

#[test]
fn a_codec_change_on_a_topic_keeps_playing() {
    let _turn = turn();
    let clips = Clips::new();
    let (w, h) = (1290, 730);
    let (h264, mjpeg, av1, deep) = (clips.h264(w, h), clips.mjpeg(w, h), clips.av1(w, h), clips.av1_10bit(w, h));
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<VideoFrame> = node.publisher("Test/Switch", Qos::new()).unwrap();
    let (phase, _feed) = phases(vec![(VideoCodec::H264, h264), (VideoCodec::Mjpeg, mjpeg), (VideoCodec::Av1, av1), (VideoCodec::Av1, deep)], move |(codec, clip), i| {
        let _ = p.send_captured(&frame(clip, i, *codec, w, h), now());
    });
    let viewer = Viewer::watch(&[]);
    let preview = viewer.read("Test/Switch/Preview");
    plays(&preview, "the first frames");
    for (next, what) in [(1, "H264 to MJPEG"), (2, "MJPEG to AV1"), (0, "AV1 to H264"), (1, "H264 to MJPEG again"), (3, "MJPEG to 10 bit AV1"), (0, "10 bit AV1 to H264")] {
        phase.store(next, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
        plays(&preview, what);
    }
    service.alive();
}

#[test]
fn an_image_format_change_keeps_playing() {
    let _turn = turn();
    let clips = Clips::new();
    let (w, h) = (640, 480);
    let sized = |format, data: Vec<u8>| Image { width: w, height: h, stride: 0, format, data };
    let pixels = (w * h) as usize;
    let formats = vec![
        ("JPEG", sized(ImageFormat::Jpeg, clips.picture("jpg", w, h))),
        ("PNG", sized(ImageFormat::Png, clips.picture("png", w, h))),
        ("JPEG 4:4:4", sized(ImageFormat::Jpeg, clips.picture_as("jpg", "yuvj444p", w, h))),
        ("gray JPEG", sized(ImageFormat::Jpeg, clips.picture_as("jpg", "gray", w, h))),
        ("PNG with alpha", sized(ImageFormat::Png, clips.picture_as("png", "rgba", w, h))),
        ("16 bit PNG", sized(ImageFormat::Png, clips.picture_as("png", "gray16be", w, h))),
        ("Bgr8", sized(ImageFormat::Bgr8, vec![90; pixels * 3])),
        ("Mono16", sized(ImageFormat::Mono16, vec![90; pixels * 2])),
        ("Monof32", sized(ImageFormat::Monof32, vec![0; pixels * 4])),
        ("Nv12", sized(ImageFormat::Nv12, vec![90; pixels * 3 / 2])),
        ("Yuyv", sized(ImageFormat::Yuyv, vec![90; pixels * 2])),
        ("Rgba8", sized(ImageFormat::Rgba8, vec![90; pixels * 4])),
        ("Mono8", sized(ImageFormat::Mono8, vec![90; pixels])),
    ];
    let names: Vec<&str> = formats.iter().map(|f| f.0).collect();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<Image> = node.publisher("Test/Format", Qos::new()).unwrap();
    let (phase, _feed) = phases(formats, move |(_, image), _| {
        let _ = p.send_captured(image, now());
    });
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Format/Video");
    for (i, name) in names.iter().enumerate() {
        phase.store(i, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        plays(&video, name);
    }
    service.alive();
}

#[test]
fn a_size_change_keeps_playing() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<Image> = node.publisher("Test/Size", Qos::new()).unwrap();
    let sizes = [(640, 480), (1920, 1080), (320, 240), (1281, 721), (641, 479), (16, 16), (2, 2), (3840, 2160), (640, 480)];
    let (phase, _feed) = phases(sizes.iter().map(|&(w, h)| raw(w, h)).collect(), move |image, _| {
        let _ = p.send_captured(image, now());
    });
    let viewer = Viewer::watch(&[]);
    let (video, preview) = (viewer.read("Test/Size/Video"), viewer.read("Test/Size/Preview"));
    for (i, &(w, h)) in sizes.iter().enumerate() {
        phase.store(i, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        plays(&video, &format!("{w}x{h}"));
        let wanted = video_size(w, h);
        // the first pictures may still be the last size
        let from = Instant::now();
        while video.size() != wanted && from.elapsed() < SOON {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(video.size(), wanted, "Video of {w}x{h}");
        if needs_preview(w, h) {
            plays(&preview, &format!("{w}x{h}"));
        }
    }
    service.alive();
}

#[test]
fn time_going_back_keeps_the_preview() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<Image> = node.publisher("Test/Time", Qos::new()).unwrap();
    let picture = raw(1290, 730);
    let offset = Arc::new(Mutex::new(0i64));
    let at = offset.clone();
    let _feed = Ticker::every(FRAME, move |_| {
        let t = wall_us() as i64 + *at.lock().unwrap();
        let _ = p.send_captured(&picture, Timestamp { us: t });
    });
    let viewer = Viewer::watch(&[]);
    let preview = viewer.read("Test/Time/Preview");
    plays(&preview, "the first frames");
    let hour = 3_600_000_000;
    for (to, what) in [(-hour, "an hour back"), (hour, "an hour on"), (-2 * hour, "two hours back")] {
        *offset.lock().unwrap() = to;
        std::thread::sleep(Duration::from_millis(500));
        plays(&preview, what);
    }
    service.alive();
}

#[test]
fn a_looping_timestamp_keeps_the_preview() {
    let _turn = turn();
    let clips = Clips::new();
    let (w, h) = (1290, 730);
    let clip = clips.mjpeg(w, h);
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<VideoFrame> = node.publisher("Test/Loop", Qos::new()).unwrap();
    // a file played in a loop: its frame times run on, start over a minute back, and no capture time
    let _feed = Ticker::every(FRAME, move |i| {
        let mut f = frame(&clip, i, VideoCodec::Mjpeg, w, h);
        f.pts = Timestamp { us: 60_000_000 + (i % 60) as i64 * 33_333 };
        let _ = p.send(&f);
    });
    let viewer = Viewer::watch(&[]);
    let preview = viewer.read("Test/Loop/Preview");
    for _ in 0..5 {
        plays(&preview, "a jump in frame times");
        std::thread::sleep(Duration::from_millis(500));
    }
    service.alive();
}

#[test]
fn garbage_stops_nothing() {
    let _turn = turn();
    let clips = Clips::new();
    let (w, h) = (1290, 730);
    let (h264, jpeg, png) = (clips.h264(w, h), clips.picture("jpg", 640, 480), clips.picture("png", 640, 480));
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let frames: Publisher<VideoFrame> = node.publisher("Test/BadFrames", Qos::new()).unwrap();
    let images: Publisher<Image> = node.publisher("Test/BadImages", Qos::new()).unwrap();
    let viewer = Viewer::watch(&[]);
    let (preview, video) = (viewer.read("Test/BadFrames/Preview"), viewer.read("Test/BadImages/Video"));

    let noise: Vec<u8> = (0..200_000u32).map(|i| i.wrapping_mul(2654435761).to_le_bytes()[3]).collect();
    let bad = |codec, width, height, keyframe, data: Vec<u8>| VideoFrame { codec, width, height, keyframe, pts: now(), data };
    let first = &h264[0].0;
    let bad_frames = [
        bad(VideoCodec::H264, w, h, true, noise.clone()),
        bad(VideoCodec::H264, w, h, false, vec![]),
        bad(VideoCodec::Unknown, w, h, true, noise.clone()),
        bad(VideoCodec::Mjpeg, w, h, true, noise.clone()),
        bad(VideoCodec::H265, w, h, true, noise.clone()),
        bad(VideoCodec::Av1, w, h, true, noise.clone()),
        bad(VideoCodec::H264, u32::MAX, u32::MAX, true, first.clone()),
        bad(VideoCodec::H264, w, h, true, first[..first.len() / 2].to_vec()),
    ];
    // a JPEG that claims to be huge, the size patched in its frame header
    let mut huge = jpeg.clone();
    if let Some(sof) = huge.windows(2).position(|b| b == [0xff, 0xc0]) {
        huge[sof + 5..sof + 9].copy_from_slice(&[0x75, 0x30, 0x75, 0x30]);
    }
    let image = |width, height, stride, format, data: Vec<u8>| Image { width, height, stride, format, data };
    let bad_images = [
        image(65535, 65535, 0, ImageFormat::Bgr8, vec![1; 100]),
        image(u32::MAX, u32::MAX, u32::MAX, ImageFormat::Bgr8, vec![1; 100]),
        image(0, 0, 0, ImageFormat::Bgr8, vec![1; 100]),
        image(640, 480, 1, ImageFormat::Bgr8, vec![1; 640 * 480 * 3]),
        image(640, 480, 0, ImageFormat::Jpeg, noise.clone()),
        image(640, 480, 0, ImageFormat::Png, noise.clone()),
        image(640, 480, 0, ImageFormat::Jpeg, jpeg[..jpeg.len() / 2].to_vec()),
        image(640, 480, 0, ImageFormat::Png, png[..png.len() / 2].to_vec()),
        image(640, 480, 0, ImageFormat::Jpeg, huge),
        image(640, 480, 0, ImageFormat::Bgr8, vec![]),
    ];
    let good_image = image(640, 480, 0, ImageFormat::Jpeg, jpeg);

    // each bad thing in turn while viewers watch, with good frames either side
    for i in 0..bad_frames.len().max(bad_images.len()) {
        let from = Instant::now();
        let mut n = 0;
        while from.elapsed() < Duration::from_millis(1500) {
            if from.elapsed() < Duration::from_millis(1000) {
                let _ = frames.send_captured(&frame(&h264, n, VideoCodec::H264, w, h), now());
                let _ = images.send_captured(&good_image, now());
            } else {
                let _ = frames.send_captured(&bad_frames[i % bad_frames.len()], now());
                let _ = images.send_captured(&bad_images[i % bad_images.len()], now());
            }
            n += 1;
            std::thread::sleep(FRAME);
        }
        service.alive();
    }
    let _feed = Ticker::every(FRAME, move |i| {
        let _ = frames.send_captured(&frame(&h264, i, VideoCodec::H264, w, h), now());
        let _ = images.send_captured(&good_image, now());
    });
    plays(&preview, "garbage frames");
    plays(&video, "garbage images");
    service.alive();
}

#[test]
fn a_new_url_takes_over_from_a_hung_one() {
    let _turn = turn();
    let clips = Clips::new();
    let hole = BlackHole::start();
    let good = clips.ts("good.ts", "testsrc2=size=640x480:rate=30", "4");
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let hung = format!("http://127.0.0.1:{}/video", hole.port);
    let variable = stream_variable(&node, "Test/Hung", &hung, VideoStreamKind::Other);
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Hung/Video");
    std::thread::sleep(Duration::from_secs(3));
    variable.set(&stream(&good, VideoStreamKind::Other)).unwrap();
    let took = video.plays(5, Duration::from_secs(15)).expect("the new URL plays");
    assert!(took < Duration::from_secs(6), "the new URL took {took:?} to play");
    service.alive();
}

#[test]
fn a_url_that_ends_at_once_is_not_hammered() {
    let _turn = turn();
    let clips = Clips::new();
    let jpeg = clips.picture("jpg", 320, 240);
    let server = JpegServer::start(0, jpeg, 2);
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _stream = stream_variable(&node, "Test/Short", &server.url(), VideoStreamKind::HttpMjpeg);
    let viewer = Viewer::watch(&[]);
    let _video = viewer.read("Test/Short/Video");
    std::thread::sleep(Duration::from_secs(10));
    let connects = server.accepted.load(Ordering::Relaxed);
    assert!(connects <= 10, "{connects} connections in 10 s");
    service.alive();
}

#[test]
fn a_url_comes_back_and_closes_with_its_viewer() {
    let _turn = turn();
    let clips = Clips::new();
    let jpeg = clips.picture("jpg", 320, 240);
    let server = JpegServer::start(0, jpeg.clone(), 0);
    let port = server.port;
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _stream = stream_variable(&node, "Test/Web", &server.url(), VideoStreamKind::HttpMjpeg);
    let viewer = Viewer::watch(&[]);
    {
        let video = viewer.read("Test/Web/Video");
        plays(&video, "the first viewer");
        assert_eq!(server.open(), 1, "connections while watched");
    }
    // nobody watches: the connection closes
    let from = Instant::now();
    while server.open() > 0 && from.elapsed() < SOON {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(server.open(), 0, "connections with no viewer");
    let video = viewer.read("Test/Web/Video");
    plays(&video, "a second viewer");
    assert_eq!(server.open(), 1, "connections while watched again");
    // the camera restarts while watched
    drop(server);
    std::thread::sleep(Duration::from_secs(3));
    let server = JpegServer::start(port, jpeg, 0);
    plays(&video, "the server's restart");
    assert_eq!(server.open(), 1, "connections after the restart");
    service.alive();
}

/// The first message on a child, as it was sent.
fn first_frame(viewer: &Viewer, name: &str) -> VideoFrame {
    let mut raw = viewer.node().pulled_subscriber::<VideoFrame>(name, Qos::new()).unwrap();
    let from = Instant::now();
    loop {
        assert!(from.elapsed() < SOON, "no frame on {name}");
        if let Ok(Some(f)) = raw.take(Duration::from_millis(100)) {
            return f;
        }
    }
}

/// The sum of a JPEG's first quantization table, which rises as its quality falls.
fn coarseness(jpeg: &[u8]) -> u32 {
    let at = jpeg.windows(2).position(|b| b == [0xff, 0xdb]).expect("a quantization table");
    // the marker, two bytes of length and one of table id come first
    jpeg[at + 5..at + 69].iter().map(|&v| v as u32).sum()
}

#[test]
fn the_mjpeg_preview_is_smaller_and_coarser_than_the_mjpeg() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Low", 1280, 720);
    let viewer = Viewer::watch(&[]);
    let (full, low) = (first_frame(&viewer, "Test/Low/Mjpeg"), first_frame(&viewer, "Test/Low/MjpegPreview"));
    assert_eq!((full.width, full.height), (1280, 720));
    assert_eq!((low.width, low.height), (640, 360));
    let (full, low) = (coarseness(&full.data), coarseness(&low.data));
    println!("quantization tables sum to {full} and {low}");
    assert!(low >= full * 2, "the preview's table sums to {low}, the full stream's to {full}");
    service.alive();
}

#[test]
fn an_mjpeg_url_comes_onto_the_mesh_as_it_is() {
    let _turn = turn();
    let clips = Clips::new();
    let jpeg = clips.picture("jpg", 320, 240);
    let server = JpegServer::start(0, jpeg.clone(), 0);
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _stream = stream_variable(&node, "Test/Passed", &server.url(), VideoStreamKind::HttpMjpeg);
    let viewer = Viewer::watch(&[]);
    let frame = first_frame(&viewer, "Test/Passed/Mjpeg");
    assert_eq!(frame.codec, VideoCodec::Mjpeg);
    assert!(frame.keyframe);
    assert_eq!((frame.width, frame.height), (320, 240));
    assert_eq!(frame.data, jpeg, "the camera's own JPEG bytes");
    service.alive();
}

#[test]
fn a_jpeg_image_gets_no_mjpeg_copy() {
    let _turn = turn();
    let clips = Clips::new();
    let jpeg = clips.picture("jpg", 640, 480);
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<Image> = node.publisher("Test/JpegImage", Qos::new()).unwrap();
    let image = Image { width: 640, height: 480, stride: 0, format: ImageFormat::Jpeg, data: jpeg };
    let _feed = Ticker::every(FRAME, move |_| {
        let _ = p.send_captured(&image, now());
    });
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/JpegImage/Video");
    plays(&video, "the first pictures");
    std::thread::sleep(Duration::from_secs(3));
    assert!(viewer.find("Test/JpegImage/Mjpeg").is_none_or(|e| e.providers == 0), "an MJPEG copy of JPEG pictures");
    service.alive();
}

#[test]
fn a_topic_silent_at_first_gets_its_children_when_it_starts() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<Image> = node.publisher("Test/Silent", Qos::new()).unwrap();
    let viewer = Viewer::watch(&[]);
    let preview = viewer.read("Test/Silent/Preview");
    // long enough for the first probe to find nothing
    std::thread::sleep(Duration::from_secs(5));
    let picture = raw(1290, 730);
    let _feed = Ticker::every(FRAME, move |_| {
        let _ = p.send_captured(&picture, now());
    });
    let took = preview.plays(5, Duration::from_secs(40)).expect("the preview plays");
    assert!(took < SOON, "the preview took {took:?} after the first picture");
    service.alive();
}

#[test]
fn a_size_change_with_no_viewer_changes_the_children() {
    let _turn = turn();
    let clips = Clips::new();
    let (small, big) = (clips.h264(640, 480), clips.h264(1290, 730));
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<VideoFrame> = node.publisher("Test/Grow", Qos::new()).unwrap();
    let (phase, _feed) = phases(vec![(640, 480, small), (1290, 730, big)], move |(w, h, clip), i| {
        let _ = p.send_captured(&frame(clip, i, VideoCodec::H264, *w, *h), now());
    });
    let viewer = Viewer::watch(&[]);
    std::thread::sleep(Duration::from_secs(8));
    assert!(viewer.find("Test/Grow/Preview").is_none_or(|e| e.providers == 0), "a preview of 480p");
    phase.store(1, Ordering::Relaxed);
    let preview = viewer.read("Test/Grow/Preview");
    let took = preview.plays(5, Duration::from_secs(40)).expect("the preview plays");
    assert!(took < SOON, "the preview took {took:?} to appear");
    service.alive();
}

#[test]
fn one_stuck_stream_slows_no_other() {
    let _turn = turn();
    let clips = Clips::new();
    // a picture every 2 s, which the URL source paces in one long sleep per picture
    let sparse = clips.ts("sparse.ts", "testsrc2=size=320x240:rate=0.5", "20");
    let mut service = Service::start();
    let node = Node::new("cam", local().max_topics(128)).unwrap();
    let _healthy = raw_camera(&node, "Test/Healthy", 640, 480);
    let stuck: Vec<_> = (0..6).map(|i| stream_variable(&node, &format!("Test/Sparse{i}"), &sparse, VideoStreamKind::Other)).collect();
    let viewer = Viewer::watch(&[]);
    let readers: Vec<Reader> = (0..6).map(|i| viewer.read(&format!("Test/Sparse{i}/Video"))).collect();
    let healthy = viewer.read("Test/Healthy/Video");
    plays(&healthy, "the first viewer");
    drop(healthy);
    std::thread::sleep(Duration::from_secs(4)); // the healthy stream's worker stops
    drop(stuck);
    std::thread::sleep(Duration::from_millis(600)); // the scan sees them go, and closes each
    let healthy = viewer.read("Test/Healthy/Video");
    let took = healthy.plays(1, Duration::from_secs(20)).expect("the healthy stream plays");
    assert!(took < Duration::from_secs(5), "a viewer waited {took:?} while other streams closed");
    drop(readers);
    service.alive();
}

#[test]
fn a_killed_transcoder_comes_back() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Kill", 640, 480);
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Kill/Video");
    plays(&video, "the first start");
    for _ in 0..3 {
        service.kill();
        std::thread::sleep(Duration::from_millis(300));
        service = Service::start();
        plays(&video, "a kill and restart");
    }
    service.alive();
}

#[test]
fn a_second_transcoder_does_not_double_a_child() {
    let _turn = turn();
    let mut one = Service::start();
    let mut two = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Two", 640, 480);
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Two/Video");
    plays(&video, "two transcoders");
    std::thread::sleep(Duration::from_secs(4));
    let found = viewer.find("Test/Two/Video").expect("the child");
    assert_eq!(found.providers, 1, "providers of one child");
    one.alive();
    two.alive();
    // the one left takes over when the other goes
    let serving = if one.log().contains("Test/Two: found") { &mut one } else { &mut two };
    serving.kill();
    let took = video.plays(5, Duration::from_secs(30)).expect("the other takes over");
    assert!(took < Duration::from_secs(20), "the other took {took:?} to take over");
}

#[test]
fn a_child_another_node_publishes_is_left_to_it() {
    let _turn = turn();
    let clips = Clips::new();
    let clip = clips.h264(640, 480);
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _raw = raw_camera(&node, "Test/Own", 640, 480);
    let own: Publisher<VideoFrame> = node.publisher("Test/Own/Video", Qos::new()).unwrap();
    let _own = Ticker::every(FRAME, move |i| {
        let _ = own.send_captured(&frame(&clip, i, VideoCodec::H264, 640, 480), now());
    });
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Own/Video");
    plays(&video, "the camera's own child");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(viewer.find("Test/Own/Video").expect("the child").providers, 1, "providers of the camera's own child");
    service.alive();
}

#[test]
fn viewers_come_and_go() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Join", 640, 480);
    let viewer = Viewer::watch(&[]);
    let mut worst = Duration::ZERO;
    for i in 0..12 {
        let video = viewer.read("Test/Join/Video");
        worst = worst.max(plays(&video, "a viewer joined"));
        drop(video);
        // sometimes back at once, sometimes after the worker stopped
        std::thread::sleep(Duration::from_millis([0, 100, 500, 2500][i % 4]));
    }
    println!("the slowest viewer waited {worst:?}");
    assert!(worst < SOON, "a viewer waited {worst:?}");
    service.alive();
}

#[test]
fn many_streams_at_once_all_play() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local().max_topics(128)).unwrap();
    let names: Vec<String> = (0..12).map(|i| format!("Test/Many{i}")).collect();
    let _feeds: Vec<Ticker> = names.iter().map(|n| raw_camera(&node, n, 640, 360)).collect();
    let viewer = Viewer::watch(&[]);
    let readers: Vec<Reader> = names.iter().map(|n| viewer.read(&format!("{n}/Video"))).collect();
    for r in &readers {
        r.plays(5, Duration::from_secs(20)).unwrap_or_else(|| panic!("{} never played", r.name));
    }
    service.alive();
}

#[test]
fn a_flood_of_topics_starves_nothing() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Calm", 640, 480);
    let viewer = Viewer::watch(&[]);
    let calm = viewer.read("Test/Calm/Video");
    plays(&calm, "the first viewer");
    // far more than the transcoder's topic table holds
    let flood = Node::new("flood", local().max_topics(1024)).unwrap();
    let many: Vec<Publisher<Image>> = (0..800).map(|i| flood.publisher(&format!("Flood/T{i}"), Qos::new()).unwrap()).collect();
    std::thread::sleep(Duration::from_secs(5));
    plays(&calm, "a flood of topics");
    drop(many);
    std::thread::sleep(Duration::from_secs(3));
    plays(&calm, "the flood going");
    drop(calm);
    std::thread::sleep(Duration::from_secs(3));
    let calm = viewer.read("Test/Calm/Video");
    plays(&calm, "a viewer after the flood");
    service.alive();
}

#[test]
fn a_rate_limited_viewer_of_mjpeg_gets_whole_pictures() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Capped", 640, 480);
    let viewer = Viewer::watch(&[]);
    let capped = viewer.read_with("Test/Capped/Mjpeg", Qos::new().max_rate_hz(5).queue_bytes(16 << 20));
    plays(&capped, "the first pictures");
    let from = Instant::now();
    std::thread::sleep(Duration::from_secs(4));
    let w = capped.watch.lock().unwrap();
    let fps = w.since(from) as f64 / from.elapsed().as_secs_f64();
    println!("MJPEG capped at 5 Hz: {fps:.1} fps, {} refused of {}", w.errors, w.received);
    assert!((3.0..=6.0).contains(&fps), "MJPEG capped at 5 Hz plays at {fps:.1} fps");
    assert_eq!(w.errors, 0, "every JPEG stands alone, so none may fail");
    service.alive();
}

/// A camera as a process of its own, for `camera_process` below, so it can die without goodbye.
struct CameraProcess(Child);

impl CameraProcess {
    fn start(name: &str) -> CameraProcess {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "camera_process", "--nocapture"])
            .env("SURVIVE_CAMERA", name)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        CameraProcess(child)
    }

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for CameraProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Not a test: what `CameraProcess` runs, until it is killed.
#[test]
#[ignore]
fn camera_process() {
    let Ok(name) = std::env::var("SURVIVE_CAMERA") else { return };
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, &name, 640, 480);
    std::thread::sleep(Duration::from_secs(600));
}

#[test]
fn a_killed_camera_comes_back_before_its_peer_times_out() {
    let _turn = turn();
    let mut service = Service::start();
    let mut camera = CameraProcess::start("Test/Crash");
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Crash/Video");
    plays(&video, "the first start");
    for _ in 0..3 {
        // no goodbye, so the dead peer stays on the mesh for 12 s beside its replacement
        camera.kill();
        std::thread::sleep(Duration::from_millis(500));
        camera = CameraProcess::start("Test/Crash");
        let took = video.plays(5, Duration::from_secs(30)).expect("the restarted camera plays");
        assert!(took < SOON, "the restarted camera took {took:?} to play");
    }
    service.alive();
}

#[test]
fn a_flapping_url_settles_on_the_last() {
    let _turn = turn();
    let clips = Clips::new();
    let jpeg = clips.picture("jpg", 320, 240);
    let (good, other) = (JpegServer::start(0, jpeg.clone(), 0), JpegServer::start(0, jpeg, 0));
    let hole = BlackHole::start();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let variable = stream_variable(&node, "Test/Flap", &good.url(), VideoStreamKind::HttpMjpeg);
    let viewer = Viewer::watch(&[]);
    let video = viewer.read("Test/Flap/Video");
    plays(&video, "the first URL");
    let hung = format!("http://127.0.0.1:{}/video", hole.port);
    let urls = [other.url(), hung, String::new(), "rtsp://127.0.0.1:1/none".into(), "nothing that is a URL".into(), good.url()];
    for i in 0..120 {
        variable.set(&stream(&urls[i % urls.len()], VideoStreamKind::HttpMjpeg)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }
    variable.set(&stream(&other.url(), VideoStreamKind::HttpMjpeg)).unwrap();
    plays(&video, "a storm of URLs");
    // the last URL alone is left connected
    let from = Instant::now();
    while (good.open() != 0 || other.open() != 1) && from.elapsed() < SOON {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!((good.open(), other.open()), (0, 1), "connections to the old URL and the last");
    service.alive();
}

#[test]
fn a_viewer_that_never_reads_slows_no_other() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _feed = raw_camera(&node, "Test/Slow", 1280, 720);
    let (viewer, other) = (Viewer::watch(&[]), Viewer::watch(&[]));
    // subscribes and never takes a message
    let _deaf = other.node().pulled_subscriber::<VideoFrame>("Test/Slow/Video", Qos::new().queue_bytes(1 << 16)).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let video = viewer.read("Test/Slow/Video");
    plays(&video, "a viewer beside a deaf one");
    let fps = video.plays(60, Duration::from_secs(5)).map(|d| 60.0 / d.as_secs_f64());
    assert!(fps.is_some_and(|f| f > 15.0), "the viewer beside a deaf one got {fps:?} fps");
    service.alive();
}

#[test]
fn a_viewer_of_all_four_children_gets_them_all() {
    let _turn = turn();
    let clips = Clips::new();
    let (w, h) = (1920, 1080);
    let clip = clips.h264(w, h);
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let p: Publisher<VideoFrame> = node.publisher("Test/Four", Qos::new()).unwrap();
    let _feed = Ticker::every(FRAME, move |i| {
        let _ = p.send_captured(&frame(&clip, i, VideoCodec::H264, w, h), now());
    });
    let raw = raw_camera(&node, "Test/FourRaw", w, h);
    let viewer = Viewer::watch(&[]);
    // every child of both sources, so a device pool too small to feed them all shows
    let names = ["Test/FourRaw/Video", "Test/FourRaw/Preview", "Test/FourRaw/Mjpeg", "Test/FourRaw/MjpegPreview", "Test/Four/Preview", "Test/Four/Mjpeg", "Test/Four/MjpegPreview"];
    let watching = names.map(|name| viewer.read(name));
    std::thread::sleep(Duration::from_secs(3));
    for r in &watching {
        let fps = r.plays(30, Duration::from_secs(10)).map(|d| 30.0 / d.as_secs_f64());
        println!("{} plays at {fps:.1?} fps", r.name);
        assert!(fps.is_some(), "{} stopped beside the others", r.name);
    }
    drop(raw);
    service.alive();
}

#[test]
fn a_long_name_keeps_the_children_that_fit() {
    let _turn = turn();
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let name = format!("Test/{}", "n".repeat(53));
    let _feed = raw_camera(&node, &name, 1290, 730);
    let viewer = Viewer::watch(&[]);
    let (video, mjpeg) = (viewer.read(&format!("{name}/Video")), viewer.read(&format!("{name}/Mjpeg")));
    plays(&video, "the first pictures");
    plays(&mjpeg, "the first pictures");
    service.alive();
}

#[test]
fn a_transcoder_with_a_hung_url_stops_in_time() {
    let _turn = turn();
    let hole = BlackHole::start();
    let camera = Node::new("cam", local()).unwrap();
    let hung = format!("http://127.0.0.1:{}/video", hole.port);
    let _stream = stream_variable(&camera, "Test/Exit", &hung, VideoStreamKind::Other);
    let _feed = raw_camera(&camera, "Test/ExitRaw", 640, 480);
    let viewer = Viewer::watch(&[]);
    let (_a, _b) = (viewer.read("Test/Exit/Video"), viewer.read("Test/ExitRaw/Video"));
    let done = Arc::new(AtomicBool::new(false));
    let (stop, finished) = (done.clone(), Arc::new(AtomicBool::new(false)));
    let ended = finished.clone();
    let thread = std::thread::spawn(move || {
        let mut t = rant_transcoder::Transcoder::new(rant_transcoder::Options { domain: DOMAIN, encoder: None, interface: Some(LOOPBACK.into()) }).unwrap();
        t.run(|| stop.load(Ordering::Relaxed));
        ended.store(true, Ordering::Relaxed);
    });
    std::thread::sleep(Duration::from_secs(5));
    let from = Instant::now();
    done.store(true, Ordering::Relaxed);
    while !finished.load(Ordering::Relaxed) && from.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(finished.load(Ordering::Relaxed), "the transcoder did not stop within {:?}", from.elapsed());
    println!("stopped in {:?}", from.elapsed());
    thread.join().unwrap();
}

/// An RTSP camera over TCP, as small as FFmpeg accepts, serving a clip in RFC 6184 packets. It
/// counts who plays and who says goodbye.
struct RtspCamera {
    port: u16,
    stop: Arc<AtomicBool>,
    /// Connections that have this many hangs behind them or fewer keep sending.
    hangs: Arc<AtomicUsize>,
    playing: Arc<AtomicUsize>,
    goodbyes: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl RtspCamera {
    fn start(port: u16, clip: Arc<Clip>) -> RtspCamera {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, hangs) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let (playing, goodbyes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (stopped, h, g, pl) = (stop.clone(), hangs.clone(), goodbyes.clone(), playing.clone());
        let thread = std::thread::spawn(move || {
            let mut conns = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                let Ok((c, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let session = RtspSession { clip: clip.clone(), stop: stopped.clone(), hangs: h.clone(), born: h.load(Ordering::Relaxed), playing: pl.clone(), goodbyes: g.clone() };
                conns.push(std::thread::spawn(move || session.serve(c)));
            }
            for c in conns {
                let _ = c.join();
            }
        });
        RtspCamera { port, stop, hangs, playing, goodbyes, thread: Some(thread) }
    }

    fn url(&self) -> String {
        format!("rtsp://127.0.0.1:{}/cam", self.port)
    }

    /// Those connected now stop sending but stay connected, as a hung camera does.
    fn hang(&self) {
        self.hangs.fetch_add(1, Ordering::Relaxed);
    }

    fn playing(&self) -> usize {
        self.playing.load(Ordering::Relaxed)
    }

    /// Waits for this many sessions, failing when they do not settle there.
    fn settles_at(&self, want: usize, what: &str) {
        let from = Instant::now();
        while self.playing() != want && from.elapsed() < SOON {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(self.playing(), want, "RTSP sessions {what}");
        println!("{want} RTSP sessions {what}, in {:?}", from.elapsed());
    }
}

impl Drop for RtspCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct RtspSession {
    clip: Arc<Clip>,
    stop: Arc<AtomicBool>,
    hangs: Arc<AtomicUsize>,
    born: usize,
    playing: Arc<AtomicUsize>,
    goodbyes: Arc<AtomicUsize>,
}

impl RtspSession {
    fn serve(&self, mut c: TcpStream) {
        let _ = c.set_nonblocking(false);
        let _ = c.set_read_timeout(Some(Duration::from_millis(1)));
        let (mut pending, mut started, mut frame, mut packets, mut next) = (Vec::new(), false, 0usize, 0u16, Instant::now());
        let mut chunk = [0u8; 4096];
        while !self.stop.load(Ordering::Relaxed) {
            match c.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => pending.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
            let mut said_bye = false;
            while let Some(request) = next_request(&mut pending) {
                let (method, cseq) = request;
                let answer = self.answer(&method, cseq, c.peer_addr().map(|a| a.port()).unwrap_or(0));
                if c.write_all(answer.as_bytes()).is_err() {
                    said_bye = true;
                }
                match method.as_str() {
                    "PLAY" => {
                        if !started {
                            self.playing.fetch_add(1, Ordering::Relaxed);
                        }
                        started = true;
                    }
                    "TEARDOWN" => {
                        self.goodbyes.fetch_add(1, Ordering::Relaxed);
                        said_bye = true;
                    }
                    _ => {}
                }
            }
            if said_bye {
                break;
            }
            if started && self.hangs.load(Ordering::Relaxed) == self.born && Instant::now() >= next {
                next += FRAME;
                let (unit, _) = &self.clip[frame % self.clip.len()];
                if send_unit(&mut c, unit, frame as u32 * 3000, &mut packets).is_err() {
                    break;
                }
                frame += 1;
            }
        }
        if started {
            self.playing.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn answer(&self, method: &str, cseq: u32, session: u16) -> String {
        let head = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n");
        match method {
            "OPTIONS" => format!("{head}Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER\r\n\r\n"),
            "DESCRIBE" => {
                let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=cam\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1\r\na=control:track0\r\n";
                format!("{head}Content-Base: rtsp://127.0.0.1/cam/\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}", sdp.len())
            }
            "SETUP" => format!("{head}Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nSession: {session}\r\n\r\n"),
            _ => format!("{head}Session: {session}\r\n\r\n"),
        }
    }
}

/// Takes one request off the bytes received, skipping the player's interleaved RTCP. Its method
/// and sequence number.
fn next_request(pending: &mut Vec<u8>) -> Option<(String, u32)> {
    loop {
        if pending.first() == Some(&b'$') {
            let length = 4 + u16::from_be_bytes([*pending.get(2)?, *pending.get(3)?]) as usize;
            if pending.len() < length {
                return None;
            }
            pending.drain(..length);
            continue;
        }
        let end = pending.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let text = String::from_utf8_lossy(&pending[..end]).into_owned();
        pending.drain(..end);
        let method = text.split_whitespace().next()?.to_owned();
        let cseq = text.lines().find_map(|l| l.strip_prefix("CSeq:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        return Some((method, cseq));
    }
}

/// Sends one access unit as interleaved RTP, each NAL whole or in fragments.
fn send_unit(c: &mut TcpStream, unit: &[u8], time: u32, packets: &mut u16) -> std::io::Result<()> {
    const MOST: usize = 1400;
    let nals: Vec<&[u8]> = rant_transcoder::annexb::units(rant_transcoder::annexb::Codec::H264, unit).map(|n| n.body()).collect();
    for (i, nal) in nals.iter().enumerate() {
        let last = i + 1 == nals.len();
        if nal.len() <= MOST {
            send_packet(c, nal, &[], time, last, packets)?;
            continue;
        }
        let (indicator, kind) = ((nal[0] & 0xe0) | 28, nal[0] & 0x1f);
        let parts: Vec<&[u8]> = nal[1..].chunks(MOST).collect();
        for (j, part) in parts.iter().enumerate() {
            let (first, end) = (j == 0, j + 1 == parts.len());
            let header = [indicator, kind | (first as u8) << 7 | (end as u8) << 6];
            send_packet(c, &header, part, time, last && end, packets)?;
        }
    }
    Ok(())
}

fn send_packet(c: &mut TcpStream, head: &[u8], body: &[u8], time: u32, marker: bool, packets: &mut u16) -> std::io::Result<()> {
    let length = (12 + head.len() + body.len()) as u16;
    let mut p = vec![b'$', 0];
    p.extend_from_slice(&length.to_be_bytes());
    p.extend_from_slice(&[0x80, 96 | (marker as u8) << 7]);
    p.extend_from_slice(&packets.to_be_bytes());
    p.extend_from_slice(&time.to_be_bytes());
    p.extend_from_slice(&[1, 2, 3, 4]);
    p.extend_from_slice(head);
    p.extend_from_slice(body);
    *packets = packets.wrapping_add(1);
    c.write_all(&p)
}

#[test]
fn rtsp_sessions_close_and_come_back() {
    let _turn = turn();
    let clips = Clips::new();
    let clip = Arc::new(clips.h264(1290, 730));
    let camera = RtspCamera::start(0, clip.clone());
    let port = camera.port;
    let mut service = Service::start();
    let node = Node::new("cam", local()).unwrap();
    let _stream = stream_variable(&node, "Test/Rtsp", &camera.url(), VideoStreamKind::Rtsp);
    let viewer = Viewer::watch(&[]);

    let video = viewer.read("Test/Rtsp/Video");
    let (mjpeg, preview, small) = (viewer.read("Test/Rtsp/Mjpeg"), viewer.read("Test/Rtsp/Preview"), viewer.read("Test/Rtsp/MjpegPreview"));
    for r in [&video, &mjpeg, &preview, &small] {
        println!("{} played in {:?}", r.name, plays(r, "the first viewer"));
    }
    camera.settles_at(1, "while watched");
    drop((video, mjpeg, preview, small));
    camera.settles_at(0, "once nobody watches");
    assert!(camera.goodbyes.load(Ordering::Relaxed) >= 1, "the closing session said no goodbye");

    let video = viewer.read("Test/Rtsp/Video");
    plays(&video, "a second viewer");
    camera.settles_at(1, "for the second viewer");

    // it hangs while watched: the stall ends the session and another takes over
    camera.hang();
    let took = video.plays(5, Duration::from_secs(20)).expect("plays again after the camera hung");
    assert!(took < Duration::from_secs(10), "took {took:?} to play again after the camera hung");
    println!("played {took:?} after the camera hung");
    camera.settles_at(1, "after the camera hung");

    // it dies while watched and comes back at the same address
    drop(camera);
    std::thread::sleep(Duration::from_secs(3));
    let camera = RtspCamera::start(port, clip);
    println!("played {:?} after the camera restarted", plays(&video, "the camera's restart"));
    camera.settles_at(1, "after the camera restarted");

    drop(video);
    camera.settles_at(0, "at the end");
    service.alive();
}

/// Cameras and viewers coming and going for a while: nothing may grow. Run on demand with
/// `cargo test --release --test survive churn -- --ignored --nocapture`.
#[test]
#[ignore]
fn churn_grows_nothing() {
    let _turn = turn();
    let clips = Clips::new();
    let clip = clips.h264(640, 480);
    let jpeg = clips.picture("jpg", 320, 240);
    let server = JpegServer::start(0, jpeg, 0);
    let hole = BlackHole::start();
    let cycles: usize = std::env::var("CHURN_CYCLES").ok().and_then(|n| n.parse().ok()).unwrap_or(40);
    let mut service = Service::start();
    let viewer = Viewer::watch(&[]);
    let mut settled = None;
    for cycle in 0..cycles {
        let node = Node::new("cam", local()).unwrap();
        let raw = raw_camera(&node, "Test/ChurnRaw", 640, 480);
        let h264: Publisher<VideoFrame> = node.publisher("Test/ChurnFrames", Qos::new()).unwrap();
        let clip = clip.clone();
        let frames = Ticker::every(FRAME, move |i| {
            let _ = h264.send_captured(&frame(&clip, i, VideoCodec::H264, 640, 480), now());
        });
        let url = stream_variable(&node, "Test/ChurnUrl", &server.url(), VideoStreamKind::HttpMjpeg);
        let hung = stream_variable(&node, "Test/ChurnHung", &format!("http://127.0.0.1:{}/video", hole.port), VideoStreamKind::Other);
        let hung_viewer = viewer.read("Test/ChurnHung/Video");
        for child in ["Test/ChurnRaw/Video", "Test/ChurnRaw/Mjpeg", "Test/ChurnFrames/Mjpeg", "Test/ChurnUrl/Video", "Test/ChurnUrl/Mjpeg"] {
            let r = viewer.read(child);
            plays(&r, "a churn cycle");
        }
        drop((raw, frames, url, hung, hung_viewer, node));
        std::thread::sleep(Duration::from_secs(1));
        service.alive();
        let (mb, threads, handles) = service.usage();
        println!("cycle {cycle:3}: {mb} MB, {threads} threads, {handles} handles, {} open connections", server.open());
        if cycle == 5 {
            settled = Some((mb, threads, handles));
        }
    }
    std::thread::sleep(Duration::from_secs(5));
    let (mb, threads, handles) = service.usage();
    let (mb0, threads0, handles0) = settled.unwrap();
    println!("from cycle 5 to the end: {mb0} to {mb} MB, {threads0} to {threads} threads, {handles0} to {handles} handles");
    assert!(mb < mb0 + 150, "memory grew from {mb0} to {mb} MB");
    assert!(threads <= threads0 + 8, "threads grew from {threads0} to {threads}");
    assert!(handles < handles0 + 200, "handles grew from {handles0} to {handles}");
}
