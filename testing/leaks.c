// leaks.c — distinct leaking and well-behaved call sites.
//
//   sherlock testing/leaks.c
//
// Best view: 4 (Leaks). Two leak sites never free their memory, so within a few
// seconds they show up as *probable* leaks (alive longer than --leak-age, 3s by
// default) while the program runs; once it exits (~10s) every survivor becomes a
// *definite* leak and the panel turns red. The well-behaved site frees correctly
// and never appears. The Live "top offenders" and the flame graph (2) also show
// the leak sites accumulating.
//
// Three call sites with different leak profiles:
//   - cache_insert:   many small (256 B) leaks
//   - load_blob:      a few large (64 KiB) leaks
//   - handle_request: allocates and frees correctly (should NOT appear in leaks)
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

// Leaked: 256 bytes every call, never freed.
static void cache_insert(int key) {
    char *entry = malloc(256);
    if (entry) {
        memset(entry, key & 0xff, 256);
    }
    // intentionally leaked
}

// Leaked: a big buffer, occasionally. The previous pointer is overwritten, so
// each call leaks another 64 KiB.
static char *g_sink;
static void load_blob(void) {
    g_sink = malloc(64 * 1024);
    if (g_sink) {
        memset(g_sink, 0, 64 * 1024);
    }
}

// Well-behaved: balanced malloc/free.
static void handle_request(void) {
    char *buf = malloc(1024);
    if (buf) {
        memset(buf, 1, 1024);
        free(buf);
    }
}

int main(void) {
    for (int tick = 0; tick < 2000; tick++) {
        handle_request();
        handle_request();
        if (tick % 4 == 0) {
            cache_insert(tick); // steady stream of small leaks
        }
        if (tick % 200 == 0) {
            load_blob(); // occasional large leak
        }
        usleep(5000); // ~10s total runtime
    }
    return 0; // on exit, everything still live is a definite leak
}
