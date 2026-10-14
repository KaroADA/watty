use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::HrZones;
use crate::devices::{DeviceEvent, LinkStatus, Role, TrainerTarget};
use crate::workout::Workout;

const STALE: Duration = Duration::from_secs(3);
const AVG_WINDOW: Duration = Duration::from_secs(3);
/// Power at or above this counts as pedaling, so auto-pause works without cadence.
const PEDALING_W: u16 = 15;
const AUTO_PAUSE_AFTER: u32 = 3;
const AUTO_RESUME_AFTER: u32 = 2;
/// After (re)starting, ERG climbs from this fraction of the target over `SOFT_START_SECS`,
/// so pushing off from a standstill isn't a wall.
const SOFT_START_FROM: f64 = 0.5;
const SOFT_START_SECS: u32 = 5;
/// HR has to stay above the segment's ceiling this long before suggesting lower intensity.
const HR_HIGH_AFTER: u32 = 60;
/// HR lags behind power, so the start of a segment doesn't count.
const HR_GRACE_SECS: u32 = 120;
const HR_SUGGEST_COOLDOWN: u32 = 120;

/// Power zone 1–6 for a fraction of FTP.
pub fn power_zone(frac: f64) -> u8 {
    [0.56, 0.76, 0.91, 1.06, 1.21].iter().take_while(|&&b| frac >= b).count() as u8 + 1
}

/// Latest sensor values; anything older than `STALE` reads as missing.
#[derive(Default)]
pub struct Sensors {
    power: VecDeque<(Instant, i16)>,
    cadence: Option<(Instant, f64)>,
    hr: Option<(Instant, u16)>,
    pub trainer_link: Option<(LinkStatus, Option<String>)>,
    pub hr_link: Option<(LinkStatus, Option<String>)>,
    pub applied: Option<TrainerTarget>,
}

fn fresh<T: Copy>(v: Option<(Instant, T)>) -> Option<T> {
    v.filter(|(t, _)| t.elapsed() < STALE).map(|(_, v)| v)
}

impl Sensors {
    /// Returns a user-facing message if the event carries one.
    pub fn apply(&mut self, ev: DeviceEvent) -> Option<String> {
        let now = Instant::now();
        match ev {
            DeviceEvent::Bike(b) => {
                if let Some(p) = b.power_w {
                    self.power.push_back((now, p.max(0)));
                    while self.power.front().is_some_and(|(t, _)| now - *t > AVG_WINDOW) {
                        self.power.pop_front();
                    }
                }
                if let Some(c) = b.cadence_rpm {
                    self.cadence = Some((now, c));
                }
            }
            DeviceEvent::Hr(h) => self.hr = Some((now, h.bpm)),
            DeviceEvent::Link { role, status, name } => {
                let msg = match &status {
                    LinkStatus::Connected => Some(format!("{} connected", role.label())),
                    LinkStatus::Disconnected(e) => Some(format!("{} disconnected: {e}", role.label())),
                    _ => None,
                };
                let slot = match role {
                    Role::Trainer => &mut self.trainer_link,
                    Role::Hr => &mut self.hr_link,
                };
                let name = name.or_else(|| slot.as_ref().and_then(|(_, n)| n.clone()));
                if role == Role::Trainer && status != LinkStatus::Connected {
                    self.applied = None;
                }
                *slot = Some((status, name));
                return msg;
            }
            DeviceEvent::TargetApplied(t) => self.applied = Some(t),
            DeviceEvent::Notice(m) => return Some(m),
        }
        None
    }

    /// 3 s average.
    pub fn power(&self) -> Option<u16> {
        let last = self.power.back()?;
        if last.0.elapsed() >= STALE {
            return None;
        }
        let sum: i64 = self.power.iter().map(|(_, p)| *p as i64).sum();
        Some((sum / self.power.len() as i64) as u16)
    }

    pub fn instant_power(&self) -> Option<u16> {
        fresh(self.power.back().copied()).map(|p| p as u16)
    }

    pub fn cadence(&self) -> Option<u16> {
        fresh(self.cadence).map(|c| c.round() as u16)
    }

    pub fn hr(&self) -> Option<u16> {
        fresh(self.hr)
    }

    pub fn pedaling(&self) -> bool {
        self.instant_power().is_some_and(|p| p >= PEDALING_W) || self.cadence().is_some_and(|c| c > 0)
    }
}

const BIKE_KG: f64 = 8.0;
const CRR: f64 = 0.004;
const CDA: f64 = 0.32;
const RHO: f64 = 1.225;
const DRIVETRAIN: f64 = 0.97;

/// Flat-road speed after `dt` seconds of `watts`, with inertia (coasts when power drops).
/// The trainer's own speed assumes a fixed big gear and is far too high in ERG.
pub fn virtual_speed(v_ms: f64, watts: f64, rider_kg: f64, dt: f64) -> f64 {
    let m = rider_kg + BIKE_KG;
    let steps = (dt / 0.1).ceil().max(1.0);
    let h = dt / steps;
    let mut v = v_ms;
    for _ in 0..steps as u32 {
        let drive = watts.max(0.0) * DRIVETRAIN / v.max(1.0);
        let resist = CRR * m * 9.81 + 0.5 * RHO * CDA * v * v;
        v = (v + (drive - resist) / m * h).max(0.0);
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Ready,
    Running,
    /// `auto`: paused because pedaling stopped; resumes when it starts again.
    Paused { auto: bool },
    /// Workout over; keeps recording until the user quits.
    Finished,
}

/// Heart rate above what the current segment calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrSuggestion {
    pub hr: u16,
    /// Power zone of the segment, whose HR zone was exceeded.
    pub zone: u8,
}

#[derive(Debug, Clone)]
pub struct Sample {
    /// Seconds since ride start, pauses included.
    pub clock: u32,
    pub power: Option<u16>,
    pub target: Option<u16>,
    pub hr: Option<u16>,
    pub cadence: Option<u16>,
    pub speed_kmh: Option<f64>,
    pub distance_m: f64,
}

pub struct Session {
    pub workout: Workout,
    pub ftp: u32,
    pub rider_kg: f64,
    pub intensity: f64,
    pub state: RunState,
    /// Position in the workout, seconds.
    pub pos: u32,
    /// Riding time, pauses excluded.
    pub elapsed: u32,
    /// Seconds since start, pauses included.
    pub clock: u32,
    pub distance_m: f64,
    /// Virtual speed, m/s.
    pub speed_ms: f64,
    /// Actual power per workout second, for the chart.
    pub trace: Vec<Option<u16>>,
    pub auto_pause: bool,
    pub soft_start: bool,
    pub hr_zones: Option<HrZones>,
    pub hr_suggestion: Option<HrSuggestion>,
    hr_high_secs: u32,
    hr_cooldown: u32,
    /// Seconds of soft start remaining.
    soft_left: u32,
    /// Consecutive seconds with / without pedaling.
    pedal_secs: u32,
    idle_secs: u32,
}

impl Session {
    pub fn new(workout: Workout, ftp: u32, rider_kg: f64, auto_pause: bool) -> Self {
        let n = workout.total_duration() as usize;
        Self {
            workout,
            ftp,
            rider_kg,
            intensity: 1.0,
            state: RunState::Ready,
            pos: 0,
            elapsed: 0,
            clock: 0,
            distance_m: 0.0,
            speed_ms: 0.0,
            trace: vec![None; n],
            auto_pause,
            soft_start: true,
            hr_zones: None,
            hr_suggestion: None,
            hr_high_secs: 0,
            hr_cooldown: 0,
            soft_left: 0,
            pedal_secs: 0,
            idle_secs: 0,
        }
    }

    pub fn speed_kmh(&self) -> f64 {
        self.speed_ms * 3.6
    }

    pub fn target_watts(&self) -> Option<u16> {
        self.workout.target_watts(self.pos as f64, self.ftp, self.intensity)
    }

    pub fn trainer_target(&self) -> TrainerTarget {
        match self.state {
            RunState::Ready => TrainerTarget::Idle,
            RunState::Finished => TrainerTarget::FreeRide,
            RunState::Running | RunState::Paused { .. } => match self.target_watts() {
                Some(w) => TrainerTarget::Erg((w as f64 * self.soft_factor()).round() as u16),
                None => TrainerTarget::FreeRide,
            },
        }
    }

    fn soft_factor(&self) -> f64 {
        match self.state {
            RunState::Paused { .. } if self.soft_start => SOFT_START_FROM,
            RunState::Running if self.soft_left > 0 => {
                let done = (SOFT_START_SECS - self.soft_left) as f64 / SOFT_START_SECS as f64;
                SOFT_START_FROM + (1.0 - SOFT_START_FROM) * done
            }
            _ => 1.0,
        }
    }

    fn resume(&mut self) {
        self.state = RunState::Running;
        if self.soft_start {
            self.soft_left = SOFT_START_SECS;
        }
    }

    pub fn toggle_pause(&mut self) {
        match self.state {
            RunState::Ready | RunState::Paused { .. } => self.resume(),
            RunState::Running => self.state = RunState::Paused { auto: false },
            RunState::Finished => {}
        }
        self.pedal_secs = 0;
        self.idle_secs = 0;
    }

    fn update_auto_pause(&mut self, pedaling: bool) {
        if pedaling {
            self.pedal_secs += 1;
            self.idle_secs = 0;
        } else {
            self.idle_secs += 1;
            self.pedal_secs = 0;
        }
        if !self.auto_pause {
            return;
        }
        match self.state {
            RunState::Running if self.idle_secs >= AUTO_PAUSE_AFTER => self.state = RunState::Paused { auto: true },
            RunState::Paused { auto: true } if self.pedal_secs >= AUTO_RESUME_AFTER => self.resume(),
            _ => {}
        }
    }

    pub fn adjust_intensity(&mut self, delta: f64) {
        self.intensity = ((self.intensity + delta) * 100.0).round().clamp(50.0, 150.0) / 100.0;
    }

    /// Endurance and tempo segments only: harder efforts are expected to push HR up.
    fn hr_ceiling(&self) -> Option<(u8, u16)> {
        let zones = self.hr_zones?;
        let seg = self.workout.segment_at(self.pos as f64)?;
        if self.pos - seg.start < HR_GRACE_SECS {
            return None;
        }
        let zone = power_zone(seg.power_at(self.pos as f64)? * self.intensity);
        (2..=3).contains(&zone).then(|| (zone, zones.0[zone as usize - 1]))
    }

    fn update_hr_check(&mut self, hr: Option<u16>) {
        self.hr_cooldown = self.hr_cooldown.saturating_sub(1);
        let high = match (hr, self.hr_ceiling()) {
            (Some(hr), Some((zone, max))) if self.state == RunState::Running && hr > max => Some(HrSuggestion { hr, zone }),
            _ => None,
        };
        let Some(high) = high else {
            self.hr_high_secs = 0;
            self.hr_suggestion = None;
            return;
        };
        self.hr_high_secs += 1;
        if self.hr_suggestion.is_some() || (self.hr_high_secs >= HR_HIGH_AFTER && self.hr_cooldown == 0) {
            self.hr_suggestion = Some(high);
        }
    }

    /// Closes the HR suggestion, lowering intensity if `accept`.
    pub fn answer_hr_suggestion(&mut self, accept: bool) {
        if self.hr_suggestion.take().is_none() {
            return;
        }
        if accept {
            self.adjust_intensity(-0.05);
        }
        self.hr_high_secs = 0;
        self.hr_cooldown = HR_SUGGEST_COOLDOWN;
    }

    pub fn skip_segment(&mut self) {
        if let Some(i) = self.workout.segment_index_at(self.pos as f64) {
            self.pos = self.workout.segments[i].end();
            if self.pos >= self.workout.total_duration() {
                self.state = RunState::Finished;
            }
        }
    }

    /// Advance one second. Returns the sample to record, if riding.
    pub fn tick(&mut self, s: &Sensors) -> Option<Sample> {
        if self.state != RunState::Ready {
            self.update_auto_pause(s.pedaling());
        }
        match self.state {
            RunState::Ready => return None,
            RunState::Paused { .. } => {
                self.clock += 1;
                self.speed_ms = 0.0;
                return None;
            }
            _ => self.clock += 1,
        }
        self.update_hr_check(s.hr());
        let target = self.target_watts();
        let power = s.instant_power();
        if let Some(slot) = self.trace.get_mut(self.pos as usize) {
            *slot = power;
        }
        self.speed_ms = virtual_speed(self.speed_ms, power.unwrap_or(0) as f64, self.rider_kg, 1.0);
        self.distance_m += self.speed_ms;
        self.elapsed += 1;
        self.soft_left = self.soft_left.saturating_sub(1);
        if self.state == RunState::Running {
            self.pos += 1;
            if self.pos >= self.workout.total_duration() {
                self.state = RunState::Finished;
            }
        }
        Some(Sample {
            clock: self.clock,
            power,
            target,
            hr: s.hr(),
            cadence: s.cadence(),
            speed_kmh: Some(self.speed_kmh()),
            distance_m: self.distance_m,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::IndoorBikeData;

    fn session() -> Session {
        let w = Workout::parse(include_str!("../workouts/test_short.zwo")).unwrap();
        let mut s = Session::new(w, 200, 75.0, true);
        s.soft_start = false;
        s
    }

    fn riding(power: Option<i16>, cadence: Option<f64>) -> Sensors {
        let mut s = Sensors::default();
        s.apply(DeviceEvent::Bike(IndoorBikeData {
            speed_kmh: None,
            cadence_rpm: cadence,
            power_w: power,
            distance_m: None,
            heart_rate: None,
        }));
        s
    }

    #[test]
    fn virtual_speed_is_realistic() {
        let mut v = 0.0;
        for _ in 0..120 {
            v = virtual_speed(v, 200.0, 75.0, 1.0);
        }
        let kmh = v * 3.6;
        assert!((31.0..36.0).contains(&kmh), "{kmh}");
        let mut v100 = 0.0;
        for _ in 0..120 {
            v100 = virtual_speed(v100, 100.0, 75.0, 1.0);
        }
        assert!((24.0..28.5).contains(&(v100 * 3.6)), "{}", v100 * 3.6);
        // Coasting slows down gradually, not instantly.
        let coast = virtual_speed(v, 0.0, 75.0, 1.0);
        assert!(coast < v && coast > v * 0.9);
    }

    #[test]
    fn ready_until_started() {
        let mut s = session();
        assert_eq!(s.trainer_target(), TrainerTarget::Idle);
        assert!(s.tick(&Sensors::default()).is_none());
        s.toggle_pause();
        assert_eq!(s.trainer_target(), TrainerTarget::Erg(80));
        assert!(s.tick(&Sensors::default()).is_some());
        assert_eq!(s.pos, 1);
    }

    #[test]
    fn pause_stops_clock() {
        let mut s = session();
        s.toggle_pause();
        s.tick(&riding(Some(100), None));
        s.toggle_pause();
        assert!(s.tick(&Sensors::default()).is_none());
        assert_eq!((s.pos, s.elapsed), (1, 1));
    }

    #[test]
    fn auto_pause_on_power_only() {
        let mut s = session();
        s.toggle_pause();
        let pedal = riding(Some(120), None);
        s.tick(&pedal);
        let idle = riding(Some(0), None);
        for _ in 0..AUTO_PAUSE_AFTER {
            s.tick(&idle);
        }
        assert_eq!(s.state, RunState::Paused { auto: true });
        s.tick(&pedal);
        assert_eq!(s.state, RunState::Paused { auto: true });
        s.tick(&pedal);
        assert_eq!(s.state, RunState::Running);
    }

    #[test]
    fn cadence_counts_as_pedaling() {
        let mut s = session();
        s.toggle_pause();
        let spin = riding(Some(0), Some(60.0));
        for _ in 0..10 {
            s.tick(&spin);
        }
        assert_eq!(s.state, RunState::Running);
    }

    #[test]
    fn no_data_auto_pauses() {
        let mut s = session();
        s.toggle_pause();
        for _ in 0..AUTO_PAUSE_AFTER {
            s.tick(&Sensors::default());
        }
        assert_eq!(s.state, RunState::Paused { auto: true });
    }

    #[test]
    fn manual_pause_does_not_auto_resume() {
        let mut s = session();
        s.toggle_pause();
        s.toggle_pause();
        let pedal = riding(Some(150), Some(90.0));
        for _ in 0..10 {
            s.tick(&pedal);
        }
        assert_eq!(s.state, RunState::Paused { auto: false });
    }

    #[test]
    fn soft_start_ramps_up_after_resume() {
        let mut s = session();
        s.soft_start = true;
        let erg = |s: &Session, frac: f64| TrainerTarget::Erg((s.target_watts().unwrap() as f64 * frac).round() as u16);
        s.toggle_pause();
        assert_eq!(s.trainer_target(), erg(&s, 0.5));
        let pedal = riding(Some(100), Some(90.0));
        s.tick(&pedal);
        assert_eq!(s.trainer_target(), erg(&s, 0.6));
        for _ in 1..SOFT_START_SECS {
            s.tick(&pedal);
        }
        assert_eq!(s.trainer_target(), erg(&s, 1.0));
        s.toggle_pause();
        assert_eq!(s.trainer_target(), erg(&s, 0.5));
        s.toggle_pause();
        assert_eq!(s.trainer_target(), erg(&s, 0.5));
    }

    #[test]
    fn auto_pause_can_be_disabled() {
        let mut s = session();
        s.auto_pause = false;
        s.toggle_pause();
        for _ in 0..10 {
            s.tick(&Sensors::default());
        }
        assert_eq!(s.state, RunState::Running);
    }

    fn with_hr(power: i16, bpm: u16) -> Sensors {
        let mut s = riding(Some(power), Some(90.0));
        s.apply(DeviceEvent::Hr(crate::protocol::HeartRate { bpm, contact: Some(true), rr_ms: vec![] }));
        s
    }

    fn endurance(secs: u32) -> Session {
        let xml = format!(r#"<workout_file><name>Z2</name><workout><SteadyState Duration="{secs}" Power="0.65"/><SteadyState Duration="600" Power="1.0"/></workout></workout_file>"#);
        let mut s = Session::new(Workout::parse(&xml).unwrap(), 200, 75.0, true);
        s.soft_start = false;
        s.hr_zones = Some(HrZones([114, 133, 152, 171]));
        s.toggle_pause();
        s
    }

    #[test]
    fn power_zones() {
        assert_eq!([0.5, 0.56, 0.75, 0.9, 1.0, 1.1, 1.5].map(power_zone), [1, 2, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn high_hr_suggests_lower_intensity() {
        let mut s = endurance(1200);
        let high = with_hr(130, 140);
        for _ in 0..HR_GRACE_SECS + HR_HIGH_AFTER - 1 {
            s.tick(&high);
        }
        assert_eq!(s.hr_suggestion, None);
        s.tick(&high);
        assert_eq!(s.hr_suggestion, Some(HrSuggestion { hr: 140, zone: 2 }));
        s.answer_hr_suggestion(true);
        assert_eq!((s.hr_suggestion, s.intensity), (None, 0.95));
        for _ in 0..HR_SUGGEST_COOLDOWN - 1 {
            s.tick(&high);
        }
        assert_eq!(s.hr_suggestion, None);
        s.tick(&high);
        assert!(s.hr_suggestion.is_some());
        s.tick(&with_hr(130, 120));
        assert_eq!(s.hr_suggestion, None);
    }

    #[test]
    fn no_hr_suggestion_for_hard_segments_or_without_zones() {
        let mut s = endurance(60);
        for _ in 0..600 {
            s.tick(&with_hr(200, 185));
        }
        assert_eq!(s.hr_suggestion, None);
        let mut s = endurance(1200);
        s.hr_zones = None;
        for _ in 0..600 {
            s.tick(&with_hr(130, 185));
        }
        assert_eq!(s.hr_suggestion, None);
    }

    #[test]
    fn intensity_and_skip() {
        let mut s = session();
        s.toggle_pause();
        s.skip_segment();
        assert_eq!(s.pos, 60);
        assert_eq!(s.trainer_target(), TrainerTarget::Erg(140));
        s.adjust_intensity(0.05);
        assert_eq!(s.trainer_target(), TrainerTarget::Erg(147));
        for _ in 0..20 {
            s.adjust_intensity(-0.05);
        }
        assert_eq!(s.intensity, 0.5);
    }

    #[test]
    fn free_ride_then_finish() {
        let mut s = session();
        s.toggle_pause();
        while s.workout.segment_at(s.pos as f64).unwrap().power_start.is_some() {
            s.skip_segment();
        }
        assert_eq!(s.trainer_target(), TrainerTarget::FreeRide);
        s.skip_segment();
        assert_eq!(s.state, RunState::Finished);
        assert_eq!(s.trainer_target(), TrainerTarget::FreeRide);
        assert!(s.tick(&Sensors::default()).is_some());
    }
}
