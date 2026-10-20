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
    /// the one named. None when none does.
    pub fn open(picture: &Picture, width: u32, height: u32, fps: f64, only: Option<&str>) -> Option<Output> {
        let f = picture.frame();
        let colors = Colors::for_child(f);
        let on_device = f.on_device();
        let output = |encoder, path| Some(Output { encoder, path, colors, on_device });
        // a device's pictures stay there with the first encoder that takes them in place
        if let Some(device) = hw::device_of(f) {
            let full = f.raw().color_range == ffi::AVCOL_RANGE_JPEG;
            for spec in encoder::specs(only) {
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
        for spec in encoder::specs(only) {
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
    /// whether it is a keyframe.
    pub fn encode(&mut self, picture: &Picture, time_us: u64, keyframe: bool, mut out: impl FnMut(&[u8], bool)) {
        let e = &mut self.encoder;
        let (w, h, colors) = (e.width(), e.height(), e.colors());
        match &mut self.path {
            Path::Device(filter, _) => {
                filter.run(picture.frame(), |f: &mut Frame| e.encode(f, time_us, keyframe, &mut out));
            }
            Path::System(scaler) => {
                let format = e.format();
                if let Some(f) = picture.system().and_then(|src| scaler.convert(src, w, h, format, colors)) {
                    e.encode(f, time_us, keyframe, out);
                }
            }
            Path::Upload(scaler, pool, _) => {
                let nv12 = picture.system().and_then(|src| scaler.convert(src, w, h, PixelFormat::NV12, colors));
                if let Some(mut f) = nv12.and_then(|f| pool.upload(f)) {
                    e.encode(&mut f, time_us, keyframe, out);
                }
            }
        }
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
