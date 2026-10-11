//! Shared by the BLE and simulated backends.

use crate::protocol::{HeartRate, IndoorBikeData};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Trainer,
    Hr,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Trainer => "Trainer",
            Role::Hr => "HR",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkStatus {
    Searching,
    Connecting,
    Connected,
    /// Lost or failed; the device task keeps retrying.
    Disconnected(String),
}

/// Latest wish via `watch`; the trainer task decides when to transmit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrainerTarget {
    Idle,
    Erg(u16),
    /// Flat-road simulation instead of ERG.
    FreeRide,
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Link { role: Role, status: LinkStatus, name: Option<String> },
    Bike(IndoorBikeData),
    Hr(HeartRate),
    TargetApplied(TrainerTarget),
    Notice(String),
}
