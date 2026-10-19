#pragma once

#include "rant.hpp"

#include <atomic>
#include <chrono>
#include <cstdint>
#include <mutex>
#include <string>
#include <thread>

namespace transcoder {

/* What a parent carries. */
enum class Origin { Image, VideoFrame, StreamTopic, StreamVariable };

/* One local entity the mesh scan found: a topic of pictures, or a stream descriptor. */
struct Parent {
    std::string name;
    Origin origin = Origin::Image;
    uint32_t peer = 0;   /* the local publisher or owner */

    bool operator==(const Parent& o) const { return name == o.name && origin == o.origin && peer == o.peer; }
};

/* A parent and the children that change it: Parent/Video at the source's own size and rate
 * unless the parent is an H264 topic already, Parent/Preview when it would be smaller. A
 * worker runs only while a child has a subscriber, apart from a probe that measures the source. */
class Stream {
public:
    Stream(rant::Node& node, Parent parent, std::string encoder);
    ~Stream();
    Stream(const Stream&) = delete;
    Stream& operator=(const Stream&) = delete;

    const Parent& parent() const { return parent_; }
    /* The main thread calls it about ten times a second. */
    void tick();

private:
    using Clock = std::chrono::steady_clock;

    void start(bool probe);
    void run();
    bool external() const { return parent_.origin == Origin::StreamTopic || parent_.origin == Origin::StreamVariable; }
    void set_descriptor(const rant::types::ExternalVideoStream& d);
    void measured(int width, int height, double fps, bool h264);
    void child(rant::Publisher<rant::types::VideoFrame>& pub, bool need, const char* suffix);
    int matches(bool preview);
    void publish(bool preview, const uint8_t* data, size_t size, bool key, int width, int height, uint64_t time_us);

    rant::Node& node_;
    Parent parent_;
    std::string encoder_;

    bool skip_ = false;   /* the name leaves no room for a child's */
    std::mutex mu_;       /* guards the children, the shape and the descriptor */
    rant::Publisher<rant::types::VideoFrame> video_;
    rant::Publisher<rant::types::VideoFrame> preview_;
    int width_ = 0, height_ = 0;
    double fps_ = 0;
    bool h264_ = false;
    std::string url_;
    rant::types::VideoStreamKind kind_ = rant::types::VideoStreamKind::Rtsp;
    std::atomic<uint32_t> descriptor_gen_{ 0 };
    rant::RemoteVariable<rant::types::ExternalVideoStream> descriptor_var_;
    rant::Subscriber<rant::types::ExternalVideoStream> descriptor_topic_;

    std::thread worker_;
    std::atomic<bool> stop_{ false };
    std::atomic<bool> done_{ true };
    std::atomic<bool> probing_{ false };
    std::atomic<bool> key_video_{ false };
    std::atomic<bool> key_preview_{ false };
    int seen_video_ = 0, seen_preview_ = 0;
    uint32_t seen_gen_ = 0;
    Clock::time_point last_wanted_{};
    Clock::time_point next_probe_{};
    rant::types::VideoFrame out_;   /* the worker's reused outgoing message */
};

}
