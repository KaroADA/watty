//! Home screen: pick a workout and ride it, then come back here.

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::DeviceArgs;
use crate::config::Config;
use crate::library::{self, Entry};
use crate::ride::{self, RideSetup, key_press, spawn_keys};
use crate::session::Session;
use crate::ui::{Chart, HINT, LABEL, centered, key_hints};
use crate::workout::fmt_duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Workouts,
}

const TABS: [(Tab, &str); 1] = [(Tab::Workouts, "Workouts")];

struct Popup {
    title: String,
    lines: Vec<String>,
    error: bool,
}

struct Home {
    cfg: Config,
    tab: Tab,
    workouts: Vec<Entry>,
    list: ListState,
    popup: Option<Popup>,
}

pub async fn run(cfg: Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut keys = spawn_keys();
    let result = home(&mut terminal, &mut keys, cfg).await;
    ratatui::restore();
    result
}

async fn home(terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, cfg: Config) -> Result<()> {
    let mut h = Home { cfg, tab: Tab::Workouts, workouts: vec![], list: ListState::default().with_selected(Some(0)), popup: None };
    h.reload_workouts();
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    loop {
        terminal.draw(|f| draw(f, &mut h))?;
        let ev = tokio::select! {
            Some(ev) = keys.recv() => ev,
            _ = term.recv() => return Ok(()),
            _ = hangup.recv() => return Ok(()),
        };
        let Some((code, mods)) = key_press(&ev) else { continue };
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if h.popup.is_some() {
            if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ')) {
                h.popup = None;
            }
            continue;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Tab => h.tab = next_tab(h.tab, 1),
            KeyCode::BackTab => h.tab = next_tab(h.tab, TABS.len() - 1),
            KeyCode::Char(c @ '1'..='9') => {
                if let Some((t, _)) = TABS.get(c as usize - '1' as usize) {
                    h.tab = *t;
                }
            }
            _ => match h.tab {
                Tab::Workouts => match code {
                    KeyCode::Up | KeyCode::Char('k') => h.list.select_previous(),
                    KeyCode::Down | KeyCode::Char('j') => h.list.select_next(),
                    KeyCode::Char('r') => h.reload_workouts(),
                    KeyCode::Enter => h.start(terminal, keys, false).await,
                    KeyCode::Char('s') => h.start(terminal, keys, true).await,
                    _ => {}
                },
            },
        }
    }
}

fn next_tab(tab: Tab, step: usize) -> Tab {
    let i = TABS.iter().position(|(t, _)| *t == tab).unwrap_or(0);
    TABS[(i + step) % TABS.len()].0
}

impl Home {
    fn reload_workouts(&mut self) {
        match library::scan(&self.cfg.workouts_dir()) {
            Ok(w) => self.workouts = w,
            Err(e) => self.error("Cannot list workouts", format!("{e:#}")),
        }
        let n = self.workouts.len();
        self.list.select((n > 0).then(|| self.list.selected().unwrap_or(0).min(n - 1)));
    }

    fn error(&mut self, title: &str, text: String) {
        self.popup = Some(Popup { title: title.into(), lines: text.lines().map(String::from).collect(), error: true });
    }

    fn selected(&self) -> Option<&Entry> {
        self.workouts.get(self.list.selected()?)
    }

    async fn start(&mut self, terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, sim: bool) {
        let Some(entry) = self.selected() else { return };
        let workout = match &entry.workout {
            Ok(w) => w.clone(),
            Err(e) => return self.error("Invalid workout", e.clone()),
        };
        let (Some(ftp), Some(weight)) = (self.cfg.ftp, self.cfg.weight) else {
            return self.error("FTP and weight needed", "Set `ftp` and `weight` in ~/.config/watty/config.toml.".into());
        };
        let setup = RideSetup { workout, ftp, weight, select: false, dev: DeviceArgs::configured(sim) };
        let result = ride::ride(terminal, keys, &mut self.cfg, setup).await;
        let _ = terminal.clear();
        match result {
            Ok(summary) => {
                let lines = summary.lines().map(tilde).collect();
                self.popup = Some(Popup { title: "Ride summary".into(), lines, error: false })
            }
            Err(e) => self.error("Ride failed", format!("{e:#}")),
        }
    }
}

/// Shortens paths under the home directory to `~/…`.
fn tilde(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => text.replace(&format!("{home}/"), "~/"),
        _ => text.to_string(),
    }
}

fn draw(f: &mut Frame, h: &mut Home) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
    let mut spans = vec![Span::styled(" watty ", Style::new().black().on_cyan().bold()), Span::raw("  ")];
    for (i, (tab, name)) in TABS.iter().enumerate() {
        let style = if *tab == h.tab { Style::new().black().on_white().bold() } else { HINT };
        spans.push(Span::styled(format!(" {} {name} ", i + 1), style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Line::from(spans), header);

    let mut keys = vec![("Tab", "switch"), ("q", "quit")];
    match h.tab {
        Tab::Workouts => {
            draw_workouts(f, body, h);
            keys.splice(0..0, [("↑/↓", "select"), ("Enter", "ride"), ("s", "test ride (simulator)"), ("r", "reload")]);
        }
    }
    f.render_widget(key_hints(&keys), footer);

    if let Some(p) = &h.popup {
        draw_popup(f, p);
    }
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(format!(" {title} "), LABEL))
}

fn draw_workouts(f: &mut Frame, area: Rect, h: &mut Home) {
    let [list_area, detail] = Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)]).areas(area);
    let dir = h.cfg.workouts_dir();
    let items: Vec<ListItem> = h
        .workouts
        .iter()
        .map(|e| match &e.workout {
            Ok(w) => ListItem::new(Line::from(vec![
                Span::raw(format!("{:<30} ", e.title())),
                Span::styled(fmt_duration(w.total_duration()), HINT),
            ])),
            Err(_) => ListItem::new(Line::styled(format!("{} (invalid)", e.file_name()), Style::new().red())),
        })
        .collect();
    let list = List::new(items)
        .block(panel(&tilde(&dir.display().to_string())))
        .highlight_style(Style::new().reversed())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, list_area, &mut h.list);

    let Some(entry) = h.selected() else {
        let text = format!("No .zwo files in {}", dir.display());
        f.render_widget(Paragraph::new(text).block(panel("Workout")), detail);
        return;
    };
    let w = match &entry.workout {
        Ok(w) => w,
        Err(e) => {
            let p = Paragraph::new(e.as_str()).style(Style::new().red()).wrap(Wrap { trim: false });
            f.render_widget(p.block(panel(&entry.file_name())), detail);
            return;
        }
    };
    let [info, chart] = Layout::vertical([Constraint::Length(6), Constraint::Min(5)]).areas(detail);
    let mut lines = vec![Line::from(vec![
        Span::styled(fmt_duration(w.total_duration()), LABEL),
        Span::styled(format!("  ·  {} segments  ·  {}", w.segments.len(), entry.file_name()), HINT),
    ])];
    if !w.description.is_empty() {
        lines.push(Line::raw(w.description.clone()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel(&entry.title())), info);
    let session = Session::new(w.clone(), h.cfg.ftp.unwrap_or(200), h.cfg.weight.unwrap_or(75.0), false);
    f.render_widget(Chart { session: &session, zoom: 0.0 }, chart);
}

fn draw_popup(f: &mut Frame, p: &Popup) {
    let width = p.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(p.title.chars().count()) as u16 + 6;
    let area = centered(f.area(), width.max(30), p.lines.len() as u16 + 4);
    let color = if p.error { Color::Red } else { Color::Green };
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(color))
        .title(Span::styled(format!(" {} ", p.title), Style::new().fg(color).bold()))
        .title_bottom(Line::styled(" Enter close ", HINT).right_aligned());
    let mut lines: Vec<Line> = p.lines.iter().map(|l| Line::raw(format!(" {l}"))).collect();
    lines.insert(0, Line::raw(""));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
