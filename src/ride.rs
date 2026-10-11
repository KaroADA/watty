use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use crate::ble::Ble;
use crate::config::{self, Config};
use crate::recorder::Recorder;
use crate::session::{Sensors, Session};
use crate::ui::{self, Picker, View};
use crate::workout::{Workout, fmt_duration};
use crate::{DeviceArgs, Devices, start_devices};

fn spawn_keys() -> mpsc::UnboundedReceiver<Event> {
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

fn key_press(ev: &Event) -> Option<(KeyCode, KeyModifiers)> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some((k.code, k.modifiers)),
        _ => None,
    }
}

pub async fn run(file: PathBuf, ftp: Option<u32>, weight: Option<f64>, select: bool, dev: DeviceArgs) -> Result<()> {
    let mut cfg = Config::load()?;
    let workout = Workout::load(&file)?;
    let ftp = ftp.or(cfg.ftp).ok_or_else(|| anyhow!("FTP unknown: pass --ftp <watts> (it will be saved to config)"))?;
    let weight = weight
        .or(cfg.weight)
        .filter(|w| (30.0..=200.0).contains(w))
        .ok_or_else(|| anyhow!("weight unknown: pass --weight <kg> (it will be saved to config)"))?;
    cfg.ftp = Some(ftp);
    cfg.weight = Some(weight);
    cfg.save()?;

    if !select {
        crate::resolve_devices(&mut cfg, &dev).await?;
    }

    let mut terminal = ratatui::init();
    let mut keys = spawn_keys();
    let result = ride(&mut terminal, &mut keys, &mut cfg, workout, ftp, weight, select, dev).await;
    ratatui::restore();
    let summary = result?;
    println!("{summary}");
    Ok(())
}

async fn ride(
    terminal: &mut DefaultTerminal,
    keys: &mut mpsc::UnboundedReceiver<Event>,
    cfg: &mut Config,
    workout: Workout,
    ftp: u32,
    weight: f64,
    select: bool,
    dev: DeviceArgs,
) -> Result<String> {
    let mut trainer = dev.trainer.clone().or(cfg.trainer.clone());
    let mut hr = if dev.no_hr { None } else { dev.hr.clone().or(cfg.hr.clone()) };

    if !dev.sim && (trainer.is_none() || select) {
        let ble = Ble::new().await?;
        let Some((t, h)) = pick_devices(terminal, keys, &ble, trainer, hr).await? else {
            return Ok("cancelled".into());
        };
        (trainer, hr) = (Some(t), h);
        cfg.trainer = trainer.clone();
        cfg.hr = hr.clone();
        cfg.save()?;
    }

    let mut devices = start_devices(trainer, hr, &dev, true).await?;
    let mut session = Session::new(workout, ftp, weight);
    let mut sensors = Sensors::default();
    let mut recorder: Option<Recorder> = None;
    let mut message: Option<(Instant, String)> = None;
    let mut confirm_quit = false;

    let mut tick = interval(Duration::from_secs_f64(1.0 / dev.speed.max(0.1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut redraw = interval(Duration::from_millis(250));
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;

    loop {
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
                        match Recorder::create(&config::data_dir().join("rides"), &session.workout.name) {
                            Ok(r) => recorder = Some(r),
                            Err(e) => {
                                error!("recorder: {e:#}");
                                message = Some((Instant::now(), format!("CANNOT RECORD: {e:#}")));
                            }
                        }
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
    if let Some(r) = recorder {
        let (csv, tcx) = (r.csv_path().to_path_buf(), r.tcx_path().to_path_buf());
        match r.finish() {
            Ok(n) => out += &format!("\n{n} samples\nTCX: {}\nCSV: {}", tcx.display(), csv.display()),
            Err(e) => out += &format!("\nTCX write failed: {e:#}\nrecover with: watty export {}", csv.display()),
        }
    } else {
        out += "\nnothing recorded";
    }
    Ok(out)
}

async fn shutdown(devices: &mut Devices) {
    if let Some(ble) = &devices.ble {
        for addr in &devices.addresses {
            ble.disconnect(addr).await;
        }
    }
}

async fn pick_devices(
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
