// mixed.c — exercises every interposed allocator entry point.
//
//   sherlock testing/mixed.c
//
// Each iteration touches malloc, calloc, realloc, posix_memalign, aligned_alloc,
// and free, so the Live view's "recent events" feed shows every event kind and
// the offenders/flame views attribute them to this call site. Runs until you
// press q. Useful as a general smoke check that all hooks fire.
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    for (;;) {
        char *a = malloc(100);
        char *b = calloc(8, 16); // 128 zeroed bytes
        a = realloc(a, 300);

        void *c = NULL;
        if (posix_memalign(&c, 64, 200) != 0) {
            c = NULL;
        }

        char *d = aligned_alloc(32, 256); // size is a multiple of alignment

        if (a) {
            memset(a, 1, 300);
        }

        free(a);
        free(b);
        free(c);
        free(d);

        usleep(2000);
    }
}
