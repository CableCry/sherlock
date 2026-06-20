// bursts.c — allocation activity migrates between call sites over time.
//
//   sherlock testing/bursts.c
//
// Best view: 3 (Heat map). Every ~2 seconds the "hot" call site rotates between
// three phases, so the heat map's rows light up in turn from left (older) to
// right (newer) — a clear visualization of shifting allocation pressure over
// time. Runs until you press q.
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void phase_alpha(void) {
    void *p = malloc(128);
    if (p) {
        memset(p, 0, 128);
        free(p);
    }
}

static void phase_beta(void) {
    void *p = malloc(256);
    if (p) {
        memset(p, 0, 256);
        free(p);
    }
}

static void phase_gamma(void) {
    void *p = malloc(512);
    if (p) {
        memset(p, 0, 512);
        free(p);
    }
}

int main(void) {
    int t = 0;
    for (;;) {
        int phase = (t / 400) % 3; // rotate the active site roughly every 2s
        for (int i = 0; i < 50; i++) {
            switch (phase) {
            case 0:
                phase_alpha();
                break;
            case 1:
                phase_beta();
                break;
            default:
                phase_gamma();
                break;
            }
        }
        usleep(5000);
        t++;
    }
}
