//! One recorded ride: stats and charts of power, heart rate, cadence and speed.

use anyhow::Result;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::config::HrZones;
use crate::history::{self, RideFile, RideStats};
use crate::recorder::read_journal;
use crate::session::Sample;
use crate::ui::{HINT, LABEL, hr_zone_color, panel, zone_color};
use crate::workout::fmt_duration;

const EIGHTHS: [&str; 9] = ["", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Lines shorter than this many rows aren't worth a chart.
const MIN_CHART_ROWS: u16 = 6;

pub struct RideView {
    pub ride: RideFile,
    samples: Vec<Sample>,
    stats: RideStats,
    ftp: Option<u32>,
    hr_zones: Option<HrZones>,
}

impl RideView {
    pub fn load(ride: &RideFile, ftp: Option<u32>, hr_zones: Option<HrZones>) -> Result<Self> {
        let (_, samples) = read_journal(&ride.csv)?;
        let stats = history::stats(&samples);
        let ftp = ride.meta.as_ref().map(|m| m.ftp).or(ftp);
        Ok(Self { ride: ride.clone(), samples, stats, ftp, hr_zones })
    }

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let s = &self.stats;
        let mut parts = vec![fmt_duration(s.duration), format!("{:.2} km", s.distance_m / 1000.0)];
        if s.duration > 0 {
            parts.push(format!("{:.1} km/h", s.distance_m / s.duration as f64 * 3.6));
        }
        parts.extend(s.avg_power.map(|p| format!("avg {p} W")));
        if let Some(np) = s.np {
            parts.push(format!("NP {np} W"));
            if let Some(ftp) = self.ftp.filter(|&f| f > 0) {
                let intensity = np as f64 / ftp as f64;
                let tss = s.duration as f64 * np as f64 * intensity / (ftp as f64 * 3600.0) * 100.0;
                parts.push(format!("IF {intensity:.2}"));
                parts.push(format!("TSS {tss:.0}"));
            }
        }
        if let (Some(avg), Some(max)) = (s.avg_hr, s.max_hr) {
            parts.push(format!("HR {avg}/{max}"));
        }
        let cadence: Vec<f64> = self.samples.iter().filter_map(|s| s.cadence).filter(|&c| c > 0).map(f64::from).collect();
        if !cadence.is_empty() {
            parts.push(format!("{:.0} rpm", cadence.iter().sum::<f64>() / cadence.len() as f64));
        }
        parts.push(format!("{:.0} kJ", s.kj));
        let width = area.width.saturating_sub(2) as usize;
        let header_lines = crate::calendar::join_wrapped(&parts, "  ·  ", width);
        let mut title = format!("{} · {}", self.ride.started.format("%a %-d %b %H:%M"), self.ride.name());
        if self.ride.meta.as_ref().is_some_and(|m| m.strava_id.is_some()) {
            title += " · on Strava";
        }

        let charts = self.charts();
        let header_h = header_lines.len() as u16 + 2;
        let room = area.height.saturating_sub(header_h);
        let fit = (room / MIN_CHART_ROWS).max(1) as usize;
        let charts = &charts[..charts.len().min(fit)];

        let mut rows = vec![Constraint::Length(header_h)];
        rows.extend(charts.iter().enumerate().map(|(i, _)| Constraint::Fill(if i == 0 { 2 } else { 1 })));
        let areas = Layout::vertical(rows).split(area);
        let lines: Vec<Line> = header_lines.into_iter().map(|l| Line::styled(l, LABEL)).collect();
        f.render_widget(Paragraph::new(lines).block(panel(&title)), areas[0]);
        for (chart, &a) in charts.iter().zip(&areas[1..]) {
            f.render_widget(chart, a);
        }
    }

    /// Power first; the rest only when the sensor recorded something.
    fn charts(&self) -> Vec<Series> {
        let ftp = self.ftp.unwrap_or(0) as f64;
        let mut charts = vec![];
        let power: Vec<Option<f64>> = self.samples.iter().map(|s| s.power.map(f64::from)).collect();
        let target: Vec<Option<f64>> = self.samples.iter().map(|s| s.target.map(f64::from)).collect();
        let peak = power.iter().chain(&target).flatten().copied().fold(0.0, f64::max);
        charts.push(Series {
            title: summary("POWER", &power, "W"),
            values: power,
            target: Some(target),
            min: 0.0,
            max: peak.max(1.0) * 1.1,
            color: Box::new(move |w| if ftp > 0.0 { zone_color(w / ftp) } else { Color::Yellow }),
        });

        let hr: Vec<Option<f64>> = self.samples.iter().map(|s| s.hr.filter(|&h| h > 0).map(f64::from)).collect();
        if hr.iter().any(Option::is_some) {
            let (lo, hi) = (percentile(&hr, 0.02), percentile(&hr, 1.0));
            let zones = self.hr_zones;
            charts.push(Series {
                title: summary("HEART RATE", &hr, "bpm"),
                values: hr,
                target: None,
                min: (lo - 5.0).max(0.0),
                max: hi + 2.0,
                color: Box::new(move |b| zones.map_or(Color::LightRed, |z| hr_zone_color(z.zone(b.round() as u16)))),
            });
        }

        let cadence: Vec<Option<f64>> = self.samples.iter().map(|s| s.cadence.map(f64::from)).collect();
        if cadence.iter().flatten().any(|&c| c > 0.0) {
            let (lo, hi) = (percentile(&cadence, 0.05), percentile(&cadence, 0.99));
            charts.push(Series {
                title: summary("CADENCE", &cadence, "rpm"),
                values: cadence,
                target: None,
                min: (lo - 15.0).max(0.0),
                max: hi * 1.1,
                color: Box::new(|_| Color::LightCyan),
            });
        }

        let speed: Vec<Option<f64>> = self.samples.iter().map(|s| s.speed_kmh).collect();
        if speed.iter().flatten().any(|&v| v > 0.0) {
            let (lo, hi) = (percentile(&speed, 0.05), percentile(&speed, 0.99));
            charts.push(Series {
                title: summary("SPEED", &speed, "km/h"),
                values: speed,
                target: None,
                min: lo * 0.8,
                max: hi * 1.05,
                color: Box::new(|_| Color::White),
            });
        }
        charts
    }
}

/// Value below which `q` of the recorded values fall, so a stray reading doesn't squash the chart.
fn percentile(values: &[Option<f64>], q: f64) -> f64 {
    let mut vals: Vec<f64> = values.iter().flatten().copied().collect();
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(f64::total_cmp);
    vals[((vals.len() - 1) as f64 * q).round() as usize]
}

fn summary(name: &str, values: &[Option<f64>], unit: &str) -> String {
    let vals: Vec<f64> = values.iter().flatten().copied().collect();
    if vals.is_empty() {
        return name.to_string();
    }
    let avg = vals.iter().sum::<f64>() / vals.len() as f64;
    let max = vals.iter().copied().fold(0.0, f64::max);
    format!("{name} · avg {avg:.0} · max {max:.0} {unit}")
}

/// Means of `values` over `width` equal slices.
fn columns(values: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    let n = values.len();
    (0..width)
        .map(|x| {
            let (a, b) = (x * n / width, ((x + 1) * n / width).max(x * n / width + 1).min(n));
            let vals: Vec<f64> = values.get(a..b).unwrap_or(&[]).iter().flatten().copied().collect();
            (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64)
        })
        .collect()
}

/// Bar chart of one value over the ride, optionally over a dimmed target profile.
struct Series {
    title: String,
    values: Vec<Option<f64>>,
    target: Option<Vec<Option<f64>>>,
    min: f64,
    max: f64,
    color: Box<dyn Fn(f64) -> Color>,
}

impl Widget for &Series {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let n = self.values.len() as u32;
        let block = panel(&self.title)
            .title_bottom(Line::styled(" 00:00 ", HINT).left_aligned())
            .title_bottom(Line::styled(format!(" {} ", fmt_duration(n)), HINT).right_aligned());
        let inner = block.inner(area);
        block.render(area, buf);
        bars(buf, inner, &self.values, self.target.as_deref(), (self.min, self.max), &self.color);
    }
}

/// Column chart of `values` filling `area`, over a dimmed `target` profile.
fn bars(buf: &mut Buffer, area: Rect, values: &[Option<f64>], target: Option<&[Option<f64>]>, (min, max): (f64, f64), color: &dyn Fn(f64) -> Color) {
    if area.width == 0 || area.height == 0 || values.is_empty() {
        return;
    }
    let h = area.height as f64;
    let span = (max - min).max(1.0);
    let level = |v: f64| (((v - min) / span) * h * 8.0).round().clamp(0.0, h * 8.0) as usize;
    let actual = columns(values, area.width as usize);
    let target = target.map(|t| columns(t, area.width as usize));
    for x in 0..area.width {
        let cx = area.x + x;
        if let Some(t) = target.as_ref().and_then(|t| t[x as usize]) {
            let style = Style::new().fg(color(t)).add_modifier(Modifier::DIM);
            for row in 0..(level(t) / 8) as u16 {
                buf[(cx, area.bottom() - 1 - row)].set_symbol("░").set_style(style);
            }
        }
        let Some(v) = actual[x as usize] else { continue };
        let filled = level(v).max(1);
        let style = Style::new().fg(color(v));
        for row in 0..area.height {
            let part = filled.saturating_sub(row as usize * 8).min(8);
            if part == 0 {
                break;
            }
            buf[(cx, area.bottom() - 1 - row)].set_symbol(EIGHTHS[part]).set_style(style);
        }
    }
}

/// Power over the ride for lists, on top of the workout target, colored by zone.
pub fn power_chart(buf: &mut Buffer, area: Rect, power: &[Option<u16>], target: &[Option<u16>], ftp: Option<u32>) {
    let to_f64 = |v: &[Option<u16>]| -> Vec<Option<f64>> { v.iter().map(|p| p.map(f64::from)).collect() };
    let (power, target) = (to_f64(power), to_f64(target));
    let peak = power.iter().chain(&target).flatten().copied().fold(1.0, f64::max);
    let ftp = ftp.unwrap_or(0) as f64;
    let color = move |w: f64| if ftp > 0.0 { zone_color(w / ftp) } else { Color::Yellow };
    bars(buf, area, &power, Some(&target), (0.0, peak * 1.05), &color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_average_slices_and_stretch_short_rides() {
        let v: Vec<Option<f64>> = [100.0, 200.0, 300.0, 400.0].map(Some).to_vec();
        assert_eq!(columns(&v, 2), [Some(150.0), Some(350.0)]);
        assert_eq!(columns(&v, 8).len(), 8);
        assert_eq!(columns(&v, 8)[7], Some(400.0));
        assert_eq!(columns(&[None, None], 1), [None]);
    }

    #[test]
    fn percentile_ignores_outliers() {
        let mut v: Vec<Option<f64>> = (0..100).map(|i| Some(80.0 + (i % 10) as f64)).collect();
        v.push(Some(156.0));
        v.push(None);
        assert_eq!(percentile(&v, 0.99), 89.0);
        assert_eq!(percentile(&v, 1.0), 156.0);
        assert_eq!(percentile(&[None], 0.5), 0.0);
    }
}
