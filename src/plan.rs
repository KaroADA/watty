//! `~/.local/share/watty/plan.toml`: workouts scheduled for given days, editable by hand.
//!
//! ```toml
//! [[workout]]
//! date = 2026-10-16
//! file = "sweet_spot_3x10.zwo"   # relative to the workouts folder, or absolute
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::history::RideFile;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Planned {
    #[serde(with = "toml_date")]
    pub date: NaiveDate,
    pub file: PathBuf,
}

impl Planned {
    pub fn path(&self, workouts_dir: &Path) -> PathBuf {
        workouts_dir.join(&self.file)
    }

    /// Ridden that day, judged by the workout's file name.
    pub fn done(&self, rides: &[RideFile]) -> bool {
        rides.iter().any(|r| {
            r.started.date() == self.date
                && r.meta.as_ref().and_then(|m| m.workout_path.as_ref()).and_then(|p| p.file_name()) == self.file.file_name()
        })
    }
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default, rename = "workout")]
    pub workouts: Vec<Planned>,
}

pub fn plan_path() -> PathBuf {
    config::data_dir().join("plan.toml")
}

impl Plan {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?).with_context(|| format!("cannot write {}", path.display()))
    }

    /// Indexes into `workouts` for one day.
    pub fn on(&self, date: NaiveDate) -> Vec<usize> {
        (0..self.workouts.len()).filter(|&i| self.workouts[i].date == date).collect()
    }

    /// Keeps the path relative when the workout is in the workouts folder.
    pub fn add(&mut self, date: NaiveDate, workout: &Path, workouts_dir: &Path) {
        let file = workout.strip_prefix(workouts_dir).unwrap_or(workout).to_path_buf();
        self.workouts.push(Planned { date, file });
        self.workouts.sort_by_key(|p| p.date);
    }
}

/// Dates as TOML dates (`2026-10-16`), also accepting strings when edited by hand.
mod toml_date {
    use std::str::FromStr;

    use chrono::NaiveDate;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
    use toml::value::Datetime;

    pub fn serialize<S: Serializer>(date: &NaiveDate, s: S) -> Result<S::Ok, S::Error> {
        Datetime::from_str(&date.to_string()).map_err(serde::ser::Error::custom)?.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveDate, D::Error> {
        let text = match toml::Value::deserialize(d)? {
            toml::Value::String(s) => s,
            toml::Value::Datetime(dt) => dt.to_string(),
            v => return Err(D::Error::custom(format!("expected a date, got {v}"))),
        };
        NaiveDate::from_str(text.trim()).map_err(|e| D::Error::custom(format!("bad date {text:?}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::RideMeta;

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn hand_written_plan_roundtrips() {
        let text = r#"
            [[workout]]
            date = 2026-10-16
            file = "sweet_spot_3x10.zwo"

            [[workout]]
            date = "2026-10-14"
            file = "/elsewhere/z2.zwo"
        "#;
        let mut plan: Plan = toml::from_str(text).unwrap();
        assert_eq!(plan.on(day(16)), [0]);
        assert_eq!(plan.workouts[1].path(Path::new("/w")), Path::new("/elsewhere/z2.zwo"));
        assert_eq!(plan.workouts[0].path(Path::new("/w")), Path::new("/w/sweet_spot_3x10.zwo"));

        plan.add(day(15), Path::new("/w/spin_up_40.zwo"), Path::new("/w"));
        assert_eq!(plan.workouts[1], Planned { date: day(15), file: "spin_up_40.zwo".into() });
        let saved = toml::to_string_pretty(&plan).unwrap();
        assert!(saved.contains("date = 2026-10-15"), "{saved}");
        assert_eq!(toml::from_str::<Plan>(&saved).unwrap(), plan);
    }

    #[test]
    fn done_when_ridden_that_day() {
        let ride = |d: u32, path: &str| RideFile {
            csv: "x.csv".into(),
            started: day(d).and_hms_opt(18, 0, 0).unwrap(),
            meta: Some(RideMeta {
                workout: "x".into(),
                workout_path: Some(path.into()),
                ftp: 250,
                intensity: 1.0,
                finished: true,
                closed: false,
            }),
            modified: std::time::SystemTime::now(),
        };
        let p = Planned { date: day(14), file: "z2.zwo".into() };
        assert!(p.done(&[ride(14, "/w/z2.zwo")]));
        assert!(!p.done(&[ride(13, "/w/z2.zwo"), ride(14, "/w/other.zwo")]));
    }
}
