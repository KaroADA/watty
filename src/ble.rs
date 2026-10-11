//! BLE via btleplug/BlueZ. Each sensor task loops connect → session → retry.
//! BlueZ never ends the notification stream on disconnect, hence the
//! `is_connected()` polling and the no-data watchdog.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::StreamExt;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::{interval, sleep, timeout};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::devices::{DeviceEvent, LinkStatus, Role, TrainerTarget};
use crate::protocol::{self as proto, hex, op};

pub type EventTx = mpsc::UnboundedSender<DeviceEvent>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(4);
/// Minimum spacing between resistance commands (ramps change ~1 W/s).
const TARGET_MIN_INTERVAL: Duration = Duration::from_secs(1);
const NO_DATA_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct Ble {
    adapter: Adapter,
    /// Number of active scan users; discovery runs while > 0.
    scan_users: Arc<Mutex<usize>>,
}

#[derive(Debug, Clone)]
pub struct Discovered {
    pub address: String,
    pub name: String,
    pub rssi: Option<i16>,
    pub ftms: bool,
    pub hr: bool,
}

/// Keeps discovery running while alive. Shared between device tasks.
pub struct ScanGuard {
    ble: Ble,
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        let ble = self.ble.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { ble.release_scan().await });
        }
    }
}

impl Ble {
    pub async fn new() -> Result<Self> {
        let manager = Manager::new()
            .await
            .context("cannot reach BlueZ over D-Bus (is bluetooth.service running?)")?;
        let adapter = manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Bluetooth adapter found (try `bluetoothctl power on`)"))?;
        info!("using adapter {}", adapter.adapter_info().await.unwrap_or_default());
        Ok(Self { adapter, scan_users: Arc::new(Mutex::new(0)) })
    }

    pub async fn scan(&self) -> Result<ScanGuard> {
        let mut users = self.scan_users.lock().await;
        if *users == 0 {
            // Unfiltered: some devices list services only in the scan response.
            self.adapter
                .start_scan(ScanFilter::default())
                .await
                .context("cannot start BLE scan (adapter powered? `bluetoothctl power on`)")?;
            debug!("scan started");
        }
        *users += 1;
        Ok(ScanGuard { ble: self.clone() })
    }

    async fn release_scan(&self) {
        let mut users = self.scan_users.lock().await;
        *users = users.saturating_sub(1);
        if *users == 0 {
            if let Err(e) = self.adapter.stop_scan().await {
                debug!("stop_scan: {e}");
            }
            debug!("scan stopped");
        }
    }

    /// Scans until every wanted role has a candidate in range (or `max`).
    /// A role is resolved only when exactly one candidate is visible.
    pub async fn autodetect(&self, want_trainer: bool, want_hr: bool, max: Duration) -> Result<(Option<String>, Option<String>)> {
        let _scan = self.scan().await?;
        let deadline = Instant::now() + max;
        loop {
            // rssi == None: stale BlueZ cache entry, not actually in range.
            let found: Vec<Discovered> = self.discovered().await?.into_iter().filter(|d| d.rssi.is_some()).collect();
            let trainers: Vec<&Discovered> = found.iter().filter(|d| d.ftms).collect();
            let hrs: Vec<&Discovered> = found.iter().filter(|d| d.hr && !d.ftms).collect();
            let done = (!want_trainer || !trainers.is_empty()) && (!want_hr || !hrs.is_empty());
            if done || Instant::now() >= deadline {
                let unique = |v: &[&Discovered]| (v.len() == 1).then(|| v[0].address.clone());
                return Ok((unique(&trainers).filter(|_| want_trainer), unique(&hrs).filter(|_| want_hr)));
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    /// Every device BlueZ knows about, FTMS/HR candidates first.
    pub async fn discovered(&self) -> Result<Vec<Discovered>> {
        let mut out = Vec::new();
        for p in self.adapter.peripherals().await? {
            let Ok(Some(props)) = p.properties().await else { continue };
            let ftms = props.services.contains(&proto::FTMS_SERVICE);
            let hr = props.services.contains(&proto::HR_SERVICE);
            let name = props.local_name.or(props.advertisement_name).unwrap_or_default();
            if name.is_empty() && !ftms && !hr {
                continue;
            }
            out.push(Discovered { address: p.address().to_string(), name, rssi: props.rssi, ftms, hr });
        }
        out.sort_by_key(|d| (!(d.ftms || d.hr), d.rssi.is_none(), -(d.rssi.unwrap_or(0) as i32)));
        Ok(out)
    }

    pub async fn disconnect(&self, address: &str) {
        let Ok(list) = self.adapter.peripherals().await else { return };
        for p in list.into_iter().filter(|p| p.address().to_string().eq_ignore_ascii_case(address)) {
            let _ = timeout(Duration::from_secs(3), p.disconnect()).await;
        }
    }

    async fn find(&self, address: &str) -> Result<Peripheral> {
        let mut _scan = None;
        loop {
            for p in self.adapter.peripherals().await? {
                if p.address().to_string().eq_ignore_ascii_case(address) {
                    return Ok(p);
                }
            }
            if _scan.is_none() {
                _scan = Some(self.scan().await?);
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    /// After a failed connect: wait until the device actually advertises.
    async fn wait_for_advert(&self, id: &PeripheralId, max: Duration) -> Result<()> {
        let mut events = self.adapter.events().await?;
        let _scan = self.scan().await?;
        let _ = timeout(max, async {
            while let Some(ev) = events.next().await {
                let seen = match &ev {
                    CentralEvent::DeviceDiscovered(i) | CentralEvent::DeviceUpdated(i) => i == id,
                    CentralEvent::RssiUpdate { id: i, .. }
                    | CentralEvent::ManufacturerDataAdvertisement { id: i, .. }
                    | CentralEvent::ServiceDataAdvertisement { id: i, .. }
                    | CentralEvent::ServicesAdvertisement { id: i, .. } => i == id,
                    _ => false,
                };
                if seen {
                    return;
                }
            }
        })
        .await;
        Ok(())
    }

    /// Retries forever; only returns a connected peripheral.
    async fn connect(&self, address: &str, role: Role, tx: &EventTx) -> Peripheral {
        let mut backoff = Duration::from_secs(2);
        loop {
            link(tx, role, LinkStatus::Searching, None);
            let attempt = async {
                let p = self.find(address).await?;
                link(tx, role, LinkStatus::Connecting, None);
                if !p.is_connected().await.unwrap_or(false) {
                    match timeout(CONNECT_TIMEOUT, p.connect()).await {
                        Ok(r) => r.context("connect failed")?,
                        Err(_) => {
                            let _ = p.disconnect().await; // cancel the pending BlueZ connect
                            bail!("connect timed out");
                        }
                    }
                }
                timeout(DISCOVER_TIMEOUT, p.discover_services())
                    .await
                    .map_err(|_| anyhow!("service discovery timed out"))?
                    .context("service discovery failed")?;
                Ok::<_, anyhow::Error>(p)
            };
            match attempt.await {
                Ok(p) => return p,
                Err(e) => {
                    let msg = format!("{e:#}");
                    warn!("{} {address}: {msg}", role.label());
                    link(tx, role, LinkStatus::Disconnected(msg), None);
                    if let Ok(p) = self.find(address).await {
                        let _ = self.wait_for_advert(&p.id(), backoff * 4).await;
                    }
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }
        }
    }
}

fn link(tx: &EventTx, role: Role, status: LinkStatus, name: Option<String>) {
    let _ = tx.send(DeviceEvent::Link { role, status, name });
}

fn notice(tx: &EventTx, msg: String) {
    warn!("{msg}");
    let _ = tx.send(DeviceEvent::Notice(msg));
}

fn find_char(p: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    p.characteristics().into_iter().find(|c| c.uuid == uuid)
}

async fn device_name(p: &Peripheral) -> Option<String> {
    p.properties().await.ok().flatten().and_then(|pr| pr.local_name)
}

fn log_services(p: &Peripheral, role: Role) {
    for s in p.services() {
        for c in &s.characteristics {
            debug!("{} service {} char {} {:?}", role.label(), s.uuid, c.uuid, c.properties);
        }
    }
}

// ---------------------------------------------------------------- HR

pub async fn run_hr(ble: Ble, address: String, tx: EventTx) {
    loop {
        let p = ble.connect(&address, Role::Hr, &tx).await;
        let err = hr_session(&p, &tx).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
        warn!("HR session ended: {err}");
        let _ = p.disconnect().await;
        link(&tx, Role::Hr, LinkStatus::Disconnected(err), None);
        sleep(Duration::from_secs(1)).await;
    }
}

async fn hr_session(p: &Peripheral, tx: &EventTx) -> Result<()> {
    log_services(p, Role::Hr);
    let ch = find_char(p, proto::HR_MEASUREMENT).ok_or_else(|| anyhow!("no Heart Rate Measurement (0x2A37)"))?;
    let mut stream = p.notifications().await?;
    p.subscribe(&ch).await.context("subscribe 0x2A37")?;
    link(tx, Role::Hr, LinkStatus::Connected, device_name(p).await);

    let mut check = interval(Duration::from_secs(2));
    let mut last_data = Instant::now();
    loop {
        tokio::select! {
            n = stream.next() => {
                let n = n.ok_or_else(|| anyhow!("notification stream ended"))?;
                if n.uuid != proto::HR_MEASUREMENT { continue; }
                debug!(target: "watty::raw", "HR 2a37 <- {}", hex(&n.value));
                match proto::parse_heart_rate(&n.value) {
                    Some(hr) => { last_data = Instant::now(); let _ = tx.send(DeviceEvent::Hr(hr)); }
                    None => warn!("unparsable HR packet: {}", hex(&n.value)),
                }
            }
            _ = check.tick() => {
                if !p.is_connected().await.unwrap_or(false) { bail!("link lost"); }
                if last_data.elapsed() > NO_DATA_TIMEOUT { bail!("no HR data for {}s", NO_DATA_TIMEOUT.as_secs()); }
            }
        }
    }
}

// ---------------------------------------------------------------- Trainer

/// `control = false` only reads data (never touches the Control Point).
pub async fn run_trainer(ble: Ble, address: String, tx: EventTx, mut target: watch::Receiver<TrainerTarget>, control: bool) {
    loop {
        let p = ble.connect(&address, Role::Trainer, &tx).await;
        let err = trainer_session(&p, &tx, &mut target, control)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        warn!("trainer session ended: {err}");
        let _ = p.disconnect().await;
        link(&tx, Role::Trainer, LinkStatus::Disconnected(err), None);
        sleep(Duration::from_secs(1)).await;
    }
}

struct Pending {
    op: u8,
    target: Option<TrainerTarget>,
    at: Instant,
}

/// One CP request in flight: RequestControl → Start → SetTargetPower/SetSimulation.
struct Control {
    cp: Characteristic,
    has_control: bool,
    started: bool,
    pending: Option<Pending>,
    applied: Option<TrainerTarget>,
    last_target_sent: Option<Instant>,
    retry_after: Option<Instant>,
}

impl Control {
    fn new(cp: Characteristic) -> Self {
        Self { cp, has_control: false, started: false, pending: None, applied: None, last_target_sent: None, retry_after: None }
    }

    fn backoff(&mut self, d: Duration) {
        self.retry_after = Some(Instant::now() + d);
    }

    async fn drive(&mut self, p: &Peripheral, tx: &EventTx, target: TrainerTarget) {
        if let Some(pd) = &self.pending {
            if pd.at.elapsed() < CP_RESPONSE_TIMEOUT {
                return;
            }
            notice(tx, format!("ERG: no response to {} (0x{:02x})", proto::cp_op_name(pd.op), pd.op));
            self.pending = None;
            self.backoff(Duration::from_secs(2));
        }
        if self.retry_after.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if !self.has_control {
            self.send(p, tx, proto::cp_request_control(), None).await;
        } else if !self.started {
            self.send(p, tx, proto::cp_start(), None).await;
        } else if target != TrainerTarget::Idle
            && self.applied != Some(target)
            && self.last_target_sent.is_none_or(|t| t.elapsed() >= TARGET_MIN_INTERVAL)
        {
            let bytes = match target {
                TrainerTarget::Erg(w) => proto::cp_set_target_power(w),
                TrainerTarget::FreeRide => proto::cp_simulation_flat(),
                TrainerTarget::Idle => unreachable!(),
            };
            self.last_target_sent = Some(Instant::now());
            self.send(p, tx, bytes, Some(target)).await;
        }
    }

    async fn send(&mut self, p: &Peripheral, tx: &EventTx, bytes: Vec<u8>, target: Option<TrainerTarget>) {
        let op = bytes[0];
        info!(target: "watty::raw", "Trainer 2ad9 -> {} ({})", hex(&bytes), proto::cp_op_name(op));
        match timeout(WRITE_TIMEOUT, p.write(&self.cp, &bytes, WriteType::WithResponse)).await {
            Ok(Ok(())) => self.pending = Some(Pending { op, target, at: Instant::now() }),
            Ok(Err(e)) => {
                notice(tx, format!("ERG: write {} failed: {e}", proto::cp_op_name(op)));
                self.backoff(Duration::from_secs(3));
            }
            Err(_) => {
                notice(tx, format!("ERG: write {} timed out", proto::cp_op_name(op)));
                self.backoff(Duration::from_secs(3));
            }
        }
    }

    fn on_response(&mut self, tx: &EventTx, data: &[u8]) {
        let Some(resp) = proto::parse_cp_response(data) else {
            warn!("unexpected control point indication: {}", hex(data));
            return;
        };
        let Some(pd) = self.pending.take() else {
            debug!("unsolicited CP response {resp:?}");
            return;
        };
        if resp.request_op != pd.op {
            warn!("CP response for 0x{:02x}, expected 0x{:02x}", resp.request_op, pd.op);
        }
        let name = proto::cp_op_name(pd.op);
        if resp.ok() {
            info!("ERG: {name} OK");
            match pd.op {
                op::REQUEST_CONTROL => self.has_control = true,
                op::START_RESUME => self.started = true,
                _ => {
                    self.applied = pd.target;
                    if let Some(t) = pd.target {
                        let _ = tx.send(DeviceEvent::TargetApplied(t));
                    }
                }
            }
            return;
        }
        notice(tx, format!("ERG: {name} rejected: {} (0x{:02x})", proto::cp_result_name(resp.result), resp.result));
        match (pd.op, resp.result) {
            // Some trainers don't need/implement Start; carry on without it.
            (op::START_RESUME, _) => self.started = true,
            // Free ride is best effort: don't retry it.
            (op::SET_SIM_PARAMS, _) => self.applied = pd.target,
            (_, 0x05) => {
                self.has_control = false;
                self.started = false;
                self.backoff(Duration::from_secs(5));
            }
            _ => self.backoff(Duration::from_secs(5)),
        }
    }

    fn on_status(&mut self, tx: &EventTx, data: &[u8]) {
        if data.first() == Some(&proto::STATUS_CONTROL_LOST) {
            notice(tx, "ERG: trainer reports control lost (another app?) — re-requesting".into());
            self.has_control = false;
            self.started = false;
            self.applied = None;
        }
    }
}

async fn trainer_session(
    p: &Peripheral,
    tx: &EventTx,
    target: &mut watch::Receiver<TrainerTarget>,
    control: bool,
) -> Result<()> {
    log_services(p, Role::Trainer);
    let data_ch = find_char(p, proto::INDOOR_BIKE_DATA)
        .ok_or_else(|| anyhow!("no Indoor Bike Data (0x2AD2) — is this an FTMS trainer?"))?;
    let mut stream = p.notifications().await?;
    p.subscribe(&data_ch).await.context("subscribe 0x2AD2")?;

    if let Some(st) = find_char(p, proto::FTMS_STATUS)
        && let Err(e) = p.subscribe(&st).await {
            warn!("subscribe 0x2ADA: {e}");
        }
    let mut ctl = None;
    if control {
        match find_char(p, proto::FTMS_CONTROL_POINT) {
            // Indications must be on before any CP write (else ATT 0xFD).
            Some(cp) => {
                p.subscribe(&cp).await.context("enable indications on 0x2AD9")?;
                ctl = Some(Control::new(cp));
            }
            None => notice(tx, "trainer has no FTMS Control Point (0x2AD9): ERG unavailable".into()),
        }
    }
    link(tx, Role::Trainer, LinkStatus::Connected, device_name(p).await);

    let mut tick = interval(Duration::from_millis(250));
    let mut last_check = Instant::now();
    let mut last_data = Instant::now();
    loop {
        tokio::select! {
            n = stream.next() => {
                let n = n.ok_or_else(|| anyhow!("notification stream ended"))?;
                match n.uuid {
                    proto::INDOOR_BIKE_DATA => {
                        debug!(target: "watty::raw", "Trainer 2ad2 <- {}", hex(&n.value));
                        match proto::parse_indoor_bike_data(&n.value) {
                            Some(d) => { last_data = Instant::now(); let _ = tx.send(DeviceEvent::Bike(d)); }
                            None => warn!("unparsable Indoor Bike Data: {}", hex(&n.value)),
                        }
                    }
                    proto::FTMS_CONTROL_POINT => {
                        info!(target: "watty::raw", "Trainer 2ad9 <- {}", hex(&n.value));
                        if let Some(c) = ctl.as_mut() { c.on_response(tx, &n.value); }
                    }
                    proto::FTMS_STATUS => {
                        info!(target: "watty::raw", "Trainer 2ada <- {}", hex(&n.value));
                        if let Some(c) = ctl.as_mut() { c.on_status(tx, &n.value); }
                    }
                    other => debug!(target: "watty::raw", "Trainer {other} <- {}", hex(&n.value)),
                }
            }
            _ = tick.tick() => {
                if let Some(c) = ctl.as_mut() {
                    let t = *target.borrow_and_update();
                    c.drive(p, tx, t).await;
                }
                if last_check.elapsed() >= Duration::from_secs(2) {
                    last_check = Instant::now();
                    if !p.is_connected().await.unwrap_or(false) { bail!("link lost"); }
                    if last_data.elapsed() > NO_DATA_TIMEOUT {
                        bail!("no Indoor Bike Data for {}s", NO_DATA_TIMEOUT.as_secs());
                    }
                }
            }
        }
    }
}
