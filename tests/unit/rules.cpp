#include "check.hpp"

#include "hosts.hpp"
#include "media.hpp"
#include "rules.hpp"

#include <vector>

using namespace transcoder;

/* How many of n pictures at fps the gate passes. */
static int gated(double fps, int n) {
    RateGate g(kPreviewFps);
    int passed = 0;
    for (int i = 0; i < n; i++) passed += g.pass(uint64_t(i * 1e6 / fps)) ? 1 : 0;
    return passed;
}

int main() {
    /* a preview only when it would be smaller than the source */
    CHECK(needs_preview(1920, 1080));
    CHECK(needs_preview(1440, 1080));
    CHECK(needs_preview(1080, 1920));
    CHECK(needs_preview(1281, 720));
    CHECK(!needs_preview(1280, 720));
    CHECK(!needs_preview(640, 480));

    int w = 0, h = 0;
    preview_size(1920, 1080, w, h);
    CHECK(w == 1280 && h == 720);
    preview_size(3840, 2160, w, h);
    CHECK(w == 1280 && h == 720);
    preview_size(1440, 1080, w, h);
    CHECK(w == 960 && h == 720);
    preview_size(1080, 1920, w, h);
    CHECK(w == 404 && h == 720);
    preview_size(640, 480, w, h);
    CHECK(w == 640 && h == 480);

    /* ten seconds at each rate: the gate keeps 24 a second, a slower source keeps every one */
    CHECK(gated(30, 300) >= 238 && gated(30, 300) <= 242);
    CHECK(gated(60, 600) >= 238 && gated(60, 600) <= 242);
    CHECK(gated(24, 240) == 240);
    CHECK(gated(15, 150) == 150);

    RateMeter m;
    for (int i = 0; i < 90; i++) m.add(uint64_t(i * 1e6 / 30));
    CHECK(m.fps() > 29.5 && m.fps() < 30.5);

    CHECK(address_ip("192.168.1.20:7400") == "192.168.1.20");
    CHECK(host_addresses().count("127.0.0.1") == 1);

    /* a packed BGR picture with padded rows, then one too short to hold it */
    std::vector<uint8_t> bgr(64 * 3 + 16 + 64 * 3);
    AVFrame* f = av_frame_alloc();
    CHECK(wrap_picture(f, bgr.data(), bgr.size(), 64, 2, 64 * 3 + 16, AV_PIX_FMT_BGR24));
    CHECK(f->linesize[0] == 64 * 3 + 16 && f->width == 64 && f->height == 2);
    CHECK(!wrap_picture(f, bgr.data(), bgr.size() - 1, 64, 2, 64 * 3 + 16, AV_PIX_FMT_BGR24));
    std::vector<uint8_t> nv12(16 * 16 * 3 / 2);
    CHECK(wrap_picture(f, nv12.data(), nv12.size(), 16, 16, 0, AV_PIX_FMT_NV12));
    CHECK(f->data[1] == nv12.data() + 256 && f->linesize[1] == 16);
    av_frame_free(&f);

    const uint8_t idr[] = { 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4 };
    const uint8_t inter[] = { 0, 0, 0, 1, 0x41, 9, 9 };
    CHECK(has_idr(AV_CODEC_ID_H264, idr, sizeof idr));
    CHECK(!has_idr(AV_CODEC_ID_H264, inter, sizeof inter));
    return finish();
}
