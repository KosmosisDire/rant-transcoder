//! H264 and H265 access units in Annex B, as Rant's VideoFrame carries them: their NAL units,
//! whether one is a keyframe, and the parameter sets a viewer joining on a keyframe needs.

use std::borrow::Cow;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    H265,
}

/// One NAL unit, its start code included.
pub struct Nal<'a> {
    pub kind: u8,
    pub bytes: &'a [u8],
}

impl<'a> Nal<'a> {
    /// The unit without its start code.
    pub fn body(&self) -> &'a [u8] {
        let bytes = self.bytes;
        if bytes.starts_with(&[0, 0, 0, 1]) { &bytes[4..] } else { &bytes[3..] }
    }
}

/// The NAL units of an Annex B buffer. A start code with nothing after it yields nothing.
pub fn units(codec: Codec, data: &[u8]) -> Units<'_> {
    Units { codec, data, next: find(data, 0) }
}

pub struct Units<'a> {
    codec: Codec,
    data: &'a [u8],
    /// Where the next unit's start code and body begin.
    next: Option<(usize, usize)>,
}

impl<'a> Iterator for Units<'a> {
    type Item = Nal<'a>;

    fn next(&mut self) -> Option<Nal<'a>> {
        loop {
            let (start, body) = self.next?;
            self.next = find(self.data, body);
            let end = self.next.map_or(self.data.len(), |(s, _)| s);
            if body < end {
                let b = self.data[body];
                let kind = match self.codec {
                    Codec::H264 => b & 0x1f,
                    Codec::H265 => (b >> 1) & 0x3f,
                };
                return Some(Nal { kind, bytes: &self.data[start..end] });
            }
        }
    }
}

/// The next start code at or after `from`, and where the unit after it begins.
fn find(data: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..data.len()).find_map(|i| match &data[i..] {
        [0, 0, 1, ..] => Some((i, i + 3)),
        [0, 0, 0, 1, ..] => Some((i, i + 4)),
        _ => None,
    })
}

/// True when the access unit holds an IDR picture, or an IRAP one in H265.
pub fn has_idr(codec: Codec, data: &[u8]) -> bool {
    units(codec, data).any(|n| match codec {
        Codec::H264 => n.kind == 5,
        Codec::H265 => (16..=21).contains(&n.kind),
    })
}

const H264_SPS: u8 = 7;
const H264_PPS: u8 = 8;

/// What an H264 stream says it is in its SPS: its profile, the constraints it keeps and its level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Profile {
    pub idc: u8,
    pub constraints: u8,
    pub level: u8,
}

impl Profile {
    /// Baseline that keeps to its constrained subset, which every WebRTC browser decodes.
    pub fn constrained_baseline(self) -> bool {
        self.idc == 66 && self.constraints & 0x40 != 0
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.idc {
            66 if self.constrained_baseline() => f.write_str("constrained baseline")?,
            66 => f.write_str("baseline")?,
            77 => f.write_str("main")?,
            100 => f.write_str("high")?,
            idc => write!(f, "profile {idc}")?,
        }
        write!(f, " level {}.{}", self.level / 10, self.level % 10)
    }
}

/// The profile the access unit's SPS states, None when it holds none.
pub fn profile(data: &[u8]) -> Option<Profile> {
    let sps = units(Codec::H264, data).find(|n| n.kind == H264_SPS)?;
    match sps.body() {
        [_, idc, constraints, level, ..] => Some(Profile { idc: *idc, constraints: *constraints, level: *level }),
        _ => None,
    }
}

/// The latest SPS and PPS out of an H264 encoder, put in front of a keyframe that lacks them,
/// since some encoders send them only once.
#[derive(Default)]
pub struct ParameterSets {
    latest: Vec<u8>,
}

impl ParameterSets {
    /// The access unit as a viewer joining on it needs it.
    pub fn complete<'a>(&mut self, data: &'a [u8], key: bool) -> Cow<'a, [u8]> {
        if units(Codec::H264, data).any(|n| n.kind == H264_SPS) {
            self.latest.clear();
            for n in units(Codec::H264, data).filter(|n| n.kind == H264_SPS || n.kind == H264_PPS) {
                self.latest.extend_from_slice(n.bytes);
            }
            return Cow::Borrowed(data);
        }
        if !key || self.latest.is_empty() {
            return Cow::Borrowed(data);
        }
        Cow::Owned([self.latest.as_slice(), data].concat())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDR: &[u8] = &[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4];
    const INTER: &[u8] = &[0, 0, 0, 1, 0x41, 9, 9];

    #[test]
    fn splits_units_at_start_codes() {
        let kinds: Vec<(u8, usize)> = units(Codec::H264, IDR).map(|n| (n.kind, n.bytes.len())).collect();
        assert_eq!(kinds, [(7, 7), (8, 5), (5, 5)]);
        assert_eq!(units(Codec::H264, &[0, 0, 1]).count(), 0);
        assert_eq!(units(Codec::H264, &[9, 9]).count(), 0);
    }

    #[test]
    fn finds_keyframes() {
        assert!(has_idr(Codec::H264, IDR));
        assert!(!has_idr(Codec::H264, INTER));
        // an H265 IDR_W_RADL (19) and a TRAIL_R (1)
        assert!(has_idr(Codec::H265, &[0, 0, 1, 19 << 1, 1, 7]));
        assert!(!has_idr(Codec::H265, &[0, 0, 1, 1 << 1, 1, 7]));
    }

    #[test]
    fn reads_the_profile() {
        let p = profile(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0, 0, 1, 0x68, 3]).unwrap();
        assert!(p.constrained_baseline());
        assert_eq!(p.to_string(), "constrained baseline level 3.1");
        assert_eq!(profile(&[0, 0, 1, 0x67, 0x42, 0x00, 0x28]).unwrap().to_string(), "baseline level 4.0");
        assert_eq!(profile(&[0, 0, 1, 0x67, 0x64, 0x08, 0x33]).unwrap().to_string(), "high level 5.1");
        assert_eq!(profile(INTER), None);
        assert_eq!(profile(&[0, 0, 1, 0x67, 0x42]), None);
    }

    #[test]
    fn a_keyframe_without_parameter_sets_gets_the_last_ones() {
        let mut p = ParameterSets::default();
        let bare_idr = &IDR[12..];
        assert_eq!(&*p.complete(bare_idr, true), bare_idr); // none seen yet
        assert_eq!(&*p.complete(IDR, true), IDR);
        assert_eq!(&*p.complete(INTER, false), INTER);
        assert_eq!(&*p.complete(bare_idr, true), IDR);
    }
}
