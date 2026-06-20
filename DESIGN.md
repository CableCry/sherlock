# sherlock — design & internals

<sub>[README](README.md) · [Usage & dashboard](docs/USAGE.md) · Design &amp; internals</sub>

Rationale that used to live in source comments. Code carries only terse
`SAFETY:` notes and the asm register layout; everything explanatory is here.

## Architecture

A double-buffered data pipeline across three crates:

- **`profiler` → `libprofiler.so`** — `LD_PRELOAD` hooks intercept
  malloc/free/etc., walk the frame-pointer stack, and push events into a
  lock-free shared-memory ring. Also catches fatal signals.
- **`analyzer` → `analyzer` bin** — drains the ring, resolves addresses to
  `func (file:line)`, tracks live allocations, and draws the TUI.
- **`sherlock` → `sherlock` bin** — orchestrator: builds both, compiles +
  launches the target under the profiler, runs the dashboard, cleans up.

`analyzer` and `sherlock` depend on `profiler` with `default-features = false`.

### `profiler-hooks` feature

Off by default. It exports the `#[no_mangle]` malloc/free/etc. hooks and the
`ctor` that installs them. It **must** stay opt-in: `#[no_mangle]` symbols are
pulled into the final link regardless of use, so any binary linking the
profiler rlib (including `cargo test` and the analyzer) would self-hook the
moment it linked the crate. Only `sherlock`'s explicit
`cargo rustc --crate-type cdylib --features profiler-hooks` build links them.

## Profiler (the `.so`)

**Symbol interposition.** Hooks fetch the real allocator via
`dlsym(RTLD_NEXT, name)` — the next occurrence of the symbol after our own,
i.e. real libc. `REAL_*` are plain `AtomicUsize`, **not** `OnceLock`: glibc's
`dlsym` can itself call `malloc` on first use, re-entering our hook on the same
thread before `dlsym` returns. `OnceLock::get_or_init` detects that reentrancy
and panics; a bare atomic load just sees the `0` "unresolved" sentinel (a real
symbol address is never null) and falls through to the bootstrap arena.

**Bootstrap arena.** A 64 KiB static bump arena (`BOOTSTRAP_ARENA`) serves the
handful of tiny allocations made before the real symbols resolve (chiefly
dlsym's own bookkeeping). Never reclaimed — `free` of a bootstrap pointer is a
no-op; `realloc` of one returns a fresh zeroed block (the original size is
unknown). All access goes through `bootstrap_alloc`, which hands out
non-overlapping ranges via a single atomic bump cursor.

**`AllocEvent`.** `Copy + Default`, every field a plain integer/array, so the
all-zeros bit pattern (what `Default` and a freshly mapped shm page produce) is
always valid. For `EventKind::Crash` the fields are overloaded: `ptr` = faulting
instruction pointer, `old_ptr` = faulting address (`si_addr`), `size` = signal
number, `frames`/`frame_count` = crash backtrace (innermost first).

**Shared-memory name.** `SHM_NAME_ENV_VAR` overrides `SHM_NAME` so concurrent
sessions don't collide. `sherlock` sets it once on its own process before
spawning the target (ctor reads it) and the analyzer (reads it via
`resolve_shm_name`), so both ends agree without it on every command line.

### Stack walking

`capture_stack` reads the live frame pointer (`rbp` on x86_64, `x29` on
aarch64) and hands it to `walk_from_rbp`, which chases the saved-fp chain
nearest-caller-first, capped at `MAX_STACK_DEPTH`. The frame layout (saved fp at
`[fp]`, return address at `[fp+8]`) is identical on both arches.

Frame pointers are forced workspace-wide via `.cargo/config.toml`, so *our*
frames are always walkable; whether the walk reaches further depends on the
target having frame pointers too (it's built with `-fno-omit-frame-pointer`).

The walk is best-effort: there's no cheap way to verify a "saved rbp" slot is a
real frame pointer vs. garbage. Heuristics catch the common case — the chain
must stay 8-byte aligned and move strictly *upward* (stack grows down) by less
than 1 MiB per step, else we've wandered off and stop with a shorter,
still-valid trace. `seed_pc`, when set, becomes `frames[0]` (the crash handler
seeds the faulting PC); `skip` steps over leading frames so a caller reading its
own `rbp` can drop its own/intermediate frames.

### Crash hardening (x86_64 only)

Heuristics reduce but can't eliminate faulting on a corrupted chain, so the hot
path wraps the walk in a `setjmp`/`longjmp` trampoline, plus a fatal-signal
handler records a final crash event before the target dies. aarch64 has the
walk but neither guard nor handler, so it relies on the heuristics alone.

**Hand-rolled `setjmp`/`longjmp`.** `libc` doesn't export
`sigsetjmp`/`siglongjmp` on Linux, so `fast_setjmp`/`fast_longjmp` are naked asm
doing the SysV x86_64 save/restore. The 8 `JMP_WORDS` slots hold, in order:
`rbx, rbp, r12, r13, r14, r15, rsp, return address`. `fast_setjmp` records its
*caller's* continuation (return address on the stack, rsp just above it), so a
later `fast_longjmp` resumes inside that caller. The only caller is
`guarded_walk`, whose frame stays live while the walk runs, so jumping back into
it from the signal handler is well defined.

**Signal handler.** Two cases:
1. If the per-thread `GUARD.active` flag is set, *we* faulted mid-walk — not a
   target crash. Unblock the trapped signals (longjmp skips the kernel sigreturn
   that would do it) and `fast_longjmp` back into `guarded_walk`, returning a
   partial trace.
2. Otherwise it's a genuine target crash. An `IN_HANDLER` swap guards against
   re-entry (if recording itself faults, we fall straight to the default
   disposition). Pull `rip`/`rbp` from the `ucontext_t`, walk the crash stack
   (unguarded — `IN_HANDLER` already covers a fault here), push a `Crash` event,
   then restore the default handler so the host core-dumps naturally.

Async-signal-safety: the handler only does already-resolved TLS reads, an atomic
swap, and a lock-free ring push — no malloc/printf/mutex. It runs on a dedicated
`sigaltstack` so stack-overflow faults are still handled.

**TLS caveat.** `GUARD` is const-initialized and destructor-free, so it compiles
to a direct `#[thread_local]` access with no lazy-init guard, and the hot path
touches it every `capture_stack` so it's resolved before any signal reads it. A
fully airtight guarantee would need initial-exec TLS (nightly `#[thread_local]`
or a C TU) — a known residual caveat.

`init` (the ctor) resolves every real symbol first (so allocations *during*
resolution fall back to the arena), creates the ring, then installs the signal
handler **last** so the ring it writes into already exists.

## Ring buffer

Lock-free SPSC. `head`/`tail`/`dropped` are each `#[repr(align(64))]`
(`Padded`) so they sit on separate cache lines: the producer writes
tail/dropped while the consumer writes head, and we don't want false sharing.

Orderings: producer publishes with `tail.store(Release)` after writing the slot;
consumer reads `tail` with `Acquire` before reading the slot (and vice-versa for
`head`). A full ring drops the event and bumps `dropped` rather than
back-pressuring the target — overflow is visible, never silent, and the target
is never perturbed.

`init_in_place` constructs directly into already-mapped memory (never building a
huge `[T; N]` on the stack — that used to overflow). `ShmRingBuffer` maps a
POSIX shm segment (`0o600`, owner-only — all components run as the same user);
`open` fstat-checks the segment size against `size_of::<RingBuffer>()` to catch
a type/capacity mismatch between processes.

## Analyzer

**Two threads.** A drain thread owns the ring, `Tracker`, and `Resolver` and
drains as fast as it can, publishing an immutable `Snapshot` into a mutex slot
on a fixed cadence. The UI thread reads the latest snapshot at ~30 FPS and feeds
input back through atomic `Controls`. Render throttling can never hold up
draining. Publish cadence matches the UI draw cadence (33 ms) — publishing
faster just builds snapshots nobody reads. The `Resolver` holds `Rc`/`RefCell`
(not `Send`), so it's built inside the drain thread.

While paused, draining stops so the inspected snapshot can't scroll away; the
ring keeps filling and drops rather than back-pressuring.

### Tracker

- **Memoized resolution.** Resolving a DWARF frame dwarfs everything else in
  `apply`, and the same call sites repeat constantly, so resolved chains are
  cached by address.
- **Stale-reuse reconciliation.** The allocator reuses a freed pointer's address
  immediately. If an alloc lands on an address we still hold active, the freeing
  event was dropped under load — reconcile then, or `active_bytes` only grows.
- **realloc.** A null result means realloc failed and the original block is
  untouched (C semantics), so bookkeeping only moves on success.
- **Offender depth.** Offenders aggregate on the frame at `offender_depth`
  (0 = immediate caller), fixed per session so the key is stable and we needn't
  retain every active alloc's full stack to re-key it; only the bounded `recent`
  feed keeps raw frames (for the call-stack viewer).
- **Crash events** decode with the full backtrace resolved (each raw frame
  expanded through any inlined chain), not just the top frame.
- **Stats.** `peak_bytes` (high-water mark), `total_allocated` (cumulative,
  never decremented), alloc/free counts, `temporary_count` (frees whose alloc
  lived ≤ one tick — a coarse churn proxy; true lifetime needs per-event
  timestamps), and a `heap_series` sampled once per heat tick for the header
  sparkline.
- **Heat** advances on a wall-clock tick (independent of event volume) so each
  column is a fixed time slice.

### Resolver

Built from a snapshot of `/proc/<pid>/maps` taken while the target is alive.
Every file-backed mapping (target + libc + libstdc++ + any shared lib) gets its
own `addr2line::Loader` and a load base, built lazily on first hit (most libs
are never touched). The load base is the lowest mapped start minus the file's
lowest ELF segment `p_vaddr` (0 for PIE/`.so`, nonzero for fixed `ET_EXEC`) —
this defeats ASLR and keeps working after the target exits (when
`/proc/<pid>/maps` is gone), which is exactly when the crash trace resolves.
`resolve` returns the full inlined-frame chain for one PC (innermost first);
empty means raw-hex fallback.

### Dashboard

Pure presentation off an immutable `Snapshot`. Views: **live** (event feed + top
offenders, or a selected event's call stack), **flame** (active bytes by call
stack, width & color = share of live heap), **heat** (call-site × time grid),
**leaks** (live allocs by call site; definite once the target exits). Header
carries live metrics + a heap-over-time sparkline; footer carries per-view
legends; `?` opens help.

### Export

`Tracker::folded_stacks` emits live allocations as collapsed stacks
(`outer;…;inner <bytes>`, identical paths aggregated) — the format
`flamegraph.pl` and speedscope read directly. Written on exit via `--export`;
`sherlock` wires it to a `report.folded` automatically.
