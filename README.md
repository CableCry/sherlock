<div align="center">


# sherlock

<p>
  <strong>Live, low-overhead heap profiler for C/C++.</strong><br>
  <code> LD_PRELOAD</code> in → real-time flame graphs, leak detection, and crash backtraces out.
</p>

<p>
  <img src="https://img.shields.io/badge/Rust-2024-orange?logo=rust" alt="Rust 2024">
  <img src="https://img.shields.io/badge/platform-Linux-blue?logo=linux&logoColor=white" alt="Linux">
  <img src="https://img.shields.io/badge/arch-x86__64%20%7C%20aarch64-lightgrey" alt="x86_64 | aarch64">
  <img src="https://img.shields.io/badge/TUI-ratatui-8A2BE2" alt="ratatui">
  <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0">
</p>

---

<img src="assets/demo.gif" alt="sherlock dashboard" width="960">

</div>

---

## What it is

Point sherlock at a C/C++ source file and it compiles it, runs it under an
allocator-interposing profiler, and opens a live terminal dashboard showing
exactly where your heap memory is going — as it happens.

```sh
sherlock <program.c|program.cpp> [extra compiler flags...]
```

One command: compiles the target with the right flags, launches it under the
profiler, and attaches the dashboard. Press `q` to quit.

- 🔥 **Flame graph** of live heap by call stack — width *and* color = share of memory.
- 🩸 **Leak detection** — probable while running, definite once the target exits.
- 🌡️ **Heat map** of allocation activity per call site over time.
- 💥 **Crash backtraces** — catches fatal signals and records a final trace before the core dump.
- 📈 **Live stats** — peak heap, cumulative bytes, churn, and a heap-over-time sparkline.
- 📤 **Export** to folded stacks for `flamegraph.pl` / [speedscope](https://speedscope.app).

Near-zero target overhead: symbol resolution and rendering happen in a separate
process, so the profiled program is never made to wait.

## Quick start

**Prerequisites:** a Rust toolchain (`cargo`), a C/C++ compiler (`cc` / `c++`),
and Linux (x86_64 for the full feature set; aarch64 supported with caveats).

### Install

```sh
git clone https://github.com/CableCry/sherlock && cd sherlock
cargo install --path sherlock
```

This puts the `sherlock` command on your `PATH` (in `~/.cargo/bin`).

> **Keep the clone.** `sherlock` rebuilds `libprofiler.so` and the analyzer from
> source on each run, so the checkout must stay reachable. If you move it, point
> `SHERLOCK_WORKSPACE` at the new location:
> ```sh
> export SHERLOCK_WORKSPACE=/path/to/sherlock
> ```

### Run

```sh
# Profile a bundled fixture
sherlock testing/leaks.c

# Your own program, passing extra compiler flags through
sherlock myprog.c -O2 -lpthread
```

Prefer not to install? Run straight from the checkout with
`cargo run -p sherlock -- testing/leaks.c`.

The dashboard opens attached to the running target. See
**[docs/USAGE.md](docs/USAGE.md)** for every panel, view, and key.

## How it works

A **double-buffered data pipeline** across three crates:

| Crate | Output | Role |
| --- | --- | --- |
| `profiler` | `libprofiler.so` | `LD_PRELOAD` hooks; walks the frame-pointer stack; writes events to shared memory; catches fatal signals |
| `analyzer` | `analyzer` (bin) | drains the ring buffer, resolves addresses to `func (file:line)`, tracks live allocations, draws the TUI |
| `sherlock` | `sherlock` (bin) | orchestrator: builds everything, compiles + launches the target, runs the dashboard |

The **producer** (`libprofiler.so`) records each event plus a backtrace into a
lock-free SPSC ring buffer in POSIX shared memory — no symbol resolution on the
hot path. The **consumer** (`analyzer`) drains that ring on a dedicated thread
that never blocks on rendering, while the UI thread reads immutable snapshots at
~30 FPS.

Full rationale (dlsym reentrancy, crash guard, memory orderings, ASLR
resolution, …) is in **[DESIGN.md](DESIGN.md)**.

## Performance

### Results

2-core Linux VM, target built with `-O2 -fno-builtin -fno-omit-frame-pointer`:

| Allocator calls / sec | Run | Baseline | With sherlock | Overhead | Events captured |
| --- | --- | --- | --- | --- | --- |
| **~440K** (realistic heavy workload) | 1 | 4,538.7 ns/iter | 4,455.6 ns/iter | −1.8% (noise) | 97.7% |
| | 2 | 4,582.4 ns/iter | 4,383.0 ns/iter | −4.4% (noise) | 98.3% |
| **~4.5M** (allocation-heavy) | 1 | 448.7 ns/iter | 531.9 ns/iter | +18.5% | 53.4% |
| | 2 | 446.0 ns/iter | 557.6 ns/iter | +25.0% | 53.7% |
| **~200M** (tight malloc/free loop) | 1 | 10.2 ns/iter | 37.3 ns/iter | ~14 ns per call | 3.7% |
| | 2 | 9.6 ns/iter | 69.2 ns/iter | ~30 ns per call | 9.5% |

### What it means

- **At realistic allocation rates the overhead is below the noise floor.** At ~440K calls/sec,
  both runs came out slightly *faster* under sherlock, so the true cost is under ~2%, and ~98%
  of events reach the analyzer.
- **Each hooked call costs roughly 15–30 ns.** It's mostly the 32-frame stack walk plus one copy
  into the 4096-slot ring. Symbol resolution and rendering never happen in the target process.
- **The drain rate is the limit.** Above a few million calls/sec the ring fills faster than the
  consumer empties it. The producer never blocks: events are dropped and counted, so the
  dashboard totals stay honest.

## Building & testing

```sh
cargo build                 # build the workspace
cargo test                  # unit tests: ring buffer, stack walker, tracker, resolver
./bench/run.sh              # overhead benchmark (see Performance below)
```

## Limitations

Stack walking is frame-pointer based (sherlock builds targets with
`-fno-omit-frame-pointer`); stripped or frame-pointer-omitting libraries may
truncate or show raw `0x…` addresses. The crash handler and crash-proof walk are
**x86_64-only** (aarch64 has the walk but not the handler). Under extreme
allocation rates the ring can fill faster than it drains — dropped events are
counted, never silently lost. See [docs/USAGE.md](docs/USAGE.md#limitations) for
the full list.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
