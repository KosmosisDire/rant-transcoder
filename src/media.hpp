#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <string>
#include <vector>

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/frame.h>
}

struct SwsContext;

namespace transcoder {

/* One coded H264 access unit out of an encoder, Annex B. */
struct Coded {
    const uint8_t* data;
    size_t size;
    bool key;
};

/* Decodes one codec, in hardware when a device takes it. Pictures come out in system memory. */
class Decoder {
public:
    Decoder() = default;
    ~Decoder();
    Decoder(const Decoder&) = delete;
    Decoder& operator=(const Decoder&) = delete;

    /* par carries a demuxed stream's extradata, null for a Rant topic. */
    bool open(AVCodecID codec, const AVCodecParameters* par);
    void close();
    bool is_open() const { return ctx_ != nullptr; }
    /* Feeds one access unit. out runs for each picture it completes. false on a decode error. */
    bool decode(const uint8_t* data, size_t size, const std::function<void(const AVFrame*)>& out);
    /* The decoder's name, and the device when in hardware. */
    std::string describe() const;

private:
    bool open_with(const AVCodec* codec, AVBufferRef* device, AVPixelFormat hw);
    bool fail();
    static AVPixelFormat pick_format(AVCodecContext* ctx, const AVPixelFormat* list);

    AVCodecContext* ctx_ = nullptr;
    AVCodecParameters* par_ = nullptr;
    AVPacket* pkt_ = nullptr;
    AVFrame* frame_ = nullptr;
    AVFrame* sw_ = nullptr;
    AVPixelFormat hw_ = AV_PIX_FMT_NONE;
    AVCodecID codec_ = AV_CODEC_ID_NONE;
    int errors_ = 0;
};

/* Converts pictures to one size and format. The result lives until the next convert. */
class Scaler {
public:
    Scaler() = default;
    ~Scaler();
    Scaler(const Scaler&) = delete;
    Scaler& operator=(const Scaler&) = delete;

    AVFrame* convert(const AVFrame* src, int width, int height, AVPixelFormat format);

private:
    SwsContext* sws_ = nullptr;
    AVFrame* out_ = nullptr;
};

/* An H264 encoder set for low latency: no B frames, no lookahead, in band parameter sets. */
class Encoder {
public:
    Encoder() = default;
    ~Encoder();
    Encoder(const Encoder&) = delete;
    Encoder& operator=(const Encoder&) = delete;

    /* only names one FFmpeg encoder, empty = the first of the platform's list that opens:
     * the hardware ones, then openh264. A full NVENC falls through to the next. */
    bool open(int width, int height, double fps, const std::string& only);
    void close();
    bool is_open() const { return ctx_ != nullptr; }
    int width() const { return ctx_ ? ctx_->width : 0; }
    int height() const { return ctx_ ? ctx_->height : 0; }
    AVPixelFormat format() const { return ctx_ ? ctx_->pix_fmt : AV_PIX_FMT_NONE; }
    const char* name() const { return ctx_ ? ctx_->codec->name : ""; }
    /* picture is at the encoder's size and format. keyframe forces an IDR. */
    void encode(AVFrame* picture, uint64_t time_us, bool keyframe, const std::function<void(const Coded&)>& out);

private:
    void emit(const uint8_t* data, size_t size, bool key, const std::function<void(const Coded&)>& out);

    AVCodecContext* ctx_ = nullptr;
    AVPacket* pkt_ = nullptr;
    int64_t last_pts_ = INT64_MIN;
    std::vector<uint8_t> params_;   /* the latest SPS and PPS, put in front of a keyframe that lacks them */
    std::vector<uint8_t> joined_;
};

/* Points frame at raw image bytes without copying. stride 0 = tightly packed rows. false
 * when size cannot hold the picture. */
bool wrap_picture(AVFrame* frame, const uint8_t* data, size_t size, int width, int height, int stride,
                  AVPixelFormat format);

/* True when an H264 or H265 access unit holds an IDR or IRAP picture. */
bool has_idr(AVCodecID codec, const uint8_t* data, size_t size);

/* The H264 bit rate for a size and rate, about 0.08 bits a pixel. */
int64_t bitrate_for(int width, int height, double fps);

std::string av_error(int code);

}
