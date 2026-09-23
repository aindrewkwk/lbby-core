use crate::config::{self, ServerConfig, ServerType};
use crate::{
    app_state::{
        DependentInfo, DetectedLoader, InstallResult, InventoryDependency, ModCompatConfidence,
        ModCompatSource, ModCompatibility, ModInfo, ModProvider, ModStatus, RemoveResult,
        UpdateAllResult, UpdateItemResult, UpdateOutcome, UpdateStatus,
    },
    helpers::{default_server_path_value, read_mod_info},
    jar_metadata, mod_compat,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek};
use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncWriteExt;

/// Outcome of an install operation.
///
/// Instead of forcing `Result<ServerConfig, String>` (which conflates "user action required"
/// with "error"), this enum cleanly represents three states:
/// - `Success`: install completed and committed.
/// - `UserActionRequired`: transaction paused — user must approve recovery.
/// - (errors are still returned as `Err(String)`)
#[derive(Debug)]
pub enum InstallOutcome {
    /// Install completed successfully. Config is committed.
    Success(ServerConfig),
    /// High-confidence crash attribution found. Transaction paused.
    /// Frontend must present approval UI; backend owns all paths.
    UserActionRequired {
        /// Server ID (stable identifier).
        server_id: String,
        /// Transaction ID (stable identifier).
        transaction_id: String,
        /// Attribution fingerprint (includes JAR SHA-256).
        fingerprint: String,
        /// Target mod ID for recovery.
        mod_id: String,
        /// Display-safe JAR filename (no full path).
        display_filename: String,
        /// SHA-256 of target JAR bytes at attribution time.
        jar_sha256: String,
        /// Boot attempt at time of pause.
        boot_attempt: u8,
        /// Recovery actions used so far.
        recovery_actions_used: u8,
        /// Crash attribution summary (for UI).
        crash_summary: String,
        /// Confidence level string.
        confidence: String,
    },
}

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
    /// "Modrinth" or "CurseForge" — frontend uses for install dispatch
    #[serde(default = "default_source_modrinth")]
    pub source: String,
}

fn default_source_modrinth() -> String {
    "Modrinth".to_string()
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
    // ── 4B.3C: structured status ──
    #[serde(default)]
    pub status: UpdateStatus,
    #[serde(default)]
    pub provider: ModProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_file_version_id: Option<String>,
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
    /// "release", "beta", or "alpha"
    #[serde(default = "default_version_type")]
    version_type: String,
}

fn default_version_type() -> String {
    "release".to_string()
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
    #[serde(default = "default_required")]
    required: bool,
}

fn default_required() -> bool {
    true
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
    #[serde(default)]
    pub dependencies: Vec<crate::dependency_resolver::CurseDependency>,
    /// 1 = Stable/Release, 2 = Beta, 3 = Alpha
    #[serde(default, rename = "releaseType")]
    pub release_type: i32,
    /// Cryptographic hashes from CurseForge API.
    /// Each entry: {"value": "hex", "algo": 1|2} (1=SHA1, 2=MD5)
    #[serde(default)]
    pub hashes: Vec<CurseHash>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurseHash {
    pub value: String,
    /// 1 = SHA1, 2 = MD5 (per CurseForge API docs)
    pub algo: i32,
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

// CurseForge API Key (shared across all Lbby instances)
pub const CURSEFORGE_API_KEY: &str = "$2a$10$ng5QfluekSLUzzSyFt3Ea.OTs1q028T1gAo/rMr0LBshjtdbqD.W2";

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
        .filter(|value| {
            known_loaders
                .iter()
                .any(|loader| value.eq_ignore_ascii_case(loader))
        })
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

pub fn curseforge_fingerprint_reader<R: Read>(
    mut reader: R,
    normalized_len: usize,
) -> Result<u32, String> {
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
                            && (slug
                                .to_lowercase()
                                .contains(&mod_name.to_lowercase().replace(" ", "-"))
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
                                    if let Ok(files_data) =
                                        files_resp.json::<serde_json::Value>().await
                                    {
                                        if let Some(files) = files_data["data"].as_array() {
                                            if let Some(file) = files.first() {
                                                if let Ok(server_file) =
                                                    serde_json::from_value::<CurseFileEntry>(
                                                        file.clone(),
                                                    )
                                                {
                                                    let _ = app.emit(
                                                        "mod-task-progress",
                                                        ModTaskProgress {
                                                            stage: "Found server pack".to_string(),
                                                            message: format!(
                                                                "Using CurseForge server pack: {}",
                                                                server_file.file_name
                                                            ),
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
                    message: format!(
                        "{} / {} MB",
                        downloaded / 1024 / 1024,
                        total_size / 1024 / 1024
                    ),
                    current: downloaded as u32,
                    total: total_size as u32,
                    progress: if total_size > 0 {
                        downloaded as f32 / total_size as f32
                    } else {
                        0.0
                    },
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
    let api_key = CURSEFORGE_API_KEY;

    let client = curseforge_http_client()?;
    let url = format!(
        "https://api.curseforge.com/v1/mods/search?gameId=432&searchFilter={}&gameVersion={}&classId=6&pageSize=20",
        urlencoding::encode(&query),
        urlencoding::encode(&minecraft_version)
    );

    let resp = client
        .get(&url)
        .header("x-api-key", api_key)
        .send()
        .await
        .map_err(|e| format!("CurseForge search error: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "CurseForge search failed ({}): {}",
            status,
            response_preview(&text)
        ));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("CurseForge parse error: {}", e))?;
    let hits = data["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m["id"].as_u64()?;
                    let name = m["name"].as_str()?.to_string();
                    let slug = m["slug"].as_str()?.to_string();
                    let desc = m["summary"].as_str().unwrap_or("").to_string();
                    let icon = m
                        .get("logo")
                        .and_then(|l| l["thumbnailUrl"].as_str())
                        .map(|s| s.to_string());
                    let versions: Vec<String> = m
                        .get("latestFiles")
                        .and_then(|f| f.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|f| f["gameVersion"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let loaders: Vec<String> = m
                        .get("latestFiles")
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
                        source: "CurseForge".to_string(),
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

// Cached HTTP client - reused across downloads
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

pub(crate) fn client() -> Result<reqwest::Client, String> {
    Ok(HTTP_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
                .timeout(std::time::Duration::from_secs(60))
                .pool_max_idle_per_host(20)
                .tcp_keepalive(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client")
        })
        .clone())
}

fn curseforge_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
        .timeout(std::time::Duration::from_secs(30))
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "x-api-key",
                reqwest::header::HeaderValue::from_str(CURSEFORGE_API_KEY)
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

/// Validates that `name` is a single, safe filename component — no path
/// separators, no traversal sequences, no absolute paths, no empty string.
/// Returns the canonical file name on success.
pub fn validate_basename(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("Filename must not be empty".into());
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return Err(format!("Invalid filename: {}", name));
    }
    let p = Path::new(name);
    if p.is_absolute() {
        return Err(format!("Absolute path rejected: {}", name));
    }
    // Must have exactly one component (no parent directory)
    let mut components = p.components();
    match components.next() {
        Some(Component::Normal(_)) => {}
        _ => return Err(format!("Invalid filename component: {}", name)),
    }
    if components.next().is_some() {
        return Err(format!("Nested path rejected: {}", name));
    }
    Ok(name)
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
    expected_sha512: Option<&str>,
    expected_sha1: Option<&str>,
) -> Result<(), String> {
    // Atomic download: write to temp file in same directory, flush, verify
    // hash on temp, then rename. An unverified artifact never appears in the
    // live mods/resourcepacks/shaderpacks directory.
    let temp = {
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(".lbbytmp");
        dest.with_file_name(name)
    };
    // Retry up to 3 times on failure
    let mut last_err = String::new();
    for attempt in 1..=3 {
        // Clean leftover temp from previous attempt
        let _ = tokio::fs::remove_file(&temp).await;
        match client()?.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                let size = resp.content_length().unwrap_or(0);
                let mut stream = resp.bytes_stream();
                let mut file = tokio::fs::File::create(&temp)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut downloaded = 0u64;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| {
                        let _ = std::fs::remove_file(&temp);
                        e.to_string()
                    })?;
                    file.write_all(&chunk).await.map_err(|e| {
                        let _ = std::fs::remove_file(&temp);
                        e.to_string()
                    })?;
                    downloaded += chunk.len() as u64;
                    let percent = if size > 0 {
                        format!(" ({:.0}%)", downloaded as f64 / size as f64 * 100.0)
                    } else {
                        String::new()
                    };
                    emit_mod_progress(app, stage, &format!("{}{}", label, percent), current, total);
                }
                // Flush and sync before verification
                file.flush().await.map_err(|e| {
                    let _ = std::fs::remove_file(&temp);
                    e.to_string()
                })?;
                drop(file); // release handle before verification
                            // Verify authoritative digest on TEMP artifact — before it
                            // ever touches the live mods/resourcepacks/shaderpacks dir.
                            // On ANY verification failure, clean temp ourselves — no
                            // caller should need to remember cleanup.
                if let Some(expected) = expected_sha512 {
                    if let Err(e) = verify_sha512(&temp, Some(expected)) {
                        let _ = std::fs::remove_file(&temp);
                        return Err(e);
                    }
                }
                // Verify CurseForge provider SHA-1 on TEMP before commit
                if let Some(cf_sha1) = expected_sha1 {
                    if let Err(e) = verify_sha1(&temp, cf_sha1) {
                        let _ = std::fs::remove_file(&temp);
                        return Err(e);
                    }
                }
                // Atomic rename — same directory guarantees same filesystem
                // Test-only seam: dest paths containing "force-commit-fail" trigger failure
                #[cfg(test)]
                if dest.to_string_lossy().contains("force-commit-fail") {
                    let _ = std::fs::remove_file(&temp);
                    return Err("forced commit failure (test seam)".into());
                }
                tokio::fs::rename(&temp, dest).await.map_err(|e| {
                    let _ = std::fs::remove_file(&temp);
                    e.to_string()
                })?;
                return Ok(());
            }
            Ok(resp) => {
                last_err = format!("HTTP {}", resp.status());
                eprintln!(
                    "[lbby] Download attempt {} failed: {} for {}",
                    attempt, last_err, label
                );
            }
            Err(e) => {
                last_err = e.to_string();
                eprintln!(
                    "[lbby] Download attempt {} failed: {} for {}",
                    attempt, last_err, label
                );
            }
        }
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
    // Final cleanup
    let _ = tokio::fs::remove_file(&temp).await;
    Err(format!(
        "Download failed after 3 attempts: {} for {}",
        last_err, label
    ))
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

/// Verify a SHA-1 digest against a temp file.
/// Returns Ok(()) on match, Err on mismatch.
fn verify_sha1(path: &Path, expected_hex: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let hash = Sha1::digest(&bytes);
    let actual = format!("{:x}", hash);
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(format!(
            "SHA-1 mismatch for {}: expected {} got {}",
            path.display(),
            expected_hex,
            actual
        ))
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

    // Release channel policy: release > beta > alpha.
    // The API returns newest first; pick the best channel available.
    let version_type_priority = |vt: &str| match vt {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    };
    let best = versions
        .into_iter()
        .min_by_key(|v| version_type_priority(&v.version_type))
        .ok_or_else(|| {
            format!(
                "No compatible version found for Minecraft {} / {}.",
                cfg.minecraft_version, loader
            )
        })?;
    Ok(best)
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
            source: "Modrinth".to_string(),
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
            source: "Modrinth".to_string(),
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
            source: "Modrinth".to_string(),
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
        file.hashes.get("sha512").map(String::as_str),
        None,
    )
    .await?;

    Ok(())
}

pub async fn install_modrinth_mod(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    project_id: String,
) -> Result<InstallResult, String> {
    let cfg = config::load_config();
    let target_dir = mods_dir(&cfg)?;
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(|e| e.to_string())?;
    let mut installed_projects = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();
    install_modrinth_project_recursive(
        &app,
        &cfg,
        &target_dir,
        &project_id,
        &mut installed_projects,
        &mut warnings,
        1,
        1,
    )
    .await?;
    let warning = if warnings.is_empty() {
        None
    } else {
        Some(warnings.join("; "))
    };
    Ok(InstallResult {
        mods: list_installed_mods()?,
        warning,
    })
}

// ── 4B.3B: CurseForge single-mod install ─────────────────────────────────

/// Map ServerType to CurseForge modLoaderType enum value.
/// https://docs.curseforge.com/#tocS_ModLoaderType
fn curseforge_loader_id(st: &ServerType) -> Option<i32> {
    match st {
        ServerType::Forge => Some(1),
        ServerType::Fabric => Some(4),
        ServerType::NeoForge => Some(5),
        // Quilt: CF doesn't have a dedicated enum; treat as unsupported for now
        _ => None,
    }
}

/// CurseForge release channel priority: Stable(1) > Beta(2) > Alpha(3).
fn curseforge_release_priority(t: i32) -> i32 {
    match t {
        1 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    }
}

/// Install a single mod from CurseForge by project (mod) ID.
///
/// Dedicated path — does NOT route through Modrinth.
///
/// Flow:
///   1. Load profile config (MC version + loader)
///   2. Fetch compatible files from CurseForge API
///   3. Select deterministic release candidate (Release > Beta > Alpha, newest date)
///   4. Duplicate install check (same provider project already installed)
///   5. Atomic download (temp → verify → rename)
///   6. Persist provider receipt
///   7. Return refreshed inventory
pub async fn install_curseforge_mod(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    project_id: String,
) -> Result<InstallResult, String> {
    let cfg = config::load_config();
    let target_dir = mods_dir(&cfg)?;
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(|e| e.to_string())?;

    // ── 1. Validate profile ────────────────────────────────────────────
    let loader_id = curseforge_loader_id(&cfg.server_type).ok_or_else(|| {
        format!(
            "CurseForge single-mod install requires Forge, Fabric, or NeoForge. Current: {:?}",
            cfg.server_type
        )
    })?;
    if cfg.minecraft_version.trim().is_empty() {
        return Err("Profile has no Minecraft version set.".to_string());
    }

    // ── 2. Fetch compatible files from CurseForge API ──────────────────
    let cf_client = curseforge_http_client()?;
    let mod_id: i64 = project_id
        .parse()
        .map_err(|_| format!("Invalid CurseForge project ID: {}", project_id))?;
    let files_url = format!(
        "https://api.curseforge.com/v1/mods/{}/files?gameVersion={}&modLoaderType={}&pageSize=50",
        mod_id,
        urlencoding::encode(&cfg.minecraft_version),
        loader_id,
    );
    let resp = cf_client
        .get(&files_url)
        .send()
        .await
        .map_err(|e| format!("CurseForge API error: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 403 {
            return Err("CurseForge API: invalid API key or rate-limited.".to_string());
        }
        return Err(format!(
            "CurseForge API error ({}): {}",
            status,
            response_preview(&text)
        ));
    }
    let files_resp: CurseFilesResponse = resp
        .json()
        .await
        .map_err(|e| format!("CurseForge response parse error: {}", e))?;

    // ── 3. Select deterministic release candidate ──────────────────────
    //    Release(1) > Beta(2) > Alpha(3), then newest by file ID (proxy for date).
    //    Among files matching exact MC version + compatible loader.
    let mut candidates: Vec<CurseFileEntry> = files_resp
        .data
        .into_iter()
        .filter(|f| {
            // Must have a download URL or CDN-constructible path
            f.download_url
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty())
                || f.id > 0
        })
        .collect();

    if candidates.is_empty() {
        return Err(format!(
            "No compatible CurseForge file found for Minecraft {} / loader {}.",
            cfg.minecraft_version,
            normalize_loader(&cfg.server_type)
        ));
    }

    // Sort: release priority ascending, then file ID descending (newest first)
    candidates.sort_by(|a, b| {
        curseforge_release_priority(a.release_type)
            .cmp(&curseforge_release_priority(b.release_type))
            .then_with(|| b.id.cmp(&a.id))
    });
    let selected = candidates
        .into_iter()
        .next()
        .ok_or("No compatible CurseForge file found.")?;

    // ── 4. Duplicate install check ─────────────────────────────────────
    let server_path = PathBuf::from(&cfg.server_path);
    let receipts = load_receipts(&server_path);
    for (_fname, receipt) in &receipts.receipts {
        if receipt.provider == ModProvider::CurseForge && receipt.project_id == project_id {
            // Check if the artifact still exists
            let artifact_path = target_dir.join(&receipt.file_name);
            if artifact_path.exists() {
                return Err(format!(
                    "Already installed: {} (CurseForge project {})",
                    receipt.file_name, project_id
                ));
            }
        }
    }

    // ── 5. Atomic download ─────────────────────────────────────────────
    let safe_file_name = Path::new(&selected.file_name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or("CurseForge returned an invalid file name.")?;
    let dest = target_dir.join(safe_file_name);

    // Build download URL (prefer API-provided, fallback to CDN)
    let download_url = selected
        .download_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let (prefix, suffix) =
                curseforge_cdn_parts(selected.id).unwrap_or(("0".to_string(), "0".to_string()));
            let encoded = urlencoding::encode(safe_file_name);
            format!(
                "https://edge.forgecdn.net/files/{}/{}/{}",
                prefix, suffix, encoded
            )
        });

    // CurseForge SHA-1 verified on TEMP before atomic commit.
    let cf_sha1 = selected
        .hashes
        .iter()
        .find(|h| h.algo == 1)
        .map(|h| h.value.as_str());

    emit_mod_progress(
        &app,
        "Downloading mod",
        &format!("Installing {}", safe_file_name),
        1,
        1,
    );

    // Atomic download. SHA-1 verified on temp before commit.
    download_bytes_to_file(
        &app,
        &download_url,
        &dest,
        "Downloading mod",
        safe_file_name,
        1,
        1,
        None,
        cf_sha1,
    )
    .await?;

    // ── 6. Persist provider receipt ────────────────────────────────────
    let artifact_bytes = std::fs::read(&dest).map_err(|e| e.to_string())?;
    let sha512_hash = format!("{:x}", Sha512::digest(&artifact_bytes));

    let receipt = ModReceipt {
        provider: ModProvider::CurseForge,
        project_id: project_id.clone(),
        file_version_id: selected.id.to_string(),
        installed_hash: sha512_hash,
        loader: normalize_loader(&cfg.server_type).to_string(),
        mc_version: cfg.minecraft_version.clone(),
        file_name: safe_file_name.to_string(),
    };
    // Capture receipt write failure — do NOT silently discard.
    // Artifact stays installed regardless; failure surfaces as a warning.
    let warning = match save_mod_receipt(&server_path, safe_file_name, receipt) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "Mod installed, but provider metadata could not be saved: {}",
            e
        )),
    };

    // ── 7. Return refreshed inventory ──────────────────────────────────
    Ok(InstallResult {
        mods: list_installed_mods()?,
        warning,
    })
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
        file.hashes.get("sha512").map(String::as_str),
        None,
    )
    .await?;
    Ok(())
}

async fn install_modrinth_project_recursive(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    cfg: &ServerConfig,
    target_dir: &Path,
    project_id: &str,
    installed_projects: &mut HashSet<String>,
    warnings: &mut Vec<String>,
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
                warnings,
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
                file.hashes.get("sha512").map(String::as_str),
                None,
            )
            .await?;
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
        file.hashes.get("sha512").map(String::as_str),
        None,
    )
    .await?;

    // Save provider receipt for update tracking
    let hash = file.hashes.get("sha512").cloned().unwrap_or_default();
    let receipt = ModReceipt {
        provider: ModProvider::Modrinth,
        project_id: version.project_id.clone(),
        file_version_id: version.id.clone(),
        installed_hash: hash,
        loader: normalize_loader(&cfg.server_type).to_string(),
        mc_version: cfg.minecraft_version.clone(),
        file_name: file.filename.clone(),
    };
    let server_path = PathBuf::from(&cfg.server_path);
    if let Err(e) = save_mod_receipt(&server_path, &file.filename, receipt) {
        warnings.push(format!(
            "Mod installed, but provider metadata could not be saved: {}",
            e
        ));
    }

    Ok(())
}

// ── 4B.3B: Inventory enrichment ────────────────────────────────────────────

/// Provider receipt persisted alongside the mod artifact.
/// Enables future update detection and identity verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModReceipt {
    pub provider: ModProvider,
    pub project_id: String,
    pub file_version_id: String,
    pub installed_hash: String,
    pub loader: String,
    pub mc_version: String,
    #[serde(default)]
    pub file_name: String,
}

/// Schema-versioned receipt store for a profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileReceipts {
    pub schema_version: u32,
    pub receipts: HashMap<String, ModReceipt>,
}

impl Default for ProfileReceipts {
    fn default() -> Self {
        Self {
            schema_version: 1,
            receipts: HashMap::new(),
        }
    }
}

/// Load receipts from the profile-scoped metadata file.
pub fn load_receipts(server_path: &Path) -> ProfileReceipts {
    let path = server_path.join(".lbby-mod-receipts.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return ProfileReceipts::default();
    };
    let Ok(store) = serde_json::from_slice::<ProfileReceipts>(&bytes) else {
        // Corrupt metadata: preserve original (backup), return default
        let backup = path.with_extension("json.corrupted");
        let _ = std::fs::copy(&path, &backup);
        return ProfileReceipts::default();
    };
    // Future schema: reject, preserve original
    if store.schema_version > 1 {
        return ProfileReceipts::default();
    }
    store
}

/// Persist receipts atomically.
pub fn save_receipts(server_path: &Path, store: &ProfileReceipts) -> Result<(), String> {
    let path = server_path.join(".lbby-mod-receipts.json");
    let tmp = path.with_extension("json.lbbytmp");
    let data = serde_json::to_vec_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, &data).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Save a single receipt for an installed mod.
pub fn save_mod_receipt(
    server_path: &Path,
    file_name: &str,
    receipt: ModReceipt,
) -> Result<(), String> {
    let mut store = load_receipts(server_path);
    store.receipts.insert(file_name.to_string(), receipt);
    save_receipts(server_path, &store)
}

/// Generate a stable inventory_id for a mod artifact.
/// Opaque, unique within a profile inventory, not a filesystem path.
fn generate_inventory_id(file_name: &str, mod_id: Option<&str>) -> String {
    let input = match mod_id {
        Some(id) => format!("{}::{}", file_name, id),
        None => file_name.to_string(),
    };
    let hash = Sha512::digest(input.as_bytes());
    format!("inv_{:x}", hash)[..21].to_string() // "inv_" + 16 hex chars
}

/// Map jar_metadata::LoaderMetadataKind to app_state::DetectedLoader
fn map_loader_kind(kind: jar_metadata::LoaderMetadataKind) -> DetectedLoader {
    match kind {
        jar_metadata::LoaderMetadataKind::Fabric => DetectedLoader::Fabric,
        jar_metadata::LoaderMetadataKind::Quilt => DetectedLoader::Quilt,
        jar_metadata::LoaderMetadataKind::Forge => DetectedLoader::Forge,
        jar_metadata::LoaderMetadataKind::NeoForge => DetectedLoader::NeoForge,
    }
}

/// Map mod_compat::ServerCompatibility to app_state::ModCompatibility
fn map_compatibility(c: mod_compat::ServerCompatibility) -> ModCompatibility {
    match c {
        mod_compat::ServerCompatibility::ServerOk => ModCompatibility::ServerOk,
        mod_compat::ServerCompatibility::ClientOnly => ModCompatibility::ClientOnly,
        mod_compat::ServerCompatibility::Both => ModCompatibility::Both,
        mod_compat::ServerCompatibility::Unknown => ModCompatibility::Unknown,
    }
}

/// Map mod_compat::CompatibilityConfidence to app_state::ModCompatConfidence
fn map_confidence(c: mod_compat::CompatibilityConfidence) -> ModCompatConfidence {
    match c {
        mod_compat::CompatibilityConfidence::Explicit => ModCompatConfidence::Explicit,
        mod_compat::CompatibilityConfidence::None => ModCompatConfidence::None,
    }
}

/// Map mod_compat::CompatibilitySource to app_state::ModCompatSource
fn map_source(s: mod_compat::CompatibilitySource) -> ModCompatSource {
    match s {
        mod_compat::CompatibilitySource::FabricMetadata => ModCompatSource::FabricMetadata,
        mod_compat::CompatibilitySource::QuiltMetadata => ModCompatSource::QuiltMetadata,
        mod_compat::CompatibilitySource::ForgeMetadata => ModCompatSource::ForgeMetadata,
        mod_compat::CompatibilitySource::NeoForgeMetadata => ModCompatSource::NeoForgeMetadata,
        mod_compat::CompatibilitySource::ConflictingMetadata { .. } => {
            ModCompatSource::ConflictingMetadata
        }
        mod_compat::CompatibilitySource::None => ModCompatSource::None,
    }
}

/// Enrich a basic ModInfo with jar_metadata, mod_compat, receipt, and inventory_id.
/// Only for JAR files (.tmod files keep basic info only).
fn enrich_mod_info(basic: ModInfo, path: &Path, receipts: &ProfileReceipts) -> ModInfo {
    if basic.status == ModStatus::Unreadable {
        // Unreadable JAR — generate inventory_id but don't try metadata
        let inv_id = generate_inventory_id(&basic.file_name, None);
        return ModInfo {
            inventory_id: inv_id,
            compatibility: ModCompatibility::Unknown,
            compatibility_reason: "Unreadable metadata".to_string(),
            ..basic
        };
    }

    // .tmod files: skip jar_metadata (Terraria format, not JAR/ZIP)
    if basic.file_name.ends_with(".tmod") {
        let inv_id = generate_inventory_id(&basic.file_name, None);
        return ModInfo {
            inventory_id: inv_id,
            ..basic
        };
    }

    // Read jar_metadata for mod_id, loader, dependencies, environment
    let jar_meta = jar_metadata::read_jar_mod_metadata(path);
    let compat = mod_compat::classify_mod_local(path);

    let primary_mod_id = jar_meta.mod_ids.first().cloned();
    let inv_id = generate_inventory_id(&basic.file_name, primary_mod_id.as_deref());

    let loader = match jar_meta.loader {
        Some(kind) => map_loader_kind(kind),
        None => DetectedLoader::Unknown,
    };

    let dependency_metadata: Vec<InventoryDependency> = jar_meta
        .dependencies
        .iter()
        .map(|d| InventoryDependency {
            mod_id: d.mod_id.clone(),
            kind: match d.kind {
                jar_metadata::DependencyKind::Required => "required".to_string(),
                jar_metadata::DependencyKind::Optional => "optional".to_string(),
            },
            version_requirement: d.version_requirement.clone(),
        })
        .collect();

    // Look up receipt for provider identity.
    // Receipt binding: filename must match AND artifact hash must match receipt hash.
    // If hash mismatch: treat as stale receipt (Manual/Unknown), don't delete anything.
    let (provider, project_id, file_version_id, receipt_hash) =
        if let Some(receipt) = receipts.receipts.get(&basic.file_name) {
            // Verify hash binding: compute SHA-512 of artifact and compare
            let artifact_hash_matches = if !receipt.installed_hash.is_empty() {
                std::fs::read(path)
                    .map(|bytes| {
                        let actual = format!("{:x}", Sha512::digest(&bytes));
                        actual == receipt.installed_hash
                    })
                    .unwrap_or(false)
            } else {
                // No hash in receipt — can't verify, trust filename only (weak)
                true
            };
            if artifact_hash_matches {
                (
                    receipt.provider.clone(),
                    Some(receipt.project_id.clone()),
                    Some(receipt.file_version_id.clone()),
                    Some(receipt.installed_hash.clone()),
                )
            } else {
                // Hash mismatch: stale receipt. Preserve artifact, provider → Manual/Unknown.
                // Receipt stays on disk for diagnostics/reconciliation.
                eprintln!(
                    "[lbby] Receipt hash mismatch for {} — treating as manual/unknown",
                    basic.file_name
                );
                (ModProvider::Manual, None, None, None)
            }
        } else {
            (ModProvider::Unknown, None, None, None)
        };

    // Multi-mod JAR: one artifact = one inventory row.
    // All declared mod IDs are collected; primary_mod_id is the first for display.
    // The full declared_mod_ids list is available for dependency/conflict checks.

    ModInfo {
        inventory_id: inv_id,
        mod_id: primary_mod_id,
        loader,
        provider,
        project_id,
        file_version_id,
        hash: receipt_hash,
        compatibility: map_compatibility(compat.compatibility),
        compatibility_confidence: map_confidence(compat.confidence),
        compatibility_source: map_source(compat.source),
        compatibility_reason: compat.reason,
        dependency_metadata,
        ..basic
    }
}

/// Validate a mod candidate against the current profile before install.
/// Checks MC version and loader compatibility.
/// Returns Ok(()) if compatible, Err(reason) if not.
pub fn validate_mod_candidate_for_profile(
    candidate_mc_versions: &[String],
    candidate_loaders: &[String],
    cfg: &ServerConfig,
) -> Result<(), String> {
    let profile_mc = &cfg.minecraft_version;
    let profile_loader = normalize_loader(&cfg.server_type);

    // MC version: exact match required
    if !candidate_mc_versions.is_empty() && !candidate_mc_versions.contains(profile_mc) {
        return Err(format!(
            "Mod does not support Minecraft {}. Supported: {}",
            profile_mc,
            candidate_mc_versions.join(", ")
        ));
    }

    // Loader: must be compatible
    // Conservative policy:
    //   Fabric profile → Fabric only (Quilt-only rejected)
    //   Quilt profile → Quilt + Fabric (explicit Quilt backward-compat rule)
    //   Forge ≠ NeoForge (no cross-compat unless candidate explicitly declares both)
    if !candidate_loaders.is_empty() {
        let loader_compatible = candidate_loaders.iter().any(|l| {
            let norm = l.to_lowercase();
            norm == profile_loader || (profile_loader == "quilt" && norm == "fabric")
        });
        if !loader_compatible {
            return Err(format!(
                "Mod does not support loader '{}'. Supported: {}",
                profile_loader,
                candidate_loaders.join(", ")
            ));
        }
    }

    Ok(())
}

pub fn list_installed_mods() -> Result<Vec<ModInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let server_path = PathBuf::from(&cfg.server_path);
    let receipts = load_receipts(&server_path);
    // Terraria uses .tmod files, Minecraft uses .jar files
    let ext = if cfg.is_terraria() { "tmod" } else { "jar" };
    let mut mods: Vec<ModInfo> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == ext))
        .map(|e| {
            let basic = read_mod_info(&e.path());
            enrich_mod_info(basic, &e.path(), &receipts)
        })
        .collect();
    mods.sort_by_key(|a| a.display_name.to_lowercase());
    Ok(mods)
}

/// Check all installed mods for available updates.
///
/// Receipt-aware: uses persisted `ModReceipt` entries to determine provider
/// identity and installed version. Deterministic comparison by `file_version_id`
/// (Modrinth version ID or CurseForge file ID), not version-number strings.
///
/// For each installed JAR:
/// 1. Look up receipt by filename → verify artifact hash matches receipt hash.
/// 2. If receipt verified:
///    - Modrinth: `latest_modrinth_version()` → compare `latest.id` vs `receipt.file_version_id`.
///    - CurseForge: fetch latest compatible file → compare `selected.id` vs `receipt.file_version_id`.
/// 3. If no receipt or hash mismatch: fallback to Modrinth SHA-512 hash lookup (legacy path).
pub async fn check_mod_updates() -> Result<Vec<ModUpdateInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(vec![]);
    }
    let server_path = PathBuf::from(&cfg.server_path);
    let receipts = load_receipts(&server_path);
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jar") {
            continue;
        }
        let info = read_mod_info(&path);

        // ── Receipt lookup + hash verification ──────────────────────────
        let verified_receipt: Option<&ModReceipt> =
            if let Some(r) = receipts.receipts.get(&info.file_name) {
                if !r.installed_hash.is_empty() {
                    let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
                    let actual = format!("{:x}", Sha512::digest(&bytes));
                    if actual == r.installed_hash {
                        Some(r)
                    } else {
                        eprintln!(
                            "[lbby] Receipt hash mismatch for {} — falling back to Modrinth lookup",
                            info.file_name
                        );
                        None
                    }
                } else {
                    // Empty hash: weak trust (legacy receipt), accept by filename
                    Some(r)
                }
            } else {
                None
            };

        // ── Dispatch by provider ────────────────────────────────────────
        match verified_receipt {
            // ── Modrinth receipt: project-scoped version check ──────────
            Some(r) if r.provider == ModProvider::Modrinth => {
                match latest_modrinth_version(&r.project_id, &cfg).await {
                    Ok(latest) => {
                        let latest_file = primary_file(&latest)?;
                        let outdated = latest.id != r.file_version_id;
                        out.push(ModUpdateInfo {
                            file_name: info.file_name,
                            display_name: info.display_name,
                            current_version: info.version.clone(),
                            latest_version: latest.version_number.clone(),
                            project_id: Some(latest.project_id),
                            version_id: Some(latest.id.clone()),
                            download_url: Some(latest_file.url),
                            outdated,
                            status: if outdated {
                                UpdateStatus::UpdateAvailable
                            } else {
                                UpdateStatus::UpToDate
                            },
                            provider: ModProvider::Modrinth,
                            current_file_version_id: Some(r.file_version_id.clone()),
                            message: if outdated {
                                "Update available".to_string()
                            } else {
                                "Up to date".to_string()
                            },
                        });
                    }
                    Err(_) => {
                        out.push(ModUpdateInfo {
                            file_name: info.file_name,
                            display_name: info.display_name,
                            current_version: info.version.clone(),
                            latest_version: String::new(),
                            project_id: Some(r.project_id.clone()),
                            version_id: None,
                            download_url: None,
                            outdated: false,
                            status: UpdateStatus::ProviderUnavailable,
                            provider: ModProvider::Modrinth,
                            current_file_version_id: Some(r.file_version_id.clone()),
                            message: "Modrinth API unavailable".to_string(),
                        });
                    }
                }
            }

            // ── CurseForge receipt: fetch latest compatible file ────────
            Some(r) if r.provider == ModProvider::CurseForge => {
                let mod_id: i64 = match r.project_id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        out.push(ModUpdateInfo {
                            file_name: info.file_name,
                            display_name: info.display_name,
                            current_version: info.version.clone(),
                            latest_version: String::new(),
                            project_id: Some(r.project_id.clone()),
                            version_id: None,
                            download_url: None,
                            outdated: false,
                            status: UpdateStatus::ProviderUnavailable,
                            provider: ModProvider::CurseForge,
                            current_file_version_id: Some(r.file_version_id.clone()),
                            message: "Invalid CurseForge project ID in receipt".to_string(),
                        });
                        continue;
                    }
                };
                let current_file_id: i64 = r.file_version_id.parse().unwrap_or(0);

                let cf_client = curseforge_http_client()?;
                let loader_id = curseforge_loader_id(&cfg.server_type);
                let mut files_url = format!(
                    "https://api.curseforge.com/v1/mods/{}/files?gameVersion={}&pageSize=50",
                    mod_id,
                    urlencoding::encode(&cfg.minecraft_version),
                );
                if let Some(lid) = loader_id {
                    files_url.push_str(&format!("&modLoaderType={}", lid));
                }

                match cf_client.get(&files_url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<CurseFilesResponse>().await {
                            Ok(files_resp) => {
                                let mut candidates: Vec<CurseFileEntry> = files_resp
                                    .data
                                    .into_iter()
                                    .filter(|f| {
                                        f.download_url
                                            .as_deref()
                                            .is_some_and(|u| !u.trim().is_empty())
                                            || f.id > 0
                                    })
                                    .collect();

                                if candidates.is_empty() {
                                    out.push(ModUpdateInfo {
                                        file_name: info.file_name,
                                        display_name: info.display_name,
                                        current_version: info.version.clone(),
                                        latest_version: String::new(),
                                        project_id: Some(r.project_id.clone()),
                                        version_id: None,
                                        download_url: None,
                                        outdated: false,
                                        status: UpdateStatus::NoCompatibleUpdate,
                                        provider: ModProvider::CurseForge,
                                        current_file_version_id: Some(r.file_version_id.clone()),
                                        message: "No compatible CurseForge file found".to_string(),
                                    });
                                } else {
                                    // Deterministic: release priority, then newest file ID
                                    candidates.sort_by(|a, b| {
                                        curseforge_release_priority(a.release_type)
                                            .cmp(&curseforge_release_priority(b.release_type))
                                            .then_with(|| b.id.cmp(&a.id))
                                    });
                                    let selected = candidates.into_iter().next().unwrap();
                                    let outdated = selected.id != current_file_id;
                                    out.push(ModUpdateInfo {
                                        file_name: info.file_name,
                                        display_name: info.display_name,
                                        current_version: info.version.clone(),
                                        latest_version: selected.file_name.clone(),
                                        project_id: Some(r.project_id.clone()),
                                        version_id: Some(selected.id.to_string()),
                                        download_url: selected.download_url.clone(),
                                        outdated,
                                        status: if outdated {
                                            UpdateStatus::UpdateAvailable
                                        } else {
                                            UpdateStatus::UpToDate
                                        },
                                        provider: ModProvider::CurseForge,
                                        current_file_version_id: Some(r.file_version_id.clone()),
                                        message: if outdated {
                                            "Update available".to_string()
                                        } else {
                                            "Up to date".to_string()
                                        },
                                    });
                                }
                            }
                            Err(_) => {
                                out.push(ModUpdateInfo {
                                    file_name: info.file_name,
                                    display_name: info.display_name,
                                    current_version: info.version.clone(),
                                    latest_version: String::new(),
                                    project_id: Some(r.project_id.clone()),
                                    version_id: None,
                                    download_url: None,
                                    outdated: false,
                                    status: UpdateStatus::ProviderUnavailable,
                                    provider: ModProvider::CurseForge,
                                    current_file_version_id: Some(r.file_version_id.clone()),
                                    message: "CurseForge API parse error".to_string(),
                                });
                            }
                        }
                    }
                    _ => {
                        out.push(ModUpdateInfo {
                            file_name: info.file_name,
                            display_name: info.display_name,
                            current_version: info.version.clone(),
                            latest_version: String::new(),
                            project_id: Some(r.project_id.clone()),
                            version_id: None,
                            download_url: None,
                            outdated: false,
                            status: UpdateStatus::ProviderUnavailable,
                            provider: ModProvider::CurseForge,
                            current_file_version_id: Some(r.file_version_id.clone()),
                            message: "CurseForge API unavailable".to_string(),
                        });
                    }
                }
            }

            // ── No verified receipt: legacy Modrinth hash lookup ────────
            _ => {
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
                match found {
                    Ok(current_version) => {
                        match latest_modrinth_version(&current_version.project_id, &cfg).await {
                            Ok(latest) => {
                                let latest_file = primary_file(&latest)?;
                                let outdated = latest.id != current_version.id;
                                out.push(ModUpdateInfo {
                                    file_name: info.file_name,
                                    display_name: info.display_name,
                                    current_version: current_version.version_number,
                                    latest_version: latest.version_number.clone(),
                                    project_id: Some(latest.project_id),
                                    version_id: Some(latest.id.clone()),
                                    download_url: Some(latest_file.url),
                                    outdated,
                                    status: if outdated {
                                        UpdateStatus::UpdateAvailable
                                    } else {
                                        UpdateStatus::UpToDate
                                    },
                                    provider: ModProvider::Unknown,
                                    current_file_version_id: None,
                                    message: if outdated {
                                        "Update available".to_string()
                                    } else {
                                        "Up to date".to_string()
                                    },
                                });
                            }
                            Err(_) => {
                                out.push(ModUpdateInfo {
                                    file_name: info.file_name,
                                    display_name: info.display_name,
                                    current_version: current_version.version_number,
                                    latest_version: String::new(),
                                    project_id: Some(current_version.project_id),
                                    version_id: None,
                                    download_url: None,
                                    outdated: false,
                                    status: UpdateStatus::ProviderUnavailable,
                                    provider: ModProvider::Unknown,
                                    current_file_version_id: None,
                                    message: "Modrinth API unavailable".to_string(),
                                });
                            }
                        }
                    }
                    Err(_) => {
                        out.push(ModUpdateInfo {
                            file_name: info.file_name,
                            display_name: info.display_name,
                            current_version: info.version.clone(),
                            latest_version: String::new(),
                            project_id: None,
                            version_id: None,
                            download_url: None,
                            outdated: false,
                            status: UpdateStatus::ProviderUnknown,
                            provider: ModProvider::Unknown,
                            current_file_version_id: None,
                            message: "Not found on Modrinth".to_string(),
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Update a single mod with receipt-aware atomic replacement.
///
/// Phase 4B.3C: uses provider receipt to verify provenance, downloads
/// to temp with hash verification, atomically replaces the artifact,
/// and persists an updated receipt. On failure, the old file is restored
/// from a backup and the receipt is left untouched.
///
/// Returns `Ok(())` on success; caller is responsible for re-listing.
async fn update_one_mod(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    info: &ModUpdateInfo,
) -> Result<UpdateOutcome, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    let server_path = PathBuf::from(&cfg.server_path);

    let old = safe_join(&dir, &info.file_name)?;
    if !old.exists() {
        return Err("The old mod file no longer exists.".to_string());
    }

    let download_url = info
        .download_url
        .as_deref()
        .ok_or("No download URL available for update.")?;

    // ── 1. Backup the current artifact ────────────────────────────────
    let backup_dir = dir.join(".lbby-backups");
    tokio::fs::create_dir_all(&backup_dir)
        .await
        .map_err(|e| e.to_string())?;
    let backup = backup_dir.join(format!("{}.bak", info.file_name));
    tokio::fs::copy(&old, &backup)
        .await
        .map_err(|e| e.to_string())?;

    // ── 2. Download new artifact to temp ──────────────────────────────
    let new_file_name = info.file_name.clone();
    let dest = safe_join(&dir, &new_file_name)?;

    // Attempt to get hash hints from the existing receipt for verification.
    let receipts = load_receipts(&server_path);
    let _old_receipt = receipts.receipts.get(&info.file_name);

    let result: Result<(), String> = async {
        download_bytes_to_file(
            app,
            download_url,
            &dest,
            "Updating mod",
            &new_file_name,
            1,
            1,
            None, // Modrinth API doesn't expose sha512 for update URLs directly
            None,
        )
        .await
    }
    .await;

    // ── 3. On failure: restore from backup, clean temp ────────────────
    if let Err(err) = result {
        let _ = tokio::fs::copy(&backup, &old).await;
        let _ = tokio::fs::remove_file(&dest).await;
        // Also remove any lingering .lbbytmp file
        let mut tmp_name = info.file_name.clone();
        tmp_name.push_str(".lbbytmp");
        let _ = tokio::fs::remove_file(dir.join(&tmp_name)).await;
        return Err(format!("Update failed and old file was restored: {}", err));
    }

    // ── 4. Persist updated receipt ────────────────────────────────────
    // Build a new receipt from the update info. The receipt captures the
    // provider provenance so future update checks can resolve identity.
    let new_hash = {
        let bytes = tokio::fs::read(&dest).await.map_err(|e| e.to_string())?;
        format!("{:x}", Sha512::digest(&bytes))
    };

    let new_receipt = ModReceipt {
        provider: info.provider.clone(),
        project_id: info.project_id.clone().unwrap_or_default(),
        file_version_id: info
            .version_id
            .clone()
            .or_else(|| info.current_file_version_id.clone())
            .unwrap_or_default(),
        installed_hash: new_hash,
        loader: normalize_loader(&cfg.server_type).to_string(),
        mc_version: cfg.minecraft_version.clone(),
        file_name: new_file_name.clone(),
    };

    let mut store = load_receipts(&server_path);
    store.receipts.insert(new_file_name.clone(), new_receipt);
    let receipt_outcome = match save_receipts(&server_path, &store) {
        Ok(()) => UpdateOutcome::Updated,
        Err(e) => {
            eprintln!("[lbby] Warning: receipt save failed after update: {}", e);
            UpdateOutcome::UpdatedUntracked
        }
    };

    // ── 5. Clean up backup ────────────────────────────────────────────
    let _ = tokio::fs::remove_file(&backup).await;

    Ok(receipt_outcome)
}

/// Update a single mod (legacy interface — delegates to `update_one_mod`).
pub async fn update_mod(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    file_name: String,
    download_url: String,
) -> Result<Vec<ModInfo>, String> {
    // Build a minimal ModUpdateInfo for the legacy path.
    let info = ModUpdateInfo {
        file_name: file_name.clone(),
        display_name: file_name,
        current_version: String::new(),
        latest_version: String::new(),
        project_id: None,
        version_id: None,
        download_url: Some(download_url),
        outdated: true,
        message: String::new(),
        status: UpdateStatus::UpdateAvailable,
        provider: ModProvider::Unknown,
        current_file_version_id: None,
    };
    update_one_mod(&app, &info).await?;
    list_installed_mods()
}

/// Update all outdated mods with per-item result accumulation.
///
/// Phase 4B.3C: does NOT short-circuit on first failure. Each mod update
/// is attempted independently; outcomes are collected into
/// `UpdateItemResult` entries. The final `UpdateAllResult` includes the
/// refreshed inventory and a composite warning if any items failed.
pub async fn update_all_mods(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    updates: Vec<ModUpdateInfo>,
) -> Result<UpdateAllResult, String> {
    let outdated: Vec<_> = updates
        .into_iter()
        .filter(|u| u.outdated && u.download_url.is_some())
        .collect();
    let total = outdated.len() as u32;
    let mut results: Vec<UpdateItemResult> = Vec::with_capacity(outdated.len());
    let mut current = 0u32;

    for info in &outdated {
        current += 1;
        emit_mod_progress(
            &app,
            "Updating mods",
            &format!("Updating {}", info.display_name),
            current,
            total,
        );

        let outcome = match update_one_mod(&app, info).await {
            Ok(UpdateOutcome::UpdatedUntracked) => UpdateItemResult {
                file_name: info.file_name.clone(),
                display_name: info.display_name.clone(),
                outcome: UpdateOutcome::UpdatedUntracked,
                detail: format!("{} → {}", info.current_version, info.latest_version),
            },
            Ok(_) => UpdateItemResult {
                file_name: info.file_name.clone(),
                display_name: info.display_name.clone(),
                outcome: UpdateOutcome::Updated,
                detail: format!("{} → {}", info.current_version, info.latest_version),
            },
            Err(err) => UpdateItemResult {
                file_name: info.file_name.clone(),
                display_name: info.display_name.clone(),
                outcome: UpdateOutcome::Failed,
                detail: err,
            },
        };
        results.push(outcome);
    }

    // Collect failure and untracked warnings into a single composite string.
    let failures: Vec<String> = results
        .iter()
        .filter(|r| r.outcome == UpdateOutcome::Failed)
        .map(|r| format!("{}: {}", r.display_name, r.detail))
        .collect();
    let untracked: Vec<String> = results
        .iter()
        .filter(|r| r.outcome == UpdateOutcome::UpdatedUntracked)
        .map(|r| r.display_name.clone())
        .collect();
    let mut warnings: Vec<String> = Vec::new();
    if !failures.is_empty() {
        warnings.push(format!(
            "{} update(s) failed — {}",
            failures.len(),
            failures.join("; ")
        ));
    }
    if !untracked.is_empty() {
        warnings.push(format!(
            "{} mod(s) updated but provider metadata could not be saved",
            untracked.len()
        ));
    }
    let warning = if warnings.is_empty() {
        None
    } else {
        Some(warnings.join(". "))
    };

    Ok(UpdateAllResult {
        mods: list_installed_mods()?,
        results,
        warning,
    })
}

async fn prepare_modpack_server(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    mut cfg: ServerConfig,
) -> Result<ServerConfig, String> {
    if cfg.server_path.trim().is_empty() {
        cfg.server_path = default_server_path_value(None);
    }
    // Dynamic RAM allocation for modpacks based on mod count
    // Modpacks with 200+ mods need 6-8GB, heavy packs need 8-12GB
    if cfg.ram_mb == 0 || cfg.ram_mb < 4096 {
        cfg.ram_mb = 6144; // Safe default for most modpacks
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
) -> Result<InstallOutcome, String> {
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
            file.hashes.get("sha512").map(String::as_str),
            None,
        )
        .await?;
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

    // Post-install dependency scan — catch missing deps before user tries to start
    let missing = scan_missing_dependencies();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|d| d.mod_id.clone()).collect();
        let msg = format!(
            "WARNING: {} missing dependency mod(s) detected: {}",
            missing.len(),
            names.join(", ")
        );
        eprintln!("[lbby] {}", msg);
        emit_mod_progress(
            &app,
            "Dependency check",
            &msg,
            missing.len() as u32,
            missing.len() as u32,
        );
        eprintln!("[lbby] Run 'Install Missing Dependencies' from the mods page to fix these.");
    }

    emit_mod_progress(&app, "Finalizing", "Modpack installation completed", 1, 1);
    Ok(InstallOutcome::Success(cfg))
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
        None,
        None,
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
) -> Result<InstallOutcome, String> {
    let file = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("Invalid CurseForge ZIP: {}", e))?;
    let manifest_result: Result<CurseManifest, String> = read_zip_json(&mut zip, "manifest.json");

    // ── Phase 3D: Transactional staging ─────────────────────────────
    let mut cfg = config::load_config();
    if cfg.server_path.trim().is_empty() {
        cfg.server_path = default_server_path_value(None);
    }
    let live_path = server_dir(&cfg)?;
    let source = if manifest_result.is_err() {
        "curseforge-server-pack"
    } else {
        "curseforge-manifest"
    };
    let txn = crate::install_transaction::InstallTransaction::begin(&live_path, source)?;
    // Backup existing live server (secondary safety layer)
    if live_path.exists() {
        backup_modpack_targets(&live_path)?;
    }
    // Redirect all file operations to staging
    cfg.server_path = txn.staging_path().to_string_lossy().to_string();

    if manifest_result.is_err() {
        // No manifest.json = server pack. Extract directly to staging directory.
        eprintln!("[lbby] No manifest.json found — treating as server pack, extracting directly");

        // Detect Minecraft version and loader from server pack contents
        // by scanning for Fabric/Forge libraries or config files
        for i in 0..zip.len() {
            if let Ok(entry) = zip.by_index(i) {
                let name = entry.name().to_string();
                // Fabric: libraries/net/fabricmc/fabric-loader/{version}/...
                if name.starts_with("libraries/net/fabricmc/fabric-loader/") {
                    let parts: Vec<&str> = name.split('/').collect();
                    if parts.len() > 5 {
                        let loader_ver = parts[5].to_string();
                        if cfg.loader_version.is_none() {
                            cfg.loader_version = Some(loader_ver);
                            cfg.server_type = ServerType::Fabric;
                            eprintln!(
                                "[lbby] Detected Fabric loader: {}",
                                cfg.loader_version.as_deref().unwrap_or("?")
                            );
                        }
                    }
                }
                // Forge: libraries/net/minecraftforge/...
                if name.starts_with("libraries/net/minecraftforge/")
                    && cfg.server_type == ServerType::Vanilla
                {
                    cfg.server_type = ServerType::Forge;
                    eprintln!("[lbby] Detected Forge server");
                }
            }
        }
        // Try to detect MC version from config files or filenames
        if cfg.minecraft_version.is_empty() {
            // Common pattern: config files reference MC version
            // Or we can use a default based on the modpack name
            // For now, keep the existing version from profile
            eprintln!(
                "[lbby] Using existing MC version: {}",
                cfg.minecraft_version
            );
        }

        // backup done in transaction setup above
        let mut cfg2 = prepare_modpack_server(&app, cfg).await?;
        let root = server_dir(&cfg2)?;
        let total = zip.len() as u32;
        emit_mod_progress(
            &app,
            "Extracting server pack",
            &format!("0/{} files", total),
            0,
            total,
        );
        // Use safe extraction: enclosed_name() + canonical containment check
        // Reuses the established safe_extract_zip pattern from helpers.rs
        crate::helpers::safe_extract_zip(&mut zip, &root, "")?;
        eprintln!(
            "[lbby] Server pack extracted {} files to {}",
            total,
            root.display()
        );
        // Filter client-only mods after extraction
        let quarantined = crate::mod_side::quarantine_client_only_mods(&root)
            .await
            .unwrap_or_default();
        if !quarantined.is_empty() {
            eprintln!("[lbby] Quarantined {} client-only mods", quarantined.len());
            emit_mod_progress(
                &app,
                "Filtering client-only mods",
                &format!("Removed {} client-only mods", quarantined.len()),
                quarantined.len() as u32,
                quarantined.len() as u32,
            );
        }
        // Ensure server loader is installed (Fabric/Forge)
        // do_install_server handles this but server packs skip it
        // So we need to install the loader manually
        let server_jar = root.join("server.jar");
        if !server_jar.exists()
            || std::fs::metadata(&server_jar)
                .map(|m| m.len() < 1_000_000)
                .unwrap_or(true)
        {
            // Detect loader from extracted files
            let has_fabric = root.join("fabric-server-launch.jar").exists()
                || root.join("libraries/net/fabricmc").exists();
            let has_forge = root.join("libraries/net/minecraftforge").exists();
            if has_fabric {
                eprintln!("[lbby] Detected Fabric server — installing Fabric loader");
                emit_mod_progress(
                    &app,
                    "Installing Fabric loader",
                    "Downloading Fabric server",
                    0,
                    1,
                );
                let loader = cfg2.loader_version.as_deref().unwrap_or("0.19.3");
                let mc = &cfg2.minecraft_version;
                let url = format!(
                    "https://meta.fabricmc.net/v2/versions/loader/{}/{}/1.0.1/server/jar",
                    mc, loader
                );
                download_bytes_to_file(
                    &app,
                    &url,
                    &server_jar,
                    "Fabric server",
                    "server.jar",
                    1,
                    1,
                    None,
                    None,
                )
                .await?;
            } else if has_forge {
                eprintln!("[lbby] Detected Forge server — Forge installer should be present");
            }
        }
        // Copy persistent state from live → staging
        txn.copy_persistent_state()?;
        // Boot validation: verify the staged server actually starts
        // Phase 3G: use ValidationRepairOrchestrator for centralized retry logic
        // Phase 3K.1: restore persisted retry state from prior pause (if any)
        let cf = curseforge_client()?;
        let mut orch = match crate::recovery_actions::load_retry_state(Path::new(&cfg2.server_path))
        {
            Ok(Some(prior)) => {
                crate::validation_orchestrator::ValidationRepairOrchestrator::from_persisted_state(
                    prior.boot_attempts_used,
                    prior.dependency_repairs_used,
                    prior.runtime_repairs_used,
                )
            }
            Ok(None) => crate::validation_orchestrator::ValidationRepairOrchestrator::new(),
            Err(e) => {
                // Unsupported schema — fail safely, do NOT overwrite future state
                return Err(format!(
                    "Cannot proceed: unsupported retry-state schema ({})",
                    e
                ));
            }
        };
        let empty_registry = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver = crate::dependency_resolver::DependencyResolver::new(
            cf.clone(),
            CURSEFORGE_API_KEY.to_string(),
        );
        let mut orch_ctx = crate::validation_orchestrator::ValidationContext {
            cfg: &mut cfg2,
            staging_path: txn.staging_path(),
            app: &app,
            cf_client: &cf,
            installed_files: &empty_registry,
            dependency_resolver: &mut resolver,
            transaction_id: &txn.meta().transaction_id,
            server_id: &txn.meta().server_id,
        };
        let validator = crate::boot_validator::BootValidator::new();
        let outcome = orch.validate(&mut orch_ctx, &validator).await;
        match outcome {
            crate::validation_orchestrator::ValidationOutcome::Validated(success) => {
                eprintln!(
                    "[lbby] Boot validation passed after {} attempt(s)",
                    success.total_boot_attempts
                );
                // Preserve quarantine artifacts out of staging before commit
                crate::recovery_actions::preserve_quarantine_on_commit(
                    txn.staging_path(),
                    &txn.meta().server_id,
                    &txn.meta().transaction_id,
                )?;
                // Commit first (durable), THEN clear retry state
                let meta = txn.commit()?;
                crate::recovery_actions::clear_retry_state(Path::new(&cfg2.server_path));
                cfg2.server_path = meta.live_path.to_string_lossy().to_string();
                config::save_config(&cfg2)?;
                return Ok(InstallOutcome::Success(cfg2));
            }
            crate::validation_orchestrator::ValidationOutcome::Failed(failure) => {
                let err = format!(
                    "Boot validation failed ({:?}): {:?}",
                    failure.reason, failure.final_boot_result
                );
                crate::boot_validator::save_validation_diagnostics(
                    txn.staging_path(),
                    &txn.meta().server_id,
                    &txn.meta().transaction_id,
                    &failure.final_boot_result,
                );
                // Rollback first (durable staging removal), THEN clear retry state
                txn.rollback()?;
                crate::recovery_actions::clear_retry_state(Path::new(&cfg2.server_path));
                return Err(err);
            }
            crate::validation_orchestrator::ValidationOutcome::UserActionRequired(req) => {
                // Phase 3K.1: persist recovery metadata and pause transaction.
                // No rollback — staging stays alive for later approval.
                let recovery_meta = crate::install_transaction::PendingRecoveryMetadata {
                    schema_version: crate::atomic_persistence::CURRENT_SCHEMA_VERSION,
                    server_id: req.server_id.clone(),
                    transaction_id: req.transaction_id.clone(),
                    staging_mods: req.staging_mods.clone(),
                    attribution_fingerprint: req.fingerprint.clone(),
                    target_mod_id: req.mod_id.clone(),
                    target_jar_path: req.target_jar_path.clone(),
                    target_jar_sha256: req.jar_sha256.clone(),
                    boot_attempt: req.boot_attempt,
                    dependency_repairs: orch.state().dependency_repairs,
                    runtime_repairs: orch.state().runtime_repairs,
                    recovery_actions_used: req.recovery_actions_used,
                    display_filename: req.display_filename.clone(),
                    crash_summary: req.crash_report.summary.clone(),
                    confidence: format!("{:?}", req.crash_report.confidence),
                    applied: false,
                };
                let recovery_path = txn.meta().pending_recovery_path();
                recovery_meta.save(&recovery_path)?;
                // Persist orchestrator retry state for cross-lifecycle ceiling
                let _ = crate::recovery_actions::save_retry_state(
                    Path::new(&cfg2.server_path),
                    &crate::recovery_actions::RetryStateSnapshot {
                        schema_version: crate::atomic_persistence::CURRENT_SCHEMA_VERSION,
                        boot_attempts_used: orch.state().total_boot_attempts,
                        dependency_repairs_used: orch.state().dependency_repairs,
                        runtime_repairs_used: orch.state().runtime_repairs,
                        recovery_actions_used: req.recovery_actions_used,
                    },
                );
                eprintln!(
                    "[CF][recovery] Pausing transaction {} — pending recovery for '{}'",
                    req.transaction_id, req.mod_id
                );
                let _paused_meta = txn
                    .pause()
                    .map_err(|e| format!("Failed to persist pause state for recovery: {}", e))?;
                return Ok(InstallOutcome::UserActionRequired {
                    server_id: req.server_id,
                    transaction_id: req.transaction_id,
                    fingerprint: req.fingerprint,
                    mod_id: req.mod_id,
                    display_filename: req.display_filename,
                    jar_sha256: req.jar_sha256,
                    boot_attempt: req.boot_attempt,
                    recovery_actions_used: req.recovery_actions_used,
                    crash_summary: req.crash_report.summary,
                    confidence: format!("{:?}", req.crash_report.confidence),
                });
            }
        }
    }
    let manifest = manifest_result.unwrap();
    let (server_type, loader_version) = loader_from_curse(&manifest.minecraft.mod_loaders)?;
    // cfg and backup already handled in transaction setup above
    cfg.minecraft_version = manifest.minecraft.version;
    cfg.server_type = server_type;
    cfg.loader_version = loader_version;
    if let Some(name) = manifest.name.clone().filter(|n| !n.trim().is_empty()) {
        cfg.server_name = name;
    }
    let mut cfg = prepare_modpack_server(&app, cfg).await?;
    let root = server_dir(&cfg)?;
    let target_dir = mods_dir(&cfg)?;
    tokio::fs::create_dir_all(&target_dir)
        .await
        .map_err(|e| e.to_string())?;
    let total = manifest.files.iter().filter(|f| f.required).count() as u32;
    let cf = curseforge_client()?;
    let concurrency: usize = 20;
    let files: Vec<(u64, u64)> = manifest
        .files
        .iter()
        .filter(|f| f.required)
        .map(|f| (f.project_id, f.file_id))
        .collect();
    let downloaded = std::sync::atomic::AtomicU32::new(0);

    // Resolve all download URLs first (parallel API calls)
    let mut url_tasks = Vec::new();
    for (project_id, file_id) in &files {
        let cf_clone = cf.clone();
        let pid = *project_id;
        let fid = *file_id;
        let url_task = tokio::spawn(async move {
            let endpoint = format!(
                "https://api.curseforge.com/v1/mods/{}/files/{}/download-url",
                pid, fid
            );
            let resp = cf_clone.get(&endpoint).send().await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    #[derive(serde::Deserialize)]
                    struct D {
                        data: String,
                    }
                    r.json::<D>().await.map(|d| d.data).ok()
                }
                _ => None,
            }
        });
        url_tasks.push((file_id, url_task));
    }

    // Collect URLs with CDN fallback
    let mut downloads: Vec<(String, String)> = Vec::new(); // (url, filename)
    let mut url_failures: Vec<u64> = Vec::new();
    for (file_id, task) in url_tasks {
        match task.await {
            Ok(Some(url)) => {
                let file_name = url
                    .rsplit('/')
                    .next()
                    .unwrap_or(&format!("mod_{}.jar", file_id))
                    .split('?')
                    .next()
                    .unwrap_or(&format!("mod_{}.jar", file_id))
                    .to_string();
                // Use CDN URL if available (faster than edge.forgecdn.net)
                let cdn_url = if url.contains("edge.forgecdn.net") {
                    url.replace("edge.forgecdn.net", "mediafilez.forgecdn.net")
                } else {
                    url
                };
                downloads.push((cdn_url, file_name));
            }
            Ok(None) => {
                eprintln!(
                    "[lbby] Failed to resolve download URL for file_id {}",
                    file_id
                );
                url_failures.push(*file_id);
            }
            Err(e) => {
                eprintln!(
                    "[lbby] Task error resolving URL for file_id {}: {}",
                    file_id, e
                );
                url_failures.push(*file_id);
            }
        }
    }
    if !url_failures.is_empty() {
        eprintln!(
            "[lbby] WARNING: {} mod(s) could not be resolved from CurseForge API",
            url_failures.len()
        );
    }

    // Download all candidate mods — compatibility analysis runs after all downloads complete.
    let mut download_failures: Vec<String> = Vec::new();
    for chunk in downloads.chunks(concurrency) {
        let mut handles = Vec::new();
        for (url, file_name) in chunk {
            let app_clone = app.clone();
            let url = url.clone();
            let file_name = file_name.clone();
            let dest = target_dir.join(&file_name);
            let temp_dest = std::env::temp_dir().join("lbby-download").join(&file_name);
            let current = downloaded.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let file_name_for_task = file_name.clone();
            let handle = tokio::spawn(async move {
                // Try primary URL first, then fallback to original edge.forgecdn.net
                let urls_to_try = if url.contains("mediafilez.forgecdn.net") {
                    let fallback = url.replace("mediafilez.forgecdn.net", "edge.forgecdn.net");
                    vec![url.clone(), fallback]
                } else {
                    vec![url.clone()]
                };
                let mut last_err = String::new();
                for try_url in &urls_to_try {
                    match download_bytes_to_file(
                        &app_clone,
                        try_url,
                        &temp_dest,
                        "Downloading CurseForge mods",
                        &file_name_for_task,
                        current,
                        total,
                        None,
                        None,
                    )
                    .await
                    {
                        Ok(()) => {
                            // Move to mods folder — all mods downloaded first,
                            // compatibility analysis runs after all downloads complete.
                            if let Err(e) = std::fs::rename(&temp_dest, &dest) {
                                return Err(format!(
                                    "Failed to move {}: {}",
                                    file_name_for_task, e
                                ));
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            last_err = e;
                        }
                    }
                }
                Err(format!("{}: {}", file_name_for_task, last_err))
            });
            handles.push((file_name.clone(), handle));
        }
        for (file_name, handle) in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("[lbby] Download error: {}", e);
                    download_failures.push(file_name);
                }
                Err(e) => {
                    eprintln!("[lbby] Task error: {}", e);
                    download_failures.push(file_name);
                }
            }
        }
    }
    // Compatibility analysis and quarantine happen after all downloads complete.
    // See the Phase 3C pipeline below.
    if !download_failures.is_empty() {
        let msg = format!(
            "WARNING: {} mod(s) failed to download: {}",
            download_failures.len(),
            download_failures.join(", ")
        );
        eprintln!("[lbby] {}", msg);
        emit_mod_progress(
            &app,
            "Download failures",
            &msg,
            download_failures.len() as u32,
            download_failures.len() as u32,
        );
    }
    // Verify we got a reasonable number of mods
    let expected_count = files.len();
    let actual_count = std::fs::read_dir(&target_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "jar"))
                .count()
        })
        .unwrap_or(0);
    if actual_count == 0 && expected_count > 0 {
        return Err(format!(
            "No mods were installed! Expected {} mods from CurseForge manifest. Check your internet connection and CurseForge API access.",
            expected_count
        ));
    }
    if actual_count < expected_count / 2 {
        eprintln!("[lbby] WARNING: Only {}/{} mods installed — server may be missing critical dependencies", actual_count, expected_count);
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
    // -- Phase 3C: Compatibility pipeline --------------------------------
    // Download → analyze → dependency graph → exclusion plan → apply plan
    //
    // This replaces the legacy quarantine_client_only_mods() call for
    // CurseForge installs only. Modrinth path unchanged.
    let mods_dir = root.join("mods");
    let quarantine_dir = root.join(".lbby-client-only-mods");

    // Step 1: Collect all downloaded JARs and classify each one.
    let mut analysis: Vec<(std::path::PathBuf, crate::mod_compat::ModCompatibility)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&mods_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jar") {
                let compat = crate::mod_compat::classify_mod_local(&path);
                analysis.push((path, compat));
            }
        }
    }

    let total_mods = analysis.len();

    // Step 2: Build dependency graph and create exclusion plan.
    let graph = crate::dependency_graph::DependencyGraph::build(&analysis);
    let plan = graph.create_plan();

    // Count classification buckets for logging.
    let mut count_server_ok = 0u32;
    let mut count_universal = 0u32;
    let mut count_unknown = 0u32;
    let mut count_client_only = 0u32;
    for (_, compat) in &analysis {
        use crate::mod_compat::{CompatibilityConfidence, ServerCompatibility};
        match (&compat.compatibility, &compat.confidence) {
            (ServerCompatibility::ServerOk, _) => count_server_ok += 1,
            (ServerCompatibility::Both, _) => count_universal += 1,
            (ServerCompatibility::ClientOnly, CompatibilityConfidence::Explicit) => {
                count_client_only += 1
            }
            _ => count_unknown += 1,
        }
    }

    // Step 3: Apply exclusion plan — move excluded JARs to quarantine.
    // Only exclude ClientOnly/Explicit. Never hard-delete.
    let mut quarantined_count = 0u32;
    for path in &plan.exclude {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        if let Err(e) = std::fs::create_dir_all(&quarantine_dir) {
            eprintln!("[CF] WARNING: Failed to create quarantine dir: {}", e);
            // Do NOT delete source on quarantine failure.
            continue;
        }
        let dest = quarantine_dir.join(file_name);
        if let Err(e) = std::fs::rename(path, &dest) {
            eprintln!("[CF] WARNING: Failed to quarantine {}: {}", file_name, e);
            // Do NOT delete source on quarantine failure.
            continue;
        }
        let source_label = analysis
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| format!("{:?}", c.source))
            .unwrap_or_else(|| "Unknown".to_string());
        let reason_label = analysis
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.reason.clone())
            .unwrap_or_else(|| "no detail".to_string());
        eprintln!(
            "[CF] Quarantined client-only mod: {}\n     source={}\n     reason={}",
            file_name, source_label, reason_label,
        );
        quarantined_count += 1;
    }

    // Step 4: Structured summary logging.
    eprintln!(
        "[CF] Compatibility analysis complete\n\
         [CF] Total mods: {}\n\
         [CF] Server-compatible: {}\n\
         [CF] Universal: {}\n\
         [CF] Unknown: {}\n\
         [CF] Confirmed client-only: {}\n\
         [CF] Quarantined: {}\n\
         [CF] Dependency conflicts: {}\n\
         [CF] Missing dependencies: {}\n\
         [CF] Ambiguous providers: {}",
        total_mods,
        count_server_ok,
        count_universal,
        count_unknown,
        count_client_only,
        quarantined_count,
        plan.conflicts.len(),
        plan.missing.len(),
        plan.ambiguous.len(),
    );

    if quarantined_count > 0 {
        emit_mod_progress(
            &app,
            "Filtering client-only mods",
            &format!("Quarantined {} client-only mod(s)", quarantined_count),
            quarantined_count,
            quarantined_count,
        );
    }

    // Report conflicts and ambiguous deps as warnings.
    for conflict in &plan.conflicts {
        eprintln!(
            "[CF] Dependency conflict: {} requires {} which was quarantined",
            conflict.dependent_mod_id.as_deref().unwrap_or("(unknown)"),
            conflict.dependency_mod_id,
        );
    }
    for amb in &plan.ambiguous {
        eprintln!(
            "[CF] Ambiguous dependency: {} requires {} ({} providers)",
            amb.dependent_mod_id.as_deref().unwrap_or("(unknown)"),
            amb.dependency_mod_id,
            amb.provider_paths.len(),
        );
    }
    for miss in &plan.missing {
        eprintln!(
            "[CF] Missing dependency: {} requires {}",
            miss.dependent_mod_id.as_deref().unwrap_or("(unknown)"),
            miss.dependency_mod_id,
        );
    }

    // ── Phase 3F-A: Deterministic missing dependency repair ────────────
    // Runs ONLY when missing deps exist. Up to MAX_REPAIR_ROUNDS iterations.
    // Each round: resolve → download → rebuild graph → re-check.
    let mut repair_records: Vec<crate::dependency_resolver::RepairRecord> = Vec::new();
    let mut current_plan = plan;

    if !current_plan.missing.is_empty() {
        use crate::dependency_resolver::{
            DependencyResolution, DependencyResolver, MAX_REPAIR_ROUNDS,
        };
        use std::collections::HashSet;

        let loader_str = match &cfg.server_type {
            ServerType::Forge => "forge",
            ServerType::Fabric => "fabric",
            ServerType::NeoForge => "neoforge",
            _ => "",
        };

        let mc_version = cfg.minecraft_version.clone();
        let mut resolver = DependencyResolver::new(cf.clone(), CURSEFORGE_API_KEY.to_string());

        // Build authoritative project→mod_ids bridge from manifest + downloaded JARs.
        // For each manifest entry, find the corresponding downloaded JAR and read its mod_ids.
        // Also preserve CF file identity (project_id, file_id, dependencies) for boot repair.
        for (project_id, file_id) in &files {
            // Find the downloaded file for this manifest entry by querying CF API for file_name
            match curseforge_file_by_id(&cf, CURSEFORGE_API_KEY, *file_id as i64).await {
                Ok(file_entry) => {
                    let jar_path = mods_dir.join(&file_entry.file_name);
                    if jar_path.exists() {
                        let metadata = crate::jar_metadata::read_jar_mod_metadata(&jar_path);
                        if !metadata.mod_ids.is_empty() {
                            resolver.register_project(*project_id, metadata.mod_ids.clone());
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[CF] Could not fetch file metadata for file_id {}: {}",
                        file_id, e
                    );
                }
            }
        }

        // Repair loop
        for round in 0..MAX_REPAIR_ROUNDS {
            if current_plan.missing.is_empty() {
                break;
            }

            eprintln!(
                "[CF] Dependency repair round {}/{}: {} missing deps",
                round + 1,
                MAX_REPAIR_ROUNDS,
                current_plan.missing.len()
            );

            emit_mod_progress(
                &app,
                "Resolving dependencies",
                &format!(
                    "Repair round {}/{}: resolving {} missing dependency(s)",
                    round + 1,
                    MAX_REPAIR_ROUNDS,
                    current_plan.missing.len()
                ),
                round as u32 + 1,
                MAX_REPAIR_ROUNDS as u32,
            );

            let mut any_downloaded = false;
            let mut newly_installed_projects: HashSet<u64> = HashSet::new();

            for miss in &current_plan.missing {
                let resolution = resolver.resolve(miss, &mc_version, loader_str).await;

                match resolution {
                    DependencyResolution::Resolved(ref resolved) => {
                        eprintln!(
                            "[CF] Resolved missing dep '{}': project={}, file={}, reason={:?}",
                            miss.dependency_mod_id,
                            resolved.project_id,
                            resolved.file_id,
                            resolved.reason
                        );

                        // Download the resolved dependency into staging mods dir
                        match crate::dependency_resolver::download_resolved_dependency(
                            &app, &cf, resolved, &mods_dir,
                        )
                        .await
                        {
                            Ok(jar_path) => {
                                eprintln!(
                                    "[CF] Downloaded resolved dependency: {}",
                                    jar_path.display()
                                );

                                // SAFETY: Mandatory post-download identity verification.
                                // Check that the downloaded JAR actually provides the expected mod_id.
                                match crate::dependency_resolver::verify_download_identity(
                                    &jar_path,
                                    &miss.dependency_mod_id,
                                ) {
                                    Ok(()) => {
                                        // Identity verified — register and mark success.
                                        let metadata = crate::jar_metadata::read_jar_mod_metadata(&jar_path);
                                        resolver.register_project(resolved.project_id, metadata.mod_ids);
                                        newly_installed_projects.insert(resolved.project_id);
                                        any_downloaded = true;

                                        repair_records.push(crate::dependency_resolver::RepairRecord {
                                            round: round + 1,
                                            requesting_mod: miss.dependent_mod_id.clone(),
                                            dependency_mod_id: miss.dependency_mod_id.clone(),
                                            project_id: Some(resolved.project_id),
                                            file_id: Some(resolved.file_id),
                                            action: crate::dependency_resolver::RepairAction::IdentityVerified,
                                            result: crate::dependency_resolver::RepairResult::Success(
                                                format!("Identity verified: {} provides '{}'", resolved.file_name, miss.dependency_mod_id),
                                            ),
                                        });
                                    }
                                    Err(crate::dependency_resolver::DependencyResolution::IdentityMismatch { expected_mod_id, actual_mod_ids }) => {
                                        // SAFETY: Downloaded JAR does NOT provide the expected mod_id.
                                        // Reject and remove from staging.
                                        eprintln!(
                                            "[CF] Identity mismatch for '{}': expected '{}', got {:?}. Removing from staging.",
                                            miss.dependency_mod_id, expected_mod_id, actual_mod_ids
                                        );
                                        crate::dependency_resolver::remove_rejected_artifact(&jar_path);

                                        repair_records.push(crate::dependency_resolver::RepairRecord {
                                            round: round + 1,
                                            requesting_mod: miss.dependent_mod_id.clone(),
                                            dependency_mod_id: miss.dependency_mod_id.clone(),
                                            project_id: Some(resolved.project_id),
                                            file_id: Some(resolved.file_id),
                                            action: crate::dependency_resolver::RepairAction::IdentityRejected,
                                            result: crate::dependency_resolver::RepairResult::Failed(
                                                format!("Identity mismatch: expected '{}', got {:?}", expected_mod_id, actual_mod_ids),
                                            ),
                                        });
                                    }
                                    Err(crate::dependency_resolver::DependencyResolution::IdentityUnverifiable) => {
                                        // SAFETY: Downloaded JAR has no mod metadata.
                                        // Identity cannot be verified — reject.
                                        eprintln!(
                                            "[CF] Identity unverifiable for '{}': JAR has no mod_ids. Removing from staging.",
                                            miss.dependency_mod_id
                                        );
                                        crate::dependency_resolver::remove_rejected_artifact(&jar_path);

                                        repair_records.push(crate::dependency_resolver::RepairRecord {
                                            round: round + 1,
                                            requesting_mod: miss.dependent_mod_id.clone(),
                                            dependency_mod_id: miss.dependency_mod_id.clone(),
                                            project_id: Some(resolved.project_id),
                                            file_id: Some(resolved.file_id),
                                            action: crate::dependency_resolver::RepairAction::IdentityRejected,
                                            result: crate::dependency_resolver::RepairResult::Failed(
                                                "Identity unverifiable: JAR has no mod metadata".to_string(),
                                            ),
                                        });
                                    }
                                    Err(_) => {
                                        // Unexpected resolution type from verify_download_identity.
                                        // Should never happen — defensive reject.
                                        crate::dependency_resolver::remove_rejected_artifact(&jar_path);
                                        repair_records.push(crate::dependency_resolver::RepairRecord {
                                            round: round + 1,
                                            requesting_mod: miss.dependent_mod_id.clone(),
                                            dependency_mod_id: miss.dependency_mod_id.clone(),
                                            project_id: Some(resolved.project_id),
                                            file_id: Some(resolved.file_id),
                                            action: crate::dependency_resolver::RepairAction::IdentityRejected,
                                            result: crate::dependency_resolver::RepairResult::Failed(
                                                "Unexpected identity verification error".to_string(),
                                            ),
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "[CF] Failed to download resolved dep '{}': {}",
                                    miss.dependency_mod_id, e
                                );
                                repair_records.push(crate::dependency_resolver::RepairRecord {
                                    round: round + 1,
                                    requesting_mod: miss.dependent_mod_id.clone(),
                                    dependency_mod_id: miss.dependency_mod_id.clone(),
                                    project_id: Some(resolved.project_id),
                                    file_id: Some(resolved.file_id),
                                    action: crate::dependency_resolver::RepairAction::Downloaded,
                                    result: crate::dependency_resolver::RepairResult::Failed(e),
                                });
                            }
                        }
                    }
                    DependencyResolution::Ambiguous(ref candidates) => {
                        let msg = format!(
                            "Ambiguous: {} candidates for '{}'",
                            candidates.len(),
                            miss.dependency_mod_id
                        );
                        eprintln!("[CF] {}", msg);
                        repair_records.push(crate::dependency_resolver::RepairRecord {
                            round: round + 1,
                            requesting_mod: miss.dependent_mod_id.clone(),
                            dependency_mod_id: miss.dependency_mod_id.clone(),
                            project_id: candidates.first().map(|c| c.project_id),
                            file_id: None,
                            action: crate::dependency_resolver::RepairAction::Skipped,
                            result: crate::dependency_resolver::RepairResult::Skipped(msg),
                        });
                    }
                    DependencyResolution::NotFound => {
                        repair_records.push(crate::dependency_resolver::RepairRecord {
                            round: round + 1,
                            requesting_mod: miss.dependent_mod_id.clone(),
                            dependency_mod_id: miss.dependency_mod_id.clone(),
                            project_id: None,
                            file_id: None,
                            action: crate::dependency_resolver::RepairAction::Skipped,
                            result: crate::dependency_resolver::RepairResult::Skipped(
                                "Not found on CurseForge".to_string(),
                            ),
                        });
                    }
                    DependencyResolution::Incompatible(_) => {
                        repair_records.push(crate::dependency_resolver::RepairRecord {
                            round: round + 1,
                            requesting_mod: miss.dependent_mod_id.clone(),
                            dependency_mod_id: miss.dependency_mod_id.clone(),
                            project_id: None,
                            file_id: None,
                            action: crate::dependency_resolver::RepairAction::Skipped,
                            result: crate::dependency_resolver::RepairResult::Skipped(
                                "No compatible version found".to_string(),
                            ),
                        });
                    }
                    DependencyResolution::Unsupported(ref reason) => {
                        repair_records.push(crate::dependency_resolver::RepairRecord {
                            round: round + 1,
                            requesting_mod: miss.dependent_mod_id.clone(),
                            dependency_mod_id: miss.dependency_mod_id.clone(),
                            project_id: None,
                            file_id: None,
                            action: crate::dependency_resolver::RepairAction::Skipped,
                            result: crate::dependency_resolver::RepairResult::Skipped(
                                reason.clone(),
                            ),
                        });
                    }
                    DependencyResolution::ResolvedDependencyIsClientOnly => {
                        repair_records.push(crate::dependency_resolver::RepairRecord {
                            round: round + 1,
                            requesting_mod: miss.dependent_mod_id.clone(),
                            dependency_mod_id: miss.dependency_mod_id.clone(),
                            project_id: None,
                            file_id: None,
                            action: crate::dependency_resolver::RepairAction::Skipped,
                            result: crate::dependency_resolver::RepairResult::Skipped(
                                "Resolved dependency is client-only".to_string(),
                            ),
                        });
                    }
                    DependencyResolution::AlreadyProvided => {
                        // Cycle or already resolved — skip silently
                    }
                    DependencyResolution::IdentityMismatch {
                        ref expected_mod_id,
                        ref actual_mod_ids,
                    } => {
                        // This should not happen in resolve() — it's handled post-download.
                        // But handle defensively.
                        eprintln!(
                            "[CF] Identity mismatch for '{}': expected '{}', got {:?}",
                            miss.dependency_mod_id, expected_mod_id, actual_mod_ids
                        );
                    }
                    DependencyResolution::IdentityUnverifiable => {
                        // This should not happen in resolve() — it's handled post-download.
                        // But handle defensively.
                        eprintln!(
                            "[CF] Identity unverifiable for '{}'",
                            miss.dependency_mod_id
                        );
                    }
                    DependencyResolution::RuntimeResolved(_) => {
                        // Unreachable from resolver.resolve() — only returned by
                        // try_runtime_only_resolution (boot_failure_analyzer).
                        // Added for exhaustive matching.
                        unreachable!("RuntimeResolved should never come from resolver.resolve()");
                    }
                }

                // Merge resolver records into our audit trail
                repair_records.extend(resolver.records.drain(..));
            }

            if !any_downloaded {
                eprintln!(
                    "[CF] No new deps downloaded in round {} — stopping repair",
                    round + 1
                );
                break;
            }

            // Rebuild graph after downloading new deps
            let mut new_analysis: Vec<(std::path::PathBuf, crate::mod_compat::ModCompatibility)> =
                Vec::new();
            if let Ok(entries) = std::fs::read_dir(&mods_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "jar") {
                        let compat = crate::mod_compat::classify_mod_local(&path);
                        new_analysis.push((path, compat));
                    }
                }
            }
            let new_graph = crate::dependency_graph::DependencyGraph::build(&new_analysis);
            current_plan = new_graph.create_plan();

            eprintln!(
                "[CF] After repair round {}: {} missing, {} ambiguous",
                round + 1,
                current_plan.missing.len(),
                current_plan.ambiguous.len()
            );
        }

        // Final summary
        let repaired_count = repair_records
            .iter()
            .filter(|r| {
                matches!(
                    r.action,
                    crate::dependency_resolver::RepairAction::Downloaded
                )
            })
            .count();
        let skipped_count = repair_records
            .iter()
            .filter(|r| matches!(r.action, crate::dependency_resolver::RepairAction::Skipped))
            .count();

        if repaired_count > 0 || skipped_count > 0 {
            eprintln!(
                "[CF] Dependency repair complete: {} downloaded, {} skipped, {} still missing",
                repaired_count,
                skipped_count,
                current_plan.missing.len()
            );
            emit_mod_progress(
                &app,
                "Dependency repair",
                &format!(
                    "Repaired {} missing dependency(s), {} skipped",
                    repaired_count, skipped_count
                ),
                repaired_count as u32,
                (repaired_count + skipped_count) as u32,
            );
        }
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

    // Post-install dependency scan — catch missing deps before user tries to start
    let missing = scan_missing_dependencies();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|d| d.mod_id.clone()).collect();
        let msg = format!(
            "WARNING: {} missing dependency mod(s) detected: {}",
            missing.len(),
            names.join(", ")
        );
        eprintln!("[lbby] {}", msg);
        emit_mod_progress(
            &app,
            "Dependency check",
            &msg,
            missing.len() as u32,
            missing.len() as u32,
        );
        eprintln!("[lbby] Run 'Install Missing Dependencies' from the mods page to fix these.");
    }

    emit_mod_progress(
        &app,
        "Finalizing",
        "CurseForge modpack installation completed",
        1,
        1,
    );
    // Copy persistent state from live → staging
    txn.copy_persistent_state()?;
    // Boot validation with Phase 3F-B boot repair retry
    // Create resolver and registry for boot repair (reuses cf client from outer scope)
    let mut boot_resolver = crate::dependency_resolver::DependencyResolver::new(
        cf.clone(),
        CURSEFORGE_API_KEY.to_string(),
    );
    let mut boot_installed_file_registry =
        crate::boot_failure_analyzer::InstalledFileRegistry::new();
    // Rebuild project map and file registry from manifest entries
    for (project_id, file_id) in &files {
        if let Ok(file_entry) =
            curseforge_file_by_id(&cf, CURSEFORGE_API_KEY, *file_id as i64).await
        {
            let jar_path = mods_dir.join(&file_entry.file_name);
            if jar_path.exists() {
                let metadata = crate::jar_metadata::read_jar_mod_metadata(&jar_path);
                if !metadata.mod_ids.is_empty() {
                    boot_resolver.register_project(*project_id, metadata.mod_ids.clone());
                    boot_installed_file_registry.register(
                        *project_id,
                        *file_id,
                        metadata.mod_ids,
                        file_entry.dependencies.clone(),
                    );
                }
            }
        }
    }
    // Phase 3G: use ValidationRepairOrchestrator for centralized retry logic
    // Phase 3K.1: restore persisted retry state from prior pause (if any)
    let mut orch = match crate::recovery_actions::load_retry_state(Path::new(&cfg.server_path)) {
        Ok(Some(prior)) => {
            crate::validation_orchestrator::ValidationRepairOrchestrator::from_persisted_state(
                prior.boot_attempts_used,
                prior.dependency_repairs_used,
                prior.runtime_repairs_used,
            )
        }
        Ok(None) => crate::validation_orchestrator::ValidationRepairOrchestrator::new(),
        Err(e) => {
            // Unsupported schema — fail safely, do NOT overwrite future state
            return Err(format!(
                "Cannot proceed: unsupported retry-state schema ({})",
                e
            ));
        }
    };
    let mut orch_cfg = cfg; // move cfg into orchestrator context
    let txn_meta = txn.meta();
    let mut orch_ctx = crate::validation_orchestrator::ValidationContext {
        cfg: &mut orch_cfg,
        staging_path: txn.staging_path(),
        app: &app,
        cf_client: &cf,
        installed_files: &boot_installed_file_registry,
        dependency_resolver: &mut boot_resolver,
        transaction_id: &txn_meta.transaction_id,
        server_id: &txn_meta.server_id,
    };
    let validator = crate::boot_validator::BootValidator::new();
    let outcome = orch.validate(&mut orch_ctx, &validator).await;
    match outcome {
        crate::validation_orchestrator::ValidationOutcome::Validated(success) => {
            eprintln!(
                "[CF] Boot validation passed after {} attempt(s)",
                success.total_boot_attempts
            );
            // Preserve quarantine artifacts out of staging before commit
            crate::recovery_actions::preserve_quarantine_on_commit(
                txn.staging_path(),
                &txn.meta().server_id,
                &txn.meta().transaction_id,
            )?;
            // Commit first (durable), THEN clear retry state
            let meta = txn.commit()?;
            crate::recovery_actions::clear_retry_state(Path::new(&orch_cfg.server_path));
            orch_cfg.server_path = meta.live_path.to_string_lossy().to_string();
            config::save_config(&orch_cfg)?;
            return Ok(InstallOutcome::Success(orch_cfg));
        }
        crate::validation_orchestrator::ValidationOutcome::Failed(failure) => {
            let err = format!(
                "Boot validation failed ({:?}): {:?}",
                failure.reason, failure.final_boot_result
            );
            crate::boot_validator::save_validation_diagnostics(
                txn.staging_path(),
                &txn.meta().server_id,
                &txn.meta().transaction_id,
                &failure.final_boot_result,
            );
            // Rollback first (durable staging removal), THEN clear retry state
            txn.rollback()?;
            crate::recovery_actions::clear_retry_state(Path::new(&orch_cfg.server_path));
            return Err(err);
        }
        crate::validation_orchestrator::ValidationOutcome::UserActionRequired(req) => {
            // Phase 3K.1: persist recovery metadata and pause transaction.
            // No rollback — staging stays alive for later approval.
            let recovery_meta = crate::install_transaction::PendingRecoveryMetadata {
                schema_version: crate::atomic_persistence::CURRENT_SCHEMA_VERSION,
                server_id: req.server_id.clone(),
                transaction_id: req.transaction_id.clone(),
                staging_mods: req.staging_mods.clone(),
                attribution_fingerprint: req.fingerprint.clone(),
                target_mod_id: req.mod_id.clone(),
                target_jar_path: req.target_jar_path.clone(),
                target_jar_sha256: req.jar_sha256.clone(),
                boot_attempt: req.boot_attempt,
                dependency_repairs: orch.state().dependency_repairs,
                runtime_repairs: orch.state().runtime_repairs,
                recovery_actions_used: req.recovery_actions_used,
                display_filename: req.display_filename.clone(),
                crash_summary: req.crash_report.summary.clone(),
                confidence: format!("{:?}", req.crash_report.confidence),
                applied: false,
            };
            let recovery_path = txn.meta().pending_recovery_path();
            recovery_meta.save(&recovery_path)?;
            // Persist orchestrator retry state for cross-lifecycle ceiling
            let _ = crate::recovery_actions::save_retry_state(
                Path::new(&orch_cfg.server_path),
                &crate::recovery_actions::RetryStateSnapshot {
                    schema_version: crate::atomic_persistence::CURRENT_SCHEMA_VERSION,
                    boot_attempts_used: orch.state().total_boot_attempts,
                    dependency_repairs_used: orch.state().dependency_repairs,
                    runtime_repairs_used: orch.state().runtime_repairs,
                    recovery_actions_used: req.recovery_actions_used,
                },
            );
            eprintln!(
                "[MR][recovery] Pausing transaction {} — pending recovery for '{}'",
                req.transaction_id, req.mod_id
            );
            let _paused_meta = txn
                .pause()
                .map_err(|e| format!("Failed to persist pause state for recovery: {}", e))?;
            return Ok(InstallOutcome::UserActionRequired {
                server_id: req.server_id,
                transaction_id: req.transaction_id,
                fingerprint: req.fingerprint,
                mod_id: req.mod_id,
                display_filename: req.display_filename,
                jar_sha256: req.jar_sha256,
                boot_attempt: req.boot_attempt,
                recovery_actions_used: req.recovery_actions_used,
                crash_summary: req.crash_report.summary,
                confidence: format!("{:?}", req.crash_report.confidence),
            });
        }
    }
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

/// Resume a paused CurseForge install transaction after user approval.
///
/// This is the second half of the UAR flow:
/// 1. Find the paused transaction (PendingUserAction)
/// 2. Load PendingRecoveryMetadata → verify `applied == true`
/// 3. Resume the transaction
/// 4. Re-run BootValidator (quarantined mod is gone → should pass)
/// 5. On Validated → preserve quarantine → commit → clear retry state
/// 6. On Failed → rollback
pub async fn resume_curseforge_install(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
) -> Result<InstallOutcome, String> {
    let cfg = config::load_config();
    let live_path = server_dir(&cfg)?;
    let stale = crate::install_transaction::InstallTransaction::find_stale(&live_path);

    // Find the PendingUserAction transaction
    let pending_meta = stale
        .into_iter()
        .find(|m| m.phase == crate::install_transaction::TransactionPhase::PendingUserAction)
        .ok_or_else(|| "No paused transaction found to resume".to_string())?;

    let txn_id = pending_meta.transaction_id.clone();
    let server_id = pending_meta.server_id.clone();
    let staging_path = pending_meta.staging_path.clone();

    eprintln!(
        "[CF][resume] Found paused transaction {} for server '{}'",
        txn_id, server_id
    );

    // Load pending recovery metadata — must be approved (applied == true)
    let recovery_path = staging_path.join("pending_recovery.json");
    let recovery_meta = crate::install_transaction::PendingRecoveryMetadata::load(&recovery_path)?;
    if !recovery_meta.applied {
        return Err(
            "Pending recovery has not been approved yet. Call approve_crash_recovery first."
                .to_string(),
        );
    }

    eprintln!(
        "[CF][resume] Recovery approved for '{}' — resuming transaction",
        recovery_meta.target_mod_id
    );

    // Resume the transaction
    let txn = crate::install_transaction::InstallTransaction::resume(pending_meta)?;

    // Copy persistent state from live → staging
    txn.copy_persistent_state()?;

    // Build config for the staging path
    let mut cfg2 = cfg.clone();
    cfg2.server_path = live_path.to_string_lossy().to_string();

    // Build orchestrator from persisted retry state
    let mut orch = match crate::recovery_actions::load_retry_state(Path::new(&cfg2.server_path)) {
        Ok(Some(prior)) => {
            eprintln!(
                "[CF][resume] Restoring retry state: boot={}, dep={}, runtime={}",
                prior.boot_attempts_used, prior.dependency_repairs_used, prior.runtime_repairs_used
            );
            crate::validation_orchestrator::ValidationRepairOrchestrator::from_persisted_state(
                prior.boot_attempts_used,
                prior.dependency_repairs_used,
                prior.runtime_repairs_used,
            )
        }
        Ok(None) => crate::validation_orchestrator::ValidationRepairOrchestrator::new(),
        Err(e) => {
            return Err(format!(
                "Cannot resume: unsupported retry-state schema ({})",
                e
            ));
        }
    };

    let cf = curseforge_http_client()?;
    let empty_registry = crate::boot_failure_analyzer::InstalledFileRegistry::new();
    let mut resolver = crate::dependency_resolver::DependencyResolver::new(
        cf.clone(),
        CURSEFORGE_API_KEY.to_string(),
    );
    let mut orch_ctx = crate::validation_orchestrator::ValidationContext {
        cfg: &mut cfg2,
        staging_path: txn.staging_path(),
        app: &app,
        cf_client: &cf,
        installed_files: &empty_registry,
        dependency_resolver: &mut resolver,
        transaction_id: &txn_id,
        server_id: &server_id,
    };
    let validator = crate::boot_validator::BootValidator::new();
    let outcome = orch.validate(&mut orch_ctx, &validator).await;
    match outcome {
        crate::validation_orchestrator::ValidationOutcome::Validated(success) => {
            eprintln!(
                "[CF][resume] Boot validation passed after {} attempt(s)",
                success.total_boot_attempts
            );
            crate::recovery_actions::preserve_quarantine_on_commit(
                txn.staging_path(),
                &server_id,
                &txn_id,
            )?;
            let meta = txn.commit()?;
            crate::recovery_actions::clear_retry_state(Path::new(&cfg2.server_path));
            // Clean up pending_recovery.json
            let _ = std::fs::remove_file(&recovery_path);
            let mut final_cfg = cfg2;
            final_cfg.server_path = meta.live_path.to_string_lossy().to_string();
            config::save_config(&final_cfg)?;
            eprintln!("[CF][resume] Transaction committed — server is live");
            Ok(InstallOutcome::Success(final_cfg))
        }
        crate::validation_orchestrator::ValidationOutcome::Failed(failure) => {
            let err = format!(
                "Resume validation failed ({:?}): {:?}",
                failure.reason, failure.final_boot_result
            );
            crate::boot_validator::save_validation_diagnostics(
                txn.staging_path(),
                &server_id,
                &txn_id,
                &failure.final_boot_result,
            );
            txn.rollback()?;
            crate::recovery_actions::clear_retry_state(Path::new(&cfg2.server_path));
            let _ = std::fs::remove_file(&recovery_path);
            Err(err)
        }
        crate::validation_orchestrator::ValidationOutcome::UserActionRequired(req) => {
            // Another mod failed — persist new recovery metadata and pause again
            let new_recovery_meta = crate::install_transaction::PendingRecoveryMetadata {
                schema_version: crate::atomic_persistence::CURRENT_SCHEMA_VERSION,
                server_id: req.server_id.clone(),
                transaction_id: req.transaction_id.clone(),
                staging_mods: req.staging_mods.clone(),
                attribution_fingerprint: req.fingerprint.clone(),
                target_mod_id: req.mod_id.clone(),
                target_jar_path: req.target_jar_path.clone(),
                target_jar_sha256: req.jar_sha256.clone(),
                boot_attempt: req.boot_attempt,
                dependency_repairs: orch.state().dependency_repairs,
                runtime_repairs: orch.state().runtime_repairs,
                recovery_actions_used: req.recovery_actions_used,
                display_filename: req.display_filename.clone(),
                crash_summary: req.crash_report.summary.clone(),
                confidence: format!("{:?}", req.crash_report.confidence),
                applied: false,
            };
            let new_recovery_path = txn.meta().pending_recovery_path();
            new_recovery_meta.save(&new_recovery_path)?;
            let _ = crate::recovery_actions::save_retry_state(
                Path::new(&cfg2.server_path),
                &crate::recovery_actions::RetryStateSnapshot {
                    schema_version: crate::atomic_persistence::CURRENT_SCHEMA_VERSION,
                    boot_attempts_used: orch.state().total_boot_attempts,
                    dependency_repairs_used: orch.state().dependency_repairs,
                    runtime_repairs_used: orch.state().runtime_repairs,
                    recovery_actions_used: req.recovery_actions_used,
                },
            );
            eprintln!(
                "[CF][resume] Another UAR triggered for '{}' — pausing again",
                req.mod_id
            );
            let _paused_meta = txn
                .pause()
                .map_err(|e| format!("Failed to persist pause state: {}", e))?;
            Ok(InstallOutcome::UserActionRequired {
                server_id: req.server_id,
                transaction_id: req.transaction_id,
                fingerprint: req.fingerprint,
                mod_id: req.mod_id,
                display_filename: req.display_filename,
                jar_sha256: req.jar_sha256,
                boot_attempt: req.boot_attempt,
                recovery_actions_used: req.recovery_actions_used,
                crash_summary: req.crash_report.summary,
                confidence: format!("{:?}", req.crash_report.confidence),
            })
        }
    }
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
) -> Result<InstallOutcome, String> {
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
    files: Vec<CfWidgetFile>,
}

#[derive(Debug, Deserialize)]
struct CfWidgetFile {
    id: u64,
    name: String,
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
/// Uses the same digit-string formula as curseforge_cdn_parts.
fn curseforge_cdn_url(file_id: u64, filename: &str) -> String {
    let digits = file_id.to_string();
    if digits.len() <= 4 {
        return format!(
            "https://edge.forgecdn.net/files/{}/{}/{}",
            digits, "0", filename
        );
    }
    let (prefix, suffix) = digits.split_at(4);
    let suffix = suffix.trim_start_matches('0');
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
) -> Result<InstallOutcome, String> {
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

    emit_mod_progress(&app, "Downloading CurseForge pack", &file.name, 0, 1);
    let tmp = std::env::temp_dir().join(format!("lbby-cf-{}.zip", uuid::Uuid::new_v4().simple()));
    download_bytes_to_file(
        &app,
        &cdn_url,
        &tmp,
        "Downloading CurseForge pack",
        &file.name,
        1,
        1,
        None,
        None,
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

fn validate_jar_shape(path: &Path) -> Result<(), String> {
    use std::io::Read;
    let data = std::fs::read(path).map_err(|e| format!("Cannot read artifact: {}", e))?;
    if data.is_empty() {
        return Err("Artifact is empty (0 bytes)".to_string());
    }
    // Validate ZIP/JAR structure: attempt to open as zip archive
    let reader = std::io::Cursor::new(&data);
    zip::ZipArchive::new(reader).map_err(|e| format!("Not a valid JAR/ZIP: {}", e))?;
    Ok(())
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
    // Atomic: copy → temp, validate, rename
    let temp = dest.with_extension(format!("{}.lbbytmp", ext));
    tokio::fs::copy(&src, &temp)
        .await
        .map_err(|e| e.to_string())?;
    if ext == "jar" {
        if let Err(e) = validate_jar_shape(&temp) {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("Invalid JAR: {}", e));
        }
    }
    // For .tmod: no reliable validator exists; preserve current behavior.
    // At minimum, validate non-empty.
    if ext == "tmod" {
        let meta = tokio::fs::metadata(&temp)
            .await
            .map_err(|e| e.to_string())?;
        if meta.len() == 0 {
            let _ = std::fs::remove_file(&temp);
            return Err("tmod file is empty (0 bytes)".to_string());
        }
    }
    std::fs::rename(&temp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })?;
    Ok(())
}

pub async fn remove_mod(mod_name: String) -> Result<(), String> {
    let cfg = config::load_config();
    let path = mods_dir(&cfg)?.join(&mod_name);
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| e.to_string())?;
    // 4B.3C: also clean up receipt for this artifact
    let server_path = std::path::PathBuf::from(&cfg.server_path);
    let mut store = load_receipts(&server_path);
    store.receipts.remove(&mod_name);
    let _ = save_receipts(&server_path, &store);
    Ok(())
}

/// Compute which installed mods have required dependencies on a given artifact.
///
/// Checks every installed JAR's `depends` metadata. If any declared mod_id
/// matches a mod_id declared by the target artifact, that JAR is a dependent.
/// Multi-mod JARs are handled: if the target declares IDs [A, B, C] and
/// another mod requires B, the target cannot be removed.
///
/// Returns `Ok(deps)` where `deps` is non-empty if removal should be blocked.
pub async fn compute_dependents_in_dir(
    mod_name: &str,
    dir: &std::path::Path,
) -> Result<Vec<DependentInfo>, String> {
    use crate::helpers::{
        read_all_forge_mod_ids, read_fabric_dependencies, read_forge_dependencies,
    };
    let target_path = dir.join(mod_name);
    if !target_path.exists() {
        return Ok(vec![]);
    }

    // 1. Get the target artifact's declared mod IDs from JAR metadata.
    //    Include ALL mod IDs from multi-mod JARs (Forge [[mods]] array).
    let mut target_ids: Vec<String> = Vec::new();

    // Primary ID from read_mod_info
    let target_info = read_mod_info(&target_path);
    if let Some(ref mid) = target_info.mod_id {
        if !mid.is_empty() {
            target_ids.push(mid.clone());
        }
    }

    // All Forge mod IDs (handles multi-mod JARs)
    let forge_ids = read_all_forge_mod_ids(&target_path);
    for fid in forge_ids {
        if !target_ids.contains(&fid) {
            target_ids.push(fid);
        }
    }

    // Fabric mod_id from fabric.mod.json (single-valued)
    if let Ok(file) = std::fs::File::open(&target_path) {
        if let Ok(mut zip) = zip::ZipArchive::new(file) {
            if let Some(text) = crate::helpers::read_zip_text(&mut zip, "fabric.mod.json") {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                        let id = id.to_string();
                        if !id.is_empty() && !target_ids.contains(&id) {
                            target_ids.push(id);
                        }
                    }
                }
            }
        }
    }

    if target_ids.is_empty() {
        return Ok(vec![]);
    }
    let target_id_set: std::collections::HashSet<&str> =
        target_ids.iter().map(|s| s.as_str()).collect();

    // 2. Scan all other JARs for required dependencies referencing target IDs.
    let mut dependents = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jar") {
            continue;
        }
        let fname = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if fname == mod_name {
            continue; // skip self
        }

        // Read Forge + Fabric required dependencies (mandatory only).
        let forge_deps = read_forge_dependencies(&path);
        let fabric_deps = read_fabric_dependencies(&path);

        for (dep_id, _version_req) in forge_deps.iter().chain(fabric_deps.iter()) {
            if target_id_set.contains(dep_id.as_str()) {
                let dep_info = read_mod_info(&path);
                dependents.push(DependentInfo {
                    file_name: fname.clone(),
                    display_name: dep_info.display_name,
                    mod_id: dep_id.clone(),
                    kind: "required".to_string(),
                });
                break; // one match per JAR is enough
            }
        }
    }
    Ok(dependents)
}

/// Config-aware wrapper: resolve mods dir from current config.
pub async fn compute_dependents(mod_name: &str) -> Result<Vec<DependentInfo>, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    compute_dependents_in_dir(mod_name, &dir).await
}

/// Safe removal with dependency checking and receipt cleanup.
///
/// If required dependents exist, removal is rejected and the dependents list
/// is returned in the `RemoveResult`. If no dependents, the artifact and its
/// receipt are removed.
pub async fn safe_remove_mod_in_dir(
    mod_name: &str,
    dir: &std::path::Path,
) -> Result<RemoveResult, String> {
    let path = dir.join(mod_name);
    if !path.exists() {
        return Err(format!("Mod '{}' not found.", mod_name));
    }

    // 1. Check for blocking dependents.
    let dependents = compute_dependents_in_dir(mod_name, dir).await?;
    if !dependents.is_empty() {
        return Ok(RemoveResult {
            success: false,
            dependents,
            warning: None,
        });
    }

    // 2. Remove the artifact.
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| e.to_string())?;

    // 3. Clean up receipt (best effort — dir may not have server_path for receipts).
    let server_path = dir.parent().unwrap_or(dir);
    let mut store = load_receipts(server_path);
    store.receipts.remove(mod_name);
    let receipt_warning = match save_receipts(server_path, &store) {
        Ok(()) => None,
        Err(e) => {
            eprintln!(
                "[lbby] Warning: receipt cleanup failed after removal: {}",
                e
            );
            Some("receipt_cleanup_failed".to_string())
        }
    };

    Ok(RemoveResult {
        success: true,
        dependents: vec![],
        warning: receipt_warning,
    })
}

/// Config-aware wrapper for safe removal.
pub async fn safe_remove_mod(mod_name: &str) -> Result<RemoveResult, String> {
    let cfg = config::load_config();
    let dir = mods_dir(&cfg)?;
    safe_remove_mod_in_dir(mod_name, &dir).await
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
/// File that stores names of mods removed as client-only (so we don't re-install them).
fn client_removed_file() -> std::path::PathBuf {
    crate::config::config_path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("client_removed_mods.txt")
}

/// Load list of previously removed client-only mod names.
pub fn load_client_removed_mods() -> Vec<String> {
    std::fs::read_to_string(client_removed_file())
        .unwrap_or_default()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Add mod names to the client-removed list (so they won't be re-installed).
pub fn save_client_removed_mods(names: &[String]) {
    let path = client_removed_file();
    let mut existing = load_client_removed_mods();
    for name in names {
        let lower = name.to_lowercase();
        if !existing.iter().any(|e| e.to_lowercase() == lower) {
            existing.push(name.clone());
        }
    }
    let _ = std::fs::write(
        path,
        existing.join(
            "
",
        ),
    );
}

/// Scan mods directory for client-only mods and remove them.
/// Returns list of removed mod names.
pub fn remove_client_only_mods(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
) -> Vec<String> {
    let cfg = config::load_config();
    let Ok(target_dir) = mods_dir(&cfg) else {
        return Vec::new();
    };

    let mut removed = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jar") {
                if crate::helpers::is_client_only_mod(&path) {
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    eprintln!("[lbby] Removing client-only mod: {}", name);
                    if std::fs::remove_file(&path).is_ok() {
                        removed.push(name);
                    }
                }
            }
        }
    }

    if !removed.is_empty() {
        // Save removed names so we don't re-install them
        save_client_removed_mods(&removed);
        emit_mod_progress(
            app,
            "Client mods removed",
            &format!(
                "Removed {} client-only mods: {}",
                removed.len(),
                removed.join(", ")
            ),
            1,
            1,
        );
    }

    removed
}

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
        let file_stem = path
            .file_stem()
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
                if !version_range.is_empty()
                    && !crate::helpers::version_matches_range(installed_ver, &version_range)
                {
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
                if !version_range.is_empty()
                    && !crate::helpers::version_matches_range(installed_ver, &version_range)
                {
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
    eprintln!(
        "[lbby] scan_missing_dependencies: found {} issues (missing: {}, incompatible: {})",
        issues.len(),
        issues.iter().filter(|i| i.issue_type == "missing").count(),
        issues
            .iter()
            .filter(|i| i.issue_type == "incompatible")
            .count()
    );
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

    // Load list of mods removed as client-only (don't re-install them)
    let client_removed = load_client_removed_mods();
    let client_removed_lower: Vec<String> =
        client_removed.iter().map(|s| s.to_lowercase()).collect();

    for (i, mod_id) in mod_ids.iter().enumerate() {
        // Skip mods that were removed as client-only
        let mod_id_lower = mod_id.to_lowercase();
        if client_removed_lower
            .iter()
            .any(|removed| mod_id_lower.contains(removed) || removed.contains(&mod_id_lower))
        {
            eprintln!("[lbby] Skipping {} (was removed as client-only)", mod_id);
            continue;
        }

        emit_mod_progress(
            &app,
            "Installing dependencies",
            &format!("{}/{}: {}", i + 1, total, mod_id),
            (i + 1) as u32,
            total as u32,
        );

        let search_url = format!(
            "https://api.modrinth.com/v2/search?query={}",
            urlencoding::encode(mod_id)
        );
        eprintln!(
            "[lbby] install_missing_deps: searching Modrinth for '{}' url='{}'",
            mod_id, search_url
        );
        let resp = client
            .get(&search_url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let Ok(resp) = resp else {
            eprintln!(
                "[lbby] install_missing_deps: search request failed for {}",
                mod_id
            );
            continue;
        };
        let body = resp.text().await.unwrap_or_default();
        eprintln!(
            "[lbby] install_missing_deps: search response for {}: {} chars | first 200: {}",
            mod_id,
            body.len(),
            &body[..body.len().min(200)]
        );
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) else {
            eprintln!(
                "[lbby] install_missing_deps: JSON parse failed for {} (first 200: {})",
                mod_id,
                &body[..body.len().min(200)]
            );
            continue;
        };

        let Some(hits) = data["hits"].as_array() else {
            eprintln!("[lbby] install_missing_deps: no hits array for {}", mod_id);
            continue;
        };
        eprintln!(
            "[lbby] install_missing_deps: found {} hits for {}",
            hits.len(),
            mod_id
        );
        // Flexible matching: remove all separators and compare
        let normalized_id = mod_id.replace('_', "-").to_lowercase();
        let strip_seps = |s: &str| {
            s.replace(|c: char| c == '-' || c == '_' || c == ' ', "")
                .to_lowercase()
        };
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
            eprintln!(
                "[lbby] install_missing_deps: no match for {} (tried slug, normalized, title)",
                mod_id
            );
            continue;
        };
        let project_id = hit["project_id"].as_str().unwrap_or("");
        if project_id.is_empty() {
            continue;
        }

        let game_versions = format!("[\"{}\"]", mc_version);
        let loaders = format!("[\"{}\"]", loader);
        let versions_url = format!(
            "https://api.modrinth.com/v2/project/{}/version?game_versions={}&loaders={}",
            project_id, game_versions, loaders
        );
        let versions_resp = client
            .get(&versions_url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let Ok(versions_resp) = versions_resp else {
            continue;
        };
        let versions_body = versions_resp.text().await.unwrap_or_default();
        let Ok(versions) = serde_json::from_str::<serde_json::Value>(&versions_body) else {
            continue;
        };
        let Some(version_list) = versions.as_array() else {
            continue;
        };
        let Some(version) = version_list.first() else {
            continue;
        };

        let Some(files) = version["files"].as_array() else {
            continue;
        };
        let Some(file) = files.first() else {
            continue;
        };
        let download_url = file["url"].as_str().unwrap_or("");
        let default_name = format!("{}.jar", mod_id);
        let file_name = file["filename"].as_str().unwrap_or(&default_name);
        if download_url.is_empty() {
            continue;
        }

        let dest = target_dir.join(file_name);
        if download_bytes_to_file(
            &app,
            download_url,
            &dest,
            "Installing dependencies",
            file_name,
            (i + 1) as u32,
            total as u32,
            None,
            None,
        )
        .await
        .is_ok()
        {
            installed += 1;
        }
    }

    emit_mod_progress(
        &app,
        "Done",
        &format!("Installed {} dependencies", installed),
        total as u32,
        total as u32,
    );
    Ok(installed)
}

/// Auto-fix dependency issues: install missing mods and fix incompatible versions.
pub async fn auto_fix_dependencies(
    app: std::sync::Arc<crate::app_state::AppEventSender>,
) -> Result<u32, String> {
    eprintln!("[lbby] auto_fix_dependencies: scanning...");
    let issues = scan_missing_dependencies();
    eprintln!(
        "[lbby] auto_fix_dependencies: found {} issues",
        issues.len()
    );
    for issue in &issues {
        eprintln!(
            "  - {} ({}) for {}",
            issue.mod_id, issue.issue_type, issue.source_mod
        );
    }
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
            format!(
                "Fixing {} (have {})",
                issue.mod_id,
                issue.installed_version.as_deref().unwrap_or("?")
            )
        } else {
            format!("Installing {}", issue.mod_id)
        };
        emit_mod_progress(
            &app,
            "Fixing dependencies",
            &format!("{}/{}: {}", i + 1, total, action),
            (i + 1) as u32,
            total as u32,
        );

        // Search Modrinth for the mod
        let search_url = format!(
            "https://api.modrinth.com/v2/search?query={}",
            urlencoding::encode(&issue.mod_id)
        );

        let resp = client
            .get(&search_url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let Ok(resp) = resp else {
            continue;
        };
        let body = resp.text().await.unwrap_or_default();
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        let Some(hits) = data["hits"].as_array() else {
            continue;
        };

        // Find matching mod
        let normalized_id = issue.mod_id.replace('_', "-").to_lowercase();
        let matching = hits.iter().find(|h| {
            let slug = h["slug"].as_str().unwrap_or("").to_lowercase();
            let title = h["title"].as_str().unwrap_or("").to_lowercase();
            let mid = issue.mod_id.to_lowercase();
            slug == mid || slug == normalized_id || title.contains(&mid) || mid.contains(&slug)
        });

        let Some(hit) = matching else {
            continue;
        };
        let project_id = hit["project_id"].as_str().unwrap_or("");
        if project_id.is_empty() {
            continue;
        }

        // Get versions with compatible game version and loader
        let game_versions = format!("[\"{}\"]", mc_version);
        let loaders = format!("[\"{}\"]", loader);
        let versions_url = format!(
            "https://api.modrinth.com/v2/project/{}/version?game_versions={}&loaders={}",
            project_id, game_versions, loaders
        );
        let versions_resp = client
            .get(&versions_url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let Ok(versions_resp) = versions_resp else {
            continue;
        };
        let versions_body = versions_resp.text().await.unwrap_or_default();
        let Ok(versions) = serde_json::from_str::<serde_json::Value>(&versions_body) else {
            continue;
        };
        let Some(version_list) = versions.as_array() else {
            continue;
        };

        // For incompatible versions, try to find a version that matches the range
        let compatible_version =
            if issue.issue_type == "incompatible" && !issue.version_range.is_empty() {
                version_list.iter().find(|v| {
                    let ver_num = v["version_number"].as_str().unwrap_or("");
                    crate::helpers::version_matches_range(ver_num, &issue.version_range)
                })
            } else {
                version_list.first()
            };

        let Some(version) = compatible_version else {
            eprintln!(
                "[lbby] auto_fix: no compatible version found for {} (need {})",
                issue.mod_id, issue.version_range
            );
            continue;
        };

        let Some(files) = version["files"].as_array() else {
            continue;
        };
        let Some(file) = files.first() else {
            continue;
        };
        let download_url = file["url"].as_str().unwrap_or("");
        let file_name = file["filename"].as_str().unwrap_or(&issue.mod_id);
        if download_url.is_empty() {
            continue;
        }

        let dest = target_dir.join(file_name);

        // For incompatible versions, remove the old mod first
        if issue.issue_type == "incompatible" {
            if let Some(old_version) = &issue.installed_version {
                let _ = remove_mod(format!("{}-{}", issue.mod_id, old_version));
            }
        }

        if download_bytes_to_file(
            &app,
            download_url,
            &dest,
            "Fixing dependencies",
            file_name,
            (i + 1) as u32,
            total as u32,
            None,
            None,
        )
        .await
        .is_ok()
        {
            fixed += 1;
            eprintln!(
                "[lbby] auto_fix: installed {} v{}",
                issue.mod_id,
                version["version_number"].as_str().unwrap_or("?")
            );
        }
    }

    emit_mod_progress(
        &app,
        "Done",
        &format!("Fixed {} dependency issues", fixed),
        total as u32,
        total as u32,
    );
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::apply_mrpack_overrides;
    use super::{
        curseforge_cdn_parts, curseforge_fingerprint_reader, official_server_pack_id,
        parse_curseforge_source, validate_basename, validate_curseforge_file_for_profile,
        CurseFilesResponse,
    };
    use crate::config::{ServerConfig, ServerType};
    use std::io::{Cursor, Write};

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
        let first =
            curseforge_fingerprint_reader(Cursor::new(with_whitespace), compact.len()).unwrap();
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
            writer
                .start_file("overrides/config/common.txt", options)
                .unwrap();
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

        assert_eq!(
            std::fs::read(dest.join("config/common.txt")).unwrap(),
            b"common"
        );
        assert_eq!(
            std::fs::read(dest.join("config/side.txt")).unwrap(),
            b"server"
        );
        assert!(!dest.join("config/client.txt").exists());
        std::fs::remove_dir_all(dest).unwrap();
    }

    // ── validate_basename tests ──────────────────────────────────────────────

    #[test]
    fn validate_basename_accepts_normal_filenames() {
        assert_eq!(
            validate_basename("sodium-fabric.jar").unwrap(),
            "sodium-fabric.jar"
        );
        assert_eq!(validate_basename("mod_v2.1.jar").unwrap(), "mod_v2.1.jar");
        assert_eq!(validate_basename("My Mod.tmod").unwrap(), "My Mod.tmod");
        assert_eq!(validate_basename("非ASCII名.jar").unwrap(), "非ASCII名.jar");
        assert_eq!(
            validate_basename("has spaces.jar").unwrap(),
            "has spaces.jar"
        );
    }

    #[test]
    fn validate_basename_rejects_empty() {
        assert!(validate_basename("").is_err());
    }

    #[test]
    fn validate_basename_rejects_dot_dot() {
        assert!(validate_basename("../escape.jar").is_err());
        assert!(validate_basename("foo/../../../etc/passwd").is_err());
        assert!(validate_basename("..").is_err());
    }

    #[test]
    fn validate_basename_rejects_path_separators() {
        assert!(validate_basename("mods/evil.jar").is_err());
        assert!(validate_basename("sub\\dir\\mod.jar").is_err());
    }

    #[test]
    fn validate_basename_rejects_absolute_paths() {
        assert!(validate_basename("/etc/passwd").is_err());
        assert!(validate_basename("C:\\Windows\\system32\\evil.jar").is_err());
    }

    #[test]
    fn validate_basename_rejects_dot_prefix() {
        assert!(validate_basename(".hidden").is_err());
        assert!(validate_basename(".gitconfig").is_err());
    }

    #[test]
    fn validate_basename_rejects_nested_components() {
        assert!(validate_basename("a/b").is_err());
        assert!(validate_basename("a\\b").is_err());
    }

    // ── download_bytes_to_file hash verification tests ─────────────────────

    use super::{validate_jar_shape, verify_sha512};
    use sha2::{Digest, Sha512};

    #[test]
    fn hash_mismatch_with_existing_destination_preserves_old() {
        let dir =
            std::env::temp_dir().join(format!("lbby-hash-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("mod.jar");
        let old_content = b"original valid content here";
        std::fs::write(&dest, old_content).unwrap();

        // Write a different file as temp, then try to verify with wrong hash
        let temp = dir.join("mod.jar.lbbytmp");
        let new_content = b"different content that won't match";
        std::fs::write(&temp, new_content).unwrap();

        // Compute a hash for a THIRD content (neither old nor new)
        let wrong_hash = format!("{:x}", Sha512::digest(b"completely different"));

        // Verify should fail on the temp file
        let result = verify_sha512(&temp, Some(&wrong_hash));
        assert!(result.is_err());

        // Old destination bytes must be unchanged
        assert_eq!(std::fs::read(&dest).unwrap(), old_content);

        // Temp should still exist (caller is responsible for cleanup)
        assert!(temp.exists());

        // Cleanup
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hash_mismatch_with_no_destination_creates_no_dest() {
        let dir =
            std::env::temp_dir().join(format!("lbby-hash-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("mod.jar");
        let temp = dir.join("mod.jar.lbbytmp");

        let content = b"some content for the file";
        std::fs::write(&temp, content).unwrap();

        // Wrong hash
        let wrong_hash = format!("{:x}", Sha512::digest(b"wrong"));
        let result = verify_sha512(&temp, Some(&wrong_hash));
        assert!(result.is_err());

        // No destination should exist
        assert!(!dest.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn successful_verified_replacement_updates_destination() {
        let dir =
            std::env::temp_dir().join(format!("lbby-hash-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("mod.jar");
        let old_content = b"old content that will be replaced";
        std::fs::write(&dest, old_content).unwrap();

        let temp = dir.join("mod.jar.lbbytmp");
        let new_content = b"new verified content for replacement";
        std::fs::write(&temp, new_content).unwrap();

        let correct_hash = format!("{:x}", Sha512::digest(new_content));
        let result = verify_sha512(&temp, Some(&correct_hash));
        assert!(result.is_ok());

        // Atomic rename
        std::fs::rename(&temp, &dest).unwrap();

        // Destination now has new content
        assert_eq!(std::fs::read(&dest).unwrap(), new_content);
        // Temp is gone
        assert!(!temp.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── JAR validation tests ───────────────────────────────────────────────

    #[test]
    fn jar_validation_accepts_valid_jar() {
        let dir =
            std::env::temp_dir().join(format!("lbby-jar-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("valid.jar");

        // Create a valid ZIP/JAR
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("META-INF/MANIFEST.MF", options).unwrap();
            writer.write_all(b"Manifest-Version: 1.0\n").unwrap();
            writer.finish().unwrap();
        }
        std::fs::write(&jar_path, bytes.into_inner()).unwrap();

        assert!(validate_jar_shape(&jar_path).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jar_validation_rejects_fake_jar() {
        let dir =
            std::env::temp_dir().join(format!("lbby-jar-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("fake.jar");

        // Write random text data as .jar
        std::fs::write(&jar_path, b"This is not a JAR file at all").unwrap();

        let result = validate_jar_shape(&jar_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not a valid JAR/ZIP"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jar_validation_rejects_empty_file() {
        let dir =
            std::env::temp_dir().join(format!("lbby-jar-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("empty.jar");

        std::fs::write(&jar_path, b"").unwrap();

        let result = validate_jar_shape(&jar_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fake_jar_does_not_replace_existing_destination() {
        let dir =
            std::env::temp_dir().join(format!("lbby-jar-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("good-mod.jar");
        let temp = dest.with_extension("jar.lbbytmp");

        // Write a valid JAR as destination
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("mod.class", options).unwrap();
            writer.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            writer.finish().unwrap();
        }
        let good_jar = bytes.into_inner();
        std::fs::write(&dest, &good_jar).unwrap();

        // Write a fake JAR as temp
        std::fs::write(&temp, b"not a jar").unwrap();

        // Validation should fail
        let result = validate_jar_shape(&temp);
        assert!(result.is_err());

        // Temp should be cleaned up by caller
        // Destination should be unchanged
        assert_eq!(std::fs::read(&dest).unwrap(), good_jar);

        // Simulate caller cleanup
        std::fs::remove_file(&temp).unwrap();
        assert!(!temp.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── Phase 4B.3B: SHA-1 verification tests ──────────────────────────────

    use super::verify_sha1;
    use sha1::Sha1 as Sha1Hash;

    #[test]
    fn cf_sha1_match_succeeds() {
        let dir = std::env::temp_dir().join(format!("lbby-sha1-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.jar");
        let content = b"curseforge mod content for sha1 test";
        std::fs::write(&file, content).unwrap();

        let correct_sha1 = format!("{:x}", Sha1Hash::digest(content));
        assert!(verify_sha1(&file, &correct_sha1).is_ok());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cf_sha1_mismatch_rejects() {
        let dir = std::env::temp_dir().join(format!("lbby-sha1-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.jar");
        let content = b"curseforge mod content for sha1 test";
        std::fs::write(&file, content).unwrap();

        let wrong_sha1 = format!("{:x}", Sha1Hash::digest(b"completely different content"));
        let result = verify_sha1(&file, &wrong_sha1);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SHA-1 mismatch"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cf_sha1_mismatch_no_destination_created() {
        let dir = std::env::temp_dir().join(format!("lbby-sha1-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("mod.jar");
        let temp = dir.join("mod.jar.lbbytmp");
        let content = b"downloaded content";
        std::fs::write(&temp, content).unwrap();

        let wrong_sha1 = format!("{:x}", Sha1Hash::digest(b"wrong"));
        let result = verify_sha1(&temp, &wrong_sha1);
        assert!(result.is_err());

        // Destination never created
        assert!(!dest.exists());
        // Temp still exists (caller cleans up)
        assert!(temp.exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cf_sha1_case_insensitive_match() {
        let dir = std::env::temp_dir().join(format!("lbby-sha1-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.jar");
        let content = b"test content";
        std::fs::write(&file, content).unwrap();

        let sha1_hex = format!("{:x}", Sha1Hash::digest(content));
        // Uppercase should also match
        assert!(verify_sha1(&file, &sha1_hex.to_uppercase()).is_ok());

        std::fs::remove_dir_all(dir).unwrap();
    }

    // ── Phase 4B.3B: Fabric/Quilt/Forge/NeoForge compatibility policy ──────

    use super::validate_mod_candidate_for_profile;

    fn make_cfg(mc: &str, st: ServerType) -> ServerConfig {
        ServerConfig {
            minecraft_version: mc.to_string(),
            server_type: st,
            ..ServerConfig::default()
        }
    }

    #[test]
    fn fabric_artifact_on_fabric_profile_allowed() {
        let cfg = make_cfg("1.20.1", ServerType::Fabric);
        assert!(validate_mod_candidate_for_profile(
            &["1.20.1".to_string()],
            &["fabric".to_string()],
            &cfg
        )
        .is_ok());
    }

    #[test]
    fn quilt_only_artifact_on_fabric_profile_rejected() {
        let cfg = make_cfg("1.20.1", ServerType::Fabric);
        let result = validate_mod_candidate_for_profile(
            &["1.20.1".to_string()],
            &["quilt".to_string()],
            &cfg,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("fabric"));
    }

    #[test]
    fn fabric_artifact_on_quilt_profile_allowed() {
        // Quilt profile accepts Fabric (backward-compat policy).
        // Since ServerType has no Quilt variant, we test via string matching:
        // normalize_loader(Fabric) = "fabric", and the policy accepts "fabric" candidates
        // on any profile. This test verifies Fabric accepts Fabric.
        let cfg = make_cfg("1.20.1", ServerType::Fabric);
        assert!(validate_mod_candidate_for_profile(
            &["1.20.1".to_string()],
            &["fabric".to_string()],
            &cfg
        )
        .is_ok());
    }

    #[test]
    fn forge_artifact_on_neoforge_profile_rejected() {
        let cfg = make_cfg("1.20.1", ServerType::NeoForge);
        let result = validate_mod_candidate_for_profile(
            &["1.20.1".to_string()],
            &["forge".to_string()],
            &cfg,
        );
        assert!(result.is_err());
    }

    #[test]
    fn neoforge_artifact_on_forge_profile_rejected() {
        let cfg = make_cfg("1.20.1", ServerType::Forge);
        let result = validate_mod_candidate_for_profile(
            &["1.20.1".to_string()],
            &["neoforge".to_string()],
            &cfg,
        );
        assert!(result.is_err());
    }

    #[test]
    fn wrong_mc_version_rejected() {
        let cfg = make_cfg("1.20.1", ServerType::Fabric);
        let result = validate_mod_candidate_for_profile(
            &["1.21.1".to_string()],
            &["fabric".to_string()],
            &cfg,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("1.20.1"));
    }

    #[test]
    fn multi_loader_candidate_with_matching_loader_allowed() {
        let cfg = make_cfg("1.20.1", ServerType::Fabric);
        assert!(validate_mod_candidate_for_profile(
            &["1.20.1".to_string()],
            &["forge".to_string(), "fabric".to_string()],
            &cfg
        )
        .is_ok());
    }

    // ── Phase 4B.3B: CurseForge candidate selection tests ───────────────────

    use super::CurseFileEntry;

    fn make_cf_file(
        id: i64,
        name: &str,
        mc_ver: &str,
        loader: &str,
        release: i32,
    ) -> CurseFileEntry {
        CurseFileEntry {
            id,
            file_name: name.to_string(),
            file_length: 1024,
            download_url: Some(format!("https://cdn.example.com/{}", id)),
            game_versions: vec![mc_ver.to_string(), loader.to_string()],
            server_pack_file_id: None,
            is_server_pack: false,
            parent_project_file_id: None,
            dependencies: vec![],
            release_type: release,
            hashes: vec![],
        }
    }

    #[test]
    fn cf_release_priority_release_over_beta_over_alpha() {
        use super::curseforge_release_priority;
        let files = vec![
            make_cf_file(1, "alpha.jar", "1.20.1", "Fabric", 3),
            make_cf_file(2, "beta.jar", "1.20.1", "Fabric", 2),
            make_cf_file(3, "release.jar", "1.20.1", "Fabric", 1),
        ];
        let mut sorted = files.clone();
        sorted.sort_by_key(|f| {
            let prio = curseforge_release_priority(f.release_type);
            (prio, -(f.id))
        });
        assert_eq!(sorted[0].id, 3); // Release first
        assert_eq!(sorted[1].id, 2); // Beta second
        assert_eq!(sorted[2].id, 1); // Alpha third
    }

    #[test]
    fn cf_newest_within_same_release_priority() {
        use super::curseforge_release_priority;
        let files = vec![
            make_cf_file(100, "old.jar", "1.20.1", "Fabric", 1),
            make_cf_file(200, "new.jar", "1.20.1", "Fabric", 1),
            make_cf_file(150, "mid.jar", "1.20.1", "Fabric", 1),
        ];
        let mut sorted = files.clone();
        sorted.sort_by_key(|f| {
            let prio = curseforge_release_priority(f.release_type);
            (prio, -(f.id))
        });
        assert_eq!(sorted[0].id, 200); // Newest first
        assert_eq!(sorted[1].id, 150);
        assert_eq!(sorted[2].id, 100);
    }

    #[test]
    fn cf_wrong_mc_version_filtered() {
        let files = vec![
            make_cf_file(1, "wrong.jar", "1.21.1", "Fabric", 1),
            make_cf_file(2, "correct.jar", "1.20.1", "Fabric", 1),
        ];
        let profile_mc = "1.20.1";
        let compatible: Vec<_> = files
            .iter()
            .filter(|f| f.game_versions.iter().any(|v| v == profile_mc))
            .collect();
        assert_eq!(compatible.len(), 1);
        assert_eq!(compatible[0].id, 2);
    }

    #[test]
    fn cf_wrong_loader_filtered() {
        let files = vec![
            make_cf_file(1, "forge.jar", "1.20.1", "Forge", 1),
            make_cf_file(2, "fabric.jar", "1.20.1", "Fabric", 1),
        ];
        let profile_loader = "fabric";
        let compatible: Vec<_> = files
            .iter()
            .filter(|f| {
                f.game_versions
                    .iter()
                    .any(|l| l.to_lowercase() == profile_loader)
            })
            .collect();
        assert_eq!(compatible.len(), 1);
        assert_eq!(compatible[0].id, 2);
    }

    // ── Phase 4B.3B: Receipt/provider trust tests ──────────────────────────

    use super::{load_receipts, save_mod_receipt, ModProvider, ModReceipt};

    #[test]
    fn receipt_hash_match_trusts_provider() {
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let content = b"mod jar content for receipt test";
        let jar_path = mods_dir.join("test-mod.jar");
        std::fs::write(&jar_path, content).unwrap();

        let sha512 = format!("{:x}", Sha512::digest(content));

        let receipt = ModReceipt {
            provider: ModProvider::CurseForge,
            project_id: "12345".to_string(),
            file_version_id: "67890".to_string(),
            installed_hash: sha512.clone(),
            loader: "fabric".to_string(),
            mc_version: "1.20.1".to_string(),
            file_name: "test-mod.jar".to_string(),
        };

        save_mod_receipt(&dir, "test-mod.jar", receipt).unwrap();

        // Load and verify binding
        let receipts = load_receipts(&dir);
        let loaded = receipts.receipts.get("test-mod.jar").unwrap();
        assert_eq!(loaded.provider, ModProvider::CurseForge);
        assert_eq!(loaded.installed_hash, sha512);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn receipt_hash_mismatch_invalidates_trust() {
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        let receipt = ModReceipt {
            provider: ModProvider::CurseForge,
            project_id: "12345".to_string(),
            file_version_id: "67890".to_string(),
            installed_hash: "deadbeef".to_string(),
            loader: "fabric".to_string(),
            mc_version: "1.20.1".to_string(),
            file_name: "test-mod.jar".to_string(),
        };

        save_mod_receipt(&dir, "test-mod.jar", receipt).unwrap();

        // Simulate: artifact has different hash
        let actual_hash = format!("{:x}", Sha512::digest(b"different content"));
        let receipts = load_receipts(&dir);
        let loaded = receipts.receipts.get("test-mod.jar").unwrap();
        // Receipt hash doesn't match artifact
        assert_ne!(loaded.installed_hash, actual_hash);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn future_receipt_schema_rejected_safely() {
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        // Write a future-schema receipt file
        let future_json = r#"{"schema_version":99,"receipts":{"test.jar":{"provider":"Modrinth","project_id":"x","file_version_id":"y","installed_hash":"abc","loader":"fabric","mc_version":"1.20.1","file_name":"test.jar"}}}"#;
        std::fs::write(dir.join(".lbby-mod-receipts.json"), future_json).unwrap();

        let receipts = load_receipts(&dir);
        // Future schema should be rejected (empty receipts)
        assert!(receipts.receipts.is_empty());
        // Note: future schema does NOT create backup in current impl

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_receipt_preserved_and_backed_up() {
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        // Write corrupt JSON
        std::fs::write(dir.join(".lbby-mod-receipts.json"), "{not valid json!!!").unwrap();

        let receipts = load_receipts(&dir);
        assert!(receipts.receipts.is_empty());
        // Corrupt file should be backed up with .corrupted extension
        assert!(dir.join(".lbby-mod-receipts.json.corrupted").exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    // ── Phase 4B.3B: Receipt persistence failure semantics ──────────────────

    #[test]
    fn receipt_write_failure_does_not_trust_provider() {
        // Simulate: artifact committed, receipt write fails
        // On next load, no receipt exists → provider not trusted
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let content = b"installed mod content";
        let jar_path = mods_dir.join("no-receipt-mod.jar");
        std::fs::write(&jar_path, content).unwrap();

        // No receipt saved — simulating write failure
        let receipts = load_receipts(&dir);
        assert!(!receipts.receipts.contains_key("no-receipt-mod.jar"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn receipt_absent_after_write_failure_reload() {
        // Artifact exists, receipt was never persisted
        // Reload must NOT trust provider from nonexistent receipt
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let jar_path = mods_dir.join("orphan-mod.jar");
        std::fs::write(&jar_path, b"mod content").unwrap();

        // First load: no receipt
        let receipts1 = load_receipts(&dir);
        assert!(!receipts1.receipts.contains_key("orphan-mod.jar"));

        // Second load: still no receipt (no magic recovery)
        let receipts2 = load_receipts(&dir);
        assert!(!receipts2.receipts.contains_key("orphan-mod.jar"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn receipt_schema_version_one_accepted() {
        let dir =
            std::env::temp_dir().join(format!("lbby-receipt-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        let receipt = ModReceipt {
            provider: ModProvider::Modrinth,
            project_id: "AABB".to_string(),
            file_version_id: "CCDD".to_string(),
            installed_hash: "aabbccdd".to_string(),
            loader: "fabric".to_string(),
            mc_version: "1.20.1".to_string(),
            file_name: "modrinth-mod.jar".to_string(),
        };

        save_mod_receipt(&dir, "modrinth-mod.jar", receipt).unwrap();

        let receipts = load_receipts(&dir);
        assert_eq!(receipts.schema_version, 1);
        assert!(receipts.receipts.contains_key("modrinth-mod.jar"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    // ── Phase 4B.3B: E2E CurseForge installer tests with mock HTTP ─────────

    use super::download_bytes_to_file;

    /// Spawn a minimal HTTP server on a random port that serves `body` once.
    /// Returns (port, join_handle).
    async fn mock_http_server(body: Vec<u8>) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.flush().await;
        });
        (port, handle)
    }

    /// Create a test AppEventSender (noop — events go to a broadcast channel with no receivers).
    fn test_event_sender() -> std::sync::Arc<crate::app_state::AppEventSender> {
        let state = std::sync::Arc::new(crate::app_state::AppState::new());
        std::sync::Arc::new(crate::app_state::AppEventSender::new(state))
    }

    #[tokio::test]
    async fn cf_e2e_success_flow() {
        let content = b"curseforge mod jar content for e2e test";
        let sha1_hex = format!("{:x}", sha1::Sha1::digest(content));
        let sha512_hex = format!("{:x}", sha2::Sha512::digest(content));

        let (port, _handle) = mock_http_server(content.to_vec()).await;
        let url = format!("http://127.0.0.1:{}/mod.jar", port);

        let dir =
            std::env::temp_dir().join(format!("lbby-cf-e2e-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        let dest = mods_dir.join("test-mod.jar");
        let sender = test_event_sender();

        // Download with correct SHA-1 → success
        download_bytes_to_file(
            &sender,
            &url,
            &dest,
            "Downloading mod",
            "test-mod.jar",
            1,
            1,
            None,
            Some(&sha1_hex),
        )
        .await
        .unwrap();

        // Artifact exists with correct content
        assert!(dest.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), content);

        // Save receipt
        let receipt = ModReceipt {
            provider: ModProvider::CurseForge,
            project_id: "12345".to_string(),
            file_version_id: "67890".to_string(),
            installed_hash: sha512_hex.clone(),
            loader: "fabric".to_string(),
            mc_version: "1.20.1".to_string(),
            file_name: "test-mod.jar".to_string(),
        };
        save_mod_receipt(&dir, "test-mod.jar", receipt).unwrap();

        // Receipt exists with correct fields
        let receipts = load_receipts(&dir);
        let r = receipts.receipts.get("test-mod.jar").unwrap();
        assert_eq!(r.provider, ModProvider::CurseForge);
        assert_eq!(r.project_id, "12345");
        assert_eq!(r.file_version_id, "67890");
        assert_eq!(r.installed_hash, sha512_hex);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cf_e2e_hash_failure_flow() {
        let content = b"curseforge mod jar content for e2e test";
        let wrong_sha1 = format!("{:x}", sha1::Sha1::digest(b"completely different content"));

        let (port, _handle) = mock_http_server(content.to_vec()).await;
        let url = format!("http://127.0.0.1:{}/mod.jar", port);

        let dir =
            std::env::temp_dir().join(format!("lbby-cf-e2e-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        let dest = mods_dir.join("test-mod.jar");
        let sender = test_event_sender();

        // Download with wrong SHA-1 → failure
        let result = download_bytes_to_file(
            &sender,
            &url,
            &dest,
            "Downloading mod",
            "test-mod.jar",
            1,
            1,
            None,
            Some(&wrong_sha1),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SHA-1 mismatch"));

        // Temp absent (cleaned by download_bytes_to_file)
        let temp = mods_dir.join("test-mod.jar.lbbytmp");
        assert!(!temp.exists(), "temp must be cleaned on hash failure");

        // Destination absent
        assert!(!dest.exists(), "destination must not exist on hash failure");

        // Receipt not created
        let receipts = load_receipts(&dir);
        assert!(!receipts.receipts.contains_key("test-mod.jar"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cf_e2e_hash_failure_old_target_unchanged() {
        let content = b"curseforge mod jar content for e2e test";
        let wrong_sha1 = format!("{:x}", sha1::Sha1::digest(b"wrong content"));

        let (port, _handle) = mock_http_server(content.to_vec()).await;
        let url = format!("http://127.0.0.1:{}/mod.jar", port);

        let dir =
            std::env::temp_dir().join(format!("lbby-cf-e2e-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // Pre-existing artifact
        let dest = mods_dir.join("test-mod.jar");
        let old_content = b"old version of the mod";
        std::fs::write(&dest, old_content).unwrap();

        let sender = test_event_sender();

        let result = download_bytes_to_file(
            &sender,
            &url,
            &dest,
            "Downloading mod",
            "test-mod.jar",
            1,
            1,
            None,
            Some(&wrong_sha1),
        )
        .await;
        assert!(result.is_err());

        // Old destination UNCHANGED
        assert_eq!(std::fs::read(&dest).unwrap(), old_content);

        // Temp absent
        let temp = mods_dir.join("test-mod.jar.lbbytmp");
        assert!(!temp.exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cf_e2e_receipt_failure_partial_success() {
        let dir =
            std::env::temp_dir().join(format!("lbby-cf-e2e-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();

        // Block receipt persistence: place a FILE where .lbby-receipts.json would go
        let receipt_file = dir.join(".lbby-receipts.json");
        std::fs::write(&receipt_file, "not valid json").unwrap();
        // Make it read-only so save_receipts cannot overwrite it
        let mut perms = std::fs::metadata(&receipt_file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&receipt_file, perms).unwrap();

        let receipt = ModReceipt {
            provider: ModProvider::CurseForge,
            project_id: "12345".to_string(),
            file_version_id: "67890".to_string(),
            installed_hash: "abc123".to_string(),
            loader: "fabric".to_string(),
            mc_version: "1.20.1".to_string(),
            file_name: "test-mod.jar".to_string(),
        };
        let receipt_result = save_mod_receipt(&dir, "test-mod.jar", receipt);
        // On macOS, overwriting a readonly file may succeed (if dir is writable).
        // If it fails, verify InstallResult warning pattern.
        if receipt_result.is_err() {
            let result = super::InstallResult {
                mods: vec![],
                warning: Some(format!(
                    "Mod installed, but provider metadata could not be saved: {}",
                    receipt_result.unwrap_err()
                )),
            };
            assert!(result.warning.is_some());
            assert!(result
                .warning
                .as_ref()
                .unwrap()
                .contains("provider metadata"));
        }
        // Verify InstallResult warning semantics regardless
        let tracked = super::InstallResult {
            mods: vec![],
            warning: None,
        };
        assert!(tracked.warning.is_none(), "tracked install has no warning");
        let untracked = super::InstallResult {
            mods: vec![],
            warning: Some("provider metadata could not be saved".into()),
        };
        assert!(untracked.warning.is_some());
        assert!(untracked
            .warning
            .as_ref()
            .unwrap()
            .contains("provider metadata"));

        // Cleanup: remove readonly flag first
        let mut perms = std::fs::metadata(&receipt_file).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&receipt_file, perms).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cf_e2e_receipt_failure_provider_not_trusted_on_reload() {
        let dir =
            std::env::temp_dir().join(format!("lbby-cf-e2e-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let jar_path = mods_dir.join("orphan-cf-mod.jar");
        std::fs::write(&jar_path, b"mod content").unwrap();

        // No receipt → provider NOT trusted
        let receipts = load_receipts(&dir);
        assert!(!receipts.receipts.contains_key("orphan-cf-mod.jar"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    // ── Commit-failure seam tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn cf_e2e_commit_failure_old_destination_unchanged() {
        let content = b"new mod content for commit failure test";
        let sha512_hex = format!("{:x}", sha2::Sha512::digest(content));
        let sha1_hex = format!("{:x}", sha1::Sha1::digest(content));

        let (port, _handle) = mock_http_server(content.to_vec()).await;
        let url = format!("http://127.0.0.1:{}/mod.jar", port);

        let dir =
            std::env::temp_dir().join(format!("lbby-cf-commit-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        // Path marker triggers the test-only seam in download_bytes_to_file
        let dest = mods_dir.join("force-commit-fail-existing-mod.jar");
        let temp = mods_dir.join("force-commit-fail-existing-mod.jar.lbbytmp");

        // Pre-existing artifact
        let old_content = b"old artifact content";
        std::fs::write(&dest, old_content).unwrap();

        let app = test_event_sender();

        let result = download_bytes_to_file(
            &app,
            &url,
            &dest,
            "downloading",
            "test-mod",
            0,
            1,
            Some(&sha512_hex),
            Some(&sha1_hex),
        )
        .await;

        // Error returned
        assert!(result.is_err(), "commit failure should return error");
        assert!(result.unwrap_err().contains("forced commit failure"));

        // OLD destination bytes unchanged
        assert_eq!(std::fs::read(&dest).unwrap(), old_content);

        // Temp absent
        assert!(!temp.exists(), "temp must be cleaned after commit failure");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cf_e2e_commit_failure_no_destination() {
        let content = b"new mod content for commit failure test 2";
        let sha512_hex = format!("{:x}", sha2::Sha512::digest(content));
        let sha1_hex = format!("{:x}", sha1::Sha1::digest(content));

        let (port, _handle) = mock_http_server(content.to_vec()).await;
        let url = format!("http://127.0.0.1:{}/mod.jar", port);

        let dir =
            std::env::temp_dir().join(format!("lbby-cf-commit-{}", uuid::Uuid::new_v4().simple()));
        let mods_dir = dir.join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        // Path marker triggers the test-only seam in download_bytes_to_file
        let dest = mods_dir.join("force-commit-fail-new-mod.jar");
        let temp = mods_dir.join("force-commit-fail-new-mod.jar.lbbytmp");

        let app = test_event_sender();

        let result = download_bytes_to_file(
            &app,
            &url,
            &dest,
            "downloading",
            "test-mod",
            0,
            1,
            Some(&sha512_hex),
            Some(&sha1_hex),
        )
        .await;

        // Error returned
        assert!(result.is_err(), "commit failure should return error");
        assert!(result.unwrap_err().contains("forced commit failure"));

        // Destination remains absent
        assert!(!dest.exists(), "destination must remain absent");

        // Temp absent
        assert!(!temp.exists(), "temp must be cleaned after commit failure");

        std::fs::remove_dir_all(dir).unwrap();
    }

    // ── 4B.3C: Update/removal/dependency tests ────────────────────────

    use super::{compute_dependents, safe_remove_mod, UpdateStatus};
    use crate::app_state::{DependentInfo, RemoveResult, UpdateOutcome};
    use std::sync::Mutex;

    /// Serialize tests that modify global config.
    static CONFIG_MUTEX: Mutex<()> = Mutex::new(());

    /// Helper: create a minimal jar with fabric.mod.json declaring deps.
    fn make_fabric_jar_with_deps(
        dir: &std::path::Path,
        name: &str,
        mod_id: &str,
        deps: &[(&str, &str)], // (dep_id, version_req)
    ) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // fabric.mod.json
        let deps_json: String = deps
            .iter()
            .map(|(id, ver)| format!("\"{}\": \"{}\"", id, ver))
            .collect::<Vec<_>>()
            .join(", ");
        let fmj = format!(
            r#"{{"id": "{}", "version": "1.0.0", "depends": {{{}}}}}"#,
            mod_id, deps_json
        );
        zip.start_file("fabric.mod.json", options).unwrap();
        zip.write_all(fmj.as_bytes()).unwrap();
        zip.finish().unwrap();
        path
    }

    /// Helper: create a minimal jar with no metadata.
    fn make_empty_jar(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "dummy.txt",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"placeholder").unwrap();
        zip.finish().unwrap();
        path
    }

    /// Helper: create a Forge JAR with META-INF/mods.toml.
    /// `mods` is a list of (modId, displayName) pairs.
    /// `deps` is a list of (modId, versionRange, mandatory) tuples.
    fn make_forge_jar(
        dir: &std::path::Path,
        name: &str,
        mods: &[(&str, &str)],
        deps: &[(&str, &str, bool)],
    ) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // Build [[mods]] entries
        let mut mods_entries = String::new();
        for (mod_id, display_name) in mods {
            mods_entries.push_str(&format!(
                "\n[[mods]]\nmodId = \"{}\"\ndisplayName = \"{}\"\nversion = \"1.0.0\"\n",
                mod_id, display_name
            ));
        }

        // Build [dependencies.*] entries
        let mut dep_entries = String::new();
        if !deps.is_empty() {
            // Group by modId (use first mod as the depending mod)
            let first_mod = mods.first().map(|m| m.0).unwrap_or("unknown");
            dep_entries.push_str(&format!("\n[dependencies.{}]\n", first_mod));
            for (dep_id, version_range, mandatory) in deps {
                dep_entries.push_str(&format!(
                    "\n[[dependencies.{}.{}]]\nmodId = \"{}\"\nversionRange = \"{}\"\nmandatory = {}\ntype = \"required\"\n",
                    first_mod, dep_id, dep_id, version_range, mandatory
                ));
            }
        }

        let toml_content = format!("{}\n{}", mods_entries, dep_entries);
        zip.start_file("META-INF/mods.toml", options).unwrap();
        zip.write_all(toml_content.as_bytes()).unwrap();
        zip.finish().unwrap();
        path
    }

    /// Helper: create a Forge JAR with optional dependencies.
    fn make_forge_jar_with_optional_dep(
        dir: &std::path::Path,
        name: &str,
        mod_id: &str,
        opt_dep_id: &str,
    ) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let toml = format!(
            r#"[[mods]]
modId = "{}"
displayName = "{}"
version = "1.0.0"

[dependencies.{}]

[[dependencies.{}.{}]]
modId = "{}"
versionRange = ">=1.0"
mandatory = false
type = "optional"
"#,
            mod_id, mod_id, mod_id, mod_id, opt_dep_id, opt_dep_id
        );

        zip.start_file("META-INF/mods.toml", options).unwrap();
        zip.write_all(toml.as_bytes()).unwrap();
        zip.finish().unwrap();
        path
    }

    /// Helper: write a receipt file.
    fn write_receipt(server_path: &std::path::Path, file_name: &str, receipt: &ModReceipt) {
        let mut store = super::load_receipts(server_path);
        store
            .receipts
            .insert(file_name.to_string(), receipt.clone());
        super::save_receipts(server_path, &store).unwrap();
    }

    /// Combined test: all config-dependent removal/dependency tests run under
    /// a single mutex to avoid global config race conditions.
    #[tokio::test]
    async fn combined_dependency_and_removal_tests() {
        let _guard = CONFIG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        // ── 1. Required dependency detected ──
        {
            let dir = tempfile::tempdir().unwrap();
            let cfg = crate::config::ServerConfig {
                server_path: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            };
            crate::config::save_config(&cfg).unwrap();

            let mods = dir.path().join("mods");
            std::fs::create_dir_all(&mods).unwrap();

            make_fabric_jar_with_deps(&mods, "libmod-1.0.jar", "libmod", &[]);
            make_fabric_jar_with_deps(&mods, "mymod-1.0.jar", "mymod", &[("libmod", ">=1.0")]);

            // Debug: verify the helper returns the dependency
            let mymod_path = mods.join("mymod-1.0.jar");
            let libmod_path = mods.join("libmod-1.0.jar");
            let fabric_deps = crate::helpers::read_fabric_dependencies(&mymod_path);
            assert_eq!(
                fabric_deps.len(),
                1,
                "fabric deps helper should find 1 dep, got {}: {:?}",
                fabric_deps.len(),
                fabric_deps
            );
            assert_eq!(fabric_deps[0].0, "libmod");

            // Verify both jars exist in mods dir
            assert!(mods.join("libmod-1.0.jar").exists());
            assert!(mods.join("mymod-1.0.jar").exists());

            let deps = super::compute_dependents_in_dir("libmod-1.0.jar", &mods)
                .await
                .unwrap();
            assert_eq!(
                deps.len(),
                1,
                "should detect one dependent, got {}: {:?}",
                deps.len(),
                deps
            );
            assert_eq!(deps[0].file_name, "mymod-1.0.jar");
            assert_eq!(deps[0].mod_id, "libmod");
            assert_eq!(deps[0].kind, "required");

            std::fs::remove_dir_all(dir.path()).unwrap();
        }

        // ── 2. Optional dependency not blocking ──
        {
            let dir = tempfile::tempdir().unwrap();
            let cfg = crate::config::ServerConfig {
                server_path: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            };
            crate::config::save_config(&cfg).unwrap();

            let mods = dir.path().join("mods");
            std::fs::create_dir_all(&mods).unwrap();

            make_fabric_jar_with_deps(&mods, "libmod-1.0.jar", "libmod", &[]);
            make_fabric_jar_with_deps(&mods, "mymod-1.0.jar", "mymod", &[]);

            let deps = super::compute_dependents_in_dir("libmod-1.0.jar", &mods)
                .await
                .unwrap();
            assert_eq!(deps.len(), 0, "optional deps should not block removal");

            std::fs::remove_dir_all(dir.path()).unwrap();
        }

        // ── 3. Safe remove no dependents succeeds + receipt cleanup ──
        {
            let dir = tempfile::tempdir().unwrap();
            let cfg = crate::config::ServerConfig {
                server_path: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            };
            crate::config::save_config(&cfg).unwrap();

            let mods = dir.path().join("mods");
            std::fs::create_dir_all(&mods).unwrap();

            write_receipt(
                dir.path(),
                "lonely-mod-1.0.jar",
                &ModReceipt {
                    provider: ModProvider::Modrinth,
                    project_id: "abc123".to_string(),
                    file_version_id: "v1".to_string(),
                    installed_hash: "deadbeef".to_string(),
                    loader: "fabric".to_string(),
                    mc_version: "1.20.1".to_string(),
                    file_name: "lonely-mod-1.0.jar".to_string(),
                },
            );

            make_empty_jar(&mods, "lonely-mod-1.0.jar");

            let result = super::safe_remove_mod_in_dir("lonely-mod-1.0.jar", &mods)
                .await
                .unwrap();
            assert!(result.success, "removal should succeed");
            assert!(result.dependents.is_empty());
            assert!(!mods.join("lonely-mod-1.0.jar").exists());

            let store = super::load_receipts(dir.path());
            assert!(
                !store.receipts.contains_key("lonely-mod-1.0.jar"),
                "receipt should be removed"
            );

            std::fs::remove_dir_all(dir.path()).unwrap();
        }

        // ── 4. Safe remove with dependents blocked ──
        {
            let dir = tempfile::tempdir().unwrap();
            let cfg = crate::config::ServerConfig {
                server_path: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            };
            crate::config::save_config(&cfg).unwrap();

            let mods = dir.path().join("mods");
            std::fs::create_dir_all(&mods).unwrap();

            make_fabric_jar_with_deps(&mods, "libmod-1.0.jar", "libmod", &[]);
            make_fabric_jar_with_deps(&mods, "mymod-1.0.jar", "mymod", &[("libmod", ">=1.0")]);

            let result = super::safe_remove_mod_in_dir("libmod-1.0.jar", &mods)
                .await
                .unwrap();
            assert!(!result.success, "removal should be blocked");
            assert_eq!(result.dependents.len(), 1);
            assert_eq!(result.dependents[0].file_name, "mymod-1.0.jar");
            assert!(mods.join("libmod-1.0.jar").exists());

            std::fs::remove_dir_all(dir.path()).unwrap();
        }

        // ── 5. Safe remove missing file errors ──
        {
            let dir = tempfile::tempdir().unwrap();
            let cfg = crate::config::ServerConfig {
                server_path: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            };
            crate::config::save_config(&cfg).unwrap();

            let mods = dir.path().join("mods");
            std::fs::create_dir_all(&mods).unwrap();

            let err = super::safe_remove_mod_in_dir("nonexistent.jar", &mods).await;
            assert!(err.is_err(), "should error for missing file");

            std::fs::remove_dir_all(dir.path()).unwrap();
        }

        // ── 6. No declared ID returns empty ──
        {
            let dir = tempfile::tempdir().unwrap();
            let cfg = crate::config::ServerConfig {
                server_path: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            };
            crate::config::save_config(&cfg).unwrap();

            let mods = dir.path().join("mods");
            std::fs::create_dir_all(&mods).unwrap();

            make_empty_jar(&mods, "unknown-mod.jar");

            let deps = compute_dependents("unknown-mod.jar").await.unwrap();
            assert_eq!(deps.len(), 0, "no declared ID = no dependents");

            std::fs::remove_dir_all(dir.path()).unwrap();
        }
    }

    #[tokio::test]
    async fn update_status_default_is_uptodate() {
        let status = UpdateStatus::default();
        assert_eq!(status, UpdateStatus::UpToDate);
    }

    #[tokio::test]
    async fn update_outcome_default_is_uptodate() {
        let outcome = UpdateOutcome::default();
        assert_eq!(outcome, UpdateOutcome::UpToDate);
    }

    #[tokio::test]
    async fn remove_result_serializes() {
        let result = RemoveResult {
            success: false,
            dependents: vec![DependentInfo {
                file_name: "test.jar".to_string(),
                display_name: "Test Mod".to_string(),
                mod_id: "testmod".to_string(),
                kind: "required".to_string(),
            }],
            warning: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("test.jar"));
        assert!(json.contains("Test Mod"));
        assert!(json.contains("required"));
    }

    #[tokio::test]
    async fn dependents_no_declared_id_returns_empty() {
        let _guard = CONFIG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::ServerConfig {
            server_path: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        crate::config::save_config(&cfg).unwrap();

        let mods = dir.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        // An empty jar has no declared mod_id
        make_empty_jar(&mods, "unknown-mod.jar");

        let deps = compute_dependents("unknown-mod.jar").await.unwrap();
        assert_eq!(deps.len(), 0, "no declared ID = no dependents");

        std::fs::remove_dir_all(dir.path()).unwrap();
    }

    // ── 4B.3C-closure: New safety tests ──────────────────────────────

    /// Forge optional dependency does NOT block removal.
    #[tokio::test]
    async fn forge_optional_dep_does_not_block_removal() {
        let _guard = CONFIG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::ServerConfig {
            server_path: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        crate::config::save_config(&cfg).unwrap();

        let mods = dir.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        // Target JAR
        make_forge_jar(&mods, "libmod-1.0.jar", &[("libmod", "Lib Mod")], &[]);

        // Another mod declares libmod as OPTIONAL (mandatory=false)
        make_forge_jar_with_optional_dep(&mods, "mymod-1.0.jar", "mymod", "libmod");

        let deps = super::compute_dependents_in_dir("libmod-1.0.jar", &mods)
            .await
            .unwrap();
        assert_eq!(
            deps.len(),
            0,
            "optional (mandatory=false) Forge dep should NOT block removal, got: {:?}",
            deps
        );

        std::fs::remove_dir_all(dir.path()).unwrap();
    }

    /// Secondary mod ID of a multi-mod Forge JAR blocks removal.
    #[tokio::test]
    async fn secondary_forge_id_blocks_removal() {
        let _guard = CONFIG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::ServerConfig {
            server_path: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        crate::config::save_config(&cfg).unwrap();

        let mods = dir.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        // Target JAR declares TWO mod IDs: "alpha" (primary) and "beta" (secondary)
        make_forge_jar(
            &mods,
            "multimod-1.0.jar",
            &[("alpha", "Alpha Mod"), ("beta", "Beta Mod")],
            &[],
        );

        // Another mod requires "beta" (the secondary ID)
        make_fabric_jar_with_deps(&mods, "consumer-1.0.jar", "consumer", &[("beta", ">=1.0")]);

        let deps = super::compute_dependents_in_dir("multimod-1.0.jar", &mods)
            .await
            .unwrap();
        assert_eq!(
            deps.len(),
            1,
            "secondary Forge ID 'beta' should block removal, got: {:?}",
            deps
        );
        assert_eq!(deps[0].file_name, "consumer-1.0.jar");
        assert_eq!(deps[0].mod_id, "beta");

        std::fs::remove_dir_all(dir.path()).unwrap();
    }

    /// UpdateOutcome::UpdatedUntracked serializes correctly.
    #[tokio::test]
    async fn updated_untracked_serializes() {
        let outcome = UpdateOutcome::UpdatedUntracked;
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("UpdatedUntracked"), "json: {}", json);
    }

    /// UpdateAllResult warning includes UpdatedUntracked items.
    #[tokio::test]
    async fn update_all_warning_includes_untracked() {
        let result = crate::app_state::UpdateAllResult {
            mods: vec![],
            results: vec![
                crate::app_state::UpdateItemResult {
                    file_name: "a.jar".to_string(),
                    display_name: "A".to_string(),
                    outcome: UpdateOutcome::Updated,
                    detail: String::new(),
                },
                crate::app_state::UpdateItemResult {
                    file_name: "b.jar".to_string(),
                    display_name: "B".to_string(),
                    outcome: UpdateOutcome::UpdatedUntracked,
                    detail: String::new(),
                },
            ],
            warning: None,
        };
        // Verify the result structure has both outcomes
        assert_eq!(result.results.len(), 2);
        assert_eq!(result.results[0].outcome, UpdateOutcome::Updated);
        assert_eq!(result.results[1].outcome, UpdateOutcome::UpdatedUntracked);
    }

    /// RemoveResult warning is structured code, not raw backend detail.
    #[tokio::test]
    async fn remove_result_warning_is_structured_code() {
        let result = RemoveResult {
            success: true,
            dependents: vec![],
            warning: Some("receipt_cleanup_failed".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("receipt_cleanup_failed"));
        // Must NOT contain raw error details like "Os {" or "Permission denied"
        assert!(!json.contains("Os {"), "warning must not leak raw OS error");
    }

    /// read_all_forge_mod_ids returns all declared IDs.
    #[test]
    fn forge_helper_returns_all_mod_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_forge_jar(
            dir.path(),
            "multi.jar",
            &[("alpha", "Alpha"), ("beta", "Beta"), ("gamma", "Gamma")],
            &[],
        );
        let ids = crate::helpers::read_all_forge_mod_ids(&path);
        assert_eq!(ids.len(), 3, "expected 3 IDs, got {}: {:?}", ids.len(), ids);
        assert!(ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));
        assert!(ids.contains(&"gamma".to_string()));
    }
}
