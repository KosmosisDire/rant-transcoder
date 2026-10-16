#include "media.hpp"

#include "log.hpp"

#include <algorithm>
#include <cmath>
#include <map>
#include <mutex>

#ifdef __linux__
#include <dlfcn.h>
#endif

extern "C" {
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>
}

namespace transcoder {

namespace {

/* The device types tried for decoding, best first. */
const AVHWDeviceType kDevices[] = {
    AV_HWDEVICE_TYPE_CUDA,
#if defined(_WIN32)
    AV_HWDEVICE_TYPE_D3D11VA,
#elif defined(__APPLE__)
    AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
#else
    AV_HWDEVICE_TYPE_VAAPI,
#endif
};

/* VAAPI, which Quick Sync also runs on under Linux, is usable. The FFmpeg build binds libva
 * lazily and aborts the process when a call finds it missing, so look before asking. */
bool have_vaapi() {
#ifdef __linux__
    static const bool ok = [] {
        for (const char* lib : { "libva.so.2", "libva-drm.so.2" }) {
            void* h = dlopen(lib, RTLD_LAZY | RTLD_LOCAL);
            if (!h) return false;
            dlclose(h);
        }
        return true;
    }();
    return ok;
#else
    return true;
#endif
}

/* One device per type for the whole process, null when the machine has none. */
AVBufferRef* device(AVHWDeviceType type) {
    if (type == AV_HWDEVICE_TYPE_VAAPI && !have_vaapi()) return nullptr;
    static std::mutex mu;
    static std::map<AVHWDeviceType, AVBufferRef*> made;
    std::lock_guard<std::mutex> g(mu);
    auto it = made.find(type);
    if (it != made.end()) return it->second;
    AVBufferRef* d = nullptr;
    if (av_hwdevice_ctx_create(&d, type, nullptr, nullptr, 0) < 0) d = nullptr;
    made[type] = d;
    return d;
}

/* The picture format codec decodes to on a device of type, NONE when it cannot. */
AVPixelFormat device_format(const AVCodec* codec, AVHWDeviceType type) {
    for (int i = 0;; i++) {
        const AVCodecHWConfig* c = avcodec_get_hw_config(codec, i);
        if (!c) return AV_PIX_FMT_NONE;
        if ((c->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) && c->device_type == type) return c->pix_fmt;
    }
}

/* Calls visit(type, unit, size) for each NAL unit of an Annex B buffer, its start code included. */
template <class Visit>
void each_nal(AVCodecID codec, const uint8_t* d, size_t n, Visit visit) {
    auto code = [&](size_t k) -> size_t {
        if (k + 3 <= n && d[k] == 0 && d[k + 1] == 0 && d[k + 2] == 1) return 3;
        if (k + 4 <= n && d[k] == 0 && d[k + 1] == 0 && d[k + 2] == 0 && d[k + 3] == 1) return 4;
        return 0;
    };
    auto type = [&](size_t body) { return codec == AV_CODEC_ID_HEVC ? (d[body] >> 1) & 0x3f : d[body] & 0x1f; };
    size_t start = SIZE_MAX, body = 0;
    for (size_t i = 0; i < n;) {
        size_t c = code(i);
        if (!c) { i++; continue; }
        if (start != SIZE_MAX && body < i) visit(type(body), d + start, i - start);
        start = i;
        body = i + c;
        i += c;
    }
    if (start != SIZE_MAX && body < n) visit(type(body), d + start, n - start);
}

const std::vector<std::string>& encoder_order() {
#if defined(__APPLE__)
    static const std::vector<std::string> names = { "h264_videotoolbox", "libopenh264" };
#elif defined(_WIN32)
    static const std::vector<std::string> names = { "h264_nvenc", "h264_qsv", "h264_amf", "libopenh264" };
#else
    static const std::vector<std::string> names = { "h264_nvenc", "h264_qsv", "libopenh264" };
#endif
    return names;
}

/* Each encoder's low latency settings. Constrained baseline decodes in every browser. */
void tune(const std::string& name, AVDictionary** o) {
    auto set = [&](const char* k, const char* v) { av_dict_set(o, k, v, 0); };
    if (name == "h264_nvenc") {
        set("preset", "p1"); set("tune", "ull"); set("zerolatency", "1"); set("delay", "0");
        set("rc", "cbr"); set("forced-idr", "1"); set("profile", "baseline");
    } else if (name == "h264_qsv") {
        set("preset", "veryfast"); set("async_depth", "1"); set("look_ahead", "0");
        set("forced_idr", "1"); set("profile", "baseline");
    } else if (name == "h264_amf") {
        set("usage", "ultralowlatency"); set("quality", "speed"); set("profile", "constrained_baseline");
    } else if (name == "h264_videotoolbox") {
        set("realtime", "1"); set("profile", "baseline");
    } else if (name == "libopenh264") {
        set("profile", "constrained_baseline"); set("allow_skip_frames", "0"); set("rc_mode", "bitrate");
    }
}

/* NV12 when the encoder takes it, since hardware encoders read it natively, else 4:2:0 planar. */
AVPixelFormat input_format(AVCodecContext* ctx, const AVCodec* codec) {
    const AVPixelFormat* fmts = nullptr;
    int n = 0;
    if (avcodec_get_supported_config(ctx, codec, AV_CODEC_CONFIG_PIX_FORMAT, 0,
                                     reinterpret_cast<const void**>(&fmts), &n) < 0 || !fmts || n == 0)
        return AV_PIX_FMT_YUV420P;
    for (AVPixelFormat want : { AV_PIX_FMT_NV12, AV_PIX_FMT_YUV420P })
        for (int i = 0; i < n; i++)
            if (fmts[i] == want) return want;
    return fmts[0];
}

}

std::string av_error(int code) {
    char buf[AV_ERROR_MAX_STRING_SIZE] = {};
    av_strerror(code, buf, sizeof buf);
    return buf;
}

int64_t bitrate_for(int width, int height, double fps) {
    double bits = double(width) * height * std::max(fps, 1.0) * 0.08;
    return std::clamp<int64_t>(int64_t(bits), 200000, 40000000);
}

bool has_idr(AVCodecID codec, const uint8_t* data, size_t size) {
    bool idr = false;
    each_nal(codec, data, size, [&](int type, const uint8_t*, size_t) {
        if (codec == AV_CODEC_ID_HEVC ? (type >= 16 && type <= 21) : type == 5) idr = true;
    });
    return idr;
}

bool wrap_picture(AVFrame* frame, const uint8_t* data, size_t size, int width, int height, int stride,
                  AVPixelFormat format) {
    if (width <= 0 || height <= 0 || format == AV_PIX_FMT_NONE) return false;
    int line = av_image_get_linesize(format, width, 0);
    if (line <= 0) return false;
    if (stride == 0) stride = line;
    if (stride < line) return false;
    std::fill(std::begin(frame->data), std::end(frame->data), nullptr);
    std::fill(std::begin(frame->linesize), std::end(frame->linesize), 0);
    uint8_t* d = const_cast<uint8_t*>(data);
    size_t luma = size_t(stride) * height;
    if (format == AV_PIX_FMT_NV12) {
        if (size < luma + size_t(stride) * ((height + 1) / 2)) return false;
        frame->data[1] = d + luma;
        frame->linesize[1] = stride;
    } else if (size < luma - stride + line) {
        return false;
    }
    frame->data[0] = d;
    frame->linesize[0] = stride;
    frame->width = width;
    frame->height = height;
    frame->format = format;
    return true;
}

/* ---- Decoder ------------------------------------------------------------------------ */

Decoder::~Decoder() {
    close();
}

void Decoder::close() {
    avcodec_free_context(&ctx_);
    avcodec_parameters_free(&par_);
    av_packet_free(&pkt_);
    av_frame_free(&frame_);
    av_frame_free(&sw_);
    hw_ = AV_PIX_FMT_NONE;
    errors_ = 0;
}

bool Decoder::open(AVCodecID codec, const AVCodecParameters* par) {
    close();
    codec_ = codec;
    if (par) {
        par_ = avcodec_parameters_alloc();
        avcodec_parameters_copy(par_, par);
    }
    const AVCodec* native = avcodec_find_decoder(codec);
    if (native)
        for (AVHWDeviceType type : kDevices) {
            AVPixelFormat fmt = device_format(native, type);
            AVBufferRef* dev = fmt == AV_PIX_FMT_NONE ? nullptr : device(type);
            if (dev && open_with(native, dev, fmt)) return true;
        }
    /* FFmpeg's own AV1 decoder needs a device, dav1d decodes in software */
    const AVCodec* sw = codec == AV_CODEC_ID_AV1 ? avcodec_find_decoder_by_name("libdav1d") : native;
    return sw && open_with(sw, nullptr, AV_PIX_FMT_NONE);
}

AVPixelFormat Decoder::pick_format(AVCodecContext* ctx, const AVPixelFormat* list) {
    auto* self = static_cast<Decoder*>(ctx->opaque);
    for (const AVPixelFormat* p = list; *p != AV_PIX_FMT_NONE; p++)
        if (*p == self->hw_) return *p;
    for (const AVPixelFormat* p = list; *p != AV_PIX_FMT_NONE; p++)
        if (!(av_pix_fmt_desc_get(*p)->flags & AV_PIX_FMT_FLAG_HWACCEL)) return *p;
    return AV_PIX_FMT_NONE;
}

bool Decoder::open_with(const AVCodec* codec, AVBufferRef* dev, AVPixelFormat hw) {
    avcodec_free_context(&ctx_);
    ctx_ = avcodec_alloc_context3(codec);
    if (par_) avcodec_parameters_to_context(ctx_, par_);
    /* frame threads hold pictures back a frame each, slice threads add no delay */
    ctx_->thread_type = FF_THREAD_SLICE;
    ctx_->flags |= AV_CODEC_FLAG_LOW_DELAY;
    hw_ = AV_PIX_FMT_NONE;
    if (dev) {
        ctx_->hw_device_ctx = av_buffer_ref(dev);
        ctx_->opaque = this;
        ctx_->get_format = &Decoder::pick_format;
        hw_ = hw;
    }
    AVDictionary* o = nullptr;
    av_dict_set(&o, "max_frame_delay", "1", 0);   /* dav1d's own low latency switch */
    int r = avcodec_open2(ctx_, codec, &o);
    av_dict_free(&o);
    if (r < 0) {
        avcodec_free_context(&ctx_);
        hw_ = AV_PIX_FMT_NONE;
        return false;
    }
    if (!pkt_) pkt_ = av_packet_alloc();
    if (!frame_) frame_ = av_frame_alloc();
    if (!sw_) sw_ = av_frame_alloc();
    errors_ = 0;
    return true;
}

bool Decoder::fail() {
    /* a device that keeps failing (a profile it lacks) gives way to software */
    if (hw_ != AV_PIX_FMT_NONE && ++errors_ >= 30) {
        say("decoder %s keeps failing, switching to software", describe().c_str());
        const AVCodec* sw = codec_ == AV_CODEC_ID_AV1 ? avcodec_find_decoder_by_name("libdav1d")
                                                      : avcodec_find_decoder(codec_);
        if (!sw || !open_with(sw, nullptr, AV_PIX_FMT_NONE)) avcodec_free_context(&ctx_);
    }
    return false;
}

bool Decoder::decode(const uint8_t* data, size_t size, const std::function<void(const AVFrame*)>& out) {
    if (!ctx_ || !size) return false;
    /* not reference counted, so the send copies it into a padded buffer */
    pkt_->data = const_cast<uint8_t*>(data);
    pkt_->size = int(size);
    int r = avcodec_send_packet(ctx_, pkt_);
    pkt_->data = nullptr;
    pkt_->size = 0;
    if (r < 0 && r != AVERROR(EAGAIN)) return fail();
    for (;;) {
        r = avcodec_receive_frame(ctx_, frame_);
        if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) break;
        if (r < 0) return fail();
        const AVFrame* pic = frame_;
        if (hw_ != AV_PIX_FMT_NONE && frame_->format == hw_) {
            av_frame_unref(sw_);
            if (av_hwframe_transfer_data(sw_, frame_, 0) < 0) {
                av_frame_unref(frame_);
                return fail();
            }
            pic = sw_;
        }
        errors_ = 0;
        out(pic);
        av_frame_unref(frame_);
    }
    return true;
}

std::string Decoder::describe() const {
    if (!ctx_) return "closed";
    std::string s = ctx_->codec->name;
    if (hw_ != AV_PIX_FMT_NONE) s += std::string(" on ") + av_get_pix_fmt_name(hw_);
    return s;
}

/* ---- Scaler ------------------------------------------------------------------------- */

Scaler::~Scaler() {
    sws_freeContext(sws_);
    av_frame_free(&out_);
}

AVFrame* Scaler::convert(const AVFrame* src, int width, int height, AVPixelFormat format) {
    if (!out_ || out_->width != width || out_->height != height || out_->format != format) {
        av_frame_free(&out_);
        out_ = av_frame_alloc();
        out_->width = width;
        out_->height = height;
        out_->format = format;
        if (av_frame_get_buffer(out_, 0) < 0) {
            av_frame_free(&out_);
            return nullptr;
        }
    } else if (av_frame_make_writable(out_) < 0) {   /* an encoder may still hold the last one */
        return nullptr;
    }
    sws_ = sws_getCachedContext(sws_, src->width, src->height, AVPixelFormat(src->format),
                                width, height, format, SWS_BILINEAR, nullptr, nullptr, nullptr);
    if (!sws_) return nullptr;
    sws_scale(sws_, src->data, src->linesize, 0, src->height, out_->data, out_->linesize);
    return out_;
}

/* ---- Encoder ------------------------------------------------------------------------ */

Encoder::~Encoder() {
    close();
}

void Encoder::close() {
    avcodec_free_context(&ctx_);
    av_packet_free(&pkt_);
    params_.clear();
    last_pts_ = INT64_MIN;
}

bool Encoder::open(int width, int height, double fps, const std::string& only) {
    close();
    if (fps <= 0) fps = 30;
    std::vector<std::string> names = only.empty() ? encoder_order() : std::vector<std::string>{ only };
    for (const std::string& name : names) {
        const AVCodec* codec = avcodec_find_encoder_by_name(name.c_str());
        if (!codec) continue;
#ifdef __linux__
        if (name == "h264_qsv" && !have_vaapi()) continue;
#endif
        AVCodecContext* ctx = avcodec_alloc_context3(codec);
        ctx->width = width;
        ctx->height = height;
        ctx->time_base = { 1, 1000000 };
        ctx->framerate = av_d2q(fps, 1000);
        ctx->pix_fmt = input_format(ctx, codec);
        ctx->bit_rate = bitrate_for(width, height, fps);
        ctx->gop_size = std::max(1, int(std::lround(fps * 2)));
        ctx->max_b_frames = 0;
        AVDictionary* o = nullptr;
        tune(name, &o);
        int r = avcodec_open2(ctx, codec, &o);
        av_dict_free(&o);
        if (r == 0) {
            ctx_ = ctx;
            pkt_ = av_packet_alloc();
            return true;
        }
        avcodec_free_context(&ctx);
    }
    return false;
}

void Encoder::encode(AVFrame* picture, uint64_t time_us, bool keyframe, const std::function<void(const Coded&)>& out) {
    if (!ctx_) return;
    int64_t pts = int64_t(time_us);
    if (pts <= last_pts_) pts = last_pts_ + 1;
    last_pts_ = pts;
    picture->pts = pts;
    picture->pict_type = keyframe ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    if (avcodec_send_frame(ctx_, picture) < 0) return;
    while (avcodec_receive_packet(ctx_, pkt_) == 0) {
        emit(pkt_->data, size_t(pkt_->size), (pkt_->flags & AV_PKT_FLAG_KEY) != 0, out);
        av_packet_unref(pkt_);
    }
}

void Encoder::emit(const uint8_t* data, size_t size, bool key, const std::function<void(const Coded&)>& out) {
    bool sps = false;
    each_nal(AV_CODEC_ID_H264, data, size, [&](int type, const uint8_t*, size_t) { sps |= type == 7; });
    if (sps) {
        params_.clear();
        each_nal(AV_CODEC_ID_H264, data, size, [&](int type, const uint8_t* unit, size_t n) {
            if (type == 7 || type == 8) params_.insert(params_.end(), unit, unit + n);
        });
    } else if (key && !params_.empty()) {
        /* a viewer that joins on this keyframe needs the parameter sets with it */
        joined_.assign(params_.begin(), params_.end());
        joined_.insert(joined_.end(), data, data + size);
        out({ joined_.data(), joined_.size(), true });
        return;
    }
    out({ data, size, key });
}

}
