#pragma once

#include <cstdio>
#include <cstdlib>

static int g_failures = 0;

#define CHECK(cond) do { if (!(cond)) { std::fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #cond); g_failures++; } } while (0)

static int finish() {
    if (g_failures) std::fprintf(stderr, "%d check(s) failed\n", g_failures);
    return g_failures ? 1 : 0;
}
