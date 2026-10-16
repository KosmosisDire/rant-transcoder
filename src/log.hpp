#pragma once

namespace transcoder {

/* One line on stdout, the local time in front. */
void say(const char* fmt, ...)
#if defined(__GNUC__) || defined(__clang__)
    __attribute__((format(printf, 1, 2)))
#endif
    ;

}
