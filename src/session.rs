use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::devices::{DeviceEvent, LinkStatus, Role, TrainerTarget};
use crate::workout::Workout;

const STALE: Duration = Duration::from_secs(3);
const AVG_WINDOW: Duration = Duration::from_secs(3);

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
    Paused,
    /// Workout over; keeps recording until the user quits.
    Finished,
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
}

impl Session {
    pub fn new(workout: Workout, ftp: u32, rider_kg: f64) -> Self {
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
            RunState::Running | RunState::Paused => {
                self.target_watts().map_or(TrainerTarget::FreeRide, TrainerTarget::Erg)
            }
        }
    }

    pub fn toggle_pause(&mut self) {
        self.state = match self.state {
            RunState::Ready | RunState::Paused => RunState::Running,
            RunState::Running => RunState::Paused,
            RunState::Finished => RunState::Finished,
        };
    }

    pub fn adjust_intensity(&mut self, delta: f64) {
        self.intensity = ((self.intensity + delta) * 100.0).round().clamp(50.0, 150.0) / 100.0;
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
        match self.state {
            RunState::Ready => return None,
            RunState::Paused => {
                self.clock += 1;
                self.speed_ms = 0.0;
                return None;
            }
            _ => self.clock += 1,
        }
        let target = self.target_watts();
        let power = s.instant_power();
        if let Some(slot) = self.trace.get_mut(self.pos as usize) {
            *slot = power;
        }
        self.speed_ms = virtual_speed(self.speed_ms, power.unwrap_or(0) as f64, self.rider_kg, 1.0);
        self.distance_m += self.speed_ms;
        self.elapsed += 1;
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

    fn session() -> Session {
        let w = Workout::parse(include_str!("../workouts/test_short.zwo")).unwrap();
        Session::new(w, 200, 75.0)
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
        s.tick(&Sensors::default());
        s.toggle_pause();
        assert!(s.tick(&Sensors::default()).is_none());
        assert_eq!((s.pos, s.elapsed), (1, 1));
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
