use super::encoder::{self, Encoder};
use super::filter::Filter;
use super::hw::{self, Frames, Kind};
use super::{Colors, Frame, Picture, PixelFormat, Scaler};
use crate::ffi;

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
                    let Some(pool) = hw::device(kind).and_then(|d| Frames::new(d, width, height)) else { continue };
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

    /// It takes pictures like this one at this size, else it must be opened again.
    pub fn fits(&self, picture: &Picture, width: u32, height: u32) -> bool {
        let f = picture.frame();
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
    use crate::av::{CodecId, Decoder, Layout, Pool};
    use crate::rules::video_size;

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
}
