#pragma once

#include <cstdint>
#include <deque>

namespace transcoder {

/* The preview: the source scaled to fit 1280x720, at most 24 fps. */
constexpr int kPreviewWidth = 1280;
constexpr int kPreviewHeight = 720;
constexpr double kPreviewFps = 24.0;

/* A preview exists only when it would be smaller than its source. */
bool needs_preview(int width, int height);

/* The source scaled to fit 1280x720, never up, both sides even. */
void preview_size(int width, int height, int& out_width, int& out_height);

/* Passes at most fps pictures a second, judged by their source times. */
class RateGate {
public:
    explicit RateGate(double fps);
    bool pass(uint64_t time_us);
    void reset() { started_ = false; }

private:
    double period_;
    double next_ = 0;
    bool started_ = false;
};

/* Pictures a second over the last two seconds of source time. */
class RateMeter {
public:
    void add(uint64_t time_us);
    double fps() const;
    size_t count() const { return times_.size(); }

private:
    std::deque<uint64_t> times_;
};

}
