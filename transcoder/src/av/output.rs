use super::encoder::{self, Encoder};
use super::filter::Filter;
use super::hw::{self, Kind};
use super::{Colors, Frame, Picture, Scaler};
use crate::ffi;

/// An encoder that takes a device's pictures in place: the device to decode on, and the chain
/// that brings its pictures to the encoder's size, format and range.
struct InPlace {
    encoder: &'static str,
    device: Kind,
    chain: fn(width: u32, height: u32, full_range: bool) -> String,
}

const IN_PLACE: &[InPlace] = &[
    InPlace {
        encoder: "h264_nvenc",
        device: Kind::CUDA,
        chain: |w, h, full| format!("{}scale_cuda=w={w}:h={h}:format=nv12", if full { "colorspace_cuda=range=tv," } else { "" }),
    },
    #[cfg(windows)]
    InPlace {
        encoder: "h264_qsv",
        device: Kind::D3D11_INTEL,
        chain: |w, h, _| format!("hwmap=derive_device=qsv,format=qsv,vpp_qsv=w={w}:h={h}:format=nv12:out_range=limited"),
    },
];

/// The device whose pictures the encoder takes in place, None when it takes none.
pub(super) fn device_for(encoder: &str) -> Option<Kind> {
    IN_PLACE.iter().find(|p| p.encoder == encoder).map(|p| p.device)
}

/// A child's encoder and what brings pictures to it: a chain on the device they are on when
/// the encoder takes that device's pictures, else the scaler in system memory.
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
}

impl Output {
    /// Opens for pictures like `picture` at a size, with the first encoder that opens, or only
    /// the one named. None when none does.
    pub fn open(picture: &Picture, width: u32, height: u32, fps: f64, only: Option<&str>) -> Option<Output> {
        let f = picture.frame();
        let colors = Colors::for_child(f);
        if let Some(device) = hw::device_of(f) {
            let full = f.raw().color_range == ffi::AVCOL_RANGE_JPEG;
            for name in encoder::names(only) {
                let Some(p) = IN_PLACE.iter().find(|p| p.encoder == name && p.device.kind == device) else { continue };
                let filter = match Filter::new(f, &(p.chain)(width, height, full)) {
                    Ok(filter) => filter,
                    Err(e) => {
                        tracing::debug!("{name} cannot take {} pictures in place: {e}", hw::name(device));
                        continue;
                    }
                };
                if let Some(encoder) = Encoder::open_on(name, filter.frames(), width, height, fps, colors) {
                    return Some(Output { encoder, path: Path::Device(filter, device), colors, on_device: true });
                }
            }
        }
        let encoder = Encoder::open(width, height, fps, colors, only)?;
        Some(Output { encoder, path: Path::System(Scaler::default()), colors, on_device: f.on_device() })
    }

    /// It takes pictures like this one at this size, else it must be opened again.
    pub fn fits(&self, picture: &Picture, width: u32, height: u32) -> bool {
        let f = picture.frame();
        let path = match &self.path {
            Path::Device(filter, _) => filter.takes(f),
            Path::System(_) => f.on_device() == self.on_device,
        };
        path && self.encoder.width() == width && self.encoder.height() == height && self.colors == Colors::for_child(f)
    }

    /// Encodes a picture. `keyframe` forces an IDR. `out` gets each access unit in Annex B and
    /// whether it is a keyframe.
    pub fn encode(&mut self, picture: &Picture, time_us: u64, keyframe: bool, mut out: impl FnMut(&[u8], bool)) {
        let e = &mut self.encoder;
        match &mut self.path {
            Path::Device(filter, _) => {
                filter.run(picture.frame(), |f: &mut Frame| e.encode(f, time_us, keyframe, &mut out));
            }
            Path::System(scaler) => {
                let Some(src) = picture.system() else { return };
                if let Some(f) = scaler.convert(src, e.width(), e.height(), e.format(), e.colors()) {
                    e.encode(f, time_us, keyframe, out);
                }
            }
        }
    }

    /// The encoder's name, and the device when it takes pictures there.
    pub fn describe(&self) -> String {
        match self.path {
            Path::Device(_, device) => format!("{} on {}", self.encoder.name(), hw::name(device)),
            Path::System(_) => self.encoder.name().into(),
        }
    }
}
