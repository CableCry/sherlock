//! Shared state between the threads: immutable [`Snapshot`]s published by the
//! drain thread, lightweight atomic [`Controls`] written by the UI thread.

use crate::tracker::{CrashInfo, DisplayEvent};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8};

pub const MAX_SELECT: isize = 199;

pub const HEAT_TICK_MS: u64 = 250;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Live,
    Flame,
    Heatmap,
    Leaks,
}

impl ViewMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ViewMode::Flame,
            2 => ViewMode::Heatmap,
            3 => ViewMode::Leaks,
            _ => ViewMode::Live,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            ViewMode::Live => 0,
            ViewMode::Flame => 1,
            ViewMode::Heatmap => 2,
            ViewMode::Leaks => 3,
        }
    }

    pub fn next(self) -> Self {
        Self::from_u8((self.as_u8() + 1) % 4)
    }
}

pub struct Controls {
    pub quit: AtomicBool,
    pub paused: AtomicBool,
    pub selected: AtomicIsize,
    pub show_help: AtomicBool,
    pub view: AtomicU8,
}

impl Controls {
    pub fn new() -> Self {
        Self {
            quit: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            selected: AtomicIsize::new(-1),
            show_help: AtomicBool::new(false),
            view: AtomicU8::new(0),
        }
    }
}

pub struct SelectedView {
    pub header: String,
    pub stack: Vec<String>,
}

pub struct FlameNode {
    pub label: String,
    pub bytes: u64,
    pub children: Vec<FlameNode>,
}

pub struct FlameView {
    pub total: u64,
    pub roots: Vec<FlameNode>,
}

pub struct HeatView {
    pub sites: Vec<String>,
    pub cells: Vec<Vec<u64>>,
    pub max: u64,
    pub buckets: usize,
}

pub struct LeakRow {
    pub site: String,
    pub bytes: u64,
    pub count: u64,
    pub oldest_age: u64,
}

pub struct LeakView {
    pub rows: Vec<LeakRow>,
    pub total_bytes: u64,
    pub total_count: u64,
    pub definite: bool,
    pub age_threshold: u64,
}

pub struct Snapshot {
    pub status: &'static str,
    pub active_count: usize,
    pub active_bytes: u64,
    pub peak_bytes: u64,
    pub total_allocated: u64,
    pub alloc_count: u64,
    pub free_count: u64,
    pub temporary_count: u64,
    pub heap_series: Vec<u64>,
    pub total_events: u64,
    pub dropped: u64,
    pub paused: bool,
    pub offender_depth: usize,
    pub view: ViewMode,
    pub recent: Vec<DisplayEvent>,
    pub offenders: Vec<(String, u64)>,
    pub crash: Option<CrashInfo>,
    pub selected: Option<usize>,
    pub selected_view: Option<SelectedView>,
    pub flame: Option<FlameView>,
    pub heatmap: Option<HeatView>,
    pub leaks: Option<LeakView>,
    pub show_help: bool,
    pub note: Option<String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: "connecting",
            active_count: 0,
            active_bytes: 0,
            peak_bytes: 0,
            total_allocated: 0,
            alloc_count: 0,
            free_count: 0,
            temporary_count: 0,
            heap_series: Vec::new(),
            total_events: 0,
            dropped: 0,
            paused: false,
            offender_depth: 0,
            view: ViewMode::Live,
            recent: Vec::new(),
            offenders: Vec::new(),
            crash: None,
            selected: None,
            selected_view: None,
            flame: None,
            heatmap: None,
            leaks: None,
            show_help: false,
            note: None,
        }
    }
}

pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        0 => "malloc",
        1 => "free",
        2 => "calloc",
        3 => "realloc",
        4 => "posix_memalign",
        5 => "aligned_alloc",
        6 => "CRASH",
        _ => "?",
    }
}

pub fn signal_name(sig: i32) -> &'static str {
    match sig {
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        11 => "SIGSEGV",
        _ => "signal",
    }
}
