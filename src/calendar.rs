//! Activities tab: a month of past rides and planned workouts.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{Datelike, Local, Months, NaiveDate};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::activity::{RideView, power_chart};
use crate::config::Config;
use crate::history::{self, RideFile, RideStats};
use crate::library::Entry;
use crate::plan::{Plan, plan_path};
use crate::recorder::{delete_ride, read_journal};
use crate::ui::{HINT, LABEL, centered, panel};
use crate::workout::fmt_duration;

const CELL: usize = 6;

pub enum Action {
    None,
    Ride(PathBuf),
    Resume(RideFile),
    /// A ride was deleted, so the ride list needs reloading.
    Deleted,
    Error(String, String),
}

/// Stats, power and target per second of a ride, for the day list.
type Summary = (RideStats, Vec<Option<u16>>, Vec<Option<u16>>);

/// Tallest power chart in the day list.
const MAX_LIST_CHART_ROWS: u16 = 14;

pub struct Calendar {
    pub day: NaiveDate,
    pub plan: Plan,
    summaries: HashMap<PathBuf, Option<Summary>>,
    /// Workout picker for planning the selected day.
    adding: Option<ListState>,
    /// Selected ride in the day list, while it has focus.
    focus: Option<usize>,
    open: Option<RideView>,
    /// Ride waiting for the user to confirm its deletion.
    deleting: Option<RideFile>,
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

impl Calendar {
    pub fn new() -> Self {
        Self { day: today(), plan: Plan::default(), summaries: HashMap::new(), adding: None, focus: None, open: None, deleting: None }
    }

    /// Keys like `q` and `Esc` step back inside the calendar instead of leaving the app.
    pub fn captures_keys(&self) -> bool {
        self.adding.is_some() || self.focus.is_some() || self.open.is_some() || self.deleting.is_some()
    }

    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.deleting.is_some() {
            vec![("y", "delete"), ("n", "keep")]
        } else if self.open.is_some() {
            vec![("x", "delete"), ("Esc", "back")]
        } else if self.adding.is_some() {
            vec![("↑/↓", "select"), ("Enter", "plan"), ("Esc", "cancel")]
        } else if self.focus.is_some() {
            vec![("↑/↓", "select"), ("Enter", "open"), ("x", "delete"), ("r", "resume"), ("Esc", "back")]
        } else {
            vec![
                ("←→↑↓", "day"),
                ("[ ]", "month"),
                ("t", "today"),
                ("Enter", "activities"),
                ("a", "plan"),
                ("d", "unplan"),
                ("p", "ride planned"),
                ("r", "resume"),
            ]
        }
    }

    fn day_rides<'a>(&self, rides: &'a [RideFile]) -> Vec<&'a RideFile> {
        rides.iter().filter(|r| r.started.date() == self.day).collect()
    }

    fn open_ride(&mut self, ride: &RideFile, cfg: &Config) -> Action {
        match RideView::load(ride, cfg.ftp, cfg.hr_zones().ok().flatten()) {
            Ok(v) => {
                self.open = Some(v);
                Action::None
            }
            Err(e) => Action::Error("Cannot open ride".into(), format!("{e:#}")),
        }
    }

    pub fn reload_plan(&mut self) -> Result<()> {
        self.plan = Plan::load(&plan_path())?;
        Ok(())
    }

    pub fn planned_today(&self) -> impl Iterator<Item = &crate::plan::Planned> {
        let today = today();
        self.plan.workouts.iter().filter(move |p| p.date == today)
    }

    fn summary(&mut self, ride: &RideFile) -> Option<&Summary> {
        self.summaries
            .entry(ride.csv.clone())
            .or_insert_with(|| {
                let (_, samples) = read_journal(&ride.csv).ok()?;
                Some((history::stats(&samples), samples.iter().map(|s| s.power).collect(), samples.iter().map(|s| s.target).collect()))
            })
            .as_ref()
    }

    fn stats(&mut self, ride: &RideFile) -> Option<&RideStats> {
        self.summary(ride).map(|(s, _, _)| s)
    }

    pub fn key(&mut self, code: KeyCode, workouts: &[Entry], rides: &[RideFile], cfg: &Config) -> Action {
        let workouts_dir = &cfg.workouts_dir();
        if self.deleting.is_some() {
            match code {
                KeyCode::Char('y') => {
                    let Some(ride) = self.deleting.take() else { return Action::None };
                    self.summaries.remove(&ride.csv);
                    self.open = None;
                    let left = self.day_rides(rides).len().saturating_sub(1);
                    self.focus = self.focus.filter(|_| left > 1).map(|i| i.min(left - 1));
                    return match delete_ride(&ride.csv) {
                        Ok(()) => Action::Deleted,
                        Err(e) => Action::Error("Cannot delete ride".into(), format!("{e:#}")),
                    };
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.deleting = None,
                _ => {}
            }
            return Action::None;
        }
        if let Some(view) = &self.open {
            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.open = None,
                KeyCode::Char('x') => self.deleting = Some(view.ride.clone()),
                _ => {}
            }
            return Action::None;
        }
        if let Some(i) = self.focus {
            let day_rides = self.day_rides(rides);
            match code {
                KeyCode::Up | KeyCode::Char('k') => self.focus = Some(i.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => self.focus = Some((i + 1).min(day_rides.len().saturating_sub(1))),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(r) = day_rides.get(i) {
                        let r = (*r).clone();
                        return self.open_ride(&r, cfg);
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(r) = day_rides.get(i).filter(|r| r.resumable()) {
                        return Action::Resume((*r).clone());
                    }
                }
                KeyCode::Char('x') => self.deleting = day_rides.get(i).map(|r| (*r).clone()),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left | KeyCode::Char('h') => self.focus = None,
                _ => {}
            }
            return Action::None;
        }
        if let Some(list) = &mut self.adding {
            let valid: Vec<&Entry> = workouts.iter().filter(|e| e.workout.is_ok()).collect();
            match code {
                KeyCode::Up | KeyCode::Char('k') => list.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => list.select_next(),
                KeyCode::Enter => {
                    if let Some(e) = list.selected().and_then(|i| valid.get(i)) {
                        self.plan.add(self.day, &e.path, workouts_dir);
                        self.adding = None;
                        return self.save();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => self.adding = None,
                _ => {}
            }
            return Action::None;
        }
        let day = self.day;
        self.day = match code {
            KeyCode::Left | KeyCode::Char('h') => day.pred_opt().unwrap_or(day),
            KeyCode::Right | KeyCode::Char('l') => day.succ_opt().unwrap_or(day),
            KeyCode::Up | KeyCode::Char('k') => day - chrono::Days::new(7),
            KeyCode::Down | KeyCode::Char('j') => day + chrono::Days::new(7),
            KeyCode::Char('[') => day.checked_sub_months(Months::new(1)).unwrap_or(day),
            KeyCode::Char(']') => day.checked_add_months(Months::new(1)).unwrap_or(day),
            KeyCode::Char('t') => today(),
            _ => day,
        };
        match code {
            KeyCode::Char('a') => self.adding = Some(ListState::default().with_selected(Some(0))),
            KeyCode::Char('d') => {
                if let Some(&i) = self.plan.on(day).last() {
                    self.plan.workouts.remove(i);
                    return self.save();
                }
            }
            KeyCode::Enter => {
                let day_rides = self.day_rides(rides);
                match day_rides.as_slice() {
                    [] => {}
                    [one] => {
                        let one = (*one).clone();
                        return self.open_ride(&one, cfg);
                    }
                    _ => self.focus = Some(0),
                }
            }
            KeyCode::Char('p') => {
                let next = self.plan.on(day).into_iter().map(|i| &self.plan.workouts[i]).find(|p| !p.done(rides));
                if let Some(p) = next {
                    return Action::Ride(p.path(workouts_dir));
                }
            }
            KeyCode::Char('r') => {
                if let Some(r) = rides.iter().rev().find(|r| r.started.date() == day && r.resumable()) {
                    return Action::Resume(r.clone());
                }
            }
            _ => {}
        }
        Action::None
    }

    fn save(&self) -> Action {
        match self.plan.save(&plan_path()) {
            Ok(()) => Action::None,
            Err(e) => Action::Error("Cannot save plan".into(), format!("{e:#}")),
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, rides: &[RideFile], workouts: &[Entry], cfg: &Config) {
        if let Some(view) = &self.open {
            view.draw(f, area);
        } else {
            self.draw_calendar(f, area, rides, workouts, cfg);
        }
        if let Some(ride) = &self.deleting {
            let popup = centered(f.area(), 56, 6);
            let block = panel("Delete ride").border_type(BorderType::Double).border_style(Style::new().fg(Color::Red));
            let lines = vec![
                Line::raw(""),
                Line::styled(format!(" {}  {}", ride.started.format("%a %-d %b %H:%M"), ride.name()), LABEL),
                Line::from(vec![
                    Span::raw(" Delete it for good?  "),
                    Span::styled("[y]", LABEL),
                    Span::raw(" yes   "),
                    Span::styled("[n]", LABEL),
                    Span::raw(" no"),
                ]),
            ];
            f.render_widget(Clear, popup);
            f.render_widget(Paragraph::new(lines).block(block), popup);
        }
    }

    fn draw_calendar(&mut self, f: &mut Frame, area: Rect, rides: &[RideFile], workouts: &[Entry], cfg: &Config) {
        let [left, details] = Layout::horizontal([Constraint::Length(7 * CELL as u16 + 2), Constraint::Fill(1)]).areas(area);
        let [month, summary] = Layout::vertical([Constraint::Length(16), Constraint::Min(0)]).areas(left);
        self.draw_month(f, month, rides);
        self.draw_summary(f, summary, rides);
        self.draw_day(f, details, rides, workouts, cfg);
        if let Some(list) = &mut self.adding {
            let items: Vec<ListItem> = workouts
                .iter()
                .filter_map(|e| Some(ListItem::new(format!("{:<32} {}", e.title(), fmt_duration(e.workout.as_ref().ok()?.total_duration())))))
                .collect();
            let popup = centered(f.area(), 50, (items.len() as u16 + 2).min(20));
            let block = panel(&format!("Plan for {}", self.day.format("%a %-d %b")))
                .border_type(BorderType::Double)
                .border_style(Style::new().fg(Color::Yellow));
            f.render_widget(Clear, popup);
            f.render_stateful_widget(
                List::new(items).block(block).highlight_style(Style::new().reversed()).highlight_symbol("▶ "),
                popup,
                list,
            );
        }
    }

    fn draw_month(&self, f: &mut Frame, area: Rect, rides: &[RideFile]) {
        let first = self.day.with_day(1).unwrap_or(self.day);
        let offset = first.weekday().num_days_from_monday() as usize;
        let days = first.checked_add_months(Months::new(1)).map_or(31, |next| (next - first).num_days() as usize);
        let mut lines = vec![Line::styled(
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map(|d| format!("{d:>4}  ")).concat(),
            HINT,
        )];
        for week in 0..(offset + days).div_ceil(7) {
            let mut spans = vec![];
            for col in 0..7 {
                let n = week * 7 + col;
                if n < offset || n >= offset + days {
                    spans.push(Span::raw(" ".repeat(CELL)));
                    continue;
                }
                let date = first + chrono::Days::new((n - offset) as u64);
                let ridden = rides.iter().any(|r| r.started.date() == date);
                let planned = self.plan.workouts.iter().any(|p| p.date == date && !p.done(rides));
                let mut num = Style::new();
                if date == today() {
                    num = num.fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                let selected = date == self.day;
                let cell = |s: Style| if selected { s.add_modifier(Modifier::REVERSED) } else { s };
                spans.push(Span::styled(format!("{:>3}", date.day()), cell(num)));
                spans.push(Span::styled(if ridden { "●" } else { " " }, cell(Style::new().fg(Color::Green))));
                spans.push(Span::styled(if planned { "○" } else { " " }, cell(Style::new().fg(Color::Yellow))));
                spans.push(Span::raw(" "));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines).block(panel(&self.day.format("%B %Y").to_string())), area);
    }

    fn draw_summary(&mut self, f: &mut Frame, area: Rect, rides: &[RideFile]) {
        let (year, month) = (self.day.year(), self.day.month());
        let in_month: Vec<&RideFile> = rides.iter().filter(|r| r.started.year() == year && r.started.month() == month).collect();
        let (mut secs, mut meters, mut kj) = (0, 0.0, 0.0);
        for r in &in_month {
            if let Some(s) = self.stats(r) {
                secs += s.duration;
                meters += s.distance_m;
                kj += s.kj;
            }
        }
        let lines = vec![
            Line::from(vec![Span::styled(format!("{} ride{}", in_month.len(), if in_month.len() == 1 { "" } else { "s" }), LABEL)]),
            Line::styled(format!("{} · {:.1} km · {:.0} kJ", fmt_duration(secs), meters / 1000.0, kj), HINT),
            Line::raw(""),
            Line::from(vec![
                Span::styled("●", Style::new().fg(Color::Green)),
                Span::styled(" ridden  ", HINT),
                Span::styled("○", Style::new().fg(Color::Yellow)),
                Span::styled(" planned", HINT),
            ]),
        ];
        f.render_widget(Paragraph::new(lines).block(panel("This month")), area);
    }

    fn draw_day(&mut self, f: &mut Frame, area: Rect, rides: &[RideFile], workouts: &[Entry], cfg: &Config) {
        let block = panel(&self.day.format("%A, %-d %B").to_string());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let width = inner.width.saturating_sub(2);
        let workouts_dir = cfg.workouts_dir();

        let day_rides = self.day_rides(rides);
        let mut texts: Vec<Vec<Line>> = vec![];
        let mut charted = vec![];
        for (i, r) in day_rides.iter().enumerate() {
            let mut lines = vec![];
            let finished = r.meta.as_ref().is_none_or(|m| m.finished);
            let selected = self.focus == Some(i);
            let name_style = if selected { LABEL.add_modifier(Modifier::REVERSED) } else { LABEL };
            lines.push(Line::from(vec![
                Span::styled(if selected { "▶ " } else { "● " }, Style::new().fg(Color::Green)),
                Span::styled(format!("{}  {}", r.started.format("%H:%M"), r.name()), name_style),
            ]));
            if !finished {
                let resume = if r.resumable() { " · [r] resume" } else { "" };
                lines.push(Line::styled(format!("  unfinished{resume}"), Style::new().fg(Color::Yellow)));
            }
            let parts = match self.stats(r) {
                Some(s) => {
                    let mut parts = vec![fmt_duration(s.duration), format!("{:.1} km", s.distance_m / 1000.0)];
                    parts.extend(s.avg_power.map(|p| format!("avg {p} W")));
                    parts.extend(s.np.map(|p| format!("NP {p} W")));
                    if let (Some(avg), Some(max)) = (s.avg_hr, s.max_hr) {
                        parts.push(format!("HR {avg}/{max}"));
                    }
                    parts.push(format!("{:.0} kJ", s.kj));
                    parts
                }
                None => vec!["cannot read the ride".into()],
            };
            lines.extend(join_wrapped(&parts, " · ", width as usize).into_iter().map(|l| Line::styled(format!("  {l}"), HINT)));
            texts.push(lines);
            charted.push(self.summary(r).is_some_and(|(_, p, _)| p.iter().any(Option::is_some)) && width >= 10);
        }

        let mut plan_lines = vec![];
        for i in self.plan.on(self.day) {
            let p = &self.plan.workouts[i];
            let path = p.path(&workouts_dir);
            let entry = workouts.iter().find(|e| e.path == path);
            let name = entry.map_or_else(|| p.file.display().to_string(), |e| e.title());
            let (mark, style) = if p.done(rides) { ("✓ ", Color::Green) } else { ("○ ", Color::Yellow) };
            let mut spans = vec![Span::styled(mark, Style::new().fg(style)), Span::styled(name, LABEL)];
            match entry.map(|e| &e.workout) {
                Some(Ok(w)) => spans.push(Span::styled(format!("  {}", fmt_duration(w.total_duration())), HINT)),
                Some(Err(_)) => spans.push(Span::styled("  invalid workout", Style::new().fg(Color::Red))),
                None if !path.exists() => spans.push(Span::styled("  file not found", Style::new().fg(Color::Red))),
                None => {}
            }
            plan_lines.push(Line::from(spans));
        }
        if day_rides.is_empty() && plan_lines.is_empty() {
            plan_lines.push(Line::styled("Nothing on this day.", HINT));
        }

        // Charts share whatever height the text leaves.
        let text_rows: u16 = texts.iter().map(|t| t.len() as u16 + 1).sum::<u16>() + plan_lines.len() as u16;
        let n_charts = charted.iter().filter(|&&c| c).count() as u16;
        let chart_h = inner.height.saturating_sub(text_rows).checked_div(n_charts).map_or(0, |h| h.clamp(1, MAX_LIST_CHART_ROWS));
        let clip = |y: u16, h: u16| Rect { y, height: h.min(inner.bottom().saturating_sub(y)), ..inner };
        let mut y = inner.y;
        for ((r, lines), chart) in day_rides.iter().zip(texts).zip(charted) {
            let h = lines.len() as u16;
            f.render_widget(Paragraph::new(lines), clip(y, h));
            y += h;
            if chart && y < inner.bottom() {
                let ftp = r.meta.as_ref().map(|m| m.ftp).or(cfg.ftp);
                if let Some((_, power, target)) = self.summary(r) {
                    let a = clip(y, chart_h);
                    power_chart(f.buffer_mut(), Rect { x: a.x + 2, width, ..a }, power, target, ftp);
                }
                y += chart_h;
            }
            y += 1;
            if y >= inner.bottom() {
                return;
            }
        }
        f.render_widget(Paragraph::new(plan_lines).wrap(Wrap { trim: false }), clip(y, inner.bottom() - y));
    }
}

/// Joins `parts` into lines of at most `width` chars, breaking only between parts.
pub fn join_wrapped(parts: &[String], sep: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for part in parts {
        match lines.last_mut() {
            Some(line) if line.chars().count() + sep.chars().count() + part.chars().count() <= width => {
                line.push_str(sep);
                line.push_str(part);
            }
            _ => lines.push(part.clone()),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_wrap_between_parts() {
        let parts = ["40:21", "19.5 km", "avg 140 W", "NP 148 W"].map(String::from);
        assert_eq!(join_wrapped(&parts, " · ", 80), ["40:21 · 19.5 km · avg 140 W · NP 148 W"]);
        assert_eq!(join_wrapped(&parts, " · ", 30), ["40:21 · 19.5 km · avg 140 W", "NP 148 W"]);
        assert_eq!(join_wrapped(&parts, " · ", 5), ["40:21", "19.5 km", "avg 140 W", "NP 148 W"]);
    }
}
