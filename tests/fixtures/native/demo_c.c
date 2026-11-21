/* Plain C functions for ffi.open tests: mixed int/float arguments and
 * narrow return types (src/vm/libs/ffi). */
#include <stdint.h>

#ifdef _WIN32
#define EXPORT __declspec(dllexport)
#else
#define EXPORT __attribute__((visibility("default")))
#endif

EXPORT double mix(int32_t a, double b, int64_t c, float d, uint8_t e, double f) {
    return a + b * 10 + c * 100 + d * 1000 + e * 10000 + f * 100000;
}

EXPORT float scale(float x, int32_t times) { return x * (float)times; }

EXPORT int64_t sum8(int64_t a, int64_t b, int64_t c, int64_t d, int64_t e, int64_t f) {
    return a + b + c + d + e + f;
}

EXPORT double fsum8(double a, double b, double c, double d, double e, double f, double g,
                    double h) {
    return a + b + c + d + e + f + g + h;
}

EXPORT int8_t neg8(int8_t x) { return (int8_t)-x; }
EXPORT uint16_t wrap16(uint32_t x) { return (uint16_t)x; }
EXPORT int32_t minus_one(void) { return -1; }
EXPORT _Bool is_even(int32_t x) { return x % 2 == 0; }
