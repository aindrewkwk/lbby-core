//! AutoModpack integration.
//!
//! AutoModpack (https://github.com/Skidamek/AutoModpack) is a Forge / NeoForge
//! / Fabric / Quilt mod that, when installed on both server and client, lets
//! the server push mod updates to joining players automatically. This module
//! is responsible for the *server-side* half of that setup: detecting the
//! right release for the user's MC + loader and dropping the jar into the
//! server's `mods/` folder. The client-side bundle generator lives elsewhere.
//!
//! Strategy:
//! - Query the GitHub Releases API for `Skidamek/AutoModpack`.
//! - Walk releases newest-first, prefer non-prerelease, pick the first whose
//!   assets contain a jar whose filename matches our MC version + loader.
//! - Download that jar straight into the server's mods folder.
//!
//! We deliberately don't hard-code a release version — AutoModpack ships
//! updates often and we want to follow head without an app update.

use crate::config::{ServerConfig, ServerType};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

const GITHUB_API: &str = "https://api.github.com/repos/Skidamek/AutoModpack/releases";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoModpackStatus {
    /// Whether an AutoModpack jar is currently present in the server's mods folder.
    pub installed: bool,
    /// Filename of the installed jar, if any.
    pub installed_file: Option<String>,
    /// Latest AutoModpack release tag compatible with this server's MC + loader.
    pub latest_compatible: Option<String>,
    /// Filename Lbby would install if the user clicked "Install".
    pub latest_asset_name: Option<String>,
    /// Whether the server's loader is one AutoModpack supports.
    pub loader_supported: bool,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    prerelease: bool,
    draft: bool,
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Whether AutoModpack supports the given loader at all.
pub fn loader_supported(loader: &ServerType) -> bool {
    matches!(
        loader,
        ServerType::Forge | ServerType::NeoForge | ServerType::Fabric
        | ServerType::SpongeForge
    )
}

fn loader_token(loader: &ServerType) -> Option<&'static str> {
    match loader {
        ServerType::Forge => Some("forge"),
        ServerType::NeoForge => Some("neoforge"),
        ServerType::Fabric => Some("fabric"),
        ServerType::SpongeForge => Some("sponge"),
        _ => None,
    }
}

fn mods_dir(cfg: &ServerConfig) -> Result<PathBuf, String> {
    if cfg.server_path.trim().is_empty() {
        return Err("Choose a server folder first.".to_string());
    }
    Ok(PathBuf::from(&cfg.server_path).join("mods"))
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (AutoModpack installer)")
        .build()
        .map_err(|e| e.to_string())
}

/// Returns the AutoModpack jar already in the server's mods folder, if any.
async fn find_installed(cfg: &ServerConfig) -> Option<String> {
    let dir = mods_dir(cfg).ok()?;
    let mut entries = tokio::fs::read_dir(&dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let lower = name.to_ascii_lowercase();
        let is_jar = lower.ends_with(".jar");
        let looks_like_automodpack =
            lower.contains("automodpack") || lower.starts_with("automodpack-mod-");
        if is_jar && looks_like_automodpack {
            return Some(name);
        }
    }
    None
}

/// Pulls the GitHub Releases list and finds the first asset that matches our
/// server's MC version + loader. Returns `(tag_name, asset)`.
async fn find_compatible_asset(
    mc_version: &str,
    loader: &ServerType,
) -> Result<(String, GhAsset), String> {
    let token = loader_token(loader).ok_or_else(|| {
        format!(
            "AutoModpack doesn't support {:?} servers (Forge, NeoForge, and Fabric only).",
            loader
        )
    })?;

    let mut releases: Vec<GhRelease> = http()?
        .get(format!("{}?per_page=50", GITHUB_API))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach GitHub Releases: {}", e))?
        .error_for_status()
        .map_err(|e| format!("GitHub Releases error: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Couldn't parse GitHub response: {}", e))?;

    // Prefer non-prerelease, non-draft releases (newest first); if none match,
    // we'll fall back to prereleases below.
    releases.retain(|r| !r.draft);

    // Asset filename heuristics. AutoModpack's asset names look like:
    //   automodpack-mod-4.0.0-beta3+mc1.20.1-forge.jar
    //   automodpack-4.0.0+mc1.20.1-fabric.jar
    // We need to match: contains MC version AND loader token (case-insensitive,
    // dot/dash tolerant), and ends with .jar.
    let mc_lower = mc_version.to_ascii_lowercase();
    let want_loader = token.to_ascii_lowercase();

    let matches_asset = |asset: &GhAsset| -> bool {
        let lower = asset.name.to_ascii_lowercase();
        if !lower.ends_with(".jar") {
            return false;
        }
        if !lower.contains(&mc_lower) {
            // Also try "mc1.20.1" form
            let with_prefix = format!("mc{}", mc_lower);
            if !lower.contains(&with_prefix) {
                return false;
            }
        }
        // Loader filter — be careful: "forge" is a substring of "neoforge",
        // so for Forge we must reject anything that contains "neoforge".
        if want_loader == "forge" && lower.contains("neoforge") {
            return false;
        }
        lower.contains(&want_loader)
    };

    // Pass 1: stable releases.
    for release in releases.iter().filter(|r| !r.prerelease) {
        if let Some(asset) = release.assets.iter().find(|a| matches_asset(a)) {
            return Ok((release.tag_name.clone(), asset.clone()));
        }
    }
    // Pass 2: include prereleases.
    for release in releases.iter() {
        if let Some(asset) = release.assets.iter().find(|a| matches_asset(a)) {
            return Ok((release.tag_name.clone(), asset.clone()));
        }
    }

    Err(format!(
        "No AutoModpack release found for Minecraft {} on {}. The mod may not have been updated for this version yet — check https://github.com/Skidamek/AutoModpack/releases.",
        mc_version, token
    ))
}

/// Reports current install + compatibility status without modifying anything.
pub async fn status(cfg: &ServerConfig) -> AutoModpackStatus {
    let supported = loader_supported(&cfg.server_type);
    let installed_file = find_installed(cfg).await;
    let installed = installed_file.is_some();

    let (latest_tag, latest_asset) = if supported && !cfg.minecraft_version.is_empty() {
        match find_compatible_asset(&cfg.minecraft_version, &cfg.server_type).await {
            Ok((tag, asset)) => (Some(tag), Some(asset.name)),
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };

    AutoModpackStatus {
        installed,
        installed_file,
        latest_compatible: latest_tag,
        latest_asset_name: latest_asset,
        loader_supported: supported,
    }
}

/// Downloads the latest compatible AutoModpack jar into the server's mods
/// folder. If an older AutoModpack jar already exists, it is replaced.
/// Emits `mod-progress` events during download so the UI can show progress.
pub async fn install(app: std::sync::Arc<crate::app_state::AppEventSender>, cfg: &ServerConfig) -> Result<AutoModpackStatus, String> {
    if !loader_supported(&cfg.server_type) {
        return Err(format!(
            "AutoModpack supports Forge, NeoForge, and Fabric servers only. Current server type: {:?}.",
            cfg.server_type
        ));
    }
    if cfg.minecraft_version.trim().is_empty() {
        return Err("Pick a Minecraft version first.".to_string());
    }

    let (tag, asset) = find_compatible_asset(&cfg.minecraft_version, &cfg.server_type).await?;

    let dir = mods_dir(cfg)?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Couldn't create mods folder: {}", e))?;

    // Remove any older AutoModpack jar so we don't end up with two side-by-side.
    if let Some(existing) = find_installed(cfg).await {
        let _ = tokio::fs::remove_file(dir.join(&existing)).await;
    }

    let dest = dir.join(&asset.name);
    emit_progress(
        &app,
        "Downloading AutoModpack",
        &format!("{} ({})", asset.name, tag),
        0,
        100,
    );
    download_with_progress(&app, &asset.browser_download_url, &dest, &asset.name).await?;
    emit_progress(&app, "AutoModpack installed", &asset.name, 100, 100);

    Ok(AutoModpackStatus {
        installed: true,
        installed_file: Some(asset.name.clone()),
        latest_compatible: Some(tag),
        latest_asset_name: Some(asset.name),
        loader_supported: true,
    })
}

/// Builds a "Join my server" client pack ZIP at `dest_path`. The ZIP
/// contains the same AutoModpack jar (it's loader-side, used by both server
/// and client) plus a plain-English README explaining install + connect.
/// The server's public address is included if known so the user only has to
/// send the file — friends paste the address from inside the README.
pub async fn generate_client_pack(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    cfg: &ServerConfig,
    dest_path: &Path,
    server_address: Option<&str>,
) -> Result<(u64, String), String> {
    if !loader_supported(&cfg.server_type) {
        return Err(format!(
            "AutoModpack supports Forge, NeoForge, and Fabric servers only. Current server type: {:?}.",
            cfg.server_type
        ));
    }
    if cfg.minecraft_version.trim().is_empty() {
        return Err("Pick a Minecraft version first.".to_string());
    }

    let (tag, asset) = find_compatible_asset(&cfg.minecraft_version, &cfg.server_type).await?;

    // Download the jar to a temp file (re-used regardless of where the user
    // saves the ZIP). We don't reuse the server-side install because the
    // client pack might be generated before/without installing on the
    // server.
    let tmp_dir = std::env::temp_dir().join("lbby-automodpack");
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|e| format!("Couldn't create temp dir: {}", e))?;
    let tmp_jar = tmp_dir.join(&asset.name);

    emit_progress(
        &app,
        "Building client pack",
        &format!("Downloading {} ({})", asset.name, tag),
        0,
        100,
    );
    download_with_progress(&app, &asset.browser_download_url, &tmp_jar, &asset.name).await?;

    // README that ships inside the ZIP.
    let loader_pretty = match cfg.server_type {
        ServerType::Forge => "Forge",
        ServerType::NeoForge => "NeoForge",
        ServerType::Fabric => "Fabric",
        ServerType::SpongeForge => "SpongeForge",
        _ => "(unsupported)",
    };
    let server_line = match server_address {
        Some(addr) if !addr.trim().is_empty() => format!("    Server address:  {}", addr),
        _ => "    Server address:  <ask your host>".to_string(),
    };
    let readme = format!(
        "How to join {server_name}\n\
         ================================\n\
         \n\
         {server_line}\n\
         \n\
         Minecraft version:  {mc}\n\
         Loader:             {loader} {loader_ver}\n\
         \n\
         ── 1.  Install {loader} for Minecraft {mc} ──\n\
         If you don't already have {loader} for {mc}, install it via your\n\
         launcher (Modrinth App, Prism, CurseForge, vanilla launcher with\n\
         the {loader} installer). Make sure you can launch a profile that\n\
         uses {loader} {loader_ver}.\n\
         \n\
         ── 2.  Drop the included .jar into your mods folder ──\n\
         Copy `{jar}` into your Minecraft `mods/` folder. On macOS / Linux\n\
         that's usually `~/Library/Application Support/minecraft/mods/` or\n\
         `~/.minecraft/mods/`. On Windows it's `%APPDATA%/.minecraft/mods/`.\n\
         (Your launcher may show a button labelled \"Open mods folder\".)\n\
         \n\
         ── 3.  Launch Minecraft, then connect to the server ──\n\
         Multiplayer → Add Server → paste the address above → Done → Join.\n\
         AutoModpack will pop up automatically the first time, listing the\n\
         mods the server requires. Click Update, wait for the download,\n\
         restart Minecraft when prompted, and you're in.\n\
         \n\
         Future mod changes will sync automatically on join — you'll only\n\
         see this prompt once per change.\n\
         \n\
         ── Need help? ──\n\
         Ask your server host. They can re-export this pack any time from\n\
         Lbby → Mods → Installed → Auto-sync with players → Generate.\n\
         \n\
         — Generated by Lbby\n",
        server_name = if cfg.server_name.trim().is_empty() { "this server".into() } else { cfg.server_name.clone() },
        server_line = server_line,
        mc = cfg.minecraft_version,
        loader = loader_pretty,
        loader_ver = cfg.loader_version.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or("(latest)"),
        jar = asset.name,
    );

    // Assemble the ZIP. Sync IO inside spawn_blocking keeps the runtime happy.
    let tmp_jar_for_zip = tmp_jar.clone();
    let dest_for_zip = dest_path.to_path_buf();
    let asset_name_for_zip = asset.name.clone();
    let written = tokio::task::spawn_blocking(move || -> Result<u64, String> {
        use std::io::Write;
        let file = std::fs::File::create(&dest_for_zip)
            .map_err(|e| format!("Couldn't create zip: {}", e))?;
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        // README first so it's the first thing the user sees on extract.
        zip.start_file("README.txt", opts).map_err(|e| e.to_string())?;
        zip.write_all(readme.as_bytes()).map_err(|e| e.to_string())?;

        // The mod jar.
        zip.start_file(&asset_name_for_zip, opts).map_err(|e| e.to_string())?;
        let jar_bytes = std::fs::read(&tmp_jar_for_zip)
            .map_err(|e| format!("Couldn't read downloaded jar: {}", e))?;
        zip.write_all(&jar_bytes).map_err(|e| e.to_string())?;

        zip.finish().map_err(|e| e.to_string())?;
        let meta = std::fs::metadata(&dest_for_zip).map_err(|e| e.to_string())?;
        Ok(meta.len())
    })
    .await
    .map_err(|e| format!("Bundle task panicked: {}", e))??;

    emit_progress(&app, "Client pack ready", &asset.name, 100, 100);
    Ok((written, asset.name))
}

/// Removes any AutoModpack jar from the server's mods folder.
pub async fn uninstall(cfg: &ServerConfig) -> Result<(), String> {
    let dir = mods_dir(cfg)?;
    if let Some(existing) = find_installed(cfg).await {
        tokio::fs::remove_file(dir.join(&existing))
            .await
            .map_err(|e| format!("Couldn't remove {}: {}", existing, e))?;
    }
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────

fn emit_progress(app: &std::sync::Arc<crate::app_state::AppEventSender>, stage: &str, label: &str, current: u32, total: u32) {
    let _ = app.emit(
        "mod-progress",
        serde_json::json!({
            "stage": stage,
            "label": label,
            "current": current,
            "total": total,
        }),
    );
}

async fn download_with_progress(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    url: &str,
    dest: &Path,
    label: &str,
) -> Result<(), String> {
    let resp = http()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?
        .error_for_status()
        .map_err(|e| format!("Download failed: {}", e))?;
    let size = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("Couldn't write to mods folder: {}", e))?;
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if size > 0 {
            let pct = ((downloaded as f64 / size as f64) * 100.0).round() as u32;
            emit_progress(app, "Downloading AutoModpack", label, pct.min(99), 100);
        }
    }
    Ok(())
}
