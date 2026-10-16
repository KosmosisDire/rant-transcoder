#include "log.hpp"

#include <cstdarg>
#include <cstdio>
#include <ctime>
#include <mutex>

namespace transcoder {

void say(const char* fmt, ...) {
    static std::mutex mu;
    char line[1024];
    std::time_t now = std::time(nullptr);
    std::tm local{};
#ifdef _WIN32
    localtime_s(&local, &now);
#else
    localtime_r(&now, &local);
#endif
    size_t n = std::strftime(line, sizeof line, "%H:%M:%S ", &local);
    va_list args;
    va_start(args, fmt);
    std::vsnprintf(line + n, sizeof line - n, fmt, args);
    va_end(args);
    std::lock_guard<std::mutex> g(mu);
    std::fputs(line, stdout);
    std::fputc('\n', stdout);
    std::fflush(stdout);
}

}
