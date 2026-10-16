#pragma once

#include "rant.hpp"
#include "stream.hpp"

#include <chrono>
#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>

namespace transcoder {

struct Options {
    uint16_t domain = 0;                 /* 0 takes RANT_DOMAIN */
    std::string name = "transcoder";     /* every transcoder's node is skipped as a source */
    std::string encoder;                 /* one FFmpeg encoder to use, empty = the best that opens */
};

/* Watches the mesh for video published on this host and keeps a Stream for each. */
class Transcoder {
public:
    explicit Transcoder(const Options& options);
    /* Runs until stop says so, then closes every stream. */
    void run(const std::function<bool()>& stop);

private:
    void scan();

    Options options_;
    std::mutex told_mu_;   /* before the node, so they outlive its event handler */
    std::map<std::string, std::chrono::steady_clock::time_point> told_;
    rant::Node node_;
    std::map<std::string, std::unique_ptr<Stream>> streams_;
    uint32_t epoch_ = 0;
};

}
