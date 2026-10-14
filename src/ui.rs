use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Widget};

use crate::ble::Discovered;
use crate::config::HrZones;
use crate::devices::{LinkStatus, TrainerTarget};
use crate::session::{RunState, Sensors, Session, power_zone};
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

#[derive(Clone, Copy)]
enum Icon {
    Power,
    Target,
    Heart,
    Cadence,
    Hourglass,
    Clock,
    Flag,
}

impl Icon {
    /// Half-block pixel art, drawn at full resolution regardless of the digit scale.
    fn rows(self) -> [&'static str; GLYPH_ROWS] {
        match self {
            Icon::Power => ["   ▄███▀", " ▄███▀  ", "▀▀▀████▀", " ▄██▀▀  ", " █▀     "],
            Icon::Target => [" ▄██████▄ ", "██▀ ▄▄ ▀██", "██ ████ ██", "██▄ ▀▀ ▄██", " ▀██████▀ "],
            Icon::Heart => ["▄███▄ ▄███▄", "███████████", "▀█████████▀", "  ▀█████▀  ", "    ▀█▀    "],
            Icon::Cadence => ["    ████▄   ", "  ▄     ▀█▄ ", "▄███▄  ▀███▀", " ▀█▄     ▀  ", "   ▀████    "],
            Icon::Hourglass => ["██████████", " ▀█▄  ▄█▀ ", "   ████   ", " ▄█▀  ▀█▄ ", "██████████"],
            Icon::Flag => ["██▄▄  ▄▄  ▄▄", "██▀▀▄▄▀▀▄▄▀▀", "██▄▄▀▀▄▄▀▀▄▄", "██▀▀  ▀▀  ▀▀", "██          "],
            Icon::Clock => ["  ▄▄████▄▄  ", "▄█▀▀ ▄▄ ▀▀█▄", "██   ██▄▄ ██", "▀█▄▄    ▄▄█▀", "  ▀▀████▀▀  "],
        }
    }
}

/// Draws block art, each cell widened `scale` times.
fn draw_art(buf: &mut Buffer, x: u16, y: u16, rows: &[&str; GLYPH_ROWS], scale: u16, style: Style) {
    for (row, line) in rows.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            for s in 0..scale {
                buf[(x + col as u16 * scale + s, y + row as u16)].set_char(ch).set_style(style);
            }
        }
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
    icon: Option<Icon>,
}

impl Widget for BigValue<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(format!(" {} ", self.title), LABEL));
        let block = if self.unit.is_empty() {
            block
        } else {
            block.title_bottom(Line::from(Span::styled(format!(" {} ", self.unit), LABEL)).right_aligned())
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
        let digits_w = big_width(&self.value, scale);
        // The icon only shows when it fits next to the digits at the same scale.
        let icon_w = |icon: Icon| icon.rows()[0].chars().count() as u16 + 2 * scale;
        let icon = self.icon.filter(|&i| digits_w + icon_w(i) <= inner.width);
        let icon_w = icon.map_or(0, icon_w);
        let mut x = inner.x + (inner.width - digits_w - icon_w) / 2;
        let y0 = inner.y + (inner.height - GLYPH_ROWS as u16) / 2;
        if let Some(icon) = icon {
            draw_art(buf, x, y0, &icon.rows(), 1, Style::new().fg(self.color));
            x += icon_w;
        }
        for c in self.value.chars() {
            let g = glyph(c);
            draw_art(buf, x, y0, &g, scale, style);
            x += g[0].chars().count() as u16 * scale + 1;
        }
    }
}

// ---------------------------------------------------------------- colors

/// Small text that must stay readable from a few meters away.
pub const LABEL: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
pub const HINT: Style = Style::new().fg(Color::Gray);

pub fn zone_color(frac: f64) -> Color {
    match power_zone(frac) {
        1 => Color::Gray,
        2 => Color::Blue,
        3 => Color::Green,
        4 => Color::Yellow,
        5 => Color::Rgb(255, 140, 0),
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
    pub zoom: &'a ChartZoom,
    pub hr_zones: Option<HrZones>,
    /// Seconds before a segment change to announce it; 0 = off.
    pub segment_warning: u32,
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
    let mut spans = vec![Span::styled(" watty ", Style::new().black().on_cyan().bold()), Span::styled(format!(" {}  ", s.workout.name), LABEL)];
    spans.extend(link_span("Trainer", &v.sensors.trainer_link));
    spans.extend(link_span("HR", &v.sensors.hr_link));
    let erg = match v.sensors.applied {
        Some(TrainerTarget::Erg(w)) => Span::styled(format!("ERG {w} W ✓  "), Style::new().fg(Color::Green)),
        Some(TrainerTarget::FreeRide) => Span::styled("FREE RIDE  ", Style::new().fg(Color::Cyan)),
        _ => Span::styled("ERG –  ", Style::new().fg(Color::DarkGray)),
    };
    spans.push(erg);
    spans.push(Span::raw(format!("Intensity {:.0}%  ", s.intensity * 100.0)));
    spans.push(Span::raw(format!("{:.2} km  {:.1} km/h  ", s.distance_m / 1000.0, s.speed_kmh())));
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
    f.render_widget(BigValue { title: "POWER 3s", value: num(power), unit: "W", color: power_color, note: None, icon: Some(Icon::Power) }, cols[0]);
    let target_color = target.map_or(Color::DarkGray, |t| zone_color(t as f64 / s.ftp as f64));
    f.render_widget(BigValue { title: "TARGET", value: num(target), unit: "W", color: target_color, note: None, icon: Some(Icon::Target) }, cols[1]);
    draw_hr(f, cols[2], v);
    draw_cadence(f, cols[3], v);

    let seg_idx = s.workout.segment_index_at(s.pos as f64);
    let seg_left = seg_idx.map_or(0, |i| s.workout.segments[i].end() - s.pos);
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)]).split(row2);
    f.render_widget(BigValue { title: "SEGMENT LEFT", value: fmt_duration(seg_left), unit: "", color: Color::White, note: None, icon: Some(Icon::Hourglass) }, cols[0]);
    f.render_widget(BigValue { title: "ELAPSED", value: fmt_duration(s.elapsed), unit: "", color: Color::White, note: None, icon: Some(Icon::Clock) }, cols[1]);
    let left = s.workout.total_duration().saturating_sub(s.pos);
    f.render_widget(BigValue { title: "REMAINING", value: fmt_duration(left), unit: "", color: Color::White, note: None, icon: Some(Icon::Flag) }, cols[2]);

    f.render_widget(Chart { session: s, zoom: v.zoom.progress() }, chart);

    // status line
    let status_line = match s.state {
        RunState::Ready => Line::styled(" Press SPACE to start", Style::new().yellow().bold()),
        RunState::Paused { auto: false } => Line::styled(" ⏸ PAUSED — SPACE to resume", Style::new().yellow().bold()),
        RunState::Paused { auto: true } => Line::styled(" ⏸ AUTO-PAUSED — start pedaling to resume", Style::new().yellow().bold()),
        RunState::Finished => Line::styled(" ✓ Workout complete — keep riding or press q", Style::new().green().bold()),
        RunState::Running if s.hr_suggestion.is_some() => {
            let h = s.hr_suggestion.unwrap();
            Line::styled(format!(" ♥ HR {} above zone {} — [a] lower intensity 5%   [x] dismiss", h.hr, h.zone), Style::new().fg(Color::LightRed).bold())
        }
        RunState::Running => match v.message {
            Some((t, m)) if t.elapsed().as_secs() < 15 => Line::styled(format!(" {m}"), Style::new().fg(Color::Yellow)),
            _ => Line::styled(v.recording.map_or(String::new(), |p| format!(" ● REC {p}")), HINT),
        },
    };
    f.render_widget(status_line, status);
    let mut keys = vec![("SPACE", "pause/resume"), ("+/-", "intensity ±5%"), ("n", "next segment"), ("q", "quit")];
    if s.hr_suggestion.is_some() {
        keys.extend([("a", "lower intensity"), ("x", "dismiss")]);
    }
    if v.sim {
        keys.push(("s", "sim stop/start pedaling"));
    }
    f.render_widget(key_hints(&keys), footer);
    draw_upcoming(f, chart, v);

    if v.confirm_quit {
        let area = centered(f.area(), 56, 5);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(vec![Line::raw(""), Line::from("Quit?  [y] save ride   [d] discard   [n] cancel").bold()])
                .alignment(Alignment::Center)
                .block(Block::bordered().border_type(BorderType::Double).border_style(Style::new().yellow())),
            area,
        );
    }
}

/// Popup over the chart counting down to the next segment.
fn draw_upcoming(f: &mut Frame, area: Rect, v: &View) {
    let s = v.session;
    let Some(i) = s.workout.segment_index_at(s.pos as f64).filter(|_| s.state == RunState::Running) else { return };
    let left = s.workout.segments[i].end() - s.pos;
    if left == 0 || left > v.segment_warning {
        return;
    }
    let next = s.workout.segments.get(i + 1);
    let color = next.and_then(|n| n.power_start).map_or(Color::White, |p| zone_color(p * s.intensity));
    let title = match next {
        Some(n) => format!(" NEXT: {} · {} ", n.kind.label(), fmt_duration(n.duration)),
        None => " WORKOUT ENDS ".to_string(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(color))
        .title(Span::styled(title, Style::new().fg(Color::Black).bg(color).bold()));
    let popup = centered(area, area.width * 3 / 4, 11);
    f.render_widget(Clear, popup);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let countdown = BigValue { title: "IN", value: left.to_string(), unit: "s", color: Color::White, note: None, icon: Some(Icon::Hourglass) };
    let Some(next) = next else {
        f.render_widget(countdown, inner);
        return;
    };
    let watts = |p: f64| (p * s.ftp as f64 * s.intensity).round() as u32;
    let power = match (next.power_start, next.power_end) {
        (Some(a), Some(b)) if watts(a) != watts(b) => format!("{}-{}", watts(a), watts(b)),
        (Some(a), _) => watts(a).to_string(),
        _ => "-".to_string(),
    };
    let target = BigValue { title: "TARGET", value: power, unit: "W", color, note: None, icon: Some(Icon::Target) };
    match next.cadence {
        Some(rpm) => {
            let cols = Layout::horizontal([Constraint::Fill(1); 3]).split(inner);
            let cadence = BigValue { title: "CADENCE", value: rpm.to_string(), unit: "rpm", color: Color::White, note: None, icon: Some(Icon::Cadence) };
            f.render_widget(countdown, cols[0]);
            f.render_widget(target, cols[1]);
            f.render_widget(cadence, cols[2]);
        }
        None => {
            let cols = Layout::horizontal([Constraint::Fill(1); 2]).split(inner);
            f.render_widget(countdown, cols[0]);
            f.render_widget(target, cols[1]);
        }
    }
}

fn draw_hr(f: &mut Frame, area: Rect, v: &View) {
    let hr = v.sensors.hr();
    let value = hr.map_or("-".to_string(), |b| b.to_string());
    let Some(zone) = v.hr_zones.zip(hr).map(|(z, b)| z.zone(b)) else {
        let w = BigValue { title: "HEART RATE", value, unit: "bpm", color: Color::LightRed, note: None, icon: Some(Icon::Heart) };
        f.render_widget(w, area);
        return;
    };
    let color = hr_zone_color(zone);
    let title = format!("HEART RATE Z{zone}");
    let note = Some(Span::styled(format!(" Z{zone} "), Style::new().fg(Color::Black).bg(color).bold()));
    f.render_widget(BigValue { title: &title, value, unit: "bpm", color, note, icon: Some(Icon::Heart) }, area);
}

/// Same palette as the power zones.
pub fn hr_zone_color(zone: u8) -> Color {
    match zone {
        1 => Color::Gray,
        2 => Color::Blue,
        3 => Color::Green,
        4 => Color::Yellow,
        _ => Color::Red,
    }
}

/// Shortens paths under the home directory to `~/…`.
pub fn tilde(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => text.replace(&format!("{home}/"), "~/"),
        _ => text.to_string(),
    }
}

pub fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(format!(" {title} "), LABEL))
}

pub fn key_hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (key, desc) in keys {
        spans.push(Span::styled(key.to_string(), LABEL));
        spans.push(Span::styled(format!(" {desc}   "), HINT));
    }
    Line::from(spans)
}

/// Cadence with the workout's target range and a faster/slower hint.
fn draw_cadence(f: &mut Frame, area: Rect, v: &View) {
    let s = v.session;
    let cadence = v.sensors.cadence();
    let value = cadence.map_or("-".to_string(), |c| c.to_string());
    let Some(target) = s.workout.cadence_at(s.pos as f64).filter(|_| s.state != RunState::Finished) else {
        let w = BigValue { title: "CADENCE", value, unit: "rpm", color: Color::LightCyan, note: None, icon: Some(Icon::Cadence) };
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
    f.render_widget(BigValue { title: &title, value, unit: "rpm", color, note, icon: Some(Icon::Cadence) }, area);
}

pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// Zone-colored target profile; the ridden part shows actual power instead.
pub struct Chart<'a> {
    pub session: &'a Session,
    /// 0 = whole workout, 1 = following the cursor.
    pub zoom: f64,
}

const ZOOM_TIME: Duration = Duration::from_millis(600);

/// Animated switch between the whole-workout chart and the view following the cursor.
pub struct ChartZoom {
    follow: bool,
    since: Instant,
}

impl ChartZoom {
    pub fn new() -> Self {
        Self { follow: false, since: Instant::now().checked_sub(ZOOM_TIME).unwrap_or_else(Instant::now) }
    }

    fn linear(&self) -> f64 {
        let t = (self.since.elapsed().as_secs_f64() / ZOOM_TIME.as_secs_f64()).min(1.0);
        if self.follow { t } else { 1.0 - t }
    }

    pub fn set(&mut self, follow: bool) {
        if follow == self.follow {
            return;
        }
        // Continue from the current position if an animation is still running.
        let p = self.linear();
        let done = if follow { p } else { 1.0 - p };
        self.follow = follow;
        self.since = Instant::now().checked_sub(ZOOM_TIME.mul_f64(done)).unwrap_or_else(Instant::now);
    }

    pub fn animating(&self) -> bool {
        self.since.elapsed() < ZOOM_TIME
    }

    fn progress(&self) -> f64 {
        let p = self.linear();
        p * p * (3.0 - 2.0 * p)
    }
}

impl Widget for Chart<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let s = self.session;
        let total = s.workout.total_duration().max(1) as f64;
        let width = area.width.saturating_sub(2).max(1) as f64;
        // Too long for a readable scale: while riding, show a window that follows the cursor.
        let follow = self.zoom > 0.0 && total / width > MAX_COL_SECS;
        let (view_start, col_secs) = if follow {
            let span = width * MAX_COL_SECS;
            let start = ((s.pos as f64 - span / 3.0) / MAX_COL_SECS).floor() * MAX_COL_SECS;
            let start = start.clamp(0.0, total - span);
            let lerp = |a: f64, b: f64| a + (b - a) * self.zoom;
            let (a, b) = (lerp(0.0, start), lerp(total, start + span));
            (a, (b - a) / width)
        } else {
            (0.0, total / width)
        };
        let title = if follow {
            let end = (view_start + width * col_secs) as u32;
            format!(" PROFILE {}–{} ", fmt_duration(view_start as u32), fmt_duration(end))
        } else {
            " PROFILE ".to_string()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(title, LABEL));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        // Bottom row shows cadence targets, which power alone often doesn't reveal.
        let has_cadence = s.workout.segments.iter().any(|g| g.cadence.is_some());
        let lane = (has_cadence && inner.height > 3).then(|| Rect { y: inner.bottom() - 1, height: 1, ..inner });
        let inner = if lane.is_some() { Rect { height: inner.height - 1, ..inner } } else { inner };
        let top = (s.workout.max_power() * s.intensity).max(1.0) * 1.1;
        let h = inner.height as f64;
        let eighths = ["", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        let row_of = |frac: f64| -> u16 { ((frac / top) * h).ceil().clamp(1.0, h) as u16 };

        let ftp_row = inner.bottom() - row_of(1.0);
        let segs = &s.workout.segments;
        let col_seg: Vec<Option<usize>> =
            (0..inner.width).map(|x| s.workout.segment_index_at(view_start + (x as f64 + 0.5) * col_secs)).collect();
        // Dim every other segment in a run of the same color, so neighbors don't blend.
        let color = |p: Option<f64>| p.map(|p| zone_color(p * s.intensity));
        let mut shaded = vec![false; segs.len()];
        for i in 1..segs.len() {
            shaded[i] = color(segs[i].power_start) == color(segs[i - 1].power_end) && !shaded[i - 1];
        }
        let mut heights = vec![0u16; inner.width as usize];
        for x in 0..inner.width {
            let t0 = view_start + x as f64 * col_secs;
            let t1 = t0 + col_secs;
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
            let shade = col_seg[x as usize].is_some_and(|i| shaded[i]);
            if shade || (past && actual.flatten().is_none()) {
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
            heights[x as usize] = level.div_ceil(8) as u16;
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

        let mut runs = vec![];
        let mut x1 = 0;
        for run in col_seg.chunk_by(|a, b| a == b) {
            let x0 = x1;
            x1 += run.len() as u16;
            if let Some(i) = run[0] {
                runs.push((x0, x1, i));
            }
        }
        let clipped = |x0: u16, x1: u16| (x0 == 0 && view_start > 0.0) || (x1 == inner.width && follow);

        let watts = |p: f64| (p * s.ftp as f64 * s.intensity).round() as u32;
        let power_labels: Vec<_> = runs
            .iter()
            .filter_map(|&(x0, x1, i)| {
                let seg = &segs[i];
                let (a, b) = (watts(seg.power_start?), watts(seg.power_end?));
                let label = if a == b { a.to_string() } else { format!("{a}–{b}") };
                Some((x0, x1, label, short_duration(seg.duration), zone_color(seg.power_start? * s.intensity)))
            })
            .collect();
        let shown = consistent_labels(power_labels.iter().map(|(x0, x1, l, _, _)| (*x0, *x1, l.as_str())), clipped);
        let durations_shown = consistent_labels(power_labels.iter().map(|(x0, x1, _, d, _)| (*x0, *x1, d.as_str())), clipped);
        for ((x0, x1, label, duration, color), (show, show_duration)) in power_labels.iter().zip(shown.into_iter().zip(durations_shown)) {
            if !show {
                continue;
            }
            let top = heights[*x0 as usize..*x1 as usize].iter().copied().max().unwrap_or(0);
            let centered_x = |text: &str| inner.x + x0 + (x1 - x0 - text.chars().count() as u16) / 2;
            let on_bar = Style::new().fg(Color::Black).bg(*color).bold();
            // Above the bar if there's room under the cursor row, otherwise on its top cell.
            let y = match (inner.bottom() - top).checked_sub(1).filter(|&y| y > inner.y) {
                Some(y) => {
                    buf.set_string(centered_x(label), y, label, Style::new().fg(*color).bold());
                    y
                }
                None => {
                    let y = inner.bottom() - top.max(1);
                    buf.set_string(centered_x(label), y, label, on_bar);
                    y
                }
            };
            // Segment length goes on the bar right under the power label.
            if show_duration && y + 1 < inner.bottom() {
                buf.set_string(centered_x(duration), y + 1, duration, on_bar);
            }
        }

        let Some(lane) = lane else { return };
        let cadence_labels: Vec<_> = runs
            .iter()
            .filter_map(|&(x0, x1, i)| segs[i].cadence.map(|rpm| (x0, x1, rpm, rpm.to_string())))
            .collect();
        let shown = consistent_labels(cadence_labels.iter().map(|(x0, x1, _, l)| (*x0, *x1, l.as_str())), clipped);
        for ((x0, x1, rpm, label), show) in cadence_labels.iter().zip(shown) {
            let style = if *rpm >= HIGH_CADENCE {
                Style::new().fg(Color::Black).bg(Color::White).bold()
            } else {
                Style::new().fg(Color::White).bg(Color::DarkGray).bold()
            };
            for x in *x0..*x1 {
                buf[(lane.x + x, lane.y)].set_char(' ').set_style(style);
            }
            if show {
                buf.set_string(lane.x + x0 + (x1 - x0 - label.len() as u16) / 2, lane.y, label, style);
            }
        }
    }
}

/// Which labels to show: a label only appears if it fits in every segment carrying it,
/// so equal segments look alike. Segments cut off by the window edge don't count.
fn consistent_labels<'a>(
    labels: impl Iterator<Item = (u16, u16, &'a str)> + Clone,
    clipped: impl Fn(u16, u16) -> bool,
) -> Vec<bool> {
    let fits = |x0: u16, x1: u16, label: &str| x1 - x0 >= label.chars().count() as u16;
    let cramped: Vec<&str> =
        labels.clone().filter(|&(x0, x1, l)| !clipped(x0, x1) && !fits(x0, x1, l)).map(|(_, _, l)| l).collect();
    labels.map(|(x0, x1, l)| !cramped.contains(&l) && fits(x0, x1, l)).collect()
}

/// Compact segment length for chart labels: "30s", "5m", "2:30", "1h15".
fn short_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}:{s:02}"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m:02}"),
    }
}

/// Coarsest chart scale before it starts following the cursor, seconds per column.
const MAX_COL_SECS: f64 = 15.0;

/// Cadence targets at or above this stand out in the chart's cadence row.
const HIGH_CADENCE: u16 = 95;

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
        key_hints(&[("↑/↓", "select"), ("t", "trainer"), ("h", "HR"), ("Enter", "start"), ("q", "quit")]),
        help,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workout::Workout;

    fn render_chart(width: u16, height: u16) -> String {
        let w = Workout::parse(include_str!("../workouts/sweet_spot_3x10.zwo")).unwrap();
        let s = Session::new(w, 200, 75.0, true);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        Chart { session: &s, zoom: 0.0 }.render(area, &mut buf);
        (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn segment_durations_shown_when_they_fit() {
        let wide = render_chart(240, 16);
        assert_eq!(wide.matches("10m").count(), 4, "{wide}");
        assert_eq!(wide.matches("5m").count(), 3, "{wide}");
        let narrow = render_chart(20, 16);
        assert!(narrow.contains("10m") && !narrow.contains("5m"), "{narrow}");
    }

    #[test]
    fn short_durations() {
        assert_eq!([30, 300, 150, 3600, 4500].map(short_duration), ["30s", "5m", "2:30", "1h", "1h15"]);
    }
}
