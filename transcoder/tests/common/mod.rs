//! The camera and the viewer the end to end test and the bench share. Each uses part of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rant::types::{ExternalVideoStream, Timestamp, Uri, VideoCodec, VideoFrame, VideoStreamKind};
use rant::{Bytes, Entity, EntityKind, MessageBuilder, Node, NodeOptions, Qos, VariableDefinition, VariableOptions};
use rant_transcoder::annexb::{self, Codec};
use rant_transcoder::av::{CodecId, Decoder};
use rant_transcoder::source::wall_us;

/// Off domain 0, where the lab mesh runs, and on loopback as Rant's own tests are, so a
/// host's other networks never join in.
pub const DOMAIN: u16 = 151;
pub const LOOPBACK: &str = "127.0.0.1";

pub fn local() -> NodeOptions {
    NodeOptions::new().domain(DOMAIN).multicast_interface(LOOPBACK).max_topics(64)
}

/// One child the viewer watches. A size of 0 means the transcoder must not publish it.
pub struct Expect {
    pub name: &'static str,
    pub size: (u32, u32),
    pub fps: (f64, f64),
}

const fn expect(name: &'static str, width: u32, height: u32, min: f64, max: f64) -> Expect {
    Expect { name, size: (width, height), fps: (min, max) }
}

/// The children each of the camera's sources must and must not get.
pub const CHILDREN: [Expect; 12] = [
    expect("Test/Raw/Video", 1920, 1080, 20.0, 32.0), // raw: Video and a smaller Preview
    expect("Test/Raw/Preview", 1280, 720, 18.0, 26.0),
    expect("Test/Small/Video", 640, 480, 14.0, 22.0), // 480p: a preview would be the same size
    expect("Test/Small/Preview", 0, 0, 0.0, 0.0),
    expect("Test/Big264/Video", 0, 0, 0.0, 0.0), // H264 already: only the smaller Preview
    expect("Test/Big264/Preview", 1280, 720, 18.0, 26.0),
    expect("Test/Small264/Video", 0, 0, 0.0, 0.0), // H264 at 720p: nothing changes, nothing published
    expect("Test/Small264/Preview", 0, 0, 0.0, 0.0),
    expect("Test/Jpeg/Video", 640, 480, 20.0, 32.0), // MJPEG: Video only
    expect("Test/Jpeg/Preview", 0, 0, 0.0, 0.0),
    expect("Test/Cam/Video", 1280, 720, 24.0, 36.0), // a URL: Video brings it onto the mesh
    expect("Test/Cam/Preview", 0, 0, 0.0, 0.0),
];

/// Coded frames to publish in a loop, each with its keyframe flag.
type Clip = Vec<(Vec<u8>, bool)>;

/// Runs the pinned FFmpeg's ffmpeg tool, which makes the clips.
fn ffmpeg(args: &[&str]) {
    let exe = Path::new(env!("TRANSCODER_FFMPEG_DIR")).join("bin").join("ffmpeg");
    let status = Command::new(&exe).args(["-v", "error", "-y"]).args(args).status().expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

fn pattern(width: u32, height: u32) -> String {
    format!("testsrc2=size={width}x{height}:rate=30")
}

/// Ten seconds of a moving pattern as H264 at 30 fps, one access unit each.
fn h264_clip(dir: &Path, width: u32, height: u32) -> Clip {
    let path = dir.join(format!("{width}x{height}.h264"));
    let p = path.to_string_lossy();
    ffmpeg(&["-f", "lavfi", "-i", &pattern(width, height), "-t", "10", "-c:v", "libopenh264", "-b:v", "2M", "-g", "60", "-bsf:v", "h264_metadata=aud=insert", "-f", "h264", &p]);
    let data = std::fs::read(&path).unwrap();
    // an access unit delimiter starts each unit
    let mut starts: Vec<usize> = annexb::units(Codec::H264, &data).filter(|n| n.kind == 9).map(|n| n.bytes.as_ptr() as usize - data.as_ptr() as usize).collect();
    starts.push(data.len());
    starts
        .windows(2)
        .map(|w| {
            let unit = &data[w[0]..w[1]];
            (unit.to_vec(), annexb::has_idr(Codec::H264, unit))
        })
        .collect()
}

/// One second of MJPEG, every frame a keyframe.
fn mjpeg_clip(dir: &Path, width: u32, height: u32) -> Clip {
    let frames = dir.join("jpeg%02d.jpg");
    ffmpeg(&["-f", "lavfi", "-i", &pattern(width, height), "-frames:v", "30", "-c:v", "mjpeg", "-q:v", "5", &frames.to_string_lossy()]);
    (1..=30).map(|i| (std::fs::read(dir.join(format!("jpeg{i:02}.jpg"))).unwrap(), true)).collect()
}

/// A moving gradient, BGR, over some rows.
fn paint(bgr: &mut [u8], width: usize, rows: std::ops::Range<usize>, frame: usize) {
    for y in rows {
        for x in 0..width {
            let p = &mut bgr[(y * width + x) * 3..][..3];
            p.copy_from_slice(&[(x + frame * 4) as u8, (y + frame * 2) as u8, ((x ^ y) + frame) as u8]);
        }
    }
}

fn frame_of(clip: &Clip, i: usize, codec: VideoCodec, width: u32, height: u32, now: Timestamp) -> VideoFrame {
    let (data, keyframe) = &clip[i % clip.len()];
    VideoFrame { codec, width, height, keyframe: *keyframe, pts: now, data: data.clone() }
}

/// A camera node publishing at 30 fps: `Test/Raw` (1080p Bgr8), `Test/Small` (480p Bgr8 at
/// 20 fps), `Test/Big264` (1080p H264), `Test/Small264` (720p H264), `Test/Jpeg` (480p MJPEG)
/// and `Test/Cam`, a variable naming an MPEG-TS file.
pub struct Camera {
    _node: Node,
    _stream: VariableDefinition<ExternalVideoStream>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    dir: PathBuf,
}

impl Camera {
    /// Makes the clips in a folder of its own, then starts publishing.
    pub fn start() -> Camera {
        let dir = std::env::temp_dir().join(format!("rant-transcoder-camera-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (big, small, jpeg) = (h264_clip(&dir, 1920, 1080), h264_clip(&dir, 1280, 720), mjpeg_clip(&dir, 640, 480));
        assert!(big.len() >= 290 && small.len() >= 290 && jpeg.len() == 30, "clips of {} {} {}", big.len(), small.len(), jpeg.len());
        let ts = dir.join("cam.ts");
        // MPEG-TS, the container SRT carries, with the timestamps a stream has
        ffmpeg(&["-f", "lavfi", "-i", &pattern(1280, 720), "-t", "10", "-c:v", "libopenh264", "-b:v", "2M", "-g", "60", "-f", "mpegts", &ts.to_string_lossy()]);

        let node = Node::new("cam", local()).unwrap();
        // Rant's Rust binding encodes a Vec<u8> byte by byte, too slow for raw pictures at 30 fps,
        // so they go as bytes a builder copies at once
        let image_schema = node.schema("Image").unwrap();
        let raw = node.publisher::<Bytes>("Test/Raw", Qos::new().schema(&image_schema)).unwrap();
        let little = node.publisher::<Bytes>("Test/Small", Qos::new().schema(&image_schema)).unwrap();
        let big264 = node.publisher::<VideoFrame>("Test/Big264", Qos::new()).unwrap();
        let small264 = node.publisher::<VideoFrame>("Test/Small264", Qos::new()).unwrap();
        let mjpeg = node.publisher::<VideoFrame>("Test/Jpeg", Qos::new()).unwrap();
        let url = format!("file:{}", ts.to_string_lossy().replace('\\', "/"));
        let desc = ExternalVideoStream {
            kind: VideoStreamKind::Other,
            codec: VideoCodec::H264,
            width: 1280,
            height: 720,
            url: Uri { value: url.as_str().try_into().unwrap() },
            name: "e2e".try_into().unwrap(),
        };
        let stream = node.variable_definition::<ExternalVideoStream>("Test/Cam", VariableOptions::new().initial(desc)).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let picture = |width: u32, height: u32| {
                let mut m = MessageBuilder::new(&image_schema);
                m.set_uint("width", width.into()).unwrap().set_uint("height", height.into()).unwrap().set_enum("format", "Bgr8").unwrap();
                m
            };
            let (mut bigmsg, mut smallmsg) = (picture(1920, 1080), picture(640, 480));
            let mut bigpic = vec![0u8; 1920 * 1080 * 3];
            let mut smallpic = vec![0u8; 640 * 480 * 3];
            paint(&mut smallpic, 640, 0..480, 0);
            smallmsg.set_array("data", &smallpic).unwrap();
            let mut next = Instant::now();
            for i in 0.. {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                // a band of the big picture moves, enough to make the encoder work
                paint(&mut bigpic, 1920, (i * 8 % 1080)..(i * 8 % 1080 + 8), i);
                let now = Timestamp { us: wall_us() as i64 };
                bigmsg.set_array("data", &bigpic).unwrap();
                let _ = raw.send_captured(bigmsg.bytes(), now);
                if i % 3 != 2 {
                    let _ = little.send_captured(smallmsg.bytes(), now); // 20 fps
                }
                let _ = big264.send_captured(&frame_of(&big, i, VideoCodec::H264, 1920, 1080, now), now);
                let _ = small264.send_captured(&frame_of(&small, i, VideoCodec::H264, 1280, 720, now), now);
                let _ = mjpeg.send_captured(&frame_of(&jpeg, i, VideoCodec::Mjpeg, 640, 480, now), now);
                next += Duration::from_micros(33_333);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Camera { _node: node, _stream: stream, stop, thread: Some(thread), dir }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What the viewer saw on one child.
#[derive(Default)]
pub struct Watch {
    decoder: Option<Decoder>,
    synced: bool,
    pub size: (u32, u32),
    pub received: u32,
    decoded: Vec<Instant>,
}

impl Watch {
    fn add(&mut self, f: &VideoFrame) {
        self.received += 1;
        if f.codec != VideoCodec::H264 || (!self.synced && !f.keyframe) {
            return;
        }
        let d = self.decoder.get_or_insert_with(|| Decoder::open(CodecId::H264, None).expect("an H264 decoder"));
        let (size, decoded) = (&mut self.size, &mut self.decoded);
        self.synced = d.decode(&f.data, |p| {
            *size = (p.width(), p.height());
            decoded.push(Instant::now());
        });
    }

    /// Decoded pictures a second over the last window.
    pub fn fps(&self, window: Duration) -> f64 {
        let from = Instant::now() - window;
        self.decoded.iter().filter(|&&t| t >= from).count() as f64 / window.as_secs_f64()
    }
}

/// A viewer node decoding each child it watches on a thread of its own, so a slow decode of
/// one never drops frames of another.
pub struct Viewer {
    node: Node,
    pub watches: Vec<Arc<Mutex<Watch>>>,
    stop: Arc<AtomicBool>,
    readers: Vec<JoinHandle<()>>,
}

impl Viewer {
    pub fn watch(names: &[&str]) -> Viewer {
        let node = Node::new("viewer", local()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let watches: Vec<Arc<Mutex<Watch>>> = names.iter().map(|_| Arc::default()).collect();
        let readers = names
            .iter()
            .zip(&watches)
            .map(|(name, w)| {
                let (w, stopped) = (w.clone(), stop.clone());
                let mut sub = node.pulled_subscriber::<VideoFrame>(name, Qos::new().queue_bytes(16 << 20)).unwrap();
                std::thread::spawn(move || {
                    while !stopped.load(Ordering::Relaxed) {
                        if let Ok(Some(f)) = sub.take(Duration::from_millis(100)) {
                            w.lock().unwrap().add(&f);
                        }
                    }
                })
            })
            .collect();
        Viewer { node, watches, stop, readers }
    }

    /// The topic as the viewer's mesh shows it now.
    pub fn find(&self, name: &str) -> Option<Entity> {
        self.node.reflection().find(EntityKind::Topic, name)
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
    }
}
