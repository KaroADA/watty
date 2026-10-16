//! Settings tab: edits `config.toml` in place.

use std::path::PathBuf;

use anyhow::{Result, bail};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::config::{Config, config_path};
use crate::ui::{HINT, LABEL, panel, tilde};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Ftp,
    Weight,
    MaxHr,
    HrZones,
    CadenceTolerance,
    SegmentWarning,
    AutoPause,
    KeepAwake,
    SoftStart,
    WorkoutsDir,
    Trainer,
    HrSensor,
    Strava,
    StravaUpload,
}

const FIELDS: [Field; 14] = [
    Field::Ftp,
    Field::Weight,
    Field::MaxHr,
    Field::HrZones,
    Field::CadenceTolerance,
    Field::SegmentWarning,
    Field::AutoPause,
    Field::KeepAwake,
    Field::SoftStart,
    Field::WorkoutsDir,
    Field::Trainer,
    Field::HrSensor,
    Field::Strava,
    Field::StravaUpload,
];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Ftp => "FTP",
            Field::Weight => "Weight",
            Field::MaxHr => "Max heart rate",
            Field::HrZones => "Heart-rate zones",
            Field::CadenceTolerance => "Cadence tolerance",
            Field::SegmentWarning => "Segment warning",
            Field::AutoPause => "Auto-pause",
            Field::KeepAwake => "Keep screen awake",
            Field::SoftStart => "Soft start",
            Field::WorkoutsDir => "Workouts folder",
            Field::Trainer => "Trainer",
            Field::HrSensor => "HR sensor",
            Field::Strava => "Strava",
            Field::StravaUpload => "Upload to Strava",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Ftp => "Functional threshold power in watts. Workout targets are fractions of it.",
            Field::Weight => "Rider weight in kg (30–200), used for virtual speed and distance.",
            Field::MaxHr => "Maximum heart rate. Without explicit zones, zones 1–4 end at 60/70/80/90% of it. Empty = no zones.",
            Field::HrZones => "Upper bounds of heart-rate zones 1–4 in bpm, e.g. `120, 140, 155, 170`. Overrides max heart rate. Empty = use max heart rate.",
            Field::CadenceTolerance => "Acceptable deviation from a workout's target cadence, ± rpm.",
            Field::SegmentWarning => "Seconds before a segment change to show the countdown popup. 0 turns it off.",
            Field::AutoPause => "Pause when pedaling stops and resume when it starts again.",
            Field::KeepAwake => "Keep the screen from dimming or locking during a ride.",
            Field::SoftStart => "While paused ERG drops to 50% of the target, and after starting or resuming it climbs back over 5 s.",
            Field::WorkoutsDir => "Folder with .zwo files. Empty = ~/.local/share/watty/workouts.",
            Field::Trainer => "FTMS smart trainer. Enter opens the device picker.",
            Field::HrSensor => "Heart-rate strap. Enter opens the device picker.",
            Field::Strava => "Strava account. Enter connects it in the browser, or disconnects it.",
            Field::StravaUpload => "Upload each saved ride to Strava right after it ends. Any ride can also be uploaded from Activities with `u`.",
        }
    }

    fn is_bool(self) -> bool {
        matches!(self, Field::AutoPause | Field::KeepAwake | Field::SoftStart | Field::StravaUpload)
    }

    fn is_device(self) -> bool {
        matches!(self, Field::Trainer | Field::HrSensor)
    }

    fn get(self, cfg: &Config) -> String {
        let opt = |v: Option<String>| v.unwrap_or_default();
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        match self {
            Field::Ftp => opt(cfg.ftp.map(|v| v.to_string())),
            Field::Weight => opt(cfg.weight.map(|v| v.to_string())),
            Field::MaxHr => opt(cfg.max_hr.map(|v| v.to_string())),
            Field::HrZones => opt(cfg.hr_zones.as_ref().map(|z| z.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "))),
            Field::CadenceTolerance => cfg.cadence_tolerance.to_string(),
            Field::SegmentWarning => cfg.segment_warning.to_string(),
            Field::AutoPause => on_off(cfg.auto_pause),
            Field::KeepAwake => on_off(cfg.keep_awake),
            Field::SoftStart => on_off(cfg.soft_start),
            Field::WorkoutsDir => opt(cfg.workouts_dir.as_ref().map(|p| p.display().to_string())),
            Field::Trainer => opt(cfg.trainer.clone()),
            Field::HrSensor => opt(cfg.hr.clone()),
            Field::StravaUpload => on_off(cfg.strava_upload),
            Field::Strava => String::new(),
        }
    }

    /// Shown for an empty value.
    fn placeholder(self, cfg: &Config) -> String {
        match self {
            Field::Ftp | Field::Weight => "not set".into(),
            Field::HrZones => match cfg.hr_zones() {
                Ok(Some(z)) => format!("from max heart rate ({})", z.0.map(|v| v.to_string()).join(", ")),
                _ => "off".into(),
            },
            Field::MaxHr => "off".into(),
            Field::Strava => "not connected".into(),
            Field::WorkoutsDir => format!("default ({})", tilde(&cfg.workouts_dir().display().to_string())),
            _ => "none".into(),
        }
    }

    fn set(self, cfg: &mut Config, text: &str) -> Result<()> {
        let text = text.trim();
        fn num<T: std::str::FromStr>(text: &str) -> Result<Option<T>> {
            if text.is_empty() {
                return Ok(None);
            }
            match text.parse() {
                Ok(v) => Ok(Some(v)),
                Err(_) => bail!("not a number: {text}"),
            }
        }
        match self {
            Field::Ftp => cfg.ftp = num(text)?.filter(|&v| v > 0),
            Field::Weight => {
                let w: Option<f64> = num(text)?;
                if w.is_some_and(|w| !(30.0..=200.0).contains(&w)) {
                    bail!("weight must be 30–200 kg");
                }
                cfg.weight = w;
            }
            Field::MaxHr => cfg.max_hr = num(text)?,
            Field::HrZones => {
                cfg.hr_zones = if text.is_empty() {
                    None
                } else {
                    Some(text.split([',', ' ']).filter(|s| !s.is_empty()).map(|s| num(s).map(Option::unwrap)).collect::<Result<_>>()?)
                };
                cfg.hr_zones()?;
            }
            Field::CadenceTolerance => cfg.cadence_tolerance = num(text)?.unwrap_or(0),
            Field::SegmentWarning => cfg.segment_warning = num(text)?.unwrap_or(0),
            Field::AutoPause => cfg.auto_pause = !cfg.auto_pause,
            Field::KeepAwake => cfg.keep_awake = !cfg.keep_awake,
            Field::SoftStart => cfg.soft_start = !cfg.soft_start,
            Field::StravaUpload => cfg.strava_upload = !cfg.strava_upload,
            Field::WorkoutsDir => cfg.workouts_dir = (!text.is_empty()).then(|| PathBuf::from(expand_home(text))),
            Field::Trainer | Field::HrSensor | Field::Strava => {}
        }
        Ok(())
    }
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

pub enum Action {
    None,
    /// Config changed and was saved.
    Saved,
    PickDevices,
    StravaConnect,
    StravaDisconnect,
    Error(String),
}

pub struct Settings {
    list: ListState,
    /// Text being typed for the selected field.
    editing: Option<String>,
    /// Name of the connected Strava athlete.
    pub strava_athlete: Option<String>,
    /// Waiting for the user to confirm disconnecting Strava.
    pub disconnecting: bool,
}

impl Settings {
    pub fn new() -> Self {
        let strava_athlete = crate::strava::load_tokens().map(|t| t.athlete);
        Self { list: ListState::default().with_selected(Some(0)), editing: None, strava_athlete, disconnecting: false }
    }

    /// Keys go to the settings instead of switching tabs or quitting.
    pub fn editing(&self) -> bool {
        self.editing.is_some() || self.disconnecting
    }

    fn field(&self) -> Field {
        FIELDS[self.list.selected().unwrap_or(0).min(FIELDS.len() - 1)]
    }

    pub fn select_weight_or_ftp(&mut self, cfg: &Config) {
        let i = if cfg.ftp.is_none() { 0 } else { 1 };
        self.list.select(Some(i));
    }

    pub fn key(&mut self, code: KeyCode, cfg: &mut Config) -> Action {
        let field = self.field();
        if self.disconnecting {
            self.disconnecting = false;
            return if code == KeyCode::Char('y') { Action::StravaDisconnect } else { Action::None };
        }
        if let Some(text) = &mut self.editing {
            match code {
                KeyCode::Char(c) => text.push(c),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Esc => self.editing = None,
                KeyCode::Enter => {
                    let text = self.editing.take().unwrap_or_default();
                    return apply(cfg, field, &text);
                }
                _ => {}
            }
            return Action::None;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.list.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.list.select(Some((self.list.selected().unwrap_or(0) + 1).min(FIELDS.len() - 1))),
            KeyCode::Enter | KeyCode::Char(' ') if field.is_device() => return Action::PickDevices,
            KeyCode::Enter | KeyCode::Char(' ') if field == Field::Strava => {
                if self.strava_athlete.is_none() {
                    return Action::StravaConnect;
                }
                self.disconnecting = true;
            }
            KeyCode::Enter | KeyCode::Char(' ') if field.is_bool() => return apply(cfg, field, ""),
            KeyCode::Enter => self.editing = Some(field.get(cfg)),
            _ => {}
        }
        Action::None
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, cfg: &Config) {
        let [list_area, help] = Layout::horizontal([Constraint::Percentage(55), Constraint::Fill(1)]).areas(area);
        let selected = self.field();
        let items: Vec<ListItem> = FIELDS
            .iter()
            .map(|&field| {
                let value = match &self.editing {
                    Some(text) if field == selected => Span::styled(format!("{text}▏"), Style::new().fg(Color::Yellow)),
                    _ if field == Field::Strava && self.disconnecting => {
                        Span::styled("disconnect? [y] yes  [n] no", Style::new().fg(Color::Yellow))
                    }
                    _ if field == Field::Strava => match &self.strava_athlete {
                        Some(name) if name.is_empty() => Span::raw("connected"),
                        Some(name) => Span::raw(format!("connected as {name}")),
                        None => Span::styled(field.placeholder(cfg), Style::new().fg(Color::DarkGray)),
                    },
                    _ => match field.get(cfg) {
                        v if v.is_empty() => Span::styled(field.placeholder(cfg), Style::new().fg(Color::DarkGray)),
                        v => Span::raw(v),
                    },
                };
                ListItem::new(Line::from(vec![Span::styled(format!("{:<20}", field.label()), LABEL), value]))
            })
            .collect();
        let list = List::new(items).block(panel("Settings")).highlight_style(Style::new().reversed()).highlight_symbol("▶ ");
        let list = if self.editing.is_some() { list.highlight_style(Style::new()) } else { list };
        f.render_stateful_widget(list, list_area, &mut self.list);

        let lines = vec![
            Line::raw(selected.help()),
            Line::raw(""),
            Line::styled(format!("Saved to {}", tilde(&config_path().display().to_string())), HINT),
        ];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel(selected.label())), help);
    }
}

/// Changes one field and saves, leaving `cfg` untouched if the value is invalid.
fn apply(cfg: &mut Config, field: Field, text: &str) -> Action {
    let mut new = cfg.clone();
    if let Err(e) = field.set(&mut new, text) {
        return Action::Error(format!("{e:#}"));
    }
    if let Err(e) = new.save() {
        return Action::Error(format!("{e:#}"));
    }
    *cfg = new;
    Action::Saved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_parse_and_validate() {
        let mut cfg = Config::default();
        Field::Ftp.set(&mut cfg, " 250 ").unwrap();
        Field::Weight.set(&mut cfg, "72.5").unwrap();
        Field::HrZones.set(&mut cfg, "120, 140,155 170").unwrap();
        assert_eq!((cfg.ftp, cfg.weight, cfg.hr_zones.clone()), (Some(250), Some(72.5), Some(vec![120, 140, 155, 170])));
        assert_eq!(Field::HrZones.get(&cfg), "120, 140, 155, 170");

        assert!(Field::Weight.set(&mut cfg, "300").is_err());
        assert!(Field::Ftp.set(&mut cfg, "abc").is_err());
        assert!(Field::HrZones.set(&mut cfg, "120, 110, 155, 170").is_err());
        assert!(Field::HrZones.set(&mut cfg, "120, 140").is_err());

        Field::Ftp.set(&mut cfg, "").unwrap();
        Field::HrZones.set(&mut cfg, "").unwrap();
        assert_eq!((cfg.ftp, cfg.hr_zones.clone()), (None, None));

        Field::AutoPause.set(&mut cfg, "").unwrap();
        assert!(!cfg.auto_pause);
    }
}
