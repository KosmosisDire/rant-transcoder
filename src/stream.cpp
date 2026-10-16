#include "stream.hpp"

#include "log.hpp"
#include "media.hpp"
#include "rules.hpp"
#include "source.hpp"

#include <cmath>
#include <memory>

namespace transcoder {

using namespace std::chrono;
using rant::types::ExternalVideoStream;
using rant::types::VideoFrame;

namespace {

/* Rant's topic name limit, which a child's name must also fit. */
constexpr size_t kNameMax = 64;

/* One child's encoding state inside the worker. */
struct Tier {
    Encoder encoder;
    Scaler scaler;
    int failed_width = 0, failed_height = 0;   /* the size no encoder opened at, not retried */
};

/* Opens the tier's encoder for a size when it is not open at that size already. */
bool ready(Tier& t, int width, int height, double fps, const std::string& only, const std::string& child) {
    if (t.encoder.is_open() && t.encoder.width() == width && t.encoder.height() == height) return true;
    if (t.failed_width == width && t.failed_height == height) return false;
    if (!t.encoder.open(width, height, fps, only)) {
        t.failed_width = width;
        t.failed_height = height;
        say("%s: no H264 encoder opens at %dx%d", child.c_str(), width, height);
        return false;
    }
    say("%s: %dx%d at %.1f fps with %s", child.c_str(), width, height, fps, t.encoder.name());
    return true;
}

}

Stream::Stream(rant::Node& node, Parent parent, std::string encoder)
    : node_(node), parent_(std::move(parent)), encoder_(std::move(encoder)) {
    if (parent_.name.size() + std::char_traits<char>::length("/Preview") > kNameMax) {
        say("%s: the name is too long to add /Video and /Preview, skipped", parent_.name.c_str());
        return;
    }
    try {
        video_ = node_.publisher<VideoFrame>(parent_.name + "/Video");
        if (parent_.origin == Origin::StreamVariable) {
            descriptor_var_ = node_.remote_variable<ExternalVideoStream>(parent_.name);
            descriptor_var_.on_change([this](const ExternalVideoStream& d) { set_descriptor(d); });
        } else if (parent_.origin == Origin::StreamTopic) {
            rant::Qos q;
            q.catch_up = 1;
            descriptor_topic_ = node_.subscriber<ExternalVideoStream>(parent_.name, q);
        }
    } catch (const rant::Error& e) {
        say("%s: %s", parent_.name.c_str(), e.what());
    }
    next_probe_ = Clock::now();
}

Stream::~Stream() {
    descriptor_var_ = {};   /* no descriptor callback may reach a stream being torn down */
    stop_ = true;
    if (worker_.joinable()) worker_.join();
}

void Stream::tick() {
    if (!video_.valid()) return;
    if (parent_.origin == Origin::StreamTopic)
        if (auto d = descriptor_topic_.take_latest()) set_descriptor(*d);
    if (descriptor_gen_ != seen_gen_) {   /* a new URL is probed at once */
        seen_gen_ = descriptor_gen_;
        next_probe_ = Clock::now();
    }

    int vm = matches(false), pm = matches(true);
    if (vm > seen_video_) key_video_ = true;   /* a new viewer starts on a keyframe */
    if (pm > seen_preview_) key_preview_ = true;
    seen_video_ = vm;
    seen_preview_ = pm;

    auto now = Clock::now();
    bool wanted = vm > 0 || pm > 0;
    if (wanted) last_wanted_ = now;
    if (worker_.joinable() && done_) worker_.join();
    if (!worker_.joinable()) {
        bool shaped, openable;
        {
            std::lock_guard<std::mutex> g(mu_);
            shaped = width_ > 0;
            openable = !external() || !url_.empty();
        }
        if (wanted && openable) {
            start(false);
        } else if (!shaped && openable && now >= next_probe_) {
            next_probe_ = now + seconds(30);
            start(true);
        }
    } else if (!wanted && !probing_ && now - last_wanted_ > seconds(2)) {
        stop_ = true;   /* nobody watched for 2 s: the parent subscription closes with the worker */
    }
}

void Stream::start(bool probe) {
    stop_ = false;
    done_ = false;
    probing_ = probe;
    worker_ = std::thread([this] {
        run();
        done_ = true;
    });
}

void Stream::set_descriptor(const ExternalVideoStream& d) {
    std::lock_guard<std::mutex> g(mu_);
    std::string url(d.url.value.view());
    if (url == url_ && d.kind == kind_) return;
    url_ = url;
    kind_ = d.kind;
    width_ = height_ = 0;   /* another stream: measure it again */
    fps_ = 0;
    descriptor_gen_++;
}

int Stream::matches(bool preview) {
    std::lock_guard<std::mutex> g(mu_);
    auto& pub = preview ? preview_ : video_;
    return pub.valid() ? pub.match_count() : 0;
}

void Stream::measured(int width, int height, double fps) {
    std::lock_guard<std::mutex> g(mu_);
    bool changed = width != width_ || height != height_ || std::abs(fps - fps_) > fps_ * 0.2;
    width_ = width;
    height_ = height;
    fps_ = fps;
    bool need = needs_preview(width, height, fps);
    if (need && !preview_.valid()) {
        try {
            preview_ = node_.publisher<VideoFrame>(parent_.name + "/Preview");
            say("%s: %dx%d at %.1f fps, with a preview", parent_.name.c_str(), width, height, fps);
        } catch (const rant::Error& e) {
            say("%s/Preview: %s", parent_.name.c_str(), e.what());
        }
    } else if (!need && preview_.valid() && preview_.match_count() == 0) {
        /* a watched preview stays, since a busy worker undercounts a raw source's rate */
        preview_ = {};
        say("%s: %dx%d at %.1f fps, no preview needed", parent_.name.c_str(), width, height, fps);
    } else if (changed) {
        say("%s: %dx%d at %.1f fps%s", parent_.name.c_str(), width, height, fps,
            need ? ", with a preview" : ", no preview needed");
    }
}

void Stream::publish(bool preview, const uint8_t* data, size_t size, bool key, int width, int height,
                     uint64_t time_us) {
    out_.codec = rant::types::VideoCodec::H264;
    out_.width = uint32_t(width);
    out_.height = uint32_t(height);
    out_.keyframe = key;
    out_.pts.us = int64_t(time_us);
    out_.data.assign(data, data + size);
    std::lock_guard<std::mutex> g(mu_);
    auto& pub = preview ? preview_ : video_;
    if (pub.valid() && pub.match_count() > 0) pub.send(out_, rant::types::Timestamp{ int64_t(time_us) });
}

void Stream::run() {
    const std::string video_name = parent_.name + "/Video", preview_name = parent_.name + "/Preview";
    std::unique_ptr<Source> src;
    uint32_t gen = 0;
    Decoder dec;
    Tier vt, pt;
    RateGate gate(kPreviewFps);
    RateMeter meter;
    bool synced = false;            /* the decoder started on a keyframe and missed nothing since */
    bool told_open = false, told_decode = false;
    int width = 0, height = 0;      /* the latest picture size, decoded or stated */
    auto began = Clock::now(), last_measure = began;

    auto rate = [&] { return meter.count() >= 2 ? meter.fps() : src ? src->stated_fps() : 0.0; };

    while (!stop_) {
        if (probing_ && Clock::now() - began > seconds(3)) {
            if (width > 0) measured(width, height, rate());
            probing_ = false;
            if (matches(false) == 0 && matches(true) == 0) break;
        }

        if (!src || src->ended() || (external() && gen != descriptor_gen_)) {
            src.reset();
            dec.close();
            synced = false;
            std::string error;
            if (external()) {
                std::string url;
                rant::types::VideoStreamKind kind;
                {
                    std::lock_guard<std::mutex> g(mu_);
                    url = url_;
                    kind = kind_;
                    gen = descriptor_gen_;
                }
                if (!url.empty()) src = open_url(url, kind, stop_, error);
                else error = "no URL yet";
            } else {
                src = open_topic(node_, parent_.name, parent_.origin == Origin::Image, parent_.peer);
                if (!src) error = "the subscription failed";
            }
            if (!src) {
                if (!told_open) say("%s: cannot open: %s", parent_.name.c_str(), error.c_str());
                told_open = true;
                for (int i = 0; i < 20 && !stop_; i++) std::this_thread::sleep_for(milliseconds(100));
                if (probing_) break;
                continue;
            }
            told_open = false;
        }

        Unit u;
        if (!src->read(u, milliseconds(200))) continue;
        meter.add(u.time_us);
        if (u.picture) {
            width = u.picture->width;
            height = u.picture->height;
        } else if (u.width > 0 && u.height > 0) {
            width = u.width;
            height = u.height;
        }
        double fps = rate() > 0 ? rate() : 30.0;

        bool video = matches(false) > 0, preview = matches(true) > 0;
        bool pass = !u.picture && u.codec == AV_CODEC_ID_H264;   /* H264 already: Video is the same bytes */
        if (video && pass) publish(false, u.data, u.size, u.key, width, height, u.time_us);

        auto encode = [&](const AVFrame* pic, bool to_video) {
            if (to_video && ready(vt, pic->width, pic->height, fps, encoder_, video_name))
                if (AVFrame* f = vt.scaler.convert(pic, pic->width, pic->height, vt.encoder.format()))
                    vt.encoder.encode(f, u.time_us, key_video_.exchange(false), [&](const Coded& c) {
                        publish(false, c.data, c.size, c.key, pic->width, pic->height, u.time_us);
                    });
            if (preview && gate.pass(u.time_us)) {
                int pw, ph;
                preview_size(pic->width, pic->height, pw, ph);
                if (ready(pt, pw, ph, std::min(fps, kPreviewFps), encoder_, preview_name))
                    if (AVFrame* f = pt.scaler.convert(pic, pw, ph, pt.encoder.format()))
                        pt.encoder.encode(f, u.time_us, key_preview_.exchange(false), [&](const Coded& c) {
                            publish(true, c.data, c.size, c.key, pw, ph, u.time_us);
                        });
            }
        };

        if (u.picture) {
            if (video || preview) encode(u.picture, video);
        } else if ((video && !pass) || preview || width == 0) {
            if (!dec.is_open()) {
                if (!dec.open(u.codec, src->parameters())) {
                    if (!told_decode) say("%s: no decoder for %s", parent_.name.c_str(), avcodec_get_name(u.codec));
                    told_decode = true;
                    continue;
                }
                say("%s: decoding with %s", parent_.name.c_str(), dec.describe().c_str());
            }
            if (!synced && !u.key) continue;
            synced = dec.decode(u.data, u.size, [&](const AVFrame* f) {
                width = f->width;
                height = f->height;
                encode(f, video && !pass);
            });
        } else {
            synced = false;   /* frames go by undecoded, so the next decode starts on a keyframe */
        }

        if (!probing_ && width > 0 && Clock::now() - last_measure > seconds(2)) {
            measured(width, height, rate());
            last_measure = Clock::now();
        }
    }
}

}
