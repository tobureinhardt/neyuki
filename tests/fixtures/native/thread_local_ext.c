/* An extension that leaves a thread-exit destructor on the script's thread,
 * as Rust thread-locals (tokio's, for one) do. Its code must still be mapped
 * when the thread exits, after the runtime has dropped its libraries. */
#include <pthread.h>
#include <string.h>

#include "../../../include/neyuki_ext.h"

static pthread_key_t key;
static volatile int destroyed;

static void on_thread_exit(void *value) {
    (void)value;
    destroyed = 1;
}

/* touch() sets this thread's value, so the destructor runs at its exit. */
static NeyukiExtValue touch(size_t argc, const NeyukiExtValue *argv, void *ud) {
    (void)argc;
    (void)argv;
    (void)ud;
    pthread_setspecific(key, (void *)1);
    return neyuki_bool(1);
}

NEYUKI_EXPORT int32_t neyuki_extension_init(const NeyukiExtApi *host, void *ctx) {
    if (host->abi_version < 1) return 1;
    if (pthread_key_create(&key, on_thread_exit) != 0) return 2;
    if (!host->register_fn(ctx, (const uint8_t *)"touch", 5, touch, NULL)) return 3;
    return 0;
}
