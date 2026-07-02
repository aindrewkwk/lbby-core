use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Truncate a string to at most `max` characters (safe for UTF-8).
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { return s; }
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Where playit-cli stores the agent secret on this OS
pub fn secret_file_path() -> PathBuf {
    let lbby_secret = lbby_secret_file_path();
    if lbby_secret.exists() {
        return lbby_secret;
    }

    #[cfg(target_os = "macos")]
    return dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("playit_gg")
        .join("playit.toml");
    #[cfg(target_os = "linux")]
    return dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("playit_gg")
        .join("playit.toml");
    #[cfg(target_os = "windows")]
    {
        // The playit.gg agent may store its config in several locations
        // depending on the version. Check all known locations.
        let candidates: Vec<PathBuf> = [
            dirs::config_dir().map(|p| p.join("playit_gg").join("playit.toml")),       // AppData\Roaming
            dirs::data_local_dir().map(|p| p.join("playit_gg").join("playit.toml")),    // AppData\Local
            dirs::home_dir().map(|p| p.join(".playit_gg").join("playit.toml")),          // ~\.playit_gg
        ].into_iter().flatten().collect();

        for candidate in &candidates {
            if candidate.exists() {
                return candidate.clone();
            }
        }
        // Default to the Lbby-managed path if none exist yet because
        // start_playit passes this path to the agent with --secret-path.
        lbby_secret
    }
}

/// Return all possible secret file paths for this OS.
/// Used during reset to ensure we delete secrets from every known location.
pub fn all_secret_file_paths() -> Vec<PathBuf> {
    let mut paths = vec![lbby_secret_file_path()];
    #[cfg(target_os = "macos")]
    if let Some(dir) = dirs::data_dir() {
        paths.push(dir.join("playit_gg").join("playit.toml"));
    }
    #[cfg(target_os = "linux")]
    if let Some(dir) = dirs::config_dir() {
        paths.push(dir.join("playit_gg").join("playit.toml"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(dir) = dirs::config_dir() {
            paths.push(dir.join("playit_gg").join("playit.toml"));       // AppData\Roaming
        }
        if let Some(dir) = dirs::data_local_dir() {
            paths.push(dir.join("playit_gg").join("playit.toml"));       // AppData\Local
        }
        if let Some(dir) = dirs::home_dir() {
            paths.push(dir.join(".playit_gg").join("playit.toml"));      // ~\.playit_gg
        }
    }
    paths
}

/// Read the agent secret key from the playit.toml file
pub fn read_secret() -> Option<String> {
    for path in all_secret_file_paths() {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for line in content.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("secret_key") {
                let after_eq = rest.split_once('=')?.1.trim();
                let key = after_eq.trim_matches(|c| c == '"' || c == '\'').to_string();
                if !key.is_empty() { return Some(key); }
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    status: String,
    data: serde_json::Value,
}

fn tunnel_address_from_value(tunnel: &serde_json::Value) -> Option<String> {
    let domain = tunnel.get("custom_domain")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| tunnel.get("assigned_domain").and_then(|v| v.as_str()))
        .or_else(|| tunnel.get("domain").and_then(|v| v.as_str()))
        .or_else(|| tunnel.get("hostname").and_then(|v| v.as_str()))
        .or_else(|| tunnel.get("host").and_then(|v| v.as_str()))?;

    // Some responses already include host:port in the domain field.
    if domain.contains(':') {
        return Some(domain.to_string());
    }

    let port = tunnel.get("port")
        .and_then(|v| v.get("from").or_else(|| v.get("public")).or_else(|| v.get("port")))
        .and_then(|v| v.as_u64())
        .or_else(|| tunnel.get("from").and_then(|v| v.as_u64()))
        .or_else(|| tunnel.get("public_port").and_then(|v| v.as_u64()))
        .or_else(|| tunnel.get("port").and_then(|v| v.as_u64()));

    match port {
        Some(p) => Some(format!("{}:{}", domain, p)),
        None => Some(domain.to_string()),
    }
}

/// Which game's tunnel to look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelGame {
    Minecraft,
    Terraria,
}

/// Query the playit API for the current agent's tunnels and return the first
/// matching tunnel address as "host:port".
/// - For Minecraft: prefers `tunnel_type == "minecraft-java"`, falls back to first tunnel.
/// - For Terraria: looks for tunnels targeting local port 7777, or `terraria` type.
///
/// Returns Err with a diagnostic message if anything goes wrong.
pub async fn query_tunnel_address_for_game(game: TunnelGame) -> Result<String, String> {
    let secret = read_secret().ok_or_else(|| {
        let paths = all_secret_file_paths()
            .into_iter()
            .map(|p| format!("{} (exists: {})", p.display(), p.exists()))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "No secret key found. Checked playit.toml paths: {}",
            paths
        )
    })?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let resp = client
        .post("https://api.playit.gg/agents/rundata")
        .header("Authorization", format!("agent-key {}", secret))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| format!("API request failed: {}", e))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("API returned HTTP {}: {}", status, truncate(&body, 200)));
    }

    let parsed: ApiResponse = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse API response: {} (body: {})", e, truncate(&body, 200)))?;

    if parsed.status != "success" {
        let data_str = parsed.data.to_string();
        return Err(format!("API status not success: {} (data: {})", parsed.status, truncate(&data_str, 200)));
    }

    let tunnels = parsed.data.get("tunnels")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("No 'tunnels' array in response. Data keys: {:?}", parsed.data.as_object().map(|o| o.keys().collect::<Vec<_>>())))?;

    if tunnels.is_empty() {
        return Err("API returned empty tunnels array — no tunnels configured yet".to_string());
    }

    // Find the right tunnel based on game type
    let preferred = match game {
        TunnelGame::Minecraft => {
            // Prefer minecraft-java tunnel type, fallback to first
            tunnels.iter().find(|t| {
                t.get("tunnel_type").and_then(|v| v.as_str()) == Some("minecraft-java")
            })
        }
        TunnelGame::Terraria => {
            // Look for tunnels targeting local port 7777 (Terraria default)
            // or with "terraria" in the tunnel type
            tunnels.iter().find(|t| {
                let tunnel_type = t.get("tunnel_type").and_then(|v| v.as_str()).unwrap_or("");
                if tunnel_type.contains("terraria") {
                    return true;
                }
                // Check if local port is 7777
                let local_port = t.get("port")
                    .and_then(|v| v.get("local").or_else(|| v.get("to")))
                    .and_then(|v| v.as_u64())
                    .or_else(|| t.get("local_port").and_then(|v| v.as_u64()));
                local_port == Some(7777)
            })
        }
    };

    if let Some(addr) = preferred.and_then(tunnel_address_from_value) {
        return Ok(addr);
    }

    // Fallback: return first tunnel address
    tunnels.first().and_then(tunnel_address_from_value)
        .ok_or_else(|| {
            let first = tunnels[0].to_string();
            match game {
                TunnelGame::Terraria => {
                    format!("No Terraria tunnel found (port 7777). Create one at https://playit.gg/dashboard → Add Tunnel → set local port to 7777. First tunnel: {}", truncate(&first, 300))
                }
                TunnelGame::Minecraft => {
                    format!("Tunnels exist but could not extract address. First tunnel: {}", truncate(&first, 300))
                }
            }
        })
}

/// Query the playit API for the current agent's tunnels and return the first
/// Minecraft tunnel address as "host:port" (legacy compatibility).
pub async fn query_tunnel_address() -> Result<String, String> {
    query_tunnel_address_for_game(TunnelGame::Minecraft).await
}

/// Check if a Terraria tunnel exists in the playit.gg configuration.
pub async fn has_terraria_tunnel() -> bool {
    let Some(secret) = read_secret() else { return false };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Ok(resp) = client
        .post("https://api.playit.gg/agents/rundata")
        .header("Authorization", format!("agent-key {}", secret))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await else { return false };
    let Ok(body) = resp.text().await else { return false };
    let Ok(parsed) = serde_json::from_str::<ApiResponse>(&body) else { return false };
    let Some(tunnels) = parsed.data.get("tunnels").and_then(|v| v.as_array()) else { return false };

    tunnels.iter().any(|t| {
        let tunnel_type = t.get("tunnel_type").and_then(|v| v.as_str()).unwrap_or("");
        if tunnel_type.contains("terraria") {
            return true;
        }
        let local_port = t.get("port")
            .and_then(|v| v.get("local").or_else(|| v.get("to")))
            .and_then(|v| v.as_u64())
            .or_else(|| t.get("local_port").and_then(|v| v.as_u64()));
        local_port == Some(7777)
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayitState {
    pub running: bool,
    pub address: Option<String>,
    pub claim_url: Option<String>,
    pub pid: Option<u32>,
}

impl PlayitState {
    pub fn new() -> Self {
        Self { running: false, address: None, claim_url: None, pid: None }
    }
}

impl Default for PlayitState {
    fn default() -> Self {
        Self::new()
    }
}

/// Stop the running playit.gg process and update shared state.
pub async fn stop(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    let state = app.state();
    let mut pl = state.playit.lock().await;

    if !pl.running {
        return Ok(());
    }

    if let Some(pid) = pl.pid {
        #[cfg(unix)]
        unsafe { libc::kill(pid as i32, libc::SIGTERM); }
        #[cfg(windows)]
        {
            let mut cmd = tokio::process::Command::new("taskkill");
            cmd.args(["/PID", &pid.to_string(), "/F"]);
            crate::helpers::hide_child_window(&mut cmd);
            cmd.output().await.ok();
        }
    }

    pl.running = false;
    pl.pid = None;
    pl.address = None;

    let snapshot = pl.clone();
    drop(pl);

    let _ = app.emit("playit-status", &snapshot);

    Ok(())
}

pub fn playit_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby")
        .join("playit");
    std::fs::create_dir_all(&base).ok();
    base
}

/// Lbby-managed playit secret file. Passing this to playit with
/// --secret-path makes setup deterministic on Windows.
pub fn lbby_secret_file_path() -> PathBuf {
    playit_dir().join("playit.toml")
}

pub fn playit_cached_binary() -> PathBuf {
    #[cfg(target_os = "windows")]
    return playit_dir().join("playit.exe");
    #[cfg(not(target_os = "windows"))]
    return playit_dir().join("playit");
}

/// The v1 Windows "playit" binary is a daemon that waits for IPC secret
/// provisioning and does not print a claim URL. Lbby needs the CLI flow that
/// supports `claim` and `--stdout`, so reject daemon-only binaries.
pub fn is_supported_agent_cli(path: &PathBuf) -> bool {
    let mut cmd = std::process::Command::new(path);
    cmd.arg("--help");
    crate::helpers::hide_std_child_window(&mut cmd);
    let Ok(out) = cmd.output() else { return false };
    if !out.status.success() {
        return false;
    }
    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    help.contains(" claim")
        && help.contains("--secret_path")
        && help.contains("--stdout")
}

/// Try to find an existing playit install on PATH or common locations.
pub fn find_existing_playit() -> Option<PathBuf> {
    let names = ["playit-cli", "playit"];
    let mut v: Vec<PathBuf> = vec![];

    // Try `which <name>` for each
    for name in &names {
        let mut cmd = std::process::Command::new(if cfg!(target_os = "windows") { "where" } else { "which" });
        cmd.arg(name);
        crate::helpers::hide_std_child_window(&mut cmd);
        if let Ok(out) = cmd.output() {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                v.extend(stdout.lines().map(|l| PathBuf::from(l.trim())).filter(|p| !p.as_os_str().is_empty()));
            }
        }
    }

    // Cargo install location (any platform)
    if let Some(home) = dirs::home_dir() {
        for name in &names {
            v.push(home.join(".cargo/bin").join(name));
        }
    }

    #[cfg(target_os = "macos")]
    for name in &names {
        v.push(PathBuf::from("/opt/homebrew/bin").join(name));
        v.push(PathBuf::from("/usr/local/bin").join(name));
        v.push(PathBuf::from(format!("/Applications/playit.app/Contents/MacOS/{}", name)));
    }

    #[cfg(target_os = "windows")]
    for name in &names {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            v.push(PathBuf::from(&local).join("playit").join(format!("{}.exe", name)));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            v.push(PathBuf::from(&pf).join("playit").join(format!("{}.exe", name)));
        }
    }

    #[cfg(target_os = "linux")]
    for name in &names {
        v.push(PathBuf::from("/usr/bin").join(name));
        v.push(PathBuf::from("/usr/local/bin").join(name));
    }

    v.into_iter()
        .find(|path| path.exists() && is_supported_agent_cli(path))
}

/// Download URL for the GitHub release binary, or None if no direct download available
/// (macOS — install via Homebrew instead).
pub fn playit_download_url() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Some("https://github.com/playit-cloud/playit-agent/releases/download/v0.17.1/playit-linux-aarch64");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Some("https://github.com/playit-cloud/playit-agent/releases/download/v0.17.1/playit-linux-amd64");
    #[cfg(target_os = "windows")]
    return Some("https://github.com/playit-cloud/playit-agent/releases/download/v0.17.1/playit-windows-x86_64-signed.exe");
    #[cfg(target_os = "macos")]
    return None;
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    return None;
}

/// Attempt to create a Terraria tunnel (TCP on port 7777) via the playit.gg API.
///
/// Returns Ok with a status message on success, or Err with a diagnostic message.
/// If the API doesn't support tunnel creation, returns a message telling the user
/// to create it manually.
pub async fn create_terraria_tunnel() -> Result<String, String> {
    let secret = read_secret().ok_or_else(|| {
        "No playit.gg secret key found. Start playit.gg first.".to_string()
    })?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // Try to create a tunnel via the playit.gg API.
    // The API endpoint for creating tunnels from an agent.
    let resp = client
        .post("https://api.playit.gg/agents/tunnels")
        .header("Authorization", format!("agent-key {}", secret))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "tunnel_type": "minecraft-java",
            "port": {
                "local": 7777
            },
            "protocol": "tcp"
        }))
        .send()
        .await
        .map_err(|e| format!("API request failed: {}", e))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status.is_success() {
        // Verify the tunnel was created
        if has_terraria_tunnel().await {
            return Ok("Terraria tunnel created successfully on port 7777.".to_string());
        }
        return Ok(format!(
            "Tunnel creation request sent. If it doesn't appear, create one manually at https://playit.gg/dashboard → Add Tunnel → TCP → local port 7777."
        ));
    }

    // API might not support direct tunnel creation — provide helpful fallback
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return Err(format!(
            "The playit.gg API doesn't support automatic tunnel creation from here. \
             Please create a tunnel manually:\n\
             1. Go to https://playit.gg/dashboard\n\
             2. Click Add Tunnel → TCP\n\
             3. Set local port to 7777\n\
             4. Save and restart playit.gg from the Connection tab"
        ));
    }

    Err(format!(
        "Failed to create tunnel (HTTP {}): {}. Create one manually at https://playit.gg/dashboard → Add Tunnel → TCP → local port 7777.",
        status,
        truncate(&body, 200)
    ))
}

/// Build playit from source via cargo. Slow (3-5 min first time) but works on any platform with Rust.
/// Used as fallback on macOS where there's no prebuilt binary.
pub async fn cargo_install_playit() -> Result<PathBuf, String> {
    // Verify cargo is available
    let mut cargo_check_cmd = tokio::process::Command::new(if cfg!(target_os = "windows") { "where" } else { "which" });
    cargo_check_cmd.arg("cargo");
    crate::helpers::hide_child_window(&mut cargo_check_cmd);
    let cargo_check = cargo_check_cmd.output().await
        .map_err(|e| e.to_string())?;
    if !cargo_check.status.success() {
        return Err(
            "Rust/Cargo not found. Install from https://rustup.rs/ first, or download playit manually from https://playit.gg/download"
                .to_string(),
        );
    }

    let mut cargo_cmd = tokio::process::Command::new("cargo");
    cargo_cmd.args([
            "install",
            "--git", "https://github.com/playit-cloud/playit-agent.git",
            "--locked",
            "playit-cli",
        ]);
    crate::helpers::hide_child_window(&mut cargo_cmd);
    let out = cargo_cmd.output().await
        .map_err(|e| format!("cargo install failed to launch: {}", e))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect::<Vec<_>>();
        let mut tail = last;
        tail.reverse();
        return Err(format!("cargo install failed:\n{}", tail.join("\n")));
    }

    find_existing_playit()
        .ok_or_else(|| "playit-cli built but binary not found at ~/.cargo/bin/playit-cli".to_string())
}
