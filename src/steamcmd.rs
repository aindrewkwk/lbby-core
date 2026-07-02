//! SteamCMD integration for downloading and updating game servers.
//!
//! SteamCMD is Valve's command-line tool for downloading dedicated servers.
//! Lbby uses it to install tModLoader (App ID 1281930) and optionally
//! vanilla Terraria (App ID 105600) server files.

use std::path::{Path, PathBuf};


/// Steam App IDs
pub const TERRARIA_APP_ID: &str = "105600";
/// tModLoader game App ID (workshop mods live under the game, not the dedicated server).
pub const TMODLOADER_APP_ID: &str = "1281930";

/// Find the SteamCMD binary on the system.
///
/// Search order:
/// 1. `steamcmd` in PATH
/// 2. Common install locations per OS
pub fn find_steamcmd() -> Option<PathBuf> {
    // Check PATH first
    let binary = if cfg!(target_os = "windows") {
        "steamcmd.exe"
    } else {
        "steamcmd"
    };

    if let Ok(output) = std::process::Command::new(if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    })
    .arg(binary)
    .output()
       {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout);
            let path = path_str.trim().lines().next().unwrap_or("").trim();
            if !path.is_empty() {
                let p = PathBuf::from(path);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    // Check common install locations
    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        vec![
            PathBuf::from("C:\\steamcmd\\steamcmd.exe"),
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("steamcmd")
                .join("steamcmd.exe"),
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("steamcmd")
                .join("steamcmd.exe"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            // Homebrew install location
            PathBuf::from("/usr/local/bin/steamcmd"),
            PathBuf::from("/opt/homebrew/bin/steamcmd"),
            // Lbby-managed install
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("steamcmd")
                .join("steamcmd.sh"),
            lbby_steamcmd_dir().join("steamcmd.sh"),
        ]
    } else {
        vec![
            PathBuf::from("/usr/bin/steamcmd"),
            PathBuf::from("/usr/games/steamcmd"),
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("steamcmd")
                .join("steamcmd.sh"),
        ]
    };

    for candidate in candidates {
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

/// Get the directory where Lbby manages its own SteamCMD installation.
///
/// Uses `~/.lbby-steamcmd` to avoid spaces in the path (SteamCMD's shell
/// script breaks on paths with spaces like "Application Support").
pub fn lbby_steamcmd_dir() -> PathBuf {
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".lbby-steamcmd");
    std::fs::create_dir_all(&base).ok();
    base
}

/// Download and install SteamCMD to the Lbby-managed directory.
///
/// On Windows: downloads `steamcmd.zip` and extracts it.
/// On Linux: downloads the tarball and extracts it.
/// On macOS: downloads the macOS version and extracts it.
pub async fn install_steamcmd(app: &std::sync::Arc<crate::app_state::AppEventSender>) -> Result<PathBuf, String> {
    let install_dir = lbby_steamcmd_dir();

    let (url, binary_name) = if cfg!(target_os = "windows") {
        (
            "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip",
            "steamcmd.exe",
        )
    } else if cfg!(target_os = "macos") {
        (
            "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_osx.tar.gz",
            "steamcmd.sh",
        )
    } else {
        (
            "https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz",
            "steamcmd.sh",
        )
    };

    let binary_path = install_dir.join(binary_name);
    if binary_path.exists() {
        return Ok(binary_path);
    }

    app.emit("install-progress", serde_json::json!({
        "stage": "Downloading SteamCMD",
        "progress": 0.1,
    }))
    .ok();

    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to download SteamCMD: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("SteamCMD download failed with HTTP {}", resp.status()));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read SteamCMD download: {}", e))?;

    let archive_name = if cfg!(target_os = "windows") {
        "steamcmd.zip"
    } else if cfg!(target_os = "macos") {
        "steamcmd_osx.tar.gz"
    } else {
        "steamcmd_linux.tar.gz"
    };
    let archive_path = install_dir.join(archive_name);

    tokio::fs::write(&archive_path, &bytes)
        .await
        .map_err(|e| format!("Failed to write SteamCMD archive: {}", e))?;

    app.emit("install-progress", serde_json::json!({
        "stage": "Extracting SteamCMD",
        "progress": 0.5,
    }))
    .ok();

    // Extract
    if cfg!(target_os = "windows") {
        // Use zip extraction
        let file = std::fs::File::open(&archive_path).map_err(|e| e.to_string())?;
        let mut zip =
            zip::ZipArchive::new(file).map_err(|e| format!("Invalid SteamCMD zip: {}", e))?;
        zip.extract(&install_dir)
            .map_err(|e| format!("Failed to extract SteamCMD: {}", e))?;
    } else {
        // Use tar
        let status = tokio::process::Command::new("tar")
            .args(["-xzf", archive_path.to_str().unwrap_or(""), "-C", install_dir.to_str().unwrap_or("")])
            .status()
            .await
            .map_err(|e| format!("Failed to extract SteamCMD: {}", e))?;
        if !status.success() {
            return Err("Failed to extract SteamCMD tarball".to_string());
        }
        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755));
        }
    }

    // Clean up archive
    let _ = tokio::fs::remove_file(&archive_path).await;

    // Verify the binary exists
    if !binary_path.exists() {
        // Check if it's in a subdirectory
        let alt_path = install_dir.join("osx32").join(&binary_name);
        if alt_path.exists() {
            // Move it to the expected location
            let _ = tokio::fs::rename(&alt_path, &binary_path).await;
        } else {
            let alt_path2 = install_dir.join("linux32").join(&binary_name);
            if alt_path2.exists() {
                let _ = tokio::fs::rename(&alt_path2, &binary_path).await;
            } else {
                return Err(format!(
                    "SteamCMD extraction completed but {} not found in {}",
                    binary_name,
                    install_dir.display()
                ));
            }
        }
    }

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755));
    }

    app.emit("install-progress", serde_json::json!({
        "stage": "SteamCMD installed",
        "progress": 1.0,
    }))
    .ok();

    Ok(binary_path)
}

/// Run SteamCMD with the given arguments.
///
/// Returns stdout+stderr as a string on success.
pub async fn run_steamcmd(steamcmd_path: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(steamcmd_path);
    cmd.args(args);
    crate::helpers::hide_child_window(&mut cmd);

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to run SteamCMD: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}\n{}", stdout, stderr);

    if !output.status.success() {
        return Err(format!("SteamCMD failed:\n{}", combined));
    }

    Ok(combined)
}

/// Download a game server via SteamCMD.
///
/// `app_id`: Steam App ID (e.g., "1281930" for tModLoader)
/// `install_dir`: Where to install the server files
/// `username`: Steam username. Use "anonymous" for free dedicated servers.
pub async fn download_server(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    app_id: &str,
    install_dir: &Path,
    username: &str,
) -> Result<(), String> {
    let steamcmd_path = match find_steamcmd() {
        Some(p) => p,
        None => install_steamcmd(app).await?,
    };

    tokio::fs::create_dir_all(install_dir)
        .await
        .map_err(|e| e.to_string())?;

    app.emit("install-progress", serde_json::json!({
        "stage": format!("Downloading server (App {})", app_id),
        "progress": 0.1,
    }))
    .ok();

    let install_str = install_dir.to_string_lossy().to_string();
    let args = vec![
        "+force_install_dir",
        &install_str,
        "+login",
        username,
        "+app_update",
        app_id,
        "validate",
        "+quit",
    ];

    let output = run_steamcmd(&steamcmd_path, &args).await?;

    // Check for common SteamCMD errors
    if output.contains("FAILED") || output.contains("Error") {
        // SteamCMD sometimes reports "0 updates" which is actually success
        if !output.contains("Success") && !output.contains("fully installed") {
            return Err(format!("SteamCMD download may have failed:\n{}", output));
        }
    }

    app.emit("install-progress", serde_json::json!({
        "stage": "Server download complete",
        "progress": 1.0,
    }))
    .ok();

    Ok(())
}

/// Download a Steam Workshop item via SteamCMD.
///
/// `workshop_id`: The Steam Workshop file ID
/// `app_id`: The parent app ID (e.g., "1281930" for tModLoader)
/// `download_dir`: Where to download the workshop content
pub async fn download_workshop_item(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    workshop_id: &str,
    app_id: &str,
    download_dir: &Path,
) -> Result<PathBuf, String> {
    let steamcmd_path = match find_steamcmd() {
        Some(p) => p,
        None => install_steamcmd(app).await?,
    };

    tokio::fs::create_dir_all(download_dir)
        .await
        .map_err(|e| e.to_string())?;

    app.emit("mod-task-progress", serde_json::json!({
        "stage": "Downloading from Workshop",
        "message": format!("Downloading Workshop item {}", workshop_id),
        "progress": 0.1,
    }))
    .ok();

    let args = vec![
        "+login",
        "anonymous",
        "+workshop_download_item",
        app_id,
        workshop_id,
        "+quit",
    ];

    let output = run_steamcmd(&steamcmd_path, &args).await?;

    if !output.contains("Success") && !output.contains("Downloaded item") {
        return Err(format!("Workshop download failed:\n{}", output));
    }

    // The downloaded item is typically in:
    // <download_dir>/steamapps/workshop/content/<app_id>/<workshop_id>/
    let item_dir = download_dir
        .join("steamapps")
        .join("workshop")
        .join("content")
        .join(app_id)
        .join(workshop_id);

    if !item_dir.exists() {
        return Err(format!(
            "Workshop item {} downloaded but directory not found at {}",
            workshop_id,
            item_dir.display()
        ));
    }

    app.emit("mod-task-progress", serde_json::json!({
        "stage": "Workshop download complete",
        "message": format!("Downloaded Workshop item {}", workshop_id),
        "progress": 1.0,
    }))
    .ok();

    Ok(item_dir)
}
