use crate::{config, remote};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareTunnelState {
    pub running: bool,
    pub url: Option<String>,
    pub pid: Option<u32>,
    pub message: String,
}

impl Default for CloudflareTunnelState {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudflareTunnelState {
    pub fn new() -> Self {
        Self {
            running: false,
            url: None,
            pid: None,
            message: String::new(),
        }
    }
}

pub fn cloudflared_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby")
        .join("cloudflared");
    std::fs::create_dir_all(&base).ok();
    base
}

pub fn cloudflared_cached_binary() -> PathBuf {
    #[cfg(target_os = "windows")]
    return cloudflared_dir().join("cloudflared.exe");
    #[cfg(not(target_os = "windows"))]
    return cloudflared_dir().join("cloudflared");
}

pub fn find_existing_cloudflared() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(cloudflared_cached_binary());

    let mut which = std::process::Command::new(if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    });
    which.arg("cloudflared");
    crate::helpers::hide_std_child_window(&mut which);
    if let Ok(out) = which.output() {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            candidates.extend(stdout.lines().map(|line| PathBuf::from(line.trim())));
        }
    }

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/opt/homebrew/bin/cloudflared"));
        candidates.push(PathBuf::from("/usr/local/bin/cloudflared"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join("cloudflared").join("cloudflared.exe"));
        }
    }

    candidates.into_iter().find(|path| path.exists())
}

fn download_url() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Some("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-arm64.tgz");
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Some("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-amd64.tgz");
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Some("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64");
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Some("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64");
    #[cfg(not(any(
        all(target_os = "macos", any(target_arch = "aarch64", target_arch = "x86_64")),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"))
    )))]
    return None;
}

pub async fn ensure_cloudflared() -> Result<PathBuf, String> {
    if let Some(path) = find_existing_cloudflared() {
        return Ok(path);
    }

    let url = download_url().ok_or_else(|| {
        "No bundled cloudflared download is available for this platform.".to_string()
    })?;
    let dest = cloudflared_cached_binary();
    let tmp = if url.ends_with(".tgz") {
        cloudflared_dir().join("cloudflared.tgz")
    } else {
        dest.clone()
    };

    let bytes = reqwest::Client::builder()
        .user_agent("Lbby")
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to download cloudflared: {}", e))?
        .error_for_status()
        .map_err(|e| format!("Failed to download cloudflared: {}", e))?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(&tmp, bytes).await.map_err(|e| e.to_string())?;

    if url.ends_with(".tgz") {
        extract_cloudflared_tgz(&tmp, &dest)?;
        tokio::fs::remove_file(&tmp).await.ok();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).map_err(|e| e.to_string())?;
    }

    Ok(dest)
}

fn extract_cloudflared_tgz(tgz: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(tgz).map_err(|e| e.to_string())?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?;
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == "cloudflared" {
            entry.unpack(dest).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("Downloaded cloudflared archive did not contain a binary.".to_string())
}

/// Kill any orphan `cloudflared tunnel --url http://localhost:<port>...`
/// processes that aren't tracked by the current Lbby instance. These pile
/// up when Lbby is force-quit or crashes mid-tunnel without a chance to
/// run its own stop_quick_tunnel.
async fn kill_stray_cloudflareds(local_port: u16) {
    #[cfg(unix)]
    {
        let pattern = format!("cloudflared.*localhost:{}", local_port);
        let out = match std::process::Command::new("pgrep").args(["-f", &pattern]).output() {
            Ok(o) if o.status.success() => o,
            _ => return,
        };
        let mut killed = 0u32;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                killed += 1;
            }
        }
        if killed > 0 {
            eprintln!("[lbby] killed {} stray cloudflared process(es)", killed);
        }
    }
    #[cfg(windows)]
    {
        // On Windows we can't easily filter by command-line; users hitting
        // this issue can use Task Manager. Most Windows users won't accumulate
        // zombies because the close-confirm flow runs reliably there.
        let _ = local_port;
    }
}

pub async fn start_quick_tunnel(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<CloudflareTunnelState, String> {
    remote::sync(&app).await?;
    let cfg = config::load_config();
    if !cfg.remote_control_enabled {
        return Err("Enable Remote Control and save settings first.".to_string());
    }

    // Stop the tunnel we currently track (if any).
    stop_quick_tunnel(app.clone()).await.ok();
    // Also kill any *untracked* cloudflareds left behind by previous Lbby
    // sessions that crashed or force-quit before they could clean up. Without
    // this, every app launch piles up another zombie cloudflared bound to
    // localhost:<remote_port>, eventually flooding the system.
    kill_stray_cloudflareds(cfg.remote_control_port).await;

    let cloudflared = ensure_cloudflared().await?;
    let local_url = format!("http://localhost:{}", cfg.remote_control_port);
    let mut cmd = tokio::process::Command::new(cloudflared);
    cmd.args(["tunnel", "--url", &local_url, "--no-autoupdate"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::helpers::hide_child_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start Cloudflare tunnel: {}", e))?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    {
        let state = app.state();
        let mut tunnel = state.cloudflare_remote.lock().await;
        tunnel.running = true;
        tunnel.pid = pid;
        tunnel.url = None;
        tunnel.message = "Starting Cloudflare tunnel...".to_string();
        app.emit("cloudflare-remote-update", tunnel.clone()).ok();
    }

    if let Some(stdout) = stdout {
        spawn_cloudflared_reader(app.clone(), stdout, tx.clone());
    }
    if let Some(stderr) = stderr {
        spawn_cloudflared_reader(app.clone(), stderr, tx.clone());
    }

    let app_wait = app.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        let state = app_wait.state();
        let mut tunnel = state.cloudflare_remote.lock().await;
        tunnel.running = false;
        tunnel.pid = None;
        match status {
            Ok(s) if !s.success() => {
                let code = s.code().unwrap_or(-1);
                tunnel.message = format!("Cloudflare tunnel exited with code {}.", code);
                eprintln!("[lbby] cloudflared exited with code {}", code);
            }
            Ok(_) => {
                tunnel.message = "Cloudflare tunnel stopped.".to_string();
            }
            Err(e) => {
                tunnel.message = format!("Cloudflare tunnel error: {}", e);
                eprintln!("[lbby] cloudflared wait error: {}", e);
            }
        }
        app_wait.emit("cloudflare-remote-update", tunnel.clone()).ok();
    });

    let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(35));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => {
                let state = app.state();
                let tunnel = state.cloudflare_remote.lock().await.clone();
                if tunnel.running {
                    return Ok(tunnel);
                }
                return Err("Cloudflare tunnel stopped before a public URL was available.".to_string());
            }
            maybe_line = rx.recv() => {
                let Some(line) = maybe_line else { continue; };
                if let Some(url) = parse_trycloudflare_url(&line) {
                    // Persist to disk is best-effort — the URL is already in
                    // memory (CloudflareTunnelState). A disk write failure
                    // (permissions, disk full) must not abort the tunnel.
                    if let Err(e) = save_public_url(&url) {
                        eprintln!("[lbby] could not save Cloudflare URL to config: {}", e);
                    }
                    let state = app.state();
                    let mut tunnel = state.cloudflare_remote.lock().await;
                    tunnel.running = true;
                    tunnel.url = Some(url.clone());
                    tunnel.message = "Cloudflare tunnel ready.".to_string();
                    app.emit("cloudflare-remote-update", tunnel.clone()).ok();
                    return Ok(tunnel.clone());
                }
            }
        }
    }
}

pub async fn stop_quick_tunnel(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<CloudflareTunnelState, String> {
    let state = app.state();
    let mut tunnel = state.cloudflare_remote.lock().await;
    if let Some(pid) = tunnel.pid {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        #[cfg(target_os = "windows")]
        {
            let mut cmd = tokio::process::Command::new("taskkill");
            cmd.args(["/PID", &pid.to_string(), "/F"]);
            crate::helpers::hide_child_window(&mut cmd);
            cmd.output().await.ok();
        }
    }
    tunnel.running = false;
    tunnel.pid = None;
    tunnel.url = None;
    tunnel.message = "Cloudflare tunnel stopped.".to_string();
    app.emit("cloudflare-remote-update", tunnel.clone()).ok();
    Ok(tunnel.clone())
}

pub async fn status(app: std::sync::Arc<crate::app_state::AppEventSender>) -> CloudflareTunnelState {
    app.state().cloudflare_remote.lock().await.clone()
}

fn spawn_cloudflared_reader<R>(app: std::sync::Arc<crate::app_state::AppEventSender>, stream: R, tx: mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let reader = BufReader::new(stream);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = parse_trycloudflare_url(&line) {
                save_public_url(&url).ok();
                let state = app.state();
                let mut tunnel = state.cloudflare_remote.lock().await;
                tunnel.url = Some(url);
                tunnel.message = "Cloudflare tunnel ready.".to_string();
                app.emit("cloudflare-remote-update", tunnel.clone()).ok();
            } else {
                let state = app.state();
                let mut tunnel = state.cloudflare_remote.lock().await;
                tunnel.message = line.clone();
                app.emit("cloudflare-remote-update", tunnel.clone()).ok();
            }
            tx.send(line).ok();
        }
    });
}

fn save_public_url(url: &str) -> Result<(), String> {
    let mut cfg = config::load_config();
    cfg.remote_control_public_url = url.to_string();
    config::save_config(&cfg)
}

fn parse_trycloudflare_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|part| part.starts_with("https://") && part.contains(".trycloudflare.com"))
        .map(|part| {
            part.trim_matches(|c: char| {
                matches!(c, ',' | '.' | ';' | ')' | '(' | '"' | '\'' | '`')
            })
            .to_string()
        })
}
