use crate::config::{self, ServerConfig, ServerType};
use crate::{
    app_state::ModInfo,
    helpers::{default_server_path_value, read_mod_info},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek};
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

// ── CurseForge Search & API Types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CurseFilesResponse {
    pub data: Vec<CurseFileEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurseFileEntry {
    pub id: i64,
    #[serde(default, rename = "fileName")]
    pub file_name: String,
    #[serde(default, rename = "fileLength")]
    pub file_length: u64,
    #[serde(default, rename = "downloadUrl")]
    pub download_url: Option<String>,
    #[serde(default, rename = "serverPackFileId")]
    pub server_pack_file_id: Option<i64>,
    #[serde(default, rename = "isServerPack")]
    pub is_server_pack: bool,
    #[serde(default, rename = "parentProjectFileId")]
    pub parent_project_file_id: Option<i64>,
    #[serde(default, rename = "gameVersions")]
    pub game_versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CurseFingerprintResponse {
    pub data: CurseFingerprintData,
}

#[derive(Debug, Deserialize)]
pub struct CurseFingerprintData {
    #[serde(default, rename = "exactMatches")]
    pub exact_matches: Vec<CurseFingerprintMatch>,
}

#[derive(Debug, Deserialize)]
pub struct CurseFingerprintMatch {
    pub file: CurseFileEntry,
}

pub const UNVERIFIED_CURSEFORGE_ZIP: &str = "UNVERIFIED_CURSEFORGE_ZIP:";

// ── CurseForge Helpers ──────────────────────────────────────────────────────

pub fn response_preview(body: &str) -> String {
    body.chars().take(200).collect()
}

/// Resolve CurseForge source to (mod_id, optional_file_id).
/// Source can be: numeric ID, slug, or URL like .../modpacks/{slug}/files/{fileId}
pub fn parse_curseforge_source(source: &str) -> (String, Option<i64>) {
    let trimmed = source.trim().trim_end_matches('/');
    // Extract from URL: .../modpacks/{slug}/files/{fileId}
    if let Some(after_modpacks) = trimmed.split("/modpacks/").nth(1) {
        let parts: Vec<&str> = after_modpacks.split('/').collect();
        let slug = parts[0];
        let file_id = if parts.len() >= 3 && parts[1] == "files" {
            parts[2].parse::<i64>().ok()
        } else {
            None
        };
        return (slug.to_string(), file_id);
    }
    // Plain numeric ID
    if let Ok(id) = trimmed.parse::<i64>() {
        return (id.to_string(), None);
    }
    // Assume it's a slug
    (trimmed.to_string(), None)
}

pub fn curseforge_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!(
            "Lbby/{} (Minecraft server hosting app)",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

pub fn expected_curseforge_loader(server_type: &ServerType) -> Option<&'static str> {
    match server_type {
        ServerType::Forge => Some("forge"),
        ServerType::Fabric => Some("fabric"),
        ServerType::NeoForge => Some("neoforge"),
        _ => None,
    }
}

pub fn validate_curseforge_file_for_profile(
    file: &CurseFileEntry,
    cfg: &ServerConfig,
) -> Result<(), String> {
    let has_mc_version = file
        .game_versions
        .iter()
        .any(|version| version.eq_ignore_ascii_case(cfg.minecraft_version.trim()));
    if !has_mc_version {
        return Err(format!(
            "CurseForge file {} is not for Minecraft {}. Available metadata: {}",
            file.id,
            cfg.minecraft_version,
            if file.game_versions.is_empty() {
                "none".to_string()
            } else {
                file.game_versions.join(", ")
            }
        ));
    }

    let Some(expected_loader) = expected_curseforge_loader(&cfg.server_type) else {
        return Ok(());
    };
    let known_loaders = ["forge", "fabric", "neoforge", "quilt"];
    let declared_loaders: Vec<String> = file
        .game_versions
        .iter()
        .filter(|value| known_loaders.iter().any(|loader| value.eq_ignore_ascii_case(loader)))
        .map(|value| value.to_ascii_lowercase())
        .collect();
    if !declared_loaders
        .iter()
        .any(|loader| loader == expected_loader)
    {
        return Err(format!(
            "CurseForge file {} does not match the profile loader {}. Declared loaders: {}",
            file.id,
            expected_loader,
            if declared_loaders.is_empty() {
                "none".to_string()
            } else {
                declared_loaders.join(", ")
            }
        ));
    }
    Ok(())
}

pub async fn curseforge_file_by_id(
    client: &reqwest::Client,
    api_key: &str,
    file_id: i64,
) -> Result<CurseFileEntry, String> {
    let response = client
        .post("https://api.curseforge.com/v1/mods/files")
        .header("x-api-key", api_key)
        .json(&serde_json::json!({"fileIds": [file_id]}))
        .send()
        .await
        .map_err(|e| format!("CurseForge file lookup error: {}", e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "CurseForge file lookup error ({}): {}",
            status,
            response_preview(&body)
        ));
    }
    serde_json::from_str::<CurseFilesResponse>(&body)
        .map_err(|e| format!("CurseForge file parse error: {}", e))?
        .data
        .into_iter()
        .next()
        .ok_or_else(|| format!("CurseForge file {} was not found", file_id))
}

pub async fn curseforge_validation_file(
    client: &reqwest::Client,
    api_key: &str,
    file: &CurseFileEntry,
) -> Result<CurseFileEntry, String> {
    if file.is_server_pack {
        if let Some(parent_id) = file.parent_project_file_id.filter(|id| *id > 0) {
            return curseforge_file_by_id(client, api_key, parent_id).await;
        }
    }
    Ok(file.clone())
}

// ── CurseForge Fingerprinting ──────────────────────────────────────────────

fn is_curseforge_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\r' | b' ')
}

fn murmur2_block(hash: &mut u32, block: [u8; 4]) {
    const M: u32 = 0x5bd1_e995;
    let mut value = u32::from_le_bytes(block);
    value = value.wrapping_mul(M);
    value ^= value >> 24;
    value = value.wrapping_mul(M);
    *hash = hash.wrapping_mul(M) ^ value;
}

pub fn curseforge_fingerprint_reader<R: Read>(mut reader: R, normalized_len: usize) -> Result<u32, String> {
    const M: u32 = 0x5bd1_e995;
    let normalized_len = u32::try_from(normalized_len)
        .map_err(|_| "CurseForge fingerprint input exceeds 4 GiB".to_string())?;
    let mut hash = 1u32 ^ normalized_len;
    let mut input = [0u8; 64 * 1024];
    let mut block = [0u8; 4];
    let mut block_len = 0usize;
    loop {
        let read = reader.read(&mut input).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        for byte in input[..read]
            .iter()
            .copied()
            .filter(|byte| !is_curseforge_whitespace(*byte))
        {
            block[block_len] = byte;
            block_len += 1;
            if block_len == 4 {
                murmur2_block(&mut hash, block);
                block_len = 0;
                block = [0u8; 4];
            }
        }
    }
    match block_len {
        3 => {
            hash ^= (block[2] as u32) << 16;
            hash ^= (block[1] as u32) << 8;
            hash ^= block[0] as u32;
            hash = hash.wrapping_mul(M);
        }
        2 => {
            hash ^= (block[1] as u32) << 8;
            hash ^= block[0] as u32;
            hash = hash.wrapping_mul(M);
        }
        1 => {
            hash ^= block[0] as u32;
            hash = hash.wrapping_mul(M);
        }
        _ => {}
    }
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(M);
    hash ^= hash >> 15;
    Ok(hash)
}

pub fn curseforge_fingerprint_file(path: &Path) -> Result<u32, String> {
    let mut counter = BufReader::new(std::fs::File::open(path).map_err(|e| e.to_string())?);
    let mut input = [0u8; 64 * 1024];
    let mut normalized_len = 0usize;
    loop {
        let read = counter.read(&mut input).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        normalized_len += input[..read]
            .iter()
            .filter(|byte| !is_curseforge_whitespace(**byte))
            .count();
    }
    let reader = BufReader::new(std::fs::File::open(path).map_err(|e| e.to_string())?);
    curseforge_fingerprint_reader(reader, normalized_len)
}

pub async fn identify_curseforge_upload(
    client: &reqwest::Client,
    api_key: &str,
    path: &Path,
) -> Result<Option<CurseFileEntry>, String> {
    let path = path.to_path_buf();
    let fingerprint = tokio::task::spawn_blocking(move || curseforge_fingerprint_file(&path))
        .await
        .map_err(|e| format!("Fingerprint task failed: {}", e))??;
    let response = client
        .post("https://api.curseforge.com/v1/fingerprints/432")
        .header("x-api-key", api_key)
        .json(&serde_json::json!({"fingerprints": [fingerprint]}))
        .send()
        .await
        .map_err(|e| format!("CurseForge fingerprint lookup failed: {}", e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "CurseForge fingerprint lookup failed ({}): {}",
            status,
            response_preview(&body)
        ));
    }
    let response: CurseFingerprintResponse = serde_json::from_str(&body)
        .map_err(|e| format!("CurseForge fingerprint response was invalid: {}", e))?;
    Ok(response
        .data
        .exact_matches
        .into_iter()
        .next()
        .map(|matched| matched.file))
}

// ── CurseForge Download ────────────────────────────────────────────────────

pub fn official_server_pack_id(file: &CurseFileEntry) -> Option<i64> {
    (!file.is_server_pack)
        .then_some(file.server_pack_file_id)
        .flatten()
        .filter(|id| *id > 0)
}

pub fn curseforge_cdn_parts(file_id: i64) -> Result<(String, String), String> {
    if file_id <= 0 {
        return Err("CurseForge returned an invalid file ID".to_string());
    }
    let digits = file_id.to_string();
    if digits.len() <= 4 {
        return Err("CurseForge returned a file ID that is too short".to_string());
    }
    let prefix = digits[..4].to_string();
    let suffix = digits[4..].trim_start_matches('0');
    Ok((
        prefix,
        if suffix.is_empty() { "0" } else { suffix }.to_string(),
    ))
}

pub async fn prefer_curseforge_server_pack(
    app: &crate::app_state::AppEventSender,
    client: &reqwest::Client,
    api_key: &str,
    client_file: CurseFileEntry,
) -> Result<CurseFileEntry, String> {
    if client_file.is_server_pack {
        return Ok(client_file);
    }

    // 1. Check official serverPackFileId on the client file
    if let Some(server_pack_id) = official_server_pack_id(&client_file) {
        let _ = app.emit(
            "mod-task-progress",
            ModTaskProgress {
                stage: "Selecting server pack".to_string(),
                message: format!("Using official CurseForge server pack {}", server_pack_id),
                current: 1,
                total: 1,
                progress: 1.0,
            },
        );
        return curseforge_file_by_id(client, api_key, server_pack_id).await;
    }

    // 2. Search CurseForge for a separate server pack mod (e.g. "{slug} server pack")
    let _ = app.emit(
        "mod-task-progress",
        ModTaskProgress {
            stage: "Searching for server pack".to_string(),
            message: "Looking for a dedicated server pack on CurseForge...".to_string(),
            current: 0,
            total: 1,
            progress: 0.0,
        },
    );

    // Extract mod name from file_name to build search query
    let mod_name = client_file
        .file_name
        .replace(".zip", "")
        .replace("-server", "")
        .replace("-client", "")
        .replace("_server", "")
        .replace("_client", "");
    let search_query = format!("{} server pack", mod_name);

    let search_url = format!(
        "https://api.curseforge.com/v1/mods/search?gameId=432&searchFilter={}&classId=4471&pageSize=5",
        urlencoding::encode(&search_query)
    );

    let resp = client
        .get(&search_url)
        .header("x-api-key", api_key)
        .send()
        .await;

    if let Ok(resp) = resp {
        if resp.status().is_success() {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                if let Some(hits) = data["data"].as_array() {
                    // Find a mod that looks like a server pack
                    for hit in hits {
                        let name = hit["name"].as_str().unwrap_or("").to_string();
                        let id = hit["id"].as_u64();
                        let slug = hit["slug"].as_str().unwrap_or("");

                        // Check if this mod's name contains "server" and is related to the original
                        if name.to_lowercase().contains("server")
                            && (slug.to_lowercase().contains(&mod_name.to_lowercase().replace(" ", "-"))
                                || name.to_lowercase().contains(&mod_name.to_lowercase()))
                        {
                            if let Some(mod_id) = id {
                                // Get the latest file for this server pack mod
                                let files_url = format!(
                                    "https://api.curseforge.com/v1/mods/{}/files?gameVersion={}&pageSize=1",
                                    mod_id,
                                    urlencoding::encode(&client_file.game_versions.first().map(|s| s.as_str()).unwrap_or(""))
                                );
                                if let Ok(files_resp) = client
                                    .get(&files_url)
                                    .header("x-api-key", api_key)
                                    .send()
                                    .await
                                {
                                    if let Ok(files_data) = files_resp.json::<serde_json::Value>().await {
                                        if let Some(files) = files_data["data"].as_array() {
                                            if let Some(file) = files.first() {
                                                if let Ok(server_file) = serde_json::from_value::<CurseFileEntry>(file.clone()) {
                                                    let _ = app.emit(
                                                        "mod-task-progress",
                                                        ModTaskProgress {
                                                            stage: "Found server pack".to_string(),
                                                            message: format!("Using CurseForge server pack: {}", server_file.file_name),
                                                            current: 1,
                                                            total: 1,
                                                            progress: 1.0,
                                                        },
                                                    );
                                                    return Ok(server_file);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. No server pack found - use client pack
    let _ = app.emit(
        "mod-task-progress",
        ModTaskProgress {
            stage: "No server pack found".to_string(),
            message: "No dedicated server pack found; using the client modpack.".to_string(),
            current: 1,
            total: 1,
            progress: 1.0,
        },
    );
    Ok(client_file)
}

pub async fn download_curseforge_file(
    app: &crate::app_state::AppEventSender,
    client: &reqwest::Client,
    file: &CurseFileEntry,
) -> Result<PathBuf, String> {
    let safe_file_name = Path::new(&file.file_name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or("CurseForge returned an invalid file name")?;
    let (prefix, suffix) = curseforge_cdn_parts(file.id)?;
    let encoded_name = urlencoding::encode(safe_file_name);
    let download_url = file
        .download_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "https://edge.forgecdn.net/files/{}/{}/{}",
                prefix, suffix, encoded_name
            )
        });
    let temp_dir = std::env::temp_dir()
        .join("lbby-curseforge")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let zip_path = temp_dir.join(safe_file_name);

    let mut response = None;
    for attempt in 1..=3 {
        match client.get(&download_url).send().await {
            Ok(candidate) if candidate.status().is_success() => {
                response = Some(candidate);
                break;
            }
            Ok(candidate) if attempt == 3 => {
                return Err(format!("CurseForge CDN error ({})", candidate.status()));
            }
            Err(error) if attempt == 3 => {
                return Err(format!("CurseForge download error: {error}"));
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
        }
    }
    let response = response.ok_or("CurseForge download failed after 3 attempts")?;
    let total_size = response.content_length().unwrap_or(file.file_length);
    let mut downloaded = 0u64;
    let mut stream = response.bytes_stream();
    let mut writer = std::fs::File::create(&zip_path)
        .map_err(|e| format!("Failed to create downloaded modpack: {e}"))?;
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("CurseForge download stream error: {e}"))?;
        std::io::Write::write_all(&mut writer, &chunk)
            .map_err(|e| format!("Failed to write downloaded modpack: {e}"))?;
        downloaded += chunk.len() as u64;
        if last_emit.elapsed() >= std::time::Duration::from_millis(500) {
            let _ = app.emit(
                "mod-task-progress",
                ModTaskProgress {
                    stage: "Downloading modpack".to_string(),
                    message: format!("{} / {} MB", downloaded / 1024 / 1024, total_size / 1024 / 1024),
                    current: downloaded as u32,
                    total: total_size as u32,
                    progress: if total_size > 0 { downloaded as f32 / total_size as f32 } else { 0.0 },
                },
            );
            last_emit = std::time::Instant::now();
        }
    }
    writer
        .sync_all()
        .map_err(|e| format!("Failed to finalize downloaded modpack: {e}"))?;
    Ok(zip_path)
}

// ── CurseForge Search ──────────────────────────────────────────────────────

pub async fn search_curseforge_mods(
    query: String,
    minecraft_version: String,
    _server_type: ServerType,
) -> Result<Vec<ModrinthSearchHit>, String> {
    let api_key = crate::config::load_config()
        .curseforge_api_key
        .filter(|k| !k.trim().is_empty())
        .ok_or("CurseForge search requires an API key")?;

    let client = curseforge_http_client()?;
    let url = format!(
        "https://api.curseforge.com/v1/mods/search?gameId=432&searchFilter={}&gameVersion={}&classId=6&pageSize=20",
        urlencoding::encode(&query),
        urlencoding::encode(&minecraft_version)
    );

    let resp = client
        .get(&url)
        .header("x-api-key", &api_key)
        .send()
        .await
        .map_err(|e| format!("CurseForge search error: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("CurseForge search failed ({}): {}", status, response_preview(&text)));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("CurseForge parse error: {}", e))?;
    let hits = data["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m["id"].as_u64()?;
                    let name = m["name"].as_str()?.to_string();
                    let slug = m["slug"].as_str()?.to_string();
                    let desc = m["summary"].as_str().unwrap_or("").to_string();
                    let icon = m.get("logo").and_then(|l| l["thumbnailUrl"].as_str()).map(|s| s.to_string());
                    let versions: Vec<String> = m.get("latestFiles")
                        .and_then(|f| f.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|f| f["gameVersion"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let loaders: Vec<String> = m.get("latestFiles")
                        .and_then(|f| f.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|f| f.get("gameVersion"))
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(ModrinthSearchHit {
                        project_id: id.to_string(),
                        slug,
                        title: name,
                        description: desc,
                        icon_url: icon,
                        versions,
                        loaders,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(hits)
}

fn emit_mod_progress(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    stage: &str,
    message: &str,
    current: u32,
    total: u32,
) {
    let progress = if total > 0 {
        current as f32 / total as f32
    } else {
        0.0
    };
    app.emit(
        "mod-task-progress",
        ModTaskProgress {
            stage: stage.to_string(),
            message: message.to_string(),
            current,
            total,
            progress,
        },
    )
    .ok();
}

pub(crate) fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

fn curseforge_client() -> Result<reqwest::Client, String> {
    // CurseForge official API requires API key
    // If no API key is configured, return error with helpful message
    let api_key = config::load_config()
        .curseforge_api_key
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            "CurseForge API key is required. Get one from https://curseforge.com/account/api-tokens and add it in Settings.".to_string()
        })?;
    
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
        .timeout(std::time::Duration::from_secs(30))
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "x-api-key",
                reqwest::header::HeaderValue::from_str(api_key.trim())
                    .map_err(|_| "Invalid CurseForge API key".to_string())?,
            );
            headers
        })
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
        ServerType::Paper
        | ServerType::Bukkit
        | ServerType::Spigot
        | ServerType::Folia
        | ServerType::Purpur => "plugins",
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
        let mut output = std::fs::File::create(&out)
            .map_err(|e| format!("Failed to create {}: {}", out.display(), e))?;
        std::io::copy(&mut file, &mut output).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn apply_mrpack_overrides<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    dest: &Path,
) -> Result<(), String> {
    // `overrides` applies to both sides. Server-specific files must be applied
    // afterwards so they win conflicts. `client-overrides` is intentionally
    // never extracted into a dedicated server.
    safe_extract_prefix(zip, "overrides", dest)?;
    safe_extract_prefix(zip, "server-overrides", dest)
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
    let resp = client()?
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
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let size = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| e.to_string())?;
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
    let Some(expected) = expected else {
        return Ok(());
    };
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
        ServerType::Paper
            | ServerType::Bukkit
            | ServerType::Spigot
            | ServerType::Folia
            | ServerType::Purpur
            | ServerType::SpongeVanilla
            | ServerType::SpongeForge
    )
}

async fn latest_modrinth_version(
    project_id: &str,
    cfg: &ServerConfig,
) -> Result<ModrinthVersion, String> {
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
    let versions: Vec<ModrinthVersion> = client()?
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    versions.into_iter().next().ok_or_else(|| {
        format!(
            "No compatible version found for Minecraft {} / {}.",
            cfg.minecraft_version, loader
        )
    })
}

fn primary_file(version: &ModrinthVersion) -> Result<ModrinthFile, String> {
    version
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
        .cloned()
        .ok_or_else(|| "Modrinth version has no downloadable files.".to_string())
}

pub async fn search_modrinth_mods(
    query: String,
    mc_version: String,
    loader: String,
    project_type: Option<String>,
) -> Result<Vec<ModrinthSearchHit>, String> {
    // Both mod loaders and plugin platforms are valid categories on Modrinth.
    let valid_loaders = [
        "forge", "fabric", "neoforge", "paper", "bukkit", "spigot", "folia", "purpur", "sponge",
    ];
    if !valid_loaders.contains(&loader.as_str()) {
        return Err(format!("Unsupported loader: {}", loader));
    }
    // Determine project type: explicit param > infer from loader
    let pt = project_type.unwrap_or_else(|| match loader.as_str() {
        "paper" | "bukkit" | "spigot" | "folia" | "purpur" | "sponge" => "plugin".to_string(),
        _ => "mod".to_string(),
    });
    let facets = format!(
        "[[\"project_type:{}\"],[\"versions:{}\"],[\"categories:{}\"]]",
        pt, mc_version, loader
    );
    let resp: ModrinthSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[
            ("query", query),
            ("facets", facets),
            ("limit", "20".to_string()),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp
        .hits
        .into_iter()
        .map(|hit| ModrinthSearchHit {
            project_id: hit.project_id,
            slug: hit.slug,
            title: hit.title,
            description: hit.description,
            icon_url: hit.icon_url,
            versions: hit.versions,
            loaders: hit
                .categories
                .into_iter()
                .filter(|c| {
                    matches!(
                        c.as_str(),
                        "forge"
                            | "fabric"
                            | "neoforge"
                            | "quilt"
                            | "paper"
                            | "bukkit"
                            | "spigot"
                            | "folia"
                            | "purpur"
                            | "sponge"
                    )
                })
                .collect(),
        })
        .collect())
}

/// Search Modrinth for resource packs compatible with the given MC version.
pub async fn search_modrinth_resource_packs(
    query: String,
    mc_version: String,
) -> Result<Vec<ModrinthSearchHit>, String> {
    let facets = format!(
        "[[\"project_type:resourcepack\"],[\"versions:{}\"]]",
        mc_version
    );
    let resp: ModrinthSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[
            ("query", query),
            ("facets", facets),
            ("limit", "20".to_string()),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp
        .hits
        .into_iter()
        .map(|hit| ModrinthSearchHit {
            project_id: hit.project_id,
            slug: hit.slug,
            title: hit.title,
            description: hit.description,
            icon_url: hit.icon_url,
            versions: hit.versions,
            loaders: vec![],
        })
        .collect())
}

/// Search Modrinth for shader packs compatible with the given MC version.
pub async fn search_modrinth_shader_packs(
    query: String,
    mc_version: String,
) -> Result<Vec<ModrinthSearchHit>, String> {
    let facets = format!("[[\"project_type:shader\"],[\"versions:{}\"]]", mc_version);
    let resp: ModrinthSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[
            ("query", query),
            ("facets", facets),
            ("limit", "20".to_string()),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp
        .hits
        .into_iter()
        .map(|hit| ModrinthSearchHit {
            project_id: hit.project_id,
            slug: hit.slug,
            title: hit.title,
            description: hit.description,
            icon_url: hit.icon_url,
            versions: hit.versions,
            loaders: vec![],
        })
        .collect())
}

/// Install a shader pack from Modrinth into the server's shaderpacks/ folder.
pub async fn install_modrinth_shader_pack(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    project_id: String,
) -> Result<(), String> {
    let cfg = config::load_config();
    let root = server_dir(&cfg)?;
    let shader_dir = root.join("shaderpacks");
    tokio::fs::create_dir_all(&shader_dir)
        .await
        .map_err(|e| e.to_string())?;

    let version = latest_modrinth_version(&project_id, &cfg).await?;
    let file = primary_file(&version)?;
    let dest = shader_dir.join(&file.filename);

    emit_mod_progress(
        &app,
        "Downloading shader pack",
        &format!("Installing {}", version.name),
        1,
        1,
    );
    download_bytes_to_file(
        &app,
        &file.url,
        &dest,
        "Downloading shader pack",
        &file.filename,
        1,
        1,
    )
    .await?;
    verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;

    Ok(())
}

pub async fn install_modrinth_mod(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    project_id: String,
) -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let target_dir = mods_dir(&cfg)?;
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(|e| e.to_string())?;
    let mut installed_projects = HashSet::new();
    install_modrinth_project_recursive(
        &app,
        &cfg,
        &target_dir,
        &project_id,
        &mut installed_projects,
        1,
        1,
    )
    .await?;
    list_installed_mods()
}

/// Install a resource pack from Modrinth by project ID.
/// Downloads the .zip to the resourcepacks/ directory and auto-enables
/// require-resource-pack in server.properties. Also installs required
/// dependency resource packs recursively.
pub async fn install_modrinth_resource_pack(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    project_id: String,
) -> Result<Vec<ResourcePackInfo>, String> {
    let cfg = config::load_config();
    let root = server_dir(&cfg)?;
    let rp_dir = root.join("resourcepacks");
    tokio::fs::create_dir_all(&rp_dir)
        .await
        .map_err(|e| e.to_string())?;

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
    for dep in version
        .dependencies
        .iter()
        .filter(|d| d.dependency_type == "required")
    {
        if let Some(dep_project_id) = dep.project_id.as_deref() {
            Box::pin(install_resource_pack_recursive(
                app,
                cfg,
                rp_dir,
                dep_project_id,
                installed,
            ))
            .await?;
        }
    }

    let file = primary_file(&version)?;
    let dest = rp_dir.join(&file.filename);

    emit_mod_progress(
        app,
        "Downloading resource pack",
        &format!("Installing {}", version.name),
        1,
        1,
    );
    download_bytes_to_file(
        app,
        &file.url,
        &dest,
        "Downloading resource pack",
        &file.filename,
        1,
        1,
    )
    .await?;
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
    for dep in version
        .dependencies
        .iter()
        .filter(|d| d.dependency_type == "required")
    {
        if let Some(dep_project_id) = dep.project_id.as_deref() {
            Box::pin(install_modrinth_project_recursive(
                app,
                cfg,
                target_dir,
                dep_project_id,
                installed_projects,
                current,
                total,
            ))
            .await?;
        } else if let Some(dep_version_id) = dep.version_id.as_deref() {
            let dep_version: ModrinthVersion = client()?
                .get(format!(
                    "https://api.modrinth.com/v2/version/{}",
                    dep_version_id
                ))
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            let file = primary_file(&dep_version)?;
            let dest = target_dir.join(&file.filename);
            download_bytes_to_file(
                app,
                &file.url,
                &dest,
                "Downloading dependency",
                &file.filename,
                current,
                total,
            )
            .await?;
            verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
        }
    }
    let file = primary_file(&version)?;
    let dest = target_dir.join(&file.filename);
    emit_mod_progress(
        app,
        "Downloading mod",
        &format!("Installing {}", version.name),
        current,
        total,
    );
    download_bytes_to_file(
        app,
        &file.url,
        &dest,
        "Downloading mod",
        &file.filename,
        current,
        total,
    )
    .await?;
    verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
    Ok(())
}

pub fn list_installed_mods() -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    // Terraria uses .tmod files, Minecraft uses .jar files
    let ext = if cfg.is_terraria() { "tmod" } else { "jar" };
    let mut mods: Vec<ModInfo> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
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
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(vec![]);
    }
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
        let found: Result<ModrinthVersion, _> = client()?
            .get(format!(
                "https://api.modrinth.com/v2/version_file/{}?algorithm=sha512",
                hash
            ))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string());
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
            message: if latest.id != current_version.id {
                "Update available".to_string()
            } else {
                "Up to date".to_string()
            },
        });
    }
    Ok(out)
}

pub async fn update_mod(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    file_name: String,
    download_url: String,
) -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    let old = safe_join(&dir, &file_name)?;
    if !old.exists() {
        return Err("The old mod file no longer exists.".to_string());
    }
    let backup_dir = dir.join(".lbby-backups");
    tokio::fs::create_dir_all(&backup_dir)
        .await
        .map_err(|e| e.to_string())?;
    let backup = backup_dir.join(format!("{}.bak", file_name));
    tokio::fs::copy(&old, &backup)
        .await
        .map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{}.download", file_name));
    let result = async {
        download_bytes_to_file(&app, &download_url, &tmp, "Updating mod", &file_name, 1, 1).await?;
        tokio::fs::rename(&tmp, &old)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(err) = result {
        let _ = tokio::fs::copy(&backup, &old).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(format!("Update failed and old file was restored: {}", err));
    }
    list_installed_mods()
}

pub async fn update_all_mods(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    updates: Vec<ModUpdateInfo>,
) -> Result<Vec<ModInfo>, String> {
    let total = updates
        .iter()
        .filter(|u| u.outdated && u.download_url.is_some())
        .count() as u32;
    let mut current = 0;
    for update in updates.into_iter().filter(|u| u.outdated) {
        if let Some(url) = update.download_url {
            current += 1;
            emit_mod_progress(
                &app,
                "Updating mods",
                &format!("Updating {}", update.display_name),
                current,
                total,
            );
            update_mod(app.clone(), update.file_name, url).await?;
        }
    }
    list_installed_mods()
}

async fn prepare_modpack_server(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    mut cfg: ServerConfig,
) -> Result<ServerConfig, String> {
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

fn loader_from_mrpack(
    deps: &HashMap<String, String>,
) -> Result<(ServerType, Option<String>), String> {
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
        return Err(
            "This pack uses Quilt. Lbby does not support installing Quilt servers yet.".to_string(),
        );
    }
    Ok((ServerType::Vanilla, None))
}

pub async fn install_modrinth_modpack(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    source: String,
) -> Result<ServerConfig, String> {
    emit_mod_progress(&app, "Reading manifest", "Preparing Modrinth modpack", 0, 1);
    let pack_path = if source.starts_with("http://") || source.starts_with("https://") {
        resolve_or_download_mrpack(&app, &source).await?
    } else {
        PathBuf::from(source)
    };
    let file = std::fs::File::open(&pack_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("Invalid .mrpack file: {}", e))?;
    let manifest: MrpackManifest = read_zip_json(&mut zip, "modrinth.index.json")?;
    let mc = manifest
        .dependencies
        .get("minecraft")
        .cloned()
        .ok_or("Modpack manifest is missing Minecraft version")?;
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
        let url = file
            .downloads
            .first()
            .ok_or_else(|| format!("No download URL for {}", file.path))?;
        let dest = safe_join(&root, &file.path)?;
        download_bytes_to_file(
            &app,
            url,
            &dest,
            "Downloading mods",
            &file.path,
            idx as u32 + 1,
            total,
        )
        .await?;
        verify_sha512(&dest, file.hashes.get("sha512").map(String::as_str))?;
    }
    emit_mod_progress(
        &app,
        "Applying overrides",
        "Copying modpack override files",
        1,
        1,
    );
    apply_mrpack_overrides(&mut zip, &root)?;
    let quarantined = crate::mod_side::quarantine_client_only_mods(&root).await?;
    if !quarantined.is_empty() {
        emit_mod_progress(
            &app,
            "Filtering client-only mods",
            &format!("Quarantined {} client-only mod(s)", quarantined.len()),
            quarantined.len() as u32,
            quarantined.len() as u32,
        );
    }
    ensure_server_properties(&root, &cfg.server_name)?;

    // Auto-enable require-resource-pack if the modpack included resource packs
    let rp_dir = root.join("resourcepacks");
    if rp_dir.exists() {
        let has_packs = std::fs::read_dir(&rp_dir)
            .ok()
            .and_then(|mut d| {
                d.find_map(|e| {
                    e.ok()
                        .filter(|e| e.path().extension().is_some_and(|ext| ext == "zip"))
                })
            })
            .is_some();
        if has_packs {
            let _ = update_resource_pack_requirement(&cfg, true);
        }
    }

    emit_mod_progress(&app, "Finalizing", "Modpack is ready", 1, 1);
    Ok(cfg)
}

async fn resolve_or_download_mrpack(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    source: &str,
) -> Result<PathBuf, String> {
    let url = if source.ends_with(".mrpack") {
        source.to_string()
    } else {
        let slug = source
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .ok_or("Could not read Modrinth modpack link")?;
        let versions: Vec<ModrinthVersion> = client()?
            .get(format!(
                "https://api.modrinth.com/v2/project/{}/version",
                slug
            ))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let version = versions
            .first()
            .ok_or("No compatible Modrinth pack version found for this profile.")?;
        primary_file(version)?.url
    };
    let dest = std::env::temp_dir().join(format!(
        "lbby-pack-{}.mrpack",
        uuid::Uuid::new_v4().simple()
    ));
    download_bytes_to_file(
        app,
        &url,
        &dest,
        "Downloading modpack",
        "Modrinth pack",
        1,
        1,
    )
    .await?;
    Ok(dest)
}

fn loader_from_curse(loaders: &[CurseLoader]) -> Result<(ServerType, Option<String>), String> {
    let selected = loaders
        .iter()
        .find(|l| l.primary)
        .or_else(|| loaders.first())
        .ok_or("CurseForge manifest has no mod loader")?;
    let mut parts = selected.id.splitn(2, '-');
    let kind = parts.next().unwrap_or_default();
    let version = parts.next().map(str::to_string);
    match kind {
        "forge" => Ok((ServerType::Forge, version)),
        "fabric" => Ok((ServerType::Fabric, version)),
        "neoforge" => Ok((ServerType::NeoForge, version)),
        "quilt" => Err(
            "This pack uses Quilt. Lbby does not support installing Quilt servers yet.".to_string(),
        ),
        _ => Err(format!("Unsupported CurseForge loader: {}", selected.id)),
    }
}

pub async fn install_curseforge_modpack(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    zip_path: String,
) -> Result<ServerConfig, String> {
    let file = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("Invalid CurseForge ZIP: {}", e))?;
    let manifest_result: Result<CurseManifest, String> = read_zip_json(&mut zip, "manifest.json");
    if manifest_result.is_err() {
        // No manifest.json = server pack. Extract directly to server directory.
        eprintln!("[lbby] No manifest.json found — treating as server pack, extracting directly");
        let cfg = config::load_config();
        if let Ok(root) = server_dir(&cfg) {
            backup_modpack_targets(&root)?;
        }
        let cfg2 = prepare_modpack_server(&app, cfg).await?;
        let root = server_dir(&cfg2)?;
        let total = zip.len() as u32;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            let outpath = root.join(entry.mangled_name());
            if entry.is_dir() {
                std::fs::create_dir_all(&outpath).ok();
            } else {
                if let Some(parent) = outpath.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let mut outfile = std::fs::File::create(&outpath).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;
            }
            if i % 50 == 0 || i + 1 == total as usize {
                emit_mod_progress(&app, "Extracting server pack", &format!("{}/{} files", i + 1, total), (i + 1) as u32, total);
            }
        }
        eprintln!("[lbby] Server pack extracted {} files to {}", total, root.display());
        return Ok(cfg2);
    }
    let manifest = manifest_result.unwrap();
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
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(|e| e.to_string())?;
    let total = manifest.files.iter().filter(|f| f.required).count() as u32;
    let cf = curseforge_client()?;
    let mut current = 0;
    for item in manifest.files.iter().filter(|f| f.required) {
        current += 1;
        emit_mod_progress(
            &app,
            "Resolving CurseForge file",
            &format!("{} / {}", current, total),
            current,
            total,
        );
        // Use CurseForge official API to get download URL
        // API key is already included in curseforge_client() headers
        let download_url_endpoint = format!(
            "https://api.curseforge.com/v1/mods/{}/files/{}/download-url",
            item.project_id, item.file_id
        );
        
        let download_url = match cf.get(&download_url_endpoint).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    #[derive(serde::Deserialize)]
                    struct DownloadUrlResponse {
                        data: String,
                    }
                    match resp.json::<DownloadUrlResponse>().await {
                        Ok(info) => info.data,
                        Err(e) => {
                            eprintln!("[lbby] Failed to parse download URL: {}", e);
                            return Err(format!("Failed to get download URL for mod {}", item.project_id));
                        }
                    }
                } else {
                    eprintln!("[lbby] Download URL endpoint failed: {}", resp.status());
                    return Err(format!("Failed to get download URL for mod {} (HTTP {})", item.project_id, resp.status()));
                }
            }
            Err(e) => {
                eprintln!("[lbby] Download URL request failed: {}", e);
                return Err(format!("Failed to get download URL for mod {}: {}", item.project_id, e));
            }
        };
        
        // Extract filename from URL
        let file_name = download_url
            .rsplit('/')
            .next()
            .unwrap_or(&format!("mod_{}.jar", item.file_id))
            .split('?')
            .next()
            .unwrap_or(&format!("mod_{}.jar", item.file_id))
            .to_string();

        download_bytes_to_file(
            &app,
            &download_url,
            &target_dir.join(&file_name),
            "Downloading CurseForge mods",
            &file_name,
            current,
            total,
        )
        .await?;
    }
    if let Some(overrides) = manifest.overrides.as_deref() {
        emit_mod_progress(
            &app,
            "Applying overrides",
            "Copying CurseForge override files",
            1,
            1,
        );
        safe_extract_prefix(&mut zip, overrides, &root)?;
    }
    let quarantined = crate::mod_side::quarantine_client_only_mods(&root).await?;
    if !quarantined.is_empty() {
        emit_mod_progress(
            &app,
            "Filtering client-only mods",
            &format!("Quarantined {} client-only mod(s)", quarantined.len()),
            quarantined.len() as u32,
            quarantined.len() as u32,
        );
    }
    ensure_server_properties(&root, &cfg.server_name)?;

    // Auto-enable require-resource-pack if the modpack included resource packs
    let rp_dir = root.join("resourcepacks");
    if rp_dir.exists() {
        let has_packs = std::fs::read_dir(&rp_dir)
            .ok()
            .and_then(|mut d| {
                d.find_map(|e| {
                    e.ok()
                        .filter(|e| e.path().extension().is_some_and(|ext| ext == "zip"))
                })
            })
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
        props.push_str("online-mode=true\n");
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
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut packs = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        let path = entry.path();
        let is_zip = path.extension().is_some_and(|x| x == "zip");
        if !meta.is_dir() && !is_zip {
            continue;
        }
        packs.push(ResourcePackInfo {
            name: entry.file_name().to_string_lossy().to_string(),
            kind: if meta.is_dir() {
                "folder".to_string()
            } else {
                "zip".to_string()
            },
            bytes: meta.len(),
        });
    }
    packs.sort_by_key(|p| p.name.to_lowercase());
    Ok(packs)
}

pub async fn add_resource_pack(
    file_path: String,
    overwrite: bool,
) -> Result<Vec<ResourcePackInfo>, String> {
    let cfg = config::load_config();
    let src = PathBuf::from(&file_path);
    let meta = tokio::fs::metadata(&src).await.map_err(|e| e.to_string())?;
    let name = src
        .file_name()
        .ok_or("Invalid resource pack path")?
        .to_string_lossy()
        .to_string();
    let is_zip = src.extension().is_some_and(|x| x == "zip");
    if !meta.is_dir() && !is_zip {
        return Err("Resource packs must be .zip files or folders.".to_string());
    }
    let dir = server_dir(&cfg)?.join("resourcepacks");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let dest = safe_join(&dir, &name)?;
    let dest_exists = tokio::fs::try_exists(&dest).await.unwrap_or(false);
    if dest_exists && !overwrite {
        return Err(format!("A resource pack named {} already exists.", name));
    }
    if dest_exists {
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&dest)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            tokio::fs::remove_file(&dest)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if meta.is_dir() {
        copy_dir_recursive(&src, &dest)?;
    } else {
        tokio::fs::copy(&src, &dest)
            .await
            .map_err(|e| e.to_string())?;
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
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| e.to_string())?;
    } else {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| e.to_string())?;
    }
    // If no more resource packs, disable requirement
    let remaining = list_resource_packs_internal(&cfg)?;
    if remaining.is_empty() {
        update_resource_pack_requirement(&cfg, false)?;
    }
    list_resource_packs()
}

/// Update require-resource-pack setting in server.properties.
fn update_resource_pack_requirement(
    cfg: &crate::config::ServerConfig,
    require: bool,
) -> Result<(), String> {
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
fn list_resource_packs_internal(
    cfg: &crate::config::ServerConfig,
) -> Result<Vec<ResourcePackInfo>, String> {
    let dir = server_dir(cfg)?.join("resourcepacks");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut packs = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
    {
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
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(path);
        crate::helpers::hide_std_child_window(&mut cmd);
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn install_modpack_from_file(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    file_path: String,
) -> Result<ServerConfig, String> {
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".mrpack") {
        install_modrinth_modpack(app, file_path).await
    } else if lower.ends_with(".zip") {
        install_curseforge_modpack(app, file_path).await
    } else {
        Err("Choose a .mrpack or CurseForge .zip file.".to_string())
    }
}

// ── CurseForge URL / CDN resolver ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CfWidgetResponse {
    id: u64,
    name: Option<String>,
    files: Vec<CfWidgetFile>,
}

#[derive(Debug, Deserialize)]
struct CfWidgetFile {
    id: u64,
    name: String,
    #[serde(default)]
    version: Option<String>,
}

/// Parse a CurseForge URL into (slug, optional file_id).
/// Supported formats:
///   https://www.curseforge.com/minecraft/modpacks/{slug}
///   https://www.curseforge.com/minecraft/modpacks/{slug}/files/{fileId}
fn parse_curseforge_url(url: &str) -> Result<(String, Option<u64>), String> {
    let url = url.trim().trim_end_matches('/');
    let prefix = "/minecraft/modpacks/";
    let rest = url
        .split(prefix)
        .nth(1)
        .ok_or_else(|| format!("Not a valid CurseForge modpack URL: {}", url))?;
    let mut parts = rest.split('/');
    let slug = parts
        .next()
        .ok_or("Missing modpack slug in URL")?
        .to_string();
    let file_id = if parts.next() == Some("files") {
        parts.next().and_then(|s| s.parse::<u64>().ok())
    } else {
        None
    };
    Ok((slug, file_id))
}

/// Construct a CurseForge CDN download URL from a file ID and filename.
fn curseforge_cdn_url(file_id: u64, filename: &str) -> String {
    let prefix = file_id / 1000;
    let suffix = file_id % 1000;
    format!(
        "https://edge.forgecdn.net/files/{}/{}/{}",
        prefix, suffix, filename
    )
}

/// Fetch project info from CFWidget API (no API key needed).
async fn cfwidget_project(slug: &str) -> Result<CfWidgetResponse, String> {
    let url = format!("https://api.cfwidget.com/minecraft/modpacks/{}", slug);
    let resp = client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("CFWidget request failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("CFWidget response parse error: {}", e))?;
    Ok(resp)
}

/// Find a specific file in CFWidget response by file ID, or return the latest file.
fn cfwidget_find_file<'a>(
    project: &'a CfWidgetResponse,
    file_id: Option<u64>,
) -> Result<&'a CfWidgetFile, String> {
    if let Some(fid) = file_id {
        project
            .files
            .iter()
            .find(|f| f.id == fid)
            .ok_or_else(|| format!("File ID {} not found for this modpack", fid))
    } else {
        project
            .files
            .first()
            .ok_or_else(|| "No files found for this modpack".to_string())
    }
}

/// Install a CurseForge modpack from a URL
/// (e.g. https://www.curseforge.com/minecraft/modpacks/...).
/// Uses CFWidget API to resolve project info, then downloads from CDN.
pub async fn install_curseforge_modpack_link(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    url: String,
) -> Result<ServerConfig, String> {
    let (slug, file_id) = parse_curseforge_url(&url)?;
    emit_mod_progress(
        &app,
        "Resolving CurseForge pack",
        &format!("Looking up {}", slug),
        0,
        1,
    );

    let project = cfwidget_project(&slug).await?;
    let file = cfwidget_find_file(&project, file_id)?;
    let cdn_url = curseforge_cdn_url(file.id, &file.name);

    emit_mod_progress(
        &app,
        "Downloading CurseForge pack",
        &file.name,
        0,
        1,
    );
    let tmp = std::env::temp_dir().join(format!(
        "lbby-cf-{}.zip",
        uuid::Uuid::new_v4().simple()
    ));
    download_bytes_to_file(
        &app,
        &cdn_url,
        &tmp,
        "Downloading CurseForge pack",
        &file.name,
        1,
        1,
    )
    .await?;

    // Delegate to existing CurseForge installer
    let result = install_curseforge_modpack(app, tmp.to_string_lossy().to_string()).await;
    // Clean up temp file
    let _ = tokio::fs::remove_file(&tmp).await;
    result
}

// ── Modrinth modpack search ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModrinthModpackHit {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub icon_url: Option<String>,
    pub downloads: u64,
    pub versions: Vec<String>,
    pub server_side: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModrinthModpackSearchResponse {
    hits: Vec<ModrinthModpackHit>,
}

/// Search Modrinth for modpacks matching a query.
pub async fn search_modrinth_modpacks(
    query: String,
    mc_version: String,
    loader: String,
) -> Result<Vec<ModrinthModpackHit>, String> {
    let facets = format!(
        "[[\"project_type:modpack\"],[\"versions:{}\"],[\"categories:{}\"]]",
        mc_version, loader
    );
    let resp: ModrinthModpackSearchResponse = client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[
            ("query", query.as_str()),
            ("facets", facets.as_str()),
            ("limit", "20"),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(resp.hits)
}

pub async fn add_mod(file_path: String, overwrite: Option<bool>) -> Result<(), String> {
    let cfg = config::load_config();
    let src = PathBuf::from(&file_path);
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let allowed = match cfg.server_type {
        ServerType::Terraria | ServerType::TModLoader => ext == "tmod",
        _ => ext == "jar",
    };
    if !allowed {
        return Err(format!(
            "Only {} files can be imported as mods for this server type.",
            if cfg.is_terraria() { ".tmod" } else { ".jar" }
        ));
    }
    let name = src
        .file_name()
        .ok_or("Invalid file path")?
        .to_string_lossy()
        .to_string();
    let dest = mods_dir(&cfg)?.join(&name);
    tokio::fs::create_dir_all(dest.parent().unwrap())
        .await
        .map_err(|e| e.to_string())?;
    if dest.exists() && !overwrite.unwrap_or(false) {
        return Err(format!("A mod named {} already exists.", name));
    }
    tokio::fs::copy(&src, &dest)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn remove_mod(mod_name: String) -> Result<(), String> {
    let cfg = config::load_config();
    let path = mods_dir(&cfg)?.join(&mod_name);
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| e.to_string())?;
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
        let is_mod = if is_terraria {
            ext == "tmod"
        } else {
            ext == "jar"
        };
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

/// Information about a missing dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingDependency {
    pub mod_id: String,
    pub version_range: String,
    pub source_mod: String,
    pub installed_version: Option<String>,
    pub issue_type: String, // "missing" or "incompatible"
}

/// Scan installed mods for missing or incompatible dependencies.
pub fn scan_missing_dependencies() -> Vec<MissingDependency> {
    let cfg = config::load_config();
    let Ok(target_dir) = mods_dir(&cfg) else {
        return Vec::new();
    };
    let mut installed_mods: HashMap<String, String> = HashMap::new(); // mod_id -> version
    let mut installed_files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jar") {
                installed_files.push(path);
            }
        }
    }

    // Build map of installed mods: id -> version
    for path in &installed_files {
        let info = crate::helpers::read_mod_info(path);
        let file_stem = path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let version = crate::helpers::extract_mod_version(&file_stem).unwrap_or_default();

        // Try to get mod ID from filename (strip version)
        let mod_id = if let Some(pos) = file_stem.rfind('-') {
            file_stem[..pos].to_lowercase()
        } else {
            file_stem.to_lowercase()
        };

        if !mod_id.is_empty() {
            installed_mods.insert(mod_id, version.clone());
        }
        // Also index by display name
        if !info.display_name.is_empty() {
            installed_mods.insert(info.display_name.to_lowercase(), version);
        }
    }

    let mut issues = Vec::new();
    for path in &installed_files {
        let info = crate::helpers::read_mod_info(path);
        let source_mod = info.display_name.clone();

        // Check Forge dependencies
        let deps = crate::helpers::read_forge_dependencies(path);
        for (mod_id, version_range) in deps {
            if let Some(installed_ver) = installed_mods.get(&mod_id.to_lowercase()) {
                // Mod exists - check version compatibility
                if !version_range.is_empty() && !crate::helpers::version_matches_range(installed_ver, &version_range) {
                    issues.push(MissingDependency {
                        mod_id,
                        version_range,
                        source_mod: source_mod.clone(),
                        installed_version: Some(installed_ver.clone()),
                        issue_type: "incompatible".to_string(),
                    });
                }
            } else {
                // Mod missing
                issues.push(MissingDependency {
                    mod_id,
                    version_range,
                    source_mod: source_mod.clone(),
                    installed_version: None,
                    issue_type: "missing".to_string(),
                });
            }
        }

        // Check Fabric dependencies
        let deps = crate::helpers::read_fabric_dependencies(path);
        for (mod_id, version_range) in deps {
            if let Some(installed_ver) = installed_mods.get(&mod_id.to_lowercase()) {
                // Mod exists - check version compatibility
                if !version_range.is_empty() && !crate::helpers::version_matches_range(installed_ver, &version_range) {
                    issues.push(MissingDependency {
                        mod_id,
                        version_range,
                        source_mod: source_mod.clone(),
                        installed_version: Some(installed_ver.clone()),
                        issue_type: "incompatible".to_string(),
                    });
                }
            } else {
                // Mod missing
                issues.push(MissingDependency {
                    mod_id,
                    version_range,
                    source_mod: source_mod.clone(),
                    installed_version: None,
                    issue_type: "missing".to_string(),
                });
            }
        }
    }

    issues.sort_by(|a, b| a.mod_id.cmp(&b.mod_id));
    issues.dedup_by(|a, b| a.mod_id == b.mod_id);
    issues
}

/// Install specific missing dependencies by mod_id using Modrinth API.
pub async fn install_missing_dependencies(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    mod_ids: Vec<String>,
) -> Result<u32, String> {
    let cfg = config::load_config();
    let target_dir = mods_dir(&cfg)?;
    let mc_version = &cfg.minecraft_version;
    let loader = match cfg.server_type {
        crate::config::ServerType::NeoForge => "neoforge".to_string(),
        crate::config::ServerType::Forge => "forge".to_string(),
        crate::config::ServerType::Fabric => "fabric".to_string(),
        _ => format!("{:?}", cfg.server_type).to_lowercase(),
    };
    let client = client()?;
    let mut installed = 0u32;
    let total = mod_ids.len();

    for (i, mod_id) in mod_ids.iter().enumerate() {
        emit_mod_progress(&app, "Installing dependencies", &format!("{}/{}: {}", i + 1, total, mod_id), (i + 1) as u32, total as u32);

        let search_url = format!(
            "https://api.modrinth.com/v2/search?query={}",
            urlencoding::encode(mod_id)
        );
        eprintln!("[lbby] install_missing_deps: searching Modrinth for '{}' url='{}'", mod_id, search_url);
        let resp = client.get(&search_url).timeout(std::time::Duration::from_secs(15)).send().await;
        let Ok(resp) = resp else {
            eprintln!("[lbby] install_missing_deps: search request failed for {}", mod_id);
            continue;
        };
        let body = resp.text().await.unwrap_or_default();
        eprintln!("[lbby] install_missing_deps: search response for {}: {} chars | first 200: {}", mod_id, body.len(), &body[..body.len().min(200)]);
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) else {
            eprintln!("[lbby] install_missing_deps: JSON parse failed for {} (first 200: {})", mod_id, &body[..body.len().min(200)]);
            continue;
        };

        let Some(hits) = data["hits"].as_array() else {
            eprintln!("[lbby] install_missing_deps: no hits array for {}", mod_id);
            continue;
        };
        eprintln!("[lbby] install_missing_deps: found {} hits for {}", hits.len(), mod_id);
        // Flexible matching: remove all separators and compare
        let normalized_id = mod_id.replace('_', "-").to_lowercase();
        let strip_seps = |s: &str| s.replace(|c: char| c == '-' || c == '_' || c == ' ', "").to_lowercase();
        let mid_clean = strip_seps(mod_id);
        let matching = if hits.len() == 1 {
            // Only 1 result from search = likely the right mod
            eprintln!("[lbby]   auto-selecting only hit for {}", mod_id);
            hits.first()
        } else {
            hits.iter().find(|h| {
                let slug = h["slug"].as_str().unwrap_or("").to_lowercase();
                let title = h["title"].as_str().unwrap_or("").to_lowercase();
                let mid = mod_id.to_lowercase();
                let slug_clean = strip_seps(&slug);
                slug == mid
                    || slug == normalized_id
                    || slug.replace('-', "_") == mid
                    || slug_clean == mid_clean
                    || title.contains(&mid)
                    || mid.contains(&slug)
            })
        };
        let Some(hit) = matching else {
            eprintln!("[lbby] install_missing_deps: no match for {} (tried slug, normalized, title)", mod_id);
            continue;
        };
        let project_id = hit["project_id"].as_str().unwrap_or("");
        if project_id.is_empty() { continue; }

        let game_versions = format!("[\"{}\"]", mc_version);
        let loaders = format!("[\"{}\"]", loader);
        let versions_url = format!(
            "https://api.modrinth.com/v2/project/{}/version?game_versions={}&loaders={}",
            project_id, game_versions, loaders
        );
        let versions_resp = client.get(&versions_url).timeout(std::time::Duration::from_secs(15)).send().await;
        let Ok(versions_resp) = versions_resp else { continue; };
        let versions_body = versions_resp.text().await.unwrap_or_default();
        let Ok(versions) = serde_json::from_str::<serde_json::Value>(&versions_body) else { continue; };
        let Some(version_list) = versions.as_array() else { continue; };
        let Some(version) = version_list.first() else { continue; };

        let Some(files) = version["files"].as_array() else { continue; };
        let Some(file) = files.first() else { continue; };
        let download_url = file["url"].as_str().unwrap_or("");
        let default_name = format!("{}.jar", mod_id);
        let file_name = file["filename"].as_str().unwrap_or(&default_name);
        if download_url.is_empty() { continue; }

        let dest = target_dir.join(file_name);
        if download_bytes_to_file(&app, download_url, &dest, "Installing dependencies", file_name, (i + 1) as u32, total as u32).await.is_ok() {
            installed += 1;
        }
    }

    emit_mod_progress(&app, "Done", &format!("Installed {} dependencies", installed), total as u32, total as u32);
    Ok(installed)
}

/// Auto-fix dependency issues: install missing mods and fix incompatible versions.
pub async fn auto_fix_dependencies(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
) -> Result<u32, String> {
    let issues = scan_missing_dependencies();
    if issues.is_empty() {
        return Ok(0);
    }

    let cfg = config::load_config();
    let target_dir = mods_dir(&cfg)?;
    let mc_version = &cfg.minecraft_version;
    let loader = match cfg.server_type {
        crate::config::ServerType::NeoForge => "neoforge".to_string(),
        crate::config::ServerType::Forge => "forge".to_string(),
        crate::config::ServerType::Fabric => "fabric".to_string(),
        _ => format!("{:?}", cfg.server_type).to_lowercase(),
    };

    let client = client()?;
    let mut fixed = 0u32;
    let total = issues.len();

    for (i, issue) in issues.iter().enumerate() {
        let action = if issue.issue_type == "incompatible" {
            format!("Fixing {} (have {})", issue.mod_id, issue.installed_version.as_deref().unwrap_or("?"))
        } else {
            format!("Installing {}", issue.mod_id)
        };
        emit_mod_progress(&app, "Fixing dependencies", &format!("{}/{}: {}", i + 1, total, action), (i + 1) as u32, total as u32);

        // Search Modrinth for the mod
        let search_url = format!(
            "https://api.modrinth.com/v2/search?query={}",
            urlencoding::encode(&issue.mod_id)
        );

        let resp = client.get(&search_url).timeout(std::time::Duration::from_secs(15)).send().await;
        let Ok(resp) = resp else { continue; };
        let body = resp.text().await.unwrap_or_default();
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) else { continue; };
        let Some(hits) = data["hits"].as_array() else { continue; };

        // Find matching mod
        let normalized_id = issue.mod_id.replace('_', "-").to_lowercase();
        let matching = hits.iter().find(|h| {
            let slug = h["slug"].as_str().unwrap_or("").to_lowercase();
            let title = h["title"].as_str().unwrap_or("").to_lowercase();
            let mid = issue.mod_id.to_lowercase();
            slug == mid || slug == normalized_id || title.contains(&mid) || mid.contains(&slug)
        });

        let Some(hit) = matching else { continue; };
        let project_id = hit["project_id"].as_str().unwrap_or("");
        if project_id.is_empty() { continue; }

        // Get versions with compatible game version and loader
        let game_versions = format!("[\"{}\"]", mc_version);
        let loaders = format!("[\"{}\"]", loader);
        let versions_url = format!(
            "https://api.modrinth.com/v2/project/{}/version?game_versions={}&loaders={}",
            project_id, game_versions, loaders
        );
        let versions_resp = client.get(&versions_url).timeout(std::time::Duration::from_secs(15)).send().await;
        let Ok(versions_resp) = versions_resp else { continue; };
        let versions_body = versions_resp.text().await.unwrap_or_default();
        let Ok(versions) = serde_json::from_str::<serde_json::Value>(&versions_body) else { continue; };
        let Some(version_list) = versions.as_array() else { continue; };

        // For incompatible versions, try to find a version that matches the range
        let compatible_version = if issue.issue_type == "incompatible" && !issue.version_range.is_empty() {
            version_list.iter().find(|v| {
                let ver_num = v["version_number"].as_str().unwrap_or("");
                crate::helpers::version_matches_range(ver_num, &issue.version_range)
            })
        } else {
            version_list.first()
        };

        let Some(version) = compatible_version else {
            eprintln!("[lbby] auto_fix: no compatible version found for {} (need {})", issue.mod_id, issue.version_range);
            continue;
        };

        let Some(files) = version["files"].as_array() else { continue; };
        let Some(file) = files.first() else { continue; };
        let download_url = file["url"].as_str().unwrap_or("");
        let file_name = file["filename"].as_str().unwrap_or(&issue.mod_id);
        if download_url.is_empty() { continue; }

        let dest = target_dir.join(file_name);

        // For incompatible versions, remove the old mod first
        if issue.issue_type == "incompatible" {
            if let Some(old_version) = &issue.installed_version {
                let _ = remove_mod(format!("{}-{}", issue.mod_id, old_version));
            }
        }

        if download_bytes_to_file(&app, download_url, &dest, "Fixing dependencies", file_name, (i + 1) as u32, total as u32).await.is_ok() {
            fixed += 1;
            eprintln!("[lbby] auto_fix: installed {} v{}", issue.mod_id, version["version_number"].as_str().unwrap_or("?"));
        }
    }

    emit_mod_progress(&app, "Done", &format!("Fixed {} dependency issues", fixed), total as u32, total as u32);
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::apply_mrpack_overrides;
    use crate::config::{ServerConfig, ServerType};
    use std::io::{Cursor, Write};
    use super::{
        curseforge_cdn_parts, curseforge_fingerprint_reader, official_server_pack_id,
        parse_curseforge_source, validate_curseforge_file_for_profile, CurseFilesResponse,
    };
    use std::io::Cursor;

    #[test]
    fn parses_file_id_from_curseforge_url() {
        assert_eq!(
            parse_curseforge_source(
                "https://www.curseforge.com/minecraft/modpacks/example-pack/files/8448977"
            ),
            ("example-pack".to_string(), Some(8_448_977))
        );
    }

    #[test]
    fn reads_server_pack_file_id_from_api_response() {
        let response: CurseFilesResponse = serde_json::from_str(
            r#"{"data":[{"id":10,"fileName":"client.zip","fileLength":5,"isServerPack":false,"serverPackFileId":11}]}"#,
        )
        .unwrap();
        assert_eq!(response.data[0].server_pack_file_id, Some(11));
        assert_eq!(official_server_pack_id(&response.data[0]), Some(11));
    }

    #[test]
    fn does_not_follow_invalid_or_recursive_server_pack_ids() {
        let response: CurseFilesResponse = serde_json::from_str(
            r#"{"data":[{"id":10,"fileName":"server.zip","fileLength":5,"isServerPack":true,"serverPackFileId":11},{"id":12,"fileName":"client.zip","fileLength":5,"serverPackFileId":0}]}"#,
        )
        .unwrap();
        assert_eq!(official_server_pack_id(&response.data[0]), None);
        assert_eq!(official_server_pack_id(&response.data[1]), None);
    }

    #[test]
    fn validates_minecraft_version_and_loader_metadata() {
        let response: CurseFilesResponse = serde_json::from_str(
            r#"{"data":[{"id":10,"fileName":"pack.zip","fileLength":5,"gameVersions":["1.20.1","Forge"],"parentProjectFileId":9}]}"#,
        )
        .unwrap();
        let file = &response.data[0];
        assert_eq!(file.parent_project_file_id, Some(9));
        let mut config = ServerConfig {
            minecraft_version: "1.20.1".to_string(),
            server_type: ServerType::Forge,
            ..ServerConfig::default()
        };
        assert!(validate_curseforge_file_for_profile(file, &config).is_ok());

        config.minecraft_version = "1.21.1".to_string();
        assert!(validate_curseforge_file_for_profile(file, &config)
            .unwrap_err()
            .contains("not for Minecraft 1.21.1"));
        config.minecraft_version = "1.20.1".to_string();
        config.server_type = ServerType::NeoForge;
        assert!(validate_curseforge_file_for_profile(file, &config)
            .unwrap_err()
            .contains("does not match the profile loader neoforge"));
    }

    #[test]
    fn fingerprint_ignores_curseforge_whitespace() {
        let with_whitespace = b"Curse Forge\n test\tdata";
        let compact = b"CurseForgetestdata";
        let first = curseforge_fingerprint_reader(Cursor::new(with_whitespace), compact.len())
            .unwrap();
        let second = curseforge_fingerprint_reader(Cursor::new(compact), compact.len()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, 2_704_042_519);
    }

    #[test]
    fn builds_cdn_parts_without_truncating_long_file_ids() {
        assert_eq!(
            curseforge_cdn_parts(8_448_977).unwrap(),
            ("8448".to_string(), "977".to_string())
        );
        assert_eq!(
            curseforge_cdn_parts(12_345_678).unwrap(),
            ("1234".to_string(), "5678".to_string())
        );
        assert_eq!(
            curseforge_cdn_parts(12_340_078).unwrap(),
            ("1234".to_string(), "78".to_string())
        );
    }


    #[test]
    fn mrpack_applies_server_overrides_and_ignores_client_overrides() {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("overrides/config/common.txt", options).unwrap();
            writer.write_all(b"common").unwrap();
            writer
                .start_file("server-overrides/config/side.txt", options)
                .unwrap();
            writer.write_all(b"server").unwrap();
            writer
                .start_file("client-overrides/config/client.txt", options)
                .unwrap();
            writer.write_all(b"client").unwrap();
            writer.finish().unwrap();
        }
        bytes.set_position(0);
        let mut archive = zip::ZipArchive::new(bytes).unwrap();
        let dest = std::env::temp_dir().join(format!(
            "lbby-mrpack-test-{}",
            uuid::Uuid::new_v4().simple()
        ));

        apply_mrpack_overrides(&mut archive, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("config/common.txt")).unwrap(), b"common");
        assert_eq!(std::fs::read(dest.join("config/side.txt")).unwrap(), b"server");
        assert!(!dest.join("config/client.txt").exists());
        std::fs::remove_dir_all(dest).unwrap();
    }
}
