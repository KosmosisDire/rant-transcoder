#include "rules.hpp"

#include <algorithm>
#include <cmath>

namespace transcoder {

bool needs_preview(int width, int height, double fps) {
    return width > kPreviewWidth || height > kPreviewHeight || fps > kPreviewFps * 1.1;
}

void preview_size(int width, int height, int& out_width, int& out_height) {
    double s = std::min({ 1.0, double(kPreviewWidth) / width, double(kPreviewHeight) / height });
    out_width = std::max(2, int(std::lround(width * s)) & ~1);
    out_height = std::max(2, int(std::lround(height * s)) & ~1);
}

RateGate::RateGate(double fps) : period_(1e6 / fps) {}

bool RateGate::pass(uint64_t time_us) {
    double t = double(time_us);
    /* an eighth of a period of slack, so a source at exactly the rate never loses a frame to jitter */
    if (started_ && t + period_ / 8 < next_) return false;
    next_ = (started_ && t < next_ + period_) ? next_ + period_ : t + period_;
    started_ = true;
    return true;
}

void RateMeter::add(uint64_t time_us) {
    if (!times_.empty() && time_us < times_.back()) times_.clear();   /* the clock went back: start over */
    times_.push_back(time_us);
    while (times_.size() > 2 && times_.back() - times_.front() > 2000000) times_.pop_front();
}

double RateMeter::fps() const {
    if (times_.size() < 2 || times_.back() == times_.front()) return 0;
    return double(times_.size() - 1) * 1e6 / double(times_.back() - times_.front());
}

}
