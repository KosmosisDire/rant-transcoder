/* The transcoder end to end in one process. A camera node publishes raw pictures, H264 and
 * MJPEG frames and a stream URL, and a viewer checks which children exist and what they carry. */

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

/* e2e tests stay off domain 0, where the lab mesh runs, and on loopback as Rant's own do, so
 * a host's other networks never join in */
constexpr uint16_t kDomain = 151;
constexpr const char* kLoopback = "127.0.0.1";

/* Coded frames to publish in a loop, with their keyframe flags. */
struct Clip {
    std::vector<std::vector<uint8_t>> units;
    std::vector<bool> keys;
};

/* What the viewer saw on one child. */
struct Watch {
    std::mutex mu;
    Decoder decoder;
    bool synced = false;
    int width = 0, height = 0;
    int received = 0;
    std::vector<steady_clock::time_point> decoded;

    void add(const VideoFrame& f) {
        std::lock_guard<std::mutex> g(mu);
        received++;
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

/* Ten seconds of H264 at 30 fps, one access unit each. */
static Clip make_h264(int width, int height) {
    Clip clip;
    Encoder enc;
    if (!enc.open(width, height, 30, "libopenh264")) return clip;
    std::vector<uint8_t> bgr(size_t(width) * height * 3);
    AVFrame* src = av_frame_alloc();
    Scaler scale;
    for (int i = 0; i < 300; i++) {
        paint(bgr, width, height, i);
        wrap_picture(src, bgr.data(), bgr.size(), width, height, 0, AV_PIX_FMT_BGR24);
        AVFrame* f = scale.convert(src, width, height, enc.format());
        enc.encode(f, uint64_t(i) * 33333, i == 0, [&](const Coded& c) {
            clip.units.emplace_back(c.data, c.data + c.size);
            clip.keys.push_back(c.key);
        });
    }
    av_frame_free(&src);
    return clip;
}

/* One second of MJPEG, every frame a keyframe. */
static Clip make_mjpeg(int width, int height) {
    Clip clip;
    const AVCodec* codec = avcodec_find_encoder(AV_CODEC_ID_MJPEG);
    AVCodecContext* ctx = avcodec_alloc_context3(codec);
    ctx->width = width;
    ctx->height = height;
    ctx->time_base = { 1, 30 };
    ctx->pix_fmt = AV_PIX_FMT_YUVJ420P;
    if (avcodec_open2(ctx, codec, nullptr) < 0) {
        avcodec_free_context(&ctx);
        return clip;
    }
    std::vector<uint8_t> bgr(size_t(width) * height * 3);
    AVFrame* src = av_frame_alloc();
    AVPacket* pkt = av_packet_alloc();
    Scaler scale;
    for (int i = 0; i < 30; i++) {
        paint(bgr, width, height, i);
        wrap_picture(src, bgr.data(), bgr.size(), width, height, 0, AV_PIX_FMT_BGR24);
        AVFrame* f = scale.convert(src, width, height, AV_PIX_FMT_YUVJ420P);
        f->pts = i;
        if (avcodec_send_frame(ctx, f) < 0) continue;
        while (avcodec_receive_packet(ctx, pkt) == 0) {
            clip.units.emplace_back(pkt->data, pkt->data + pkt->size);
            clip.keys.push_back(true);
            av_packet_unref(pkt);
        }
    }
    av_packet_free(&pkt);
    av_frame_free(&src);
    avcodec_free_context(&ctx);
    return clip;
}

/* The clip as MPEG-TS at 30 fps, the container SRT carries, with the timestamps a stream has. */
static bool write_ts(const std::string& path, const Clip& clip, int width, int height) {
    AVFormatContext* oc = nullptr;
    if (avformat_alloc_output_context2(&oc, nullptr, "mpegts", path.c_str()) < 0) return false;
    AVStream* st = avformat_new_stream(oc, nullptr);
    st->codecpar->codec_type = AVMEDIA_TYPE_VIDEO;
    st->codecpar->codec_id = AV_CODEC_ID_H264;
    st->codecpar->width = width;
    st->codecpar->height = height;
    st->time_base = { 1, 90000 };
    bool ok = avio_open(&oc->pb, path.c_str(), AVIO_FLAG_WRITE) >= 0 && avformat_write_header(oc, nullptr) >= 0;
    AVPacket* pkt = av_packet_alloc();
    for (size_t i = 0; ok && i < clip.units.size(); i++) {
        av_new_packet(pkt, int(clip.units[i].size()));
        std::memcpy(pkt->data, clip.units[i].data(), clip.units[i].size());
        pkt->pts = pkt->dts = av_rescale_q(int64_t(i) * 33333, { 1, 1000000 }, st->time_base);
        pkt->flags = clip.keys[i] ? AV_PKT_FLAG_KEY : 0;
        ok = av_interleaved_write_frame(oc, pkt) >= 0;
        av_packet_unref(pkt);
    }
    av_packet_free(&pkt);
    if (ok) ok = av_write_trailer(oc) >= 0;
    avio_closep(&oc->pb);
    avformat_free_context(oc);
    return ok;
}

/* One frame of a clip as a VideoFrame. */
static VideoFrame frame_of(const Clip& clip, int i, rant::types::VideoCodec codec, int width, int height,
                           rant::types::Timestamp now) {
    size_t k = size_t(i) % clip.units.size();
    VideoFrame vf;
    vf.codec = codec;
    vf.width = uint32_t(width);
    vf.height = uint32_t(height);
    vf.keyframe = clip.keys[k];
    vf.pts = now;
    vf.data = clip.units[k];
    return vf;
}

/* One child the viewer watches. width 0 = the transcoder must not publish it. */
struct Expect {
    const char* name;
    int width, height;
    double min_fps, max_fps;
};

int main() {
    av_log_set_level(AV_LOG_FATAL);
    Clip big = make_h264(1920, 1080), small = make_h264(1280, 720), jpeg = make_mjpeg(640, 480);
    CHECK(big.units.size() >= 290 && small.units.size() >= 290 && jpeg.units.size() == 30);
    if (big.units.empty() || small.units.empty() || jpeg.units.empty()) return finish();
    std::filesystem::path file = std::filesystem::temp_directory_path() / "rant_transcoder_e2e.ts";
    CHECK(write_ts(file.generic_string(), small, 1280, 720));

    std::atomic<bool> done{ false };

    /* the transcoder */
    Options o;
    o.domain = kDomain;
    o.interface = kLoopback;
    std::thread transcoder([&] {
        Transcoder t(o);
        t.run([&] { return done.load(); });
    });

    /* the camera */
    rant::NodeOptions no;
    no.domain = kDomain;
    no.multicast_interface = kLoopback;
    no.max_topics = 64;
    rant::Node cam("cam", no);
    auto raw = cam.publisher<Image>("Test/Raw");
    auto little = cam.publisher<Image>("Test/Small");
    auto big264 = cam.publisher<VideoFrame>("Test/Big264");
    auto small264 = cam.publisher<VideoFrame>("Test/Small264");
    auto mjpeg = cam.publisher<VideoFrame>("Test/Jpeg");
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
        Image bigpic, smallpic;
        bigpic.width = 1920; bigpic.height = 1080; bigpic.format = rant::types::ImageFormat::Bgr8;
        bigpic.data.resize(1920 * 1080 * 3);
        smallpic.width = 640; smallpic.height = 480; smallpic.format = rant::types::ImageFormat::Bgr8;
        smallpic.data.resize(640 * 480 * 3);
        paint(smallpic.data, 640, 480, 0);
        auto next = steady_clock::now();
        for (int i = 0; !done; i++) {
            /* a band of the big picture moves, enough to make the encoder work */
            paint(bigpic.data, 1920, 1, i);
            bigpic.data[size_t(i % 1080) * 1920 * 3] = uint8_t(i);
            rant::types::Timestamp now{ int64_t(wall_us()) };
            raw.send(bigpic, now);
            if (i % 3 != 2) little.send(smallpic, now);   /* 20 fps */
            big264.send(frame_of(big, i, rant::types::VideoCodec::H264, 1920, 1080, now), now);
            small264.send(frame_of(small, i, rant::types::VideoCodec::H264, 1280, 720, now), now);
            mjpeg.send(frame_of(jpeg, i, rant::types::VideoCodec::Mjpeg, 640, 480, now), now);
            next += microseconds(33333);
            std::this_thread::sleep_until(next);
        }
    });

    /* the viewer, with the children each source must and must not get */
    const Expect expects[] = {
        { "Test/Raw/Video", 1920, 1080, 20, 32 },        /* raw: Video and a smaller Preview */
        { "Test/Raw/Preview", 1280, 720, 18, 26 },
        { "Test/Small/Video", 640, 480, 14, 22 },        /* 480p: a preview would be the same size */
        { "Test/Small/Preview", 0, 0, 0, 0 },
        { "Test/Big264/Video", 0, 0, 0, 0 },             /* H264 already: only the smaller Preview */
        { "Test/Big264/Preview", 1280, 720, 18, 26 },
        { "Test/Small264/Video", 0, 0, 0, 0 },           /* H264 at 720p: nothing changes, nothing published */
        { "Test/Small264/Preview", 0, 0, 0, 0 },
        { "Test/Jpeg/Video", 640, 480, 20, 32 },         /* MJPEG: Video only */
        { "Test/Jpeg/Preview", 0, 0, 0, 0 },
        { "Test/Cam/Video", 1280, 720, 24, 36 },         /* a URL: Video brings it onto the mesh */
        { "Test/Cam/Preview", 0, 0, 0, 0 },
    };
    constexpr size_t n = sizeof expects / sizeof expects[0];
    rant::Node viewer("viewer", no);
    std::vector<std::unique_ptr<Watch>> watches;
    std::vector<rant::Subscriber<VideoFrame>> subs;
    for (const Expect& e : expects) {
        watches.push_back(std::make_unique<Watch>());
        Watch* w = watches.back().get();
        subs.push_back(viewer.subscriber<VideoFrame>(e.name, [w](const VideoFrame& f) { w->add(f); }));
    }

    std::this_thread::sleep_for(seconds(16));
    rant::Reflection mesh = viewer.reflection();
    for (size_t i = 0; i < n; i++) {
        const Expect& e = expects[i];
        Watch& w = *watches[i];
        double fps = w.fps(seconds(5));
        bool ok;
        if (e.width) {
            ok = w.width == e.width && w.height == e.height && fps >= e.min_fps && fps <= e.max_fps;
        } else {
            auto found = mesh.find(rant::EntityKind::Topic, e.name);
            ok = w.fps(seconds(16)) == 0 && (!found || found->providers == 0);
        }
        std::printf("%-4s %-22s %4dx%-4d %5.1f fps, %d received\n", ok ? "ok" : "FAIL", e.name, w.width, w.height,
                    fps, w.received);
        CHECK(ok);
    }

    done = true;
    camera.join();
    transcoder.join();
    subs.clear();
    std::filesystem::remove(file);
    return finish();
}
