use std::path::PathBuf;

use anyhow::{Context, Result};
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

impl Config {
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
