# testing — sample targets for the profiler

Small programs that each exercise a different part of the dashboard. Run any of
them with:

```
sherlock testing/<file>
```

`sherlock` compiles each with `-g -fno-omit-frame-pointer -O0`, launches it under
the profiler, and opens the dashboard. Press `q` to quit (and to stop the
long-running ones). Switch views with `Tab` or `1`/`2`/`3`/`4`.

| Program | Best view | What it demonstrates |
| --- | --- | --- |
| `leaks.c` | `4` Leaks | Distinct leaking sites (many small + a few large) vs. a balanced one. Leak sites show as *probable* leaks within ~3s while running, then become *definite* (red) once it **exits** (~10s). |
| `flame.c` | `2` Flame | A deep, forked call hierarchy (`compile`/`render`) with allocations at many depths — a tall, branching flame graph. |
| `bursts.c` | `3` Heat map | The hot call site rotates every ~2s, so heat-map rows light up in turn over time. |
| `mixed.c` | `1` Live | Touches every interposed allocator (`malloc`/`calloc`/`realloc`/`posix_memalign`/`aligned_alloc`/`free`) — every event kind in the feed. |
| `crash_uaf.c` | crash panel | Allocates, frees, then dereferences NULL a few frames deep — the crash handler captures the final backtrace and the red CRASH panel appears. Terminates with SIGSEGV. |
| `cpp_workload.cpp` | `1`/`4` | C++ `new`/`std::string`/`std::vector` — demangled C++ names and resolved libstdc++/libc frames; leaks one cache on purpose. |
| `webserver_sim.c` | all | **The big one.** A multi-subsystem web-service simulation (config, templates, request handling, cache, connection pool, media, analytics, sessions, logging) driven through rotating phases. Exercises every allocator kind, a deep forked flame graph, a shifting heat map, bounded live working sets, and two distinct growing leaks (session store + a rare logger bug). |

Notes:
- The long-running targets (`flame`, `bursts`, `mixed`, `cpp_workload`) loop until
  you quit, so you can switch views and watch them live.
- Leaks are flagged as *probable* once they've been alive longer than
  `--leak-age` seconds (default 3); after the target exits, every survivor is a
  *definite* leak. Press `?` in the dashboard for a key + color legend.
- `flame.c` keeps a bounded working set alive on purpose — the flame graph shows
  *live* bytes, so a program that frees everything immediately would graph empty.
