//! Modpack discovery: unified search and version resolution for Modrinth and
//! CurseForge. Phase 1 covers discovery only — installation keeps reusing the
//! existing installers in [`crate::mod_services`] (extraction, hashing,
//! client-only quarantine).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const CURSEFORGE_GAME_ID: u32 = 432;
pub const CURSEFORGE_MODPACK_CLASS_ID: u32 = 4471;
/// CurseForge caps pageSize at 50; keep one limit that works for both.
const MAX_PAGE_LIMIT: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModpackProvider {
    Modrinth,
    CurseForge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseChannel {
    Release,
    Beta,
    Alpha,
}

impl ReleaseChannel {
    fn from_modrinth(version_type: &str) -> Self {
        match version_type {
            "release" => ReleaseChannel::Release,
            "alpha" => ReleaseChannel::Alpha,
            _ => ReleaseChannel::Beta,
        }
    }

    /// CurseForge releaseType: 1 = release, 2 = beta, 3 = alpha.
    fn from_curseforge(release_type: u8) -> Self {
        match release_type {
            1 => ReleaseChannel::Release,
            3 => ReleaseChannel::Alpha,
            _ => ReleaseChannel::Beta,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModpackCompatibility {
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModpackSearchHit {
    pub provider: ModpackProvider,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub icon_url: Option<String>,
    pub downloads: u64,
    pub compatibility: ModpackCompatibility,
    /// CurseForge `serverPackFileId`, when the pack ships an official server
    /// pack. Always `None` on Modrinth (the .mrpack covers both sides).
    pub server_pack_file_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModpackSearchResults {
    pub hits: Vec<ModpackSearchHit>,
    pub total_hits: u64,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModpackVersionCandidate {
    pub provider: ModpackProvider,
    pub project_id: String,
    /// Modrinth version id or CurseForge file id.
    pub version_id: String,
    pub name: String,
    pub version_number: String,
    pub channel: ReleaseChannel,
    /// ISO-8601 timestamp; lexicographic order = chronological order.
    pub date_published: String,
    pub download_url: Option<String>,
    pub file_name: String,
    pub file_size: u64,
    pub sha512: Option<String>,
    pub sha1: Option<String>,
    pub compatibility: ModpackCompatibility,
    /// CurseForge `isServerPack`.
    pub is_server_pack: bool,
    /// CurseForge `parentProjectFileId` (set on server-pack files).
    pub parent_project_file_id: Option<String>,
    /// CurseForge: file id of the official server pack that belongs to this
    /// file, if one exists in the same listing.
    pub server_pack_file_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModpackSearchFilter<'a> {
    pub provider: ModpackProvider,
    pub query: &'a str,
    pub minecraft_version: Option<&'a str>,
    pub loader: Option<&'a str>,
    pub offset: u32,
    pub limit: u32,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Search modpacks on the given provider.
/// - Modrinth: Uses official API (no key needed)
/// - CurseForge: Uses website API (no key needed), but may be blocked by Cloudflare
pub async fn search_modpacks(
    filter: &ModpackSearchFilter<'_>,
) -> Result<ModpackSearchResults, String> {
    let limit = filter.limit.clamp(1, MAX_PAGE_LIMIT);
    match filter.provider {
        ModpackProvider::Modrinth => search_modrinth(filter, limit).await,
        ModpackProvider::CurseForge => search_curseforge(filter, limit).await,
    }
}

/// List all version/file candidates of a modpack project.
pub async fn list_modpack_versions(
    provider: ModpackProvider,
    project_id: &str,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
) -> Result<Vec<ModpackVersionCandidate>, String> {
    match provider {
        ModpackProvider::Modrinth => {
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(mc) = nonempty(minecraft_version) {
                q.push(("game_versions", format!("[\"{}\"]", mc)));
            }
            if let Some(l) = nonempty(loader) {
                q.push(("loaders", format!("[\"{}\"]", l.to_lowercase())));
            }
            let resp = crate::mod_services::client()?
                .get(format!(
                    "https://api.modrinth.com/v2/project/{}/version",
                    project_id
                ))
                .query(&q)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!(
                    "Modrinth version lookup failed with HTTP {}",
                    resp.status()
                ));
            }
            let body = resp.text().await.map_err(|e| e.to_string())?;
            parse_modrinth_versions(&body, project_id)
        }
        ModpackProvider::CurseForge => {
            // Use CurseForge website API (no key needed)
            let mut q: Vec<(&str, String)> = vec![("pageSize", "100".to_string())];
            if let Some(mc) = nonempty(minecraft_version) {
                q.push(("gameVersion", mc.to_string()));
            }
            if let Some(l) = nonempty(loader) {
                q.push(("gameVersion", curseforge_loader_name(l)));
            }
            let resp = curseforge_client()?
                .get(format!(
                    "https://www.curseforge.com/api/v1/mods/{}/files",
                    project_id
                ))
                .query(&q)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!(
                    "CurseForge file lookup failed with HTTP {} (may be blocked by Cloudflare)",
                    resp.status()
                ));
            }
            let body = resp.text().await.map_err(|e| e.to_string())?;
            let mut candidates = parse_curseforge_files(&body, project_id)?;
            associate_curseforge_server_packs(&mut candidates);
            Ok(candidates)
        }
    }
}

/// Deterministic resolver.
///
/// 1. An explicit `explicit_version` (version/file id or file name) always
///    wins; error if it is not in `candidates`.
/// 2. Otherwise pick the newest compatible stable candidate; ties break on
///    `version_id`. Non-stable candidates are used only when no stable one
///    matches.
/// 3. For CurseForge, if the chosen candidate has an official server pack in
///    the same listing, that server-pack candidate is returned instead.
pub fn resolve_modpack_version<'a>(
    candidates: &'a [ModpackVersionCandidate],
    explicit_version: Option<&str>,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
) -> Result<Option<&'a ModpackVersionCandidate>, String> {
    if let Some(id) = nonempty(explicit_version) {
        return candidates
            .iter()
            .find(|c| c.version_id == id || c.file_name == id)
            .ok_or_else(|| format!("Selected modpack version {} was not found.", id))
            .map(Some);
    }
    let pool: Vec<&ModpackVersionCandidate> = candidates
        .iter()
        .filter(|c| compatible_with(c, minecraft_version, loader))
        .collect();
    if pool.is_empty() {
        return Ok(None);
    }
    let stable: Vec<&ModpackVersionCandidate> = pool
        .iter()
        .copied()
        .filter(|c| c.channel == ReleaseChannel::Release)
        .collect();
    let mut pool = if stable.is_empty() { pool } else { stable };
    pool.sort_by(|a, b| {
        b.date_published
            .cmp(&a.date_published)
            .then_with(|| b.version_id.cmp(&a.version_id))
    });
    let best = pool[0];
    if let Some(sp_id) = &best.server_pack_file_id {
        if let Some(server_pack) = candidates.iter().find(|c| &c.version_id == sp_id) {
            return Ok(Some(server_pack));
        }
    }
    Ok(Some(best))
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested, no network)
// ---------------------------------------------------------------------------

/// Keep facet values alphanumeric plus `.-_` so user input cannot break the
/// facet JSON.
fn facet_value(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect()
}

/// Modrinth search facets for modpacks: `project_type:modpack` plus optional
/// version and loader groups.
fn modrinth_modpack_facets(minecraft_version: Option<&str>, loader: Option<&str>) -> String {
    let mut groups = vec!["[\"project_type:modpack\"]".to_string()];
    if let Some(mc) = nonempty(minecraft_version) {
        groups.push(format!("[\"versions:{}\"]", facet_value(mc)));
    }
    if let Some(l) = nonempty(loader) {
        groups.push(format!("[\"loaders:{}\"]", facet_value(&l.to_lowercase())));
    }
    format!("[{}]", groups.join(","))
}

/// CurseForge loader token as it appears in `gameVersions` / `gameVersion`.
fn curseforge_loader_name(loader: &str) -> String {
    match loader.trim().to_lowercase().as_str() {
        "forge" => "Forge".to_string(),
        "fabric" => "Fabric".to_string(),
        "neoforge" => "NeoForge".to_string(),
        "quilt" => "Quilt".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            }
        }
    }
}

const KNOWN_LOADERS: &[&str] = &["forge", "fabric", "neoforge", "quilt"];

/// Minecraft-version-looking tokens start with a digit; loader names don't.
fn looks_like_game_version(token: &str) -> bool {
    token.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn split_curseforge_game_versions(tokens: &[String]) -> (Vec<String>, Vec<String>) {
    let mut game_versions = Vec::new();
    let mut loaders = Vec::new();
    for token in tokens {
        if looks_like_game_version(token) {
            if !game_versions.iter().any(|v| v == token) {
                game_versions.push(token.clone());
            }
        } else if KNOWN_LOADERS.contains(&token.to_lowercase().as_str()) {
            let l = token.to_lowercase();
            if !loaders.contains(&l) {
                loaders.push(l);
            }
        }
    }
    (game_versions, loaders)
}

fn compatible_with(
    candidate: &ModpackVersionCandidate,
    minecraft_version: Option<&str>,
    loader: Option<&str>,
) -> bool {
    let mc_ok = minecraft_version.map_or(true, |v| {
        candidate
            .compatibility
            .game_versions
            .iter()
            .any(|g| g.eq_ignore_ascii_case(v))
    });
    let loader_ok = loader.map_or(true, |l| {
        candidate
            .compatibility
            .loaders
            .iter()
            .any(|x| x.eq_ignore_ascii_case(l))
    });
    mc_ok && loader_ok
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Modrinth
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MrSearchResponse {
    #[serde(default)]
    hits: Vec<MrSearchHit>,
    #[serde(default)]
    total_hits: u64,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct MrSearchHit {
    project_id: String,
    slug: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    versions: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    loaders: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct MrVersion {
    id: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    version_number: String,
    #[serde(default)]
    version_type: Option<String>,
    #[serde(default)]
    date_published: Option<String>,
    #[serde(default)]
    game_versions: Vec<String>,
    #[serde(default)]
    loaders: Vec<String>,
    #[serde(default)]
    files: Vec<MrFile>,
}

#[derive(Debug, Deserialize)]
struct MrFile {
    url: String,
    filename: String,
    #[serde(default)]
    primary: Option<bool>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    hashes: Option<HashMap<String, String>>,
}

async fn search_modrinth(
    filter: &ModpackSearchFilter<'_>,
    limit: u32,
) -> Result<ModpackSearchResults, String> {
    let facets = modrinth_modpack_facets(filter.minecraft_version, filter.loader);
    let resp = crate::mod_services::client()?
        .get("https://api.modrinth.com/v2/search")
        .query(&[
            ("query", filter.query.trim().to_string()),
            ("facets", facets),
            ("offset", filter.offset.to_string()),
            ("limit", limit.to_string()),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!(
            "Modrinth search failed with HTTP {}",
            resp.status()
        ));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    parse_modrinth_search(&body, filter.offset, limit)
}

fn parse_modrinth_search(
    body: &str,
    fallback_offset: u32,
    fallback_limit: u32,
) -> Result<ModpackSearchResults, String> {
    let resp: MrSearchResponse = serde_json::from_str(body)
        .map_err(|e| format!("Invalid Modrinth search response: {}", e))?;
    Ok(ModpackSearchResults {
        hits: resp
            .hits
            .into_iter()
            .map(|hit| {
                let loaders = hit
                    .loaders
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.to_lowercase())
                    .chain(hit.categories.iter().filter_map(|c| {
                        let lower = c.to_lowercase();
                        KNOWN_LOADERS.contains(&lower.as_str()).then_some(lower)
                    }))
                    .collect::<Vec<_>>();
                let mut loaders = loaders;
                loaders.sort();
                loaders.dedup();
                ModpackSearchHit {
                    provider: ModpackProvider::Modrinth,
                    project_id: hit.project_id,
                    slug: hit.slug,
                    title: hit.title,
                    description: hit.description.unwrap_or_default(),
                    icon_url: hit.icon_url,
                    downloads: hit.downloads.unwrap_or(0),
                    compatibility: ModpackCompatibility {
                        game_versions: hit.versions,
                        loaders,
                    },
                    server_pack_file_id: None,
                }
            })
            .collect(),
        total_hits: resp.total_hits,
        offset: resp.offset.unwrap_or(fallback_offset),
        limit: resp.limit.unwrap_or(fallback_limit),
    })
}

fn parse_modrinth_versions(
    body: &str,
    project_id: &str,
) -> Result<Vec<ModpackVersionCandidate>, String> {
    let versions: Vec<MrVersion> = serde_json::from_str(body)
        .map_err(|e| format!("Invalid Modrinth version response: {}", e))?;
    let mut candidates = Vec::new();
    for version in versions {
        let Some(file) = pick_mrpack_file(&version.files) else {
            continue;
        };
        candidates.push(ModpackVersionCandidate {
            provider: ModpackProvider::Modrinth,
            project_id: version
                .project_id
                .clone()
                .unwrap_or_else(|| project_id.to_string()),
            version_id: version.id,
            name: version
                .name
                .clone()
                .unwrap_or_else(|| version.version_number.clone()),
            version_number: version.version_number,
            channel: ReleaseChannel::from_modrinth(
                version.version_type.as_deref().unwrap_or("release"),
            ),
            date_published: version.date_published.unwrap_or_default(),
            download_url: Some(file.url.clone()),
            file_name: file.filename.clone(),
            file_size: file.size.unwrap_or(0),
            sha512: file.hashes.as_ref().and_then(|h| h.get("sha512").cloned()),
            sha1: file.hashes.as_ref().and_then(|h| h.get("sha1").cloned()),
            compatibility: ModpackCompatibility {
                game_versions: version.game_versions,
                loaders: version.loaders,
            },
            is_server_pack: false,
            parent_project_file_id: None,
            server_pack_file_id: None,
        });
    }
    Ok(candidates)
}

/// Primary file first, then the first `.mrpack`, then whatever exists.
fn pick_mrpack_file(files: &[MrFile]) -> Option<&MrFile> {
    files
        .iter()
        .find(|f| f.primary.unwrap_or(false))
        .or_else(|| files.iter().find(|f| f.filename.ends_with(".mrpack")))
        .or_else(|| files.first())
}

// ---------------------------------------------------------------------------
// CurseForge
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CfSearchResponse {
    #[serde(default)]
    data: Vec<CfMod>,
    #[serde(default)]
    pagination: Option<CfPagination>,
}

#[derive(Debug, Deserialize)]
struct CfPagination {
    #[serde(default)]
    index: u32,
    #[serde(default, rename = "pageSize")]
    page_size: u32,
    #[serde(default, rename = "totalCount")]
    total_count: u64,
}

#[derive(Debug, Deserialize)]
struct CfMod {
    id: u64,
    #[serde(default)]
    slug: Option<String>,
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    logo: Option<CfLogo>,
    #[serde(default, rename = "downloadCount")]
    download_count: Option<u64>,
    #[serde(default, rename = "serverPackFileId")]
    server_pack_file_id: Option<u64>,
    #[serde(default, rename = "latestFilesIndexes")]
    latest_files_indexes: Option<Vec<CfFileIndex>>,
}

#[derive(Debug, Deserialize)]
struct CfLogo {
    #[serde(default, rename = "thumbnailUrl")]
    thumbnail_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CfFileIndex {
    #[serde(default, rename = "gameVersion")]
    game_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CfFilesResponse {
    #[serde(default)]
    data: Vec<CfFile>,
}

#[derive(Debug, Deserialize)]
struct CfFile {
    id: u64,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "fileName")]
    file_name: String,
    #[serde(default, rename = "downloadUrl")]
    download_url: Option<String>,
    #[serde(default, rename = "fileDate")]
    file_date: Option<String>,
    #[serde(default, rename = "releaseType")]
    release_type: u8,
    #[serde(default, rename = "gameVersions")]
    game_versions: Vec<String>,
    #[serde(default, rename = "fileSize")]
    file_size: Option<u64>,
    #[serde(default)]
    hashes: Option<Vec<CfHash>>,
    #[serde(default, rename = "isServerPack")]
    is_server_pack: Option<bool>,
    #[serde(default, rename = "parentProjectFileId")]
    parent_project_file_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CfHash {
    /// 1 = SHA-1, 2 = MD5 (CurseForge does not publish SHA-512).
    algo: u8,
    value: String,
}

fn curseforge_client() -> Result<reqwest::Client, String> {
    // CurseForge website API doesn't require an API key
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
        .build()
        .map_err(|e| e.to_string())
}

async fn search_curseforge(
    filter: &ModpackSearchFilter<'_>,
    limit: u32,
) -> Result<ModpackSearchResults, String> {
    // Use CurseForge website API (no key needed)
    // Note: May be blocked by Cloudflare in some regions
    let mut q: Vec<(&str, String)> = vec![
        ("gameId", CURSEFORGE_GAME_ID.to_string()),
        ("classId", CURSEFORGE_MODPACK_CLASS_ID.to_string()),
        ("index", filter.offset.to_string()),
        ("pageSize", limit.to_string()),
        ("sortField", "2".to_string()),
        ("sortOrder", "desc".to_string()),
    ];
    if let Some(query) = nonempty(Some(filter.query)) {
        q.push(("searchFilter", query.to_string()));
    }
    if let Some(mc) = nonempty(filter.minecraft_version) {
        q.push(("gameVersion", mc.to_string()));
    }
    if let Some(l) = nonempty(filter.loader) {
        q.push(("gameVersion", curseforge_loader_name(l)));
    }
    let resp = curseforge_client()?
        .get("https://www.curseforge.com/api/v1/mods/search")
        .query(&q)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!(
            "CurseForge search failed with HTTP {} (may be blocked by Cloudflare)",
            resp.status()
        ));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    parse_curseforge_search(&body, filter.offset, limit)
}

fn parse_curseforge_search(
    body: &str,
    fallback_offset: u32,
    fallback_limit: u32,
) -> Result<ModpackSearchResults, String> {
    let resp: CfSearchResponse = serde_json::from_str(body)
        .map_err(|e| format!("Invalid CurseForge search response: {}", e))?;
    let hits = resp
        .data
        .into_iter()
        .map(|m| {
            let tokens: Vec<String> = m
                .latest_files_indexes
                .unwrap_or_default()
                .into_iter()
                .filter_map(|i| i.game_version)
                .collect();
            let (game_versions, loaders) = split_curseforge_game_versions(&tokens);
            ModpackSearchHit {
                provider: ModpackProvider::CurseForge,
                project_id: m.id.to_string(),
                slug: m.slug.unwrap_or_else(|| m.id.to_string()),
                title: m.name,
                description: m.summary.unwrap_or_default(),
                icon_url: m.logo.and_then(|l| l.thumbnail_url.or(l.url)),
                downloads: m.download_count.unwrap_or(0),
                compatibility: ModpackCompatibility {
                    game_versions,
                    loaders,
                },
                server_pack_file_id: m.server_pack_file_id.map(|id| id.to_string()),
            }
        })
        .collect();
    let pagination = resp.pagination;
    Ok(ModpackSearchResults {
        hits,
        total_hits: pagination.as_ref().map(|p| p.total_count).unwrap_or(0),
        offset: pagination
            .as_ref()
            .map(|p| p.index)
            .unwrap_or(fallback_offset),
        limit: pagination
            .as_ref()
            .map(|p| p.page_size)
            .unwrap_or(fallback_limit),
    })
}

fn parse_curseforge_files(
    body: &str,
    project_id: &str,
) -> Result<Vec<ModpackVersionCandidate>, String> {
    let resp: CfFilesResponse = serde_json::from_str(body)
        .map_err(|e| format!("Invalid CurseForge files response: {}", e))?;
    Ok(resp
        .data
        .into_iter()
        .map(|f| {
            let (game_versions, loaders) = split_curseforge_game_versions(&f.game_versions);
            let sha1 = f
                .hashes
                .as_ref()
                .and_then(|hs| hs.iter().find(|h| h.algo == 1).map(|h| h.value.clone()));
            ModpackVersionCandidate {
                provider: ModpackProvider::CurseForge,
                project_id: project_id.to_string(),
                version_id: f.id.to_string(),
                name: f.display_name.unwrap_or_else(|| f.file_name.clone()),
                version_number: f.file_name.clone(),
                channel: ReleaseChannel::from_curseforge(f.release_type),
                date_published: f.file_date.unwrap_or_default(),
                download_url: f.download_url,
                file_name: f.file_name,
                file_size: f.file_size.unwrap_or(0),
                sha512: None,
                sha1,
                compatibility: ModpackCompatibility {
                    game_versions,
                    loaders,
                },
                is_server_pack: f.is_server_pack.unwrap_or(false),
                parent_project_file_id: f.parent_project_file_id.map(|id| id.to_string()),
                server_pack_file_id: None,
            }
        })
        .collect())
}

/// Link server-pack files to their parent: a file with `isServerPack` points
/// back at the client pack via `parentProjectFileId`; the parent candidate
/// gets `server_pack_file_id` set to the server pack's file id.
fn associate_curseforge_server_packs(candidates: &mut [ModpackVersionCandidate]) {
    let links: Vec<(String, String)> = candidates
        .iter()
        .filter(|c| c.is_server_pack)
        .filter_map(|c| {
            c.parent_project_file_id
                .clone()
                .map(|parent| (parent, c.version_id.clone()))
        })
        .collect();
    for (parent_id, server_pack_id) in links {
        if let Some(parent) = candidates
            .iter_mut()
            .find(|c| c.version_id == parent_id && !c.is_server_pack)
        {
            parent.server_pack_file_id = Some(server_pack_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — fixtures only, no live network
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        provider: ModpackProvider,
        version_id: &str,
        date: &str,
        channel: ReleaseChannel,
        mc: &[&str],
        loaders: &[&str],
    ) -> ModpackVersionCandidate {
        ModpackVersionCandidate {
            provider,
            project_id: "1".to_string(),
            version_id: version_id.to_string(),
            name: version_id.to_string(),
            version_number: version_id.to_string(),
            channel,
            date_published: date.to_string(),
            download_url: None,
            file_name: format!("{}.zip", version_id),
            file_size: 0,
            sha512: None,
            sha1: None,
            compatibility: ModpackCompatibility {
                game_versions: mc.iter().map(|s| s.to_string()).collect(),
                loaders: loaders.iter().map(|s| s.to_string()).collect(),
            },
            is_server_pack: false,
            parent_project_file_id: None,
            server_pack_file_id: None,
        }
    }

    #[test]
    fn modrinth_facets_with_and_without_filters() {
        assert_eq!(
            modrinth_modpack_facets(None, None),
            "[[\"project_type:modpack\"]]"
        );
        assert_eq!(
            modrinth_modpack_facets(Some("1.20.1"), Some("Fabric")),
            "[[\"project_type:modpack\"],[\"versions:1.20.1\"],[\"loaders:fabric\"]]"
        );
    }

    #[test]
    fn modrinth_facets_sanitize_input() {
        assert_eq!(
            modrinth_modpack_facets(Some("1.20.1\"]]"), Some("forge\"],[\"evil")),
            // Structural chars are stripped; leftovers merge into one safe literal.
            "[[\"project_type:modpack\"],[\"versions:1.20.1\"],[\"loaders:forgeevil\"]]"
        );
    }

    #[test]
    fn parse_modrinth_search_maps_fields() {
        let body = r#"{
            "hits": [{
                "project_id": "AABBCC",
                "slug": "all-the-mods",
                "title": "All The Mods",
                "description": "A kitchen sink pack",
                "icon_url": "https://cdn.example/icon.png",
                "downloads": 12345,
                "versions": ["1.20.1", "1.19.2"],
                "categories": ["forge", "adventure"],
                "loaders": ["Forge"]
            }],
            "total_hits": 42,
            "offset": 10,
            "limit": 20
        }"#;
        let res = parse_modrinth_search(body, 0, 50).unwrap();
        assert_eq!(res.total_hits, 42);
        assert_eq!(res.offset, 10);
        assert_eq!(res.limit, 20);
        let hit = &res.hits[0];
        assert_eq!(hit.provider, ModpackProvider::Modrinth);
        assert_eq!(hit.project_id, "AABBCC");
        assert_eq!(hit.downloads, 12345);
        assert_eq!(hit.compatibility.game_versions, vec!["1.20.1", "1.19.2"]);
        assert_eq!(hit.compatibility.loaders, vec!["forge"]);
        assert_eq!(hit.server_pack_file_id, None);
    }

    #[test]
    fn parse_curseforge_search_maps_fields_and_server_pack() {
        let body = r#"{
            "data": [{
                "id": 999,
                "slug": "some-pack",
                "name": "Some Pack",
                "summary": "A pack",
                "logo": { "thumbnailUrl": "https://cdn.example/thumb.png" },
                "downloadCount": 777,
                "serverPackFileId": 5555,
                "latestFilesIndexes": [
                    { "gameVersion": "1.20.1" },
                    { "gameVersion": "NeoForge" }
                ]
            }],
            "pagination": { "index": 5, "pageSize": 10, "totalCount": 300 }
        }"#;
        let res = parse_curseforge_search(body, 0, 50).unwrap();
        assert_eq!(res.total_hits, 300);
        assert_eq!(res.offset, 5);
        assert_eq!(res.limit, 10);
        let hit = &res.hits[0];
        assert_eq!(hit.provider, ModpackProvider::CurseForge);
        assert_eq!(hit.project_id, "999");
        assert_eq!(hit.server_pack_file_id, Some("5555".to_string()));
        assert_eq!(hit.compatibility.game_versions, vec!["1.20.1"]);
        assert_eq!(hit.compatibility.loaders, vec!["neoforge"]);
    }

    #[test]
    fn curseforge_associates_server_pack_files() {
        let body = r#"{
            "data": [
                {
                    "id": 100,
                    "fileName": "pack-client.zip",
                    "fileDate": "2026-08-01T00:00:00Z",
                    "releaseType": 1,
                    "gameVersions": ["1.20.1", "Forge"]
                },
                {
                    "id": 101,
                    "fileName": "pack-server.zip",
                    "fileDate": "2026-08-01T00:00:00Z",
                    "releaseType": 1,
                    "gameVersions": ["1.20.1", "Forge"],
                    "isServerPack": true,
                    "parentProjectFileId": 100
                }
            ]
        }"#;
        let mut candidates = parse_curseforge_files(body, "999").unwrap();
        associate_curseforge_server_packs(&mut candidates);

        let client_pack = candidates.iter().find(|c| c.version_id == "100").unwrap();
        assert_eq!(client_pack.server_pack_file_id, Some("101".to_string()));
        assert!(!client_pack.is_server_pack);
        assert_eq!(
            client_pack.compatibility.game_versions,
            vec!["1.20.1".to_string()]
        );
        assert_eq!(client_pack.compatibility.loaders, vec!["forge".to_string()]);

        let server_pack = candidates.iter().find(|c| c.version_id == "101").unwrap();
        assert!(server_pack.is_server_pack);
        assert_eq!(server_pack.parent_project_file_id, Some("100".to_string()));
        assert_eq!(server_pack.server_pack_file_id, None);
    }

    #[test]
    fn resolve_explicit_version_wins_over_newer() {
        let candidates = vec![
            candidate(
                ModpackProvider::Modrinth,
                "old",
                "2026-01-01T00:00:00Z",
                ReleaseChannel::Release,
                &["1.20.1"],
                &["forge"],
            ),
            candidate(
                ModpackProvider::Modrinth,
                "new",
                "2026-06-01T00:00:00Z",
                ReleaseChannel::Release,
                &["1.20.1"],
                &["forge"],
            ),
        ];
        let picked = resolve_modpack_version(&candidates, Some("old"), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(picked.version_id, "old");
    }

    #[test]
    fn resolve_explicit_version_missing_is_error() {
        let candidates = vec![candidate(
            ModpackProvider::Modrinth,
            "a",
            "2026-01-01T00:00:00Z",
            ReleaseChannel::Release,
            &[],
            &[],
        )];
        let err = resolve_modpack_version(&candidates, Some("nope"), None, None).unwrap_err();
        assert!(err.contains("nope"));
    }

    #[test]
    fn resolve_picks_newest_compatible_stable_with_deterministic_tie_break() {
        let candidates = vec![
            candidate(
                ModpackProvider::Modrinth,
                "tie-b",
                "2026-06-01T00:00:00Z",
                ReleaseChannel::Release,
                &["1.20.1"],
                &["forge"],
            ),
            candidate(
                ModpackProvider::Modrinth,
                "tie-a",
                "2026-06-01T00:00:00Z",
                ReleaseChannel::Release,
                &["1.20.1"],
                &["forge"],
            ),
            candidate(
                ModpackProvider::Modrinth,
                "newest-beta",
                "2026-07-01T00:00:00Z",
                ReleaseChannel::Beta,
                &["1.20.1"],
                &["forge"],
            ),
            candidate(
                ModpackProvider::Modrinth,
                "wrong-mc",
                "2026-08-01T00:00:00Z",
                ReleaseChannel::Release,
                &["1.19.2"],
                &["forge"],
            ),
            candidate(
                ModpackProvider::Modrinth,
                "wrong-loader",
                "2026-08-01T00:00:00Z",
                ReleaseChannel::Release,
                &["1.20.1"],
                &["fabric"],
            ),
        ];
        let picked = resolve_modpack_version(&candidates, None, Some("1.20.1"), Some("forge"))
            .unwrap()
            .unwrap();
        assert_eq!(picked.version_id, "tie-b");
        assert_eq!(picked.channel, ReleaseChannel::Release);
    }

    #[test]
    fn resolve_falls_back_to_beta_when_no_stable() {
        let candidates = vec![candidate(
            ModpackProvider::Modrinth,
            "only-beta",
            "2026-01-01T00:00:00Z",
            ReleaseChannel::Beta,
            &["1.20.1"],
            &["fabric"],
        )];
        let picked = resolve_modpack_version(&candidates, None, Some("1.20.1"), Some("fabric"))
            .unwrap()
            .unwrap();
        assert_eq!(picked.version_id, "only-beta");
    }

    #[test]
    fn resolve_prefers_official_curseforge_server_pack() {
        let mut client_pack = candidate(
            ModpackProvider::CurseForge,
            "100",
            "2026-06-01T00:00:00Z",
            ReleaseChannel::Release,
            &["1.20.1"],
            &["forge"],
        );
        client_pack.server_pack_file_id = Some("101".to_string());
        let mut server_pack = candidate(
            ModpackProvider::CurseForge,
            "101",
            "2026-06-01T00:00:00Z",
            ReleaseChannel::Release,
            &["1.20.1"],
            &["forge"],
        );
        server_pack.is_server_pack = true;
        server_pack.parent_project_file_id = Some("100".to_string());
        let candidates = vec![client_pack, server_pack];
        let picked = resolve_modpack_version(&candidates, None, Some("1.20.1"), Some("forge"))
            .unwrap()
            .unwrap();
        assert_eq!(picked.version_id, "101");
        assert!(picked.is_server_pack);
        // MC/loader metadata preserved on the server pack candidate.
        assert_eq!(
            picked.compatibility.game_versions,
            vec!["1.20.1".to_string()]
        );
        assert_eq!(picked.compatibility.loaders, vec!["forge".to_string()]);
    }

    #[test]
    fn resolve_returns_none_when_nothing_compatible() {
        let candidates = vec![candidate(
            ModpackProvider::Modrinth,
            "a",
            "2026-01-01T00:00:00Z",
            ReleaseChannel::Release,
            &["1.19.2"],
            &["forge"],
        )];
        assert!(
            resolve_modpack_version(&candidates, None, Some("1.20.1"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn modrinth_picks_primary_mrpack_file() {
        let body = r#"[{
            "id": "v1",
            "project_id": "p1",
            "version_number": "1.0.0",
            "version_type": "release",
            "date_published": "2026-05-01T00:00:00Z",
            "game_versions": ["1.20.1"],
            "loaders": ["fabric"],
            "files": [
                { "url": "https://cdn.example/extra.txt", "filename": "extra.txt", "primary": false },
                { "url": "https://cdn.example/pack.mrpack", "filename": "pack.mrpack", "primary": true, "hashes": { "sha512": "abc" } }
            ]
        }]"#;
        let candidates = parse_modrinth_versions(body, "p1").unwrap();
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.file_name, "pack.mrpack");
        assert_eq!(c.sha512, Some("abc".to_string()));
        assert_eq!(c.compatibility.loaders, vec!["fabric".to_string()]);
    }

    #[test]
    fn curseforge_loader_names_are_canonical() {
        assert_eq!(curseforge_loader_name("fabric"), "Fabric");
        assert_eq!(curseforge_loader_name("NEOFORGE"), "NeoForge");
        assert_eq!(curseforge_loader_name("quilt"), "Quilt");
    }
}
