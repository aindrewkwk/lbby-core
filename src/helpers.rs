// Helper functions and types referenced by modules.
// These were originally in the monolithic lib.rs — extracted here for reuse.

use base64::Engine;
use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use crate::app_state::AppEventSender;
use crate::config::ServerConfig;

/// Progress event for server installation / mod installation.
#[derive(Debug, Clone, Serialize)]
pub struct InstallProgress {
    pub stage: String,
    pub label: String,
    pub current: u32,
    pub total: u32,
}

/// Platform-specific: hide child process window on Windows.
#[cfg(target_os = "windows")]
pub fn hide_child_window(cmd: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
pub fn hide_child_window(_cmd: &mut tokio::process::Command) {}

/// Platform-specific: hide std child process window on Windows.
#[cfg(target_os = "windows")]
pub fn hide_std_child_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000);
}

#[cfg(not(target_os = "windows"))]
pub fn hide_std_child_window(_cmd: &mut std::process::Command) {}

/// Default server path value — used by config and mod_services.
pub fn default_server_path_value(game: Option<&str>) -> String {
    let folder = match game.unwrap_or("minecraft") {
        "terraria" => "terraria-server",
        _ => "minecraft-server",
    };
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(folder)
        .to_string_lossy()
        .to_string()
}

/// Read mod info from a JAR/ZIP file.
pub fn read_mod_info(path: &std::path::Path) -> Option<crate::app_state::ModInfo> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    // Try fabric.mod.json first
    if let Ok(mut f) = archive.by_name("fabric.mod.json") {
        let mut contents = String::new();
        f.read_to_string(&mut contents).ok()?;
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
            let name = json["name"].as_str().unwrap_or("").to_string();
            let version = json["version"].as_str().unwrap_or("").to_string();
            let authors = json["authors"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let description = json["description"].as_str().unwrap_or("").to_string();
            return Some(crate::app_state::ModInfo {
                file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                display_name: name,
                version,
                authors,
                description,
                icon_data_url: None,
            });
        }
    }

    // Try mods.toml (Forge)
    if let Ok(mut f) = archive.by_name("META-INF/mods.toml") {
        let mut contents = String::new();
        f.read_to_string(&mut contents).ok()?;
        if let Ok(toml) = contents.parse::<toml::Value>() {
            let mods = toml.get("mods").and_then(|m| m.as_array());
            if let Some(mods) = mods {
                if let Some(first) = mods.first() {
                    let name = first.get("displayName").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let version = first.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let description = first.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    return Some(crate::app_state::ModInfo {
                        file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                        display_name: name,
                        version,
                        authors: vec![],
                        description,
                        icon_data_url: None,
                    });
                }
            }
        }
    }

    None
}

/// Stub: start server — to be implemented by agent/app.
/// The agent calls this, the app calls this, both delegate to the same logic.
pub async fn do_start_server(app: Arc<AppEventSender>) -> Result<(), String> {
    crate::server::start_server(app).await
}

/// Stub: pregenerate chunks.
pub async fn do_pregenerate_chunks(app: Arc<AppEventSender>, total_chunks: u32) -> Result<(), String> {
    // TODO: implement chunk pre-generation
    let _ = (app, total_chunks);
    Err("Chunk pre-generation not yet implemented in lbby-core".to_string())
}

/// Stub: kill server and playit for remote control.
pub async fn remote_kill_server_and_playit(app: &Arc<AppEventSender>) {
    let _ = crate::server::stop_server(app.clone()).await;
    let _ = crate::playit::stop(app.clone()).await;
}

/// Stub: install server — full implementation to be migrated from monolithic lib.rs.
pub async fn do_install_server(app: Arc<AppEventSender>, cfg: ServerConfig) -> Result<ServerConfig, String> {
    let _ = (app, cfg);
    Err("Server installation not yet implemented in lbby-core".to_string())
}

// ── Generic helpers (migrated from lbby-agent/src/lib.rs) ────────────────────

pub fn is_private_or_local_host(host: &str) -> bool {
    let h = host
        .trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if matches!(h.as_str(), "localhost" | "::1") {
        return true;
    }

    let octets = h
        .split('.')
        .map(str::parse::<u8>)
        .collect::<Result<Vec<_>, _>>();

    match octets.as_deref() {
        Ok([10, ..]) => true,
        Ok([127, ..]) => true,
        Ok([169, 254, ..]) => true,
        Ok([172, second, ..]) if (16..=31).contains(second) => true,
        Ok([192, 168, ..]) => true,
        _ => false,
    }
}

pub fn is_public_tunnel_address(addr: &str) -> bool {
    let Some(idx) = addr.rfind(':') else {
        return addr.contains("playit.gg") || addr.contains("playit.cloud");
    };
    let host = &addr[..idx];
    !is_private_or_local_host(host)
}

/// Check whether a TCP port is available for binding.
/// Returns Ok(()) if the port is free, Err with a clear message if in use.
pub fn check_port_available(port: u16) -> Result<(), String> {
    match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "Port {} is already in use ({}). Close the other application or change the port in Settings.",
            port, e
        )),
    }
}

/// Strip ANSI escape codes (VT100/CSI sequences) from a string.
/// On Windows, playit may emit ANSI sequences that corrupt URLs.
pub fn strip_ansi_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC [ ... m  (CSI sequence) or ESC ] ... BEL (OSC sequence)
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == 'm' { break; }
                }
            } else if chars.peek() == Some(&']') {
                chars.next(); // consume ']'
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '\x07' || next == '\x1b' { break; }
                }
            }
            // Other ESC sequences — just skip the ESC char
        } else {
            out.push(c);
        }
    }
    out
}

pub fn is_valid_player_name(s: &str) -> bool {
    let len = s.chars().count();
    (2..=16).contains(&len) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check if a process with the given PID is still alive.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use std::process::Command;
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                // tasklist returns "INFO: No tasks are running..." if not found
                text.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    {
        // kill(pid, 0) checks if the process exists without sending a signal
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

pub fn default_downloads_dir() -> String {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .to_string()
}

pub fn json_string(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

pub fn toml_string(value: &toml::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

pub fn read_zip_text<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let mut file = zip.by_name(name).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

pub fn read_zip_icon_data_url<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    path: &str,
) -> Option<String> {
    let clean = path.trim().trim_start_matches('/');
    if clean.is_empty() { return None; }
    let mut file = zip.by_name(clean).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() || bytes.len() > 256 * 1024 { return None; }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else {
        return None;
    };
    Some(format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}
