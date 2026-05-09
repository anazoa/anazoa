#define _GNU_SOURCE
#include <dlfcn.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>

typedef void SSL_CTX;
typedef void SSL_METHOD;
typedef void SSL;
typedef SSL_CTX *(*SSL_CTX_new_fn)(const SSL_METHOD *);
typedef void    (*SSL_CTX_set_keylog_callback_fn)(SSL_CTX *, void (*)(const SSL *, const char *));

static FILE *kf;
static FILE *dbg;

/* real_dlsym — resolved once via dlvsym to avoid infinite recursion */
static void *(*real_dlsym_fn)(void *, const char *);

static void ensure_real_dlsym(void) {
    if (!real_dlsym_fn)
        real_dlsym_fn = dlvsym(RTLD_NEXT, "dlsym", "GLIBC_2.2.5");
}

static void keylog_cb(const SSL *ssl, const char *line) {
    if (!kf) {
        const char *path = getenv("SSLKEYLOGFILE");
        if (path) kf = fopen(path, "a");
    }
    if (kf) { fprintf(kf, "%s\n", line); fflush(kf); }
}

/*
 * Exported SSL_CTX_new — intercepts both:
 *   - PLT-level calls from libcall-service.so and other standard-linked consumers
 *   - dlsym("SSL_CTX_new") lookups from Qt's TLS plugin (via our dlsym hook below)
 */
SSL_CTX *SSL_CTX_new(const SSL_METHOD *m) {
    static SSL_CTX_new_fn              real_new;
    static SSL_CTX_set_keylog_callback_fn real_keylog;
    static int initialized;

    if (!dbg) dbg = fopen("/tmp/hook_debug.txt", "a");

    if (!initialized) {
        initialized = 1;
        ensure_real_dlsym();
        if (real_dlsym_fn) {
            real_new    = (SSL_CTX_new_fn)real_dlsym_fn(RTLD_NEXT, "SSL_CTX_new");
            real_keylog = (SSL_CTX_set_keylog_callback_fn)
                          real_dlsym_fn(RTLD_NEXT, "SSL_CTX_set_keylog_callback");
        }
        if (dbg) {
            fprintf(dbg, "SSL_CTX_new init: real_new=%p real_keylog=%p\n",
                    (void*)real_new, (void*)real_keylog);
            fflush(dbg);
        }
    }

    if (dbg) { fprintf(dbg, "SSL_CTX_new called\n"); fflush(dbg); }

    if (!real_new) return NULL;
    SSL_CTX *ctx = real_new(m);
    if (ctx && real_keylog)
        real_keylog(ctx, keylog_cb);
    return ctx;
}

/*
 * dlsym hook — intercepts Qt TLS plugin's batch dlsym lookups.
 * When it asks for SSL_CTX_new we return our own SSL_CTX_new above.
 */
void *dlsym(void *handle, const char *name) {
    ensure_real_dlsym();
    if (!real_dlsym_fn) return NULL;

    void *result = real_dlsym_fn(handle, name);
    if (!name) return result;

    if (!dbg) dbg = fopen("/tmp/hook_debug.txt", "a");
    if (dbg) { fprintf(dbg, "dlsym: %s\n", name); fflush(dbg); }

    if (strcmp(name, "SSL_CTX_new") == 0) {
        /* Return our wrapper so Qt's plugin calls it through us */
        return (void *)SSL_CTX_new;
    }
    return result;
}
