use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct ModrinthHashVersion {
    project_id: String,
}

#[derive(Debug, Deserialize)]
struct ModrinthProjectSide {
    id: String,
    server_side: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ModSideScanCache {
    version: u8,
    files: HashMap<String, ModSideScanCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModSideScanCacheEntry {
    bytes: u64,
    modified_secs: u64,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Lbby/0.1.0 (Minecraft server hosting app)")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

fn jar_get_dependencies(path: &Path) -> Option<Vec<String>> {
    let Ok(file) = std::fs::File::open(path) else {
        return None;
    };
    let Ok(mut jar) = zip::ZipArchive::new(file) else {
        return None;
    };

    // Check Fabric fabric.mod.json
    if let Ok(mut entry) = jar.by_name("fabric.mod.json") {
        let mut contents = String::new();
        if entry.read_to_string(&mut contents).is_ok() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(deps) = value.get("depends").and_then(|d| d.as_object()) {
                    let dep_names: Vec<String> = deps.keys().cloned().collect();
                    return Some(dep_names);
                }
            }
        }
    }

    // Check Forge mods.toml
    for metadata_path in ["META-INF/mods.toml", "META-INF/neoforge.mods.toml"] {
        if let Ok(mut entry) = jar.by_name(metadata_path) {
            let mut contents = String::new();
            if entry.read_to_string(&mut contents).is_ok() {
                if let Ok(value) = contents.parse::<toml::Value>() {
                    if let Some(deps) = value.get("dependencies").and_then(|d| d.as_table()) {
                        let dep_names: Vec<String> = deps.keys().cloned().collect();
                        return Some(dep_names);
                    }
                }
            }
        }
    }

    None
}

fn jar_get_mod_id(path: &Path) -> Option<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return None;
    };
    let Ok(mut jar) = zip::ZipArchive::new(file) else {
        return None;
    };

    // Check Fabric fabric.mod.json
    if let Ok(mut entry) = jar.by_name("fabric.mod.json") {
        let mut contents = String::new();
        if entry.read_to_string(&mut contents).is_ok() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                    return Some(id.to_string());
                }
            }
        }
    }

    // Check Forge mods.toml
    for metadata_path in ["META-INF/mods.toml", "META-INF/neoforge.mods.toml"] {
        if let Ok(mut entry) = jar.by_name(metadata_path) {
            let mut contents = String::new();
            if entry.read_to_string(&mut contents).is_ok() {
                if let Ok(value) = contents.parse::<toml::Value>() {
                    if let Some(mod_id) = value.get("mods").and_then(|m| m.as_array()).and_then(|arr| arr.first()).and_then(|m| m.get("modId")).and_then(|v| v.as_str()) {
                        return Some(mod_id.to_string());
                    }
                }
            }
        }
    }

    None
}

pub fn jar_declares_client_only(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut jar) = zip::ZipArchive::new(file) else {
        return false;
    };

    for metadata_path in ["META-INF/mods.toml", "META-INF/neoforge.mods.toml"] {
        if let Ok(mut entry) = jar.by_name(metadata_path) {
            let mut contents = String::new();
            if entry.read_to_string(&mut contents).is_ok()
                && contents
                    .parse::<toml::Value>()
                    .ok()
                    .and_then(|value| value.get("clientSideOnly").and_then(toml::Value::as_bool))
                    == Some(true)
            {
                return true;
            }
        }
    }

    for metadata_path in ["fabric.mod.json", "quilt.mod.json"] {
        if let Ok(mut entry) = jar.by_name(metadata_path) {
            let mut contents = String::new();
            if entry.read_to_string(&mut contents).is_ok() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) {
                    // ONLY environment: "client" means client-only
                    // environment: * or environment: server means it works on server
                    let environment = value
                        .get("environment")
                        .or_else(|| value.pointer("/quilt_loader/metadata/environment"))
                        .or_else(|| value.pointer("/quilt_loader/environment"))
                        .and_then(serde_json::Value::as_str);
                    if environment == Some("client") {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn sha512_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha512::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn modrinth_client_only_hashes(
    http: &reqwest::Client,
    hashes: &[String],
) -> (HashSet<String>, bool) {
    let mut versions = HashMap::<String, ModrinthHashVersion>::new();
    let mut complete = true;
    for chunk in hashes.chunks(100) {
        let response = http
            .post("https://api.modrinth.com/v2/version_files")
            .timeout(std::time::Duration::from_secs(15))
            .json(&serde_json::json!({ "hashes": chunk, "algorithm": "sha512" }))
            .send()
            .await;
        let Ok(response) = response else {
            complete = false;
            continue;
        };
        if !response.status().is_success() {
            complete = false;
            continue;
        }
        if let Ok(found) = response
            .json::<HashMap<String, ModrinthHashVersion>>()
            .await
        {
            versions.extend(found);
        } else {
            complete = false;
        }
    }

    let project_ids = versions
        .values()
        .map(|version| version.project_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut unsupported_projects = HashSet::new();
    for chunk in project_ids.chunks(100) {
        let Ok(ids) = serde_json::to_string(chunk) else {
            continue;
        };
        let response = http
            .get("https://api.modrinth.com/v2/projects")
            .timeout(std::time::Duration::from_secs(15))
            .query(&[("ids", ids)])
            .send()
            .await;
        let Ok(response) = response else {
            complete = false;
            continue;
        };
        if !response.status().is_success() {
            complete = false;
            continue;
        }
        if let Ok(projects) = response.json::<Vec<ModrinthProjectSide>>().await {
            unsupported_projects.extend(
                projects
                    .into_iter()
                    .filter(|project| project.server_side == "unsupported")
                    .map(|project| project.id),
            );
        } else {
            complete = false;
        }
    }

    let client_only = versions
        .into_iter()
        .filter_map(|(hash, version)| {
            unsupported_projects
                .contains(&version.project_id)
                .then_some(hash)
        })
        .collect();
    (client_only, complete)
}

/// Moves confirmed client-only mods out of the server's `mods` directory.
/// Files are quarantined rather than deleted so a user can always recover them.
pub async fn quarantine_client_only_mods(server_root: &Path) -> Result<Vec<String>, String> {
    let mods = server_root.join("mods");
    if !mods.is_dir() {
        return Ok(Vec::new());
    }

    let jars = std::fs::read_dir(&mods)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
        })
        .collect::<Vec<_>>();

    let cache_path = server_root.join(".lbby-mod-side-cache.json");
    let old_cache = std::fs::read_to_string(&cache_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<ModSideScanCache>(&raw).ok())
        .filter(|cache| cache.version == 1)
        .unwrap_or_default();
    let mut safe_cache = HashMap::<String, ModSideScanCacheEntry>::new();
    let mut cached_safe_paths = HashSet::<PathBuf>::new();
    let mut signatures = HashMap::<PathBuf, (String, ModSideScanCacheEntry)>::new();
    let mut hashes_by_path = HashMap::<PathBuf, String>::new();
    for path in &jars {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let signature = ModSideScanCacheEntry {
            bytes: metadata.len(),
            modified_secs: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
        };
        signatures.insert(path.clone(), (name.clone(), signature.clone()));
        if old_cache.files.get(&name).is_some_and(|cached| {
            cached.bytes == signature.bytes && cached.modified_secs == signature.modified_secs
        }) {
            safe_cache.insert(name, signature);
            cached_safe_paths.insert(path.clone());
            continue;
        }
        if !jar_declares_client_only(path) {
            if let Ok(hash) = sha512_file(path) {
                hashes_by_path.insert(path.clone(), hash);
            }
        }
    }
    let hashes = hashes_by_path.values().cloned().collect::<Vec<_>>();
    let (remote_client_only, remote_complete) = match client() {
        Ok(http) => modrinth_client_only_hashes(&http, &hashes).await,
        Err(_) => (HashSet::new(), false),
    };

    let quarantine = server_root.join(".lbby-client-only-mods");
    let mut moved = Vec::new();
    let mut client_mod_ids = HashSet::new();

    // First pass: identify client-only mods
    for path in &jars {
        if jar_declares_client_only(path) {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            client_mod_ids.insert(name);
        }
    }

    // Second pass: check for mods that depend on client-only mods
    // Build a set of client mod IDs (not file names)
    let mut client_mod_ids_set = HashSet::new();
    for path in &jars {
        if jar_declares_client_only(path) {
            // Extract mod ID from fabric.mod.json
            if let Some(mod_id) = jar_get_mod_id(path) {
                client_mod_ids_set.insert(mod_id);
            }
        }
    }
    eprintln!("[lbby] Client mod IDs: {:?}", client_mod_ids_set);

    for path in &jars {
        let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if client_mod_ids.contains(&file_name) {
            continue;
        }
        // Check if this mod depends on any client-only mod
        if let Some(deps) = jar_get_dependencies(path) {
            for dep in &deps {
                if client_mod_ids_set.contains(dep.as_str()) {
                    eprintln!("[lbby] Removing {} because it depends on client mod {}", file_name, dep);
                    client_mod_ids.insert(file_name.clone());
                    break;
                }
            }
        }
    }

    for path in jars {
        let should_move = !cached_safe_paths.contains(&path)
            && (jar_declares_client_only(&path)
                || hashes_by_path
                    .get(&path)
                    .is_some_and(|hash| remote_client_only.contains(hash))
                || client_mod_ids.contains(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
        if !should_move {
            if let Some((name, signature)) = signatures.get(&path) {
                if remote_complete || !hashes_by_path.contains_key(&path) {
                    safe_cache.insert(name.clone(), signature.clone());
                }
            }
            continue;
        }
        std::fs::create_dir_all(&quarantine).map_err(|e| e.to_string())?;
        let name = path
            .file_name()
            .ok_or("Invalid mod file name")?
            .to_string_lossy()
            .to_string();
        let mut destination = quarantine.join(&name);
        if destination.exists() {
            destination = quarantine.join(format!("{}-{}", uuid::Uuid::new_v4(), name));
        }
        std::fs::rename(&path, &destination).map_err(|e| {
            format!(
                "Failed to quarantine client-only mod {}: {}",
                path.display(),
                e
            )
        })?;
        moved.push(name);
    }
    moved.sort_by_key(|name| name.to_ascii_lowercase());
    let new_cache = ModSideScanCache {
        version: 1,
        files: safe_cache,
    };
    if let Ok(json) = serde_json::to_vec_pretty(&new_cache) {
        let _ = std::fs::write(cache_path, json);
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_jar(entries: &[(&str, &str)]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lbby-client-only-metadata-{}.jar",
            uuid::Uuid::new_v4()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, contents) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn detects_forge_client_side_only_metadata() {
        let path = test_jar(&[(
            "META-INF/mods.toml",
            "modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=true\n",
        )]);
        assert!(jar_declares_client_only(&path));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn detects_fabric_client_environment() {
        let path = test_jar(&[(
            "fabric.mod.json",
            r#"{"schemaVersion":1,"id":"client-test","environment":"client"}"#,
        )]);
        assert!(jar_declares_client_only(&path));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn keeps_universal_mod_metadata() {
        let path = test_jar(&[(
            "META-INF/mods.toml",
            "modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=false\n",
        )]);
        assert!(!jar_declares_client_only(&path));
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn quarantines_instead_of_deleting_client_only_mods() {
        let root = std::env::temp_dir().join(format!(
            "lbby-client-only-quarantine-{}",
            uuid::Uuid::new_v4()
        ));
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        let source = test_jar(&[(
            "fabric.mod.json",
            r#"{"schemaVersion":1,"id":"client-test","environment":"client"}"#,
        )]);
        let installed = mods.join("client-test.jar");
        std::fs::rename(source, &installed).unwrap();

        let moved = quarantine_client_only_mods(&root).await.unwrap();

        assert_eq!(moved, vec!["client-test.jar"]);
        assert!(!installed.exists());
        assert!(root
            .join(".lbby-client-only-mods")
            .join("client-test.jar")
            .exists());
        std::fs::remove_dir_all(root).ok();
    }
}
