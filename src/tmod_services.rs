//! tModLoader mod management services.
//!
//! Handles reading/writing `enabled.json`, listing `.tmod` files,
//! and downloading mods from Steam Workshop via SteamCMD.

use crate::steamcmd;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};


/// Information about an installed .tmod file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TModInfo {
    pub internal_name: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub file_name: String,
    pub enabled: bool,
    pub file_size: u64,
}

/// Get the path to the Mods directory for a tModLoader server.
pub fn mods_dir(server_dir: &Path) -> PathBuf {
    server_dir.join("Mods")
}

/// Get the path to `enabled.json` in the Mods directory.
pub fn enabled_json_path(server_dir: &Path) -> PathBuf {
    mods_dir(server_dir).join("enabled.json")
}

/// Read the `enabled.json` file — returns a list of mod internal names.
pub fn read_enabled_json(server_dir: &Path) -> Result<Vec<String>, String> {
    let path = enabled_json_path(server_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read enabled.json: {}", e))?;
    let mods: Vec<String> = serde_json::from_str(&content)
        .map_err(|e| format!("Invalid enabled.json: {}", e))?;
    Ok(mods)
}

/// Write a list of mod internal names to `enabled.json`.
pub fn write_enabled_json(server_dir: &Path, mods: &[String]) -> Result<(), String> {
    let path = enabled_json_path(server_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(mods).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write enabled.json: {}", e))
}

/// Read the `install.txt` file — returns a list of Steam Workshop IDs.
pub fn read_install_txt(server_dir: &Path) -> Result<Vec<String>, String> {
    let path = mods_dir(server_dir).join("install.txt");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read install.txt: {}", e))?;
    Ok(content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Write a list of Steam Workshop IDs to `install.txt`.
pub fn write_install_txt(server_dir: &Path, workshop_ids: &[String]) -> Result<(), String> {
    let path = mods_dir(server_dir).join("install.txt");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = workshop_ids.join("\n") + "\n";
    std::fs::write(&path, content).map_err(|e| format!("Failed to write install.txt: {}", e))
}

/// List all `.tmod` files in the Mods directory with their enable status.
pub fn list_installed_mods(server_dir: &Path) -> Result<Vec<TModInfo>, String> {
    let dir = mods_dir(server_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let enabled: HashSet<String> = read_enabled_json(server_dir)?
        .into_iter()
        .collect();

    let mut mods = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|x| x == "tmod") {
            continue;
        }
        let info = read_tmod_info(&path, &enabled);
        mods.push(info);
    }

    mods.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    Ok(mods)
}

/// Read basic metadata from a `.tmod` file.
///
/// tModLoader .tmod files are essentially ZIP archives containing a manifest.
/// For simplicity, we extract the mod name from the filename and try to read
/// the embedded metadata if available.
fn read_tmod_info(path: &Path, enabled: &HashSet<String>) -> TModInfo {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // Try to extract the internal name from the filename
    // .tmod files are typically named "ModName.tmod" or "ModName_v1.2.3.tmod"
    let internal_name = path
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| file_name.clone());

    // Try to read the build.txt from inside the .tmod (it's a zip)
    let (display_name, version, author, description) = match read_tmod_metadata(path) {
        Some(meta) => meta,
        None => {
            // Fall back to filename-based info
            (internal_name.clone(), "unknown".into(), String::new(), String::new())
        }
    };

    TModInfo {
        internal_name: internal_name.clone(),
        display_name,
        version,
        author,
        description,
        file_name,
        enabled: enabled.contains(&internal_name),
        file_size,
    }
}

/// Try to read metadata from a .tmod file (ZIP archive with build.txt).
fn read_tmod_metadata(path: &Path) -> Option<(String, String, String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;

    // Try to read build.txt
    let build_content = {
        let mut f = zip.by_name("build.txt").ok()?;
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).ok()?;
        s
    };

    let mut name = String::new();
    let mut version = String::new();
    let mut author = String::new();
    let mut description = String::new();

    for line in build_content.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "displayName" => name = value.to_string(),
                "version" => version = value.to_string(),
                "author" => author = value.to_string(),
                "description" => description = value.to_string(),
                _ => {}
            }
        }
    }

    Some((
        if name.is_empty() { None } else { Some(name) },
        Some(version),
        Some(author),
        Some(description),
    ))
    .map(|(n, v, a, d)| {
        (
            n.unwrap_or_default(),
            v.unwrap_or_else(|| "unknown".into()),
            a.unwrap_or_default(),
            d.unwrap_or_default(),
        )
    })
}

/// Toggle a mod on or off by updating `enabled.json`.
pub fn toggle_mod(server_dir: &Path, mod_name: &str, enable: bool) -> Result<Vec<TModInfo>, String> {
    let mut enabled = read_enabled_json(server_dir)?;
    if enable {
        if !enabled.contains(&mod_name.to_string()) {
            enabled.push(mod_name.to_string());
        }
    } else {
        enabled.retain(|m| m != mod_name);
    }
    write_enabled_json(server_dir, &enabled)?;
    list_installed_mods(server_dir)
}

/// Enable a specific list of mods (replace entire enabled.json).
pub fn set_enabled_mods(server_dir: &Path, mod_names: &[String]) -> Result<Vec<TModInfo>, String> {
    write_enabled_json(server_dir, mod_names)?;
    list_installed_mods(server_dir)
}

/// Delete a .tmod file from the Mods directory.
pub fn delete_mod(server_dir: &Path, file_name: &str) -> Result<Vec<TModInfo>, String> {
    let dir = mods_dir(server_dir);
    let path = dir.join(file_name);
    if !path.exists() {
        return Err(format!("Mod file {} not found", file_name));
    }
    // Safety: ensure the path is within the Mods directory
    if !path.starts_with(&dir) {
        return Err("Invalid mod file path".to_string());
    }
    std::fs::remove_file(&path).map_err(|e| format!("Failed to delete mod: {}", e))?;
    // Also remove from enabled.json
    let stem = Path::new(file_name)
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let _ = toggle_mod(server_dir, &stem, false);
    list_installed_mods(server_dir)
}

/// Add a .tmod file to the Mods directory by copying it from a source path.
pub fn add_mod_file(server_dir: &Path, source_path: &Path) -> Result<Vec<TModInfo>, String> {
    if !source_path.exists() {
        return Err("Source file not found".to_string());
    }
    if !source_path.extension().is_some_and(|x| x == "tmod") {
        return Err("File is not a .tmod file".to_string());
    }

    let dir = mods_dir(server_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let file_name = source_path
        .file_name()
        .ok_or("Invalid file name")?
        .to_string_lossy()
        .to_string();
    let dest = dir.join(&file_name);

    std::fs::copy(source_path, &dest).map_err(|e| format!("Failed to copy mod file: {}", e))?;

    // Auto-enable the mod
    let stem = Path::new(&file_name)
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let _ = toggle_mod(server_dir, &stem, true);

    list_installed_mods(server_dir)
}

/// Download a mod from Steam Workshop and install it.
///
/// `workshop_id`: The Steam Workshop file ID
/// `server_dir`: The tModLoader server directory
pub async fn install_workshop_mod(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    workshop_id: &str,
    server_dir: &Path,
) -> Result<Vec<TModInfo>, String> {
    let download_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby")
        .join("steamcmd-workshop");

    let item_dir = steamcmd::download_workshop_item(
        app,
        workshop_id,
        steamcmd::TMODLOADER_APP_ID,
        &download_dir,
    )
    .await?;

    // Find .tmod files in the downloaded content
    let dir = mods_dir(server_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut installed_any = false;
    copy_tmod_recursive(&item_dir, &dir, &mut installed_any)?;

    if !installed_any {
        return Err(format!(
            "No .tmod files found in Workshop item {}",
            workshop_id
        ));
    }

    // Update install.txt with the workshop ID
    let mut ids = read_install_txt(server_dir)?;
    if !ids.contains(&workshop_id.to_string()) {
        ids.push(workshop_id.to_string());
        write_install_txt(server_dir, &ids)?;
    }

    list_installed_mods(server_dir)
}

/// Recursively copy .tmod files from source to destination.
fn copy_tmod_recursive(source: &Path, dest: &Path, copied: &mut bool) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(source).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            copy_tmod_recursive(&path, dest, copied)?;
        } else if path.extension().is_some_and(|x| x == "tmod") {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let dest_path = dest.join(&file_name);
            std::fs::copy(&path, &dest_path)
                .map_err(|e| format!("Failed to copy {}: {}", file_name, e))?;
            *copied = true;
        }
    }
    Ok(())
}

/// Import a modpack from a directory containing `enabled.json`, `install.txt`,
/// and optionally `.tmod` files.
pub async fn import_modpack(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    source_dir: &Path,
    server_dir: &Path,
) -> Result<Vec<TModInfo>, String> {
    let mods = mods_dir(server_dir);
    std::fs::create_dir_all(&mods).map_err(|e| e.to_string())?;

    // Copy .tmod files from source
    let mut copied = false;
    copy_tmod_recursive(source_dir, &mods, &mut copied)?;

    // Copy enabled.json if present
    let src_enabled = source_dir.join("enabled.json");
    if src_enabled.exists() {
        std::fs::copy(&src_enabled, enabled_json_path(server_dir))
            .map_err(|e| format!("Failed to copy enabled.json: {}", e))?;
    }

    // Copy install.txt if present, then download Workshop mods
    let src_install = source_dir.join("install.txt");
    if src_install.exists() {
        std::fs::copy(&src_install, mods.join("install.txt"))
            .map_err(|e| format!("Failed to copy install.txt: {}", e))?;

        let workshop_ids = read_install_txt(server_dir)?;
        if !workshop_ids.is_empty() {
            app.emit("mod-task-progress", serde_json::json!({
                "stage": "Downloading Workshop mods",
                "message": format!("{} mods to download", workshop_ids.len()),
                "current": 0,
                "total": workshop_ids.len(),
                "progress": 0.0,
            }))
            .ok();

            for (idx, id) in workshop_ids.iter().enumerate() {
                app.emit("mod-task-progress", serde_json::json!({
                    "stage": "Downloading Workshop mods",
                    "message": format!("Downloading mod {} / {}", idx + 1, workshop_ids.len()),
                    "current": idx + 1,
                    "total": workshop_ids.len(),
                    "progress": (idx as f32 + 1.0) / workshop_ids.len() as f32,
                }))
                .ok();

                install_workshop_mod(app, id, server_dir).await?;
            }
        }
    }

    list_installed_mods(server_dir)
}

/// Export current mod setup as a modpack directory.
///
/// Creates a directory with `enabled.json`, `install.txt`, and copies all `.tmod` files.
pub fn export_modpack(server_dir: &Path, output_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(output_dir).map_err(|e| e.to_string())?;

    // Copy enabled.json
    let enabled_src = enabled_json_path(server_dir);
    if enabled_src.exists() {
        std::fs::copy(&enabled_src, output_dir.join("enabled.json"))
            .map_err(|e| format!("Failed to copy enabled.json: {}", e))?;
    }

    // Copy install.txt
    let install_src = mods_dir(server_dir).join("install.txt");
    if install_src.exists() {
        std::fs::copy(&install_src, output_dir.join("install.txt"))
            .map_err(|e| format!("Failed to copy install.txt: {}", e))?;
    }

    // Copy all .tmod files
    let mods = mods_dir(server_dir);
    if mods.exists() {
        for entry in std::fs::read_dir(&mods).map_err(|e| e.to_string())?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "tmod") {
                let file_name = match path.file_name() {
                    Some(n) => n,
                    None => continue,
                };
                std::fs::copy(&path, output_dir.join(file_name))
                    .map_err(|e| format!("Failed to copy mod: {}", e))?;
            }
        }
    }

    Ok(())
}

/// Open the Mods directory in the system file manager.
pub fn open_mods_folder(server_dir: &Path) -> Result<(), String> {
    let dir = mods_dir(server_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;

    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(&dir);
        crate::helpers::hide_std_child_window(&mut cmd);
        cmd.spawn().map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}
