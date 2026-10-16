use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use crate::awake::KeepAwake;
use crate::ble::Ble;
use crate::config::{self, Config};
use crate::history;
use crate::recorder::{RideMeta, Recorder};
use crate::session::{RunState, Sensors, Session};
use crate::strava;
use crate::ui::{self, ChartZoom, Picker, View};
use crate::workout::{Workout, fmt_duration};
use crate::{DeviceArgs, Devices, start_devices};

pub fn spawn_keys() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

pub fn key_press(ev: &Event) -> Option<(KeyCode, KeyModifiers)> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some((k.code, k.modifiers)),
        _ => None,
    }
}

pub struct RideSetup {
    pub workout: Workout,
    pub workout_path: Option<PathBuf>,
    /// Journal of an interrupted ride to continue.
    pub resume: Option<PathBuf>,
    pub ftp: u32,
    pub weight: f64,
    /// Show the device picker even if devices are configured.
    pub select: bool,
    pub dev: DeviceArgs,
}

pub async fn run(file: PathBuf, ftp: Option<u32>, weight: Option<f64>, select: bool, dev: DeviceArgs) -> Result<()> {
    let mut cfg = Config::load()?;
    let workout = Workout::load(&file)?;
    let ftp = ftp.or(cfg.ftp).ok_or_else(|| anyhow!("FTP unknown: pass --ftp <watts> (it will be saved to config)"))?;
    let weight = weight
        .or(cfg.weight)
        .filter(|w| (30.0..=200.0).contains(w))
        .ok_or_else(|| anyhow!("weight unknown: pass --weight <kg> (it will be saved to config)"))?;
    cfg.hr_zones()?;
    cfg.ftp = Some(ftp);
    cfg.weight = Some(weight);
    cfg.save()?;

    if !select {
        crate::resolve_devices(&mut cfg, &dev).await?;
    }

    let mut terminal = ratatui::init();
    let mut keys = spawn_keys();
    let workout_path = Some(std::path::absolute(&file)?);
    let setup = RideSetup { workout, workout_path, resume: None, ftp, weight, select, dev };
    let result = ride(&mut terminal, &mut keys, &mut cfg, setup).await;
    ratatui::restore();
    let (summary, saved) = result?;
    println!("{summary}");
    if let Some(ride) = saved.as_deref().and_then(history::ride_file)
        && cfg.strava_upload
        && strava::load_tokens().is_some()
    {
        println!("uploading to Strava...");
        match strava::Client::load(&cfg).await {
            Ok(client) => match client.upload(&ride, &cfg).await {
                Ok(id) => println!("Strava: https://www.strava.com/activities/{id}"),
                Err(e) => println!("Strava upload failed: {e:#}"),
            },
            Err(e) => println!("Strava upload failed: {e:#}"),
        }
    }
    Ok(())
}

/// Runs a ride on an already initialized terminal. Returns its summary and, for a saved
/// real (not simulated) ride, its journal.
pub async fn ride(
    terminal: &mut DefaultTerminal,
    keys: &mut mpsc::UnboundedReceiver<Event>,
    cfg: &mut Config,
    setup: RideSetup,
) -> Result<(String, Option<PathBuf>)> {
    let RideSetup { workout, workout_path, resume, ftp, weight, select, dev } = setup;
    let _awake = cfg.keep_awake.then(KeepAwake::start);
    let mut trainer = dev.trainer.clone().or(cfg.trainer.clone());
    let mut hr = if dev.no_hr { None } else { dev.hr.clone().or(cfg.hr.clone()) };

    if !dev.sim && (trainer.is_none() || select) {
        let ble = Ble::new().await?;
        let Some((t, h)) = pick_devices(terminal, keys, &ble, trainer, hr).await? else {
            return Ok(("cancelled".into(), None));
        };
        (trainer, hr) = (Some(t), h);
        cfg.trainer = trainer.clone();
        cfg.hr = hr.clone();
        cfg.save()?;
    }

    let mut devices = start_devices(trainer, hr, &dev, true).await?;
    let mut session = Session::new(workout, ftp, weight, cfg.auto_pause);
    session.soft_start = cfg.soft_start;
    let hr_zones = cfg.hr_zones()?;
    session.hr_zones = hr_zones;
    let mut sensors = Sensors::default();
    let mut recorder: Option<Recorder> = None;
    if let Some(csv) = &resume {
        let (r, samples) = Recorder::resume(csv)?;
        let clock = (chrono::Utc::now() - r.start()).num_seconds().max(0) as u32;
        session.restore(&samples, r.meta.intensity, clock);
        recorder = Some(r);
    }
    let mut message: Option<(Instant, String)> = None;
    let mut confirm_quit = false;
    let mut discard = false;
    // Test rides stay out of the ride history.
    let rides_dir = if dev.sim { config::rides_dir().join("test") } else { config::rides_dir() };
    let mut zoom = ChartZoom::new();

    let mut tick = interval(Duration::from_secs_f64(1.0 / dev.speed.max(0.1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut redraw = interval(Duration::from_millis(250));
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    loop {
        zoom.set(session.state == RunState::Running);
        let rec_path = recorder.as_ref().map(|r| r.csv_path().display().to_string());
        terminal.draw(|f| {
            ui::draw_ride(
                f,
                &View {
                    session: &session,
                    sensors: &sensors,
                    message: message.as_ref(),
                    confirm_quit,
                    sim: dev.sim,
                    recording: rec_path.as_deref(),
                    zoom: &zoom,
                    hr_zones,
                    segment_warning: cfg.segment_warning,
                    cadence_tolerance: cfg.cadence_tolerance,
                },
            )
        })?;

        tokio::select! {
            Some(ev) = keys.recv() => {
                let Some((code, mods)) = key_press(&ev) else { continue };
                if confirm_quit {
                    match code {
                        KeyCode::Char('y') | KeyCode::Char('q') | KeyCode::Enter => break,
                        KeyCode::Char('d') => {
                            discard = true;
                            break;
                        }
                        KeyCode::Char('n') | KeyCode::Esc => confirm_quit = false,
                        _ => {}
                    }
                    continue;
                }
                match code {
                    KeyCode::Char(' ') => session.toggle_pause(),
                    KeyCode::Char('+') | KeyCode::Char('=') => session.adjust_intensity(0.05),
                    KeyCode::Char('-') => session.adjust_intensity(-0.05),
                    KeyCode::Char('n') => session.skip_segment(),
                    KeyCode::Char('a') => session.answer_hr_suggestion(true),
                    KeyCode::Char('x') => session.answer_hr_suggestion(false),
                    KeyCode::Char('s') => if let Some(c) = &devices.coasting {
                        c.fetch_xor(true, Ordering::Relaxed);
                    },
                    KeyCode::Char('q') | KeyCode::Esc => confirm_quit = true,
                    KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => confirm_quit = true,
                    _ => {}
                }
            }
            Some(ev) = devices.events.recv() => {
                if let Some(m) = sensors.apply(ev) {
                    info!("{m}");
                    message = Some((Instant::now(), m));
                }
            }
            _ = tick.tick() => {
                if let Some(sample) = session.tick(&sensors) {
                    if recorder.is_none() {
                        let meta = RideMeta {
                            workout: session.workout.name.clone(),
                            workout_path: workout_path.clone(),
                            ftp,
                            intensity: session.intensity,
                            finished: false,
                            closed: false,
                            strava_id: None,
                        };
                        match Recorder::create(&rides_dir, meta) {
                            Ok(r) => recorder = Some(r),
                            Err(e) => {
                                error!("recorder: {e:#}");
                                message = Some((Instant::now(), format!("CANNOT RECORD: {e:#}")));
                            }
                        }
                    }
                    if let Some(r) = recorder.as_mut() {
                        r.meta.intensity = session.intensity;
                        r.meta.finished = session.state == RunState::Finished;
                    }
                    if let Some(r) = recorder.as_mut()
                        && let Err(e) = r.record(sample)
                    {
                        error!("record: {e:#}");
                        message = Some((Instant::now(), format!("write error: {e:#}")));
                    }
                }
            }
            _ = redraw.tick() => {}
            _ = tokio::time::sleep(Duration::from_millis(33)), if zoom.animating() => {}
            _ = term.recv() => break,
            _ = hangup.recv() => break,
        }

        let target = session.trainer_target();
        devices.target.send_if_modified(|t| {
            let changed = *t != target;
            *t = target;
            changed
        });
    }

    shutdown(&mut devices).await;
    let mut out = format!(
        "{} — ridden {} ({:.2} km), workout position {}/{}",
        session.workout.name,
        fmt_duration(session.elapsed),
        session.distance_m / 1000.0,
        fmt_duration(session.pos.min(session.workout.total_duration())),
        fmt_duration(session.workout.total_duration()),
    );
    let mut saved = None;
    if let Some(r) = recorder.take_if(|_| discard) {
        match r.discard() {
            Ok(()) => out += "\nride discarded",
            Err(e) => out += &format!("\ncannot discard: {e:#}"),
        }
    } else if let Some(mut r) = recorder {
        r.meta.intensity = session.intensity;
        r.meta.finished |= session.state == RunState::Finished;
        let (csv, tcx) = (r.csv_path().to_path_buf(), r.tcx_path().to_path_buf());
        match r.finish() {
            Ok(n) => {
                out += &format!("\n{n} samples\nTCX: {}\nCSV: {}", tcx.display(), csv.display());
                saved = (!dev.sim && n > 0).then_some(csv);
            }
            Err(e) => out += &format!("\nTCX write failed: {e:#}\nrecover with: watty export {}", csv.display()),
        }
    } else {
        out += "\nnothing recorded";
    }
    Ok((out, saved))
}

async fn shutdown(devices: &mut Devices) {
    if let Some(ble) = &devices.ble {
        for addr in &devices.addresses {
            ble.disconnect(addr).await;
        }
    }
}

pub async fn pick_devices(
    terminal: &mut DefaultTerminal,
    keys: &mut mpsc::UnboundedReceiver<Event>,
    ble: &Ble,
    trainer: Option<String>,
    hr: Option<String>,
) -> Result<Option<(String, Option<String>)>> {
    let _scan = ble.scan().await?;
    let mut p = Picker { devices: vec![], state: ListState::default().with_selected(Some(0)), trainer, hr };
    let mut refresh = interval(Duration::from_millis(700));
    loop {
        terminal.draw(|f| ui::draw_picker(f, &mut p))?;
        tokio::select! {
            _ = refresh.tick() => {
                p.devices = ble.discovered().await?;
                p.devices.retain(|d| d.ftms || d.hr || d.name != d.address.replace(':', "-"));
            }
            Some(ev) = keys.recv() => {
                let Some((code, mods)) = key_press(&ev) else { continue };
                let selected = p.state.selected().and_then(|i| p.devices.get(i)).map(|d| d.address.clone());
                match code {
                    KeyCode::Up | KeyCode::Char('k') => p.state.select_previous(),
                    KeyCode::Down | KeyCode::Char('j') => p.state.select_next(),
                    KeyCode::Char('t') => p.trainer = selected.or(p.trainer.take()),
                    KeyCode::Char('h') => {
                        if selected.is_some() && selected == p.hr { p.hr = None } else if selected.is_some() { p.hr = selected }
                    }
                    KeyCode::Enter => if let Some(t) = p.trainer.clone() { return Ok(Some((t, p.hr.clone()))) },
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}
