use super::Frame;
use crate::ffi;

/// What a child's pictures stand for and its stream says they do: always limited range, which
/// H264 players assume, with a matrix, primaries and transfer taken from its source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Colors {
    matrix: ffi::AVColorSpace,
    primaries: ffi::AVColorPrimaries,
    transfer: ffi::AVColorTransferCharacteristic,
}

impl Colors {
    /// A YUV source keeps its own colours, so no matrix changes. An RGB or gray one becomes
    /// BT.709, as HD video is.
    pub fn for_child(src: &Frame) -> Colors {
        let f = src.raw();
        if is_yuv(f.format) {
            Colors { matrix: f.colorspace, primaries: f.color_primaries, transfer: f.color_trc }
        } else {
            Colors { matrix: ffi::AVCOL_SPC_BT709, primaries: ffi::AVCOL_PRI_BT709, transfer: ffi::AVCOL_TRC_BT709 }
        }
    }

    pub(super) fn stamp(self, f: &mut ffi::AVFrame) {
        f.color_range = ffi::AVCOL_RANGE_MPEG;
        f.colorspace = self.matrix;
        f.color_primaries = self.primaries;
        f.color_trc = self.transfer;
    }

    pub(super) fn stamp_encoder(self, c: &mut ffi::AVCodecContext) {
        c.color_range = ffi::AVCOL_RANGE_MPEG;
        c.colorspace = self.matrix;
        c.color_primaries = self.primaries;
        c.color_trc = self.transfer;
    }
}

/// Hardware pictures are YUV, as is any format of three or more parts that is not RGB.
fn is_yuv(format: ffi::AVPixelFormat) -> bool {
    // SAFETY: null for an unknown format, else a static table entry
    let Some(d) = (unsafe { ffi::av_pix_fmt_desc_get(format).as_ref() }) else { return false };
    d.flags & ffi::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0 || (d.flags & ffi::AV_PIX_FMT_FLAG_RGB as u64 == 0 && d.nb_components >= 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av::encoder::ORDER;
    use crate::av::{CodecId, Decoder, Layout, Output, Picture, PixelFormat, Pool, Scaler};

    const W: usize = 320;
    const H: usize = 240;

    /// A full range JPEG, black on the left half and white on the right.
    fn black_and_white() -> Vec<u8> {
        // SAFETY: a fresh encoder and frame, used only here and freed before return
        unsafe {
            let codec = ffi::avcodec_find_encoder(ffi::AV_CODEC_ID_MJPEG);
            let mut c = ffi::avcodec_alloc_context3(codec);
            (*c).width = W as _;
            (*c).height = H as _;
            (*c).pix_fmt = ffi::AV_PIX_FMT_YUVJ420P;
            (*c).color_range = ffi::AVCOL_RANGE_JPEG;
            (*c).time_base = ffi::AVRational { num: 1, den: 25 };
            assert_eq!(ffi::avcodec_open2(c, codec, std::ptr::null_mut()), 0);
            let mut pic = Frame::new();
            let f = &mut *pic.as_mut_ptr();
            (f.width, f.height, f.format) = (W as _, H as _, ffi::AV_PIX_FMT_YUVJ420P);
            assert_eq!(ffi::av_frame_get_buffer(f, 0), 0);
            for y in 0..H {
                let row = std::slice::from_raw_parts_mut(f.data[0].add(y * f.linesize[0] as usize), W);
                row.iter_mut().enumerate().for_each(|(x, p)| *p = if x < W / 2 { 0 } else { 255 });
            }
            for plane in 1..3 {
                std::ptr::write_bytes(f.data[plane], 128, f.linesize[plane] as usize * H / 2);
            }
            assert_eq!(ffi::avcodec_send_frame(c, f), 0);
            let packet = super::super::Packet::new();
            assert_eq!(ffi::avcodec_receive_packet(c, packet.as_ptr()), 0);
            let jpeg = packet.data().to_vec();
            ffi::avcodec_free_context(&mut c);
            jpeg
        }
    }

    /// The same as raw gray pixels.
    fn gray() -> Vec<u8> {
        (0..H).flat_map(|_| (0..W).map(|x| if x < W / 2 { 0 } else { 255 })).collect()
    }

    /// Luma at the middle of each half.
    fn levels(f: &Frame) -> (u8, u8) {
        let r = f.raw();
        // SAFETY: a picture in system memory at least W by H
        let at = |x: usize| unsafe { *r.data[0].add(H / 2 * r.linesize[0] as usize + x) };
        (at(W / 4), at(W * 3 / 4))
    }

    fn near(got: (u8, u8), want: (u8, u8)) -> bool {
        got.0.abs_diff(want.0) <= 2 && got.1.abs_diff(want.1) <= 2
    }

    /// Five frames of a picture through an output, as H264 access units.
    fn encode(o: &mut Output, p: &Picture) -> Vec<Vec<u8>> {
        let mut units = Vec::new();
        for i in 0..5 {
            assert!(o.encode(p, i * 40_000, i == 0, |data, _| units.push(data.to_vec())), "{} encodes", o.describe());
        }
        units
    }

    /// The first picture a decoder makes of a stream.
    fn first(d: &mut Decoder, units: &[Vec<u8>]) -> Option<Picture> {
        let (mut pool, mut got) = (Pool::default(), None);
        for u in units {
            d.decode(u, |f| got = got.take().or_else(|| pool.own(f)));
        }
        got.map(Picture::new)
    }

    /// What a viewer sees of H264: the range the stream says, and the levels.
    fn seen(units: &[Vec<u8>]) -> Option<(ffi::AVColorRange, (u8, u8))> {
        let p = first(&mut Decoder::software(CodecId::H264, None)?, units)?;
        Some((p.frame().raw().color_range, levels(p.system()?)))
    }

    #[test]
    fn every_path_keeps_black_and_white() {
        // a limited range H264 source, made from raw gray in system memory
        let pixels = gray();
        let mut raw = Frame::new();
        let wrapped = raw.wrap(&pixels, Layout { width: W as u32, height: H as u32, stride: 0, format: PixelFormat::GRAY8 }).unwrap();
        let picture = Picture::new(Pool::default().own(&wrapped).unwrap());
        let h264 = encode(&mut Output::open(&picture, W as u32, H as u32, 25.0, None, &[]).unwrap(), &picture);

        for (source, codec, units) in [("jpeg", CodecId::MJPEG, vec![black_and_white()]), ("h264", CodecId::H264, h264)] {
            let decoders = [Decoder::software(codec, None), Decoder::open(codec, None, None), Decoder::open(codec, None, Some("h264_qsv"))];
            let mut tried = Vec::new();
            for mut d in decoders.into_iter().flatten() {
                if tried.contains(&d.describe()) {
                    continue;
                }
                tried.push(d.describe());
                let picture = first(&mut d, &units).unwrap_or_else(|| panic!("{} decodes {source}", d.describe()));
                for only in std::iter::once(None).chain(ORDER.iter().map(|s| Some(s.name))) {
                    let Some(mut o) = Output::open(&picture, W as u32, H as u32, 25.0, only, &[]) else { continue };
                    let path = format!("{source} through {} into {}", d.describe(), o.describe());
                    eprintln!("{path}");
                    let (range, got) = seen(&encode(&mut o, &picture)).unwrap_or_else(|| panic!("{path}: the H264 decodes"));
                    // H264 without a stated range is limited, which Media Foundation leaves unsaid
                    assert_ne!(range, ffi::AVCOL_RANGE_JPEG, "{path}: the stream says full range");
                    assert!(near(got, (16, 235)), "{path}: H264 carries {got:?}");
                }
            }
        }
    }

    #[test]
    fn raw_gray_and_rgb_keep_their_levels() {
        for (format, size) in [(PixelFormat::GRAY8, 1), (PixelFormat::BGR24, 3)] {
            let pixels: Vec<u8> = gray().into_iter().flat_map(|v| [v; 3].into_iter().take(size)).collect();
            let mut f = Frame::new();
            let picture = f.wrap(&pixels, Layout { width: W as u32, height: H as u32, stride: 0, format }).unwrap();
            let mut s = Scaler::default();
            let out = s.convert(&picture, W as u32, H as u32, PixelFormat::NV12, Colors::for_child(&picture)).unwrap();
            assert!(near(levels(out), (16, 235)), "{format:?} scales to {:?}", levels(out));
        }
    }
}
