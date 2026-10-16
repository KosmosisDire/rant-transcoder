#pragma once

#include "rant.hpp"

#include <atomic>
#include <chrono>
#include <cstdint>
#include <memory>
#include <string>

extern "C" {
#include <libavcodec/avcodec.h>
}

namespace transcoder {

/* One step of input: a raw picture, or one coded access unit. */
struct Unit {
    const AVFrame* picture = nullptr;     /* raw: wraps the message bytes until the next read */
    AVCodecID codec = AV_CODEC_ID_NONE;   /* coded */
    const uint8_t* data = nullptr;
    size_t size = 0;
    bool key = false;
    int width = 0, height = 0;            /* 0 = unstated */
    uint64_t time_us = 0;                 /* when it was true, UTC: capture, else written, else arrival */
};

/* Where units come from: a Rant topic or a stream URL. */
class Source {
public:
    virtual ~Source() = default;
    /* The next unit, false when none came in time or the source ended. A URL blocks until a
     * unit, the stop flag or a 10 s stall, whatever timeout says. */
    virtual bool read(Unit& unit, std::chrono::milliseconds timeout) = 0;
    virtual bool ended() const { return false; }
    /* A demuxed stream's parameters for its decoder, null for a Rant topic. */
    virtual const AVCodecParameters* parameters() const { return nullptr; }
    /* The stream's own frame rate claim, 0 when it makes none. */
    virtual double stated_fps() const { return 0; }
};

/* An Image or VideoFrame topic, taking messages from one publisher only. Null when the
 * subscription fails. An Image source reads the newest picture, a VideoFrame one every frame. */
std::unique_ptr<Source> open_topic(rant::Node& node, const std::string& name, bool image, uint32_t publisher);

/* An ExternalVideoStream's URL, through FFmpeg. stop interrupts a blocking open or read.
 * Null with error set when it cannot open. */
std::unique_ptr<Source> open_url(const std::string& url, rant::types::VideoStreamKind kind,
                                 const std::atomic<bool>& stop, std::string& error);

/* UTC microseconds now. */
uint64_t wall_us();

}
