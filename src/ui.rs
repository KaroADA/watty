use std::time::Instant;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Widget};

use crate::ble::Discovered;
use crate::devices::{LinkStatus, TrainerTarget};
use crate::session::{RunState, Sensors, Session};
use crate::workout::fmt_duration;

// ---------------------------------------------------------------- big digits

const GLYPH_ROWS: usize = 5;

fn glyph(c: char) -> [&'static str; GLYPH_ROWS] {
    match c {
        '0' => ["███", "█ █", "█ █", "█ █", "███"],
        '1' => ["  █", "  █", "  █", "  █", "  █"],
        '2' => ["███", "  █", "███", "█  ", "███"],
        '3' => ["███", "  █", "███", "  █", "███"],
        '4' => ["█ █", "█ █", "███", "  █", "  █"],
        '5' => ["███", "█  ", "███", "  █", "███"],
        '6' => ["███", "█  ", "███", "█ █", "███"],
        '7' => ["███", "  █", "  █", "  █", "  █"],
        '8' => ["███", "█ █", "███", "█ █", "███"],
        '9' => ["███", "█ █", "███", "  █", "███"],
        ':' => [" ", "█", " ", "█", " "],
        _ => ["   ", "   ", "███", "   ", "   "],
    }
}

fn big_width(text: &str, scale: u16) -> u16 {
    let w: u16 = text.chars().map(|c| glyph(c)[0].chars().count() as u16 * scale).sum();
    w + text.chars().count().saturating_sub(1) as u16
}

struct BigValue<'a> {
    title: &'a str,
    value: String,
    unit: &'a str,
    color: Color,
    /// Bottom-left badge.
    note: Option<Span<'a>>,
}

impl Widget for BigValue<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(format!(" {} ", self.title), Style::new().fg(Color::Gray)));
        let block = if self.unit.is_empty() {
            block
        } else {
            block.title_bottom(Line::from(Span::styled(format!(" {} ", self.unit), Style::new().fg(Color::DarkGray))).right_aligned())
        };
        let block = match self.note {
            Some(note) => block.title_bottom(Line::from(note).left_aligned()),
            None => block,
        };
        let inner = block.inner(area);
        block.render(area, buf);
        let style = Style::new().fg(self.color).add_modifier(Modifier::BOLD);

        let scale = if big_width(&self.value, 2) <= inner.width { 2 } else { 1 };
        if inner.height < GLYPH_ROWS as u16 || big_width(&self.value, scale) > inner.width {
            let y = inner.y + inner.height / 2;
            Paragraph::new(self.value).style(style).alignment(Alignment::Center).render(Rect { y, height: 1, ..inner }, buf);
            return;
        }
        let w = big_width(&self.value, scale);
        let x0 = inner.x + (inner.width - w) / 2;
        let y0 = inner.y + (inner.height - GLYPH_ROWS as u16) / 2;
        let mut x = x0;
        for c in self.value.chars() {
            let g = glyph(c);
            let gw = g[0].chars().count() as u16;
            for (row, line) in g.iter().enumerate() {
                for (col, ch) in line.chars().enumerate() {
                    if ch == ' ' {
                        continue;
                    }
                    for s in 0..scale {
                        let cx = x + col as u16 * scale + s;
                        buf[(cx, y0 + row as u16)].set_char('█').set_style(style);
                    }
                }
            }
            x += gw * scale + 1;
        }
    }
}

// ---------------------------------------------------------------- colors

pub fn zone_color(frac: f64) -> Color {
    match frac {
        f if f < 0.56 => Color::Gray,
        f if f < 0.76 => Color::Blue,
        f if f < 0.91 => Color::Green,
        f if f < 1.06 => Color::Yellow,
        f if f < 1.21 => Color::Rgb(255, 140, 0),
        _ => Color::Red,
    }
}

fn link_span(label: &str, link: &Option<(LinkStatus, Option<String>)>) -> Vec<Span<'static>> {
    let (dot, color, text) = match link {
        None => ("○", Color::DarkGray, "none".to_string()),
        Some((LinkStatus::Connected, name)) => ("●", Color::Green, name.clone().unwrap_or_default()),
        Some((LinkStatus::Searching, _)) => ("●", Color::Yellow, "searching…".into()),
        Some((LinkStatus::Connecting, _)) => ("●", Color::Yellow, "connecting…".into()),
        Some((LinkStatus::Disconnected(_), _)) => ("●", Color::Red, "reconnecting…".into()),
    };
    vec![
        Span::raw(format!("{label} ")),
        Span::styled(dot, Style::new().fg(color)),
        Span::styled(format!(" {text}  "), Style::new().fg(color)),
    ]
}

// ---------------------------------------------------------------- ride screen

pub struct View<'a> {
    pub session: &'a Session,
    pub sensors: &'a Sensors,
    pub message: Option<&'a (Instant, String)>,
    pub confirm_quit: bool,
    pub sim: bool,
    pub recording: Option<&'a str>,
    pub cadence_tolerance: u16,
}

pub fn draw_ride(f: &mut Frame, v: &View) {
    let s = v.session;
    let [header, row1, row2, chart, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // header
    let mut spans = vec![Span::styled(" watty ", Style::new().black().on_cyan().bold()), Span::raw(format!(" {}  ", s.workout.name))];
    spans.extend(link_span("Trainer", &v.sensors.trainer_link));
    spans.extend(link_span("HR", &v.sensors.hr_link));
    let erg = match v.sensors.applied {
        Some(TrainerTarget::Erg(w)) => Span::styled(format!("ERG {w} W ✓  "), Style::new().fg(Color::Green)),
        Some(TrainerTarget::FreeRide) => Span::styled("FREE RIDE  ", Style::new().fg(Color::Cyan)),
        _ => Span::styled("ERG –  ", Style::new().fg(Color::DarkGray)),
    };
    spans.push(erg);
    spans.push(Span::raw(format!("Intensity {:.0}%  ", s.intensity * 100.0)));
    if v.sim {
        spans.push(Span::styled("SIM ", Style::new().magenta().bold()));
    }
    f.render_widget(Line::from(spans), header);

    // big numbers
    let target = s.target_watts();
    let power = v.sensors.power();
    let power_color = match (power, target, s.state) {
        (Some(p), Some(t), RunState::Running) if t > 0 => {
            let dev = (p as f64 - t as f64).abs() / t as f64;
            if dev <= 0.05 { Color::Green } else if dev <= 0.10 { Color::Yellow } else { Color::Red }
        }
        _ => Color::White,
    };
    let num = |v: Option<u16>| v.map_or("-".to_string(), |v| v.to_string());
    let cols = Layout::horizontal([Constraint::Fill(1); 4]).split(row1);
    f.render_widget(BigValue { title: "POWER 3s", value: num(power), unit: "W", color: power_color, note: None }, cols[0]);
    let target_color = target.map_or(Color::DarkGray, |t| zone_color(t as f64 / s.ftp as f64));
    f.render_widget(BigValue { title: "TARGET", value: num(target), unit: "W", color: target_color, note: None }, cols[1]);
    f.render_widget(BigValue { title: "HEART RATE", value: num(v.sensors.hr()), unit: "bpm", color: Color::LightRed, note: None }, cols[2]);
    draw_cadence(f, cols[3], v);

    let seg_idx = s.workout.segment_index_at(s.pos as f64);
    let seg_left = seg_idx.map_or(0, |i| s.workout.segments[i].end() - s.pos);
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)]).split(row2);
    f.render_widget(BigValue { title: "SEGMENT LEFT", value: fmt_duration(seg_left), unit: "", color: Color::White, note: None }, cols[0]);
    f.render_widget(BigValue { title: "ELAPSED", value: fmt_duration(s.elapsed), unit: "", color: Color::White, note: None }, cols[1]);
    draw_info(f, cols[2], v, seg_idx);

    f.render_widget(Chart { session: s }, chart);

    // status line
    let status_line = match s.state {
        RunState::Ready => Line::styled(" Press SPACE to start", Style::new().yellow().bold()),
        RunState::Paused { auto: false } => Line::styled(" ⏸ PAUSED — SPACE to resume", Style::new().yellow().bold()),
        RunState::Paused { auto: true } => Line::styled(" ⏸ AUTO-PAUSED — start pedaling to resume", Style::new().yellow().bold()),
        RunState::Finished => Line::styled(" ✓ Workout complete — keep riding or press q", Style::new().green().bold()),
        RunState::Running => match v.message {
            Some((t, m)) if t.elapsed().as_secs() < 15 => Line::styled(format!(" {m}"), Style::new().fg(Color::Yellow)),
            _ => Line::styled(v.recording.map_or(String::new(), |p| format!(" ● REC {p}")), Style::new().fg(Color::DarkGray)),
        },
    };
    f.render_widget(status_line, status);
    let keys = if v.sim { "   s sim stop/start pedaling" } else { "" };
    f.render_widget(
        Line::styled(format!(" SPACE pause/resume   + / - intensity ±5%   n next segment   q quit{keys}"), Style::new().fg(Color::DarkGray)),
        footer,
    );

    if v.confirm_quit {
        let area = centered(f.area(), 44, 5);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(vec![Line::raw(""), Line::from("Quit and save the ride?  [y] yes   [n] no").bold()])
                .alignment(Alignment::Center)
                .block(Block::bordered().border_type(BorderType::Double).border_style(Style::new().yellow())),
            area,
        );
    }
}

/// Cadence with the workout's target range and a faster/slower hint.
fn draw_cadence(f: &mut Frame, area: Rect, v: &View) {
    let s = v.session;
    let cadence = v.sensors.cadence();
    let value = cadence.map_or("-".to_string(), |c| c.to_string());
    let Some(target) = s.workout.cadence_at(s.pos as f64).filter(|_| s.state != RunState::Finished) else {
        let w = BigValue { title: "CADENCE", value, unit: "rpm", color: Color::LightCyan, note: None };
        f.render_widget(w, area);
        return;
    };
    let tol = v.cadence_tolerance;
    let (lo, hi) = (target.saturating_sub(tol), target + tol);
    let title = format!("CADENCE {lo}–{hi}");
    let (color, note) = match cadence {
        Some(c) if c < lo || c > hi => {
            let off = if c < lo { lo - c } else { c - hi };
            let color = if off > tol { Color::Red } else { Color::Yellow };
            let text = if c < lo { " ▲ FASTER " } else { " ▼ SLOWER " };
            let note = (s.state == RunState::Running).then(|| Span::styled(text, Style::new().fg(Color::Black).bg(color).bold()));
            (color, note)
        }
        Some(_) => (Color::Green, Some(Span::styled(" ✓ ", Style::new().fg(Color::Green).bold()))),
        None => (Color::LightCyan, None),
    };
    f.render_widget(BigValue { title: &title, value, unit: "rpm", color, note }, area);
}

fn draw_info(f: &mut Frame, area: Rect, v: &View, seg_idx: Option<usize>) {
    let s = v.session;
    let ws = &s.workout;
    let seg_line = |i: usize| {
        let seg = &ws.segments[i];
        let p = |x: Option<f64>| x.map(|x| (x * s.ftp as f64 * s.intensity).round() as u32);
        let power = match (p(seg.power_start), p(seg.power_end)) {
            (Some(a), Some(b)) if a != b => format!("{a}→{b} W"),
            (Some(a), _) => format!("{a} W"),
            _ => "free".into(),
        };
        let color = seg.power_start.map_or(Color::DarkGray, |p| zone_color(p * s.intensity));
        let cadence = seg.cadence.map_or(String::new(), |c| format!(" · {c} rpm"));
        Line::from(vec![
            Span::styled(format!("{} ", seg.kind.label()), Style::new().fg(color).bold()),
            Span::raw(format!("{} · {power}{cadence}", fmt_duration(seg.duration))),
        ])
    };
    let mut lines = vec![];
    match seg_idx {
        Some(i) => {
            lines.push(Line::styled("Now", Style::new().fg(Color::DarkGray)));
            lines.push(seg_line(i));
            lines.push(Line::styled("Next", Style::new().fg(Color::DarkGray)));
            lines.push(if i + 1 < ws.segments.len() { seg_line(i + 1) } else { Line::raw("— end —") });
        }
        None => lines.push(Line::styled("Workout complete", Style::new().green())),
    }
    lines.push(Line::raw(""));
    lines.push(Line::raw(format!(
        "{} / {}   {:.2} km   {:.1} km/h",
        fmt_duration(s.pos.min(ws.total_duration())),
        fmt_duration(ws.total_duration()),
        s.distance_m / 1000.0,
        s.speed_kmh(),
    )));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(format!(" SEGMENT {}/{} ", seg_idx.map_or(ws.segments.len(), |i| i + 1), ws.segments.len()), Style::new().fg(Color::Gray)));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// Zone-colored target profile; the ridden part shows actual power instead.
struct Chart<'a> {
    session: &'a Session,
}

impl Widget for Chart<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let s = self.session;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(" PROFILE ", Style::new().fg(Color::Gray)));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let total = s.workout.total_duration().max(1) as f64;
        let top = (s.workout.max_power() * s.intensity).max(1.0) * 1.1;
        let h = inner.height as f64;
        let eighths = ["", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        let row_of = |frac: f64| -> u16 { ((frac / top) * h).ceil().clamp(1.0, h) as u16 };

        let ftp_row = inner.bottom() - row_of(1.0);
        for x in 0..inner.width {
            let t0 = x as f64 * total / inner.width as f64;
            let t1 = (x + 1) as f64 * total / inner.width as f64;
            let mid = (t0 + t1) / 2.0;
            let cx = inner.x + x;
            let current = (s.pos as f64) >= t0 && (s.pos as f64) < t1;
            let past = (s.pos as f64) >= t1;

            let actual = past.then(|| {
                let (a, b) = (t0 as usize, (t1.ceil() as usize).min(s.trace.len()));
                let vals: Vec<u16> = s.trace.get(a..b).unwrap_or(&[]).iter().flatten().copied().collect();
                (!vals.is_empty()).then(|| vals.iter().map(|&v| v as f64).sum::<f64>() / vals.len() as f64 / s.ftp as f64)
            });
            let frac = match actual {
                Some(Some(a)) => Some(a),
                _ => s.workout.power_at(mid).map(|p| p * s.intensity),
            };
            let free = actual.flatten().is_none() && frac.is_none() && s.workout.segment_at(mid).is_some();
            let color = if current { Color::White } else { frac.map_or(Color::DarkGray, zone_color) };
            let mut style = Style::new().fg(color);
            if past && actual.flatten().is_none() {
                style = style.add_modifier(Modifier::DIM);
            }

            if inner.height > 1 {
                buf[(cx, ftp_row)].set_char('┄').set_style(Style::new().fg(Color::DarkGray));
            }
            let level = match frac {
                Some(f) => ((f / top) * h * 8.0).round().clamp(1.0, h * 8.0) as usize,
                None if free => 4,
                None => 0,
            };
            for row in 0..inner.height {
                let filled = level.saturating_sub(row as usize * 8).min(8);
                if filled == 0 {
                    break;
                }
                let y = inner.bottom() - 1 - row;
                let ch = if free { "░" } else { eighths[filled] };
                buf[(cx, y)].set_symbol(ch).set_style(style);
            }

            if current {
                buf[(cx, inner.y)].set_char('▼').set_style(Style::new().fg(Color::White).bold());
            }
        }
    }
}

// ---------------------------------------------------------------- device picker

pub struct Picker {
    pub devices: Vec<Discovered>,
    pub state: ListState,
    pub trainer: Option<String>,
    pub hr: Option<String>,
}

pub fn draw_picker(f: &mut Frame, p: &mut Picker) {
    let [title, list, help] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)]).areas(f.area());
    let pick = |label: &str, v: &Option<String>| match v {
        Some(a) => Span::styled(format!("{label}: {a}   "), Style::new().green()),
        None => Span::styled(format!("{label}: —   "), Style::new().fg(Color::DarkGray)),
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(" watty — choose devices (scanning…)", Style::new().bold())),
            Line::from(vec![Span::raw(" "), pick("Trainer", &p.trainer), pick("HR", &p.hr)]),
        ]),
        title,
    );
    let items: Vec<ListItem> = p
        .devices
        .iter()
        .map(|d| {
            let tag = match (d.ftms, d.hr) {
                (true, _) => Span::styled("[FTMS] ", Style::new().cyan().bold()),
                (_, true) => Span::styled("[HR]   ", Style::new().red().bold()),
                _ => Span::styled("       ", Style::new()),
            };
            let chosen = if p.trainer.as_deref() == Some(&d.address) {
                " ← trainer"
            } else if p.hr.as_deref() == Some(&d.address) {
                " ← HR"
            } else {
                ""
            };
            let style = if d.ftms || d.hr { Style::new() } else { Style::new().fg(Color::DarkGray) };
            ListItem::new(Line::from(vec![
                tag,
                Span::styled(format!("{:<28} {}  {:>4} dBm", d.name, d.address, d.rssi.map_or("--".into(), |r| r.to_string())), style),
                Span::styled(chosen, Style::new().green().bold()),
            ]))
        })
        .collect();
    let list_widget = List::new(items)
        .block(Block::bordered().border_type(BorderType::Rounded))
        .highlight_style(Style::new().reversed())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list_widget, list, &mut p.state);
    f.render_widget(
        Line::styled(" ↑/↓ select   t = trainer   h = HR   Enter = start   q = quit", Style::new().fg(Color::DarkGray)),
        help,
    );
}
