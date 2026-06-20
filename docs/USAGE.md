# Usage & dashboard

Reference for the live dashboard and the CLI. For architecture and internals see
[DESIGN.md](../DESIGN.md); for a quick start see the [README](../README.md).

## Running

```sh
sherlock <program.c|program.cpp> [extra compiler flags...]
```

`sherlock` compiles the target with `-g -fno-omit-frame-pointer -O0` (plus any
flags you pass through), launches it under `libprofiler.so`, and attaches the
dashboard. Press `q` to quit.

## Dashboard

```
┌ sherlock profiler ─────────────────────────────────────────────────┐
│ status: running  active: 128 (4096 B)  total: 90210  dropped: 0 …   │
│ heap ▁▂▃▅▇█  peak 8192 B  allocated 1.2M B  short-lived 12%  …       │
└────────────────────────────────────────────────────────────────────┘
┌ recent events ──────────────────┐┌ top offenders (active bytes) ────┐
│ #90210 malloc ptr=0x… size=64 … ││ ████████████  4096  do_work (a.c… │
│ #90209 free   ptr=0x… size=0  … ││ ███          1024  parse (b.c:…)  │
└─────────────────────────────────┘└──────────────────────────────────┘
 q quit · space pause · ↑/↓ inspect call stack · ? help
```

- **Header** — live metrics over two lines. Top: target status, active
  pointer/byte counts, total events processed, dropped events, the offender
  aggregation depth, and a `PAUSED` badge when frozen. Bottom: a live-heap
  **timeline sparkline** (~last 60s) plus the **peak** live heap, **cumulative
  bytes allocated**, the **short-lived** fraction (allocated and freed within one
  ~250ms tick — a churn proxy), and total alloc/free counts.
- **Recent events** — a color-coded firehose (green allocs, gray frees, red
  crash), newest first.
- **Top offenders** — call sites ranked by active bytes, drawn as a heat-map bar
  (red = heaviest / most leak-prone, then yellow, then green).
- **Call stack** — replaces the offenders panel when you select an event; shows
  that event's full resolved backtrace (including inlined frames).
- **CRASH panel** — appears in red if the target hits a fatal signal, with the
  signal, faulting address, PC, and crash backtrace.

## Views

Switch the main view with `Tab` or the number keys; the header/crash panel stay put.

- **`1` Live** — the recent-events feed + top-offenders heat bars (above).
- **`2` Flame graph** — an icicle of *active* bytes aggregated by full call stack
  (outermost frame on top). Each frame's width **and** color is its share of live
  heap, so wide red towers are where memory is sitting; wide bars show `NN%`.
- **`3` Heat map** — call site × time grid; each cell is allocation activity in a
  ~250 ms bucket over a rolling ~12 s window (left = older, right = newer),
  shaded green→yellow→red by intensity. Shows *bursts* and steady allocators.
- **`4` Leaks** — allocations still alive aggregated by call site, with a heat bar
  and age in seconds. While the target runs these are *probable* leaks (alive
  longer than `--leak-age` seconds, default 3); once the target **exits, every
  survivor is a definite leak** and the panel turns red.

## Keys

| Key | Action |
| --- | --- |
| `q`, `Ctrl-C` | quit |
| `1`/`2`/`3`/`4`, `Tab` | switch view (live / flame / heat / leaks) |
| `space` | pause/resume draining (freeze the view) |
| `↑`/`↓` (or `k`/`j`) | select an event and show its call stack (Live view) |
| `Esc` | clear selection (return to live offenders) |
| `?` / `h` | toggle the key help |

While **paused**, draining stops so the snapshot you're inspecting can't scroll
away. The target keeps running and the ring buffer keeps filling, so events may
be dropped (reflected in the `dropped` counter) rather than slowing the target.

## Running the analyzer directly

`sherlock` wires everything up for you, but the analyzer can attach to any
profiled process:

```sh
analyzer --binary <path> --pid <pid> --shm-name <name> [options]
```

- `--offender-depth N` aggregates "top offenders" on the Nth stack frame
  (`0` = immediate caller, the default; higher values group by a common ancestor
  call site).
- `--leak-age N` marks allocations still alive after `N` seconds as probable
  leaks in the Leaks view (default `3`). Survivors after the target exits are
  always treated as definite leaks regardless of this value.
- `--export PATH` on exit writes the live allocations as collapsed/folded stacks
  (`outer;…;inner <bytes>` per line) — the format `flamegraph.pl` and
  [speedscope](https://speedscope.app) read directly. `sherlock` sets this
  automatically and prints the path (a `report.folded` in its run directory)
  when it finishes.

### Headless drain

For scripting/CI there's a non-TUI drain that prints a summary and any crash
events, then exits:

```sh
cargo run -p analyzer --example dump <shm-name> [seconds]
```

## Limitations

- **Symbol resolution** covers every file-backed mapping (the target plus libc,
  libstdc++, and other shared libraries) using each file's own DWARF/symbol info.
  Stripped libraries with no symbols still show as raw `0x…` addresses.
- **Stack walking** is frame-pointer based, so the target must be built with
  `-fno-omit-frame-pointer` (sherlock does this for you). Frames in libraries
  compiled without frame pointers may be truncated.
- **Architectures**: full support (crash handler + crash-proof stack walking) on
  **x86_64**. aarch64 has the frame-pointer walk but not the crash handler yet.
- **Drop counter**: under extreme allocation rates the single ring buffer can
  fill faster than the analyzer drains it; dropped events are counted, not
  silently lost, so the displayed totals stay honest.
- Shared-memory segments are created `0o600` (owner only) — all components run as
  the same user.
