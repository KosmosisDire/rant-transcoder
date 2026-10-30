use super::encoder::{self, Encoder, ORDER};
use super::filter::Filter;
use super::hw::{self, Frames, Kind};
use super::{Colors, Frame, Layout, Picture, PixelFormat, Pool, Scaler};
use crate::annexb::{self, Profile};
use crate::ffi;

/// The encoders this build tries, best first.
pub fn encoder_names() -> impl Iterator<Item = &'static str> {
    ORDER.iter().map(|s| s.name)
}

/// One encoder of the list as this host has it.
pub struct Survey {
    pub name: &'static str,
    /// How it takes a picture in system memory and the profile its stream states, None when
    /// it does not open.
    pub opened: Option<(String, Option<Profile>)>,
    /// The device whose pictures it takes in place.
    pub in_place: Option<&'static str>,
}

/// Every encoder of the list, opened here for a 1280x720 picture.
pub fn survey() -> Vec<Survey> {
    let (w, h) = (1280, 720);
    let pixels = vec![128u8; (w * h * 3 / 2) as usize];
    let mut raw = Frame::new();
    let picture = raw.wrap(&pixels, Layout { width: w, height: h, stride: 0, format: PixelFormat::NV12 }).and_then(|p| Pool::default().own(&p)).map(Picture::new);
    ORDER
        .iter()
        .map(|spec| {
            let opened = picture.as_ref().and_then(|p| Output::open(p, w, h, 30.0, Some(spec.name), &[])).map(|mut o| {
                let mut profile = None;
                // some encoders hold the first picture back
                for i in 0..5 {
                    if profile.is_none() && let Some(p) = &picture {
                        o.encode(p, i * 33_333, i == 0, |data, _| profile = profile.or(annexb::profile(data)));
                    }
                }
                (o.describe(), profile)
            });
            Survey { name: spec.name, opened, in_place: spec.device.map(|(kind, _)| hw::name(kind.kind)) }
        })
        .collect()
}

/// The device whose pictures the encoder takes in place, None when it takes none.
pub(super) fn device_for(encoder: &str) -> Option<Kind> {
    encoder::specs(Some(encoder)).find_map(|s| s.device.map(|(kind, _)| kind))
}

/// A child's encoder and what brings pictures to it: a chain on the device they are on when
/// the encoder takes that device's pictures, else the scaler in system memory, and an upload
/// for an encoder that takes a device's pictures alone.
pub struct Output {
    encoder: Encoder,
    path: Path,
    colors: Colors,
    /// The pictures it was opened for were on a device.
    on_device: bool,
}

enum Path {
    System(Scaler),
    Device(Filter, ffi::AVHWDeviceType),
    Upload(Scaler, Frames, ffi::AVHWDeviceType),
}

impl Output {
    /// Opens for pictures like `picture` at a size, with the first encoder that opens, or only
    /// the one named, and none of those `refused`. None when none does.
    pub fn open(picture: &Picture, width: u32, height: u32, fps: f64, only: Option<&str>, refused: &[&str]) -> Option<Output> {
        let specs = || encoder::specs(only).filter(|s| !refused.contains(&s.name));
        let f = picture.frame();
        let colors = Colors::for_child(f);
        let on_device = f.on_device();
        let output = |encoder, path| Some(Output { encoder, path, colors, on_device });
        // a device's pictures stay there with the first encoder that takes them in place
        if let Some(device) = hw::device_of(f) {
            let full = f.raw().color_range == ffi::AVCOL_RANGE_JPEG;
            for spec in specs() {
                let Some((kind, chain)) = spec.device.filter(|(k, _)| k.kind == device) else { continue };
                let filter = match Filter::new(f, &chain(width, height, full)) {
                    Ok(filter) => filter,
                    Err(e) => {
                        tracing::debug!("{} cannot take {} pictures in place: {e}", spec.name, hw::name(kind.kind));
                        continue;
                    }
                };
                if let Some(e) = filter.frames().and_then(|pool| Encoder::open(spec, width, height, fps, colors, Some(&pool))) {
                    return output(e, Path::Device(filter, device));
                }
            }
        }
        for spec in specs() {
            match spec.device {
                Some((kind, _)) if spec.device_only => {
                    let Some(pool) = hw::device(kind).and_then(|d| Frames::new(&d, width, height)) else { continue };
                    if let Some(e) = Encoder::open(spec, width, height, fps, colors, Some(&pool)) {
                        return output(e, Path::Upload(Scaler::default(), pool, kind.kind));
                    }
                }
                _ => {
                    if let Some(e) = Encoder::open(spec, width, height, fps, colors, None) {
                        return output(e, Path::System(Scaler::default()));
                    }
                }
            }
        }
        None
    }

    /// Opens a JPEG encoder for a size at a fixed quantizer, 2 best to 31 worst. It always works
    /// in system memory, where any picture comes down to.
    pub fn open_jpeg(picture: &Picture, width: u32, height: u32, quantizer: u8) -> Option<Output> {
        let encoder = Encoder::open_jpeg(width, height, quantizer)?;
        Some(Output { encoder, path: Path::System(Scaler::default()), colors: Colors::JFIF, on_device: picture.frame().on_device() })
    }

    /// It takes pictures like this one at this size, else it must be opened again.
    pub fn fits(&self, picture: &Picture, width: u32, height: u32) -> bool {
        let f = picture.frame();
        if self.encoder.is_jpeg() {
            return self.encoder.width() == width && self.encoder.height() == height;
        }
        let path = match &self.path {
            Path::Device(filter, _) => filter.takes(f),
            Path::System(_) | Path::Upload(..) => f.on_device() == self.on_device,
        };
        path && self.encoder.width() == width && self.encoder.height() == height && self.colors == Colors::for_child(f)
    }

    /// Encodes a picture. `keyframe` forces an IDR. `out` gets each access unit in Annex B and
    /// whether it is a keyframe. False when the picture does not reach the encoder or the
    /// encoder fails.
    pub fn encode(&mut self, picture: &Picture, time_us: u64, keyframe: bool, mut out: impl FnMut(&[u8], bool)) -> bool {
        let e = &mut self.encoder;
        let (w, h, colors) = (e.width(), e.height(), e.colors());
        match &mut self.path {
            Path::Device(filter, _) => {
                let mut ok = true;
                filter.run(picture.frame(), |f: &mut Frame| ok &= e.encode(f, time_us, keyframe, &mut out)) && ok
            }
            Path::System(scaler) => {
                let format = e.format();
                picture.system().and_then(|src| scaler.convert(src, w, h, format, colors)).is_some_and(|f| e.encode(f, time_us, keyframe, out))
            }
            Path::Upload(scaler, pool, _) => {
                let nv12 = picture.system().and_then(|src| scaler.convert(src, w, h, PixelFormat::NV12, colors));
                nv12.and_then(|f| pool.upload(f)).is_some_and(|mut f| e.encode(&mut f, time_us, keyframe, out))
            }
        }
    }

    /// The encoder's own name, such as h264_nvenc.
    pub fn encoder(&self) -> &'static str {
        self.encoder.name()
    }

    /// The encoder's name, and the device when it takes pictures there.
    pub fn describe(&self) -> String {
        match self.path {
            Path::Device(_, device) => format!("{} on {}", self.encoder.name(), hw::name(device)),
            Path::Upload(_, _, device) => format!("{} on {}, uploaded", self.encoder.name(), hw::name(device)),
            Path::System(_) => self.encoder.name().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av::{CodecId, Decoder};
    use crate::rules::video_size;

    #[test]
    fn every_encoder_makes_constrained_baseline() {
        let found = survey();
        assert!(found.iter().any(|s| s.opened.is_some()), "no encoder opens");
        for s in found {
            let Some((how, profile)) = s.opened else { continue };
            eprintln!("{how}: {profile:?}");
            assert!(profile.is_some_and(Profile::constrained_baseline), "{how} makes {profile:?}");
        }
    }

    #[test]
    fn every_video_size_encodes() {
        for (w, h) in [(1921, 1081), (7680, 4320), (8, 6)] {
            let pixels = vec![128u8; w * h];
            let mut raw = Frame::new();
            let wrapped = raw.wrap(&pixels, Layout { width: w as u32, height: h as u32, stride: 0, format: PixelFormat::GRAY8 }).unwrap();
            let picture = Picture::new(Pool::default().own(&wrapped).unwrap());
            let (vw, vh) = video_size(w as u32, h as u32);
            let mut o = Output::open(&picture, vw, vh, 30.0, None, &[]).unwrap_or_else(|| panic!("{vw}x{vh} opens"));
            let mut units = Vec::new();
            for i in 0..3 {
                assert!(o.encode(&picture, i * 33_000, i == 0, |data, _| units.push(data.to_vec())), "{} encodes {vw}x{vh}", o.describe());
            }
            let mut d = Decoder::software(CodecId::H264, None).unwrap();
            let mut size = None;
            for u in &units {
                d.decode(u, |f| size = size.or(Some((f.width(), f.height()))));
            }
            assert_eq!(size, Some((vw, vh)), "{w}x{h} through {}", o.describe());
        }
    }

    /// A picture of noise, which JPEG's quantizer shows on.
    fn noise(w: usize, h: usize) -> Picture {
        let pixels: Vec<u8> = (0..w * h * 3 / 2).map(|i| (i as u32).wrapping_mul(2654435761).to_le_bytes()[3]).collect();
        let mut raw = Frame::new();
        let wrapped = raw.wrap(&pixels, Layout { width: w as u32, height: h as u32, stride: 0, format: PixelFormat::NV12 }).unwrap();
        Picture::new(Pool::default().own(&wrapped).unwrap())
    }

    fn jpegs(picture: &Picture, w: u32, h: u32, quantizer: u8) -> Vec<(Vec<u8>, bool)> {
        let mut o = Output::open_jpeg(picture, w, h, quantizer).unwrap();
        let mut units = Vec::new();
        for i in 0..3 {
            assert!(o.encode(picture, 5_000_000 - i * 1_000_000, false, |data, key| units.push((data.to_vec(), key))), "times going back encode");
        }
        units
    }

    #[test]
    fn jpeg_is_plain_and_every_picture_stands_alone() {
        let (w, h) = (64, 48);
        let picture = noise(w, h);
        let units = jpegs(&picture, w as u32, h as u32, 5);
        assert_eq!(units.len(), 3);
        for (data, key) in &units {
            assert!(key, "a JPEG is a keyframe");
            // a reader with the least JPEG support manages: one baseline frame, no restart markers
            assert!(data.starts_with(&[0xff, 0xd8]) && data.ends_with(&[0xff, 0xd9]));
            assert!(data.windows(2).any(|b| b == [0xff, 0xc0]), "a baseline frame");
            assert!(!data.windows(2).any(|b| b == [0xff, 0xc2]), "a progressive frame");
            assert!(!data.windows(2).any(|b| b == [0xff, 0xdd]), "a restart interval");
            let mut d = Decoder::software(CodecId::MJPEG, None).unwrap();
            let mut size = None;
            d.decode(data, |f| size = Some((f.width(), f.height())));
            assert_eq!(size, Some((w as u32, h as u32)));
        }
    }

    #[test]
    fn a_higher_quantizer_makes_a_smaller_jpeg() {
        let picture = noise(320, 240);
        let size = |q| jpegs(&picture, 320, 240, q)[0].0.len();
        let (fine, coarse) = (size(5), size(10));
        assert!(coarse < fine * 9 / 10, "quantizer 5 makes {fine} bytes, 10 makes {coarse}");
    }

    #[test]
    fn an_encoder_takes_any_rate_a_source_claims() {
        let picture = noise(320, 240);
        for fps in [0.0, 0.0003, 0.5, 100_000.0] {
            let mut o = Output::open(&picture, 320, 240, fps, None, &[]).unwrap_or_else(|| panic!("opens at {fps} fps"));
            assert!(o.encode(&picture, 0, true, |_, _| {}), "{} encodes at {fps} fps", o.describe());
        }
    }

    #[cfg(windows)]
    #[test]
    fn media_foundation_opens_on_threads_at_once() {
        // 8x6 is too small, so each open fails and closes again, as on a host with no GPU
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    let pixels = vec![128u8; 8 * 6];
                    let mut raw = Frame::new();
                    let wrapped = raw.wrap(&pixels, Layout { width: 8, height: 6, stride: 0, format: PixelFormat::GRAY8 }).unwrap();
                    let picture = Picture::new(Pool::default().own(&wrapped).unwrap());
                    for _ in 0..10 {
                        assert!(Output::open(&picture, 8, 6, 30.0, Some("h264_mf"), &[]).is_none());
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
}
