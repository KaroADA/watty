//! `.zwo` parser. Power stays as FTP fraction; watts are computed on use.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

const RAMP_START_W: u32 = 100;
const RAMP_STEP_W: u32 = 20;
const RAMP_STEP_SECS: u32 = 60;
/// The ramp test's own warmup climbs from here to `RAMP_START_W`.
const RAMP_WARMUP_FROM_W: u32 = 60;
/// Ramp tests don't need an FTP; this one just scales the warmup and the chart.
pub const RAMP_DEFAULT_FTP: u32 = 200;
/// FTP as a fraction of the best minute.
pub const RAMP_FTP_FACTOR: f64 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Warmup,
    Cooldown,
    Steady,
    Ramp,
    IntervalOn,
    IntervalOff,
    FreeRide,
}

impl SegmentKind {
    pub fn label(self) -> &'static str {
        match self {
            SegmentKind::Warmup => "Warmup",
            SegmentKind::Cooldown => "Cooldown",
            SegmentKind::Steady => "Steady",
            SegmentKind::Ramp => "Ramp",
            SegmentKind::IntervalOn => "Interval ON",
            SegmentKind::IntervalOff => "Interval OFF",
            SegmentKind::FreeRide => "Free ride",
        }
    }
}

/// Linear ramp power_start → power_end; `None` = free ride.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub kind: SegmentKind,
    /// Offset from workout start, seconds.
    pub start: u32,
    pub duration: u32,
    pub power_start: Option<f64>,
    pub power_end: Option<f64>,
    /// Target cadence, rpm.
    pub cadence: Option<u16>,
}

impl Segment {
    pub fn end(&self) -> u32 {
        self.start + self.duration
    }

    pub fn power_at(&self, t: f64) -> Option<f64> {
        let (a, b) = (self.power_start?, self.power_end?);
        if self.duration == 0 {
            return Some(a);
        }
        let frac = ((t - self.start as f64) / self.duration as f64).clamp(0.0, 1.0);
        Some(a + (b - a) * frac)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workout {
    pub name: String,
    pub description: String,
    pub segments: Vec<Segment>,
    /// Ends with a ramp test (`<RampTest/>`), whose segments are added by `plan_ramp`.
    pub ramp: Option<RampTest>,
}

/// Steps in watts, not FTP fractions: FTP is what the test measures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RampTest {
    /// Offset of the ramp test in the workout, seconds.
    pub start: u32,
    /// Warmup within the ramp test, seconds.
    pub warmup: u32,
}

impl RampTest {
    /// Where the steps start.
    pub fn steps_start(&self) -> u32 {
        self.start + self.warmup
    }
}

impl Workout {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn parse(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(xml)?;
        let root = doc.root_element();
        if !root.has_tag_name("workout_file") {
            bail!("root element is <{}>, expected <workout_file>", root.tag_name().name());
        }
        let child_text = |name: &str| {
            root.children()
                .find(|n| n.has_tag_name(name))
                .and_then(|n| n.text())
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        let name = child_text("name");
        let description = child_text("description");
        let body = root
            .children()
            .find(|n| n.has_tag_name("workout"))
            .ok_or_else(|| anyhow!("missing <workout> element"))?;

        let mut segments = Vec::new();
        let mut ramp = None;
        let mut t = 0u32;
        let mut push = |kind, duration: u32, a: Option<f64>, b: Option<f64>, cadence: Option<u16>| {
            if duration > 0 {
                segments.push(Segment { kind, start: t, duration, power_start: a, power_end: b, cadence });
                t += duration;
            }
        };

        for el in body.children().filter(|n| n.is_element()) {
            let tag = el.tag_name().name();
            let ctx = || format!("<{tag}> at line {}", doc.text_pos_at(el.range().start).row);
            if ramp.is_some() {
                bail!("<RampTest> must be the last element, found <{tag}> after it");
            }
            let cadence = rpm(&el, "Cadence").with_context(ctx)?;
            match tag {
                "Warmup" | "Cooldown" | "Ramp" => {
                    let kind = match tag {
                        "Warmup" => SegmentKind::Warmup,
                        "Cooldown" => SegmentKind::Cooldown,
                        _ => SegmentKind::Ramp,
                    };
                    let d = duration(&el, "Duration").with_context(ctx)?;
                    let lo = number(&el, "PowerLow").with_context(ctx)?;
                    let hi = number(&el, "PowerHigh").with_context(ctx)?;
                    push(kind, d, Some(lo), Some(hi), cadence);
                }
                // "SolidState" is a historical typo that Zwift itself accepts.
                "SteadyState" | "SolidState" => {
                    let d = duration(&el, "Duration").with_context(ctx)?;
                    let p = number(&el, "Power").with_context(ctx)?;
                    push(SegmentKind::Steady, d, Some(p), Some(p), cadence);
                }
                "IntervalsT" => {
                    let repeat = number(&el, "Repeat").with_context(ctx)?.round() as u32;
                    let on_d = duration(&el, "OnDuration").with_context(ctx)?;
                    let off_d = duration(&el, "OffDuration").with_context(ctx)?;
                    let on_p = number(&el, "OnPower").with_context(ctx)?;
                    let off_p = number(&el, "OffPower").with_context(ctx)?;
                    let resting = rpm(&el, "CadenceResting").with_context(ctx)?;
                    for _ in 0..repeat {
                        push(SegmentKind::IntervalOn, on_d, Some(on_p), Some(on_p), cadence);
                        push(SegmentKind::IntervalOff, off_d, Some(off_p), Some(off_p), resting);
                    }
                }
                "FreeRide" => {
                    let d = duration(&el, "Duration").with_context(ctx)?;
                    push(SegmentKind::FreeRide, d, None, None, cadence);
                }
                "RampTest" => {
                    let warmup = if attr(&el, "Warmup").is_some() { duration(&el, "Warmup").with_context(ctx)? } else { 0 };
                    ramp = Some(warmup);
                }
                other => tracing::warn!("ignoring unsupported workout element <{other}>"),
            }
        }

        if segments.is_empty() && ramp.is_none() {
            bail!("workout has no segments");
        }
        let start = segments.last().map_or(0, Segment::end);
        let ramp = ramp.map(|warmup| RampTest { start, warmup });
        Ok(Workout { name, description, segments, ramp })
    }

    pub fn total_duration(&self) -> u32 {
        self.segments.last().map_or(0, Segment::end)
    }

    pub fn segment_index_at(&self, t: f64) -> Option<usize> {
        if t < 0.0 {
            return None;
        }
        self.segments.iter().position(|s| t < s.end() as f64)
    }

    pub fn segment_at(&self, t: f64) -> Option<&Segment> {
        self.segment_index_at(t).map(|i| &self.segments[i])
    }

    pub fn cadence_at(&self, t: f64) -> Option<u16> {
        self.segment_at(t).and_then(|s| s.cadence)
    }

    pub fn power_at(&self, t: f64) -> Option<f64> {
        self.segment_at(t).and_then(|s| s.power_at(t))
    }

    pub fn target_watts(&self, t: f64, ftp: u32, intensity: f64) -> Option<u16> {
        self.power_at(t)
            .map(|f| (f * ftp as f64 * intensity).round().clamp(0.0, 2000.0) as u16)
    }

    /// Length as listed: a ramp test lasts until the rider gives up.
    pub fn duration_label(&self) -> String {
        match self.ramp {
            Some(r) if r.steps_start() > 0 => format!("{} + ramp", fmt_duration(r.steps_start())),
            Some(_) => "ramp".into(),
            None => fmt_duration(self.total_duration()),
        }
    }

    /// Adds the ramp test's warmup and 1-minute ERG steps from `RAMP_START_W`, `RAMP_STEP_W` higher
    /// each, planned a bit past the expected end; `add_ramp_step` continues if the rider gets further.
    pub fn plan_ramp(&mut self, ftp: u32) {
        let Some(ramp) = self.ramp.filter(|r| self.total_duration() == r.start) else { return };
        let watts = |w: u32| w as f64 / ftp.max(1) as f64;
        if ramp.warmup > 0 {
            self.segments.push(Segment {
                kind: SegmentKind::Warmup,
                start: ramp.start,
                duration: ramp.warmup,
                power_start: Some(watts(RAMP_WARMUP_FROM_W)),
                power_end: Some(watts(RAMP_START_W)),
                cadence: None,
            });
        }
        let expected = ftp as f64 / RAMP_FTP_FACTOR * 1.15;
        let steps = ((expected - RAMP_START_W as f64) / RAMP_STEP_W as f64).ceil().max(5.0) as u32 + 1;
        for _ in 0..steps {
            self.add_ramp_step(ftp);
        }
    }

    pub fn add_ramp_step(&mut self, ftp: u32) {
        let Some(ramp) = self.ramp else { return };
        let start = self.total_duration();
        let n = start.saturating_sub(ramp.steps_start()) / RAMP_STEP_SECS;
        let p = (RAMP_START_W + n * RAMP_STEP_W) as f64 / ftp.max(1) as f64;
        self.segments.push(Segment { kind: SegmentKind::Steady, start, duration: RAMP_STEP_SECS, power_start: Some(p), power_end: Some(p), cadence: None });
    }

    pub fn max_power(&self) -> f64 {
        self.segments
            .iter()
            .flat_map(|s| [s.power_start, s.power_end])
            .flatten()
            .fold(0.0, f64::max)
    }
}

/// Case-insensitive attribute lookup (hand-edited .zwo files are inconsistent).
fn attr<'a>(el: &roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    el.attributes().find(|a| a.name().eq_ignore_ascii_case(name)).map(|a| a.value())
}

fn number(el: &roxmltree::Node, name: &str) -> Result<f64> {
    let raw = attr(el, name).ok_or_else(|| anyhow!("missing attribute {name}"))?;
    let v: f64 = raw.trim().parse().map_err(|_| anyhow!("attribute {name}={raw:?} is not a number"))?;
    if !v.is_finite() || v < 0.0 {
        bail!("attribute {name}={raw:?} out of range");
    }
    Ok(v)
}

fn rpm(el: &roxmltree::Node, name: &str) -> Result<Option<u16>> {
    if attr(el, name).is_none() {
        return Ok(None);
    }
    Ok(Some(number(el, name)?.round().min(250.0) as u16).filter(|&c| c > 0))
}

fn duration(el: &roxmltree::Node, name: &str) -> Result<u32> {
    Ok(number(el, name)?.round() as u32)
}

pub fn fmt_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SWEET_SPOT: &str = include_str!("../workouts/sweet_spot_3x10.zwo");

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn parses_metadata() {
        let w = Workout::parse(SWEET_SPOT).unwrap();
        assert_eq!(w.name, "Sweet Spot 3x10");
        assert!(w.description.starts_with("3 x 10 min"));
    }

    #[test]
    fn flattens_intervals() {
        let w = Workout::parse(SWEET_SPOT).unwrap();
        let kinds: Vec<_> = w.segments.iter().map(|s| s.kind).collect();
        use SegmentKind::*;
        assert_eq!(
            kinds,
            [Warmup, Steady, Steady, IntervalOn, IntervalOff, IntervalOn, IntervalOff, IntervalOn, IntervalOff, Cooldown]
        );
        // Segments are contiguous.
        for pair in w.segments.windows(2) {
            assert_eq!(pair[0].end(), pair[1].start);
        }
        // 600 + 60 + 240 + 3 * 900 + 420 = 4020 s = 67 min.
        assert_eq!(w.total_duration(), 4020);
    }

    #[test]
    fn ramps_interpolate() {
        let w = Workout::parse(SWEET_SPOT).unwrap();
        assert!(approx(w.power_at(0.0).unwrap(), 0.50));
        assert!(approx(w.power_at(300.0).unwrap(), 0.625));
        assert!(approx(w.power_at(599.0).unwrap(), 0.50 + 0.25 * 599.0 / 600.0));
        // Cooldown goes PowerLow -> PowerHigh, i.e. 0.60 -> 0.40 here.
        let cd = w.segments.last().unwrap();
        assert_eq!(cd.start, 3600);
        assert!(approx(w.power_at(3600.0).unwrap(), 0.60));
        assert!(approx(w.power_at(3810.0).unwrap(), 0.50));
    }

    #[test]
    fn steady_and_interval_power() {
        let w = Workout::parse(SWEET_SPOT).unwrap();
        assert!(approx(w.power_at(600.0).unwrap(), 0.95));
        assert!(approx(w.power_at(700.0).unwrap(), 0.55));
        assert!(approx(w.power_at(900.0).unwrap(), 0.90)); // first ON
        assert!(approx(w.power_at(1500.0).unwrap(), 0.55)); // first OFF
        assert!(approx(w.power_at(1800.0).unwrap(), 0.90)); // second ON
        assert_eq!(w.segment_index_at(1799.0), Some(4));
        assert_eq!(w.segment_index_at(1800.0), Some(5));
    }

    #[test]
    fn target_watts_uses_ftp_and_intensity() {
        let w = Workout::parse(SWEET_SPOT).unwrap();
        assert_eq!(w.target_watts(900.0, 250, 1.0), Some(225));
        assert_eq!(w.target_watts(900.0, 250, 1.05), Some(236));
        assert_eq!(w.target_watts(0.0, 200, 0.95), Some(95));
    }

    #[test]
    fn end_of_workout() {
        let w = Workout::parse(SWEET_SPOT).unwrap();
        assert!(w.segment_at(4019.0).is_some());
        assert!(w.segment_at(4020.0).is_none());
        assert_eq!(w.power_at(5000.0), None);
    }

    #[test]
    fn free_ride_and_ramp() {
        let xml = r#"<workout_file><name>x</name><workout>
            <Ramp Duration="100" PowerLow="0.4" PowerHigh="0.8"/>
            <FreeRide Duration="60" FlatRoad="1"/>
            <textevent timeoffset="10" message="hi"/>
            <SolidState Duration="30.4" power="0.7"/>
        </workout></workout_file>"#;
        let w = Workout::parse(xml).unwrap();
        assert_eq!(w.segments.len(), 3);
        assert!(approx(w.power_at(50.0).unwrap(), 0.6));
        assert_eq!(w.segments[1].kind, SegmentKind::FreeRide);
        assert_eq!(w.power_at(120.0), None);
        assert_eq!(w.segments[2].duration, 30);
        assert!(approx(w.power_at(170.0).unwrap(), 0.7));
    }

    #[test]
    fn cadence_targets() {
        let w = Workout::parse(include_str!("../workouts/spin_up_40.zwo")).unwrap();
        assert_eq!(w.total_duration(), 2400);
        assert_eq!(w.cadence_at(0.0), None);
        assert_eq!(w.cadence_at(600.0), Some(105));
        assert_eq!(w.cadence_at(660.0), Some(88));
        assert_eq!(w.cadence_at(1500.0), None);
        assert!(Workout::parse(r#"<workout_file><workout><SteadyState Duration="60" Power="0.5" Cadence="x"/></workout></workout_file>"#).is_err());
    }

    #[test]
    fn ramp_test_steps() {
        let mut w = Workout::parse(include_str!("../workouts/ramp_test.zwo")).unwrap();
        assert_eq!((w.total_duration(), w.duration_label()), (0, "05:00 + ramp".to_string()));
        w.plan_ramp(250);
        w.plan_ramp(250);
        // 250 / 0.75 * 1.15 = 383 W: 15 steps past 100 W, plus the first one.
        assert_eq!(w.total_duration(), 300 + 16 * 60);
        assert_eq!(w.target_watts(0.0, 250, 1.0), Some(60));
        assert_eq!(w.target_watts(150.0, 250, 1.0), Some(80));
        assert_eq!(w.target_watts(300.0, 250, 1.0), Some(100));
        assert_eq!(w.target_watts(419.0, 250, 1.0), Some(120));
        w.add_ramp_step(250);
        assert_eq!(w.target_watts(300.0 + 16.0 * 60.0, 250, 1.0), Some(420));
    }

    #[test]
    fn ramp_test_after_other_segments() {
        let xml = r#"<workout_file><workout>
            <SteadyState Duration="600" Power="0.5"/>
            <RampTest/>
        </workout></workout_file>"#;
        let mut w = Workout::parse(xml).unwrap();
        assert_eq!(w.duration_label(), "10:00 + ramp");
        w.plan_ramp(200);
        assert_eq!(w.target_watts(600.0, 200, 1.0), Some(100));
        let after = r#"<workout_file><workout><RampTest/><SteadyState Duration="60" Power="0.5"/></workout></workout_file>"#;
        assert!(Workout::parse(after).is_err());
    }

    #[test]
    fn errors_are_descriptive() {
        let err = Workout::parse(r#"<workout_file><workout><SteadyState Duration="60"/></workout></workout_file>"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Power"), "{msg}");
        assert!(Workout::parse("<foo/>").is_err());
        assert!(Workout::parse("<workout_file><workout/></workout_file>").is_err());
    }
}
