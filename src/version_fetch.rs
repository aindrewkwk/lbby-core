use std::collections::HashMap;
use serde::{Deserialize, Serialize};

// ── Shared types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct McVersion {
    pub id: String,
    pub release_time: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LoaderVersion {
    pub version: String,
    pub label: String,
}

// ── Internal deserialization structs ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct VersionManifest {
    versions: Vec<VersionEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct VersionEntry {
    id: String,
    #[serde(rename = "type")]
    version_type: String,
    url: String,
    #[serde(rename = "releaseTime")]
    release_time: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ForgePromotions {
    promos: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PaperProject {
    versions: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PaperVersion {
    builds: Vec<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FabricLoader {
    loader: FabricLoaderInfo,
}

#[derive(Debug, Serialize, Deserialize)]
struct FabricLoaderInfo {
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct NeoForgeVersions {
    versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PurpurVersions {
    versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PurpurBuilds {
    builds: PurpurBuildInfo,
}

#[derive(Debug, Deserialize)]
struct PurpurBuildInfo {
    latest: String,
}

#[derive(Debug, Deserialize)]
struct SpongeArtifactVersions {
    artifacts: HashMap<String, SpongeArtifactInfo>,
}

#[derive(Debug, Deserialize)]
struct SpongeArtifactInfo {
    #[serde(rename = "tagValues")]
    tag_values: HashMap<String, String>,
}

// ── Version-fetching functions ───────────────────────────────────────────────

pub async fn fetch_mc_versions() -> Result<Vec<McVersion>, String> {
    let client = reqwest::Client::new();
    let manifest: VersionManifest = client
        .get("https://launchermeta.mojang.com/mc/game/version_manifest_v2.json")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    Ok(manifest.versions.into_iter()
        .filter(|v| v.version_type == "release")
        .take(40)
        .map(|v| McVersion { id: v.id, release_time: v.release_time })
        .collect())
}

pub async fn fetch_paper_versions() -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let proj: PaperProject = client.get("https://api.papermc.io/v2/projects/paper")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let mut versions = proj.versions;
    versions.reverse();
    Ok(versions.into_iter().take(40).collect())
}

pub async fn fetch_paper_builds(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let v: PaperVersion = client
        .get(format!("https://api.papermc.io/v2/projects/paper/versions/{}", mc_version))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let latest = v.builds.last().copied().unwrap_or(0);
    Ok(vec![LoaderVersion {
        version: latest.to_string(),
        label: format!("{} (latest)", latest),
    }])
}

pub async fn fetch_forge_versions(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let promos: ForgePromotions = client
        .get("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")
        .header("User-Agent", "MCHost/0.1")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let mut versions = vec![];
    if let Some(v) = promos.promos.get(&format!("{}-recommended", mc_version)) {
        versions.push(LoaderVersion {
            version: v.clone(),
            label: format!("{} (recommended)", v),
        });
    }
    if let Some(v) = promos.promos.get(&format!("{}-latest", mc_version)) {
        if !versions.iter().any(|x| &x.version == v) {
            versions.push(LoaderVersion {
                version: v.clone(),
                label: format!("{} (latest)", v),
            });
        }
    }
    Ok(versions)
}

pub async fn fetch_fabric_versions(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let loaders: Vec<FabricLoader> = client
        .get(format!("https://meta.fabricmc.net/v2/versions/loader/{}", mc_version))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let mut out = vec![];
    if let Some(latest) = loaders.first() {
        out.push(LoaderVersion {
            version: latest.loader.version.clone(),
            label: format!("{} (latest)", latest.loader.version),
        });
    }
    Ok(out)
}

pub async fn fetch_neoforge_versions(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    // NeoForge versions look like "20.4.244" → MC 1.20.4. We want the prefix matching mc_version.
    let client = reqwest::Client::new();
    let v: NeoForgeVersions = client
        .get("https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;

    let mc_prefix = mc_version.strip_prefix("1.").unwrap_or(&mc_version);
    let matches: Vec<String> = v.versions.iter()
        .filter(|x| x.starts_with(&format!("{}.", mc_prefix)))
        .cloned().collect();

    let mut out = vec![];
    if let Some(latest) = matches.last() {
        out.push(LoaderVersion {
            version: latest.clone(),
            label: format!("{} (latest)", latest),
        });
    }
    Ok(out)
}

// ── Folia versions (same API as Paper, different project) ────────────────────

pub async fn fetch_folia_versions() -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let proj: PaperProject = client.get("https://api.papermc.io/v2/projects/folia")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let mut versions = proj.versions;
    versions.reverse();
    Ok(versions.into_iter().take(40).collect())
}

pub async fn fetch_folia_builds(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let v: PaperVersion = client
        .get(format!("https://api.papermc.io/v2/projects/folia/versions/{}", mc_version))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let latest = v.builds.last().copied().unwrap_or(0);
    Ok(vec![LoaderVersion {
        version: latest.to_string(),
        label: format!("{} (latest)", latest),
    }])
}

// ── Purpur versions ──────────────────────────────────────────────────────────

pub async fn fetch_purpur_versions() -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let v: PurpurVersions = client.get("https://api.purpurmc.org/v2/purpur")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let mut versions = v.versions;
    versions.reverse();
    Ok(versions.into_iter().take(40).collect())
}

pub async fn fetch_purpur_builds(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let v: PurpurBuilds = client
        .get(format!("https://api.purpurmc.org/v2/purpur/{}", mc_version))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    Ok(vec![LoaderVersion {
        version: v.builds.latest.clone(),
        label: format!("{} (latest)", v.builds.latest),
    }])
}

// ── Sponge versions ──────────────────────────────────────────────────────────

pub async fn fetch_sponge_vanilla_versions(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://dl-api.spongepowered.org/v2/groups/org.spongepowered/artifacts/spongevanilla/versions?tags=minecraft:{}&limit=5",
        mc_version
    );
    let v: SpongeArtifactVersions = client.get(&url)
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let mut out: Vec<LoaderVersion> = v.artifacts.into_keys().map(|ver| LoaderVersion {
            version: ver.clone(),
            label: ver,
        })
        .collect();
    out.reverse();
    Ok(out)
}

pub async fn fetch_sponge_forge_versions(mc_version: String) -> Result<Vec<LoaderVersion>, String> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://dl-api.spongepowered.org/v2/groups/org.spongepowered/artifacts/spongeforge/versions?tags=minecraft:{}&limit=5",
        mc_version
    );
    let v: SpongeArtifactVersions = client.get(&url)
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let mut out: Vec<LoaderVersion> = v.artifacts.into_iter()
        .map(|(ver, info)| {
            let forge_ver = info.tag_values.get("forge").cloned().unwrap_or_default();
            LoaderVersion {
                version: ver.clone(),
                label: format!("{} (Forge {})", ver, forge_ver),
            }
        })
        .collect();
    out.reverse();
    Ok(out)
}
