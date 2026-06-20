//! malloc/free/etc. interposition for an `LD_PRELOAD` allocation profiler.
//! Hooks + the installing `ctor` compile only under `profiler-hooks`; the rest
//! is always compiled so it's testable. See DESIGN.md for the rationale behind
//! the dlsym reentrancy handling, bootstrap arena, stack walk, and crash guard.

use crate::ring_buffer::ShmRingBuffer;
use std::sync::OnceLock;

pub const SHM_NAME: &str = "/profiler_ring";
pub const RING_CAPACITY: usize = 4096;
pub const MAX_STACK_DEPTH: usize = 32;

pub const SHM_NAME_ENV_VAR: &str = "SHERLOCK_PROFILER_SHM";

pub fn resolve_shm_name() -> String {
    std::env::var(SHM_NAME_ENV_VAR).unwrap_or_else(|_| SHM_NAME.to_string())
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    Malloc = 0,
    Free = 1,
    Calloc = 2,
    Realloc = 3,
    PosixMemalign = 4,
    AlignedAlloc = 5,
    Crash = 6,
}

#[derive(Copy, Clone, Default)]
pub struct AllocEvent {
    pub kind: u8,
    pub ptr: usize,
    pub old_ptr: usize,
    pub size: usize,
    pub align: usize,
    pub frames: [usize; MAX_STACK_DEPTH],
    pub frame_count: u8,
}

#[allow(dead_code)]
pub(crate) static RING_BUFFER: OnceLock<ShmRingBuffer<AllocEvent, RING_CAPACITY>> = OnceLock::new();
#[allow(dead_code)]
fn record_event(event: AllocEvent) {
    if let Some(rb) = RING_BUFFER.get() {
        let _ = rb.push(event);
    }
}

/// Walk a frame-pointer chain into `out`, nearest-caller first; returns the
/// frame count. Best-effort heuristics and the fault guard are in DESIGN.md.
///
/// # Safety
/// Dereferences `rbp`; caller guarantees a valid chain or fault recovery.
#[allow(dead_code)]
pub(crate) unsafe fn walk_from_rbp(
    mut rbp: usize,
    skip: usize,
    seed_pc: Option<usize>,
    out: &mut [usize; MAX_STACK_DEPTH],
) -> u8 {
    let mut depth = 0usize;
    if let Some(pc) = seed_pc {
        out[depth] = pc;
        depth += 1;
    }

    for _ in 0..skip {
        if rbp == 0 || !rbp.is_multiple_of(8) {
            return depth as u8;
        }
        let next = unsafe { *(rbp as *const usize) };
        if next <= rbp {
            return depth as u8;
        }
        rbp = next;
    }

    while depth < MAX_STACK_DEPTH {
        if rbp == 0 || !rbp.is_multiple_of(8) {
            break;
        }
        let ret_addr = unsafe { *((rbp as *const usize).add(1)) };
        if ret_addr == 0 {
            break;
        }
        out[depth] = ret_addr;
        depth += 1;

        let next_rbp = unsafe { *(rbp as *const usize) };
        if next_rbp <= rbp || next_rbp - rbp > 0x100_000 {
            break;
        }
        rbp = next_rbp;
    }
    depth as u8
}

#[cfg(feature = "profiler-hooks")]
mod hooks {
    use super::{
        AllocEvent, EventKind, MAX_STACK_DEPTH, RING_BUFFER, RING_CAPACITY, record_event,
        walk_from_rbp,
    };
    use crate::ring_buffer::ShmRingBuffer;
    use ctor::ctor;
    use std::cell::UnsafeCell;
    use std::ffi::{CStr, c_int, c_void};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type MallocFn = unsafe extern "C" fn(usize) -> *mut c_void;
    type FreeFn = unsafe extern "C" fn(*mut c_void);
    type CallocFn = unsafe extern "C" fn(usize, usize) -> *mut c_void;
    type ReallocFn = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
    type PosixMemalignFn = unsafe extern "C" fn(*mut *mut c_void, usize, usize) -> c_int;
    type AlignedAllocFn = unsafe extern "C" fn(usize, usize) -> *mut c_void;

    static REAL_MALLOC: AtomicUsize = AtomicUsize::new(0);
    static REAL_FREE: AtomicUsize = AtomicUsize::new(0);
    static REAL_CALLOC: AtomicUsize = AtomicUsize::new(0);
    static REAL_REALLOC: AtomicUsize = AtomicUsize::new(0);
    static REAL_POSIX_MEMALIGN: AtomicUsize = AtomicUsize::new(0);
    static REAL_ALIGNED_ALLOC: AtomicUsize = AtomicUsize::new(0);

    const BOOTSTRAP_ARENA_SIZE: usize = 64 * 1024;

    #[repr(align(16))]
    struct BootstrapArena(UnsafeCell<[u8; BOOTSTRAP_ARENA_SIZE]>);
    unsafe impl Sync for BootstrapArena {}

    static BOOTSTRAP_ARENA: BootstrapArena =
        BootstrapArena(UnsafeCell::new([0; BOOTSTRAP_ARENA_SIZE]));
    static BOOTSTRAP_OFFSET: AtomicUsize = AtomicUsize::new(0);

    fn bootstrap_base() -> usize {
        BOOTSTRAP_ARENA.0.get() as usize
    }

    fn is_bootstrap_ptr(ptr: *mut c_void) -> bool {
        let base = bootstrap_base();
        let p = ptr as usize;
        p >= base && p < base + BOOTSTRAP_ARENA_SIZE
    }

    fn bootstrap_alloc(size: usize, align: usize) -> *mut c_void {
        let align = align.max(16);
        let base = bootstrap_base();
        loop {
            let current = BOOTSTRAP_OFFSET.load(Ordering::Relaxed);
            let absolute = base + current;
            let aligned_absolute = (absolute + align - 1) & !(align - 1);
            let new_relative_end = (aligned_absolute - base) + size;
            if new_relative_end > BOOTSTRAP_ARENA_SIZE {
                return std::ptr::null_mut();
            }
            if BOOTSTRAP_OFFSET
                .compare_exchange(
                    current,
                    new_relative_end,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return aligned_absolute as *mut c_void;
            }
        }
    }

    fn resolve(name: &CStr) -> usize {
        let sym = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
        sym as usize
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn capture_stack(skip: usize, out: &mut [usize; MAX_STACK_DEPTH]) -> u8 {
        let mut rbp: usize;
        unsafe {
            std::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
        }
        unsafe { guarded_walk(rbp, skip, None, out) }
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn capture_stack(skip: usize, out: &mut [usize; MAX_STACK_DEPTH]) -> u8 {
        let mut fp: usize;
        unsafe {
            std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags));
        }
        unsafe { walk_from_rbp(fp, skip, None, out) }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    unsafe fn capture_stack(_skip: usize, _out: &mut [usize; MAX_STACK_DEPTH]) -> u8 {
        0
    }

    #[cfg(target_arch = "x86_64")]
    mod crash {
        use super::{
            AllocEvent, EventKind, MAX_STACK_DEPTH, record_event, walk_from_rbp,
        };
        use std::cell::UnsafeCell;
        use std::ffi::{c_int, c_void};
        use std::sync::atomic::{AtomicBool, Ordering};

        pub(super) const TRAPPED_SIGNALS: [c_int; 5] = [
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGABRT,
            libc::SIGFPE,
            libc::SIGILL,
        ];

        // Hand-rolled SysV x86_64 setjmp/longjmp. The 8 slots hold, in order:
        //   rbx, rbp, r12, r13, r14, r15, rsp, return address.
        pub(super) const JMP_WORDS: usize = 8;

        #[unsafe(naked)]
        unsafe extern "C" fn fast_setjmp(_buf: *mut u64) -> i32 {
            core::arch::naked_asm!(
                "mov [rdi + 0x00], rbx",
                "mov [rdi + 0x08], rbp",
                "mov [rdi + 0x10], r12",
                "mov [rdi + 0x18], r13",
                "mov [rdi + 0x20], r14",
                "mov [rdi + 0x28], r15",
                "lea rax, [rsp + 8]",
                "mov [rdi + 0x30], rax",
                "mov rax, [rsp]",
                "mov [rdi + 0x38], rax",
                "xor eax, eax",
                "ret",
            )
        }

        #[unsafe(naked)]
        unsafe extern "C" fn fast_longjmp(_buf: *mut u64, _val: i32) -> ! {
            core::arch::naked_asm!(
                "mov rbx, [rdi + 0x00]",
                "mov rbp, [rdi + 0x08]",
                "mov r12, [rdi + 0x10]",
                "mov r13, [rdi + 0x18]",
                "mov r14, [rdi + 0x20]",
                "mov r15, [rdi + 0x28]",
                "mov rsp, [rdi + 0x30]",
                "mov eax, esi",
                "test eax, eax",
                "jnz 2f",
                "mov eax, 1",
                "2:",
                "jmp qword ptr [rdi + 0x38]",
            )
        }

        struct Guard {
            buf: [u64; JMP_WORDS],
            active: bool,
        }

        thread_local! {
            static GUARD: UnsafeCell<Guard> =
                const { UnsafeCell::new(Guard { buf: [0; JMP_WORDS], active: false }) };
        }

        pub(super) unsafe fn guarded_walk(
            rbp: usize,
            skip: usize,
            seed_pc: Option<usize>,
            out: &mut [usize; MAX_STACK_DEPTH],
        ) -> u8 {
            GUARD.with(|cell| {
                let g = cell.get();
                let buf = unsafe { (*g).buf.as_mut_ptr() };
                if unsafe { fast_setjmp(buf) } == 0 {
                    unsafe { std::ptr::write_volatile(&mut (*g).active, true) };
                    let n = unsafe { walk_from_rbp(rbp, skip, seed_pc, out) };
                    unsafe { std::ptr::write_volatile(&mut (*g).active, false) };
                    n
                } else {
                    unsafe { std::ptr::write_volatile(&mut (*g).active, false) };
                    0
                }
            })
        }

        const ALT_STACK_SIZE: usize = 64 * 1024;

        #[repr(align(16))]
        struct AltStack(UnsafeCell<[u8; ALT_STACK_SIZE]>);
        unsafe impl Sync for AltStack {}
        static ALT_STACK: AltStack = AltStack(UnsafeCell::new([0; ALT_STACK_SIZE]));

        static IN_HANDLER: AtomicBool = AtomicBool::new(false);

        fn unblock_trapped_signals() {
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                for &s in &TRAPPED_SIGNALS {
                    libc::sigaddset(&mut set, s);
                }
                libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            }
        }

        fn restore_default(sig: c_int) {
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = libc::SIG_DFL;
                sa.sa_flags = 0;
                libc::sigemptyset(&mut sa.sa_mask);
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }

        extern "C" fn crash_handler(sig: c_int, info: *mut libc::siginfo_t, ctx: *mut c_void) {
            let walking =
                GUARD.with(|cell| unsafe { std::ptr::read_volatile(&(*cell.get()).active) });
            if walking {
                unblock_trapped_signals();
                let buf = GUARD.with(|cell| unsafe { (*cell.get()).buf.as_mut_ptr() });
                unsafe { fast_longjmp(buf, 1) };
            }

            if IN_HANDLER.swap(true, Ordering::SeqCst) {
                restore_default(sig);
                return;
            }

            let uc = ctx as *const libc::ucontext_t;
            let (rip, rbp) = unsafe {
                let gregs = (*uc).uc_mcontext.gregs;
                (
                    gregs[libc::REG_RIP as usize] as usize,
                    gregs[libc::REG_RBP as usize] as usize,
                )
            };
            let fault_addr = unsafe { (*info).si_addr() } as usize;

            let mut frames = [0usize; MAX_STACK_DEPTH];
            let frame_count = unsafe { walk_from_rbp(rbp, 0, Some(rip), &mut frames) };

            record_event(AllocEvent {
                kind: EventKind::Crash as u8,
                ptr: rip,
                old_ptr: fault_addr,
                size: sig as usize,
                align: 0,
                frames,
                frame_count,
            });

            restore_default(sig);
        }

        pub(super) fn install() {
            unsafe {
                let ss = libc::stack_t {
                    ss_sp: ALT_STACK.0.get() as *mut c_void,
                    ss_flags: 0,
                    ss_size: ALT_STACK_SIZE,
                };
                libc::sigaltstack(&ss, std::ptr::null_mut());

                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = crash_handler as *const () as usize;
                sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                libc::sigemptyset(&mut sa.sa_mask);
                for &s in &TRAPPED_SIGNALS {
                    libc::sigaction(s, &sa, std::ptr::null_mut());
                }
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    use crash::guarded_walk;

    #[ctor(unsafe)]
    fn init() {
        REAL_MALLOC.store(resolve(c"malloc"), Ordering::Release);
        REAL_FREE.store(resolve(c"free"), Ordering::Release);
        REAL_CALLOC.store(resolve(c"calloc"), Ordering::Release);
        REAL_REALLOC.store(resolve(c"realloc"), Ordering::Release);
        REAL_POSIX_MEMALIGN.store(resolve(c"posix_memalign"), Ordering::Release);
        REAL_ALIGNED_ALLOC.store(resolve(c"aligned_alloc"), Ordering::Release);

        if let Ok(shm) =
            ShmRingBuffer::<AllocEvent, RING_CAPACITY>::create(&super::resolve_shm_name())
        {
            let _ = RING_BUFFER.set(shm);
        }

        #[cfg(target_arch = "x86_64")]
        crash::install();
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
        let addr = REAL_MALLOC.load(Ordering::Acquire);
        let ptr = if addr == 0 {
            bootstrap_alloc(size, 16)
        } else {
            let real: MallocFn = unsafe { std::mem::transmute(addr) };
            unsafe { real(size) }
        };
        let mut frames = [0usize; MAX_STACK_DEPTH];
        let frame_count = unsafe { capture_stack(1, &mut frames) };
        record_event(AllocEvent {
            kind: EventKind::Malloc as u8,
            ptr: ptr as usize,
            old_ptr: 0,
            size,
            align: 0,
            frames,
            frame_count,
        });
        ptr
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        if is_bootstrap_ptr(ptr) {
            return;
        }
        let addr = REAL_FREE.load(Ordering::Acquire);
        if addr != 0 {
            let real: FreeFn = unsafe { std::mem::transmute(addr) };
            unsafe { real(ptr) };
        }
        let mut frames = [0usize; MAX_STACK_DEPTH];
        let frame_count = unsafe { capture_stack(1, &mut frames) };
        record_event(AllocEvent {
            kind: EventKind::Free as u8,
            ptr: ptr as usize,
            old_ptr: 0,
            size: 0,
            align: 0,
            frames,
            frame_count,
        });
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
        let addr = REAL_CALLOC.load(Ordering::Acquire);
        let total = nmemb.saturating_mul(size);
        let ptr = if addr == 0 {
            let p = bootstrap_alloc(total, 16);
            if !p.is_null() {
                unsafe { std::ptr::write_bytes(p as *mut u8, 0, total) };
            }
            p
        } else {
            let real: CallocFn = unsafe { std::mem::transmute(addr) };
            unsafe { real(nmemb, size) }
        };
        let mut frames = [0usize; MAX_STACK_DEPTH];
        let frame_count = unsafe { capture_stack(1, &mut frames) };
        record_event(AllocEvent {
            kind: EventKind::Calloc as u8,
            ptr: ptr as usize,
            old_ptr: 0,
            size: total,
            align: 0,
            frames,
            frame_count,
        });
        ptr
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        let addr = REAL_REALLOC.load(Ordering::Acquire);
        let new_ptr = if addr == 0 || (!ptr.is_null() && is_bootstrap_ptr(ptr)) {
            let p = bootstrap_alloc(size, 16);
            if !p.is_null() {
                unsafe { std::ptr::write_bytes(p as *mut u8, 0, size) };
            }
            p
        } else {
            let real: ReallocFn = unsafe { std::mem::transmute(addr) };
            unsafe { real(ptr, size) }
        };
        let mut frames = [0usize; MAX_STACK_DEPTH];
        let frame_count = unsafe { capture_stack(1, &mut frames) };
        record_event(AllocEvent {
            kind: EventKind::Realloc as u8,
            ptr: new_ptr as usize,
            old_ptr: ptr as usize,
            size,
            align: 0,
            frames,
            frame_count,
        });
        new_ptr
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn posix_memalign(
        memptr: *mut *mut c_void,
        alignment: usize,
        size: usize,
    ) -> c_int {
        let addr = REAL_POSIX_MEMALIGN.load(Ordering::Acquire);
        let result = if addr == 0 {
            let p = bootstrap_alloc(size, alignment);
            if p.is_null() {
                libc::ENOMEM
            } else {
                unsafe { *memptr = p };
                0
            }
        } else {
            let real: PosixMemalignFn = unsafe { std::mem::transmute(addr) };
            unsafe { real(memptr, alignment, size) }
        };
        if result == 0 {
            let ptr = unsafe { *memptr };
            let mut frames = [0usize; MAX_STACK_DEPTH];
            let frame_count = unsafe { capture_stack(1, &mut frames) };
            record_event(AllocEvent {
                kind: EventKind::PosixMemalign as u8,
                ptr: ptr as usize,
                old_ptr: 0,
                size,
                align: alignment,
                frames,
                frame_count,
            });
        }
        result
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
        let addr = REAL_ALIGNED_ALLOC.load(Ordering::Acquire);
        let ptr = if addr == 0 {
            bootstrap_alloc(size, alignment)
        } else {
            let real: AlignedAllocFn = unsafe { std::mem::transmute(addr) };
            unsafe { real(alignment, size) }
        };
        let mut frames = [0usize; MAX_STACK_DEPTH];
        let frame_count = unsafe { capture_stack(1, &mut frames) };
        record_event(AllocEvent {
            kind: EventKind::AlignedAlloc as u8,
            ptr: ptr as usize,
            old_ptr: 0,
            size,
            align: alignment,
            frames,
            frame_count,
        });
        ptr
    }
}

#[cfg(test)]
mod walk_tests {
    use super::{MAX_STACK_DEPTH, walk_from_rbp};

    fn build_chain(buf: &mut [usize], n: usize) -> usize {
        let base = buf.as_ptr() as usize;
        for i in 0..n {
            let saved_rbp = if i + 1 < n { base + (i + 1) * 16 } else { 0 };
            buf[i * 2] = saved_rbp;
            buf[i * 2 + 1] = 0xC0DE_0000 + i;
        }
        base
    }

    #[test]
    fn follows_a_clean_chain_nearest_caller_first() {
        const FRAMES: usize = 4;
        let mut buf = vec![0usize; FRAMES * 2];
        let base = build_chain(&mut buf, FRAMES);

        let mut out = [0usize; MAX_STACK_DEPTH];
        let n = unsafe { walk_from_rbp(base, 0, None, &mut out) };

        assert_eq!(n as usize, FRAMES);
        for (i, &frame) in out.iter().take(FRAMES).enumerate() {
            assert_eq!(frame, 0xC0DE_0000 + i);
        }
    }

    #[test]
    fn seed_pc_is_recorded_as_the_top_frame() {
        let mut out = [0usize; MAX_STACK_DEPTH];
        let n = unsafe { walk_from_rbp(0, 0, Some(0xDEAD_BEEF), &mut out) };
        assert_eq!(n, 1);
        assert_eq!(out[0], 0xDEAD_BEEF);
    }

    #[test]
    fn stops_gracefully_on_a_backward_link() {
        let mut buf = [0usize; 4];
        let base = buf.as_mut_ptr() as usize;
        unsafe {
            *(base as *mut usize) = base.wrapping_sub(4096);
            *((base as *mut usize).add(1)) = 0x1234;
        }

        let mut out = [0usize; MAX_STACK_DEPTH];
        let n = unsafe { walk_from_rbp(base, 0, None, &mut out) };
        assert_eq!(n, 1);
        assert_eq!(out[0], 0x1234);
    }

    #[test]
    fn rejects_an_unaligned_rbp_without_dereferencing_it() {
        let mut out = [0usize; MAX_STACK_DEPTH];
        let n = unsafe { walk_from_rbp(0x3, 0, None, &mut out) };
        assert_eq!(n, 0);
    }

    #[test]
    fn skip_drops_leading_frames() {
        const FRAMES: usize = 4;
        let mut buf = vec![0usize; FRAMES * 2];
        let base = build_chain(&mut buf, FRAMES);

        let mut out = [0usize; MAX_STACK_DEPTH];
        let n = unsafe { walk_from_rbp(base, 2, None, &mut out) };

        assert_eq!(n as usize, FRAMES - 2);
        assert_eq!(out[0], 0xC0DE_0000 + 2);
        assert_eq!(out[1], 0xC0DE_0000 + 3);
    }
}
