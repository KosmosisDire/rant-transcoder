#include "source.hpp"

#include "media.hpp"

#include <thread>

extern "C" {
#include <libavcodec/bsf.h>
#include <libavformat/avformat.h>
}

namespace transcoder {

using namespace std::chrono;
using rant::types::VideoStreamKind;

uint64_t wall_us() {
    return uint64_t(duration_cast<microseconds>(system_clock::now().time_since_epoch()).count());
}

namespace {

/* Rant's ImageFormat to FFmpeg's, NONE for the compressed ones. */
AVPixelFormat raw_format(uint64_t format) {
    switch (format) {
    case 0: return AV_PIX_FMT_GRAY8;
    case 1: return AV_PIX_FMT_GRAY16LE;
    case 2: return AV_PIX_FMT_RGB24;
    case 3: return AV_PIX_FMT_RGBA;
    case 4: return AV_PIX_FMT_BGR24;
    case 5: return AV_PIX_FMT_YUYV422;
    case 6: return AV_PIX_FMT_NV12;
    case 7: return AV_PIX_FMT_GRAYF32LE;
    default: return AV_PIX_FMT_NONE;
    }
}

/* Rant's VideoCodec to FFmpeg's. */
AVCodecID video_codec(uint64_t codec) {
    switch (codec) {
    case 1: return AV_CODEC_ID_MJPEG;
    case 2: return AV_CODEC_ID_H264;
    case 3: return AV_CODEC_ID_HEVC;
    case 4: return AV_CODEC_ID_AV1;
    default: return AV_CODEC_ID_NONE;
    }
}

class TopicSource : public Source {
public:
    TopicSource(rant::Node& node, const std::string& name, bool image, uint32_t publisher)
        : image_(image), publisher_(publisher) {
        rant::Qos q;
        q.reflect_from_mesh = true;
        /* every coded frame matters to the decoder, so hold a burst. A raw reader keeps only the newest */
        q.queue_bytes = image ? 0 : (16u << 20);
        sub_ = node.subscriber<rant::Bytes>(name, q);
        picture_ = av_frame_alloc();
    }
    ~TopicSource() override { av_frame_free(&picture_); }

    bool valid() const { return sub_.valid(); }

    bool read(Unit& unit, milliseconds timeout) override {
        auto deadline = steady_clock::now() + timeout;
        for (;;) {
            auto left = duration_cast<milliseconds>(deadline - steady_clock::now());
            if (left.count() < 0) left = milliseconds(0);
            auto m = image_ ? sub_.take_latest(left) : sub_.take(left);
            if (!m) return false;
            /* a remote publisher of the same name is skipped, only this host's is ours */
            if (m->publisher_id() == publisher_ && fill(*m, unit)) return true;
            if (steady_clock::now() >= deadline) return false;
        }
    }

private:
    bool fill(const rant::MessageView& m, Unit& u) {
        u = Unit{};
        u.time_us = m.capture_us() ? m.capture_us() : m.written_us() ? m.written_us() : wall_us();
        u.width = int(m.get_uint("width"));
        u.height = int(m.get_uint("height"));
        rant::Bytes d = m.get_array("data");
        if (!d.size()) return false;
        if (image_) {
            uint64_t format = m.get_uint("format");
            if (format == 16 || format == 17) {
                u.codec = format == 16 ? AV_CODEC_ID_MJPEG : AV_CODEC_ID_PNG;
                u.data = d.data();
                u.size = d.size();
                u.key = true;
                return true;
            }
            if (!wrap_picture(picture_, d.data(), d.size(), u.width, u.height, int(m.get_uint("stride")),
                              raw_format(format)))
                return false;
            u.picture = picture_;
            return true;
        }
        u.codec = video_codec(m.get_uint("codec"));
        if (u.codec == AV_CODEC_ID_NONE) return false;
        u.data = d.data();
        u.size = d.size();
        u.key = m.get_bool("keyframe") || u.codec == AV_CODEC_ID_MJPEG || has_idr(u.codec, u.data, u.size);
        int64_t pts = m.get_int("pts");
        if (!m.capture_us() && pts > 0) u.time_us = uint64_t(pts);
        return true;
    }

    bool image_;
    uint32_t publisher_;
    rant::Subscriber<rant::Bytes> sub_;
    AVFrame* picture_ = nullptr;
};

class UrlSource : public Source {
public:
    ~UrlSource() override {
        av_bsf_free(&bsf_);
        av_packet_free(&pkt_);
        avformat_close_input(&fmt_);
    }

    bool open(const std::string& url, VideoStreamKind kind, const std::atomic<bool>& stop, std::string& error) {
        if (kind == VideoStreamKind::WebrtcWhep) {
            error = "WHEP is not supported yet";
            return false;
        }
        stop_ = &stop;
        deadline_ = steady_clock::now() + seconds(10);
        fmt_ = avformat_alloc_context();
        fmt_->interrupt_callback = { &UrlSource::interrupted, this };
        AVDictionary* o = nullptr;
        av_dict_set(&o, "fflags", "nobuffer", 0);
        av_dict_set(&o, "probesize", "1000000", 0);
        av_dict_set(&o, "analyzeduration", "1000000", 0);
        if (kind == VideoStreamKind::Rtsp) av_dict_set(&o, "rtsp_transport", "tcp", 0);
        if (kind == VideoStreamKind::Rtp) av_dict_set(&o, "protocol_whitelist", "file,udp,rtp,crypto,data", 0);
        const AVInputFormat* in = kind == VideoStreamKind::HttpMjpeg ? av_find_input_format("mpjpeg") : nullptr;
        int r = avformat_open_input(&fmt_, url.c_str(), in, &o);
        av_dict_free(&o);
        if (r < 0) {
            error = why(r);
            return false;   /* the failed open freed the context */
        }
        deadline_ = steady_clock::now() + seconds(10);
        if ((r = avformat_find_stream_info(fmt_, nullptr)) < 0) {
            error = why(r);
            return false;
        }
        index_ = av_find_best_stream(fmt_, AVMEDIA_TYPE_VIDEO, -1, -1, nullptr, 0);
        if (index_ < 0) {
            error = "no video stream";
            return false;
        }
        AVStream* st = fmt_->streams[index_];
        codec_ = st->codecpar->codec_id;
        fps_ = av_q2d(st->avg_frame_rate.num ? st->avg_frame_rate : st->r_frame_rate);
        /* MP4, HLS and SRT carry H264 and H265 length prefixed. Annex B passes through */
        const char* filter = codec_ == AV_CODEC_ID_H264 ? "h264_mp4toannexb"
                           : codec_ == AV_CODEC_ID_HEVC ? "hevc_mp4toannexb" : nullptr;
        if (filter) {
            if (av_bsf_alloc(av_bsf_get_by_name(filter), &bsf_) < 0) return false;
            avcodec_parameters_copy(bsf_->par_in, st->codecpar);
            bsf_->time_base_in = st->time_base;
            if ((r = av_bsf_init(bsf_)) < 0) {
                error = av_error(r);
                return false;
            }
        }
        pkt_ = av_packet_alloc();
        return true;
    }

    bool read(Unit& unit, milliseconds) override {
        if (ended_) return false;
        av_packet_unref(pkt_);
        for (;;) {
            if (bsf_) {
                int r = av_bsf_receive_packet(bsf_, pkt_);
                if (r == 0) return fill(unit);
                if (r != AVERROR(EAGAIN)) return end();
            }
            deadline_ = steady_clock::now() + seconds(10);
            if (av_read_frame(fmt_, pkt_) < 0) return end();
            if (pkt_->stream_index != index_) {
                av_packet_unref(pkt_);
                continue;
            }
            if (!bsf_) return fill(unit);
            if (av_bsf_send_packet(bsf_, pkt_) < 0) return end();
        }
    }

    bool ended() const override { return ended_; }
    const AVCodecParameters* parameters() const override {
        return bsf_ ? bsf_->par_out : fmt_->streams[index_]->codecpar;
    }
    double stated_fps() const override { return fps_; }

private:
    static int interrupted(void* self) {
        auto* s = static_cast<UrlSource*>(self);
        if (s->stop_->load()) return 1;
        s->timed_out_ = steady_clock::now() > s->deadline_;
        return s->timed_out_;
    }

    /* FFmpeg reports our own deadline as an exit request, which says nothing to a reader. */
    std::string why(int code) const {
        return timed_out_ ? "no answer within 10 s" : av_error(code);
    }

    bool end() {
        ended_ = true;
        return false;
    }

    /* Maps stream time onto the wall clock from the first packet, and sleeps when a file or a
     * burst runs ahead of it, so units leave at the stream's own pace. */
    bool fill(Unit& u) {
        u = Unit{};
        const AVCodecParameters* par = parameters();
        u.codec = codec_;
        u.data = pkt_->data;
        u.size = size_t(pkt_->size);
        u.key = (pkt_->flags & AV_PKT_FLAG_KEY) != 0 || codec_ == AV_CODEC_ID_MJPEG;
        u.width = par->width;
        u.height = par->height;
        uint64_t now = wall_us();
        int64_t ts = pkt_->dts != AV_NOPTS_VALUE ? pkt_->dts : pkt_->pts;
        if (ts == AV_NOPTS_VALUE) {
            u.time_us = now;
            return true;
        }
        AVRational tb = bsf_ ? bsf_->time_base_out : fmt_->streams[index_]->time_base;
        int64_t us = av_rescale_q(ts, tb, AVRational{ 1, 1000000 });
        int64_t drift = int64_t(anchor_ + uint64_t(us - base_)) - int64_t(now);
        if (!anchored_ || us < last_us_ || drift > 2000000 || drift < -2000000) {
            anchored_ = true;
            anchor_ = now;
            base_ = us;
            drift = 0;
        }
        last_us_ = us;
        if (drift > 0) std::this_thread::sleep_for(microseconds(drift));
        u.time_us = anchor_ + uint64_t(us - base_);
        return true;
    }

    const std::atomic<bool>* stop_ = nullptr;
    steady_clock::time_point deadline_;
    AVFormatContext* fmt_ = nullptr;
    AVBSFContext* bsf_ = nullptr;
    AVPacket* pkt_ = nullptr;
    AVCodecID codec_ = AV_CODEC_ID_NONE;
    int index_ = -1;
    double fps_ = 0;
    bool ended_ = false;
    bool timed_out_ = false;
    bool anchored_ = false;
    uint64_t anchor_ = 0;
    int64_t base_ = 0;
    int64_t last_us_ = 0;
};

}

std::unique_ptr<Source> open_topic(rant::Node& node, const std::string& name, bool image, uint32_t publisher) {
    auto s = std::make_unique<TopicSource>(node, name, image, publisher);
    if (!s->valid()) return nullptr;
    return s;
}

std::unique_ptr<Source> open_url(const std::string& url, VideoStreamKind kind, const std::atomic<bool>& stop,
                                 std::string& error) {
    auto s = std::make_unique<UrlSource>();
    if (!s->open(url, kind, stop, error)) return nullptr;
    return s;
}

}
