/* A small native extension used by the FFI tests (src/vm/libs/ffi). */
#include <string.h>

#include "../../../include/neyuki_ext.h"

static const NeyukiExtApi *api;

/* greet(name) -> "hello, <name>" */
static NeyukiExtValue greet(size_t argc, const NeyukiExtValue *argv, void *ud) {
    static char out[256];
    (void)ud;
    if (argc != 1 || argv[0].tag != NEYUKI_STRING || argv[0].len > 200) {
        static const char msg[] = "greet expects a short string";
        return neyuki_error(msg, sizeof msg - 1);
    }
    memcpy(out, "hello, ", 7);
    memcpy(out + 7, argv[0].ptr, argv[0].len);
    return neyuki_string(out, 7 + argv[0].len);
}

/* stats(list) -> min, max, sum */
static NeyukiExtValue stats(size_t argc, const NeyukiExtValue *argv, void *ud) {
    static NeyukiExtValue out[3];
    (void)ud;
    if (argc != 1 || api->handle_kind(argv[0]) != NEYUKI_KIND_TABLE) {
        static const char msg[] = "stats expects a list";
        return neyuki_error(msg, sizeof msg - 1);
    }
    size_t n = api->table_len(argv[0]);
    double lo = 0, hi = 0, sum = 0;
    for (size_t i = 1; i <= n; i++) {
        NeyukiExtValue v = api->table_get(argv[0], neyuki_int((int64_t)i));
        double x = v.tag == NEYUKI_INT ? (double)v.i : v.f;
        if (i == 1 || x < lo) lo = x;
        if (i == 1 || x > hi) hi = x;
        sum += x;
    }
    out[0] = neyuki_float(lo);
    out[1] = neyuki_float(hi);
    out[2] = neyuki_float(sum);
    return neyuki_multi(out, 3);
}

/* map(list, fn) -> { fn(list[1]), fn(list[2]), ... } */
static NeyukiExtValue map(size_t argc, const NeyukiExtValue *argv, void *ud) {
    (void)ud;
    if (argc != 2 || api->handle_kind(argv[1]) != NEYUKI_KIND_FUNCTION) {
        static const char msg[] = "map expects a list and a function";
        return neyuki_error(msg, sizeof msg - 1);
    }
    size_t n = api->table_len(argv[0]);
    NeyukiExtValue out = api->new_table(n, 0);
    for (size_t i = 1; i <= n; i++) {
        NeyukiExtValue item = api->table_get(argv[0], neyuki_int((int64_t)i));
        NeyukiExtValue r = api->call(argv[1], 1, &item);
        if (r.tag == NEYUKI_ERROR) return r;
        api->table_set(out, neyuki_int((int64_t)i), r);
    }
    return out;
}

/* reverse(buffer) reverses the bytes in place and returns the buffer */
static NeyukiExtValue reverse(size_t argc, const NeyukiExtValue *argv, void *ud) {
    uint8_t *p;
    size_t len;
    (void)ud;
    if (argc != 1 || !api->buffer_data(argv[0], &p, &len)) {
        static const char msg[] = "reverse expects a buffer";
        return neyuki_error(msg, sizeof msg - 1);
    }
    for (size_t i = 0; i < len / 2; i++) {
        uint8_t t = p[i];
        p[i] = p[len - 1 - i];
        p[len - 1 - i] = t;
    }
    return argv[0];
}

/* readable(path) -> true, or the sandbox's error */
static NeyukiExtValue readable(size_t argc, const NeyukiExtValue *argv, void *ud) {
    (void)ud;
    if (argc != 1 || argv[0].tag != NEYUKI_STRING) return neyuki_bool(false);
    NeyukiExtValue r = api->check_permission(NEYUKI_PERM_READ, argv[0].ptr, argv[0].len);
    return r.tag == NEYUKI_ERROR ? r : neyuki_bool(true);
}

NEYUKI_EXPORT int32_t neyuki_extension_init(const NeyukiExtApi *host, void *ctx) {
    static const struct {
        const char *name;
        NeyukiExtFn fn;
    } fns[] = {
        {"greet", greet}, {"stats", stats}, {"map", map}, {"reverse", reverse}, {"readable", readable},
    };
    if (host->abi_version != NEYUKI_EXT_ABI_VERSION) return 1;
    api = host;
    for (size_t i = 0; i < sizeof fns / sizeof fns[0]; i++) {
        if (!api->register_fn(ctx, (const uint8_t *)fns[i].name, strlen(fns[i].name), fns[i].fn, NULL))
            return 2;
    }
    return 0;
}
