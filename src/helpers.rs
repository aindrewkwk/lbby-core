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
use sha2::{Digest, Sha256};

/// Compute SHA-256 of a file and return it as a lowercase hex string.
pub fn sha256_hex_string(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

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
/// Download a file to a destination path with atomic-write semantics.
///
/// Streams to `<dest>.partial`, flushes/syncs, then renames to `dest`.
/// On any failure the `.partial` file is cleaned up and `dest` is untouched.
///
/// If `expected_sha256` is `Some(hex)`, the downloaded file is verified
/// against that digest before the final rename. A mismatch deletes the
/// temp file and returns an error.
pub async fn download_to_file_verified(
    app: &Arc<AppEventSender>,
    url: &str,
    dest: &Path,
    label: &str,
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("Lbby")
        .timeout(std::time::Duration::from_secs(120))
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
    let partial = dest.with_extension("partial");
    let mut file = tokio::fs::File::create(&partial)
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
    // Flush + sync before rename
    file.flush().await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    drop(file);

    // Verify checksum if provided
    if let Some(expected) = expected_sha256 {
        if !expected.is_empty() {
            let actual = sha256_hex_string(&partial)
                .map_err(|e| format!("Checksum computation failed: {}", e))?;
            if actual != *expected {
                tokio::fs::remove_file(&partial).await.ok();
                return Err(format!(
                    "{} checksum mismatch: expected {}, got {}",
                    label, expected, actual
                ));
            }
        }
    }

    // Atomic rename
    tokio::fs::rename(&partial, dest).await.map_err(|e| {
        // Clean up partial on rename failure
        let _ = std::fs::remove_file(&partial);
        format!("Failed to rename download: {}", e)
    })
}

/// Download a file. Convenience wrapper around [`download_to_file_verified`]
/// with no expected checksum.
pub async fn download_to_file(
    app: &Arc<AppEventSender>,
    url: &str,
    dest: &Path,
    label: &str,
) -> Result<(), String> {
    download_to_file_verified(app, url, dest, label, None).await
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
        ..Default::default()
    })
}

fn read_forge_mod_info<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> Option<crate::app_state::ModInfo> {
    let text = read_zip_text(zip, "META-INF/neoforge.mods.toml")
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
        ..Default::default()
    })
}

/// Read ALL declared mod IDs from a Forge/NeoForge JAR (multi-mod JARs
/// declare multiple `[[mods]]` entries). Returns deduplicated list.
pub fn read_all_forge_mod_ids(path: &std::path::Path) -> Vec<String> {
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
    let Some(mods) = value.get("mods").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in mods {
        if let Some(mid) = entry.get("modId").and_then(|v| v.as_str()) {
            let trimmed = mid.trim().to_string();
            if !trimmed.is_empty() && !ids.contains(&trimmed) {
                ids.push(trimmed);
            }
        }
    }
    ids
}

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
                let version_range = dep
                    .get("versionRange")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mandatory = dep
                    .get("mandatory")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let dep_type = dep.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let is_required = mandatory || dep_type.eq_ignore_ascii_case("required");
                if is_required && !mod_id.is_empty() && !crate::jar_metadata::is_platform_id(mod_id)
                {
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
        if !crate::jar_metadata::is_platform_id(mod_id) {
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
        status: crate::app_state::ModStatus::Unreadable,
        ..Default::default()
    };

    let Ok(file) = std::fs::File::open(path) else {
        return fallback;
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return fallback;
    };

    // JAR is readable — update fallback status
    let mut readable_fallback = fallback.clone();
    readable_fallback.status = crate::app_state::ModStatus::Readable;

    if let Some(info) = read_fabric_mod_info(&mut zip, &file_name) {
        return normalize_mod_info(info);
    }
    if let Some(info) = read_forge_mod_info(&mut zip, &file_name) {
        return normalize_mod_info(info);
    }
    readable_fallback
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

/// Check if a mod JAR is client-only (should not be on a dedicated server).
/// Uses ONLY reliable metadata checks — no heuristics, no filename matching,
/// no class-file scanning. Those cause false positives that delete server-required mods.
pub fn is_client_only_mod(path: &std::path::Path) -> bool {
    // Delegate to the conservative metadata-based check
    crate::mod_side::jar_declares_client_only(path)
}

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

/// Reject path components that indicate absolute paths, traversal, or
/// platform-specific rooted paths. Returns `true` for unsafe names.
fn has_absolute_or_traversal_components(name: &str) -> bool {
    // Reject Unix absolute paths
    if name.starts_with('/') {
        return true;
    }
    // Reject Windows drive absolute paths (C:/, C:\)
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        // Check for drive letter followed by separator or just "C:" at start
        if bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\' {
            return true;
        }
    }
    // Reject UNC paths (\\server\share)
    if name.starts_with("\\\\") || name.starts_with("//") {
        return true;
    }
    // Reject backslash separators (Windows-style in zip entries)
    // Only reject if backslashes look like path separators (not just in filenames)
    if name.contains('\\') {
        let parts: Vec<&str> = name.split('\\').collect();
        // If splitting by backslash produces path-like components, reject
        if parts.len() > 1 && parts.iter().all(|p| !p.is_empty() || p.contains("..")) {
            return true;
        }
    }
    // Reject `..` components in any position (using both / and \ separators)
    for component in name.split(&['/', '\\'][..]) {
        if component == ".." {
            return true;
        }
    }
    false
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
///
/// Rejects absolute paths (Unix, Windows drive letters, UNC), traversal
/// components (`..`), and backslash-based Windows paths.
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
            None => {
                return Err(format!(
                    "Zip entry rejected by enclosed_name(): {}",
                    entry.name()
                ))
            }
        };

        // 2) Additional validation: reject platform-specific absolute paths
        //    that enclosed_name() may miss (Windows drive letters, UNC paths)
        let name_str = entry.name();
        if has_absolute_or_traversal_components(name_str) {
            return Err(format!("Zip entry rejected as unsafe path: {}", name_str));
        }

        // 3) Strip optional prefix (e.g. "world/")
        let relative = if !strip_prefix.is_empty() {
            enclosed
                .strip_prefix(strip_prefix)
                .unwrap_or(&enclosed)
                .to_path_buf()
        } else {
            enclosed
        };

        let outpath = dest_root.join(&relative);

        // 4) Canonicalize parent (or the dir itself) and verify containment
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
                    let version_range = dep
                        .get("versionRange")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let mandatory = dep
                        .get("mandatory")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    let dep_type = dep.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let is_required = mandatory || dep_type.eq_ignore_ascii_case("required");
                    if is_required && mod_id != "neoforge" && mod_id != "minecraft" {
                        result.push((mod_id.to_string(), version_range.to_string()));
                    }
                }
            }
        }

        assert_eq!(
            result.len(),
            2,
            "Should find create and curios as dependencies"
        );
        assert!(
            result.iter().any(|(id, _)| id == "create"),
            "Should find create"
        );
        assert!(
            result.iter().any(|(id, _)| id == "curios"),
            "Should find curios"
        );
    }
}

// -- Platform dependency filtering in helpers ----------------------------

#[test]
fn forge_helpers_filter_java_pseudo_dep() {
    // Mods.toml with java, minecraft, forge dependencies — all must be filtered
    let toml = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"testmod\"\n\n[[dependencies.testmod]]\nmodId=\"java\"\nversionRange=\"[17,)\"\nmandatory=true\nside=\"BOTH\"\n\n[[dependencies.testmod]]\nmodId=\"minecraft\"\nversionRange=\"[1.20.1,1.21)\"\nmandatory=true\nside=\"BOTH\"\n\n[[dependencies.testmod]]\nmodId=\"forge\"\nversionRange=\"[47,)\"\nmandatory=true\nside=\"BOTH\"\n\n[[dependencies.testmod]]\nmodId=\"neoforge\"\nversionRange=\"[20.4,)\"\nmandatory=false\nside=\"BOTH\"\n\n[[dependencies.testmod]]\nmodId=\"embeddium\"\nversionRange=\"[0.3.1,)\"\nmandatory=true\nside=\"CLIENT\"\n\n[[dependencies.testmod]]\nmodId=\"flywheel\"\nversionRange=\"[0.6,)\"\nmandatory=true\nside=\"BOTH\"\n";
    let dir = tempfile::tempdir().unwrap();
    let jar_path = dir.path().join("testmod-1.0.jar");
    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("META-INF/mods.toml", options).unwrap();
    std::io::Write::write_all(&mut zip, toml).unwrap();
    zip.finish().unwrap();

    let deps = read_forge_dependencies(&jar_path);
    // java, minecraft, forge, neoforge must be filtered
    assert!(
        !deps.iter().any(|(id, _)| id == "java"),
        "java must NOT appear as a download dependency"
    );
    assert!(
        !deps.iter().any(|(id, _)| id == "minecraft"),
        "minecraft must NOT appear"
    );
    assert!(
        !deps.iter().any(|(id, _)| id == "forge"),
        "forge must NOT appear"
    );
    assert!(
        !deps.iter().any(|(id, _)| id == "neoforge"),
        "neoforge must NOT appear"
    );
    // real mod dependencies must remain
    assert!(
        deps.iter().any(|(id, _)| id == "flywheel"),
        "flywheel should remain as a real dependency"
    );
    assert!(
        deps.iter().any(|(id, _)| id == "embeddium"),
        "embeddium should remain as a real dependency"
    );
}

#[test]
fn fabric_helpers_filter_java_pseudo_dep() {
    let json = b"{\"schemaVersion\":1,\"id\":\"testmod\",\"environment\":\"*\",\"depends\":{\"java\":\">=17\",\"minecraft\":\"1.20.1\",\"fabricloader\":\">=0.14\",\"fabric\":\"*\",\"real_lib\":\">=2.0\"}}";
    let dir = tempfile::tempdir().unwrap();
    let jar_path = dir.path().join("testmod-1.0.jar");
    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", options).unwrap();
    std::io::Write::write_all(&mut zip, json).unwrap();
    zip.finish().unwrap();

    let deps = read_fabric_dependencies(&jar_path);
    assert!(
        !deps.iter().any(|(id, _)| id == "java"),
        "java must NOT appear as a download dependency"
    );
    assert!(
        !deps.iter().any(|(id, _)| id == "minecraft"),
        "minecraft must NOT appear"
    );
    assert!(
        !deps.iter().any(|(id, _)| id == "fabricloader"),
        "fabricloader must NOT appear"
    );
    assert!(
        !deps.iter().any(|(id, _)| id == "fabric"),
        "fabric must NOT appear"
    );
    assert!(
        deps.iter().any(|(id, _)| id == "real_lib"),
        "real_lib should remain as a real dependency"
    );
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
    assert!(
        deps.iter().any(|(id, _)| id == "apothic_attributes"),
        "Should find apothic_attributes"
    );
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
    // Save bracket types BEFORE stripping
    let upper_exclusive = range.ends_with(')');
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
            if upper_exclusive {
                // Exclusive upper: version must be strictly less than upper
                if installed >= upper {
                    return false;
                }
            } else {
                // Inclusive upper: version can be equal to upper
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
    let stem = std::path::Path::new(filename).file_stem()?.to_str()?;

    // Try to find version pattern: after last dash, before .jar
    // Common patterns: "modname-1.0.0.jar", "modname-1.0.0-beta.jar"
    let parts: Vec<&str> = stem.rsplitn(2, '-').collect();
    if parts.len() == 2 {
        let version_part = parts[0];
        // Strip common suffixes
        let version = version_part
            .split('+')
            .next()
            .unwrap_or(version_part)
            .split('_')
            .next()
            .unwrap_or(version_part);
        return Some(version.to_string());
    }
    None
}

#[cfg(test)]
mod zip_traversal_tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    /// Helper: build an in-memory zip with the given entry names (dirs end with `/`).
    fn make_zip(entries: &[&str]) -> std::io::Cursor<Vec<u8>> {
        let buf = Vec::new();
        let mut w = ZipWriter::new(std::io::Cursor::new(buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for name in entries {
            if name.ends_with('/') {
                w.add_directory(*name, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(b"payload").unwrap();
            }
        }
        let cursor = w.finish().unwrap();
        // Reset cursor to start so ZipArchive can read it
        let mut c = std::io::Cursor::new(cursor.into_inner());
        c.set_position(0);
        c
    }

    #[test]
    fn rejects_dot_dot_traversal() {
        let cursor = make_zip(&["../evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(result.is_err(), "Must reject ../ traversal");
    }

    #[test]
    fn rejects_deep_dot_dot_traversal() {
        let cursor = make_zip(&["../../evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(result.is_err(), "Must reject ../../ traversal");
    }

    #[test]
    fn rejects_unix_absolute_path() {
        let cursor = make_zip(&["/evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(result.is_err(), "Must reject /evil.jar absolute path");
    }

    #[test]
    fn rejects_windows_drive_path() {
        let cursor = make_zip(&["C:/evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(result.is_err(), "Must reject C:/evil.jar drive path");
    }

    #[test]
    fn rejects_windows_backslash_drive_path() {
        let cursor = make_zip(&["C:\\evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(
            result.is_err(),
            "Must reject C:\\evil.jar backslash drive path"
        );
    }

    #[test]
    fn rejects_unc_path() {
        let cursor = make_zip(&["\\\\server\\share\\evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(
            result.is_err(),
            "Must reject \\\\server\\share\\evil.jar UNC path"
        );
    }

    #[test]
    fn rejects_embedded_dot_dot() {
        // The entry "safe/../../evil.jar" escapes the root after entering "safe/"
        let cursor = make_zip(&["safe/../../evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "");
        assert!(result.is_err(), "Must reject embedded ../../ traversal");
    }

    #[test]
    fn safe_relative_entry_extracts_normally() {
        let cursor = make_zip(&["mods/example.jar", "config/settings.toml"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        safe_extract_zip(&mut za, dir.path(), "").unwrap();
        assert!(dir.path().join("mods/example.jar").exists());
        assert!(dir.path().join("config/settings.toml").exists());
    }

    #[test]
    fn safe_nested_entry_extracts_normally() {
        let cursor = make_zip(&["nested/valid/deep/file.txt"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        safe_extract_zip(&mut za, dir.path(), "").unwrap();
        assert!(dir.path().join("nested/valid/deep/file.txt").exists());
    }

    #[test]
    fn prefix_stripping_works_for_safe_entries() {
        // Entry "overrides/mods/example.jar" with strip_prefix "overrides"
        let cursor = make_zip(&["overrides/mods/example.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        safe_extract_zip(&mut za, dir.path(), "overrides").unwrap();
        // After stripping "overrides/", the file should be at mods/example.jar
        assert!(dir.path().join("mods/example.jar").exists());
    }

    #[test]
    fn prefix_stripping_still_rejects_traversal() {
        // Even with prefix stripping, traversal must be caught
        let cursor = make_zip(&["overrides/../../evil.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        let result = safe_extract_zip(&mut za, dir.path(), "overrides");
        assert!(
            result.is_err(),
            "Must reject traversal even with prefix stripping"
        );
    }

    #[test]
    fn directory_entries_extracts_safely() {
        let cursor = make_zip(&["mods/", "mods/example.jar"]);
        let dir = tempfile::tempdir().unwrap();
        let mut za = zip::ZipArchive::new(cursor).unwrap();
        safe_extract_zip(&mut za, dir.path(), "").unwrap();
        assert!(dir.path().join("mods/").is_dir());
        assert!(dir.path().join("mods/example.jar").exists());
    }
}
