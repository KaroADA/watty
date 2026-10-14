use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// `~/.config/watty/config.toml`. Every field is optional; CLI flags win.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub ftp: Option<u32>,
    /// Rider weight, kg (virtual speed).
    pub weight: Option<f64>,
    /// Bluetooth address of the FTMS trainer, e.g. "AA:BB:CC:DD:EE:FF".
    pub trainer: Option<String>,
    /// Bluetooth address of the heart-rate sensor.
    pub hr: Option<String>,
    /// Acceptable deviation from a workout's target cadence, ± rpm.
    #[serde(default = "default_cadence_tolerance")]
    pub cadence_tolerance: u16,
    /// Pause when pedaling stops and resume when it starts again.
    #[serde(default = "default_true")]
    pub auto_pause: bool,
    /// Heart-rate zones default to 60/70/80/90% of this.
    pub max_hr: Option<u16>,
    /// Upper bounds of zones 1–4, bpm; overrides `max_hr`.
    pub hr_zones: Option<Vec<u16>>,
    /// Seconds before a segment change to announce it; 0 turns it off.
    #[serde(default = "default_segment_warning")]
    pub segment_warning: u32,
    /// Keep the screen from dimming or locking during a ride.
    #[serde(default = "default_true")]
    pub keep_awake: bool,
    /// Lower ERG while paused and ramp it back up after resuming.
    #[serde(default = "default_true")]
    pub soft_start: bool,
    /// Where `.zwo` files are listed from; defaults to `~/.local/share/watty/workouts`.
    pub workouts_dir: Option<PathBuf>,
}

fn default_segment_warning() -> u32 {
    10
}

fn default_cadence_tolerance() -> u16 {
    5
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ftp: None,
            weight: None,
            trainer: None,
            hr: None,
            cadence_tolerance: default_cadence_tolerance(),
            auto_pause: true,
            max_hr: None,
            hr_zones: None,
            segment_warning: default_segment_warning(),
            keep_awake: true,
            soft_start: true,
            workouts_dir: None,
        }
    }
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback))
        .join("watty")
}

pub fn config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("config.toml")
}

/// Rides and logs live here.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

pub fn rides_dir() -> PathBuf {
    data_dir().join("rides")
}

impl Config {
    pub fn workouts_dir(&self) -> PathBuf {
        self.workouts_dir.clone().unwrap_or_else(|| data_dir().join("workouts"))
    }

    pub fn hr_zones(&self) -> Result<Option<HrZones>> {
        if let Some(z) = &self.hr_zones {
            let Ok(bounds) = <[u16; 4]>::try_from(z.as_slice()) else {
                bail!("hr_zones needs 4 values (upper bounds of zones 1–4), got {}", z.len());
            };
            if !bounds.is_sorted_by(|a, b| a < b) {
                bail!("hr_zones must be increasing: {z:?}");
            }
            return Ok(Some(HrZones(bounds)));
        }
        Ok(self.max_hr.map(|max| HrZones([60, 70, 80, 90].map(|pct| (max as u32 * pct / 100) as u16))))
    }

    pub fn load() -> Result<Self> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

/// Upper bounds of heart-rate zones 1–4; zone 5 is above.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HrZones(pub [u16; 4]);

impl HrZones {
    /// Zone 1–5.
    pub fn zone(&self, bpm: u16) -> u8 {
        self.0.iter().take_while(|&&b| bpm > b).count() as u8 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_from_max_hr() {
        let cfg = Config { max_hr: Some(190), ..Config::default() };
        let z = cfg.hr_zones().unwrap().unwrap();
        assert_eq!(z, HrZones([114, 133, 152, 171]));
        assert_eq!([z.zone(90), z.zone(114), z.zone(115), z.zone(152), z.zone(171), z.zone(185)], [1, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn explicit_zones_win_and_are_validated() {
        let cfg = Config { max_hr: Some(190), hr_zones: Some(vec![120, 140, 155, 170]), ..Config::default() };
        assert_eq!(cfg.hr_zones().unwrap(), Some(HrZones([120, 140, 155, 170])));
        let cfg = Config { hr_zones: Some(vec![120, 140, 155]), ..Config::default() };
        assert!(cfg.hr_zones().is_err());
        let cfg = Config { hr_zones: Some(vec![120, 160, 155, 170]), ..Config::default() };
        assert!(cfg.hr_zones().is_err());
        assert_eq!(Config::default().hr_zones().unwrap(), None);
    }
}
