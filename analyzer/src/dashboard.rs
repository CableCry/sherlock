//! Terminal dashboard rendering — pure presentation off an immutable
//! [`Snapshot`]. Header, optional crash panel, the active view, footer legend.

use crate::state::{
    FlameNode, FlameView, HEAT_TICK_MS, HeatView, LeakView, Snapshot, ViewMode, kind_name,
    signal_name,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Wrap};

fn kind_style(kind: u8) -> Style {
    match kind {
        6 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        1 => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Green),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn pad_to(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let pad = width.saturating_sub(t.chars().count());
    format!("{t}{}", " ".repeat(pad))
}

fn sparkline(series: &[u64], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if series.is_empty() || width == 0 {
        return String::new();
    }
    let slice = &series[series.len().saturating_sub(width)..];
    let max = slice.iter().copied().max().unwrap_or(0).max(1);
    slice
        .iter()
        .map(|&v| BARS[((v as u128 * 7) / max as u128).min(7) as usize])
        .collect()
}

fn heat_color(frac: f64) -> Color {
    if frac > 0.66 {
        Color::Red
    } else if frac > 0.33 {
        Color::Yellow
    } else if frac > 0.0 {
        Color::Green
    } else {
        Color::DarkGray
    }
}

fn draw_placeholder(frame: &mut Frame, area: Rect, block: Block, msg: &str, style: Style) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rect = centered_rect(inner, msg.chars().count() as u16, 1);
    frame.render_widget(Paragraph::new(msg).style(style).alignment(Alignment::Center), rect);
}

pub fn draw(frame: &mut Frame, snap: &Snapshot) {
    let area = frame.area();

    let crash_height: u16 = match &snap.crash {
        Some(c) => (c.frames.len() as u16 + 4).min(18),
        None => 0,
    };

    let mut constraints = vec![Constraint::Length(4)];
    if crash_height > 0 {
        constraints.push(Constraint::Length(crash_height));
    }
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(1));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut next = 0;
    let header_area = chunks[next];
    next += 1;
    let crash_area = if crash_height > 0 {
        let a = chunks[next];
        next += 1;
        Some(a)
    } else {
        None
    };
    let body_area = chunks[next];
    next += 1;
    let footer_area = chunks[next];

    draw_header(frame, snap, header_area);
    if let (Some(area), Some(crash)) = (crash_area, &snap.crash) {
        draw_crash(frame, crash, area);
    }

    match snap.view {
        ViewMode::Live => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                .split(body_area);
            draw_recent(frame, snap, body[0]);
            if let Some(view) = &snap.selected_view {
                draw_call_stack(frame, view, body[1]);
            } else {
                draw_offenders(frame, snap, body[1]);
            }
        }
        ViewMode::Flame => draw_flame(frame, snap.flame.as_ref(), body_area),
        ViewMode::Heatmap => draw_heat(frame, snap.heatmap.as_ref(), body_area),
        ViewMode::Leaks => draw_leaks(frame, snap.leaks.as_ref(), body_area),
    }
    draw_footer(frame, snap, footer_area);
    if snap.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame, snap: &Snapshot, area: Rect) {
    let mut spans = vec![
        Span::raw(format!("status: {}   ", snap.status)),
        Span::raw(format!(
            "active: {} ({} B)   total: {}   ",
            snap.active_count, snap.active_bytes, snap.total_events
        )),
    ];
    if snap.dropped > 0 {
        spans.push(Span::styled(
            format!("dropped: {}   ", snap.dropped),
            Style::default().fg(Color::Yellow),
        ));
    } else {
        spans.push(Span::raw("dropped: 0   "));
    }
    spans.push(Span::raw(format!("offender-depth: {}   ", snap.offender_depth)));
    if snap.paused {
        spans.push(Span::styled(
            " PAUSED ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(note) = &snap.note {
        spans.push(Span::styled(
            format!("   ({note})"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let dim = Style::default().fg(Color::Gray);
    let temp_pct = snap.temporary_count * 100 / snap.free_count.max(1);
    let stats = Line::from(vec![
        Span::styled("heap ", dim),
        Span::styled(sparkline(&snap.heap_series, 32), Style::default().fg(Color::Cyan)),
        Span::styled(
            format!(
                "  peak {} B   allocated {} B   short-lived {}%   allocs {}  frees {}",
                snap.peak_bytes,
                snap.total_allocated,
                temp_pct,
                snap.alloc_count,
                snap.free_count
            ),
            dim,
        ),
    ]);

    let header = Paragraph::new(vec![Line::from(spans), stats])
        .block(Block::default().borders(Borders::ALL).title("sherlock profiler"));
    frame.render_widget(header, area);
}

fn draw_recent(frame: &mut Frame, snap: &Snapshot, area: Rect) {
    let visible_rows = area.height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = snap
        .recent
        .iter()
        .rev()
        .take(visible_rows)
        .enumerate()
        .map(|(i, ev)| {
            let text = format!(
                "#{} {:<8} ptr=0x{:<12x} size={:<8} {}",
                ev.seq,
                kind_name(ev.kind),
                ev.ptr,
                ev.size,
                ev.offender
            );
            let mut style = kind_style(ev.kind);
            if snap.selected == Some(i) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            ListItem::new(text).style(style)
        })
        .collect();

    let title = if snap.paused {
        "recent events (paused)"
    } else {
        "recent events"
    };
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn draw_offenders(frame: &mut Frame, snap: &Snapshot, area: Rect) {
    const BAR: usize = 12;
    let max = snap.offenders.first().map(|(_, b)| *b).unwrap_or(1).max(1);
    let name_width = (area.width as usize).saturating_sub(BAR + 16);

    let items: Vec<ListItem> = snap
        .offenders
        .iter()
        .map(|(name, bytes)| {
            let frac = *bytes as f64 / max as f64;
            let filled = (frac * BAR as f64).round() as usize;
            let bar: String = "█".repeat(filled.min(BAR));
            let color = if frac > 0.5 {
                Color::Red
            } else if frac > 0.15 {
                Color::Yellow
            } else {
                Color::Green
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{bar:<BAR$} "), Style::default().fg(color)),
                Span::raw(format!("{bytes:>10}  ")),
                Span::raw(truncate(name, name_width.max(8))),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("top offenders (active bytes)"),
    );
    frame.render_widget(list, area);
}

fn draw_call_stack(frame: &mut Frame, view: &crate::state::SelectedView, area: Rect) {
    let mut lines = vec![Line::styled(
        view.header.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if view.stack.is_empty() {
        lines.push(Line::styled(
            "  (no frames captured)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        lines.extend(
            view.stack
                .iter()
                .enumerate()
                .map(|(i, f)| Line::raw(format!("  #{i} {f}"))),
        );
    }

    let panel = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("call stack (Esc to close)"),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
}

fn draw_crash(frame: &mut Frame, crash: &crate::tracker::CrashInfo, area: Rect) {
    let red = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);

    let mut items = vec![ListItem::new(format!(
        "{} ({})   fault addr=0x{:x}   pc=0x{:x}",
        signal_name(crash.signal),
        crash.signal,
        crash.fault_addr,
        crash.pc,
    ))
    .style(red)];

    items.extend(
        crash
            .frames
            .iter()
            .enumerate()
            .map(|(i, f)| ListItem::new(format!("  #{i} {f}"))),
    );

    let panel = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(red)
            .title(" CRASH — target terminated "),
    );
    frame.render_widget(panel, area);
}

struct FlameSeg {
    x0: u16,
    x1: u16,
    label: String,
    frac: f64,
}

#[allow(clippy::too_many_arguments)]
fn layout_flame(
    nodes: &[FlameNode],
    depth: usize,
    x0: u16,
    x1: u16,
    parent_bytes: u64,
    total: u64,
    rows: &mut Vec<Vec<FlameSeg>>,
    max_depth: usize,
) {
    if depth >= max_depth || x1 <= x0 || parent_bytes == 0 {
        return;
    }
    let span = (x1 - x0) as u128;
    let mut cursor = x0;
    for n in nodes {
        let w = (n.bytes as u128 * span / parent_bytes as u128) as u16;
        if w == 0 {
            continue;
        }
        let nx1 = (cursor + w).min(x1);
        if rows.len() <= depth {
            rows.resize_with(depth + 1, Vec::new);
        }
        rows[depth].push(FlameSeg {
            x0: cursor,
            x1: nx1,
            label: n.label.clone(),
            frac: n.bytes as f64 / total.max(1) as f64,
        });
        layout_flame(&n.children, depth + 1, cursor, nx1, n.bytes, total, rows, max_depth);
        cursor = nx1;
    }
}

fn flame_row_line(segs: &[FlameSeg]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut cursor = 0u16;
    for s in segs {
        if s.x0 > cursor {
            spans.push(Span::raw(" ".repeat((s.x0 - cursor) as usize)));
        }
        let w = (s.x1 - s.x0) as usize;
        let pct = (s.frac * 100.0).round() as u64;
        let suffix = format!(" {pct}%");
        let text = if w >= s.label.chars().count().min(6) + suffix.chars().count() {
            format!("{}{suffix}", s.label)
        } else {
            s.label.clone()
        };
        spans.push(Span::styled(
            pad_to(&text, w),
            Style::default().bg(heat_color(s.frac)).fg(Color::Black),
        ));
        cursor = s.x1;
    }
    Line::from(spans)
}

fn draw_flame(frame: &mut Frame, flame: Option<&FlameView>, area: Rect) {
    let title = "flame graph — width & color = share of live heap (outermost on top)";

    let Some(flame) = flame.filter(|f| f.total > 0 && !f.roots.is_empty()) else {
        draw_placeholder(
            frame,
            area,
            Block::default().borders(Borders::ALL).title(title),
            "no active allocations to chart",
            Style::default().fg(Color::DarkGray),
        );
        return;
    };

    let inner_w = area.width.saturating_sub(2);
    let max_depth = area.height.saturating_sub(2) as usize;
    let mut rows: Vec<Vec<FlameSeg>> = Vec::new();
    layout_flame(&flame.roots, 0, 0, inner_w, flame.total, flame.total, &mut rows, max_depth);

    let lines: Vec<Line> = rows.iter().map(|segs| flame_row_line(segs)).collect();
    let panel = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("flame graph — {} B live · width & color = share (outermost on top)", flame.total)),
    );
    frame.render_widget(panel, area);
}

fn draw_heat(frame: &mut Frame, heat: Option<&HeatView>, area: Rect) {
    let Some(heat) = heat.filter(|h| !h.sites.is_empty()) else {
        draw_placeholder(
            frame,
            area,
            Block::default()
                .borders(Borders::ALL)
                .title("heat map — allocation volume per call site over time"),
            "no allocation activity yet",
            Style::default().fg(Color::DarkGray),
        );
        return;
    };

    let window_secs = heat.buckets as u64 * HEAT_TICK_MS / 1000;
    let block = Block::default().borders(Borders::ALL).title(format!(
        "heat map — alloc volume per site, last ~{window_secs}s (left=older → right=now)"
    ));

    const LABEL_W: usize = 24;
    let avail = (area.width as usize).saturating_sub(2 + LABEL_W + 1);
    let cols = heat.buckets.min(avail.max(1));

    let mut lines = Vec::new();
    for (site, row) in heat.sites.iter().zip(&heat.cells) {
        let mut spans = vec![Span::raw(format!("{} ", pad_to(site, LABEL_W)))];
        let start = row.len().saturating_sub(cols);
        for &v in &row[start..] {
            let frac = v as f64 / heat.max as f64;
            let ch = if v == 0 { ' ' } else { '█' };
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(heat_color(frac)),
            ));
        }
        lines.push(Line::from(spans));
    }

    let panel = Paragraph::new(lines).block(block);
    frame.render_widget(panel, area);
}

fn draw_leaks(frame: &mut Frame, leaks: Option<&LeakView>, area: Rect) {
    let Some(leaks) = leaks else {
        return;
    };

    let title = if leaks.definite {
        format!(
            " LEAKS — definite (target exited): {} bytes in {} allocations ",
            leaks.total_bytes, leaks.total_count
        )
    } else {
        let secs = leaks.age_threshold * HEAT_TICK_MS / 1000;
        format!(
            " leaks — probable (alive ≥ {}s): {} bytes in {} allocations ",
            secs, leaks.total_bytes, leaks.total_count
        )
    };
    let border = if leaks.definite {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);

    if leaks.rows.is_empty() {
        draw_placeholder(
            frame,
            area,
            block,
            "no leaks detected",
            Style::default().fg(Color::Green),
        );
        return;
    }

    const BAR: usize = 10;
    let max = leaks.rows.first().map(|r| r.bytes).unwrap_or(1).max(1);
    let name_w = (area.width as usize).saturating_sub(2 + BAR + 1 + 14 + 8 + 8);
    let rows = leaks.rows.iter().map(|r| {
        let frac = r.bytes as f64 / max as f64;
        let bar: String = "█".repeat(((frac * BAR as f64).round() as usize).min(BAR));
        let age_secs = r.oldest_age * HEAT_TICK_MS / 1000;
        Row::new(vec![
            bar,
            r.bytes.to_string(),
            r.count.to_string(),
            format!("{age_secs}s"),
            truncate(&r.site, name_w.max(8)),
        ])
        .style(Style::default().fg(heat_color(frac)))
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(BAR as u16),
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec!["", "bytes", "count", "age", "site"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(block);
    frame.render_widget(table, area);
}

fn view_tab(label: &str, active: bool) -> Span<'static> {
    if active {
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray))
    }
}

fn swatch(color: Color, label: &str) -> [Span<'static>; 2] {
    [
        Span::styled("█", Style::default().fg(color)),
        Span::raw(format!(" {label}  ")),
    ]
}

fn legend_spans(view: ViewMode) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::Gray);
    let mut s = Vec::new();
    match view {
        ViewMode::Live => {
            s.push(Span::styled("events: ", dim));
            s.extend(swatch(Color::Green, "alloc"));
            s.extend(swatch(Color::DarkGray, "free"));
            s.extend(swatch(Color::Red, "crash"));
            s.push(Span::styled("· right = top offenders by live bytes  ", dim));
        }
        ViewMode::Flame => {
            s.push(Span::styled(
                "each bar = a call site · width & color = share of live heap · stacked outermost → innermost  ",
                dim,
            ));
        }
        ViewMode::Heatmap => {
            s.push(Span::styled("alloc volume: ", dim));
            s.extend(swatch(Color::Green, "low"));
            s.extend(swatch(Color::Yellow, "med"));
            s.extend(swatch(Color::Red, "high"));
            s.push(Span::styled("· each column ≈ 250ms  ", dim));
        }
        ViewMode::Leaks => {
            s.push(Span::styled("bar/color = share of leaked bytes · ", dim));
            s.extend(swatch(Color::Red, "definite (after exit)"));
        }
    }
    s
}

fn draw_footer(frame: &mut Frame, snap: &Snapshot, area: Rect) {
    let mut spans = vec![
        view_tab("1 live", snap.view == ViewMode::Live),
        view_tab("2 flame", snap.view == ViewMode::Flame),
        view_tab("3 heat", snap.view == ViewMode::Heatmap),
        view_tab("4 leaks", snap.view == ViewMode::Leaks),
        Span::raw("   "),
    ];
    spans.extend(legend_spans(snap.view));
    spans.push(Span::styled(
        " ?:help/legend",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::Gray);
    let head = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![
        Line::styled("Keys", head),
        Line::from(vec![Span::raw("  q / Ctrl-C   "), Span::styled("quit", dim)]),
        Line::from(vec![
            Span::raw("  1 / 2 / 3 / 4, Tab   "),
            Span::styled("switch view: live · flame · heat · leaks", dim),
        ]),
        Line::from(vec![
            Span::raw("  space   "),
            Span::styled("pause / resume draining", dim),
        ]),
        Line::from(vec![
            Span::raw("  ↑ / ↓ (k / j)   "),
            Span::styled("select an event and show its call stack (Live)", dim),
        ]),
        Line::from(vec![
            Span::raw("  Esc   "),
            Span::styled("clear selection", dim),
        ]),
        Line::from(vec![
            Span::raw("  ?   "),
            Span::styled("toggle this help", dim),
        ]),
        Line::raw(""),
        Line::styled("Views — what each one shows", head),
        Line::from(vec![
            Span::raw("  1 live    "),
            Span::styled("malloc/free firehose + top call sites by live bytes", dim),
        ]),
        Line::from(vec![
            Span::raw("  2 flame   "),
            Span::styled("live heap by call stack; bar width & color = share, top = outermost", dim),
        ]),
        Line::from(vec![
            Span::raw("  3 heat    "),
            Span::styled("alloc volume per call site over the last ~12s (left old → right now)", dim),
        ]),
        Line::from(vec![
            Span::raw("  4 leaks   "),
            Span::styled("still-live allocs by call site; red = definite once the target exits", dim),
        ]),
        Line::raw(""),
        Line::styled("Colors", head),
        Line::from(
            [
                vec![Span::raw("  intensity / size   ")],
                swatch(Color::Green, "low").to_vec(),
                swatch(Color::Yellow, "medium").to_vec(),
                swatch(Color::Red, "high").to_vec(),
            ]
            .concat(),
        ),
        Line::from(
            [
                vec![Span::raw("  events            ")],
                swatch(Color::Green, "alloc").to_vec(),
                swatch(Color::DarkGray, "free").to_vec(),
                swatch(Color::Red, "CRASH").to_vec(),
            ]
            .concat(),
        ),
        Line::from(vec![Span::raw(
            "  flame: bar width = live bytes; rows go outermost (top) → innermost",
        )]),
        Line::from(vec![Span::raw(
            "  leaks: probable while running (alive past --leak-age); definite (red) after exit",
        )]),
    ];
    lines.push(Line::raw(""));
    lines.push(Line::styled("  press ? to close", Style::default().fg(Color::DarkGray)));

    let height = lines.len() as u16 + 2;
    let popup = centered_rect(area, 78, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" help & legend ");
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{FlameNode, FlameView, HeatView, LeakRow, LeakView};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(snap: &Snapshot) -> String {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, snap)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn live_view_shows_feed_and_view_tabs() {
        let snap = Snapshot::default();
        let s = render(&snap);
        assert!(s.contains("recent events"), "{s}");
        assert!(s.contains("live"));
        assert!(s.contains("flame"));
    }

    #[test]
    fn flame_view_renders_graph_and_labels() {
        let snap = Snapshot {
            view: ViewMode::Flame,
            flame: Some(FlameView {
                total: 100,
                roots: vec![FlameNode {
                    label: "main".into(),
                    bytes: 100,
                    children: vec![FlameNode {
                        label: "worker".into(),
                        bytes: 60,
                        children: vec![],
                    }],
                }],
            }),
            ..Default::default()
        };
        let s = render(&snap);
        assert!(s.contains("flame graph"), "{s}");
        assert!(s.contains("main"));
        assert!(s.contains("100%"), "{s}");
    }

    #[test]
    fn heat_view_renders_sites() {
        let snap = Snapshot {
            view: ViewMode::Heatmap,
            heatmap: Some(HeatView {
                sites: vec!["alloc_site".into()],
                cells: vec![vec![0, 10, 50, 5]],
                max: 50,
                buckets: 4,
            }),
            ..Default::default()
        };
        let s = render(&snap);
        assert!(s.contains("heat map"), "{s}");
        assert!(s.contains("alloc_site"));
    }

    #[test]
    fn leaks_view_renders_definite_summary_and_rows() {
        let snap = Snapshot {
            view: ViewMode::Leaks,
            leaks: Some(LeakView {
                rows: vec![LeakRow {
                    site: "leaky_fn".into(),
                    bytes: 4096,
                    count: 8,
                    oldest_age: 40,
                }],
                total_bytes: 4096,
                total_count: 8,
                definite: true,
                age_threshold: 0,
            }),
            ..Default::default()
        };
        let s = render(&snap);
        assert!(s.contains("LEAKS"), "{s}");
        assert!(s.contains("definite"));
        assert!(s.contains("leaky_fn"));
        assert!(s.contains("10s"), "{s}");
    }

    #[test]
    fn help_overlay_shows_keys_and_color_legend() {
        let snap = Snapshot {
            show_help: true,
            ..Default::default()
        };
        let s = render(&snap);
        assert!(s.contains("help & legend"), "{s}");
        assert!(s.contains("Colors"));
        assert!(s.contains("alloc"));
    }

    #[test]
    fn crash_panel_renders_when_present() {
        let snap = Snapshot {
            crash: Some(crate::tracker::CrashInfo {
                signal: 11,
                fault_addr: 0,
                pc: 0xDEAD,
                frames: vec!["boom (crash.c:5)".into()],
            }),
            ..Default::default()
        };
        let s = render(&snap);
        assert!(s.contains("CRASH"), "{s}");
        assert!(s.contains("SIGSEGV"));
    }
}
