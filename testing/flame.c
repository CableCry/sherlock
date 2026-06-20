// flame.c — a deep, branching call hierarchy.
//
//   sherlock testing/flame.c
//
// Best view: 2 (Flame graph). The flame graph shows *live* (still-allocated)
// bytes grouped by call stack, so this program keeps a bounded working set alive
// instead of freeing immediately: each allocation is parked in a rotating slot
// and only freed when its slot is reused. At steady state ~LIVE allocations from
// every call path are live at once, so the graph shows a tall, forked tower
// (a "compile" pipeline and a "render" loop) whose widths reflect where bytes
// actually sit. Runs until you press q.
//
//   main
//   ├── compile → parse_stmt → parse_expr → lex → leaf_alloc
//   └── render  → render_row → leaf_alloc
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

// Bounded live working set: keep recent allocations alive so the flame graph
// (active bytes) has something to show, while keeping total memory bounded.
#define LIVE 256
static void *live[LIVE];
static int live_idx;

static void leaf_alloc(size_t n) {
    void *p = malloc(n);
    if (p) {
        memset(p, 0, n);
    }
    free(live[live_idx]); // evict the allocation currently in this slot
    live[live_idx] = p;
    live_idx = (live_idx + 1) % LIVE;
}

static void lex(void) {
    for (int i = 0; i < 3; i++) {
        leaf_alloc(64);
    }
}

static void parse_expr(void) {
    lex();
    leaf_alloc(128);
}

static void parse_stmt(void) {
    parse_expr();
    parse_expr();
    leaf_alloc(256);
}

static void compile(void) {
    parse_stmt();
    leaf_alloc(512);
}

static void render_row(void) {
    leaf_alloc(96);
}

static void render(void) {
    for (int i = 0; i < 4; i++) {
        render_row();
    }
}

int main(void) {
    for (;;) {
        compile(); // deep branch
        render();  // shallower branch
        usleep(500);
    }
}
