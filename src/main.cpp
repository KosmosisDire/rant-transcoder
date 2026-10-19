/* rant-transcoder: republishes this host's video on the Rant mesh as H264 children. README.md
 * says how to run it. */

#include "log.hpp"
#include "transcoder.hpp"

#include <atomic>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

extern "C" {
#include <libavutil/log.h>
}

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

using namespace transcoder;

static std::atomic<bool> g_stop{ false };

#ifdef _WIN32
static BOOL WINAPI on_console(DWORD) {
    g_stop = true;
    return TRUE;
}
#else
static void on_signal(int) { g_stop = true; }
#endif

static void usage() {
    std::printf("usage: rant-transcoder [--domain N] [--encoder NAME] [--interface IP] [--verbose]\n"
                "       rant-transcoder --version\n"
                "Republishes the video on this host's Rant nodes as H264, only where that changes it:\n"
                "Parent/Video at the source's size and rate unless the source is H264 on the mesh\n"
                "already, Parent/Preview at 720p and 24 fps when the source is bigger than 720p.\n"
                "--encoder forces one FFmpeg encoder, such as h264_nvenc or libopenh264.\n"
                "--interface joins the mesh on one interface only, 127.0.0.1 for this host alone.\n");
}

int main(int argc, char** argv) {
    Options o;
    bool verbose = false;
    for (int i = 1; i < argc; i++) {
        std::string a = argv[i];
        auto value = [&]() -> const char* {
            if (i + 1 >= argc) {
                std::fprintf(stderr, "%s needs a value\n", a.c_str());
                std::exit(2);
            }
            return argv[++i];
        };
        if (a == "--domain") o.domain = uint16_t(std::atoi(value()));
        else if (a == "--encoder") o.encoder = value();
        else if (a == "--interface") o.interface = value();
        else if (a == "--verbose") verbose = true;
        else if (a == "--version") {
            std::printf("rant-transcoder %s\n", RANT_TRANSCODER_VERSION);
            return 0;
        } else {
            usage();
            return a == "--help" || a == "-h" ? 0 : 2;
        }
    }

    /* the encoders it tries and skips would fill the log, so FFmpeg speaks only when asked */
    av_log_set_level(verbose ? AV_LOG_WARNING : AV_LOG_FATAL);
#ifdef _WIN32
    SetConsoleCtrlHandler(on_console, TRUE);
#else
    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);
#endif

    try {
        Transcoder t(o);
        say("rant-transcoder %s on domain %u", RANT_TRANSCODER_VERSION, unsigned(o.domain));
        t.run([] { return g_stop.load(); });
    } catch (const rant::Error& e) {
        std::fprintf(stderr, "rant-transcoder: %s\n", e.what());
        return 1;
    }
    return 0;
}
