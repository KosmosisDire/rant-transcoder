//! H264 and H265 access units in Annex B, as Rant's VideoFrame carries them: their NAL units,
//! whether one is a keyframe, and the parameter sets a viewer joining on a keyframe needs.

use std::borrow::Cow;

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
    fn a_keyframe_without_parameter_sets_gets_the_last_ones() {
        let mut p = ParameterSets::default();
        let bare_idr = &IDR[12..];
        assert_eq!(&*p.complete(bare_idr, true), bare_idr); // none seen yet
        assert_eq!(&*p.complete(IDR, true), IDR);
        assert_eq!(&*p.complete(INTER, false), INTER);
        assert_eq!(&*p.complete(bare_idr, true), IDR);
    }
}
