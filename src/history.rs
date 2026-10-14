//! Recorded rides on disk.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::NaiveDateTime;

use crate::recorder::{RideMeta, load_meta};
use crate::session::Sample;

/// An unfinished ride older than this isn't offered for resuming.
const RESUME_WITHIN: Duration = Duration::from_secs(3 * 3600);

#[derive(Debug, Clone)]
pub struct RideFile {
    pub csv: PathBuf,
    pub started: NaiveDateTime,
    pub meta: Option<RideMeta>,
    pub modified: SystemTime,
}

impl RideFile {
    pub fn name(&self) -> String {
        match &self.meta {
            Some(m) => m.workout.clone(),
            None => {
                let stem = self.csv.file_stem().unwrap_or_default().to_string_lossy();
                stem.get(18..).unwrap_or_default().replace('-', " ")
            }
        }
    }

    /// Unfinished and recent enough to pick up again.
    pub fn resumable(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| !m.finished && m.workout_path.is_some())
            && self.modified.elapsed().is_ok_and(|age| age < RESUME_WITHIN)
    }
}

/// Rides in `dir` (not its subfolders), oldest first.
pub fn list(dir: &Path) -> Vec<RideFile> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut rides: Vec<RideFile> = entries
        .flatten()
        .filter_map(|e| {
            let csv = e.path();
            if csv.extension()? != "csv" {
                return None;
            }
            let stem = csv.file_stem()?.to_str()?;
            let started = NaiveDateTime::parse_from_str(stem.get(..17)?, "%Y-%m-%d_%H%M%S").ok()?;
            let modified = e.metadata().ok()?.modified().ok()?;
            Some(RideFile { meta: load_meta(&csv), csv, started, modified })
        })
        .collect();
    rides.sort_by_key(|r| r.started);
    rides
}

#[derive(Debug, Clone, PartialEq)]
pub struct RideStats {
    /// Riding time, seconds.
    pub duration: u32,
    pub distance_m: f64,
    pub avg_power: Option<u16>,
    /// Normalized power: 4th-power mean of the 30 s rolling average.
    pub np: Option<u16>,
    pub avg_hr: Option<u16>,
    pub max_hr: Option<u16>,
    pub kj: f64,
}

pub fn stats(samples: &[Sample]) -> RideStats {
    let powers: Vec<f64> = samples.iter().map(|s| s.power.unwrap_or(0) as f64).collect();
    let measured: Vec<u16> = samples.iter().filter_map(|s| s.power).collect();
    let avg_power = (!measured.is_empty())
        .then(|| (measured.iter().map(|&p| p as f64).sum::<f64>() / measured.len() as f64).round() as u16);
    let np = (powers.len() >= 30).then(|| {
        let rolling: Vec<f64> = powers.windows(30).map(|w| w.iter().sum::<f64>() / 30.0).collect();
        (rolling.iter().map(|p| p.powi(4)).sum::<f64>() / rolling.len() as f64).powf(0.25).round() as u16
    });
    let hrs: Vec<u16> = samples.iter().filter_map(|s| s.hr).filter(|&h| h > 0).collect();
    RideStats {
        duration: samples.len() as u32,
        distance_m: samples.last().map_or(0.0, |s| s.distance_m),
        avg_power,
        np,
        avg_hr: (!hrs.is_empty()).then(|| (hrs.iter().map(|&h| h as u32).sum::<u32>() / hrs.len() as u32) as u16),
        max_hr: hrs.iter().max().copied(),
        kj: powers.iter().sum::<f64>() / 1000.0,
    }
}

pub fn closed(meta: &RideMeta) -> RideMeta {
    RideMeta { closed: true, ..meta.clone() }
}

/// The latest ride, if it was interrupted and not declined yet.
pub fn resume_offer(rides: &[RideFile]) -> Option<&RideFile> {
    rides.last().filter(|r| r.resumable() && r.meta.as_ref().is_some_and(|m| !m.closed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::{Recorder, save_meta};

    fn sample(power: Option<u16>, hr: Option<u16>) -> Sample {
        Sample { clock: 0, power, target: None, hr, cadence: None, speed_kmh: None, distance_m: 10.0, pos: 0 }
    }

    #[test]
    fn ride_stats() {
        let steady: Vec<Sample> = (0..60).map(|_| sample(Some(200), Some(140))).collect();
        let s = stats(&steady);
        assert_eq!((s.duration, s.avg_power, s.np, s.avg_hr, s.max_hr), (60, Some(200), Some(200), Some(140), Some(140)));
        assert_eq!(s.kj, 12.0);

        // Surges raise NP above the average.
        let surges: Vec<Sample> = (0..120).map(|i| sample(Some(if i % 60 < 30 { 300 } else { 100 }), None)).collect();
        let s = stats(&surges);
        assert_eq!(s.avg_power, Some(200));
        assert!(s.np.unwrap() > 210, "{:?}", s.np);
        assert_eq!((s.avg_hr, s.max_hr), (None, None));
        assert_eq!(stats(&steady[..10]).np, None);
    }

    #[test]
    fn latest_unfinished_ride_is_offered() {
        let dir = std::env::temp_dir().join(format!("watty-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let meta = RideMeta {
            workout: "Z2 45".into(),
            workout_path: Some("z2_45.zwo".into()),
            ftp: 250,
            intensity: 1.0,
            finished: false,
            closed: false,
        };
        let r = Recorder::create(&dir, meta.clone()).unwrap();
        let csv = r.csv_path().to_path_buf();
        r.finish().unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();

        let rides = list(&dir);
        assert_eq!(rides.len(), 1);
        assert_eq!(rides[0].name(), "Z2 45");
        assert!(resume_offer(&rides).is_some());

        save_meta(&csv, &RideMeta { closed: true, ..meta.clone() }).unwrap();
        assert!(resume_offer(&list(&dir)).is_none());
        assert!(list(&dir)[0].resumable());
        save_meta(&csv, &RideMeta { finished: true, ..meta }).unwrap();
        assert!(!list(&dir)[0].resumable());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
