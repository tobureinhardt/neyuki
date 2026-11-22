/*
 * neyuki_ext.h: the Neyuki native extension ABI, version 2.
 *
 * An extension is a shared library (.so / .dylib / .dll) that exports
 *
 *     int32_t neyuki_extension_init(const NeyukiExtApi *api, void *ctx);
 *
 * It keeps `api`, registers its functions with `api->register_fn(ctx, ...)`
 * and returns 0 (anything else fails the load). Scripts load it with
 * `ffi.loadExtension(path)`, or `require` it from a package that lists it
 * under `[native]` in neyuki.toml.
 *
 * Values
 *   Scalars and strings are copied. Tables, functions, buffers and big
 *   integers cross as HANDLEs (`i` holds the id):
 *   - local handles (arguments, api results) are valid until the extension
 *     call that received them returns;
 *   - `api->retain` makes a persistent handle, valid until `api->release`.
 *   A stale handle is reported as an error, never undefined behaviour.
 *
 * Ownership
 *   - What the host hands out (argument strings, `to_string`, `table_get`,
 *     `call` results, error messages) stays valid until the current
 *     extension call returns. Copy it to keep it longer.
 *   - What the extension returns (STRING, ERROR, MULTI arrays) stays owned
 *     by the extension; the host copies it before the call returns, so
 *     static or call-local storage that outlives the return is fine.
 *
 * Threads
 *   Call the api only on the thread that called you, and only while one of
 *   your functions (or neyuki_extension_init) is running. Outside of that
 *   the api returns ERROR / false / 0.
 *
 * Services and tasks (ABI 2)
 *   An extension can publish a vtable under a name at init
 *   (`provide_service`) for other extensions to find (`lookup_service`),
 *   and can drive its own event loop inside the task scheduler: park the
 *   running task on its pump (`task_park`, then return NEYUKI_YIELD),
 *   install a waker for background work, ask when the next task is due
 *   and run the due tasks.
 *
 * Compatibility
 *   NeyukiExtApi is append-only: newer runtimes add fields at the end and
 *   raise `struct_size`. Check `abi_version` (and `struct_size` before
 *   using fields of a newer ABI) in neyuki_extension_init. Runtimes load
 *   every ABI from 1 up to their own.
 */

#ifndef NEYUKI_EXT_H
#define NEYUKI_EXT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define NEYUKI_EXT_ABI_VERSION 2

enum {
    NEYUKI_NIL = 0,
    NEYUKI_BOOL = 1,   /* i: 0 or 1 */
    NEYUKI_INT = 2,    /* i */
    NEYUKI_FLOAT = 3,  /* f */
    NEYUKI_STRING = 4, /* ptr, len: UTF-8 (invalid bytes become U+FFFD) */
    NEYUKI_ERROR = 5,  /* ptr, len: message; returning it raises an error */
    NEYUKI_HANDLE = 6, /* i: handle id */
    NEYUKI_MULTI = 7,  /* ptr: NeyukiExtValue[len]; return value / call result only */
    NEYUKI_YIELD = 8   /* return value only, right after task_park parked the task */
};

/* api->handle_kind */
enum {
    NEYUKI_KIND_INVALID = 0,
    NEYUKI_KIND_TABLE = 1,
    NEYUKI_KIND_FUNCTION = 2,
    NEYUKI_KIND_BUFFER = 3,
    NEYUKI_KIND_BIGINT = 4
};

/* api->check_permission */
enum {
    NEYUKI_PERM_READ = 1,  /* a file path */
    NEYUKI_PERM_WRITE = 2, /* a file path */
    NEYUKI_PERM_NET = 3,   /* "host:port" */
    NEYUKI_PERM_RUN = 4,   /* a program */
    NEYUKI_PERM_ENV = 5,   /* a variable name */
    NEYUKI_PERM_FFI = 6    /* a library path */
};

typedef struct NeyukiExtValue {
    uint8_t tag;
    int64_t i;
    double f;
    const uint8_t *ptr;
    size_t len;
} NeyukiExtValue;

typedef NeyukiExtValue (*NeyukiExtFn)(size_t argc, const NeyukiExtValue *argv, void *user_data);

/* An event pump for task_park: wait up to `timeout_secs`, return the events
 * as an array table (or ERROR). The api is usable inside it. */
typedef NeyukiExtValue (*NeyukiExtPumpFn)(double timeout_secs, void *user_data);
/* Wakes a blocked event loop. Called from worker threads: be thread-safe and
 * do not use the api. */
typedef void (*NeyukiExtWakeFn)(void *user_data);

typedef struct NeyukiExtApi {
    uint32_t abi_version;
    uint32_t struct_size;

    /* Registers `fn` as `name` (not "close"; each name once). Init only. */
    bool (*register_fn)(void *ctx, const uint8_t *name, size_t name_len, NeyukiExtFn fn,
                        void *user_data);

    /* NEYUKI_KIND_* for a live handle, NEYUKI_KIND_INVALID otherwise. */
    uint32_t (*handle_kind)(NeyukiExtValue value);

    /* Tables. Keys and values are any NeyukiExtValue except ERROR/MULTI.
     * table_set returns NIL, or ERROR (frozen table, nil key, bad handle).
     * table_len is the length of the array part (#t).
     * table_next iterates: start with key NIL; false at the end. */
    NeyukiExtValue (*new_table)(size_t array_hint, size_t field_hint);
    NeyukiExtValue (*table_get)(NeyukiExtValue table, NeyukiExtValue key);
    NeyukiExtValue (*table_set)(NeyukiExtValue table, NeyukiExtValue key, NeyukiExtValue value);
    size_t (*table_len)(NeyukiExtValue table);
    bool (*table_next)(NeyukiExtValue table, NeyukiExtValue key, NeyukiExtValue *out_key,
                       NeyukiExtValue *out_value);

    /* Calls a script function. Returns its result: NIL for none, MULTI for
     * several, ERROR when it raised one (return that to propagate it). */
    NeyukiExtValue (*call)(NeyukiExtValue fn, size_t argc, const NeyukiExtValue *argv);

    /* Buffers. new_buffer copies `len` bytes from `data` (zeroed if NULL).
     * buffer_data exposes a buffer's bytes for reading and writing. The
     * pointer is valid until the next api->call (script code can resize
     * the buffer) or until the current call returns, whichever is first. */
    NeyukiExtValue (*new_buffer)(const uint8_t *data, size_t len);
    bool (*buffer_data)(NeyukiExtValue buffer, uint8_t **out_ptr, size_t *out_len);

    /* tostring(value) as a host-owned STRING. */
    NeyukiExtValue (*to_string)(NeyukiExtValue value);

    /* Persistent handles. release accepts any value and ignores non-persistent ones. */
    NeyukiExtValue (*retain)(NeyukiExtValue handle);
    void (*release)(NeyukiExtValue handle);

    /* Asks the sandbox (--sandbox / --allow-*) whether the script may touch
     * `target`. Returns NIL when allowed, an ERROR to return otherwise.
     * Native code is not sandboxed by itself: call this before doing I/O
     * on a script's behalf. */
    NeyukiExtValue (*check_permission)(uint32_t kind, const uint8_t *target, size_t target_len);

    /* ---- ABI 2 ---- */
    /* Init only: publishes `service` (memory the extension owns) as `name`.
     * Forgotten when the library is closed. */
    bool (*provide_service)(void *ctx, const uint8_t *name, size_t name_len, const void *service);
    /* The service published as `name`, or NULL. */
    const void *(*lookup_service)(const uint8_t *name, size_t name_len);
    /* Parks the running task on `pump` for up to `timeout_secs` and returns
     * NEYUKI_YIELD, which you must return at once; the task resumes with the
     * pump's events (or an empty table after the timeout) as your result.
     * Returns NIL when it cannot park (not in a task, zero timeout): pump in
     * place instead. */
    NeyukiExtValue (*task_park)(double timeout_secs, NeyukiExtPumpFn pump, void *user_data);
    /* Installs the waker background work calls on completion. */
    bool (*set_waker)(NeyukiExtWakeFn wake, void *user_data);
    bool (*has_waker)(void);
    /* Seconds until the next task is due in *out_secs; false when none waits. */
    bool (*next_due)(double *out_secs);
    /* Resumes due tasks for about `budget_secs`. */
    void (*run_tasks)(double budget_secs);
} NeyukiExtApi;

typedef int32_t (*NeyukiExtInitFn)(const NeyukiExtApi *api, void *ctx);

/* Convenience constructors. */
static inline NeyukiExtValue neyuki_nil(void) {
    NeyukiExtValue v = {NEYUKI_NIL, 0, 0.0, NULL, 0};
    return v;
}
static inline NeyukiExtValue neyuki_bool(bool b) {
    NeyukiExtValue v = {NEYUKI_BOOL, b ? 1 : 0, 0.0, NULL, 0};
    return v;
}
static inline NeyukiExtValue neyuki_int(int64_t i) {
    NeyukiExtValue v = {NEYUKI_INT, i, 0.0, NULL, 0};
    return v;
}
static inline NeyukiExtValue neyuki_float(double f) {
    NeyukiExtValue v = {NEYUKI_FLOAT, 0, f, NULL, 0};
    return v;
}
static inline NeyukiExtValue neyuki_string(const char *s, size_t len) {
    NeyukiExtValue v = {NEYUKI_STRING, 0, 0.0, (const uint8_t *)s, len};
    return v;
}
static inline NeyukiExtValue neyuki_error(const char *msg, size_t len) {
    NeyukiExtValue v = {NEYUKI_ERROR, 0, 0.0, (const uint8_t *)msg, len};
    return v;
}
static inline NeyukiExtValue neyuki_multi(const NeyukiExtValue *values, size_t count) {
    NeyukiExtValue v = {NEYUKI_MULTI, 0, 0.0, (const uint8_t *)values, count};
    return v;
}

#ifdef _WIN32
#define NEYUKI_EXPORT __declspec(dllexport)
#else
#define NEYUKI_EXPORT __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
}
#endif

#endif /* NEYUKI_EXT_H */
