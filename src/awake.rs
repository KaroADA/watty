//! Keeps the screen on during a ride, when nobody touches the keyboard.

use std::process::{Child, Command, Stdio};

use tracing::{info, warn};

/// Released on drop.
pub struct KeepAwake {
    /// Hyprland window tagged `noidle`; Omarchy maps that tag to `idle_inhibit = "always"`.
    window: Option<String>,
    /// For idle daemons honoring logind inhibitors, e.g. hypridle.
    systemd: Option<Child>,
    /// `org.freedesktop.ScreenSaver` (GNOME, KDE, Xfce); held while the connection stays open.
    screensaver: Option<(zbus::blocking::Connection, u32)>,
}

impl KeepAwake {
    pub fn start() -> Self {
        let window = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").and_then(|_| tag_terminal_window());
        let systemd = Command::new("systemd-inhibit")
            .args(["--what=idle", "--who=watty", "--why=Riding", "tail", "-f", "/dev/null"])
            .arg(format!("--pid={}", std::process::id()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .inspect_err(|e| warn!("systemd-inhibit: {e}"))
            .ok();
        Self { window, systemd, screensaver: screensaver_inhibit() }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(addr) = &self.window {
            hypr_tag(addr, "-noidle");
        }
        if let Some(child) = &mut self.systemd {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some((conn, cookie)) = &self.screensaver {
            let _ = conn.call_method(Some(SCREENSAVER), SCREENSAVER_PATH, Some(SCREENSAVER), "UnInhibit", cookie);
        }
    }
}

const SCREENSAVER: &str = "org.freedesktop.ScreenSaver";
const SCREENSAVER_PATH: &str = "/org/freedesktop/ScreenSaver";

fn screensaver_inhibit() -> Option<(zbus::blocking::Connection, u32)> {
    let conn = zbus::blocking::Connection::session().inspect_err(|e| info!("keep awake: no session bus: {e}")).ok()?;
    let reply = conn
        .call_method(Some(SCREENSAVER), SCREENSAVER_PATH, Some(SCREENSAVER), "Inhibit", &("watty", "Riding"))
        .inspect_err(|e| info!("keep awake: {SCREENSAVER} unavailable: {e}"))
        .ok()?;
    let cookie = reply.body().deserialize::<u32>().ok()?;
    info!("keep awake: {SCREENSAVER} inhibited");
    Some((conn, cookie))
}

fn tag_terminal_window() -> Option<String> {
    let out = Command::new("hyprctl").args(["-j", "clients"]).output().ok()?;
    let clients: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let ancestors = ancestors();
    let addr = clients.iter().find_map(|c| {
        let pid = c["pid"].as_u64()? as u32;
        ancestors.contains(&pid).then(|| c["address"].as_str().map(str::to_string)).flatten()
    });
    let Some(addr) = addr else {
        warn!("keep awake: terminal window not found");
        return None;
    };
    hypr_tag(&addr, "+noidle").then_some(addr)
}

/// Lua config (Hyprland 0.55+) first, then the legacy dispatcher.
fn hypr_tag(addr: &str, tag: &str) -> bool {
    let lua = format!("hl.dsp.window.tag({{ tag = \"{tag}\", window = \"address:{addr}\" }})");
    let legacy = format!("tagwindow {tag} address:{addr}");
    let ok = [lua, legacy].into_iter().any(|cmd| {
        Command::new("hyprctl")
            .args(["dispatch", &cmd])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok")
    });
    if ok {
        info!("keep awake: tagged {addr} {tag}");
    } else {
        warn!("keep awake: cannot tag {addr} {tag}");
    }
    ok
}

/// This process and its parents, up to init.
fn ancestors() -> Vec<u32> {
    let mut pids = vec![];
    let mut pid = std::process::id();
    while pid > 1 {
        pids.push(pid);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { break };
        // "pid (comm) state ppid ..."; comm may contain spaces and parentheses.
        let Some(ppid) = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok()) else {
            break;
        };
        pid = ppid;
    }
    pids
}
