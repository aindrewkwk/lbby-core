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

fn normalize_mod_info(mut info: crate::app_state::ModInfo) -> crate::app_state::ModInfo {
    if info.display_name.trim().is_empty() {
        info.display_name = info.file_name.trim_end_matches(".jar").trim_end_matches(".tmod").to_string();
    }
    info.version = info.version.trim().to_string();
    info.authors = info
        .authors
        .into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty() && !a.starts_with("${"))
        .collect();
    info
}

fn read_fabric_mod_info<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> Option<crate::app_state::ModInfo> {
    let text = read_zip_text(zip, "fabric.mod.json")?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let display_name = {
        let name = json_string(&value, "name");
        if name.is_empty() { json_string(&value, "id") } else { name }
    };
    let version = json_string(&value, "version");
    let description = json_string(&value, "description");
    let authors = match value.get("authors") {
        Some(serde_json::Value::Array(items)) => items.iter().filter_map(|item| {
            if let Some(s) = item.as_str() {
                Some(s.to_string())
            } else {
                item.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())
            }
        }).collect(),
        _ => Vec::new(),
    };
    let icon_path = match value.get("icon") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Object(map)) => map
            .get("64")
            .or_else(|| map.values().next())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    };
    let icon_data_url = icon_path.as_deref().and_then(|p| read_zip_icon_data_url(zip, p));
    Some(crate::app_state::ModInfo {
        file_name: file_name.to_string(),
        display_name: if display_name.is_empty() { file_name.trim_end_matches(".jar").trim_end_matches(".tmod").to_string() } else { display_name },
        version,
        authors,
        description,
        icon_data_url,
    })
}

fn read_forge_mod_info<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> Option<crate::app_state::ModInfo> {
    let text = read_zip_text(zip, "META-INF/mods.toml")
        .or_else(|| read_zip_text(zip, "mods.toml"))?;
    let value: toml::Value = text.parse().ok()?;
    let mods = value.get("mods")?.as_array()?;
    let first = mods.first()?;
    let display_name = toml_string(first, "displayName");
    let mod_id = toml_string(first, "modId");
    let raw_version = toml_string(first, "version");
    let version = if raw_version.starts_with("${") { String::new() } else { raw_version };
    let authors_raw = toml_string(first, "authors");
    let authors = authors_raw
        .split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let description = toml_string(first, "description");
    let icon_path = toml_string(first, "logoFile");
    let icon_data_url = read_zip_icon_data_url(zip, &icon_path);
    Some(crate::app_state::ModInfo {
        file_name: file_name.to_string(),
        display_name: if display_name.is_empty() { mod_id } else { display_name },
        version,
        authors,
        description,
        icon_data_url,
    })
}

/// Read mod info from a JAR/ZIP file. Always returns a ModInfo — falls back
/// to the filename if no metadata can be extracted.
pub fn read_mod_info(path: &std::path::Path) -> crate::app_state::ModInfo {
    let file_name = path
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let fallback = crate::app_state::ModInfo {
        display_name: file_name.trim_end_matches(".jar").trim_end_matches(".tmod").to_string(),
        file_name: file_name.clone(),
        version: String::new(),
        authors: Vec::new(),
        description: String::new(),
        icon_data_url: None,
    };

    let Ok(file) = std::fs::File::open(path) else { return fallback; };
    let Ok(mut zip) = zip::ZipArchive::new(file) else { return fallback; };

    if let Some(info) = read_fabric_mod_info(&mut zip, &file_name) {
        return normalize_mod_info(info);
    }
    if let Some(info) = read_forge_mod_info(&mut zip, &file_name) {
        return normalize_mod_info(info);
    }
    fallback
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

/// Install a game server — delegates to the full implementation in server.rs.
pub async fn do_install_server(app: Arc<AppEventSender>, cfg: ServerConfig) -> Result<ServerConfig, String> {
    crate::server::do_install_server(app, cfg).await
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
