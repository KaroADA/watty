mod ble;
mod config;
mod devices;
mod protocol;
mod recorder;
mod ride;
mod session;
mod sim;
mod ui;
mod workout;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

use crate::ble::Ble;
use crate::config::Config;
use crate::devices::{DeviceEvent, TrainerTarget};
use crate::sim::SimOptions;
use crate::workout::{Workout, fmt_duration};

#[derive(Parser)]
#[command(version, about = "Smart trainer (FTMS) TUI for .zwo workouts")]
struct Cli {
    /// Debug logging, incl. raw BLE bytes
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct DeviceArgs {
    /// Trainer address (default: from config)
    #[arg(long)]
    trainer: Option<String>,
    /// HR sensor address (default: from config)
    #[arg(long)]
    hr: Option<String>,
    /// Don't use an HR sensor
    #[arg(long)]
    no_hr: bool,
    /// Use simulated devices
    #[arg(long)]
    sim: bool,
    /// Simulation speed multiplier
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Simulate periodic BLE dropouts
    #[arg(long)]
    sim_dropouts: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse a .zwo file and print its segments
    Parse {
        file: PathBuf,
        #[arg(long)]
        ftp: Option<u32>,
    },
    /// List nearby BLE devices
    Scan {
        #[arg(long, default_value_t = 10)]
        secs: u64,
    },
    /// Print live trainer/HR data without controlling the trainer
    Monitor {
        #[command(flatten)]
        dev: DeviceArgs,
    },
    /// Manual ERG test: type watts + Enter, `f` = free ride, `q` = quit
    Erg {
        #[command(flatten)]
        dev: DeviceArgs,
        /// Initial target
        #[arg(long, default_value_t = 100)]
        watts: u16,
    },
    /// Ride a .zwo workout in the TUI (records to ~/.local/share/watty/rides)
    Ride {
        file: PathBuf,
        /// FTP in watts (saved to config)
        #[arg(long)]
        ftp: Option<u32>,
        /// Rider weight in kg, for virtual speed (saved to config)
        #[arg(long)]
        weight: Option<f64>,
        /// Show the device picker even if devices are configured
        #[arg(long)]
        select: bool,
        #[command(flatten)]
        dev: DeviceArgs,
    },
    /// Rebuild a TCX from a ride's CSV journal (e.g. after a crash)
    Export {
        csv: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

/// `ride` logs to a file (incl. raw BLE bytes) since the TUI owns the terminal.
fn init_logging(verbose: bool, file: Option<&std::path::Path>) -> Result<()> {
    let default = if verbose || file.is_some() { "watty=debug" } else { "watty=info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into());
    let fmt = tracing_subscriber::fmt().with_env_filter(filter);
    match file {
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            fmt.with_ansi(false).with_writer(std::sync::Mutex::new(f)).init();
        }
        None => fmt.with_writer(std::io::stderr).init(),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let log_file = config::data_dir().join("watty.log");
    let tui = matches!(cli.cmd, Cmd::Ride { .. });
    init_logging(cli.verbose, tui.then_some(log_file.as_path()))?;
    let mut cfg = Config::load()?;

    match cli.cmd {
        Cmd::Parse { file, ftp } => parse_cmd(&file, ftp.or(cfg.ftp)),
        Cmd::Scan { secs } => scan_cmd(secs).await,
        Cmd::Monitor { dev } => monitor_cmd(&mut cfg, dev, None).await,
        Cmd::Erg { dev, watts } => monitor_cmd(&mut cfg, dev, Some(watts)).await,
        Cmd::Ride { file, ftp, weight, select, dev } => {
            let r = ride::run(file, ftp, weight, select, dev).await;
            eprintln!("log: {}", log_file.display());
            r
        }
        Cmd::Export { csv, out } => {
            let (path, n) = recorder::export(&csv, out)?;
            println!("{n} samples -> {}", path.display());
            Ok(())
        }
    }
}

fn parse_cmd(file: &std::path::Path, ftp: Option<u32>) -> Result<()> {
    let w = Workout::load(file)?;
    println!("{} — {}", w.name, w.description);
    println!("total {} in {} segments\n", fmt_duration(w.total_duration()), w.segments.len());
    for (i, s) in w.segments.iter().enumerate() {
        let pct = |p: Option<f64>| p.map_or("free".to_string(), |p| format!("{:.0}%", p * 100.0));
        let watts = |p: Option<f64>| match (p, ftp) {
            (Some(p), Some(ftp)) => format!("{:.0} W", p * ftp as f64),
            _ => String::new(),
        };
        println!(
            "{i:>3}  {:>8} +{:<6} {:<13} {:>5} -> {:<5} {:>6} -> {:<6}",
            fmt_duration(s.start),
            fmt_duration(s.duration),
            s.kind.label(),
            pct(s.power_start),
            pct(s.power_end),
            watts(s.power_start),
            watts(s.power_end),
        );
    }
    Ok(())
}

async fn scan_cmd(secs: u64) -> Result<()> {
    let ble = Ble::new().await?;
    let _scan = ble.scan().await?;
    eprintln!("scanning for {secs} s...");
    tokio::time::sleep(Duration::from_secs(secs)).await;
    for d in ble.discovered().await? {
        let tag = match (d.ftms, d.hr) {
            (true, _) => "[FTMS]",
            (_, true) => "[HR]  ",
            _ => "      ",
        };
        let rssi = d.rssi.map_or("  --".into(), |r| format!("{r:>4}"));
        println!("{tag} {}  {rssi} dBm  {}", d.address, d.name);
    }
    Ok(())
}

pub struct Devices {
    pub events: mpsc::UnboundedReceiver<DeviceEvent>,
    pub target: watch::Sender<TrainerTarget>,
    pub ble: Option<Ble>,
    pub addresses: Vec<String>,
}

async fn start_devices(trainer: Option<String>, hr: Option<String>, dev: &DeviceArgs, control: bool) -> Result<Devices> {
    let (tx, events) = mpsc::unbounded_channel();
    let (target, target_rx) = watch::channel(TrainerTarget::Idle);
    if dev.sim {
        sim::spawn(tx, target_rx, SimOptions { speed: dev.speed, dropouts: dev.sim_dropouts });
        return Ok(Devices { events, target, ble: None, addresses: vec![] });
    }
    let ble = Ble::new().await?;
    let addresses: Vec<String> = trainer.iter().chain(hr.iter()).cloned().collect();
    if let Some(addr) = trainer {
        tokio::spawn(ble::run_trainer(ble.clone(), addr, tx.clone(), target_rx, control));
    }
    if let Some(addr) = hr {
        tokio::spawn(ble::run_hr(ble.clone(), addr, tx));
    }
    Ok(Devices { events, target, ble: Some(ble), addresses })
}

/// Fills devices missing from CLI/config by auto-detection and saves unique finds.
async fn resolve_devices(cfg: &mut Config, dev: &DeviceArgs) -> Result<(Option<String>, Option<String>)> {
    let trainer = dev.trainer.clone().or(cfg.trainer.clone());
    let hr = if dev.no_hr { None } else { dev.hr.clone().or(cfg.hr.clone()) };
    let (want_t, want_h) = (trainer.is_none(), hr.is_none() && !dev.no_hr);
    if dev.sim || !(want_t || want_h) {
        return Ok((trainer, hr));
    }
    eprintln!("looking for {} (up to 8 s)...", match (want_t, want_h) {
        (true, true) => "trainer and HR strap",
        (true, false) => "trainer",
        _ => "HR strap (wear it so it advertises)",
    });
    let (t, h) = Ble::new().await?.autodetect(want_t, want_h, Duration::from_secs(8)).await?;
    if let Some(a) = &t {
        eprintln!("found trainer {a}, saved to config");
        cfg.trainer = t.clone();
    }
    if let Some(a) = &h {
        eprintln!("found HR strap {a}, saved to config");
        cfg.hr = h.clone();
    }
    if t.is_some() || h.is_some() {
        cfg.save()?;
    }
    Ok((trainer.or(t), hr.or(h)))
}

async fn monitor_cmd(cfg: &mut Config, dev: DeviceArgs, erg: Option<u16>) -> Result<()> {
    let (trainer, hr) = resolve_devices(cfg, &dev).await?;
    if !dev.sim && trainer.is_none() && hr.is_none() {
        bail!("no devices found: pass --trainer/--hr (see `watty scan`) or --sim");
    }
    let mut d = start_devices(trainer, hr, &dev, erg.is_some()).await?;
    if let Some(w) = erg {
        d.target.send_replace(TrainerTarget::Erg(w));
        eprintln!("ERG {w} W. Type watts + Enter, `f` = free ride, `q` = quit.");
    }
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut term = signal(SignalKind::terminate())?;
    let started = std::time::Instant::now();
    loop {
        tokio::select! {
            ev = d.events.recv() => {
                let Some(ev) = ev else { break };
                let t = started.elapsed().as_secs_f64();
                match ev {
                    DeviceEvent::Bike(b) => println!(
                        "{t:7.1}s BIKE power {:>4} W  cadence {:>5} rpm  speed {:>5} km/h",
                        b.power_w.map_or("-".into(), |v| v.to_string()),
                        b.cadence_rpm.map_or("-".into(), |v| format!("{v:.1}")),
                        b.speed_kmh.map_or("-".into(), |v| format!("{v:.1}")),
                    ),
                    DeviceEvent::Hr(h) => println!("{t:7.1}s HR   {} bpm  contact {:?}  rr {:?}", h.bpm, h.contact, h.rr_ms),
                    DeviceEvent::Link { role, status, name } => {
                        println!("{t:7.1}s LINK {} {status:?} {}", role.label(), name.unwrap_or_default())
                    }
                    DeviceEvent::TargetApplied(target) => println!("{t:7.1}s ERG  applied {target:?}"),
                    DeviceEvent::Notice(m) => println!("{t:7.1}s !!   {m}"),
                }
            }
            line = stdin.next_line(), if erg.is_some() => {
                let Ok(Some(line)) = line else { break };
                match line.trim() {
                    "q" => break,
                    "f" => { d.target.send_replace(TrainerTarget::FreeRide); }
                    s => match s.parse::<u16>() {
                        Ok(w) if w <= 1500 => { d.target.send_replace(TrainerTarget::Erg(w)); }
                        _ => eprintln!("? watts 0-1500, `f` or `q`"),
                    },
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }
    if let Some(ble) = &d.ble {
        for addr in &d.addresses {
            ble.disconnect(addr).await;
        }
    }
    Ok(())
}
