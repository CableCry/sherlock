//! Tracks live allocations, offenders, heat, leaks, stats, and crashes from the
//! drained event stream. See DESIGN.md for the non-obvious choices (memoized
//! resolution, stale-reuse reconciliation, realloc semantics, offender depth).

use crate::resolver::Resolver;
use crate::state::{FlameNode, FlameView, HeatView, LeakRow, LeakView};
use profiler::injection::AllocEvent;
use std::collections::{HashMap, VecDeque};

const RECENT_CAPACITY: usize = 200;
const HEAT_BUCKETS: usize = 48;
const HEAP_SERIES_CAP: usize = 240;

#[derive(Default)]
struct RawFlame {
    bytes: u64,
    children: HashMap<usize, RawFlame>,
}

fn collect_flame_addrs(node: &RawFlame, out: &mut Vec<usize>) {
    for (addr, child) in &node.children {
        out.push(*addr);
        collect_flame_addrs(child, out);
    }
}

fn convert_flame(
    node: &RawFlame,
    labels: &HashMap<usize, String>,
    min_bytes: u64,
) -> Vec<FlameNode> {
    let mut kids: Vec<FlameNode> = node
        .children
        .iter()
        .filter(|(_, c)| c.bytes >= min_bytes)
        .map(|(addr, c)| FlameNode {
            label: labels
                .get(addr)
                .cloned()
                .unwrap_or_else(|| format!("0x{addr:x}")),
            bytes: c.bytes,
            children: convert_flame(c, labels, min_bytes),
        })
        .collect();
    kids.sort_by_key(|n| std::cmp::Reverse(n.bytes));
    kids
}

#[derive(Default)]
struct Heat {
    sites: HashMap<String, [u64; HEAT_BUCKETS]>,
    cursor: usize,
}

enum EventKindClass {
    Alloc,
    Free,
    Realloc,
}

impl From<u8> for EventKindClass {
    fn from(kind: u8) -> Self {
        match kind {
            1 => EventKindClass::Free,
            3 => EventKindClass::Realloc,
            _ => EventKindClass::Alloc,
        }
    }
}

struct ActiveAlloc {
    size: u64,
    offender: String,
    alloc_tick: u64,
    frames: Vec<usize>,
}

#[derive(Clone)]
pub struct DisplayEvent {
    pub seq: u64,
    pub kind: u8,
    pub ptr: usize,
    pub size: usize,
    pub offender: String,
    pub frames: Vec<usize>,
}

#[derive(Clone)]
pub struct CrashInfo {
    pub signal: i32,
    pub fault_addr: usize,
    pub pc: usize,
    pub frames: Vec<String>,
}

pub struct Tracker {
    active: HashMap<usize, ActiveAlloc>,
    offenders: HashMap<String, u64>,
    resolved_cache: HashMap<usize, Vec<String>>,
    heat: Heat,
    tick: u64,
    pub recent: VecDeque<DisplayEvent>,
    pub total_events: u64,
    pub active_bytes: u64,
    pub peak_bytes: u64,
    pub total_allocated: u64,
    pub alloc_count: u64,
    pub free_count: u64,
    pub temporary_count: u64,
    heap_series: VecDeque<u64>,
    pub crash: Option<CrashInfo>,
    offender_depth: usize,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Tracker {
    pub fn new(offender_depth: usize) -> Self {
        Self {
            active: HashMap::new(),
            offenders: HashMap::new(),
            resolved_cache: HashMap::new(),
            heat: Heat::default(),
            tick: 0,
            recent: VecDeque::new(),
            total_events: 0,
            active_bytes: 0,
            peak_bytes: 0,
            total_allocated: 0,
            alloc_count: 0,
            free_count: 0,
            temporary_count: 0,
            heap_series: VecDeque::new(),
            crash: None,
            offender_depth,
        }
    }

    pub fn heap_series(&self) -> Vec<u64> {
        self.heap_series.iter().copied().collect()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn offender_depth(&self) -> usize {
        self.offender_depth
    }

    fn resolve_cached(&mut self, addr: usize, resolver: &Option<Resolver>) -> &[String] {
        self.resolved_cache
            .entry(addr)
            .or_insert_with(|| resolver.as_ref().map(|r| r.resolve(addr)).unwrap_or_default())
    }

    fn label_for(&mut self, frames: &[usize], resolver: &Option<Resolver>) -> String {
        let Some(&addr) = frames.get(self.offender_depth).or_else(|| frames.last()) else {
            return "?".to_string();
        };
        match self.resolve_cached(addr, resolver).first() {
            Some(label) => label.clone(),
            None => format!("0x{addr:x}"),
        }
    }

    pub fn resolve_stack(&mut self, frames: &[usize], resolver: &Option<Resolver>) -> Vec<String> {
        let mut out = Vec::new();
        for &addr in frames {
            let chain = self.resolve_cached(addr, resolver);
            if chain.is_empty() {
                out.push(format!("0x{addr:x}"));
            } else {
                out.extend(chain.iter().cloned());
            }
        }
        out
    }

    pub fn apply(&mut self, ev: AllocEvent, resolver: &Option<Resolver>) {
        self.total_events += 1;

        if ev.kind == 6 {
            self.record_crash(ev, resolver);
            return;
        }

        let frames: Vec<usize> = ev.frames[..ev.frame_count as usize].to_vec();
        let offender = self.label_for(&frames, resolver);
        let now = self.tick;

        match EventKindClass::from(ev.kind) {
            EventKindClass::Alloc => {
                self.insert_active(ev.ptr, ev.size, offender.clone(), now, frames.clone());
                self.record_heat(&offender, ev.size as u64);
            }
            EventKindClass::Free => {
                self.free_count += 1;
                if self.active.get(&ev.ptr).is_some_and(|a| a.alloc_tick == now) {
                    self.temporary_count += 1;
                }
                self.remove_active(ev.ptr);
            }
            EventKindClass::Realloc => {
                if ev.ptr != 0 {
                    self.remove_active(ev.old_ptr);
                    self.insert_active(ev.ptr, ev.size, offender.clone(), now, frames.clone());
                    self.record_heat(&offender, ev.size as u64);
                }
            }
        }

        self.peak_bytes = self.peak_bytes.max(self.active_bytes);

        self.recent.push_back(DisplayEvent {
            seq: self.total_events,
            kind: ev.kind,
            ptr: ev.ptr,
            size: ev.size,
            offender,
            frames,
        });
        if self.recent.len() > RECENT_CAPACITY {
            self.recent.pop_front();
        }
    }

    fn record_crash(&mut self, ev: AllocEvent, resolver: &Option<Resolver>) {
        let raw: Vec<usize> = ev.frames[..ev.frame_count as usize].to_vec();
        let frames = self.resolve_stack(&raw, resolver);
        let top = frames.first().cloned().unwrap_or_else(|| "?".to_string());

        self.crash = Some(CrashInfo {
            signal: ev.size as i32,
            fault_addr: ev.old_ptr,
            pc: ev.ptr,
            frames,
        });

        self.recent.push_back(DisplayEvent {
            seq: self.total_events,
            kind: ev.kind,
            ptr: ev.ptr,
            size: 0,
            offender: top,
            frames: raw,
        });
        if self.recent.len() > RECENT_CAPACITY {
            self.recent.pop_front();
        }
    }

    fn insert_active(
        &mut self,
        ptr: usize,
        size: usize,
        offender: String,
        alloc_tick: u64,
        frames: Vec<usize>,
    ) {
        if ptr == 0 {
            return;
        }
        self.remove_active(ptr);
        self.total_allocated += size as u64;
        self.alloc_count += 1;
        *self.offenders.entry(offender.clone()).or_insert(0) += size as u64;
        self.active_bytes += size as u64;
        self.active.insert(
            ptr,
            ActiveAlloc {
                size: size as u64,
                offender,
                alloc_tick,
                frames,
            },
        );
    }

    fn record_heat(&mut self, site: &str, bytes: u64) {
        let cur = self.heat.cursor;
        self.heat
            .sites
            .entry(site.to_string())
            .or_insert([0; HEAT_BUCKETS])[cur] += bytes;
    }

    pub fn heat_tick(&mut self) {
        self.tick += 1;
        self.heap_series.push_back(self.active_bytes);
        if self.heap_series.len() > HEAP_SERIES_CAP {
            self.heap_series.pop_front();
        }
        self.heat.cursor = (self.heat.cursor + 1) % HEAT_BUCKETS;
        let cur = self.heat.cursor;
        for buckets in self.heat.sites.values_mut() {
            buckets[cur] = 0;
        }
    }

    pub fn build_heat(&self, max_sites: usize) -> HeatView {
        let mut totals: Vec<(&String, u64)> = self
            .heat
            .sites
            .iter()
            .map(|(k, b)| (k, b.iter().sum::<u64>()))
            .filter(|&(_, t)| t > 0)
            .collect();
        totals.sort_by_key(|&(_, t)| std::cmp::Reverse(t));
        totals.truncate(max_sites);

        let cursor = self.heat.cursor;
        let mut sites = Vec::new();
        let mut cells = Vec::new();
        let mut max = 1u64;
        for (name, _) in &totals {
            let buckets = &self.heat.sites[*name];
            let mut row = Vec::with_capacity(HEAT_BUCKETS);
            for i in 0..HEAT_BUCKETS {
                let v = buckets[(cursor + 1 + i) % HEAT_BUCKETS];
                max = max.max(v);
                row.push(v);
            }
            sites.push((*name).clone());
            cells.push(row);
        }

        HeatView {
            sites,
            cells,
            max,
            buckets: HEAT_BUCKETS,
        }
    }

    pub fn build_flame(&mut self, resolver: &Option<Resolver>) -> FlameView {
        let mut root = RawFlame::default();
        for a in self.active.values() {
            root.bytes += a.size;
            let mut node = &mut root;
            for &addr in a.frames.iter().rev() {
                node = node.children.entry(addr).or_default();
                node.bytes += a.size;
            }
        }

        let mut addrs = Vec::new();
        collect_flame_addrs(&root, &mut addrs);
        let mut labels: HashMap<usize, String> = HashMap::new();
        for addr in addrs {
            labels.entry(addr).or_insert_with(|| {
                self.resolve_cached(addr, resolver)
                    .first()
                    .cloned()
                    .unwrap_or_else(|| format!("0x{addr:x}"))
            });
        }

        let total = root.bytes;
        let min_bytes = (total / 200).max(1);
        let roots = convert_flame(&root, &labels, min_bytes);
        FlameView { total, roots }
    }

    pub fn build_leaks(&self, age_threshold: u64, definite: bool, n: usize) -> LeakView {
        let mut agg: HashMap<&str, (u64, u64, u64)> = HashMap::new();
        let mut total_bytes = 0u64;
        let mut total_count = 0u64;
        for a in self.active.values() {
            let age = self.tick.saturating_sub(a.alloc_tick);
            if definite || age >= age_threshold {
                let e = agg.entry(a.offender.as_str()).or_insert((0, 0, 0));
                e.0 += a.size;
                e.1 += 1;
                e.2 = e.2.max(age);
                total_bytes += a.size;
                total_count += 1;
            }
        }

        let mut rows: Vec<LeakRow> = agg
            .into_iter()
            .map(|(site, (bytes, count, oldest_age))| LeakRow {
                site: site.to_string(),
                bytes,
                count,
                oldest_age,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));
        rows.truncate(n);

        LeakView {
            rows,
            total_bytes,
            total_count,
            definite,
            age_threshold,
        }
    }

    fn remove_active(&mut self, ptr: usize) {
        if let Some(alloc) = self.active.remove(&ptr) {
            self.active_bytes = self.active_bytes.saturating_sub(alloc.size);
            if let Some(bytes) = self.offenders.get_mut(&alloc.offender) {
                *bytes = bytes.saturating_sub(alloc.size);
            }
        }
    }

    pub fn folded_stacks(&mut self, resolver: &Option<Resolver>) -> String {
        let entries: Vec<(Vec<usize>, u64)> = self
            .active
            .values()
            .map(|a| (a.frames.clone(), a.size))
            .collect();

        let mut agg: HashMap<String, u64> = HashMap::new();
        for (frames, size) in entries {
            let resolved = self.resolve_stack(&frames, resolver);
            let key = if resolved.is_empty() {
                "[unknown]".to_string()
            } else {
                resolved
                    .into_iter()
                    .rev()
                    .map(|f| f.replace(';', ":"))
                    .collect::<Vec<_>>()
                    .join(";")
            };
            *agg.entry(key).or_insert(0) += size;
        }

        let mut out = String::new();
        for (stack, bytes) in agg {
            out.push_str(&format!("{stack} {bytes}\n"));
        }
        out
    }

    pub fn top_offenders(&self, n: usize) -> Vec<(String, u64)> {
        let mut entries: Vec<(String, u64)> = self
            .offenders
            .iter()
            .filter(|&(_, &bytes)| bytes > 0)
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        entries.sort_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
        entries.truncate(n);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use profiler::injection::MAX_STACK_DEPTH;

    fn ev(kind: u8, ptr: usize, old_ptr: usize, size: usize, frames: &[usize]) -> AllocEvent {
        let mut f = [0usize; MAX_STACK_DEPTH];
        f[..frames.len()].copy_from_slice(frames);
        AllocEvent {
            kind,
            ptr,
            old_ptr,
            size,
            align: 0,
            frames: f,
            frame_count: frames.len() as u8,
        }
    }

    #[test]
    fn malloc_then_free_balances_active_state() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        assert_eq!(t.active_count(), 1);
        assert_eq!(t.active_bytes, 100);
        t.apply(ev(1, 0x10, 0, 0, &[0xAAAA]), &none);
        assert_eq!(t.active_count(), 0);
        assert_eq!(t.active_bytes, 0);
    }

    #[test]
    fn offenders_aggregate_by_top_frame() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(0, 0x20, 0, 50, &[0xAAAA]), &none);
        let top = t.top_offenders(10);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].1, 150);
    }

    #[test]
    fn realloc_moves_bookkeeping_to_the_new_pointer() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(3, 0x30, 0x10, 200, &[0xAAAA]), &none);
        assert_eq!(t.active_count(), 1);
        assert_eq!(t.active_bytes, 200);
    }

    #[test]
    fn stale_free_reconciles_a_reused_pointer() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(0, 0x10, 0, 70, &[0xAAAA]), &none);
        assert_eq!(t.active_count(), 1);
        assert_eq!(t.active_bytes, 70);
    }

    #[test]
    fn crash_event_is_recorded_and_fully_decoded() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(6, 0xDEAD, 0x0, 11, &[0xBBBB, 0xCCCC]), &none);
        assert_eq!(t.active_count(), 0);
        let c = t.crash.expect("crash recorded");
        assert_eq!(c.signal, 11);
        assert_eq!(c.pc, 0xDEAD);
        assert_eq!(c.frames.len(), 2);
    }

    #[test]
    fn offender_depth_aggregates_on_a_deeper_frame() {
        let mut t = Tracker::new(1);
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA, 0xBBBB]), &none);
        t.apply(ev(0, 0x20, 0, 100, &[0xCCCC, 0xBBBB]), &none);
        let top = t.top_offenders(10);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].1, 200);
    }

    #[test]
    fn flame_aggregates_active_bytes_by_call_path() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA, 0xBBBB]), &none);
        t.apply(ev(0, 0x20, 0, 50, &[0xCCCC, 0xBBBB]), &none);

        let flame = t.build_flame(&none);
        assert_eq!(flame.total, 150);
        assert_eq!(flame.roots.len(), 1);
        assert_eq!(flame.roots[0].bytes, 150);
        assert_eq!(flame.roots[0].children.len(), 2);
    }

    #[test]
    fn leaks_are_definite_after_exit_and_age_gated_while_running() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(0, 0x20, 0, 200, &[0xAAAA]), &none);

        let probable = t.build_leaks(1_000_000, false, 10);
        assert_eq!(probable.total_count, 0);
        assert!(probable.rows.is_empty());

        let definite = t.build_leaks(1_000_000, true, 10);
        assert!(definite.definite);
        assert_eq!(definite.total_bytes, 300);
        assert_eq!(definite.total_count, 2);
        assert_eq!(definite.rows.len(), 1);
        assert_eq!(definite.rows[0].bytes, 300);
    }

    #[test]
    fn freeing_an_allocation_clears_it_from_leaks_and_flame() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(1, 0x10, 0, 0, &[0xAAAA]), &none);
        assert_eq!(t.build_leaks(0, true, 10).total_bytes, 0);
        assert_eq!(t.build_flame(&none).total, 0);
    }

    #[test]
    fn tracks_peak_cumulative_and_churn() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(0, 0x20, 0, 50, &[0xAAAA]), &none);
        t.apply(ev(1, 0x10, 0, 0, &[0xAAAA]), &none);

        assert_eq!(t.active_bytes, 50);
        assert_eq!(t.peak_bytes, 150);
        assert_eq!(t.total_allocated, 150);
        assert_eq!(t.alloc_count, 2);
        assert_eq!(t.free_count, 1);
        assert_eq!(t.temporary_count, 1);

        t.heat_tick();
        assert_eq!(t.heap_series(), vec![50]);
    }

    #[test]
    fn folded_stacks_are_outermost_first_and_aggregated() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA, 0xBBBB]), &none);
        t.apply(ev(0, 0x20, 0, 40, &[0xAAAA, 0xBBBB]), &none);

        let folded = t.folded_stacks(&none);
        assert_eq!(folded.trim(), "0xbbbb;0xaaaa 140");
    }

    #[test]
    fn heat_records_activity_per_site() {
        let mut t = Tracker::default();
        let none = None;
        t.apply(ev(0, 0x10, 0, 100, &[0xAAAA]), &none);
        t.apply(ev(0, 0x20, 0, 25, &[0xAAAA]), &none);

        let heat = t.build_heat(10);
        assert_eq!(heat.sites.len(), 1);
        let row_total: u64 = heat.cells[0].iter().sum();
        assert_eq!(row_total, 125);
        assert_eq!(heat.max, 125);
    }
}
