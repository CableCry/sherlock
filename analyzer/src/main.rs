//! Analyzer entry point: a drain thread (owns ring/tracker/resolver, publishes
//! snapshots) and the UI thread (reads snapshots, handles input). See DESIGN.md.

mod dashboard;
mod resolver;
mod state;
mod tracker;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use profiler::injection::{AllocEvent, RING_CAPACITY, resolve_shm_name};
use profiler::ring_buffer::ShmRingBuffer;
use resolver::Resolver;
use state::{Controls, MAX_SELECT, SelectedView, Snapshot, ViewMode, kind_name};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracker::Tracker;

type SnapshotSlot = Arc<Mutex<Arc<Snapshot>>>;

const DEFAULT_LEAK_AGE_SECS: u64 = 3;

struct Args {
    binary: Option<PathBuf>,
    pid: Option<u32>,
    shm_name: Option<String>,
    offender_depth: usize,
    leak_age_secs: u64,
    export: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = Args {
        binary: None,
        pid: None,
        shm_name: None,
        offender_depth: 0,
        leak_age_secs: DEFAULT_LEAK_AGE_SECS,
        export: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--binary" => args.binary = it.next().map(PathBuf::from),
            "--pid" => args.pid = it.next().and_then(|s| s.parse().ok()),
            "--shm-name" => args.shm_name = it.next(),
            "--offender-depth" => {
                args.offender_depth = it.next().and_then(|s| s.parse().ok()).unwrap_or(0)
            }
            "--leak-age" => {
                args.leak_age_secs =
                    it.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_LEAK_AGE_SECS)
            }
            "--export" => args.export = it.next().map(PathBuf::from),
            other => eprintln!("analyzer: ignoring unrecognized argument {other}"),
        }
    }
    args
}

fn secs_to_ticks(secs: u64) -> u64 {
    (secs * 1000 / state::HEAT_TICK_MS).max(1)
}

fn target_status(pid: Option<u32>) -> &'static str {
    match pid {
        None => "standalone",
        Some(pid) if Path::new(&format!("/proc/{pid}")).exists() => "running",
        Some(_) => "exited",
    }
}

fn main() {
    let args = parse_args();
    let shm_name = args.shm_name.clone().unwrap_or_else(resolve_shm_name);

    eprintln!("analyzer: waiting for shared ring buffer at {shm_name}...");
    let rb = loop {
        match ShmRingBuffer::<AllocEvent, RING_CAPACITY>::open(&shm_name) {
            Ok(rb) => break rb,
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    };

    let controls = Arc::new(Controls::new());
    let slot: SnapshotSlot = Arc::new(Mutex::new(Arc::new(Snapshot::default())));

    let drain_controls = controls.clone();
    let drain_slot = slot.clone();
    let binary = args.binary.clone();
    let pid = args.pid;
    let depth = args.offender_depth;
    let leak_age_ticks = secs_to_ticks(args.leak_age_secs);
    let export = args.export.clone();

    let drain = std::thread::spawn(move || {
        let (resolver, note) = match (&binary, pid) {
            (Some(b), Some(p)) => match Resolver::new(b, p) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(format!("symbols unavailable: {e}"))),
            },
            _ => (None, Some("running without symbols (raw addresses)".to_string())),
        };
        let tracker = Tracker::new(depth);
        drain_loop(
            rb,
            resolver,
            tracker,
            drain_controls,
            drain_slot,
            pid,
            note,
            leak_age_ticks,
            export,
        );
    });

    run_ui(&controls, &slot);

    controls.quit.store(true, Ordering::Release);
    let _ = drain.join();
}

#[allow(clippy::too_many_arguments)]
fn drain_loop(
    rb: ShmRingBuffer<AllocEvent, RING_CAPACITY>,
    resolver: Option<Resolver>,
    mut tracker: Tracker,
    controls: Arc<Controls>,
    slot: SnapshotSlot,
    pid: Option<u32>,
    note: Option<String>,
    leak_age_ticks: u64,
    export: Option<PathBuf>,
) {
    let publish_interval = Duration::from_millis(33);
    let mut last_publish = Instant::now() - publish_interval;
    let heat_interval = Duration::from_millis(state::HEAT_TICK_MS);
    let mut last_heat = Instant::now();

    loop {
        if controls.quit.load(Ordering::Acquire) {
            break;
        }

        if last_heat.elapsed() >= heat_interval {
            tracker.heat_tick();
            last_heat = Instant::now();
        }

        let paused = controls.paused.load(Ordering::Acquire);
        let mut drained = 0u32;
        if !paused {
            while let Some(ev) = rb.pop() {
                tracker.apply(ev, &resolver);
                drained += 1;
                if drained >= 50_000 {
                    break;
                }
            }
        }

        if last_publish.elapsed() >= publish_interval {
            let dropped = rb.dropped_count();
            let snap = build_snapshot(
                &mut tracker,
                &resolver,
                &controls,
                dropped,
                pid,
                paused,
                &note,
                leak_age_ticks,
            );
            *slot.lock().unwrap() = Arc::new(snap);
            last_publish = Instant::now();
        }

        if drained == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    if let Some(path) = &export {
        let folded = tracker.folded_stacks(&resolver);
        match std::fs::write(path, &folded) {
            Ok(()) => eprintln!(
                "analyzer: wrote {} live-allocation stacks to {} \
                 (open with flamegraph.pl or speedscope)",
                folded.lines().count(),
                path.display()
            ),
            Err(e) => eprintln!("analyzer: failed to write export {}: {e}", path.display()),
        }
    }
    eprintln!(
        "analyzer: peak live heap {} B · {} allocations · {} still live at exit",
        tracker.peak_bytes,
        tracker.alloc_count,
        tracker.active_count()
    );
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    tracker: &mut Tracker,
    resolver: &Option<Resolver>,
    controls: &Controls,
    dropped: u64,
    pid: Option<u32>,
    paused: bool,
    note: &Option<String>,
    leak_age_ticks: u64,
) -> Snapshot {
    let status = target_status(pid);
    let view = ViewMode::from_u8(controls.view.load(Ordering::Acquire));

    let recent: Vec<tracker::DisplayEvent> = tracker.recent.iter().cloned().collect();
    let offenders = tracker.top_offenders(20);
    let crash = tracker.crash.clone();
    let offender_depth = tracker.offender_depth();

    let sel_raw = controls.selected.load(Ordering::Acquire);
    let (selected, selected_view) = if view == ViewMode::Live && sel_raw >= 0 && !recent.is_empty() {
        let idx = (sel_raw as usize).min(recent.len() - 1);
        let ev = &recent[recent.len() - 1 - idx];
        let header = format!(
            "#{} {} ptr=0x{:x} size={}",
            ev.seq,
            kind_name(ev.kind),
            ev.ptr,
            ev.size
        );
        let frames = ev.frames.clone();
        let stack = tracker.resolve_stack(&frames, resolver);
        (Some(idx), Some(SelectedView { header, stack }))
    } else {
        (None, None)
    };

    let flame = (view == ViewMode::Flame).then(|| tracker.build_flame(resolver));
    let heatmap = (view == ViewMode::Heatmap).then(|| tracker.build_heat(24));
    let leaks = (view == ViewMode::Leaks).then(|| {
        let definite = status == "exited";
        tracker.build_leaks(leak_age_ticks, definite, 50)
    });

    Snapshot {
        status,
        active_count: tracker.active_count(),
        active_bytes: tracker.active_bytes,
        peak_bytes: tracker.peak_bytes,
        total_allocated: tracker.total_allocated,
        alloc_count: tracker.alloc_count,
        free_count: tracker.free_count,
        temporary_count: tracker.temporary_count,
        heap_series: tracker.heap_series(),
        total_events: tracker.total_events,
        dropped,
        paused,
        offender_depth,
        view,
        recent,
        offenders,
        crash,
        selected,
        selected_view,
        flame,
        heatmap,
        leaks,
        show_help: controls.show_help.load(Ordering::Acquire),
        note: note.clone(),
    }
}

fn run_ui(controls: &Controls, slot: &SnapshotSlot) {
    let mut terminal = ratatui::init();
    let frame_interval = Duration::from_millis(33);
    let mut last_draw = Instant::now() - frame_interval;

    loop {
        if let Ok(true) = crossterm::event::poll(Duration::from_millis(10))
            && let Ok(Event::Key(key)) = crossterm::event::read()
        {
            handle_key(key, controls);
        }

        if controls.quit.load(Ordering::Acquire) {
            break;
        }

        if last_draw.elapsed() >= frame_interval {
            let snap = slot.lock().unwrap().clone();
            let _ = terminal.draw(|frame| dashboard::draw(frame, &snap));
            last_draw = Instant::now();
        }
    }

    ratatui::restore();
}

fn handle_key(key: KeyEvent, controls: &Controls) {
    match key.code {
        KeyCode::Char('q') => controls.quit.store(true, Ordering::Release),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            controls.quit.store(true, Ordering::Release)
        }
        KeyCode::Char(' ') => {
            controls.paused.fetch_xor(true, Ordering::AcqRel);
        }
        KeyCode::Char('?') | KeyCode::Char('h') => {
            controls.show_help.fetch_xor(true, Ordering::AcqRel);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let v = controls.selected.load(Ordering::Acquire);
            controls.selected.store((v + 1).min(MAX_SELECT), Ordering::Release);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let v = controls.selected.load(Ordering::Acquire);
            controls.selected.store((v - 1).max(-1), Ordering::Release);
        }
        KeyCode::Esc => controls.selected.store(-1, Ordering::Release),
        KeyCode::Char('1') => controls.view.store(ViewMode::Live.as_u8(), Ordering::Release),
        KeyCode::Char('2') => controls.view.store(ViewMode::Flame.as_u8(), Ordering::Release),
        KeyCode::Char('3') => controls.view.store(ViewMode::Heatmap.as_u8(), Ordering::Release),
        KeyCode::Char('4') => controls.view.store(ViewMode::Leaks.as_u8(), Ordering::Release),
        KeyCode::Tab => {
            let next = ViewMode::from_u8(controls.view.load(Ordering::Acquire)).next();
            controls.view.store(next.as_u8(), Ordering::Release);
        }
        _ => {}
    }
}
