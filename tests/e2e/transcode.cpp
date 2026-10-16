/* The transcoder end to end in one process. A camera node publishes a raw 1080p Image, a
 * small Image, H264 VideoFrames and a stream URL, and a viewer checks every child. */

#include "check.hpp"

#include "media.hpp"
#include "source.hpp"
#include "transcoder.hpp"

#include <atomic>
#include <cstdio>
#include <cstring>
#include <filesystem>
#include <mutex>
#include <thread>
#include <vector>

extern "C" {
#include <libavformat/avformat.h>
#include <libavutil/log.h>
}

using namespace transcoder;
using namespace std::chrono;
using rant::types::ExternalVideoStream;
using rant::types::Image;
using rant::types::VideoFrame;

/* e2e tests stay off domain 0, where the lab mesh runs */
constexpr uint16_t kDomain = 151;

/* What the viewer saw on one child. */
struct Watch {
    std::mutex mu;
    Decoder decoder;
    bool synced = false;
    int width = 0, height = 0;
    std::vector<steady_clock::time_point> decoded;

    void add(const VideoFrame& f) {
        std::lock_guard<std::mutex> g(mu);
        if (f.codec != rant::types::VideoCodec::H264) return;
        if (!decoder.is_open()) decoder.open(AV_CODEC_ID_H264, nullptr);
        if (!synced && !f.keyframe) return;
        synced = decoder.decode(f.data.data(), f.data.size(), [&](const AVFrame* pic) {
            width = pic->width;
            height = pic->height;
            decoded.push_back(steady_clock::now());
        });
    }
    /* Decoded pictures a second over the last window. */
    double fps(seconds window) {
        std::lock_guard<std::mutex> g(mu);
        auto from = steady_clock::now() - window;
        size_t n = 0;
        for (auto t : decoded) n += t >= from ? 1 : 0;
        return double(n) / double(window.count());
    }
};

/* A moving gradient, BGR. */
static void paint(std::vector<uint8_t>& bgr, int width, int height, int frame) {
    for (int y = 0; y < height; y++)
        for (int x = 0; x < width; x++) {
            uint8_t* p = &bgr[(size_t(y) * width + x) * 3];
            p[0] = uint8_t(x + frame * 4);
            p[1] = uint8_t(y + frame * 2);
            p[2] = uint8_t((x ^ y) + frame);
        }
}

/* Ten seconds of 1280x720 H264 at 30 fps, one access unit each. */
static std::vector<std::vector<uint8_t>> make_h264(std::vector<bool>& keys) {
    std::vector<std::vector<uint8_t>> units;
    Encoder enc;
    if (!enc.open(1280, 720, 30, "libopenh264")) return units;
    std::vector<uint8_t> bgr(1280 * 720 * 3);
    AVFrame* src = av_frame_alloc();
    Scaler scale;
    for (int i = 0; i < 300; i++) {
        paint(bgr, 1280, 720, i);
        wrap_picture(src, bgr.data(), bgr.size(), 1280, 720, 0, AV_PIX_FMT_BGR24);
        AVFrame* f = scale.convert(src, 1280, 720, enc.format());
        enc.encode(f, uint64_t(i) * 33333, i == 0, [&](const Coded& c) {
            units.emplace_back(c.data, c.data + c.size);
            keys.push_back(c.key);
        });
    }
    av_frame_free(&src);
    return units;
}

/* The units as MPEG-TS at 30 fps, the container SRT carries, with the timestamps a stream has. */
static bool write_ts(const std::string& path, const std::vector<std::vector<uint8_t>>& units,
                     const std::vector<bool>& keys) {
    AVFormatContext* oc = nullptr;
    if (avformat_alloc_output_context2(&oc, nullptr, "mpegts", path.c_str()) < 0) return false;
    AVStream* st = avformat_new_stream(oc, nullptr);
    st->codecpar->codec_type = AVMEDIA_TYPE_VIDEO;
    st->codecpar->codec_id = AV_CODEC_ID_H264;
    st->codecpar->width = 1280;
    st->codecpar->height = 720;
    st->time_base = { 1, 90000 };
    bool ok = avio_open(&oc->pb, path.c_str(), AVIO_FLAG_WRITE) >= 0 && avformat_write_header(oc, nullptr) >= 0;
    AVPacket* pkt = av_packet_alloc();
    for (size_t i = 0; ok && i < units.size(); i++) {
        av_new_packet(pkt, int(units[i].size()));
        std::memcpy(pkt->data, units[i].data(), units[i].size());
        pkt->pts = pkt->dts = av_rescale_q(int64_t(i) * 33333, { 1, 1000000 }, st->time_base);
        pkt->flags = keys[i] ? AV_PKT_FLAG_KEY : 0;
        ok = av_interleaved_write_frame(oc, pkt) >= 0;
        av_packet_unref(pkt);
    }
    av_packet_free(&pkt);
    if (ok) ok = av_write_trailer(oc) >= 0;
    avio_closep(&oc->pb);
    avformat_free_context(oc);
    return ok;
}

int main() {
    av_log_set_level(AV_LOG_FATAL);
    std::vector<bool> keys;
    auto units = make_h264(keys);
    CHECK(units.size() >= 290);
    if (units.empty()) return finish();
    std::filesystem::path file = std::filesystem::temp_directory_path() / "rant_transcoder_e2e.ts";
    CHECK(write_ts(file.generic_string(), units, keys));

    std::atomic<bool> done{ false };

    /* the transcoder */
    Options o;
    o.domain = kDomain;
    std::thread transcoder([&] {
        Transcoder t(o);
        t.run([&] { return done.load(); });
    });

    /* the camera */
    rant::NodeOptions no;
    no.domain = kDomain;
    rant::Node cam("cam", no);
    auto raw = cam.publisher<Image>("Test/Raw");
    auto small = cam.publisher<Image>("Test/Small");
    auto coded = cam.publisher<VideoFrame>("Test/Coded");
    rant::VariableOptions<ExternalVideoStream> vo;
    ExternalVideoStream desc;
    desc.kind = rant::types::VideoStreamKind::Other;
    desc.codec = rant::types::VideoCodec::H264;
    desc.width = 1280;
    desc.height = 720;
    desc.url.value.assign("file:" + file.generic_string());
    desc.name.assign("e2e");
    vo.initial = desc;
    auto stream = cam.variable_definition<ExternalVideoStream>("Test/Cam", vo);

    std::thread camera([&] {
        Image big, little;
        big.width = 1920; big.height = 1080; big.format = rant::types::ImageFormat::Bgr8;
        big.data.resize(1920 * 1080 * 3);
        little.width = 640; little.height = 480; little.format = rant::types::ImageFormat::Bgr8;
        little.data.resize(640 * 480 * 3);
        paint(little.data, 640, 480, 0);
        auto next = steady_clock::now();
        for (int i = 0; !done; i++) {
            /* a band of the big picture moves, enough to make the encoder work */
            paint(big.data, 1920, 1, i);
            big.data[size_t(i % 1080) * 1920 * 3] = uint8_t(i);
            rant::types::Timestamp now{ int64_t(wall_us()) };
            raw.send(big, now);
            if (i % 3 != 2) small.send(little, now);   /* 20 fps */
            VideoFrame vf;
            vf.codec = rant::types::VideoCodec::H264;
            vf.width = 1280; vf.height = 720;
            vf.keyframe = keys[size_t(i) % units.size()];
            vf.pts = now;
            vf.data = units[size_t(i) % units.size()];
            coded.send(vf, now);
            next += microseconds(33333);
            std::this_thread::sleep_until(next);
        }
    });

    /* the viewer */
    rant::Node viewer("viewer", no);
    const char* names[] = { "Test/Raw/Video", "Test/Raw/Preview", "Test/Small/Video", "Test/Small/Preview",
                            "Test/Coded/Video", "Test/Coded/Preview", "Test/Cam/Video" };
    std::vector<std::unique_ptr<Watch>> watches;
    std::vector<rant::Subscriber<VideoFrame>> subs;
    for (const char* n : names) {
        watches.push_back(std::make_unique<Watch>());
        Watch* w = watches.back().get();
        subs.push_back(viewer.subscriber<VideoFrame>(n, [w](const VideoFrame& f) { w->add(f); }));
    }

    std::this_thread::sleep_for(seconds(16));
    auto window = seconds(5);
    double fps[7];
    for (int i = 0; i < 7; i++) {
        fps[i] = watches[i]->fps(window);
        std::printf("%-20s %4dx%-4d %5.1f fps\n", names[i], watches[i]->width, watches[i]->height, fps[i]);
    }

    /* Raw: Video at the source's size and rate, Preview at 720p and 24 fps */
    CHECK(watches[0]->width == 1920 && watches[0]->height == 1080 && fps[0] > 20);
    CHECK(watches[1]->width == 1280 && watches[1]->height == 720 && fps[1] > 18 && fps[1] < 26);
    /* Small: 640x480 at 20 fps is under the preview's limits, so it has none */
    CHECK(watches[2]->width == 640 && watches[2]->height == 480 && fps[2] > 14);
    CHECK(watches[3]->fps(seconds(16)) == 0);
    auto none = viewer.reflection().find(rant::EntityKind::Topic, "Test/Small/Preview");
    CHECK(!none || none->providers == 0);
    /* Coded: H264 passes through, and 30 fps gets a 24 fps preview at the same 720p */
    CHECK(watches[4]->width == 1280 && watches[4]->height == 720 && fps[4] > 20);
    CHECK(watches[5]->width == 1280 && watches[5]->height == 720 && fps[5] > 18 && fps[5] < 26);
    /* Cam: the URL is read at its own pace, and its H264 passes through */
    CHECK(watches[6]->width == 1280 && watches[6]->height == 720 && fps[6] > 24 && fps[6] < 36);

    done = true;
    camera.join();
    transcoder.join();
    subs.clear();
    std::filesystem::remove(file);
    return finish();
}
