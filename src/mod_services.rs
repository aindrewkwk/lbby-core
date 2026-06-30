use crate::config::{self, ServerConfig, ServerType};
use crate::{helpers::{default_server_path_value, read_mod_info}, app_state::ModInfo};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize)]
pub struct ModTaskProgress {
    pub stage: String,
    pub message: String,
    pub current: u32,
    pub total: u32,
    pub progress: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModrinthSearchHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub icon_url: Option<String>,
    pub versions: Vec<String>,
    pub loaders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModUpdateInfo {
    pub file_name: String,
    pub display_name: String,
    pub current_version: String,
    pub latest_version: String,
    pub project_id: Option<String>,
    pub version_id: Option<String>,
    pub download_url: Option<String>,
    pub outdated: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourcePackInfo {
    pub name: String,
    pub kind: String,
    pub bytes: u64,
}

#[derive(Debug, Deserialize)]
struct ModrinthSearchResponse {
    hits: Vec<ModrinthProjectHit>,
}

#[derive(Debug, Deserialize)]
struct ModrinthProjectHit {
    project_id: String,
    slug: String,
    title: String,
    description: String,
    icon_url: Option<String>,
    versions: Vec<String>,
    categories: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModrinthVersion {
    id: String,
    project_id: String,
    name: String,
    version_number: String,
    files: Vec<ModrinthFile>,
    dependencies: Vec<ModrinthDependency>,
}

#[derive(Debug, Deserialize, Clone)]
struct ModrinthFile {
    hashes: HashMap<String, String>,
    url: String,
    filename: String,
    primary: bool,
}

#[derive(Debug, Deserialize)]
struct ModrinthDependency {
    project_id: Option<String>,
    version_id: Option<String>,
    dependency_type: String,
}

#[derive(Debug, Deserialize)]
struct MrpackManifest {
    name: String,
    dependencies: HashMap<String, String>,
    files: Vec<MrpackFile>,
}

#[derive(Debug, Deserialize)]
struct MrpackFile {
    path: String,
    hashes: HashMap<String, String>,
    downloads: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct CurseManifest {
    name: Option<String>,
    minecraft: CurseMinecraft,
    files: Vec<CurseFileRef>,
    overrides: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CurseMinecraft {
    version: String,
    #[serde(default, rename = "modLoaders")]
    mod_loaders: Vec<CurseLoader>,
}

#[derive(Debug, Deserialize)]
struct CurseLoader {
    id: String,
    #[serde(default)]
    primary: bool,
}

#[derive(Debug, Deserialize)]
struct CurseFileRef {
    #[serde(rename = "projectID")]
    project_id: u64,
    #[serde(rename = "fileID")]
    file_id: u64,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Deserialize)]
struct CurseDownloadUrl {
    data: String,
}

fn emit_mod_progress(app: &std::sync::Arc<crate::app_state::AppEventSender>, stage: &str, message: &str, current: u32, total: u32) {
    let progress = if total > 0 { current as f32 / total as f32 } else { 0.0 };
    app.emit("mod-task-progress", ModTaskProgress {
        stage: stage.to_string(),
        message: message.to_string(),
        current,
        total,
        progress,
    }).ok();
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
        .build()
        .map_err(|e| e.to_string())
}

fn server_dir(cfg: &ServerConfig) -> Result<PathBuf, String> {
    if cfg.server_path.trim().is_empty() {
        return Err("Choose a server folder first.".to_string());
    }
    Ok(PathBuf::from(&cfg.server_path))
}

fn mods_dir(cfg: &ServerConfig) -> Result<PathBuf, String> {
    Ok(server_dir(cfg)?.join(match cfg.server_type {
        ServerType::Paper | ServerType::Bukkit | ServerType::Spigot
        | ServerType::Folia | ServerType::Purpur => "plugins",
        ServerType::Terraria | ServerType::TModLoader => "Mods",
        _ => "mods",
    }))
}

fn normalize_loader(loader: &ServerType) -> &'static str {
    match loader {
        ServerType::Forge => "forge",
        ServerType::Fabric => "fabric",
        ServerType::NeoForge => "neoforge",
        ServerType::Paper => "paper",
        ServerType::Vanilla => "vanilla",
        ServerType::Bukkit => "bukkit",
        ServerType::Spigot => "spigot",
        ServerType::Folia => "folia",
        ServerType::Purpur => "purpur",
        ServerType::SpongeVanilla => "sponge",
        ServerType::SpongeForge => "sponge",
        ServerType::Terraria => "terraria",
        ServerType::TModLoader => "tmodloader",
    }
}

fn safe_join(base: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(format!("Blocked unsafe absolute path: {}", relative));
    }
    let mut out = base.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(format!("Blocked unsafe path traversal: {}", relative)),
        }
    }
    Ok(out)
}

fn read_zip_json<T: for<'de> Deserialize<'de>, R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<T, String> {
    let mut file = zip.by_name(name).map_err(|_| format!("Missing {}", name))?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("Invalid {}: {}", name, e))
}

fn safe_extract_prefix<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    prefix: &str,
    dest: &Path,
) -> Result<(), String> {
    let clean_prefix = prefix.trim_matches('/');
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = file.name().replace('\\', "/");
        let Some(stripped) = name.strip_prefix(&format!("{}/", clean_prefix)) else {
            continue;
        };
        if stripped.is_empty() || name.ends_with('/') {
            continue;
        }
        let out = safe_join(dest, stripped)?;
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = std::fs::File::create(&out).map_err(|e| format!("Failed to create {}: {}", out.display(), e))?;
        std::io::copy(&mut file, &mut output).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn download_bytes_to_file(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    url: &str,
    dest: &Path,
    stage: &str,
    label: &str,
    current: u32,
    total: u32,
) -> Result<(), String> {
    let resp = client()?.get(url).send().await.map_err(|e| format!("Download failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Download failed with HTTP {} for {}", resp.status(), url));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    let size = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    let mut downloaded = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        let percent = if size > 0 {
            format!(" ({:.0}%)", downloaded as f64 / size as f64 * 100.0)
        } else {
            String::new()
        };
        emit_mod_progress(app, stage, &format!("{}{}", label, percent), current, total);
    }
    Ok(())
}

fn verify_sha512(path: &Path, expected: Option<&str>) -> Result<(), String> {
    let Some(expected) = expected else { return Ok(()); };
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let hash = Sha512::digest(&bytes);
    let actual = format!("{:x}", hash);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("Hash check failed for {}", path.display()))
    }
}

/// Whether this server type is a plugin platform (uses `categories` facet on Modrinth).
fn is_plugin_loader(st: &ServerType) -> bool {
    matches!(
        st,
        ServerType::Paper | ServerType::Bukkit | ServerType::Spigot
        | ServerType::Folia | ServerType::Purpur | ServerType::SpongeVanilla
        | ServerType::SpongeForge
    )
}

/// Whether this server type is a Terraria type (not compatible with Modrinth/CurseForge).
#[allow(dead_code)]
fn is_terraria_type(st: &ServerType) -> bool {
    matches!(st, ServerType::Terraria | ServerType::TModLoader)
}

async fn latest_modrinth_version(project_id: &str, cfg: &ServerConfig) -> Result<ModrinthVersion, String> {
    let loader = normalize_loader(&cfg.server_type);
    if matches!(cfg.server_type, ServerType::Vanilla) {
        return Err("Modrinth install needs a server profile with a loader.".to_string());
    }
    // Plugin loaders use `categories` facet, mod loaders use `loaders` facet.
    let facet = if is_plugin_loader(&cfg.server_type) {
        format!("categories={}", loader)
    } else {
        format!("loaders={}", loader)
    };
    let url = format!(
        "https://api.modrinth.com/v2/project/{}/version?{}&game_versions=[\"{}\"]",
        project_id, facet, cfg.minecraft_version
    );
    let versions: Vec<ModrinthVersion> = client()?.get(url)
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    versions.into_iter().next().ok_or_else(|| {
        format!("No compatible version found for Minecraft {} / {}.", cfg.minecraft_version, loader)
    })
}

fn primary_file(version: &ModrinthVersion) -> Result<ModrinthFile, String> {
    version.files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
        .cloned()
        .ok_or_else(|| "Modrinth version has no downloadable files.".to_string())
}

pub async fn search_modrinth_mods(query: String, mc_version: String, loader: String, project_type: Option<String>) -> Result<Vec<ModrinthSearchHit>, String> {
    // Both mod loaders and plugin platforms are valid categories on Modrinth.
    let valid_loaders = [
        "forge", "fabric", "neoforge", "paper", "bukkit", "spigot",
        "folia", "purpur", "sponge",
    ];
    if !valid_loaders.contains(&loader.as_str()) {
        return Err(format!("Unsupported loader: {}", loader));
    }
    // Determine project type: explicit param > infer from loader
    let pt = project_type.unwrap_or_else(|| {
        match loader.as_str() {
            "paper" | "bukkit" | "spigot" | "folia" | "purpur" | "sponge" => "plugin".to_string(),
            _ => "mod".to_string(),
        }
    });
    let facets = format!("[[\"project_type:{}\"],[\"versions:{}\"],[\"categories:{}\"]]", pt, mc_version, loader);
    let resp: ModrinthSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[("query", query), ("facets", facets), ("limit", "20".to_string())])
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    Ok(resp.hits.into_iter().map(|hit| ModrinthSearchHit {
        project_id: hit.project_id,
        slug: hit.slug,
        title: hit.title,
        description: hit.description,
        icon_url: hit.icon_url,
        versions: hit.versions,
        loaders: hit.categories.into_iter()
            .filter(|c| matches!(c.as_str(), "forge" | "fabric" | "neoforge" | "quilt" | "paper" | "bukkit" | "spigot" | "folia" | "purpur" | "sponge"))
            .collect(),
    }).collect())
}

/// Search Modrinth for resource packs compatible with the given MC version.
pub async fn search_modrinth_resource_packs(query: String, mc_version: String) -> Result<Vec<ModrinthSearchHit>, String> {
    let facets = format!("[[\"project_type:resourcepack\"],[\"versions:{}\"]]", mc_version);
    let resp: ModrinthSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[("query", query), ("facets", facets), ("limit", "20".to_string())])
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    Ok(resp.hits.into_iter().map(|hit| ModrinthSearchHit {
        project_id: hit.project_id,
        slug: hit.slug,
        title: hit.title,
        description: hit.description,
        icon_url: hit.icon_url,
        versions: hit.versions,
        loaders: vec![],
    }).collect())
}

/// Search Modrinth for shader packs compatible with the given MC version.
pub async fn search_modrinth_shader_packs(query: String, mc_version: String) -> Result<Vec<ModrinthSearchHit>, String> {
    let facets = format!("[[\"project_type:shader\"],[\"versions:{}\"]]", mc_version);
    let resp: ModrinthSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[("query", query), ("facets", facets), ("limit", "20".to_string())])
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    Ok(resp.hits.into_iter().map(|hit| ModrinthSearchHit {
        project_id: hit.project_id,
        slug: hit.slug,
        title: hit.title,
        description: hit.description,
        icon_url: hit.icon_url,
        versions: hit.versions,
        loaders: vec![],
    }).collect())
}

/// Install a shader pack from Modrinth into the server's shaderpacks/ folder.
pub async fn install_modrinth_shader_pack(app: std::sync::Arc<crate::app_state::AppEventSender>, project_id: String) -> Result<(), String> {
    let cfg = config::load_config();
    let root = server_dir(&cfg)?;
    let shader_dir = root.join("shaderpacks");
    tokio::fs::create_dir_all(&shader_dir).await.map_err(|e| e.to_string())?;

    let version = latest_modrinth_version(&project_id, &cfg).await?;
    let file = primary_file(&version)?;
    let dest = shader_dir.join(&file.filename);

    emit_mod_progress(&app, "Downloading shader pack", &format!("Installing {}", version.name), 1, 1);
    download_bytes_to_file(&app, &file.url, &dest, "Downloading shader pack", &file.filename, 1, 1).await?;
    verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;

    Ok(())
}

pub async fn install_modrinth_mod(app: std::sync::Arc<crate::app_state::AppEventSender>, project_id: String) -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let target_dir = mods_dir(&cfg)?;
    tokio::fs::create_dir_all(&target_dir).await.map_err(|e| e.to_string())?;
    let mut installed_projects = HashSet::new();
    install_modrinth_project_recursive(&app, &cfg, &target_dir, &project_id, &mut installed_projects, 1, 1).await?;
    list_installed_mods()
}

/// Install a resource pack from Modrinth by project ID.
/// Downloads the .zip to the resourcepacks/ directory and auto-enables
/// require-resource-pack in server.properties. Also installs required
/// dependency resource packs recursively.
pub async fn install_modrinth_resource_pack(app: std::sync::Arc<crate::app_state::AppEventSender>, project_id: String) -> Result<Vec<ResourcePackInfo>, String> {
    let cfg = config::load_config();
    let root = server_dir(&cfg)?;
    let rp_dir = root.join("resourcepacks");
    tokio::fs::create_dir_all(&rp_dir).await.map_err(|e| e.to_string())?;

    let mut installed = HashSet::new();
    install_resource_pack_recursive(&app, &cfg, &rp_dir, &project_id, &mut installed).await?;

    // Auto-enable require-resource-pack
    let _ = update_resource_pack_requirement(&cfg, true);

    list_resource_packs()
}

async fn install_resource_pack_recursive(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    cfg: &ServerConfig,
    rp_dir: &Path,
    project_id: &str,
    installed: &mut HashSet<String>,
) -> Result<(), String> {
    if !installed.insert(project_id.to_string()) {
        return Ok(());
    }
    let version = latest_modrinth_version(project_id, cfg).await?;

    // Install required dependencies first
    for dep in version.dependencies.iter().filter(|d| d.dependency_type == "required") {
        if let Some(dep_project_id) = dep.project_id.as_deref() {
            Box::pin(install_resource_pack_recursive(app, cfg, rp_dir, dep_project_id, installed)).await?;
        }
    }

    let file = primary_file(&version)?;
    let dest = rp_dir.join(&file.filename);

    emit_mod_progress(app, "Downloading resource pack", &format!("Installing {}", version.name), 1, 1);
    download_bytes_to_file(app, &file.url, &dest, "Downloading resource pack", &file.filename, 1, 1).await?;
    verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
    Ok(())
}

async fn install_modrinth_project_recursive(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    cfg: &ServerConfig,
    target_dir: &Path,
    project_id: &str,
    installed_projects: &mut HashSet<String>,
    current: u32,
    total: u32,
) -> Result<(), String> {
    if !installed_projects.insert(project_id.to_string()) {
        return Ok(());
    }
    let version = latest_modrinth_version(project_id, cfg).await?;
    for dep in version.dependencies.iter().filter(|d| d.dependency_type == "required") {
        if let Some(dep_project_id) = dep.project_id.as_deref() {
            Box::pin(install_modrinth_project_recursive(
                app,
                cfg,
                target_dir,
                dep_project_id,
                installed_projects,
                current,
                total,
            )).await?;
        } else if let Some(dep_version_id) = dep.version_id.as_deref() {
            let dep_version: ModrinthVersion = client()?.get(format!("https://api.modrinth.com/v2/version/{}", dep_version_id))
                .send().await.map_err(|e| e.to_string())?
                .json().await.map_err(|e| e.to_string())?;
            let file = primary_file(&dep_version)?;
            let dest = target_dir.join(&file.filename);
            download_bytes_to_file(app, &file.url, &dest, "Downloading dependency", &file.filename, current, total).await?;
            verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
        }
    }
    let file = primary_file(&version)?;
    let dest = target_dir.join(&file.filename);
    emit_mod_progress(app, "Downloading mod", &format!("Installing {}", version.name), current, total);
    download_bytes_to_file(app, &file.url, &dest, "Downloading mod", &file.filename, current, total).await?;
    verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
    Ok(())
}

pub fn list_installed_mods() -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    if !dir.exists() { return Ok(vec![]); }
    // Terraria uses .tmod files, Minecraft uses .jar files
    let ext = if cfg.is_terraria() { "tmod" } else { "jar" };
    let mut mods: Vec<ModInfo> = std::fs::read_dir(&dir).map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == ext))
        .map(|e| read_mod_info(&e.path()))
        .collect();
    mods.sort_by_key(|a| a.display_name.to_lowercase());
    Ok(mods)
}

pub async fn check_mod_updates() -> Result<Vec<ModUpdateInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) { return Ok(vec![]); }
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jar") {
            continue;
        }
        let info = read_mod_info(&path);
        let hash = {
            let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
            format!("{:x}", Sha512::digest(&bytes))
        };
        let found: Result<ModrinthVersion, _> = client()?.get(format!("https://api.modrinth.com/v2/version_file/{}?algorithm=sha512", hash))
            .send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string());
        let Ok(current_version) = found else {
            out.push(ModUpdateInfo {
                file_name: info.file_name,
                display_name: info.display_name,
                current_version: info.version,
                latest_version: String::new(),
                project_id: None,
                version_id: None,
                download_url: None,
                outdated: false,
                message: "Not found on Modrinth".to_string(),
            });
            continue;
        };
        let latest = latest_modrinth_version(&current_version.project_id, &cfg).await?;
        let latest_file = primary_file(&latest)?;
        out.push(ModUpdateInfo {
            file_name: info.file_name,
            display_name: info.display_name,
            current_version: current_version.version_number,
            latest_version: latest.version_number.clone(),
            project_id: Some(latest.project_id),
            version_id: Some(latest.id.clone()),
            download_url: Some(latest_file.url),
            outdated: latest.id != current_version.id,
            message: if latest.id != current_version.id { "Update available".to_string() } else { "Up to date".to_string() },
        });
    }
    Ok(out)
}

pub async fn update_mod(app: std::sync::Arc<crate::app_state::AppEventSender>, file_name: String, download_url: String) -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    let old = safe_join(&dir, &file_name)?;
    if !old.exists() {
        return Err("The old mod file no longer exists.".to_string());
    }
    let backup_dir = dir.join(".lbby-backups");
    tokio::fs::create_dir_all(&backup_dir).await.map_err(|e| e.to_string())?;
    let backup = backup_dir.join(format!("{}.bak", file_name));
    tokio::fs::copy(&old, &backup).await.map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{}.download", file_name));
    let result = async {
        download_bytes_to_file(&app, &download_url, &tmp, "Updating mod", &file_name, 1, 1).await?;
        tokio::fs::rename(&tmp, &old).await.map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }.await;
    if let Err(err) = result {
        let _ = tokio::fs::copy(&backup, &old).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("Update failed and old file was restored: {}", err));
    }
    list_installed_mods()
}

pub async fn update_all_mods(app: std::sync::Arc<crate::app_state::AppEventSender>, updates: Vec<ModUpdateInfo>) -> Result<Vec<ModInfo>, String> {
    let total = updates.iter().filter(|u| u.outdated && u.download_url.is_some()).count() as u32;
    let mut current = 0;
    for update in updates.into_iter().filter(|u| u.outdated) {
        if let Some(url) = update.download_url {
            current += 1;
            emit_mod_progress(&app, "Updating mods", &format!("Updating {}", update.display_name), current, total);
            update_mod(app.clone(), update.file_name, url).await?;
        }
    }
    list_installed_mods()
}

async fn prepare_modpack_server(app: &std::sync::Arc<crate::app_state::AppEventSender>, mut cfg: ServerConfig) -> Result<ServerConfig, String> {
    if cfg.server_path.trim().is_empty() {
        cfg.server_path = default_server_path_value(None);
    }
    if cfg.ram_mb == 0 {
        cfg.ram_mb = 4096;
    }
    if cfg.max_players == 0 {
        cfg.max_players = 10;
    }
    cfg.performance_preset = "heavy_modpack".to_string();
    cfg.optimized_jvm_flags = true;
    crate::helpers::do_install_server(app.clone(), cfg).await
}

fn loader_from_mrpack(deps: &HashMap<String, String>) -> Result<(ServerType, Option<String>), String> {
    if let Some(v) = deps.get("forge") {
        return Ok((ServerType::Forge, Some(v.clone())));
    }
    if let Some(v) = deps.get("fabric-loader") {
        return Ok((ServerType::Fabric, Some(v.clone())));
    }
    if let Some(v) = deps.get("neoforge") {
        return Ok((ServerType::NeoForge, Some(v.clone())));
    }
    if deps.contains_key("quilt-loader") {
        return Err("This pack uses Quilt. Lbby does not support installing Quilt servers yet.".to_string());
    }
    Ok((ServerType::Vanilla, None))
}

pub async fn install_modrinth_modpack(app: std::sync::Arc<crate::app_state::AppEventSender>, source: String) -> Result<ServerConfig, String> {
    emit_mod_progress(&app, "Reading manifest", "Preparing Modrinth modpack", 0, 1);
    let pack_path = if source.starts_with("http://") || source.starts_with("https://") {
        resolve_or_download_mrpack(&app, &source).await?
    } else {
        PathBuf::from(source)
    };
    let file = std::fs::File::open(&pack_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("Invalid .mrpack file: {}", e))?;
    let manifest: MrpackManifest = read_zip_json(&mut zip, "modrinth.index.json")?;
    let mc = manifest.dependencies.get("minecraft").cloned().ok_or("Modpack manifest is missing Minecraft version")?;
    let (server_type, loader_version) = loader_from_mrpack(&manifest.dependencies)?;
    let mut cfg = config::load_config();
    if let Ok(root) = server_dir(&cfg) {
        backup_modpack_targets(&root)?;
    }
    cfg.minecraft_version = mc;
    cfg.server_type = server_type;
    cfg.loader_version = loader_version;
    cfg.server_name = manifest.name.clone();
    let cfg = prepare_modpack_server(&app, cfg).await?;
    let root = server_dir(&cfg)?;
    let total = manifest.files.len() as u32;
    for (idx, file) in manifest.files.iter().enumerate() {
        // Skip mods that are explicitly unsupported on the server
        if file.env.get("server").is_some_and(|v| v == "unsupported") {
            continue;
        }
        // Skip client-only mods: if the mod is marked as client-required
        // but has no server env specified, it's a client-side mod (e.g.
        // Sodium, Iris, shaders, minimap, etc.) that would crash a server.
        let server_env = file.env.get("server").map(|s| s.as_str());
        let client_env = file.env.get("client").map(|s| s.as_str());
        if server_env.is_none() && client_env == Some("required") {
            continue;
        }
        let url = file.downloads.first().ok_or_else(|| format!("No download URL for {}", file.path))?;
        let dest = safe_join(&root, &file.path)?;
        download_bytes_to_file(
            &app,
            url,
            &dest,
            "Downloading mods",
            &file.path,
            idx as u32 + 1,
            total,
        ).await?;
        verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
    }
    emit_mod_progress(&app, "Applying overrides", "Copying modpack override files", 1, 1);
    safe_extract_prefix(&mut zip, "overrides", &root)?;
    ensure_server_properties(&root, &cfg.server_name)?;

    // Auto-enable require-resource-pack if the modpack included resource packs
    let rp_dir = root.join("resourcepacks");
    if rp_dir.exists() {
        let has_packs = std::fs::read_dir(&rp_dir)
            .ok()
            .and_then(|mut d| d.find_map(|e| e.ok().filter(|e| e.path().extension().is_some_and(|ext| ext == "zip"))))
            .is_some();
        if has_packs {
            let _ = update_resource_pack_requirement(&cfg, true);
        }
    }

    emit_mod_progress(&app, "Finalizing", "Modpack is ready", 1, 1);
    Ok(cfg)
}

async fn resolve_or_download_mrpack(app: &std::sync::Arc<crate::app_state::AppEventSender>, source: &str) -> Result<PathBuf, String> {
    let url = if source.ends_with(".mrpack") {
        source.to_string()
    } else {
        let slug = source
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .ok_or("Could not read Modrinth modpack link")?;
        let versions: Vec<ModrinthVersion> = client()?.get(format!("https://api.modrinth.com/v2/project/{}/version", slug))
            .send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        let version = versions.first().ok_or("No compatible Modrinth pack version found for this profile.")?;
        primary_file(version)?.url
    };
    let dest = std::env::temp_dir().join(format!("lbby-pack-{}.mrpack", uuid::Uuid::new_v4().simple()));
    download_bytes_to_file(app, &url, &dest, "Downloading modpack", "Modrinth pack", 1, 1).await?;
    Ok(dest)
}

fn loader_from_curse(loaders: &[CurseLoader]) -> Result<(ServerType, Option<String>), String> {
    let selected = loaders.iter().find(|l| l.primary).or_else(|| loaders.first()).ok_or("CurseForge manifest has no mod loader")?;
    let mut parts = selected.id.splitn(2, '-');
    let kind = parts.next().unwrap_or_default();
    let version = parts.next().map(str::to_string);
    match kind {
        "forge" => Ok((ServerType::Forge, version)),
        "fabric" => Ok((ServerType::Fabric, version)),
        "neoforge" => Ok((ServerType::NeoForge, version)),
        "quilt" => Err("This pack uses Quilt. Lbby does not support installing Quilt servers yet.".to_string()),
        _ => Err(format!("Unsupported CurseForge loader: {}", selected.id)),
    }
}

pub async fn install_curseforge_modpack(app: std::sync::Arc<crate::app_state::AppEventSender>, zip_path: String) -> Result<ServerConfig, String> {
    let api_key = config::load_config().curseforge_api_key;
    if api_key.trim().is_empty() {
        return Err("CurseForge modpack install requires a CurseForge API key. Add it in Settings first.".to_string());
    }
    let file = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("Invalid CurseForge ZIP: {}", e))?;
    let manifest: CurseManifest = read_zip_json(&mut zip, "manifest.json")?;
    let (server_type, loader_version) = loader_from_curse(&manifest.minecraft.mod_loaders)?;
    let mut cfg = config::load_config();
    if let Ok(root) = server_dir(&cfg) {
        backup_modpack_targets(&root)?;
    }
    cfg.minecraft_version = manifest.minecraft.version;
    cfg.server_type = server_type;
    cfg.loader_version = loader_version;
    if let Some(name) = manifest.name.clone().filter(|n| !n.trim().is_empty()) {
        cfg.server_name = name;
    }
    let cfg = prepare_modpack_server(&app, cfg).await?;
    let root = server_dir(&cfg)?;
    let target_dir = mods_dir(&cfg)?;
    tokio::fs::create_dir_all(&target_dir).await.map_err(|e| e.to_string())?;
    let total = manifest.files.iter().filter(|f| f.required).count() as u32;
    let cf = client()?;
    let mut current = 0;
    for item in manifest.files.iter().filter(|f| f.required) {
        current += 1;
        emit_mod_progress(&app, "Resolving CurseForge file", &format!("{} / {}", current, total), current, total);
        let resp: CurseDownloadUrl = cf
            .get(format!("https://api.curseforge.com/v1/mods/{}/files/{}/download-url", item.project_id, item.file_id))
            .header("x-api-key", api_key.trim())
            .send().await.map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        let file_name = resp.data.rsplit('/').next().unwrap_or("mod.jar").split('?').next().unwrap_or("mod.jar");
        download_bytes_to_file(&app, &resp.data, &target_dir.join(file_name), "Downloading CurseForge mods", file_name, current, total).await?;
    }
    if let Some(overrides) = manifest.overrides.as_deref() {
        emit_mod_progress(&app, "Applying overrides", "Copying CurseForge override files", 1, 1);
        safe_extract_prefix(&mut zip, overrides, &root)?;
    }
    ensure_server_properties(&root, &cfg.server_name)?;

    // Auto-enable require-resource-pack if the modpack included resource packs
    let rp_dir = root.join("resourcepacks");
    if rp_dir.exists() {
        let has_packs = std::fs::read_dir(&rp_dir)
            .ok()
            .and_then(|mut d| d.find_map(|e| e.ok().filter(|e| e.path().extension().is_some_and(|ext| ext == "zip"))))
            .is_some();
        if has_packs {
            let _ = update_resource_pack_requirement(&cfg, true);
        }
    }

    emit_mod_progress(&app, "Finalizing", "CurseForge modpack is ready", 1, 1);
    Ok(cfg)
}

fn ensure_server_properties(root: &Path, server_name: &str) -> Result<(), String> {
    let path = root.join("server.properties");
    let mut props = std::fs::read_to_string(&path).unwrap_or_default();
    if !props.lines().any(|l| l.starts_with("motd=")) {
        props.push_str(&format!("\nmotd={}\n", server_name));
    }
    if !props.lines().any(|l| l.starts_with("online-mode=")) {
        props.push_str("online-mode=false\n");
    }
    std::fs::write(&path, props).map_err(|e| e.to_string())
}

fn backup_modpack_targets(root: &Path) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_root = root.join(".lbby-modpack-backups").join(stamp);
    let targets = ["mods", "config", "server.properties"];
    for target in targets {
        let src = root.join(target);
        if !src.exists() {
            continue;
        }
        let dest = backup_root.join(target);
        if src.is_dir() {
            copy_dir_recursive(&src, &dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&src, &dest).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub fn list_resource_packs() -> Result<Vec<ResourcePackInfo>, String> {
    let cfg = config::load_config();
    let dir = server_dir(&cfg)?.join("resourcepacks");
    if !dir.exists() { return Ok(vec![]); }
    let mut packs = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        let path = entry.path();
        let is_zip = path.extension().is_some_and(|x| x == "zip");
        if !meta.is_dir() && !is_zip {
            continue;
        }
        packs.push(ResourcePackInfo {
            name: entry.file_name().to_string_lossy().to_string(),
            kind: if meta.is_dir() { "folder".to_string() } else { "zip".to_string() },
            bytes: meta.len(),
        });
    }
    packs.sort_by_key(|p| p.name.to_lowercase());
    Ok(packs)
}

pub async fn add_resource_pack(file_path: String, overwrite: bool) -> Result<Vec<ResourcePackInfo>, String> {
    let cfg = config::load_config();
    let src = PathBuf::from(&file_path);
    let meta = tokio::fs::metadata(&src).await.map_err(|e| e.to_string())?;
    let name = src.file_name().ok_or("Invalid resource pack path")?.to_string_lossy().to_string();
    let is_zip = src.extension().is_some_and(|x| x == "zip");
    if !meta.is_dir() && !is_zip {
        return Err("Resource packs must be .zip files or folders.".to_string());
    }
    let dir = server_dir(&cfg)?.join("resourcepacks");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let dest = safe_join(&dir, &name)?;
    let dest_exists = tokio::fs::try_exists(&dest).await.unwrap_or(false);
    if dest_exists && !overwrite {
        return Err(format!("A resource pack named {} already exists.", name));
    }
    if dest_exists {
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&dest).await.map_err(|e| e.to_string())?;
        } else {
            tokio::fs::remove_file(&dest).await.map_err(|e| e.to_string())?;
        }
    }
    if meta.is_dir() {
        copy_dir_recursive(&src, &dest)?;
    } else {
        tokio::fs::copy(&src, &dest).await.map_err(|e| e.to_string())?;
    }
    // Auto-enable resource pack requirement in server.properties
    update_resource_pack_requirement(&cfg, true)?;
    list_resource_packs()
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.metadata().map_err(|e| e.to_string())?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub async fn remove_resource_pack(name: String) -> Result<Vec<ResourcePackInfo>, String> {
    let cfg = config::load_config();
    let dir = server_dir(&cfg)?.join("resourcepacks");
    let path = safe_join(&dir, &name)?;
    if path.is_dir() {
        tokio::fs::remove_dir_all(&path).await.map_err(|e| e.to_string())?;
    } else {
        tokio::fs::remove_file(&path).await.map_err(|e| e.to_string())?;
    }
    // If no more resource packs, disable requirement
    let remaining = list_resource_packs_internal(&cfg)?;
    if remaining.is_empty() {
        update_resource_pack_requirement(&cfg, false)?;
    }
    list_resource_packs()
}

/// Update require-resource-pack setting in server.properties.
fn update_resource_pack_requirement(cfg: &crate::config::ServerConfig, require: bool) -> Result<(), String> {
    let path = std::path::PathBuf::from(&cfg.server_path).join("server.properties");
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let key = "require-resource-pack=";
    let value = if require { "true" } else { "false" };
    let mut found = false;
    for line in lines.iter_mut() {
        if line.starts_with(key) {
            *line = format!("{}{}", key, value);
            found = true;
            break;
        }
    }
    if !found {
        lines.push(format!("{}{}", key, value));
    }
    std::fs::write(&path, lines.join("\n") + "\n").map_err(|e| e.to_string())
}

/// Internal helper to list resource packs without going through the public API.
fn list_resource_packs_internal(cfg: &crate::config::ServerConfig) -> Result<Vec<ResourcePackInfo>, String> {
    let dir = server_dir(cfg)?.join("resourcepacks");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut packs = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        let is_dir = meta.is_dir();
        let is_zip = name.ends_with(".zip");
        if is_dir || is_zip {
            let kind = if is_dir { "folder" } else { "zip" }.to_string();
            let bytes = if is_dir { 0 } else { meta.len() };
            packs.push(ResourcePackInfo { name, kind, bytes });
        }
    }
    packs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(packs)
}

pub fn open_mods_folder() -> Result<(), String> {
    open_folder(&mods_dir(&config::load_config())?)
}

pub fn open_resource_packs_folder() -> Result<(), String> {
    let cfg = config::load_config();
    let dir = server_dir(&cfg)?.join("resourcepacks");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    open_folder(&dir)
}

fn open_folder(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(path).spawn().map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(path);
        crate::helpers::hide_std_child_window(&mut cmd);
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open").arg(path).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn install_modpack_from_file(app: std::sync::Arc<crate::app_state::AppEventSender>, file_path: String) -> Result<ServerConfig, String> {
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".mrpack") {
        install_modrinth_modpack(app, file_path).await
    } else if lower.ends_with(".zip") {
        install_curseforge_modpack(app, file_path).await
    } else {
        Err("Choose a .mrpack or CurseForge .zip file.".to_string())
    }
}

pub async fn add_mod(file_path: String, overwrite: Option<bool>) -> Result<(), String> {
    let cfg = config::load_config();
    let src = PathBuf::from(&file_path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let allowed = match cfg.server_type {
        ServerType::Terraria | ServerType::TModLoader => ext == "tmod",
        _ => ext == "jar",
    };
    if !allowed {
        return Err(format!("Only {} files can be imported as mods for this server type.",
            if cfg.is_terraria() { ".tmod" } else { ".jar" }));
    }
    let name = src.file_name().ok_or("Invalid file path")?.to_string_lossy().to_string();
    let dest = mods_dir(&cfg)?.join(&name);
    tokio::fs::create_dir_all(dest.parent().unwrap()).await.map_err(|e| e.to_string())?;
    if dest.exists() && !overwrite.unwrap_or(false) {
        return Err(format!("A mod named {} already exists.", name));
    }
    tokio::fs::copy(&src, &dest).await.map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn remove_mod(mod_name: String) -> Result<(), String> {
    let cfg = config::load_config();
    let path = mods_dir(&cfg)?.join(&mod_name);
    tokio::fs::remove_file(&path).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Deletes every mod file in the mods/plugins folder. Returns the number
/// of files removed. Used by the "Remove all mods" button in the UI — the
/// frontend is responsible for confirming with the user before invoking.
pub async fn remove_all_mods() -> Result<u32, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(0);
    }
    let is_terraria = cfg.is_terraria();
    let mut removed: u32 = 0;
    let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let is_mod = if is_terraria { ext == "tmod" } else { ext == "jar" };
        if !is_mod {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            return Err(format!(
                "Failed to remove '{}': {}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                e
            ));
        }
        removed += 1;
    }
    Ok(removed)
}
