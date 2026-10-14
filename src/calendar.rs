//! Activities tab: a month of past rides and planned workouts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{Datelike, Local, Months, NaiveDate};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::history::{self, RideFile, RideStats};
use crate::library::Entry;
use crate::plan::{Plan, plan_path};
use crate::recorder::read_journal;
use crate::ui::{HINT, LABEL, centered, panel};
use crate::workout::fmt_duration;

const CELL: usize = 6;

pub enum Action {
    None,
    Ride(PathBuf),
    Resume(RideFile),
    Error(String, String),
}

pub struct Calendar {
    pub day: NaiveDate,
    pub plan: Plan,
    stats: HashMap<PathBuf, Option<RideStats>>,
    /// Workout picker for planning the selected day.
    adding: Option<ListState>,
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

impl Calendar {
    pub fn new() -> Self {
        Self { day: today(), plan: Plan::default(), stats: HashMap::new(), adding: None }
    }

    pub fn picking(&self) -> bool {
        self.adding.is_some()
    }

    pub fn reload_plan(&mut self) -> Result<()> {
        self.plan = Plan::load(&plan_path())?;
        Ok(())
    }

    pub fn planned_today(&self) -> impl Iterator<Item = &crate::plan::Planned> {
        let today = today();
        self.plan.workouts.iter().filter(move |p| p.date == today)
    }

    fn stats(&mut self, ride: &RideFile) -> Option<&RideStats> {
        self.stats
            .entry(ride.csv.clone())
            .or_insert_with(|| read_journal(&ride.csv).ok().map(|(_, s)| history::stats(&s)))
            .as_ref()
    }

    pub fn key(&mut self, code: KeyCode, workouts: &[Entry], rides: &[RideFile], workouts_dir: &Path) -> Action {
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

    pub fn draw(&mut self, f: &mut Frame, area: Rect, rides: &[RideFile], workouts: &[Entry], workouts_dir: &Path) {
        let [left, details] = Layout::horizontal([Constraint::Length(7 * CELL as u16 + 2), Constraint::Fill(1)]).areas(area);
        let [month, summary] = Layout::vertical([Constraint::Length(16), Constraint::Min(0)]).areas(left);
        self.draw_month(f, month, rides);
        self.draw_summary(f, summary, rides);
        self.draw_day(f, details, rides, workouts, workouts_dir);
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

    fn draw_day(&mut self, f: &mut Frame, area: Rect, rides: &[RideFile], workouts: &[Entry], workouts_dir: &Path) {
        let mut lines = vec![];
        let day = self.day;
        for r in rides.iter().filter(|r| r.started.date() == day) {
            let finished = r.meta.as_ref().is_none_or(|m| m.finished);
            lines.push(Line::from(vec![
                Span::styled("● ", Style::new().fg(Color::Green)),
                Span::styled(format!("{}  {}", r.started.format("%H:%M"), r.name()), LABEL),
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
            let width = area.width.saturating_sub(4) as usize;
            lines.extend(join_wrapped(&parts, " · ", width).into_iter().map(|l| Line::styled(format!("  {l}"), HINT)));
            lines.push(Line::raw(""));
        }
        for i in self.plan.on(self.day) {
            let p = &self.plan.workouts[i];
            let path = p.path(workouts_dir);
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
            lines.push(Line::from(spans));
        }
        if lines.is_empty() {
            lines.push(Line::styled("Nothing on this day.", HINT));
        }
        let title = self.day.format("%A, %-d %B").to_string();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel(&title)), area);
    }
}

/// Joins `parts` into lines of at most `width` chars, breaking only between parts.
fn join_wrapped(parts: &[String], sep: &str, width: usize) -> Vec<String> {
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
