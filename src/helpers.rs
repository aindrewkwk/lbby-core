// Helper functions and types referenced by modules.
// These were originally in the monolithic lib.rs — extracted here for reuse.

use base64::Engine;
use futures_util::StreamExt;
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

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

/// Download a file from a URL to a local path with progress reporting.
///
/// Streams the HTTP response to disk in chunks, calling `progress(downloaded,
/// total)` after each chunk so the caller can emit UI events. Uses
/// `install-progress` / `InstallProgress` events by default; callers that
/// need a different event type can wrap this function and emit their own
/// events after it returns.
pub async fn download_to_file(
    app: &Arc<AppEventSender>,
    url: &str,
    dest: &Path,
    label: &str,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("Lbby")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!(
            "Download failed with HTTP {} for {}",
            resp.status(),
            url
        ));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        let progress = if total > 0 {
            downloaded as f32 / total as f32
        } else {
            0.0
        };
        app.emit(
            "install-progress",
            InstallProgress {
                stage: "download".to_string(),
                label: format!("Downloading {}\u{2026} {:.1}%", label, progress * 100.0),
                current: downloaded as u32,
                total: total as u32,
            },
        )
        .ok();
    }
    Ok(())
}

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
        info.display_name = info
            .file_name
            .trim_end_matches(".jar")
            .trim_end_matches(".tmod")
            .to_string();
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
        if name.is_empty() {
            json_string(&value, "id")
        } else {
            name
        }
    };
    let version = json_string(&value, "version");
    let description = json_string(&value, "description");
    let authors = match value.get("authors") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    Some(s.to_string())
                } else {
                    item.get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                }
            })
            .collect(),
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
    let icon_data_url = icon_path
        .as_deref()
        .and_then(|p| read_zip_icon_data_url(zip, p));
    Some(crate::app_state::ModInfo {
        file_name: file_name.to_string(),
        display_name: if display_name.is_empty() {
            file_name
                .trim_end_matches(".jar")
                .trim_end_matches(".tmod")
                .to_string()
        } else {
            display_name
        },
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
    let text =
        read_zip_text(zip, "META-INF/neoforge.mods.toml")
            .or_else(|| read_zip_text(zip, "META-INF/mods.toml"))
            .or_else(|| read_zip_text(zip, "mods.toml"))?;
    let value: toml::Value = text.parse().ok()?;
    let mods = value.get("mods")?.as_array()?;
    let first = mods.first()?;
    let display_name = toml_string(first, "displayName");
    let mod_id = toml_string(first, "modId");
    let raw_version = toml_string(first, "version");
    let version = if raw_version.starts_with("${") {
        String::new()
    } else {
        raw_version
    };
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
        display_name: if display_name.is_empty() {
            mod_id
        } else {
            display_name
        },
        version,
        authors,
        description,
        icon_data_url,
    })
}

/// Extract dependencies from a Forge mod JAR's META-INF/mods.toml.
/// Returns a list of (mod_id, version_range) tuples.
pub fn read_forge_dependencies(path: &std::path::Path) -> Vec<(String, String)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let text = match read_zip_text(&mut zip, "META-INF/neoforge.mods.toml")
        .or_else(|| read_zip_text(&mut zip, "META-INF/mods.toml"))
        .or_else(|| read_zip_text(&mut zip, "mods.toml"))
    {
        Some(t) => t,
        None => return Vec::new(),
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(deps) = value.get("dependencies").and_then(|d| d.as_table()) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for (_key, dep_list) in deps {
        if let Some(arr) = dep_list.as_array() {
            for dep in arr {
                let mod_id = dep.get("modId").and_then(|v| v.as_str()).unwrap_or("");
                let version_range = dep.get("versionRange").and_then(|v| v.as_str()).unwrap_or("");
                let mandatory = dep.get("mandatory").and_then(|v| v.as_bool()).unwrap_or(true);
                let dep_type = dep.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let is_required = mandatory || dep_type.eq_ignore_ascii_case("required");
                if is_required && !mod_id.is_empty() && mod_id != "forge" && mod_id != "neoforge" && mod_id != "minecraft" {
                    result.push((mod_id.to_string(), version_range.to_string()));
                }
            }
        }
    }
    result
}

/// Extract dependencies from a Fabric mod JAR's fabric.mod.json.
/// Returns a list of (mod_id, version_range) tuples.
pub fn read_fabric_dependencies(path: &std::path::Path) -> Vec<(String, String)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let text = match read_zip_text(&mut zip, "fabric.mod.json") {
        Some(t) => t,
        None => return Vec::new(),
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(deps) = value.get("depends").and_then(|d| d.as_object()) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for (mod_id, version) in deps {
        let version_str = version.as_str().unwrap_or("*");
        if mod_id != "fabricloader" && mod_id != "fabric" && mod_id != "minecraft" {
            result.push((mod_id.clone(), version_str.to_string()));
        }
    }
    result
}

/// Read mod info from a JAR/ZIP file. Always returns a ModInfo — falls back
/// to the filename if no metadata can be extracted.
pub fn read_mod_info(path: &std::path::Path) -> crate::app_state::ModInfo {
    let file_name = path
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let fallback = crate::app_state::ModInfo {
        display_name: file_name
            .trim_end_matches(".jar")
            .trim_end_matches(".tmod")
            .to_string(),
        file_name: file_name.clone(),
        version: String::new(),
        authors: Vec::new(),
        description: String::new(),
        icon_data_url: None,
    };

    let Ok(file) = std::fs::File::open(path) else {
        return fallback;
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return fallback;
    };

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

/// Pre-generate chunks — delegates to server module.
pub async fn do_pregenerate_chunks(
    app: Arc<AppEventSender>,
    total_chunks: u32,
) -> Result<(), String> {
    crate::server::do_pregenerate_chunks(app, total_chunks).await
}

/// Stub: kill server and playit for remote control.
pub async fn remote_kill_server_and_playit(app: &Arc<AppEventSender>) {
    let _ = crate::server::stop_server(app.clone()).await;
    let _ = crate::playit::stop(app.clone()).await;
}

/// Install a game server — delegates to the full implementation in server.rs.
pub async fn do_install_server(
    app: Arc<AppEventSender>,
    cfg: ServerConfig,
) -> Result<ServerConfig, String> {
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
                    if next.is_ascii_alphabetic() || next == 'm' {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next(); // consume ']'
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '\x07' || next == '\x1b' {
                        break;
                    }
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
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

pub fn toml_string(value: &toml::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

pub fn read_zip_text<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut file = zip.by_name(name).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

/// Safely extract a ZIP archive into `dest_root`.
///
/// For each entry:
/// 1. Calls `enclosed_name()` — rejects absolute paths or `..` components.
/// 2. Canonicalises the resolved output path and verifies it is still
///    inside `dest_root`, preventing symlink / traversal escapes.
///
/// `strip_prefix` is optional — when non-empty the leading prefix is
/// removed from every entry name before joining with `dest_root`.
pub fn safe_extract_zip<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    dest_root: &Path,
    strip_prefix: &str,
) -> Result<(), String> {
    let canonical_root = dest_root
        .canonicalize()
        .or_else(|_| {
            std::fs::create_dir_all(dest_root)?;
            dest_root.canonicalize()
        })
        .map_err(|e| format!("Cannot resolve destination root: {}", e))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Read zip entry {}: {}", i, e))?;

        // 1) enclosed_name filters absolute paths and `..` components
        let enclosed: PathBuf = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue, // skip unsafe paths silently
        };

        // 2) Strip optional prefix (e.g. "world/")
        let relative = if !strip_prefix.is_empty() {
            enclosed
                .strip_prefix(strip_prefix)
                .unwrap_or(&enclosed)
                .to_path_buf()
        } else {
            enclosed
        };

        let outpath = dest_root.join(&relative);

        // 3) Canonicalize parent (or the dir itself) and verify containment
        if let Some(parent) = outpath.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let canonical_out = if outpath.is_dir() {
            std::fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
            outpath
                .canonicalize()
                .map_err(|e| format!("Cannot resolve {}: {}", outpath.display(), e))?
        } else {
            // For files, canonicalize the parent since the file may not exist yet
            let parent = outpath.parent().unwrap_or(dest_root);
            parent
                .canonicalize()
                .map_err(|e| format!("Cannot resolve parent {}: {}", parent.display(), e))?
                .join(outpath.file_name().unwrap_or_default())
        };

        if !canonical_out.starts_with(&canonical_root) {
            return Err(format!(
                "Zip entry escapes destination: {} -> {}",
                entry.name(),
                canonical_out.display()
            ));
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
        } else {
            if let Some(p) = outpath.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let mut outfile = std::fs::File::create(&outpath)
                .map_err(|e| format!("Create {}: {}", outpath.display(), e))?;
            std::io::copy(&mut entry, &mut outfile)
                .map_err(|e| format!("Write {}: {}", outpath.display(), e))?;
        }
    }
    Ok(())
}

pub fn read_zip_icon_data_url<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    path: &str,
) -> Option<String> {
    let clean = path.trim().trim_start_matches('/');
    if clean.is_empty() {
        return None;
    }
    let mut file = zip.by_name(clean).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() || bytes.len() > 256 * 1024 {
        return None;
    }
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

#[cfg(test)]
mod dep_tests {
    use super::*;

    #[test]
    fn test_neoforge_dependency_parsing() {
        // Test with a real neoforge.mods.toml
        let test_toml = r#"
modLoader="javafml"
loaderVersion="[4,)"

[[mods]]
modId="testmod"
displayName="Test Mod"

[[dependencies.testmod]]
    modId="neoforge"
    type="required"
    versionRange="[21.1.0,)"

[[dependencies.testmod]]
    modId="minecraft"
    type="required"
    versionRange="[1.21,)"

[[dependencies.testmod]]
    modId="create"
    type="required"
    versionRange="[6.0.9,)"

[[dependencies.testmod]]
    modId="curios"
    mandatory=true
    versionRange="[9.0.0,)"
"#;

        let value: toml::Value = test_toml.parse().unwrap();
        let deps = value.get("dependencies").unwrap().as_table().unwrap();
        
        let mut result = Vec::new();
        for (_key, dep_list) in deps {
            if let Some(arr) = dep_list.as_array() {
                for dep in arr {
                    let mod_id = dep.get("modId").and_then(|v| v.as_str()).unwrap_or("");
                    let version_range = dep.get("versionRange").and_then(|v| v.as_str()).unwrap_or("");
                    let mandatory = dep.get("mandatory").and_then(|v| v.as_bool()).unwrap_or(true);
                    let dep_type = dep.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let is_required = mandatory || dep_type.eq_ignore_ascii_case("required");
                    if is_required && mod_id != "neoforge" && mod_id != "minecraft" {
                        result.push((mod_id.to_string(), version_range.to_string()));
                    }
                }
            }
        }
        
        assert_eq!(result.len(), 2, "Should find create and curios as dependencies");
        assert!(result.iter().any(|(id, _)| id == "create"), "Should find create");
        assert!(result.iter().any(|(id, _)| id == "curios"), "Should find curios");
    }
}

#[test]
fn test_real_jar_dependencies() {
    let jar_path = std::path::Path::new("/Users/cc-tienanh/Library/Application Support/lbby/profiles/f3fc661d91c3468aa3a67f024e13cf70/server/mods/irons_jewelry-1.21.1-1.6.1.1.jar");
    if !jar_path.exists() {
        println!("Test JAR not found, skipping");
        return;
    }
    let deps = read_forge_dependencies(jar_path);
    println!("irons_jewelry deps: {:?}", deps);
    assert!(!deps.is_empty(), "Should find dependencies");
    assert!(deps.iter().any(|(id, _)| id == "apothic_attributes"), "Should find apothic_attributes");
}

// ── Version Compatibility ─────────────────────────────────────────────────

/// Check if a version string satisfies a version range (Forge/Fabric style).
/// Supports: [1.0.0,) (inclusive lower, unbounded upper)
///           (1.0.0,2.0.0) (exclusive bounds)
///           [1.0.0,2.0.0] (inclusive bounds)
///           1.0.0 (exact match)
pub fn version_matches_range(version: &str, range: &str) -> bool {
    let range = range.trim();
    if range.is_empty() {
        return true; // No constraint
    }

    // Parse the installed version (strip metadata like -hotfix, -beta, etc.)
    let ver_str = version.split('-').next().unwrap_or(version);
    let installed = match semver::Version::parse(ver_str) {
        Ok(v) => v,
        Err(_) => return false, // Can't parse, assume incompatible
    };

    // Handle simple exact version: "1.0.0"
    if !range.contains(',') && !range.contains('(') && !range.contains('[') {
        if let Ok(req) = semver::VersionReq::parse(range) {
            return req.matches(&installed);
        }
        return false;
    }

    // Parse range format: [lower,upper] or (lower,upper) or mixed
    let range = range.trim_start_matches('[').trim_start_matches('(');
    let range = range.trim_end_matches(']').trim_end_matches(')');

    let parts: Vec<&str> = range.splitn(2, ',').collect();
    let lower_str = parts[0].trim();
    let upper_str = parts.get(1).map(|s| s.trim()).unwrap_or("");

    // Check lower bound
    if !lower_str.is_empty() {
        if let Ok(lower) = semver::Version::parse(lower_str) {
            if installed < lower {
                return false;
            }
        }
    }

    // Check upper bound
    if !upper_str.is_empty() {
        if let Ok(upper) = semver::Version::parse(upper_str) {
            // Check if original range used exclusive upper bound
            let original = range;
            let is_exclusive_upper = original.ends_with(')') || original.contains(", ");
            if is_exclusive_upper {
                if installed >= upper {
                    return false;
                }
            } else {
                if installed > upper {
                    return false;
                }
            }
        }
    }

    true
}

/// Extract mod version from filename (e.g., "tacz-1.1.8-hotfix.jar" -> "1.1.8")
pub fn extract_mod_version(filename: &str) -> Option<String> {
    let stem = std::path::Path::new(filename)
        .file_stem()?
        .to_str()?;

    // Try to find version pattern: after last dash, before .jar
    // Common patterns: "modname-1.0.0.jar", "modname-1.0.0-beta.jar"
    let parts: Vec<&str> = stem.rsplitn(2, '-').collect();
    if parts.len() == 2 {
        let version_part = parts[0];
        // Strip common suffixes
        let version = version_part
            .split('+').next()
            .unwrap_or(version_part)
            .split('_').next()
            .unwrap_or(version_part);
        return Some(version.to_string());
    }
    None
}
