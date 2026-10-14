//! Recorded rides on disk.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::NaiveDateTime;

use crate::recorder::{RideMeta, load_meta};

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
