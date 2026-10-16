//! Strava: logging in and uploading rides.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{Config, data_dir};
use crate::history::RideFile;
use crate::recorder::{self, RideMeta, read_journal, save_meta};

const API: &str = "https://www.strava.com/api/v3";
const TOKEN_URL: &str = "https://www.strava.com/oauth/token";
const REDIRECT_PORT: u16 = 8476;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

/// Results of background Strava work, for the home screen.
pub enum Event {
    Connected(Result<String>),
    Uploaded(PathBuf, Result<u64>),
}

pub struct Credentials {
    id: String,
    secret: String,
}

/// The watty Strava app, from the environment; `config.toml` can point to another app.
pub fn credentials(cfg: &Config) -> Result<Credentials> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let id = cfg.strava_client_id.clone().or_else(|| env("WATTY_STRAVA_CLIENT_ID"));
    let secret = cfg.strava_client_secret.clone().or_else(|| env("WATTY_STRAVA_CLIENT_SECRET"));
    match (id, secret) {
        (Some(id), Some(secret)) => Ok(Credentials { id, secret }),
        _ => bail!("Strava app credentials missing: set WATTY_STRAVA_CLIENT_ID and WATTY_STRAVA_CLIENT_SECRET"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    access_token: String,
    refresh_token: String,
    /// Unix time.
    expires_at: i64,
    pub athlete: String,
}

fn tokens_path() -> PathBuf {
    data_dir().join("strava.json")
}

pub fn load_tokens() -> Option<Tokens> {
    serde_json::from_str(&std::fs::read_to_string(tokens_path()).ok()?).ok()
}

/// Written readable by the owner only, since the tokens give access to the account.
fn save_tokens(t: &Tokens) -> Result<()> {
    let path = tokens_path();
    std::fs::create_dir_all(data_dir())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(t)?.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    athlete: Option<Athlete>,
}

#[derive(Deserialize)]
struct Athlete {
    firstname: Option<String>,
    lastname: Option<String>,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default()
}

/// Turns an HTTP error status into an error carrying Strava's message.
async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("Strava rate limit reached, try again in 15 minutes");
    }
    bail!("Strava: {status} {body}")
}

async fn token_request(creds: &Credentials, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let mut params = vec![("client_id", creds.id.as_str()), ("client_secret", creds.secret.as_str())];
    params.extend_from_slice(form);
    let resp = http().post(TOKEN_URL).form(&params).send().await.context("cannot reach Strava")?;
    Ok(check(resp).await?.json().await?)
}

/// Waiting for the user to authorize watty in the browser.
pub struct Login {
    pub url: String,
    listener: TcpListener,
    creds: Credentials,
    state: String,
}

impl Login {
    /// Starts listening for the redirect and opens the consent page in the browser.
    pub async fn start(cfg: &Config) -> Result<Self> {
        let creds = credentials(cfg)?;
        let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
            .await
            .with_context(|| format!("cannot listen on port {REDIRECT_PORT}"))?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let state = format!("{:x}", nanos ^ ((std::process::id() as u128) << 64));
        let url = format!(
            "https://www.strava.com/oauth/authorize?client_id={}&response_type=code&redirect_uri=http://localhost:{REDIRECT_PORT}/&approval_prompt=auto&scope=read,activity:read_all,activity:write&state={state}",
            creds.id
        );
        let _ = std::process::Command::new("xdg-open")
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        Ok(Self { url, listener, creds, state })
    }

    /// Waits for the redirect, exchanges the code for tokens and saves them. Returns the athlete's name.
    pub async fn finish(self) -> Result<String> {
        let (mut sock, code) = tokio::time::timeout(LOGIN_TIMEOUT, self.wait_for_redirect())
            .await
            .map_err(|_| anyhow!("no answer from the browser within {} minutes", LOGIN_TIMEOUT.as_secs() / 60))??;
        let result = match code {
            Ok(code) => self.save_login(&code).await,
            Err(e) => Err(e),
        };
        let text = match &result {
            Ok(_) => "Connected to Strava. You can close this tab and go back to watty.".to_string(),
            Err(e) => format!("Not connected: {e:#}").replace('&', "&amp;").replace('<', "&lt;"),
        };
        let page = format!("<!doctype html><meta charset=utf-8><title>watty</title><p style=\"font:18px sans-serif;margin:3em\">{text}</p>");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = sock.write_all(reply.as_bytes()).await;
        result
    }

    async fn save_login(&self, code: &str) -> Result<String> {
        let t = token_request(&self.creds, &[("code", code), ("grant_type", "authorization_code")]).await?;
        let athlete = t
            .athlete
            .map(|a| [a.firstname, a.lastname].into_iter().flatten().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        save_tokens(&Tokens { access_token: t.access_token, refresh_token: t.refresh_token, expires_at: t.expires_at, athlete: athlete.clone() })?;
        Ok(athlete)
    }

    /// The browser's connection, kept open for the reply, and the code it brought.
    async fn wait_for_redirect(&self) -> Result<(TcpStream, Result<String>)> {
        loop {
            let (mut sock, _) = self.listener.accept().await?;
            let mut buf = vec![0; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            match redirect_result(request.lines().next().unwrap_or_default(), &self.state) {
                Some(code) => return Ok((sock, code)),
                None => {
                    let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                }
            }
        }
    }
}

/// The code from Strava's redirect, given the request line `GET /?state=…&code=…&scope=… HTTP/1.1`.
/// `None` for unrelated requests such as the favicon.
fn redirect_result(request_line: &str, state: &str) -> Option<Result<String>> {
    let target = request_line.strip_prefix("GET ")?.split(' ').next()?;
    let query = target.strip_prefix("/?")?;
    let param = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='));
    if param("state") != Some(state) {
        return None;
    }
    if let Some(e) = param("error") {
        return Some(Err(anyhow!("authorization {}", e.replace('_', " "))));
    }
    let scope = param("scope").unwrap_or_default();
    if !scope.contains("activity:write") || !scope.contains("activity:read") {
        return Some(Err(anyhow!("watty needs permission to read and upload activities, keep both boxes checked")));
    }
    Some(param("code").map(String::from).ok_or_else(|| anyhow!("no code in Strava's answer")))
}

/// Forgets the account here and revokes watty's access on Strava.
pub async fn disconnect() -> Result<()> {
    if let Some(t) = load_tokens() {
        let _ = http().post("https://www.strava.com/oauth/deauthorize").form(&[("access_token", t.access_token)]).send().await;
    }
    match std::fs::remove_file(tokens_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// An authorized API client.
pub struct Client {
    http: reqwest::Client,
    token: String,
}

impl Client {
    /// Refreshes the access token when it's about to expire.
    pub async fn load(cfg: &Config) -> Result<Self> {
        let mut t = load_tokens().ok_or_else(|| anyhow!("not connected to Strava: connect in Settings"))?;
        if t.expires_at - chrono::Utc::now().timestamp() < 300 {
            let r = token_request(&credentials(cfg)?, &[("refresh_token", &t.refresh_token), ("grant_type", "refresh_token")]).await?;
            t = Tokens { access_token: r.access_token, refresh_token: r.refresh_token, expires_at: r.expires_at, ..t };
            save_tokens(&t)?;
        }
        Ok(Self { http: http(), token: t.access_token })
    }

    /// Uploads a ride's TCX, waits until Strava has processed it and saves the activity id with the ride.
    pub async fn upload(&self, ride: &RideFile, cfg: &Config) -> Result<u64> {
        let (_, samples) = read_journal(&ride.csv)?;
        if samples.is_empty() {
            bail!("nothing recorded in this ride");
        }
        let tcx = ride.csv.with_extension("tcx");
        if !tcx.exists() {
            recorder::export(&ride.csv, None)?;
        }
        let file = reqwest::multipart::Part::bytes(std::fs::read(&tcx)?).file_name(file_name(&tcx));
        let form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("data_type", "tcx")
            .text("name", ride.name())
            .text("trainer", "1")
            .text("external_id", file_name(&ride.csv));
        let resp = self.http.post(format!("{API}/uploads")).bearer_auth(&self.token).multipart(form).send().await?;
        let mut status: UploadStatus = check(resp).await?.json().await?;
        let mut polls = 0;
        let id = loop {
            if let Some(result) = status.result() {
                break result?;
            }
            polls += 1;
            if polls > 30 {
                bail!("Strava is still processing the upload, check it on strava.com later");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let resp = self.http.get(format!("{API}/uploads/{}", status.id)).bearer_auth(&self.token).send().await?;
            status = check(resp).await?.json().await?;
        };
        let meta = ride.meta.clone().unwrap_or_else(|| RideMeta {
            workout: ride.name(),
            workout_path: None,
            ftp: cfg.ftp.unwrap_or(0),
            intensity: 1.0,
            finished: true,
            closed: true,
            strava_id: None,
        });
        save_meta(&ride.csv, &RideMeta { strava_id: Some(id), ..meta })?;
        Ok(id)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

#[derive(Debug, Deserialize)]
struct UploadStatus {
    id: u64,
    error: Option<String>,
    activity_id: Option<u64>,
}

impl UploadStatus {
    /// `None` while Strava is still processing. A duplicate counts as uploaded.
    fn result(&self) -> Option<Result<u64>> {
        if let Some(id) = self.activity_id {
            return Some(Ok(id));
        }
        let error = self.error.as_deref()?;
        let duplicate = error.split_once("duplicate of").and_then(|(_, rest)| {
            let digits: String = rest.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
        Some(duplicate.ok_or_else(|| anyhow!("Strava rejected the upload: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_gives_the_code() {
        let line = "GET /?state=abc&code=1f2e3d&scope=read,activity:write,activity:read_all HTTP/1.1";
        assert_eq!(redirect_result(line, "abc").unwrap().unwrap(), "1f2e3d");
        assert!(redirect_result(line, "other").is_none());
        assert!(redirect_result("GET /favicon.ico HTTP/1.1", "abc").is_none());
        assert!(redirect_result("GET /?state=abc&error=access_denied HTTP/1.1", "abc").unwrap().is_err());
        assert!(redirect_result("GET /?state=abc&code=1f&scope=read,activity:read_all HTTP/1.1", "abc").unwrap().is_err());
    }

    #[test]
    fn upload_status() {
        let parse = |json: &str| serde_json::from_str::<UploadStatus>(json).unwrap().result().map(|r| r.map_err(|e| e.to_string()));
        assert_eq!(parse(r#"{"id":1,"error":null,"status":"Your activity is still being processed.","activity_id":null}"#), None);
        assert_eq!(parse(r#"{"id":1,"error":null,"activity_id":42}"#), Some(Ok(42)));
        assert_eq!(parse(r#"{"id":1,"error":"x.tcx duplicate of activity 123456789","activity_id":null}"#), Some(Ok(123456789)));
        assert_eq!(parse(r#"{"id":1,"error":"x.tcx duplicate of <a href='/activities/987'>Ride</a>","activity_id":null}"#), Some(Ok(987)));
        assert!(matches!(parse(r#"{"id":1,"error":"Time information is missing","activity_id":null}"#), Some(Err(_))));
    }
}
