//! Headless drain of the profiler ring buffer — prints a summary and any crash
//! events, then exits. Used for smoke-testing the pipeline without the TUI.
//!
//! Usage: dump <shm-name> [seconds]

use profiler::injection::{AllocEvent, RING_CAPACITY, resolve_shm_name};
use profiler::ring_buffer::ShmRingBuffer;
use std::collections::HashSet;
use std::time::{Duration, Instant};

fn main() {
    let shm = std::env::args().nth(1).unwrap_or_else(resolve_shm_name);
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let rb = loop {
        if let Ok(rb) = ShmRingBuffer::<AllocEvent, RING_CAPACITY>::open(&shm) {
            break rb;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut total = 0u64;
    let mut crashes = 0u64;
    let mut live: HashSet<usize> = HashSet::new();
    while Instant::now() < deadline {
        if let Some(ev) = rb.pop() {
            total += 1;
            match ev.kind {
                0 | 2 | 4 | 5 => {
                    live.insert(ev.ptr);
                }
                1 => {
                    live.remove(&ev.ptr);
                }
                3 => {
                    if ev.old_ptr != 0 {
                        live.remove(&ev.old_ptr);
                    }
                    if ev.ptr != 0 {
                        live.insert(ev.ptr);
                    }
                }
                6 => {
                    crashes += 1;
                    println!(
                        "CRASH  signal={} fault_addr=0x{:x} pc=0x{:x} frames={}",
                        ev.size, ev.old_ptr, ev.ptr, ev.frame_count
                    );
                }
                _ => {}
            }
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    live.remove(&0);
    println!(
        "total events: {total}, crashes: {crashes}, live pointers: {}, dropped: {}",
        live.len(),
        rb.dropped_count()
    );
    std::process::exit(if crashes > 0 { 0 } else { 1 });
}
