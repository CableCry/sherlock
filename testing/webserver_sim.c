// webserver_sim.c — a large, multi-subsystem workload that exercises every part
// of the profiler at once.
//
//   sherlock testing/webserver_sim.c
//
// It simulates a small web service with several subsystems, each with its own
// call hierarchy and allocation pattern, driven through rotating phases so the
// "hot" subsystem shifts over time. Allocations happen at distinct call sites
// (not funneled through one wrapper), so the flame graph and top-offenders views
// attribute them correctly.
//
// What to look for in each view:
//   1 Live   — every allocator kind in the feed; offenders ranked by live bytes
//   2 Flame  — a tall, forked tower: request handling, cache, pool, templates,
//              config, and sessions each form their own branch of LIVE bytes
//   3 Heat   — rows light up in turn as the workload rotates between phases
//              (serving / media / analytics / maintenance)
//   4 Leaks  — the session store and a rare logger bug leak forever (probable
//              while running, definite/red after exit); the bounded caches and
//              per-request scratch do NOT leak
//
// Memory stays bounded (caches/pools evict and free) except the intentional
// leaks, which grow slowly — so it's safe to leave running while you explore.
// Press q to quit.

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

// --------------------------------------------------------------------------
// Deterministic PRNG (no allocation) — keeps the workload varied but stable.
// --------------------------------------------------------------------------
static uint64_t rng_state = 0x9e3779b97f4a7c15ULL;
static uint32_t rng(void) {
    rng_state = rng_state * 6364136223846793005ULL + 1442695040888963407ULL;
    return (uint32_t)(rng_state >> 33);
}
static uint32_t rng_range(uint32_t n) { return n ? rng() % n : 0; }

// A tiny string-dup used by a couple of subsystems.
static char *dup_str(const char *s) {
    size_t n = strlen(s) + 1;
    char *p = malloc(n);
    if (p) {
        memcpy(p, s, n);
    }
    return p;
}

// --------------------------------------------------------------------------
// Config loader (startup): builds a small, permanent key/value table. Uses
// realloc to grow the table and malloc (via dup_str) for each string. Lives for
// the whole run, so it shows as a stable live branch in the flame graph.
// --------------------------------------------------------------------------
struct kv {
    char *key;
    char *val;
};
static struct kv *g_config;
static int g_config_n;

static void parse_kv(const char *k, const char *v) {
    struct kv *grown = realloc(g_config, (size_t)(g_config_n + 1) * sizeof *g_config);
    if (!grown) {
        return;
    }
    g_config = grown;
    g_config[g_config_n].key = dup_str(k);
    g_config[g_config_n].val = dup_str(v);
    g_config_n++;
}

static void parse_section(int section) {
    for (int i = 0; i < 8; i++) {
        char k[32];
        char v[64];
        snprintf(k, sizeof k, "opt.%d.%d", section, i);
        snprintf(v, sizeof v, "value-%u", rng());
        parse_kv(k, v);
    }
}

static void load_config(void) {
    for (int s = 0; s < 4; s++) {
        parse_section(s);
    }
}

// --------------------------------------------------------------------------
// Template engine: compiled once (kept live), rendered per request. Exercises
// calloc (zeroed token array) plus per-token malloc.
// --------------------------------------------------------------------------
struct template {
    char **tokens;
    int ntok;
};

static struct template *compile_template(int ntok) {
    struct template *t = calloc(1, sizeof *t);
    if (!t) {
        return NULL;
    }
    t->tokens = calloc((size_t)ntok, sizeof *t->tokens);
    if (!t->tokens) {
        free(t);
        return NULL;
    }
    t->ntok = ntok;
    for (int i = 0; i < ntok; i++) {
        t->tokens[i] = malloc(16);
        if (t->tokens[i]) {
            snprintf(t->tokens[i], 16, "tok%d", i);
        }
    }
    return t;
}

// --------------------------------------------------------------------------
// Growable string builder (realloc) used to assemble responses and JSON.
// --------------------------------------------------------------------------
struct sb {
    char *buf;
    size_t len;
    size_t cap;
};

static void sb_append(struct sb *b, const char *s) {
    size_t n = strlen(s);
    if (b->len + n + 1 > b->cap) {
        size_t cap = (b->len + n + 1) * 2;
        char *grown = realloc(b->buf, cap);
        if (!grown) {
            return;
        }
        b->buf = grown;
        b->cap = cap;
    }
    memcpy(b->buf + b->len, s, n);
    b->len += n;
    b->buf[b->len] = '\0';
}

static void render_node(struct sb *b, const char *token) {
    char piece[48];
    snprintf(piece, sizeof piece, "<%s>", token);
    sb_append(b, piece);
}

static char *render_template(const struct template *t) {
    struct sb b = {0};
    sb_append(&b, "<html>");
    for (int i = 0; i < t->ntok; i++) {
        render_node(&b, t->tokens[i]);
    }
    sb_append(&b, "</html>");
    return b.buf; // caller frees
}

// --------------------------------------------------------------------------
// Response cache: a bounded ring that owns its entries and frees on eviction —
// so the most recent ~CACHE_CAP responses are LIVE (a bounded working set the
// flame graph attributes to the request path), but memory never grows.
// --------------------------------------------------------------------------
#define CACHE_CAP 64
static char *g_cache[CACHE_CAP];
static int g_cache_pos;

static void cache_put(char *data) {
    free(g_cache[g_cache_pos]); // evict
    g_cache[g_cache_pos] = data;
    g_cache_pos = (g_cache_pos + 1) % CACHE_CAP;
}

// --------------------------------------------------------------------------
// HTTP request handling: the deepest call chain. Per request it parses headers
// (short-lived, freed), builds a response body + JSON (freed), and caches the
// final response (bounded live set).
// --------------------------------------------------------------------------
static char *parse_header_line(const char *line) { return dup_str(line); }

static void parse_headers(int count) {
    for (int i = 0; i < count; i++) {
        char line[48];
        snprintf(line, sizeof line, "X-Header-%d: %u", i, rng());
        char *h = parse_header_line(line);
        free(h); // headers are transient
    }
}

static char *serialize_json(int fields) {
    struct sb b = {0};
    sb_append(&b, "{");
    for (int i = 0; i < fields; i++) {
        char field[32];
        snprintf(field, sizeof field, "\"k%d\":%u,", i, rng());
        sb_append(&b, field);
    }
    sb_append(&b, "\"ok\":true}");
    return b.buf;
}

static char *build_response(const struct template *t) {
    char *body = render_template(t);
    char *json = serialize_json(4 + (int)rng_range(8));
    struct sb b = {0};
    sb_append(&b, "HTTP/1.1 200 OK\r\n\r\n");
    if (body) {
        sb_append(&b, body);
    }
    if (json) {
        sb_append(&b, json);
    }
    free(body);
    free(json);
    return b.buf;
}

static void handle_connection(const struct template *t) {
    parse_headers(4 + (int)rng_range(6));
    char *response = build_response(t);
    cache_put(response); // cache takes ownership (freed on later eviction)
}

// --------------------------------------------------------------------------
// Connection pool: fixed set of 64-byte-aligned buffers (posix_memalign),
// allocated once and reused — a stable aligned live set.
// --------------------------------------------------------------------------
#define POOL_SZ 16
static void *g_pool[POOL_SZ];

static void pool_init(void) {
    for (int i = 0; i < POOL_SZ; i++) {
        void *p = NULL;
        if (posix_memalign(&p, 64, 4096) == 0) {
            memset(p, 0, 4096);
            g_pool[i] = p;
        }
    }
}

// --------------------------------------------------------------------------
// Media subsystem: decode a "frame" into a SIMD-aligned pixel buffer
// (aligned_alloc), then free it — bursty, transient activity.
// --------------------------------------------------------------------------
static void process_frame(int w, int h) {
    size_t n = (size_t)w * (size_t)h * 4u;
    n = (n + 63u) & ~(size_t)63u; // round up to the alignment
    unsigned char *pixels = aligned_alloc(64, n);
    if (pixels) {
        memset(pixels, 0, n);
        free(pixels);
    }
}

// --------------------------------------------------------------------------
// Analytics subsystem: map/reduce over a large scratch array (calloc), freed
// each job — large transient allocations that spike the heat map.
// --------------------------------------------------------------------------
static long map_reduce(int n) {
    long *scratch = calloc((size_t)n, sizeof *scratch);
    if (!scratch) {
        return 0;
    }
    for (int i = 0; i < n; i++) {
        scratch[i] = (long)rng();
    }
    long acc = 0;
    for (int i = 0; i < n; i++) {
        acc += scratch[i];
    }
    free(scratch);
    return acc;
}

static void run_batch(void) {
    for (int j = 0; j < 8; j++) {
        map_reduce(1024 + (int)rng_range(4096));
        process_frame(64, 64);
    }
}

// --------------------------------------------------------------------------
// Session store: THE leak. Sessions are created and appended forever, never
// expired — a steadily growing leak from a single, easy-to-spot call site.
// --------------------------------------------------------------------------
struct session {
    char id[32];
    void *data;
};
static struct session **g_sessions;
static size_t g_session_n;
static size_t g_session_cap;

static void session_create(void) {
    struct session *s = malloc(sizeof *s);
    if (!s) {
        return;
    }
    snprintf(s->id, sizeof s->id, "sess-%u", rng());
    s->data = malloc(128 + rng_range(256)); // also leaked
    if (g_session_n == g_session_cap) {
        size_t cap = g_session_cap ? g_session_cap * 2 : 16;
        struct session **grown = realloc(g_sessions, cap * sizeof *g_sessions);
        if (!grown) {
            free(s->data);
            free(s);
            return;
        }
        g_sessions = grown;
        g_session_cap = cap;
    }
    g_sessions[g_session_n++] = s; // never freed
}

// --------------------------------------------------------------------------
// Logger: a bounded ring of recent messages (freed on eviction) with a rare
// bug that occasionally drops a message on the floor without freeing it — a
// slow, intermittent leak distinct from the session store.
// --------------------------------------------------------------------------
#define LOG_RING 32
static char *g_logs[LOG_RING];
static int g_log_pos;

static char *format_message(const char *level, int code) {
    char *m = malloc(96);
    if (m) {
        snprintf(m, 96, "[%s] event code=%d ctx=%u", level, code, rng());
    }
    return m;
}

static void log_event(const char *level, int code) {
    char *m = format_message(level, code);
    if (!m) {
        return;
    }
    if (rng_range(1000) < 3) {
        return; // rare bug: message leaked instead of stored
    }
    free(g_logs[g_log_pos]); // evict oldest
    g_logs[g_log_pos] = m;
    g_log_pos = (g_log_pos + 1) % LOG_RING;
}

// --------------------------------------------------------------------------
// Phase scheduler: rotate the dominant subsystem over time so the heat map
// shows the hot rows shifting.
// --------------------------------------------------------------------------
enum phase { PHASE_SERVING, PHASE_MEDIA, PHASE_ANALYTICS, PHASE_MAINTENANCE, PHASE_COUNT };

int main(void) {
    load_config();
    pool_init();
    struct template *tmpl = compile_template(24);
    if (!tmpl) {
        return EXIT_FAILURE;
    }

    for (long tick = 0;; tick++) {
        enum phase phase = (enum phase)((tick / 400) % PHASE_COUNT);
        switch (phase) {
        case PHASE_SERVING:
            for (int i = 0; i < 10; i++) {
                handle_connection(tmpl);
            }
            if (tick % 5 == 0) {
                session_create(); // steady leak under traffic
            }
            log_event("info", (int)tick);
            break;
        case PHASE_MEDIA:
            for (int i = 0; i < 4; i++) {
                process_frame(128, 128);
            }
            handle_connection(tmpl);
            break;
        case PHASE_ANALYTICS:
            run_batch();
            log_event("debug", (int)tick);
            break;
        case PHASE_MAINTENANCE:
            handle_connection(tmpl);
            if (tick % 20 == 0) {
                session_create(); // occasional leak during maintenance
            }
            log_event("warn", (int)tick);
            break;
        case PHASE_COUNT:
            break;
        }
        usleep(3000); // ~3ms/tick; phases rotate roughly every ~1.2s
    }
    // unreachable in normal use (loops until you press q)
}
