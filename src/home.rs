//! Home screen: pick a workout and ride it, then come back here.

use std::path::PathBuf;

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
use crate::ble::Ble;
use crate::calendar::{self, Calendar};
use crate::settings::{self, Settings};
use crate::config::{self, Config};
use crate::history::{self, RideFile};
use crate::library::{self, Entry};
use crate::recorder::{read_journal, save_meta};
use crate::ride::{self, RideSetup, key_press, spawn_keys};
use crate::session::Session;
use crate::strava;
use crate::ui::{Chart, HINT, LABEL, centered, key_hints, panel, tilde};
use crate::workout::{Workout, fmt_duration};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Workouts,
    Activities,
    Settings,
}

const TABS: [(Tab, &str); 3] = [(Tab::Workouts, "Workouts"), (Tab::Activities, "Activities"), (Tab::Settings, "Settings")];

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
    rides: Vec<RideFile>,
    /// Interrupted ride offered for resuming, with seconds ridden.
    resume: Option<(RideFile, u32)>,
    calendar: Calendar,
    settings: Settings,
    strava: mpsc::UnboundedSender<strava::Event>,
    /// Strava login waiting for the browser: consent page URL and the waiting task.
    login: Option<(String, tokio::task::AbortHandle)>,
    /// Progress of background work, shown in the footer.
    status: Option<String>,
}

pub async fn run(cfg: Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut keys = spawn_keys();
    let result = home(&mut terminal, &mut keys, cfg).await;
    ratatui::restore();
    result
}

async fn home(terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, cfg: Config) -> Result<()> {
    let (strava_tx, mut strava_rx) = mpsc::unbounded_channel();
    let mut h = Home {
        cfg,
        tab: Tab::Workouts,
        workouts: vec![],
        list: ListState::default().with_selected(Some(0)),
        popup: None,
        rides: vec![],
        resume: None,
        calendar: Calendar::new(),
        settings: Settings::new(),
        strava: strava_tx,
        login: None,
        status: None,
    };
    h.reload_workouts();
    h.reload_rides();
    h.reload_plan();
    let dir = h.cfg.workouts_dir();
    let today: Vec<PathBuf> = h.calendar.planned_today().map(|p| p.path(&dir)).collect();
    if let Some(i) = h.workouts.iter().position(|e| today.contains(&e.path)) {
        h.list.select(Some(i));
    }
    h.resume = history::resume_offer(&h.rides).map(|r| {
        let ridden = read_journal(&r.csv).map_or(0, |(_, s)| s.len() as u32);
        (r.clone(), ridden)
    });
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    loop {
        terminal.draw(|f| draw(f, &mut h))?;
        let ev = tokio::select! {
            Some(ev) = keys.recv() => ev,
            Some(ev) = strava_rx.recv() => {
                h.strava_event(ev);
                continue;
            }
            _ = term.recv() => return Ok(()),
            _ = hangup.recv() => return Ok(()),
        };
        let Some((code, mods)) = key_press(&ev) else { continue };
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return Ok(());
        }
        if let Some((_, task)) = &h.login {
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                task.abort();
                h.login = None;
            }
            continue;
        }
        if let Some((ride, _)) = &h.resume {
            match code {
                KeyCode::Char('r') | KeyCode::Enter => {
                    let ride = ride.clone();
                    h.resume = None;
                    h.resume_ride(terminal, keys, ride).await;
                }
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                    if let Some(meta) = &ride.meta
                        && let Err(e) = save_meta(&ride.csv, &history::closed(meta))
                    {
                        h.error("Cannot update ride", format!("{e:#}"));
                    }
                    h.resume = None;
                }
                _ => {}
            }
            continue;
        }
        if h.popup.is_some() {
            if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ')) {
                h.popup = None;
            }
            continue;
        }
        if h.tab == Tab::Settings && (h.settings.editing() || !matches!(code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('1'..='9'))) {
            h.settings_key(terminal, keys, code).await;
            continue;
        }
        if h.tab == Tab::Activities && h.calendar.captures_keys() {
            h.calendar_key(terminal, keys, code).await;
            continue;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Tab => h.switch(next_tab(h.tab, 1)),
            KeyCode::BackTab => h.switch(next_tab(h.tab, TABS.len() - 1)),
            KeyCode::Char(c @ '1'..='9') => {
                if let Some((t, _)) = TABS.get(c as usize - '1' as usize) {
                    h.switch(*t);
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
                Tab::Activities => h.calendar_key(terminal, keys, code).await,
                Tab::Settings => {}
            },
        }
    }
}

fn next_tab(tab: Tab, step: usize) -> Tab {
    let i = TABS.iter().position(|(t, _)| *t == tab).unwrap_or(0);
    TABS[(i + step) % TABS.len()].0
}

impl Home {
    fn switch(&mut self, tab: Tab) {
        if tab == Tab::Activities && self.tab != tab {
            self.reload_plan();
        }
        self.tab = tab;
    }

    /// Picks up hand edits of plan.toml.
    fn reload_plan(&mut self) {
        if let Err(e) = self.calendar.reload_plan() {
            self.error("Cannot read the plan", format!("{e:#}"));
        }
    }

    async fn settings_key(&mut self, terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, code: KeyCode) {
        match self.settings.key(code, &mut self.cfg) {
            settings::Action::None => {}
            settings::Action::Saved => self.reload_workouts(),
            settings::Action::Error(e) => self.error("Invalid value", e),
            settings::Action::StravaConnect => match strava::Login::start(&self.cfg).await {
                Ok(login) => {
                    let url = login.url.clone();
                    let tx = self.strava.clone();
                    let task = tokio::spawn(async move {
                        let _ = tx.send(strava::Event::Connected(login.finish().await));
                    });
                    self.login = Some((url, task.abort_handle()));
                }
                Err(e) => self.error("Cannot connect to Strava", format!("{e:#}")),
            },
            settings::Action::StravaDisconnect => {
                if let Err(e) = strava::disconnect().await {
                    self.error("Cannot disconnect from Strava", format!("{e:#}"));
                }
                self.settings.strava_athlete = strava::load_tokens().map(|t| t.athlete);
            }
            settings::Action::PickDevices => {
                let picked = match Ble::new().await {
                    Ok(ble) => ride::pick_devices(terminal, keys, &ble, self.cfg.trainer.clone(), self.cfg.hr.clone()).await,
                    Err(e) => Err(e),
                };
                let _ = terminal.clear();
                match picked {
                    Ok(Some((trainer, hr))) => {
                        self.cfg.trainer = Some(trainer);
                        self.cfg.hr = hr;
                        if let Err(e) = self.cfg.save() {
                            self.error("Cannot save config", format!("{e:#}"));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => self.error("Bluetooth", format!("{e:#}")),
                }
            }
        }
    }

    /// Sends the rider to Settings to fill in what a ride needs.
    fn need_settings(&mut self, what: &str) {
        self.settings.select_weight_or_ftp(&self.cfg);
        self.switch(Tab::Settings);
        self.error(&format!("{what} needed"), format!("Set {} here first.", what.to_lowercase()));
    }

    async fn calendar_key(&mut self, terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, code: KeyCode) {
        match self.calendar.key(code, &self.workouts, &self.rides, &self.cfg) {
            calendar::Action::None => {}
            calendar::Action::Error(title, text) => self.error(&title, text),
            calendar::Action::Deleted => self.reload_rides(),
            calendar::Action::Upload(ride) => self.upload(ride),
            calendar::Action::Resume(ride) => self.resume_ride(terminal, keys, ride).await,
            calendar::Action::Ride(path) => {
                let loaded = match self.workouts.iter().find(|e| e.path == path) {
                    Some(e) => e.workout.clone().map_err(|e| anyhow::anyhow!(e)),
                    None => Workout::load(&path),
                };
                match loaded {
                    Ok(w) => self.start_workout(terminal, keys, path, w, false).await,
                    Err(e) => self.error("Cannot load workout", format!("{e:#}")),
                }
            }
        }
    }

    fn reload_workouts(&mut self) {
        match library::scan(&self.cfg.workouts_dir()) {
            Ok(w) => self.workouts = w,
            Err(e) => self.error("Cannot list workouts", format!("{e:#}")),
        }
        let n = self.workouts.len();
        self.list.select((n > 0).then(|| self.list.selected().unwrap_or(0).min(n - 1)));
    }

    fn reload_rides(&mut self) {
        self.rides = history::list(&config::rides_dir());
        self.calendar.refresh_open(&self.rides);
    }

    fn upload(&mut self, ride: RideFile) {
        if ride.meta.as_ref().is_some_and(|m| m.strava_id.is_some()) {
            self.status = Some("Already on Strava".into());
            return;
        }
        self.status = Some(format!("Uploading {} to Strava…", ride.name()));
        let (tx, cfg) = (self.strava.clone(), self.cfg.clone());
        tokio::spawn(async move {
            let result = async { strava::Client::load(&cfg).await?.upload(&ride, &cfg).await }.await;
            let _ = tx.send(strava::Event::Uploaded(ride.csv, result));
        });
    }

    fn strava_event(&mut self, ev: strava::Event) {
        match ev {
            strava::Event::Connected(result) => {
                self.login = None;
                match result {
                    Ok(athlete) => {
                        self.status = Some(format!("Connected to Strava as {athlete}"));
                        self.settings.strava_athlete = Some(athlete);
                    }
                    Err(e) => self.error("Cannot connect to Strava", format!("{e:#}")),
                }
            }
            strava::Event::Uploaded(csv, result) => {
                self.reload_rides();
                let name = self.rides.iter().find(|r| r.csv == csv).map(|r| r.name()).unwrap_or_default();
                match result {
                    Ok(_) => self.status = Some(format!("{name} uploaded to Strava")),
                    Err(e) => {
                        self.status = None;
                        self.error("Strava upload failed", format!("{name}\n{e:#}"));
                    }
                }
            }
        }
    }

    fn error(&mut self, title: &str, text: String) {
        self.popup = Some(Popup { title: title.into(), lines: text.lines().map(String::from).collect(), error: true });
    }

    fn selected(&self) -> Option<&Entry> {
        self.workouts.get(self.list.selected()?)
    }

    async fn start(&mut self, terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, sim: bool) {
        let Some(entry) = self.selected() else { return };
        let path = entry.path.clone();
        match entry.workout.clone() {
            Ok(w) => self.start_workout(terminal, keys, path, w, sim).await,
            Err(e) => self.error("Invalid workout", e),
        }
    }

    async fn start_workout(
        &mut self,
        terminal: &mut DefaultTerminal,
        keys: &mut mpsc::UnboundedReceiver<Event>,
        path: PathBuf,
        workout: Workout,
        sim: bool,
    ) {
        let (Some(ftp), Some(weight)) = (self.cfg.ftp, self.cfg.weight) else {
            return self.need_settings("FTP and weight");
        };
        let setup = RideSetup { workout, workout_path: Some(path), resume: None, ftp, weight, select: false, dev: DeviceArgs::configured(sim) };
        self.ride(terminal, keys, setup).await;
    }

    async fn resume_ride(&mut self, terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, ride: RideFile) {
        let Some((meta, path)) = ride.meta.as_ref().and_then(|m| Some((m, m.workout_path.clone()?))) else { return };
        let workout = match Workout::load(&path) {
            Ok(w) => w,
            Err(e) => return self.error("Cannot resume", format!("{e:#}")),
        };
        let Some(weight) = self.cfg.weight else {
            return self.need_settings("Weight");
        };
        let setup = RideSetup {
            workout,
            workout_path: Some(path),
            resume: Some(ride.csv.clone()),
            ftp: meta.ftp,
            weight,
            select: false,
            dev: DeviceArgs::configured(false),
        };
        self.ride(terminal, keys, setup).await;
    }

    async fn ride(&mut self, terminal: &mut DefaultTerminal, keys: &mut mpsc::UnboundedReceiver<Event>, setup: RideSetup) {
        let result = ride::ride(terminal, keys, &mut self.cfg, setup).await;
        self.reload_rides();
        let _ = terminal.clear();
        match result {
            Ok((summary, saved)) => {
                let lines = summary.lines().map(tilde).collect();
                self.popup = Some(Popup { title: "Ride summary".into(), lines, error: false });
                let saved = saved.and_then(|csv| self.rides.iter().find(|r| r.csv == csv).cloned());
                if let Some(ride) = saved
                    && self.cfg.strava_upload
                    && self.settings.strava_athlete.is_some()
                {
                    self.upload(ride);
                }
            }
            Err(e) => self.error("Ride failed", format!("{e:#}")),
        }
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
        Tab::Activities => {
            h.calendar.draw(f, body, &h.rides, &h.workouts, &h.cfg);
            keys = if h.calendar.captures_keys() { h.calendar.hints() } else { [h.calendar.hints(), keys].concat() };
        }
        Tab::Settings => {
            h.settings.draw(f, body, &h.cfg);
            if h.settings.disconnecting {
                keys = vec![("y", "disconnect"), ("n", "keep")];
            } else if h.settings.editing() {
                keys = vec![("Enter", "save"), ("Esc", "cancel")];
            } else {
                keys.splice(0..0, [("↑/↓", "select"), ("Enter", "change")]);
            }
        }
    }
    f.render_widget(key_hints(&keys), footer);
    if let Some(status) = &h.status {
        f.render_widget(Line::styled(format!("{status} "), Style::new().fg(Color::Cyan)).right_aligned(), footer);
    }

    if let Some(p) = &h.popup {
        draw_popup(f, p);
    }
    if let Some((ride, ridden)) = &h.resume {
        draw_resume(f, ride, *ridden);
    }
    if let Some((url, _)) = &h.login {
        draw_login(f, url);
    }
}

fn draw_login(f: &mut Frame, url: &str) {
    let area = centered(f.area(), 72, 12);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(Color::Yellow))
        .title(Span::styled(" Connect Strava ", Style::new().fg(Color::Yellow).bold()))
        .title_bottom(Line::styled(" Esc cancel ", HINT).right_aligned());
    let lines = vec![
        Line::raw(""),
        Line::raw("Authorize watty in the browser that just opened."),
        Line::raw(""),
        Line::styled("If it didn't open, visit:", HINT),
        Line::styled(url, HINT),
    ];
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block.padding(ratatui::widgets::Padding::horizontal(1))), area);
}

fn draw_workouts(f: &mut Frame, area: Rect, h: &mut Home) {
    let [list_area, detail] = Layout::horizontal([Constraint::Percentage(35), Constraint::Fill(1)]).areas(area);
    let dir = h.cfg.workouts_dir();
    let today: Vec<PathBuf> = h.calendar.planned_today().map(|p| p.path(&dir)).collect();
    let items: Vec<ListItem> = h
        .workouts
        .iter()
        .map(|e| match &e.workout {
            Ok(w) => ListItem::new(Line::from(vec![
                Span::raw(format!("{:<30} ", e.title())),
                Span::styled(fmt_duration(w.total_duration()), HINT),
                Span::styled(if today.contains(&e.path) { "  ○ today" } else { "" }, Style::new().fg(Color::Yellow)),
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

fn draw_resume(f: &mut Frame, ride: &RideFile, ridden: u32) {
    let area = centered(f.area(), 60, 6);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(Color::Yellow))
        .title(Span::styled(" Interrupted ride ", Style::new().fg(Color::Yellow).bold()));
    let lines = vec![
        Line::raw(""),
        Line::styled(format!(" {}", ride.name()), LABEL),
        Line::styled(format!(" started {}, {} ridden", ride.started.format("%H:%M"), fmt_duration(ridden)), HINT),
        Line::from(vec![Span::styled(" [r] ", LABEL), Span::raw("resume   "), Span::styled("[n] ", LABEL), Span::raw("no")]),
    ];
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
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
