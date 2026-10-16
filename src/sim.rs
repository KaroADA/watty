use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::{interval, sleep};

use crate::ble::EventTx;
use crate::devices::{DeviceEvent, LinkStatus, Role, TrainerTarget};
use crate::protocol::{HeartRate, IndoorBikeData};

#[derive(Clone, Copy)]
pub struct SimOptions {
    /// Simulated seconds per real second.
    pub speed: f64,
    /// Drop the link for a few seconds every couple of minutes.
    pub dropouts: bool,
}

struct Noise(u64);

impl Noise {
    /// Uniform in [-1, 1).
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

fn link(tx: &EventTx, role: Role, status: LinkStatus) {
    let name = (status == LinkStatus::Connected).then(|| format!("SIM {}", role.label()));
    let _ = tx.send(DeviceEvent::Link { role, status, name });
}

fn step(opts: SimOptions) -> Duration {
    Duration::from_secs_f64(1.0 / opts.speed.max(0.1))
}

/// Simulated seconds between dropouts, and dropout length.
const DROPOUT_EVERY: u32 = 150;
const DROPOUT_LEN: u32 = 6;
/// Above this ERG target the simulated rider fades, which ends a ramp test.
const MAX_W: f64 = 400.0;

/// Returns a flag that makes the simulated rider stop pedaling while set.
pub fn spawn(tx: EventTx, target: watch::Receiver<TrainerTarget>, opts: SimOptions) -> Arc<AtomicBool> {
    let power = Arc::new(AtomicU32::new(0));
    let coasting = Arc::new(AtomicBool::new(false));
    tokio::spawn(trainer(tx.clone(), target, opts, power.clone(), coasting.clone()));
    tokio::spawn(hr(tx, opts, power));
    coasting
}

async fn trainer(
    tx: EventTx,
    target: watch::Receiver<TrainerTarget>,
    opts: SimOptions,
    shared: Arc<AtomicU32>,
    coasting: Arc<AtomicBool>,
) {
    link(&tx, Role::Trainer, LinkStatus::Connecting);
    sleep(Duration::from_millis(800)).await;
    link(&tx, Role::Trainer, LinkStatus::Connected);

    let mut noise = Noise(42);
    let mut power = 0.0;
    let mut distance = 0.0;
    let mut applied = TrainerTarget::Idle;
    let mut tick = interval(step(opts));
    for t in 0u32.. {
        tick.tick().await;
        if opts.dropouts && t > 0 && t % DROPOUT_EVERY < DROPOUT_LEN {
            if t % DROPOUT_EVERY == 0 {
                link(&tx, Role::Trainer, LinkStatus::Disconnected("simulated dropout".into()));
            }
            continue;
        }
        if opts.dropouts && t > DROPOUT_EVERY && t % DROPOUT_EVERY == DROPOUT_LEN {
            link(&tx, Role::Trainer, LinkStatus::Connected);
        }

        let want = *target.borrow();
        if want != applied && want != TrainerTarget::Idle {
            applied = want;
            let _ = tx.send(DeviceEvent::TargetApplied(want));
        }
        let coast = coasting.load(Ordering::Relaxed);
        let fading = matches!(applied, TrainerTarget::Erg(w) if w as f64 > MAX_W);
        let goal = match applied {
            _ if coast => 0.0,
            _ if fading => MAX_W * 0.6,
            TrainerTarget::Erg(w) => w as f64,
            _ => 160.0,
        };
        power += (goal - power) * 0.35;
        let p = if coast { 0.0 } else { (power + noise.next() * 6.0).max(0.0) };
        shared.store(p as u32, Ordering::Relaxed);

        let speed_ms = (p / 0.28).cbrt();
        distance += speed_ms;
        let _ = tx.send(DeviceEvent::Bike(IndoorBikeData {
            speed_kmh: Some(speed_ms * 3.6),
            cadence_rpm: (!coast).then(|| (if fading { 45.0 } else { 90.0 } + noise.next() * 3.0).round()),
            power_w: Some(p.round() as i16),
            distance_m: Some(distance as u32),
            heart_rate: None,
        }));
    }
}

async fn hr(tx: EventTx, opts: SimOptions, power: Arc<AtomicU32>) {
    link(&tx, Role::Hr, LinkStatus::Connecting);
    sleep(Duration::from_millis(1500)).await;
    link(&tx, Role::Hr, LinkStatus::Connected);

    let mut noise = Noise(7);
    let mut bpm = 70.0;
    let mut tick = interval(step(opts));
    loop {
        tick.tick().await;
        let goal = 65.0 + power.load(Ordering::Relaxed) as f64 * 0.42;
        bpm += (goal - bpm) * 0.03;
        let _ = tx.send(DeviceEvent::Hr(HeartRate {
            bpm: (bpm + noise.next()).round() as u16,
            contact: Some(true),
            rr_ms: vec![],
        }));
    }
}
