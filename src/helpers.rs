// Helper functions and types referenced by modules.
// These were originally in the monolithic lib.rs — extracted here for reuse.

use serde::Serialize;
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
