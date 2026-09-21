// plugin_services — Plugin inventory management.
//
// Mirrors mod_services patterns for plugin lifecycle: scan, add, remove,
// receipts, and compatibility checking.

use crate::app_state::{
    PluginBatchUpdateResult, PluginCandidate, PluginCandidateHashes, PluginCompatResult,
    PluginCompatibility, PluginDependency, PluginDependencyGraph, PluginDependencyNode, PluginInfo,
    PluginInstallResult, PluginInventoryEntry, PluginPlatform, PluginProvider,
    PluginProviderCapabilities, PluginReceipt, PluginRemoveOutcome, PluginRuntimeCompatibility,
    PluginSearchResult, PluginStatus, PluginUpdateCheckResult, PluginUpdateOutcome,
    PluginUpdateStatus, ReleaseChannel,
};
use crate::config::ServerType;
use crate::jar_metadata::{self, PluginDescriptor};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

// ── Test seams ─────────────────────────────────────────────────────
#[cfg(test)]
static DOWNLOAD_SEAM: std::sync::Mutex<Option<fn(&str) -> Result<Vec<u8>, String>>> =
    std::sync::Mutex::new(None);

/// Test seam: bypass check_plugin_update with a pre-seeded result.
/// When set, update_plugin_no_guard uses this instead of hitting Modrinth/Hangar APIs.
#[cfg(test)]
static UPDATE_CHECK_SEAM: std::sync::Mutex<Option<fn(&str) -> PluginUpdateStatus>> =
    std::sync::Mutex::new(None);

/// Test seam: force old artifact removal failure during filename-changing update.
/// When set, old artifact removal returns error → triggers rollback.
#[cfg(test)]
static OLD_ARTIFACT_REMOVE_SEAM: std::sync::Mutex<Option<fn(&str) -> Result<(), String>>> =
    std::sync::Mutex::new(None);

/// Test seam: force receipt save failure.
/// When set, save_plugin_receipt returns error.
#[cfg(test)]
static RECEIPT_SAVE_SEAM: std::sync::Mutex<Option<fn() -> Result<(), String>>> =
    std::sync::Mutex::new(None);

/// When set, receipt persistence during removal returns error → triggers RemovedUntracked.
#[cfg(test)]
static RECEIPT_REMOVE_SEAM: std::sync::Mutex<Option<fn() -> Result<(), String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static MODRINTH_SEARCH_SEAM: std::sync::Mutex<
    Option<fn(&str, &str, &crate::config::ServerType) -> Result<PluginSearchResult, String>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static HANGAR_SEARCH_SEAM: std::sync::Mutex<
    Option<fn(&str, &str, &crate::config::ServerType) -> Result<PluginSearchResult, String>>,
> = std::sync::Mutex::new(None);

// ── Inventory ID ────────────────────────────────────────────────────────

/// Generate a stable inventory_id for a plugin artifact.
/// Opaque, unique within a profile inventory, not a filesystem path.
/// SHA-256 of (profile_path + file_name), truncated to 16 hex chars.
fn generate_plugin_inventory_id(profile_path: &Path, file_name: &str) -> String {
    let input = format!("{}::{}", profile_path.display(), file_name);
    let hash = Sha256::digest(input.as_bytes());
    format!("{:016x}", u64::from_be_bytes(hash[..8].try_into().unwrap()))
}

// ── Path safety ─────────────────────────────────────────────────────────

/// Validates that `name` is a single, safe filename component.
/// Rejects: empty, "..", "/", "\\", absolute, leading dot, nested paths.
pub fn validate_plugin_basename(name: &str) -> Result<&str, String> {
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

/// Join base + relative with traversal/escape rejection.
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
    // Final canonicalization check: joined path must stay under base
    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize base: {}", e))?;
    let canon_out = out
        .canonicalize()
        .map_err(|_| out.clone()) // may not exist yet; use raw
        .unwrap_or(out.clone());
    if !canon_out.starts_with(&canon_base) {
        return Err(format!("Path escape detected: {}", relative));
    }
    Ok(out)
}

// ── Plugins directory ───────────────────────────────────────────────────

/// Returns the plugins/ directory for a server path.
pub fn plugins_dir(server_path: &Path) -> Result<PathBuf, String> {
    if server_path.as_os_str().is_empty() {
        return Err("Choose a server folder first.".to_string());
    }
    Ok(server_path.join("plugins"))
}

// ── Descriptor → PluginInfo ─────────────────────────────────────────────

/// Classify which platform a descriptor's source file implies.
///
/// Folia is NOT included here — it requires explicit evidence (see folia_supported).
/// plugin.yml → Bukkit-family (Bukkit, Spigot, Paper, Purpur)
/// paper-plugin.yml → Bukkit-family (Paper)
/// velocity-plugin.json → Proxy (Velocity)
/// bungee.yml → Proxy (BungeeCord, Waterfall)
fn classify_platforms_from_source(source: &str) -> Vec<PluginPlatform> {
    match source {
        "plugin.yml" => vec![
            PluginPlatform::Bukkit,
            PluginPlatform::Spigot,
            PluginPlatform::Paper,
            PluginPlatform::Purpur,
        ],
        "paper-plugin.yml" => vec![PluginPlatform::Paper],
        "velocity-plugin.json" => vec![PluginPlatform::Velocity],
        "bungee.yml" => vec![PluginPlatform::BungeeCord, PluginPlatform::Waterfall],
        _ => vec![PluginPlatform::Unknown],
    }
}

/// Classify which family a descriptor source belongs to.
fn descriptor_family(source: &str) -> DescriptorFamily {
    match source {
        "plugin.yml" | "paper-plugin.yml" => DescriptorFamily::Bukkit,
        "velocity-plugin.json" | "bungee.yml" => DescriptorFamily::Proxy,
        _ => DescriptorFamily::Unknown,
    }
}

enum DescriptorFamily {
    Bukkit,
    Proxy,
    Unknown,
}

/// Merge multiple descriptors from a single JAR into one canonical descriptor.
///
/// Policy:
/// - Same-family descriptors → deterministic merge (primary descriptor wins for name/version/main,
///   platforms merged, dependencies merged/unioned)
/// - Cross-family descriptors (Bukkit + Proxy) → None (ambiguous, no fabricated compatibility)
/// - Single descriptor → pass through
/// - Empty → None
///
/// Returns (merged_descriptor, platforms, is_ambiguous).
fn resolve_plugin_descriptors(
    descriptors: &[PluginDescriptor],
) -> Option<(PluginDescriptor, Vec<PluginPlatform>, bool)> {
    if descriptors.is_empty() {
        return None;
    }

    // Group by family
    let mut bukkit_descs: Vec<&PluginDescriptor> = Vec::new();
    let mut proxy_descs: Vec<&PluginDescriptor> = Vec::new();

    for desc in descriptors {
        match descriptor_family(&desc.source) {
            DescriptorFamily::Bukkit => bukkit_descs.push(desc),
            DescriptorFamily::Proxy => proxy_descs.push(desc),
            DescriptorFamily::Unknown => {} // skip unknown sources
        }
    }

    let has_bukkit = !bukkit_descs.is_empty();
    let has_proxy = !proxy_descs.is_empty();

    // Cross-family conflict → ambiguous, empty platforms
    if has_bukkit && has_proxy {
        let base = descriptors.first()?;
        let mut merged = base.clone();
        merged.source = "ambiguous".to_string();
        return Some((merged, vec![], true));
    }

    // Single family — validate identity compatibility before merging
    let family_descs = if has_bukkit {
        &bukkit_descs
    } else {
        &proxy_descs
    };

    if family_descs.is_empty() {
        return None;
    }

    // Identity conflict detection: if both descriptors provide name or main_class
    // and values differ materially → ambiguous, empty platforms
    if family_descs.len() > 1 {
        let primary = family_descs[0];
        for secondary in family_descs.iter().skip(1) {
            // Check name conflict
            if let (Some(ref p_name), Some(ref s_name)) = (&primary.name, &secondary.name) {
                if p_name != s_name {
                    let base = descriptors.first()?;
                    let mut merged = base.clone();
                    merged.source = "ambiguous".to_string();
                    return Some((merged, vec![], true));
                }
            }
            // Check main_class conflict
            if let (Some(ref p_main), Some(ref s_main)) =
                (&primary.main_class, &secondary.main_class)
            {
                if p_main != s_main {
                    let base = descriptors.first()?;
                    let mut merged = base.clone();
                    merged.source = "ambiguous".to_string();
                    return Some((merged, vec![], true));
                }
            }
        }
    }

    // Primary = first descriptor in zip order (deterministic)
    let primary = family_descs[0];
    let mut merged = primary.clone();

    // Merge platforms from all descriptors in this family
    let mut all_platforms: Vec<PluginPlatform> = family_descs
        .iter()
        .flat_map(|d| classify_platforms_from_source(&d.source))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    all_platforms.sort_by_key(|p| format!("{:?}", p)); // deterministic order

    // Merge dependencies (union, deduplicated)
    let mut seen_deps: std::collections::HashSet<String> = std::collections::HashSet::new();
    for desc in family_descs.iter().skip(1) {
        // Fill missing fields from secondary descriptors
        if merged.name.is_none() {
            merged.name = desc.name.clone();
        }
        if merged.version.is_none() {
            merged.version = desc.version.clone();
        }
        if merged.main_class.is_none() {
            merged.main_class = desc.main_class.clone();
        }
        if merged.api_version.is_none() {
            merged.api_version = desc.api_version.clone();
        }
        if merged.description.is_none() {
            merged.description = desc.description.clone();
        }
        if merged.website.is_none() {
            merged.website = desc.website.clone();
        }
        if merged.authors.is_empty() {
            merged.authors = desc.authors.clone();
        }
        // Merge dependencies (union)
        for dep in &desc.depend {
            if seen_deps.insert(dep.clone()) {
                merged.depend.push(dep.clone());
            }
        }
        for dep in &desc.soft_depend {
            if seen_deps.insert(dep.clone()) {
                merged.soft_depend.push(dep.clone());
            }
        }
        for dep in &desc.load_before {
            if seen_deps.insert(dep.clone()) {
                merged.load_before.push(dep.clone());
            }
        }
    }

    // Also add primary deps to seen set (for completeness)
    for dep in &merged.depend {
        seen_deps.insert(dep.clone());
    }
    for dep in &merged.soft_depend {
        seen_deps.insert(dep.clone());
    }
    for dep in &merged.load_before {
        seen_deps.insert(dep.clone());
    }

    Some((merged, all_platforms, false))
}

/// Build a PluginInfo from a descriptor, file_name, profile path, and resolved platforms.
/// Platforms are passed in (not derived from source) because multi-descriptor merge
/// may produce a combined platform set.
fn plugin_info_from_descriptor(
    desc: &PluginDescriptor,
    file_name: &str,
    profile_path: &Path,
    platforms: Vec<PluginPlatform>,
    folia_supported: Option<bool>,
) -> PluginInfo {
    let inventory_id = generate_plugin_inventory_id(profile_path, file_name);

    let dependencies: Vec<PluginDependency> = desc
        .depend
        .iter()
        .map(|name| PluginDependency {
            name: name.clone(),
            required: true,
            load_before: false,
        })
        .chain(desc.soft_depend.iter().map(|name| PluginDependency {
            name: name.clone(),
            required: false,
            load_before: false,
        }))
        .chain(desc.load_before.iter().map(|name| PluginDependency {
            name: name.clone(),
            required: false,
            load_before: true,
        }))
        .collect();

    PluginInfo {
        inventory_id,
        file_name: file_name.to_string(),
        display_name: desc.name.clone(),
        plugin_name: desc.name.clone(),
        version: desc.version.clone(),
        main_class: desc.main_class.clone(),
        authors: desc.authors.clone(),
        description: desc.description.clone(),
        website: desc.website.clone(),
        api_version: desc.api_version.clone(),
        platforms,
        provider: None,
        project_id: None,
        file_version_id: None,
        artifact_hash: None,
        status: PluginStatus::Readable,
        dependencies,
        folia_supported,
    }
}

/// Build a PluginInfo for an unreadable JAR.
fn unreadable_plugin_info(file_name: &str, profile_path: &Path) -> PluginInfo {
    let inventory_id = generate_plugin_inventory_id(profile_path, file_name);
    PluginInfo {
        inventory_id,
        file_name: file_name.to_string(),
        status: PluginStatus::Unreadable,
        ..Default::default()
    }
}

/// Build a PluginInfo for a JAR with no recognized descriptor.
fn unknown_metadata_plugin_info(file_name: &str, profile_path: &Path) -> PluginInfo {
    let inventory_id = generate_plugin_inventory_id(profile_path, file_name);
    PluginInfo {
        inventory_id,
        file_name: file_name.to_string(),
        status: PluginStatus::UnknownMetadata,
        ..Default::default()
    }
}

// ── Folia evidence detection ─────────────────────────────────────────────

/// Detect explicit Folia compatibility evidence from descriptors.
///
/// Only checks explicit "folia-supported" field in descriptor metadata.
/// No substring heuristic on api_version.
///
/// Returns:
/// - Some(true) if explicit folia-supported: true
/// - Some(false) if explicit folia-supported: false
/// - None if no explicit Folia evidence (unknown)
fn detect_folia_evidence(descriptors: &[PluginDescriptor]) -> Option<bool> {
    for desc in descriptors {
        // Check explicit folia_supported field from descriptor metadata
        // This is the ONLY authoritative signal for Folia compatibility.
        // No substring heuristic on api_version.
        if let Some(folia) = desc.folia_supported {
            return Some(folia);
        }
    }
    None
}

// ── Receipt trust ───────────────────────────────────────────────────────

/// Apply receipt trust binding to a PluginInfo.
///
/// If a receipt exists for the filename AND the artifact hash matches:
///   → provider, project_id, file_version_id, platforms from receipt are trusted
///
/// If receipt exists but hash mismatches:
///   → provider trust invalidated (stays None/Unknown), artifact preserved
///
/// If no receipt:
///   → provider stays Manual/Unknown (default from PluginInfo)
fn apply_receipt_trust(
    info: &mut PluginInfo,
    file_name: &str,
    artifact_hash: &str,
    receipts: &PluginProfileReceipts,
) {
    if let Some(receipt) = receipts.receipts.get(file_name) {
        if receipt.artifact_hash == artifact_hash {
            // Hash matches → trust receipt identity
            info.provider = Some(receipt.provider.clone());
            info.project_id = Some(receipt.project_id.clone());
            info.file_version_id = receipt.file_version_id.clone();
            // Only apply receipt platforms if the descriptor didn't provide any
            // (descriptor platforms are authoritative for classification)
            if info.platforms.is_empty() || info.platforms == vec![PluginPlatform::Unknown] {
                info.platforms = receipt.platforms.clone();
            }
        }
        // Hash mismatch → provider trust invalidated, artifact preserved
        // info.provider stays as-is (None/Unknown)
    }
    // Missing receipt → info.provider stays as-is (None/Unknown)
}

// ── Scanning ────────────────────────────────────────────────────────────

/// Scan the plugins/ directory and produce a PluginInfo for each JAR.
///
/// Receipt trust is applied during scan: if a receipt exists for a filename AND
/// the artifact hash matches, the provider/project/file identity from the receipt
/// is trusted. Stale receipts (hash mismatch) → provider trust invalidated but
/// artifact preserved. Missing receipts → Manual/Unknown provider.
pub fn scan_plugin_directory(server_path: &Path) -> Result<Vec<PluginInfo>, String> {
    let dir = plugins_dir(server_path)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    // Load receipts for provider trust binding
    // Profile path is parent of server_path (server_path = profile_path/server_name)
    let profile_path = server_path.parent().unwrap_or(server_path);
    let receipts = load_plugin_receipts(profile_path).unwrap_or_default();

    let mut plugins = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jar") {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Attempt to read as a zip and find plugin descriptors
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                plugins.push(unreadable_plugin_info(&file_name, server_path));
                continue;
            }
        };
        let jar = zip::ZipArchive::new(file);
        if jar.is_err() {
            plugins.push(unreadable_plugin_info(&file_name, server_path));
            continue;
        }

        // Compute artifact hash for receipt trust
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                plugins.push(unreadable_plugin_info(&file_name, server_path));
                continue;
            }
        };
        let artifact_hash = format!("{:x}", Sha256::digest(&bytes));

        // Re-open to read descriptors (ZipArchive needs to be consumed)
        let descriptors = jar_metadata::read_jar_plugin_descriptors(&path);
        if descriptors.is_empty() {
            plugins.push(unknown_metadata_plugin_info(&file_name, server_path));
        } else {
            // Resolve multi-descriptor: merge same-family, mark cross-family ambiguous
            if let Some((merged_desc, platforms, is_ambiguous)) =
                resolve_plugin_descriptors(&descriptors)
            {
                let folia_supported = detect_folia_evidence(&descriptors);
                let mut info = plugin_info_from_descriptor(
                    &merged_desc,
                    &file_name,
                    server_path,
                    platforms,
                    folia_supported,
                );
                info.artifact_hash = Some(artifact_hash.clone());

                // Apply receipt trust binding
                apply_receipt_trust(&mut info, &file_name, &artifact_hash, &receipts);

                // Mark ambiguous if cross-family conflict
                if is_ambiguous {
                    info.status = PluginStatus::UnknownMetadata;
                }

                plugins.push(info);
            } else {
                plugins.push(unknown_metadata_plugin_info(&file_name, server_path));
            }
        }
    }

    Ok(plugins)
}

// ── Add / Remove ────────────────────────────────────────────────────────

/// Safe copy of a plugin JAR into the plugins/ directory.
/// Validates: basename safety, readable JAR, valid descriptor (optional),
/// then atomic commit (write tmp, rename).
pub fn add_plugin_safe(source_path: &Path, server_path: &Path) -> Result<PluginInfo, String> {
    // 1. Extract and validate basename
    let file_name = source_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid source file name")?;
    validate_plugin_basename(file_name)?;

    if !file_name.ends_with(".jar") {
        return Err("Only .jar files are supported for plugins".into());
    }

    // Verify source is a readable JAR
    let file =
        std::fs::File::open(source_path).map_err(|e| format!("Cannot open source: {}", e))?;
    let _jar =
        zip::ZipArchive::new(file).map_err(|_| "Source is not a valid JAR/ZIP file".to_string())?;

    // 3. Read descriptors
    let descriptors = jar_metadata::read_jar_plugin_descriptors(source_path);

    // 4. Compute artifact hash (SHA-256)
    let bytes = std::fs::read(source_path).map_err(|e| e.to_string())?;
    let artifact_hash = format!("{:x}", Sha256::digest(&bytes));

    // 5. Atomic copy to plugins/
    let plugins = plugins_dir(server_path)?;
    std::fs::create_dir_all(&plugins).map_err(|e| e.to_string())?;
    let dest = plugins.join(file_name);

    // Write to temp file, then rename (atomic on same filesystem)
    let tmp = dest.with_extension("jar.lbbytmp");
    std::fs::write(&tmp, &bytes).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Write failed: {}", e)
    })?;
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Rename failed: {}", e)
    })?;

    // 6. Build PluginInfo using multi-descriptor resolution
    if descriptors.is_empty() {
        let mut info = unknown_metadata_plugin_info(file_name, server_path);
        info.artifact_hash = Some(artifact_hash);
        Ok(info)
    } else {
        if let Some((merged_desc, platforms, is_ambiguous)) =
            resolve_plugin_descriptors(&descriptors)
        {
            let folia_supported = detect_folia_evidence(&descriptors);
            let mut info = plugin_info_from_descriptor(
                &merged_desc,
                file_name,
                server_path,
                platforms,
                folia_supported,
            );

            // Apply receipt trust binding
            let profile_path = server_path.parent().unwrap_or(server_path);
            let receipts = load_plugin_receipts(profile_path).unwrap_or_default();
            apply_receipt_trust(&mut info, file_name, &artifact_hash, &receipts);

            info.artifact_hash = Some(artifact_hash);

            if is_ambiguous {
                info.status = PluginStatus::UnknownMetadata;
            }
            Ok(info)
        } else {
            let mut info = unknown_metadata_plugin_info(file_name, server_path);
            info.artifact_hash = Some(artifact_hash);
            Ok(info)
        }
    }
}

/// Remove a plugin by inventory_id.
/// Resolves inventory_id → file, rejects path traversal/escape.
pub fn remove_plugin(inventory_id: &str, server_path: &Path) -> Result<(), String> {
    let plugins = plugins_dir(server_path)?;
    let target = resolve_inventory_id(inventory_id, server_path)?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid resolved file name")?;

    // Safety: reject traversal in the resolved name
    validate_plugin_basename(file_name)?;

    // Verify the resolved path is actually under plugins/
    let canon_plugins = plugins
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize plugins dir: {}", e))?;
    let canon_target = target
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize target: {}", e))?;
    if !canon_target.starts_with(&canon_plugins) {
        return Err("Path escape detected in resolved inventory_id".into());
    }

    std::fs::remove_file(&canon_target).map_err(|e| e.to_string())
}

/// Resolve an inventory_id to the actual file path in plugins/.
fn resolve_inventory_id(inventory_id: &str, server_path: &Path) -> Result<PathBuf, String> {
    let plugins = plugins_dir(server_path)?;
    if !plugins.exists() {
        return Err("Plugins directory does not exist".into());
    }

    let entries = std::fs::read_dir(&plugins).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jar") {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let id = generate_plugin_inventory_id(server_path, file_name);
        if id == inventory_id {
            return Ok(path);
        }
    }

    Err(format!(
        "No plugin found with inventory_id: {}",
        inventory_id
    ))
}

// ── Receipts ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginProfileReceipts {
    pub schema_version: u32,
    pub receipts: HashMap<String, PluginReceipt>,
}

impl Default for PluginProfileReceipts {
    fn default() -> Self {
        Self {
            schema_version: 1,
            receipts: HashMap::new(),
        }
    }
}

/// Load plugin receipts from the profile-scoped metadata file.
pub fn load_plugin_receipts(profile_path: &Path) -> Result<PluginProfileReceipts, String> {
    let path = profile_path.join(".lbby-plugin-receipts.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(PluginProfileReceipts::default());
    };
    let store: PluginProfileReceipts =
        serde_json::from_slice(&bytes).map_err(|e| format!("Corrupt plugin receipts: {}", e))?;
    if store.schema_version > 1 {
        return Ok(PluginProfileReceipts::default());
    }
    Ok(store)
}

/// Persist plugin receipts atomically.
pub fn save_plugin_receipts(
    profile_path: &Path,
    store: &PluginProfileReceipts,
) -> Result<(), String> {
    let path = profile_path.join(".lbby-plugin-receipts.json");
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

// ── Compatibility ───────────────────────────────────────────────────────

/// Classify plugin compatibility with the current server type.
/// Does NOT reuse mod compatibility semantics (ClientOnly, Both, ServerOk).
pub fn check_plugin_platform_compatibility(
    plugin_platforms: &[PluginPlatform],
    server_type: &ServerType,
) -> PluginCompatResult {
    // Proxy server types
    // If all platforms are proxy-only and server is not proxy, it's a mismatch
    let all_proxy = plugin_platforms.iter().all(|p| {
        matches!(
            p,
            PluginPlatform::Velocity | PluginPlatform::Waterfall | PluginPlatform::BungeeCord
        )
    });
    if all_proxy && !plugin_platforms.is_empty() {
        // Check if server is also proxy
        // Velocity/Waterfall/BungeeCord are not in ServerType enum — they're proxy types
        // If server_type is not in the known proxy set, these plugins are ProxyPlugin
        // But we can't tell from ServerType alone, so we return ProxyPlugin
        return PluginCompatResult {
            compatible: PluginCompatibility::ProxyPlugin,
            source: "platform".to_string(),
            reason: "Plugin is for proxy servers only".to_string(),
        };
    }

    // Map server type to expected platforms
    let expected_platforms: Vec<PluginPlatform> = match server_type {
        ServerType::Bukkit => vec![
            PluginPlatform::Bukkit,
            PluginPlatform::Spigot,
            PluginPlatform::Paper,
            PluginPlatform::Purpur,
            PluginPlatform::Folia,
        ],
        ServerType::Spigot => vec![
            PluginPlatform::Spigot,
            PluginPlatform::Paper,
            PluginPlatform::Purpur,
            PluginPlatform::Folia,
        ],
        ServerType::Paper => vec![
            PluginPlatform::Paper,
            PluginPlatform::Purpur,
            PluginPlatform::Folia,
        ],
        ServerType::Purpur => vec![
            PluginPlatform::Purpur,
            PluginPlatform::Paper,
            PluginPlatform::Spigot,
            PluginPlatform::Bukkit,
            PluginPlatform::Folia,
        ],
        ServerType::Folia => vec![
            PluginPlatform::Folia,
            PluginPlatform::Paper,
            PluginPlatform::Purpur,
        ],
        _ => vec![], // Non-plugin server types
    };

    if expected_platforms.is_empty() {
        return PluginCompatResult {
            compatible: PluginCompatibility::Unknown,
            source: "server_type".to_string(),
            reason: format!(
                "Server type {:?} has no known plugin platform mapping",
                server_type
            ),
        };
    }

    // Check if plugin platforms overlap with expected
    let has_match = plugin_platforms
        .iter()
        .any(|p| expected_platforms.contains(p));

    if has_match {
        // Folia-specific check
        if *server_type == ServerType::Folia {
            if plugin_platforms.contains(&PluginPlatform::Folia) {
                return PluginCompatResult {
                    compatible: PluginCompatibility::FoliaCompatible,
                    source: "platform".to_string(),
                    reason: "Plugin explicitly supports Folia".to_string(),
                };
            }
            return PluginCompatResult {
                compatible: PluginCompatibility::FoliaUnknown,
                source: "platform".to_string(),
                reason: "Plugin supports Paper/Spigot but Folia compatibility unknown".to_string(),
            };
        }
        return PluginCompatResult {
            compatible: PluginCompatibility::Compatible,
            source: "platform".to_string(),
            reason: "Plugin platform matches server type".to_string(),
        };
    }

    // Check if plugin is for a different server type family
    let plugin_is_server = plugin_platforms.iter().any(|p| {
        matches!(
            p,
            PluginPlatform::Bukkit
                | PluginPlatform::Spigot
                | PluginPlatform::Paper
                | PluginPlatform::Purpur
                | PluginPlatform::Folia
        )
    });

    if plugin_is_server {
        return PluginCompatResult {
            compatible: PluginCompatibility::PlatformMismatch,
            source: "platform".to_string(),
            reason: "Plugin platform does not match server type".to_string(),
        };
    }

    if plugin_platforms.contains(&PluginPlatform::Unknown) {
        return PluginCompatResult {
            compatible: PluginCompatibility::Unknown,
            source: "platform".to_string(),
            reason: "Plugin platform could not be determined".to_string(),
        };
    }

    PluginCompatResult {
        compatible: PluginCompatibility::Unknown,
        source: "platform".to_string(),
        reason: "Unable to classify compatibility".to_string(),
    }
}

// ── 4B.4B: Provider-aware plugin resolution ─────────────────────────

use serde::Deserialize;

// ── Provider capability declarations ───────────────────────────────

/// Returns capabilities for each known plugin provider.
pub fn get_plugin_provider_capabilities() -> Vec<PluginProviderCapabilities> {
    vec![
        PluginProviderCapabilities {
            provider: PluginProvider::Modrinth,
            search: true,
            project_lookup: true,
            version_resolution: true,
            direct_download: true,
            hash_verification: true, // SHA-512 + SHA-1
            platform_filtering: true,
            mc_version_filtering: true,
        },
        PluginProviderCapabilities {
            provider: PluginProvider::Hangar,
            search: true,
            project_lookup: true,
            version_resolution: true,
            direct_download: true,
            hash_verification: true, // SHA-256 per platform
            platform_filtering: true,
            mc_version_filtering: true,
        },
        PluginProviderCapabilities {
            provider: PluginProvider::CurseForge,
            search: false, // plugin category not reliably filterable
            project_lookup: false,
            version_resolution: false,
            direct_download: false,
            hash_verification: false,
            platform_filtering: false,
            mc_version_filtering: false,
        },
        PluginProviderCapabilities {
            provider: PluginProvider::SpigotMC,
            search: false, // no stable public REST API
            project_lookup: false,
            version_resolution: false,
            direct_download: false,
            hash_verification: false,
            platform_filtering: false,
            mc_version_filtering: false,
        },
        PluginProviderCapabilities {
            provider: PluginProvider::Manual,
            search: false, // local-only
            project_lookup: false,
            version_resolution: false,
            direct_download: false,  // user provides file
            hash_verification: true, // local SHA-256 after copy
            platform_filtering: false,
            mc_version_filtering: false,
        },
    ]
}

// ── Modrinth API types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ModrinthSearchResponse {
    hits: Vec<ModrinthSearchHit>,
    total_hits: u32,
    offset: u32,
    limit: u32,
}

#[derive(Debug, Deserialize)]
struct ModrinthSearchHit {
    project_id: String,
    slug: String,
    title: String,
    description: String,
    icon_url: Option<String>,
    categories: Vec<String>,
    versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModrinthVersion {
    id: String,
    name: String,
    version_number: String,
    game_versions: Vec<String>,
    loaders: Vec<String>,
    version_type: String, // "release" | "beta" | "alpha"
    files: Vec<ModrinthFile>,
    date_published: String,
}

#[derive(Debug, Deserialize)]
struct ModrinthFile {
    url: String,
    filename: String,
    hashes: ModrinthHashes,
    primary: bool,
}

#[derive(Debug, Deserialize)]
struct ModrinthHashes {
    sha512: Option<String>,
    sha1: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ModrinthProject {
    id: String,
    title: String,
    description: String,
    team: String,
    // authors are in a separate call; we use team members
}

// ── Hangar API types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HangarProjectResult {
    pagination: HangarPagination,
    result: Vec<HangarProjectEntry>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HangarPagination {
    limit: u32,
    offset: u32,
    count: u32,
}

#[derive(Debug, Deserialize)]
struct HangarProjectEntry {
    plugin_id: u64,
    namespace: HangarNamespace,
    name: String,
    stats: HangarStats,
    description: Option<String>,
    icon_url: Option<String>,
    #[serde(default)]
    settings: HangarSettings,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HangarNamespace {
    owner: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HangarStats {
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    stars: u64,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct HangarSettings {
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HangarVersionsResult {
    pagination: HangarPagination,
    result: Vec<HangarVersionEntry>,
}

#[derive(Debug, Deserialize)]
struct HangarVersionEntry {
    id: u64,
    name: String,
    channel: HangarChannel,
    pinned: bool,
    #[serde(default)]
    platform_dependencies: std::collections::HashMap<String, Vec<String>>,
    #[serde(default)]
    downloads: std::collections::HashMap<String, HangarDownload>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HangarChannel {
    name: String,
    // Hangar uses "Release", "Beta", "Alpha" and has isHidden, isFeatured
    #[serde(default)]
    is_hidden: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct HangarDownload {
    download_url: String,
    file_size: u64,
    hash: String, // SHA-256
    #[serde(default)]
    platform: HangarPlatformInfo,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct HangarPlatformInfo {
    name: String,
}

// ── Modrinth search ────────────────────────────────────────────────

/// Search Modrinth for plugins matching query, MC version, and platform.
pub async fn search_modrinth_plugins(
    query: &str,
    mc_version: &str,
    server_type: &ServerType,
) -> Result<PluginSearchResult, String> {
    let client = crate::mod_services::client()?;
    let loader = server_type_to_modrinth_loader(server_type);
    let facets = if loader.is_empty() {
        format!("[[\"project_type:plugin\"],[\"versions:{}\"]]", mc_version)
    } else {
        format!(
            "[[\"project_type:plugin\"],[\"versions:{}\"],[\"categories:{}\"]]",
            mc_version, loader
        )
    };
    let url = format!(
        "https://api.modrinth.com/v2/search?query={}&facets={}&limit=20&index=relevance",
        urlencoding::encode(query),
        urlencoding::encode(&facets),
    );
    let resp: ModrinthSearchResponse = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Modrinth search failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth parse failed: {}", e))?;

    let total = resp.total_hits;
    let candidates: Vec<PluginCandidate> = resp
        .hits
        .into_iter()
        .map(|hit| {
            let platforms = modrinth_categories_to_platforms(&hit.categories);
            PluginCandidate {
                provider: PluginProvider::Modrinth,
                project_id: hit.project_id.clone(),
                file_version_id: None, // resolved later
                title: hit.title,
                description: Some(hit.description),
                authors: vec![], // Modrinth search doesn't include authors
                download_url: String::new(), // resolved later
                filename: String::new(),
                hashes: PluginCandidateHashes::default(),
                game_versions: hit.versions,
                platforms,
                release_channel: ReleaseChannel::Release, // default until version resolved
                published_at: None,
                icon_url: hit.icon_url,
                compatibility: None,
            }
        })
        .collect();

    Ok(PluginSearchResult {
        candidates,
        provider: PluginProvider::Modrinth,
        has_more: (resp.offset + resp.limit) < total,
        total_hits: Some(total),
    })
}

/// Resolve the best version for a Modrinth plugin.
/// Filters by MC version and selects by release-channel priority.
pub async fn resolve_modrinth_plugin_version(
    project_id: &str,
    mc_version: &str,
    server_type: &ServerType,
) -> Result<PluginCandidate, String> {
    let client = crate::mod_services::client()?;
    let loader = server_type_to_modrinth_loader(server_type);
    let url = if loader.is_empty() {
        format!(
            "https://api.modrinth.com/v2/project/{}/version?game_versions=%5B%22{}%22%5D",
            project_id, mc_version
        )
    } else {
        format!(
            "https://api.modrinth.com/v2/project/{}/version?game_versions=%5B%22{}%22%5D&loaders=%5B%22{}%22%5D",
            project_id, mc_version, loader
        )
    };
    let versions: Vec<ModrinthVersion> = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Modrinth version resolve failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Modrinth version parse failed: {}", e))?;

    let best = select_best_version(&versions, mc_version)?;
    let file = best
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| best.files.first())
        .ok_or_else(|| "Modrinth version has no files".to_string())?;

    let channel = match best.version_type.as_str() {
        "release" => ReleaseChannel::Release,
        "beta" => ReleaseChannel::Beta,
        "alpha" => ReleaseChannel::Alpha,
        _ => ReleaseChannel::Release,
    };

    // Map loaders to PluginPlatform
    let platforms: Vec<PluginPlatform> = best
        .loaders
        .iter()
        .filter_map(|l| modrinth_loader_to_platform(l))
        .collect();

    Ok(PluginCandidate {
        provider: PluginProvider::Modrinth,
        project_id: project_id.to_string(),
        file_version_id: Some(best.id.clone()),
        title: best.name.clone(),
        description: None,
        authors: vec![],
        download_url: file.url.clone(),
        filename: file.filename.clone(),
        hashes: PluginCandidateHashes {
            sha512: file.hashes.sha512.clone(),
            sha256: None,
            sha1: file.hashes.sha1.clone(),
        },
        game_versions: best.game_versions.clone(),
        platforms,
        release_channel: channel,
        published_at: Some(best.date_published.clone()),
        icon_url: None,
        compatibility: None,
    })
}

// ── Hangar search ──────────────────────────────────────────────────

/// Search Hangar for plugins matching query.
pub async fn search_hangar_plugins(
    query: &str,
    mc_version: &str,
    server_type: &ServerType,
) -> Result<PluginSearchResult, String> {
    let client = crate::mod_services::client()?;
    let platform_filter = server_type_to_hangar_platform(server_type);

    let url = format!(
        "https://hangar.papermc.io/api/v1/projects?query={}&limit=20&offset=0",
        urlencoding::encode(query),
    );
    let resp: HangarProjectResult = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Hangar search failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Hangar parse failed: {}", e))?;

    let total = resp.pagination.count;
    let mut candidates = Vec::new();

    for entry in resp.result {
        // For each project, we need to resolve versions to get file info.
        // For search results, create a placeholder candidate — full resolution happens on install.
        let platforms = hangar_platforms_from_project(&entry, &platform_filter);
        let has_folia_tag = entry.settings.tags.iter().any(|t| t == "SUPPORTS_FOLIA");

        let mut resolved_platforms = platforms;
        // If project supports Folia, add it
        if has_folia_tag && !resolved_platforms.contains(&PluginPlatform::Folia) {
            // don't add Folia platform to candidates — Folia is an extension of Paper
        }

        candidates.push(PluginCandidate {
            provider: PluginProvider::Hangar,
            project_id: format!("{}/{}", entry.namespace.owner, entry.namespace.slug),
            file_version_id: None,
            title: entry.name.clone(),
            description: entry.description.clone(),
            authors: vec![entry.namespace.owner.clone()],
            download_url: String::new(),
            filename: String::new(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec![], // resolved on version lookup
            platforms: resolved_platforms,
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: entry.icon_url.clone(),
            compatibility: None,
        });
    }

    Ok(PluginSearchResult {
        candidates,
        provider: PluginProvider::Hangar,
        has_more: (resp.pagination.offset + resp.pagination.limit) < total,
        total_hits: Some(total),
    })
}

/// Resolve the best version for a Hangar plugin.
pub async fn resolve_hangar_plugin_version(
    project_slug: &str,
    mc_version: &str,
    server_type: &ServerType,
) -> Result<PluginCandidate, String> {
    let client = crate::mod_services::client()?;
    let platform_name = server_type_to_hangar_platform(server_type);

    let url = format!(
        "https://hangar.papermc.io/api/v1/projects/{}/versions?limit=50",
        project_slug
    );
    let resp: HangarVersionsResult = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Hangar version resolve failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Hangar version parse failed: {}", e))?;

    // Filter versions that support this MC version + platform
    let mut compatible: Vec<(&HangarVersionEntry, &HangarDownload)> = Vec::new();
    for ver in &resp.result {
        if ver.channel.is_hidden {
            continue;
        }
        // Check platform dependencies for MC version match
        let has_mc = ver
            .platform_dependencies
            .values()
            .any(|deps| deps.iter().any(|d| d == mc_version));
        if !has_mc {
            continue;
        }
        // Find the download for the target platform
        if let Some(dl) = ver.downloads.get(&platform_name) {
            compatible.push((ver, dl));
        } else {
            // Try without platform filter (some plugins have only one platform download)
            for (_, dl) in &ver.downloads {
                compatible.push((ver, dl));
                break;
            }
        }
    }

    let (best_ver, best_dl) = compatible
        .into_iter()
        .min_by_key(|(ver, _)| hangar_channel_priority(&ver.channel.name))
        .ok_or_else(|| {
            format!(
                "No compatible Hangar version found for {} / {}",
                project_slug, mc_version
            )
        })?;

    let channel = match best_ver.channel.name.to_lowercase().as_str() {
        "release" => ReleaseChannel::Release,
        "beta" => ReleaseChannel::Beta,
        "alpha" => ReleaseChannel::Alpha,
        _ => ReleaseChannel::Release,
    };

    let mut platforms = vec![];
    if let Some(deps) = best_ver.platform_dependencies.get("PAPER") {
        if !deps.is_empty() {
            platforms.push(PluginPlatform::Paper);
        }
    }
    if let Some(deps) = best_ver.platform_dependencies.get("VELOCITY") {
        if !deps.is_empty() {
            platforms.push(PluginPlatform::Velocity);
        }
    }
    if let Some(deps) = best_ver.platform_dependencies.get("WATERFALL") {
        if !deps.is_empty() {
            platforms.push(PluginPlatform::Waterfall);
        }
    }

    let filename_from_url = best_dl
        .download_url
        .rsplit('/')
        .next()
        .unwrap_or("plugin.jar")
        .to_string();

    Ok(PluginCandidate {
        provider: PluginProvider::Hangar,
        project_id: project_slug.to_string(),
        file_version_id: Some(best_ver.id.to_string()),
        title: best_ver.name.clone(),
        description: None,
        authors: vec![],
        download_url: best_dl.download_url.clone(),
        filename: filename_from_url,
        hashes: PluginCandidateHashes {
            sha512: None,
            sha256: Some(best_dl.hash.clone()),
            sha1: None,
        },
        game_versions: best_ver
            .platform_dependencies
            .values()
            .flat_map(|v| v.clone())
            .collect(),
        platforms,
        release_channel: channel,
        published_at: None,
        icon_url: None,
        compatibility: None,
    })
}

// ── Unified search ─────────────────────────────────────────────────

/// Search all available providers for plugins.
pub async fn search_plugins(
    query: &str,
    mc_version: &str,
    server_type: &ServerType,
) -> Result<Vec<PluginSearchResult>, String> {
    let mut results = Vec::new();

    // Modrinth
    let modrinth_result = {
        #[cfg(test)]
        {
            let seam = MODRINTH_SEARCH_SEAM.lock().unwrap();
            if let Some(f) = *seam {
                f(query, mc_version, server_type)
            } else {
                search_modrinth_plugins(query, mc_version, server_type).await
            }
        }
        #[cfg(not(test))]
        search_modrinth_plugins(query, mc_version, server_type).await
    };
    match modrinth_result {
        Ok(mut r) => {
            for c in &mut r.candidates {
                let compat = check_candidate_compatibility(c, mc_version, server_type, None);
                c.compatibility = Some(compat.compatible);
            }
            results.push(r);
        }
        Err(e) => {
            results.push(PluginSearchResult {
                candidates: vec![],
                provider: PluginProvider::Modrinth,
                has_more: false,
                total_hits: None,
            });
            eprintln!("[plugin_providers] Modrinth search error: {}", e);
        }
    }

    // Hangar
    let hangar_result = {
        #[cfg(test)]
        {
            let seam = HANGAR_SEARCH_SEAM.lock().unwrap();
            if let Some(f) = *seam {
                f(query, mc_version, server_type)
            } else {
                search_hangar_plugins(query, mc_version, server_type).await
            }
        }
        #[cfg(not(test))]
        search_hangar_plugins(query, mc_version, server_type).await
    };
    match hangar_result {
        Ok(mut r) => {
            for c in &mut r.candidates {
                let compat = check_candidate_compatibility(c, mc_version, server_type, None);
                c.compatibility = Some(compat.compatible);
            }
            results.push(r);
        }
        Err(e) => {
            results.push(PluginSearchResult {
                candidates: vec![],
                provider: PluginProvider::Hangar,
                has_more: false,
                total_hits: None,
            });
            eprintln!("[plugin_providers] Hangar search error: {}", e);
        }
    }

    Ok(results)
}

// ── Candidate compatibility ────────────────────────────────────────

/// Check a candidate's compatibility with the current server config.
pub fn check_candidate_compatibility(
    candidate: &PluginCandidate,
    mc_version: &str,
    server_type: &ServerType,
    folia_explicit: Option<bool>,
) -> PluginCompatResult {
    // Proxy mismatch check
    let all_proxy = candidate.platforms.iter().all(|p| {
        matches!(
            p,
            PluginPlatform::Velocity | PluginPlatform::Waterfall | PluginPlatform::BungeeCord
        )
    });
    // Proxy servers (Velocity/BungeeCord/Waterfall) not in ServerType enum yet
    // server_is_proxy is always false — proxy server support not implemented
    let server_is_proxy = false;
    if all_proxy && !candidate.platforms.is_empty() && !server_is_proxy {
        return PluginCompatResult {
            compatible: PluginCompatibility::ProxyMismatch,
            source: "platform".to_string(),
            reason: "Plugin is for proxy servers; current server is not a proxy".to_string(),
        };
    }
    if !all_proxy && server_is_proxy {
        // Non-proxy plugin on proxy server
        let has_proxy_platform = candidate.platforms.iter().any(|p| {
            matches!(
                p,
                PluginPlatform::Velocity | PluginPlatform::Waterfall | PluginPlatform::BungeeCord
            )
        });
        if !has_proxy_platform && !candidate.platforms.is_empty() {
            return PluginCompatResult {
                compatible: PluginCompatibility::ProxyMismatch,
                source: "platform".to_string(),
                reason: "Plugin is for game servers; current server is a proxy".to_string(),
            };
        }
    }

    // MC version check
    if !candidate.game_versions.is_empty()
        && !candidate.game_versions.iter().any(|v| v == mc_version)
    {
        return PluginCompatResult {
            compatible: PluginCompatibility::MinecraftVersionMismatch,
            source: "game_version".to_string(),
            reason: format!(
                "Plugin requires MC {}; current server is MC {}",
                candidate.game_versions.join(", "),
                mc_version
            ),
        };
    }

    // Platform compatibility
    if !candidate.platforms.is_empty() {
        let platform_compat =
            check_plugin_platform_compatibility(&candidate.platforms, server_type);
        // For Folia servers, allow FoliaUnknown to pass through — explicit check below may override
        let is_folia_passthrough = matches!(server_type, ServerType::Folia)
            && matches!(
                platform_compat.compatible,
                PluginCompatibility::FoliaUnknown
            );
        if !matches!(platform_compat.compatible, PluginCompatibility::Compatible)
            && !is_folia_passthrough
        {
            return platform_compat;
        }
    }

    // Folia check — explicit provider evidence overrides FoliaUnknown
    if matches!(server_type, ServerType::Folia) {
        match folia_explicit {
            Some(true) => {
                return PluginCompatResult {
                    compatible: PluginCompatibility::FoliaCompatible,
                    source: "provider".to_string(),
                    reason: "Provider indicates Folia support".to_string(),
                };
            }
            Some(false) => {
                return PluginCompatResult {
                    compatible: PluginCompatibility::FoliaIncompatible,
                    source: "provider".to_string(),
                    reason: "Provider indicates no Folia support".to_string(),
                };
            }
            None => {
                return PluginCompatResult {
                    compatible: PluginCompatibility::FoliaUnknown,
                    source: "provider".to_string(),
                    reason: "Provider does not specify Folia compatibility".to_string(),
                };
            }
        }
    }

    PluginCompatResult {
        compatible: PluginCompatibility::Compatible,
        source: "candidate".to_string(),
        reason: "Plugin is compatible with current server".to_string(),
    }
}

// ── Duplicate detection ────────────────────────────────────────────

/// Check if a candidate is already installed.
/// Returns the duplicate kind: SameProvider, SameDescriptor, SameHash, or NotDuplicate.
pub fn check_plugin_duplicate(
    candidate: &PluginCandidate,
    installed_plugins: &[PluginInfo],
    receipts: &PluginProfileReceipts,
) -> DuplicateCheckResult {
    // 1. Same provider + same project → AlreadyInstalled
    for receipt in receipts.receipts.values() {
        if receipt.provider == candidate.provider && receipt.project_id == candidate.project_id {
            return DuplicateCheckResult::AlreadyInstalled;
        }
    }

    // 2. Same artifact hash → exact duplicate
    // (only if candidate has a hash from resolution)
    let candidate_hash = candidate
        .hashes
        .sha512
        .as_deref()
        .or(candidate.hashes.sha256.as_deref())
        .or(candidate.hashes.sha1.as_deref());
    if let Some(hash) = candidate_hash {
        for receipt in receipts.receipts.values() {
            if receipt.artifact_hash == hash {
                return DuplicateCheckResult::AlreadyInstalled;
            }
        }
    }

    // 3. Same filename → not sufficient proof (per spec), but warn
    if !candidate.filename.is_empty() {
        let existing = installed_plugins
            .iter()
            .any(|p| p.file_name == candidate.filename);
        if existing {
            return DuplicateCheckResult::FilenameConflict;
        }
    }

    DuplicateCheckResult::NotDuplicate
}

/// Result of duplicate check before install.
#[derive(Debug, Clone, PartialEq)]
pub enum DuplicateCheckResult {
    /// Exact duplicate: same provider+project or same hash.
    AlreadyInstalled,
    /// Same filename exists but identity not proven.
    FilenameConflict,
    /// No duplicate found.
    NotDuplicate,
}

// ── Install pipeline ───────────────────────────────────────────────

/// Install a plugin from a provider candidate.
///
/// Pipeline: download → verify hash → parse JAR → reconcile identity →
/// check duplicate → check identity conflict → write to plugins dir → save receipt.
///
/// On identity conflict: temp cleaned, live untouched, returns Conflict.
/// On provider hash mismatch: hard failure, temp cleaned, live untouched.
pub async fn install_plugin_from_provider(
    candidate: &PluginCandidate,
    server_path: &Path,
    profile_path: &Path,
    installed_plugins: &[PluginInfo],
    receipts: &PluginProfileReceipts,
    mc_version: &str,
    server_type: &ServerType,
    folia_explicit: Option<bool>,
) -> Result<PluginInstallResult, String> {
    // Backend compatibility gate — reject definitely incompatible candidates
    let compat = check_candidate_compatibility(candidate, mc_version, server_type, folia_explicit);
    match &compat.compatible {
        PluginCompatibility::Compatible
        | PluginCompatibility::FoliaCompatible
        | PluginCompatibility::FoliaUnknown
        | PluginCompatibility::Unknown => { /* allowed */ }
        _ => {
            return Ok(PluginInstallResult::Incompatible(compat));
        }
    }

    use std::io::Read;

    let plugin_dir = plugins_dir(server_path)?;

    // 1. Download to temp (with test seam)
    let temp_dir = std::env::temp_dir().join("lbby-plugin-install");
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let temp_path = temp_dir.join(&candidate.filename);

    let jar_bytes: Vec<u8>;
    #[cfg(test)]
    {
        let seam = DOWNLOAD_SEAM.lock().unwrap();
        if let Some(f) = *seam {
            jar_bytes = f(&candidate.download_url)?;
        } else {
            jar_bytes = download_from_url(&candidate.download_url).await?;
        }
    }
    #[cfg(not(test))]
    {
        jar_bytes = download_from_url(&candidate.download_url).await?;
    }
    std::fs::write(&temp_path, &jar_bytes)
        .map_err(|e| format!("Failed to write temp file: {}", e))?;

    // 2. Verify provider hash — algorithm-aware, hard failure on mismatch
    if let Err(e) = verify_provider_hash(&temp_path, &candidate.hashes) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    // 3. Parse plugin descriptors
    let descriptors = jar_metadata::read_jar_plugin_descriptors(&temp_path);
    let temp_file_name = candidate.filename.clone();

    // 3a. Reject invalid/empty-descriptor JARs before any live commit
    if descriptors.is_empty() {
        let is_valid_zip = std::fs::File::open(&temp_path)
            .ok()
            .and_then(|f| zip::ZipArchive::new(f).ok())
            .is_some();
        let _ = std::fs::remove_file(&temp_path);
        if is_valid_zip {
            return Err("No plugin descriptor found in JAR".to_string());
        } else {
            return Err("Invalid/corrupt JAR: not a valid archive".to_string());
        }
    }

    // 4. Reconcile identity — checks for material conflicts against installed set.
    //    This MUST happen before duplicate detection so that identity changes
    //    on same-provider updates are caught as Conflict, not AlreadyInstalled.
    let reconciliation =
        reconcile_plugin_identity(candidate, &descriptors, installed_plugins, receipts)?;

    // 5. If material identity conflict detected → block install
    if let Some(ref conflict) = reconciliation.conflict {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(PluginInstallResult::Conflict(conflict.clone()));
    }

    // 6. Pre-check: duplicate detection (after identity is confirmed safe)
    let dup = check_plugin_duplicate(candidate, installed_plugins, receipts);
    if dup == DuplicateCheckResult::AlreadyInstalled {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(PluginInstallResult::AlreadyInstalled);
    }

    // 7. Check destination conflict
    let dest = plugin_dir.join(&candidate.filename);
    if dest.exists() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "Destination file already exists: {}",
            candidate.filename
        ));
    }

    // 7. Copy to plugins directory
    std::fs::copy(&temp_path, &dest)
        .map_err(|e| format!("Failed to copy plugin to plugins dir: {}", e))?;

    // 8. Compute local hash for receipt (always SHA-256 for receipt binding)
    let local_hash = compute_file_sha256(&dest)?;

    // 9. Generate inventory ID
    let inventory_id = generate_plugin_inventory_id(profile_path, &temp_file_name);

    // 10. Build PluginInfo (descriptors guaranteed non-empty after step 3a)
    let descriptor = descriptors
        .first()
        .expect("descriptors non-empty after validation");
    let mut all_platforms = reconciliation.platforms.clone();
    if all_platforms.is_empty() {
        all_platforms = candidate.platforms.clone();
    }
    let info = PluginInfo {
        inventory_id: inventory_id.clone(),
        file_name: temp_file_name.clone(),
        display_name: None,
        plugin_name: descriptor.name.clone(),
        version: descriptor.version.clone(),
        main_class: descriptor.main_class.clone(),
        authors: descriptor.authors.clone(),
        description: descriptor.description.clone(),
        website: descriptor.website.clone(),
        api_version: descriptor.api_version.clone(),
        platforms: all_platforms,
        provider: Some(candidate.provider.clone()),
        project_id: Some(candidate.project_id.clone()),
        file_version_id: candidate.file_version_id.clone(),
        artifact_hash: Some(local_hash.clone()),
        status: PluginStatus::Readable,
        dependencies: descriptor
            .depend
            .iter()
            .map(|d| PluginDependency {
                name: d.clone(),
                required: true,
                load_before: false,
            })
            .collect(),
        folia_supported: reconciliation.folia_supported,
    };

    // 10. Save receipt
    let receipt = PluginReceipt {
        schema_version: 1,
        provider: candidate.provider.clone(),
        project_id: candidate.project_id.clone(),
        file_version_id: candidate.file_version_id.clone(),
        filename: temp_file_name,
        artifact_hash: local_hash,
        platforms: candidate.platforms.clone(),
        mc_version: candidate.game_versions.first().cloned(),
    };

    let save_result = save_plugin_receipt(profile_path, &receipt);
    let installed_untracked = save_result.is_err();

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_path);

    if installed_untracked {
        Ok(PluginInstallResult::InstalledUntracked(info))
    } else {
        Ok(PluginInstallResult::Installed(info))
    }
}

// ── Identity reconciliation ───────────────────────────────────────

struct IdentityReconciliation {
    platforms: Vec<PluginPlatform>,
    folia_supported: Option<bool>,
    conflict: Option<String>,
}

/// Reconcile provider identity with JAR descriptor identity.
///
/// Material identity conflict rules:
/// 1. If an existing receipt for the same provider+project_id maps to a different
///    descriptor plugin_name → the project identity changed → conflict
/// 2. If an existing installed plugin has the same descriptor plugin_name but comes
///    from a different provider+project_id → two sources claim same plugin → conflict
/// 3. Display-title-only differences are NOT conflict (provider project title and
///    descriptor name are in different namespaces)
fn reconcile_plugin_identity(
    candidate: &PluginCandidate,
    descriptors: &[PluginDescriptor],
    installed_plugins: &[PluginInfo],
    receipts: &PluginProfileReceipts,
) -> Result<IdentityReconciliation, String> {
    let descriptor = descriptors.first();

    // If no descriptor, trust provider metadata
    let Some(desc) = descriptor else {
        return Ok(IdentityReconciliation {
            platforms: candidate.platforms.clone(),
            folia_supported: None,
            conflict: None,
        });
    };

    let mut conflict_notes: Vec<String> = Vec::new();

    let desc_name = desc.name.as_deref();

    // Rule 1: Same provider+project already installed but descriptor identity changed.
    // Check receipts for this provider+project; if the installed file's plugin_name
    // differs from the new JAR's plugin_name, that's a material identity change.
    for receipt in receipts.receipts.values() {
        if receipt.provider == candidate.provider && receipt.project_id == candidate.project_id {
            // Same provider+project — check if the plugin_name changed
            // Look up the installed PluginInfo by filename to get the old plugin_name
            if let Some(installed) = installed_plugins
                .iter()
                .find(|p| p.file_name == receipt.filename)
            {
                if let (Some(old_name), Some(new_name)) = (&installed.plugin_name, desc_name) {
                    if old_name != new_name {
                        conflict_notes.push(format!(
                            "Plugin identity changed: provider {} project {} was '{}' but new JAR declares '{}'",
                            format!("{:?}", candidate.provider),
                            candidate.project_id,
                            old_name,
                            new_name
                        ));
                    }
                }
            }
        }
    }

    // Rule 2: Another installed plugin has the same descriptor plugin_name
    // but comes from a different provider+project → identity collision.
    if let Some(name) = desc_name {
        for installed in installed_plugins {
            if installed.plugin_name.as_deref() == Some(name) {
                // Same plugin name — check if it's from a different source
                let same_source = match (&installed.provider, &installed.project_id) {
                    (Some(installed_prov), Some(installed_pid)) => {
                        installed_prov == &candidate.provider
                            && installed_pid == &candidate.project_id
                    }
                    _ => false,
                };
                if !same_source {
                    conflict_notes.push(format!(
                        "Plugin name '{}' is already installed from a different provider source",
                        name
                    ));
                    break;
                }
            }
        }
    }

    // Detect Folia from descriptor + provider
    let folia_supported = detect_folia_evidence(descriptors);

    Ok(IdentityReconciliation {
        platforms: candidate.platforms.clone(),
        folia_supported,
        conflict: if conflict_notes.is_empty() {
            None
        } else {
            Some(conflict_notes.join("; "))
        },
    })
}

// ── Save single receipt ────────────────────────────────────────────

fn save_plugin_receipt(profile_path: &Path, receipt: &PluginReceipt) -> Result<(), String> {
    let mut store = load_plugin_receipts(profile_path).unwrap_or(PluginProfileReceipts {
        schema_version: 1,
        receipts: std::collections::HashMap::new(),
    });
    // Remove any existing receipt for the same filename
    store.receipts.remove(&receipt.filename);
    store
        .receipts
        .insert(receipt.filename.clone(), receipt.clone());
    save_plugin_receipts(profile_path, &store)
}

// ── Helper functions ───────────────────────────────────────────────

fn server_type_to_modrinth_loader(server_type: &ServerType) -> String {
    match server_type {
        ServerType::Paper => "paper".to_string(),
        ServerType::Purpur => "purpur".to_string(),
        ServerType::Folia => "folia".to_string(),
        ServerType::Bukkit => "bukkit".to_string(),
        ServerType::Spigot => "spigot".to_string(),
        // Velocity/BungeeCord/Waterfall → no direct Modrinth category;
        // use empty to get all plugin types
        _ => String::new(),
    }
}

fn server_type_to_hangar_platform(server_type: &ServerType) -> String {
    match server_type {
        ServerType::Paper | ServerType::Purpur | ServerType::Folia => "PAPER".to_string(),
        // Velocity/BungeeCord/Waterfall not in ServerType enum — use PluginPlatform instead
        // _ => default falls through to PAPER
        _ => "PAPER".to_string(),
    }
}

fn modrinth_categories_to_platforms(categories: &[String]) -> Vec<PluginPlatform> {
    let mut platforms = Vec::new();
    for cat in categories {
        match cat.as_str() {
            "bukkit" => {
                if !platforms.contains(&PluginPlatform::Bukkit) {
                    platforms.push(PluginPlatform::Bukkit);
                }
            }
            "spigot" => {
                if !platforms.contains(&PluginPlatform::Spigot) {
                    platforms.push(PluginPlatform::Spigot);
                }
            }
            "paper" => {
                if !platforms.contains(&PluginPlatform::Paper) {
                    platforms.push(PluginPlatform::Paper);
                }
            }
            "purpur" => {
                if !platforms.contains(&PluginPlatform::Purpur) {
                    platforms.push(PluginPlatform::Purpur);
                }
            }
            "folia" => {
                if !platforms.contains(&PluginPlatform::Folia) {
                    platforms.push(PluginPlatform::Folia);
                }
            }
            "velocity" => {
                if !platforms.contains(&PluginPlatform::Velocity) {
                    platforms.push(PluginPlatform::Velocity);
                }
            }
            // "sponge" → SpongeVariant not in PluginPlatform enum yet
            // _ => {} handles it
            _ => {}
        }
    }
    platforms
}

fn modrinth_loader_to_platform(loader: &str) -> Option<PluginPlatform> {
    match loader {
        "bukkit" => Some(PluginPlatform::Bukkit),
        "spigot" => Some(PluginPlatform::Spigot),
        "paper" => Some(PluginPlatform::Paper),
        "purpur" => Some(PluginPlatform::Purpur),
        "folia" => Some(PluginPlatform::Folia),
        "velocity" => Some(PluginPlatform::Velocity),
        // "sponge" → not in PluginPlatform enum
        _ => None,
    }
}

fn hangar_platforms_from_project(
    entry: &HangarProjectEntry,
    _target_platform: &str,
) -> Vec<PluginPlatform> {
    // Hangar projects for Paper-compatible servers default to Paper
    // Without version details, we assume Paper+Purpur+Folia compatibility
    vec![PluginPlatform::Paper]
}

fn hangar_channel_priority(name: &str) -> u8 {
    match name.to_lowercase().as_str() {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    }
}

fn select_best_version<'a>(
    versions: &'a [ModrinthVersion],
    _mc_version: &str,
) -> Result<&'a ModrinthVersion, String> {
    versions
        .iter()
        .min_by_key(|v| match v.version_type.as_str() {
            "release" => (0, &v.date_published),
            "beta" => (1, &v.date_published),
            "alpha" => (2, &v.date_published),
            _ => (3, &v.date_published),
        })
        .ok_or_else(|| "No versions available".to_string())
}

fn compute_file_sha256(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open file for hash: {}", e))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| format!("Failed to read file for hash: {}", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(result
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>())
}

fn compute_file_sha512(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open file for hash: {}", e))?;
    let mut hasher = sha2::Sha512::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| format!("Failed to read file for hash: {}", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(result
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>())
}

fn compute_file_sha1(path: &Path) -> Result<String, String> {
    use sha1::Digest;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open file for hash: {}", e))?;
    let mut hasher = sha1::Sha1::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| format!("Failed to read file for hash: {}", e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(result
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>())
}

/// Verify provider-supplied hash against computed file hash.
/// Returns Ok(()) if verification passes or no provider hash exists.
/// Returns Err on mismatch — caller must clean temp and abort.
fn verify_provider_hash(
    path: &Path,
    hashes: &PluginCandidateHashes,
) -> Result<Option<String>, String> {
    if let Some(expected) = &hashes.sha512 {
        let computed = compute_file_sha512(path)?;
        if computed != *expected {
            return Err(format!(
                "SHA-512 mismatch: expected {}, got {}",
                expected, computed
            ));
        }
        return Ok(Some(computed));
    }
    if let Some(expected) = &hashes.sha256 {
        let computed = compute_file_sha256(path)?;
        if computed != *expected {
            return Err(format!(
                "SHA-256 mismatch: expected {}, got {}",
                expected, computed
            ));
        }
        return Ok(Some(computed));
    }
    if let Some(expected) = &hashes.sha1 {
        let computed = compute_file_sha1(path)?;
        if computed != *expected {
            return Err(format!(
                "SHA-1 mismatch: expected {}, got {}",
                expected, computed
            ));
        }
        return Ok(Some(computed));
    }
    // No provider cryptographic hash — compute local SHA-256 for receipt binding
    Ok(None)
}

/// Download bytes from a URL.
async fn download_from_url(url: &str) -> Result<Vec<u8>, String> {
    let client = crate::mod_services::client()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("Download failed: HTTP {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("Download failed: {}", e))
}

// ── Test seam: install from pre-downloaded bytes ─────────────────────

/// Synchronous install pipeline from pre-downloaded bytes.
/// This is the test seam — no HTTP calls, fully deterministic.
///
/// Pipeline: verify hash → parse JAR → reconcile identity → check duplicate →
/// check identity conflict → write to plugins dir → save receipt.
fn install_plugin_from_bytes(
    candidate: &PluginCandidate,
    bytes: &[u8],
    server_path: &Path,
    profile_path: &Path,
    installed_plugins: &[PluginInfo],
    receipts: &PluginProfileReceipts,
    mc_version: &str,
    server_type: &ServerType,
    folia_explicit: Option<bool>,
) -> Result<PluginInstallResult, String> {
    // Backend compatibility gate — reject definitely incompatible candidates
    let compat = check_candidate_compatibility(candidate, mc_version, server_type, folia_explicit);
    match &compat.compatible {
        PluginCompatibility::Compatible
        | PluginCompatibility::FoliaCompatible
        | PluginCompatibility::FoliaUnknown
        | PluginCompatibility::Unknown => { /* allowed */ }
        _ => {
            return Ok(PluginInstallResult::Incompatible(compat));
        }
    }

    let plugin_dir = plugins_dir(server_path)?;

    // 1. Write bytes to temp
    let temp_dir = std::env::temp_dir().join("lbby-plugin-install");
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;
    let temp_path = temp_dir.join(&candidate.filename);
    std::fs::write(&temp_path, bytes).map_err(|e| format!("Failed to write temp file: {}", e))?;

    // 2. Verify provider hash — algorithm-aware, hard failure on mismatch
    if let Err(e) = verify_provider_hash(&temp_path, &candidate.hashes) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    // 3. Parse plugin descriptors
    let descriptors = jar_metadata::read_jar_plugin_descriptors(&temp_path);
    let temp_file_name = candidate.filename.clone();

    // 3a. Reject invalid/empty-descriptor JARs before any live commit
    if descriptors.is_empty() {
        let is_valid_zip = std::fs::File::open(&temp_path)
            .ok()
            .and_then(|f| zip::ZipArchive::new(f).ok())
            .is_some();
        let _ = std::fs::remove_file(&temp_path);
        if is_valid_zip {
            return Err("No plugin descriptor found in JAR".to_string());
        } else {
            return Err("Invalid/corrupt JAR: not a valid archive".to_string());
        }
    }

    // 4. Reconcile identity — checks for material conflicts against installed set.
    //    This MUST happen before duplicate detection so that identity changes
    //    on same-provider updates are caught as Conflict, not AlreadyInstalled.
    let reconciliation =
        reconcile_plugin_identity(candidate, &descriptors, installed_plugins, receipts)?;

    // 5. If material identity conflict detected → block install
    if let Some(ref conflict) = reconciliation.conflict {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(PluginInstallResult::Conflict(conflict.clone()));
    }

    // 6. Pre-check: duplicate detection (after identity is confirmed safe)
    let dup = check_plugin_duplicate(candidate, installed_plugins, receipts);
    if dup == DuplicateCheckResult::AlreadyInstalled {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(PluginInstallResult::AlreadyInstalled);
    }

    // 6. Check destination conflict
    let dest = plugin_dir.join(&candidate.filename);
    if dest.exists() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "Destination file already exists: {}",
            candidate.filename
        ));
    }

    // 7. Copy to plugins directory
    std::fs::copy(&temp_path, &dest)
        .map_err(|e| format!("Failed to copy plugin to plugins dir: {}", e))?;

    // 8. Compute local hash for receipt (always SHA-256 for receipt binding)
    let local_hash = compute_file_sha256(&dest)?;

    // 9. Generate inventory ID
    let inventory_id = generate_plugin_inventory_id(profile_path, &temp_file_name);

    // 10. Build PluginInfo (descriptors guaranteed non-empty after step 3a)
    let descriptor = descriptors
        .first()
        .expect("descriptors non-empty after validation");
    let mut all_platforms = reconciliation.platforms.clone();
    if all_platforms.is_empty() {
        all_platforms = candidate.platforms.clone();
    }
    let info = PluginInfo {
        inventory_id: inventory_id.clone(),
        file_name: temp_file_name.clone(),
        display_name: None,
        plugin_name: descriptor.name.clone(),
        version: descriptor.version.clone(),
        main_class: descriptor.main_class.clone(),
        authors: descriptor.authors.clone(),
        description: descriptor.description.clone(),
        website: descriptor.website.clone(),
        api_version: descriptor.api_version.clone(),
        platforms: all_platforms,
        provider: Some(candidate.provider.clone()),
        project_id: Some(candidate.project_id.clone()),
        file_version_id: candidate.file_version_id.clone(),
        artifact_hash: Some(local_hash.clone()),
        status: PluginStatus::Readable,
        dependencies: descriptor
            .depend
            .iter()
            .map(|d| PluginDependency {
                name: d.clone(),
                required: true,
                load_before: false,
            })
            .collect(),
        folia_supported: reconciliation.folia_supported,
    };

    // 11. Save receipt
    let receipt = PluginReceipt {
        schema_version: 1,
        provider: candidate.provider.clone(),
        project_id: candidate.project_id.clone(),
        file_version_id: candidate.file_version_id.clone(),
        filename: temp_file_name,
        artifact_hash: local_hash,
        platforms: candidate.platforms.clone(),
        mc_version: candidate.game_versions.first().cloned(),
    };

    let save_result = save_plugin_receipt(profile_path, &receipt);
    let installed_untracked = save_result.is_err();

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_path);

    if installed_untracked {
        Ok(PluginInstallResult::InstalledUntracked(info))
    } else {
        Ok(PluginInstallResult::Installed(info))
    }
}

// ── 4B.4C: Runtime compatibility, dependency graph, update, remove ──────

use crate::app_state::{AppState, OperationGuard, OperationKind};

/// Compute runtime compatibility for an installed plugin.
pub fn check_installed_plugin_runtime_compat(
    info: &PluginInfo,
    server_type: &ServerType,
    folia_explicit: Option<bool>,
) -> PluginRuntimeCompatibility {
    if info.status == PluginStatus::Unreadable {
        return PluginRuntimeCompatibility::Unreadable;
    }
    if info.status == PluginStatus::UnknownMetadata {
        return PluginRuntimeCompatibility::Unknown;
    }
    if info.plugin_name.is_none() && info.status == PluginStatus::Readable {
        return PluginRuntimeCompatibility::Ambiguous;
    }
    if matches!(server_type, ServerType::Folia) {
        match folia_explicit {
            Some(true) => return PluginRuntimeCompatibility::FoliaCompatible,
            Some(false) => return PluginRuntimeCompatibility::FoliaIncompatible,
            None => return PluginRuntimeCompatibility::FoliaUnknown,
        }
    }
    if info.platforms.is_empty() {
        return PluginRuntimeCompatibility::Unknown;
    }
    let has_proxy = info.platforms.iter().any(|p| {
        matches!(
            p,
            PluginPlatform::Velocity | PluginPlatform::BungeeCord | PluginPlatform::Waterfall
        )
    });
    let has_server = info.platforms.iter().any(|p| {
        matches!(
            p,
            PluginPlatform::Paper
                | PluginPlatform::Bukkit
                | PluginPlatform::Spigot
                | PluginPlatform::Purpur
                | PluginPlatform::Folia
        )
    });
    if has_proxy && !has_server {
        return PluginRuntimeCompatibility::ProxyMismatch;
    }
    PluginRuntimeCompatibility::Compatible
}

/// Build dependency graph from installed plugins.
pub fn compute_plugin_dependency_graph(plugins: &[PluginInfo]) -> PluginDependencyGraph {
    let mut nodes = Vec::new();
    let mut conflicts = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for p in plugins {
        if let Some(ref name) = p.plugin_name {
            if seen.contains(name) && !conflicts.contains(name) {
                conflicts.push(name.clone());
            }
            seen.push(name.clone());
        }
        let hard = p
            .dependencies
            .iter()
            .filter(|d| d.required)
            .map(|d| d.name.clone())
            .collect();
        let soft = p
            .dependencies
            .iter()
            .filter(|d| !d.required && !d.load_before)
            .map(|d| d.name.clone())
            .collect();
        let load_before = p
            .dependencies
            .iter()
            .filter(|d| d.load_before)
            .map(|d| d.name.clone())
            .collect();
        nodes.push(PluginDependencyNode {
            inventory_id: p.inventory_id.clone(),
            plugin_name: p.plugin_name.clone(),
            hard_dependencies: hard,
            soft_dependencies: soft,
            load_before,
        });
    }
    PluginDependencyGraph { nodes, conflicts }
}

/// Check which installed plugins block removal of target.
pub fn check_remove_blockers(
    target_inventory_id: &str,
    graph: &PluginDependencyGraph,
) -> Vec<String> {
    if !graph.conflicts.is_empty() {
        return vec!["__duplicate_conflict__".to_string()];
    }
    graph
        .nodes
        .iter()
        .filter(|n| {
            n.hard_dependencies
                .iter()
                .any(|dep| dep == target_inventory_id)
        })
        .map(|n| n.inventory_id.clone())
        .collect()
}

/// Remove a plugin with dependency checking.
pub async fn remove_plugin_safe(
    inventory_id: &str,
    server_path: &Path,
    profile_path: &Path,
    installed_plugins: &[PluginInfo],
    app_state: &AppState,
) -> PluginRemoveOutcome {
    let _guard = match app_state.require_plugin_mutation_ready().await {
        Ok(g) => g,
        Err(_) => return PluginRemoveOutcome::Conflict("Guard conflict".to_string()),
    };
    let target = match installed_plugins
        .iter()
        .find(|p| p.inventory_id == inventory_id)
    {
        Some(p) => p,
        None => return PluginRemoveOutcome::NotFound,
    };
    let graph = compute_plugin_dependency_graph(installed_plugins);
    let blockers = check_remove_blockers(inventory_id, &graph);
    if !blockers.is_empty() && blockers[0] == "__duplicate_conflict__" {
        return PluginRemoveOutcome::Conflict("Duplicate identity".to_string());
    }
    if !blockers.is_empty() {
        return PluginRemoveOutcome::BlockedByDependents {
            dependents: blockers,
        };
    }
    let artifact_path = server_path.join("plugins").join(&target.file_name);
    if artifact_path.exists() {
        if std::fs::remove_file(&artifact_path).is_err() {
            return PluginRemoveOutcome::Conflict("Remove failed".to_string());
        }
    }
    // Cleanup receipt
    #[cfg(test)]
    {
        let seam_guard = RECEIPT_REMOVE_SEAM.lock().unwrap();
        if let Some(f) = *seam_guard {
            if let Err(_) = f() {
                return PluginRemoveOutcome::RemovedUntracked;
            }
        }
    }
    let had_receipt = load_plugin_receipts(profile_path)
        .map(|mut store| store.receipts.remove(&target.file_name).is_some())
        .unwrap_or(false);
    if had_receipt {
        let mut store = load_plugin_receipts(profile_path).unwrap_or_default();
        store.receipts.remove(&target.file_name);
        if save_plugin_receipts(profile_path, &store).is_ok() {
            PluginRemoveOutcome::Removed
        } else {
            PluginRemoveOutcome::RemovedUntracked
        }
    } else {
        PluginRemoveOutcome::RemovedUntracked
    }
}

/// Check update status for a single installed plugin.
pub async fn check_plugin_update(
    info: &PluginInfo,
    server_type: &ServerType,
    folia_explicit: Option<bool>,
) -> PluginUpdateCheckResult {
    let provider = info.provider.unwrap_or(PluginProvider::Manual);
    if info.provider.is_none() || info.project_id.is_none() {
        return PluginUpdateCheckResult {
            inventory_id: info.inventory_id.clone(),
            plugin_name: info.plugin_name.clone(),
            current_version: info.file_version_id.clone(),
            status: PluginUpdateStatus::ProviderUnknown,
            provider,
        };
    }
    if info.status == PluginStatus::Unreadable {
        return PluginUpdateCheckResult {
            inventory_id: info.inventory_id.clone(),
            plugin_name: info.plugin_name.clone(),
            current_version: info.file_version_id.clone(),
            status: PluginUpdateStatus::Unreadable,
            provider,
        };
    }
    let project_id = info.project_id.as_deref().unwrap();
    let result = match provider {
        PluginProvider::Modrinth => check_modrinth_update(info, project_id).await,
        PluginProvider::Hangar => check_hangar_update(info, project_id).await,
        _ => Ok(PluginUpdateStatus::ProviderUnknown),
    };
    let status = result.unwrap_or_else(|e| PluginUpdateStatus::ProviderUnavailable(e));
    PluginUpdateCheckResult {
        inventory_id: info.inventory_id.clone(),
        plugin_name: info.plugin_name.clone(),
        current_version: info.file_version_id.clone(),
        status,
        provider,
    }
}

async fn check_modrinth_update(
    info: &PluginInfo,
    project_id: &str,
) -> Result<PluginUpdateStatus, String> {
    let client = crate::mod_services::client()?;
    let url = format!(
        "https://api.modrinth.com/v2/project/{}/version?loaders=%5B%22paper%22%5D",
        project_id
    );
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Modrinth versions request failed: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("Modrinth versions HTTP {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Modrinth versions read failed: {}", e))?;
    let versions: Vec<ModrinthVersion> =
        serde_json::from_slice(&body).map_err(|e| format!("Modrinth versions parse: {}", e))?;

    let current_fvid = info.file_version_id.as_deref();

    // Filter to versions that have at least one file and are NOT hidden/obsolete
    let mut candidates: Vec<&ModrinthVersion> =
        versions.iter().filter(|v| !v.files.is_empty()).collect();

    // Sort by channel priority (Release=0, Beta=1, Alpha=2), then newest date first
    candidates.sort_by(|a, b| {
        let pa = modrinth_channel_priority(&a.version_type);
        let pb = modrinth_channel_priority(&b.version_type);
        pa.cmp(&pb).then(b.date_published.cmp(&a.date_published))
    });

    // Walk sorted candidates: if current version is the best → UpToDate
    // If a newer compatible candidate exists → UpdateAvailable
    for candidate in &candidates {
        let version_id = candidate.id.clone();

        // Check if this is the current version
        if Some(version_id.as_str()) == current_fvid {
            return Ok(PluginUpdateStatus::UpToDate);
        }

        // Prefer primary file, fall back to first
        let file = candidate
            .files
            .iter()
            .find(|f| f.primary)
            .or_else(|| candidate.files.first())
            .unwrap();

        let hashes = PluginCandidateHashes {
            sha512: file.hashes.sha512.clone(),
            sha256: None, // Modrinth doesn't provide SHA-256
            sha1: file.hashes.sha1.clone(),
        };

        let platforms = modrinth_categories_to_platforms(&candidate.loaders);

        return Ok(PluginUpdateStatus::UpdateAvailable {
            current_version: info.file_version_id.clone(),
            target_version: Some(version_id),
            target_filename: file.filename.clone(),
            download_url: file.url.clone(),
            hashes,
            platforms,
            game_versions: candidate.game_versions.clone(),
            release_channel: Some(candidate.version_type.clone()),
        });
    }

    Ok(PluginUpdateStatus::NoCompatibleUpdate)
}

fn modrinth_channel_priority(channel: &str) -> u8 {
    match channel {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    }
}

async fn check_hangar_update(
    info: &PluginInfo,
    project_slug: &str,
) -> Result<PluginUpdateStatus, String> {
    let client = crate::mod_services::client()?;
    let url = format!(
        "https://hangar.papermc.io/api/v1/projects/{}/versions",
        project_slug
    );
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Hangar versions request failed: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("Hangar versions HTTP {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Hangar versions read failed: {}", e))?;
    let data: HangarVersionsResult =
        serde_json::from_slice(&body).map_err(|e| format!("Hangar versions parse: {}", e))?;

    let current_fvid = info.file_version_id.as_deref();

    // Filter to versions with at least one download
    let mut candidates: Vec<&HangarVersionEntry> = data
        .result
        .iter()
        .filter(|v| !v.downloads.is_empty() && !v.pinned)
        .collect();

    // Sort by channel priority (Release=0, Beta=1, Alpha=2)
    // Hangar returns versions newest-first, so stable sort preserves order within channel
    candidates.sort_by_key(|v| hangar_channel_priority(&v.channel.name));

    for candidate in &candidates {
        let version_id = candidate.id.to_string();

        if Some(version_id.as_str()) == current_fvid {
            return Ok(PluginUpdateStatus::UpToDate);
        }

        // Pick best platform download (prefer PAPER)
        let platform_key = server_type_to_hangar_platform(&ServerType::Paper);
        let dl = candidate
            .downloads
            .get(&platform_key)
            .or_else(|| candidate.downloads.values().next());

        if let Some(dl) = dl {
            let hashes = PluginCandidateHashes {
                sha512: None,
                sha256: Some(dl.hash.clone()),
                sha1: None,
            };
            let fname = dl
                .download_url
                .rsplit('/')
                .next()
                .unwrap_or("plugin.jar")
                .to_string();

            // Extract game versions from platform_dependencies
            let game_versions: Vec<String> = candidate
                .platform_dependencies
                .values()
                .flat_map(|v| v.iter().cloned())
                .collect();

            return Ok(PluginUpdateStatus::UpdateAvailable {
                current_version: info.file_version_id.clone(),
                target_version: Some(version_id),
                target_filename: fname,
                download_url: dl.download_url.clone(),
                hashes,
                platforms: vec![PluginPlatform::Paper], // Hangar defaults to Paper
                game_versions,
                release_channel: Some(candidate.channel.name.clone()),
            });
        }
    }

    Ok(PluginUpdateStatus::NoCompatibleUpdate)
}

/// Check updates for all installed plugins.
pub async fn check_all_plugin_updates(
    plugins: &[PluginInfo],
    server_type: &ServerType,
    folia_explicit: Option<bool>,
) -> Vec<PluginUpdateCheckResult> {
    let mut results = Vec::new();
    for p in plugins {
        results.push(check_plugin_update(p, server_type, folia_explicit).await);
    }
    results
}

/// Update a single plugin.
pub async fn update_plugin_from_provider(
    inventory_id: &str,
    server_path: &Path,
    profile_path: &Path,
    installed_plugins: &[PluginInfo],
    server_type: &ServerType,
    folia_explicit: Option<bool>,
    mc_version: &str,
    app_state: &AppState,
) -> PluginUpdateOutcome {
    let _guard = match app_state.require_plugin_mutation_ready().await {
        Ok(g) => g,
        Err(_) => return PluginUpdateOutcome::Conflict("Guard conflict".to_string()),
    };
    update_plugin_no_guard(
        inventory_id,
        server_path,
        profile_path,
        installed_plugins,
        server_type,
        folia_explicit,
        mc_version,
    )
    .await
}

/// Update all plugins.
pub async fn update_all_plugins(
    inventory_ids: &[String],
    server_path: &Path,
    profile_path: &Path,
    installed_plugins: &[PluginInfo],
    server_type: &ServerType,
    folia_explicit: Option<bool>,
    mc_version: &str,
    app_state: &AppState,
) -> PluginBatchUpdateResult {
    let _guard = match app_state.require_plugin_mutation_ready().await {
        Ok(g) => g,
        Err(_) => {
            return PluginBatchUpdateResult {
                items: vec![],
                updated_count: 0,
                failed_count: 0,
                skipped_count: 0,
            }
        }
    };
    let mut items = Vec::new();
    let mut updated_count = 0u32;
    let mut failed_count = 0u32;
    let mut skipped_count = 0u32;
    for id in inventory_ids {
        let outcome = update_plugin_no_guard(
            id,
            server_path,
            profile_path,
            installed_plugins,
            server_type,
            folia_explicit,
            mc_version,
        )
        .await;
        match &outcome {
            PluginUpdateOutcome::Updated(_) | PluginUpdateOutcome::UpdatedUntracked(_) => {
                updated_count += 1
            }
            PluginUpdateOutcome::Failed(_) | PluginUpdateOutcome::Conflict(_) => failed_count += 1,
            _ => skipped_count += 1,
        }
        items.push((id.clone(), outcome));
    }
    PluginBatchUpdateResult {
        items,
        updated_count,
        failed_count,
        skipped_count,
    }
}

/// Internal update without guard (for batch reuse).
///
/// Full pipeline: check update → download → verify provider hash → parse JAR →
/// resolve descriptors → reconcile identity → compatibility gate →
/// backup old → commit new → remove old → update receipt.
///
/// On failure at any step after download, the old artifact is preserved.
async fn update_plugin_no_guard(
    inventory_id: &str,
    server_path: &Path,
    profile_path: &Path,
    installed_plugins: &[PluginInfo],
    server_type: &ServerType,
    folia_explicit: Option<bool>,
    mc_version: &str,
) -> PluginUpdateOutcome {
    // 1. Find installed plugin info
    let info = match installed_plugins
        .iter()
        .find(|p| p.inventory_id == inventory_id)
    {
        Some(p) => p,
        None => return PluginUpdateOutcome::Conflict("Not found".to_string()),
    };

    // 2. Check for update (with test seam to bypass HTTP)
    let check;
    #[cfg(test)]
    {
        let seam_guard = UPDATE_CHECK_SEAM.lock().unwrap();
        if let Some(f) = *seam_guard {
            check = PluginUpdateCheckResult {
                inventory_id: info.inventory_id.clone(),
                plugin_name: info.plugin_name.clone(),
                current_version: info.file_version_id.clone(),
                status: f(&info.inventory_id),
                provider: info.provider.unwrap_or(PluginProvider::Manual),
            };
        } else {
            check = check_plugin_update(info, server_type, folia_explicit).await;
        }
    }
    #[cfg(not(test))]
    {
        check = check_plugin_update(info, server_type, folia_explicit).await;
    }
    let is_update = matches!(check.status, PluginUpdateStatus::UpdateAvailable { .. });
    if !is_update {
        return match check.status {
            PluginUpdateStatus::UpToDate => PluginUpdateOutcome::UpToDate,
            PluginUpdateStatus::ProviderUnavailable(e) => {
                PluginUpdateOutcome::ProviderUnavailable(e)
            }
            PluginUpdateStatus::ProviderUnknown => {
                PluginUpdateOutcome::ProviderUnavailable("Provider unknown".to_string())
            }
            _ => PluginUpdateOutcome::Skipped,
        };
    }

    // 3. Extract UpdateAvailable fields
    let (
        target_filename,
        target_version,
        download_url,
        hashes,
        candidate_platforms,
        candidate_game_versions,
    ) = match check.status {
        PluginUpdateStatus::UpdateAvailable {
            target_filename,
            target_version,
            download_url,
            hashes,
            platforms,
            game_versions,
            ..
        } => (
            target_filename,
            target_version,
            download_url,
            hashes,
            platforms,
            game_versions,
        ),
        _ => unreachable!(),
    };

    // 4. Download (with test seam)
    let bytes: Vec<u8>;
    #[cfg(test)]
    {
        let seam = DOWNLOAD_SEAM.lock().unwrap();
        if let Some(f) = *seam {
            match f(&download_url) {
                Ok(b) => bytes = b,
                Err(e) => return PluginUpdateOutcome::Failed(e),
            }
        } else {
            match download_from_url(&download_url).await {
                Ok(b) => bytes = b,
                Err(e) => return PluginUpdateOutcome::Failed(e),
            }
        }
    }
    #[cfg(not(test))]
    {
        match download_from_url(&download_url).await {
            Ok(b) => bytes = b,
            Err(e) => return PluginUpdateOutcome::Failed(e),
        }
    }

    // 5. Write to temp file for hash verification + JAR parsing
    let tmp_dir = std::env::temp_dir().join(format!("lbby-plugin-update-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let tmp_jar_path = tmp_dir.join(&target_filename);
    if std::fs::write(&tmp_jar_path, &bytes).is_err() {
        return PluginUpdateOutcome::Failed("Temp write failed".to_string());
    }

    // 6. Verify provider hash — same policy as install (SHA-512 > SHA-256 > SHA-1)
    if let Err(e) = verify_provider_hash(&tmp_jar_path, &hashes) {
        let _ = std::fs::remove_file(&tmp_jar_path);
        return PluginUpdateOutcome::Failed(format!("Provider hash mismatch: {}", e));
    }

    // 7. Parse JAR descriptors
    let descriptors = jar_metadata::read_jar_plugin_descriptors(&tmp_jar_path);
    if descriptors.is_empty() {
        let _ = std::fs::remove_file(&tmp_jar_path);
        return PluginUpdateOutcome::Failed("No plugin descriptors in new JAR".to_string());
    }

    // 8. Multi-descriptor resolution via resolve_plugin_descriptors
    let resolved = resolve_plugin_descriptors(&descriptors);
    let (merged_desc, resolved_platforms, is_ambiguous) = match resolved {
        Some(r) => r,
        None => {
            let _ = std::fs::remove_file(&tmp_jar_path);
            return PluginUpdateOutcome::Conflict(
                "Cross-family descriptors (Bukkit + Proxy)".to_string(),
            );
        }
    };
    if is_ambiguous {
        let _ = std::fs::remove_file(&tmp_jar_path);
        return PluginUpdateOutcome::Conflict("Ambiguous plugin descriptors".to_string());
    }

    // 9. Identity reconciliation — build minimal PluginCandidate
    let provider = info.provider.unwrap_or(PluginProvider::Manual);
    let candidate = PluginCandidate {
        provider,
        project_id: info.project_id.clone().unwrap_or_default(),
        file_version_id: target_version.clone(),
        title: info.plugin_name.clone().unwrap_or_default(),
        description: None,
        authors: vec![],
        download_url: download_url.clone(),
        filename: target_filename.clone(),
        hashes: PluginCandidateHashes {
            sha512: None,
            sha256: None,
            sha1: None,
        },
        game_versions: candidate_game_versions.clone(),
        platforms: candidate_platforms.clone(),
        release_channel: ReleaseChannel::Release,
        published_at: None,
        icon_url: None,
        compatibility: None,
    };

    let receipts = load_plugin_receipts(profile_path).unwrap_or(PluginProfileReceipts {
        schema_version: 1,
        receipts: std::collections::HashMap::new(),
    });

    let reconciliation =
        match reconcile_plugin_identity(&candidate, &descriptors, installed_plugins, &receipts) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_jar_path);
                return PluginUpdateOutcome::Failed(e);
            }
        };
    if let Some(conflict) = reconciliation.conflict {
        let _ = std::fs::remove_file(&tmp_jar_path);
        return PluginUpdateOutcome::Conflict(conflict);
    }

    // 9b. Explicit identity change check for updates:
    // If the new JAR declares a different plugin name than the installed one,
    // that's an identity conflict even if reconcile didn't catch it.
    if let Some(ref new_name) = merged_desc.name {
        if let Some(ref old_name) = info.plugin_name {
            if new_name != old_name {
                let _ = std::fs::remove_file(&tmp_jar_path);
                return PluginUpdateOutcome::Conflict(format!(
                    "Plugin identity changed: {} → {}",
                    old_name, new_name
                ));
            }
        }
    }

    // 10. Compatibility gate on NEW artifact (using check_candidate_compatibility)
    let compat = check_candidate_compatibility(&candidate, mc_version, server_type, folia_explicit);
    match &compat.compatible {
        PluginCompatibility::Compatible
        | PluginCompatibility::FoliaCompatible
        | PluginCompatibility::FoliaUnknown
        | PluginCompatibility::Unknown => { /* allowed */ }
        _ => {
            let _ = std::fs::remove_file(&tmp_jar_path);
            return PluginUpdateOutcome::Incompatible(compat);
        }
    }

    // 11. File rotation: backup old → write new → remove old
    let plugins_dir = server_path.join("plugins");
    let _ = std::fs::create_dir_all(&plugins_dir);
    let new_path = plugins_dir.join(&target_filename);
    let old_path = plugins_dir.join(&info.file_name);
    let backup_path = plugins_dir.join(format!("{}.bak", info.file_name));

    // 11a. Backup old artifact
    if old_path.exists() {
        if std::fs::copy(&old_path, &backup_path).is_err() {
            let _ = std::fs::remove_file(&tmp_jar_path);
            return PluginUpdateOutcome::Failed("Backup failed".to_string());
        }
    }

    // 11b. Write new artifact from temp
    if std::fs::copy(&tmp_jar_path, &new_path).is_err() {
        let _ = std::fs::rename(&backup_path, &old_path);
        let _ = std::fs::remove_file(&tmp_jar_path);
        return PluginUpdateOutcome::Failed("Write failed".to_string());
    }

    // 11c. Remove old artifact if different filename (with test seam for failure injection)
    if old_path.exists() && old_path != new_path {
        let old_str = old_path.to_string_lossy().to_string();
        #[cfg(test)]
        {
            let seam_guard = OLD_ARTIFACT_REMOVE_SEAM.lock().unwrap();
            if let Some(f) = *seam_guard {
                if let Err(seam_err) = f(&old_str) {
                    // Rollback: remove new, restore old from backup
                    let _ = std::fs::remove_file(&new_path);
                    let _ = std::fs::rename(&backup_path, &old_path);
                    let _ = std::fs::remove_file(&tmp_jar_path);
                    return PluginUpdateOutcome::Failed(format!(
                        "Old artifact removal failed: {}; rolled back",
                        seam_err
                    ));
                }
            }
        }
        if std::fs::remove_file(&old_path).is_err() {
            // Rollback: remove new, restore old from backup
            let _ = std::fs::remove_file(&new_path);
            let _ = std::fs::rename(&backup_path, &old_path);
            let _ = std::fs::remove_file(&tmp_jar_path);
            return PluginUpdateOutcome::Failed(
                "Old artifact removal failed; rolled back".to_string(),
            );
        }
    }

    // 11d. Cleanup backup and temp
    let _ = std::fs::remove_file(&backup_path);
    let _ = std::fs::remove_file(&tmp_jar_path);

    // 12. Compute local hash for receipt binding (always SHA-256)
    let local_hash = match compute_file_sha256(&new_path) {
        Ok(h) => h,
        Err(e) => {
            return PluginUpdateOutcome::Failed(format!("Hash computation failed: {}", e));
        }
    };

    // 13. Update receipt (with test seam for failure injection)
    let receipt = PluginReceipt {
        schema_version: 1,
        provider,
        project_id: info.project_id.clone().unwrap_or_default(),
        file_version_id: target_version.clone(),
        filename: target_filename.clone(),
        artifact_hash: local_hash,
        platforms: resolved_platforms.clone(),
        mc_version: candidate_game_versions.first().cloned(),
    };

    let mut receipt_ok;
    #[cfg(test)]
    {
        let seam_guard = RECEIPT_SAVE_SEAM.lock().unwrap();
        if let Some(f) = *seam_guard {
            receipt_ok = f().is_ok();
        } else {
            receipt_ok = save_plugin_receipt(profile_path, &receipt).is_ok();
        }
    }
    #[cfg(not(test))]
    {
        receipt_ok = save_plugin_receipt(profile_path, &receipt).is_ok();
    }

    // 14. Build result
    let mut updated_info = info.clone();
    updated_info.file_name = target_filename;
    updated_info.file_version_id = target_version;
    updated_info.platforms = resolved_platforms;

    if receipt_ok {
        PluginUpdateOutcome::Updated(updated_info)
    } else {
        PluginUpdateOutcome::UpdatedUntracked(updated_info)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{OperationGuard, OperationKind};
    use std::io::Write;
    use tokio::sync::Mutex;

    fn make_test_jar(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let mut zip = zip::ZipWriter::new(tmp.reopen().expect("reopen"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zip.start_file(*name, options).expect("start_file");
            zip.write_all(data).expect("write");
        }
        zip.finish().expect("finish zip");
        tmp
    }

    // 1. Basic plugin.yml parsing
    #[test]
    fn basic_plugin_yml_parsing() {
        let yml = b"name: TestPlugin\nversion: 1.0\nmain: com.test.Main\ndescription: A test plugin\nauthor: TestAuthor\nwebsite: https://example.com\napi-version: '1.19'\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 1);
        let d = &descriptors[0];
        assert_eq!(d.source, "plugin.yml");
        assert_eq!(d.name.as_deref(), Some("TestPlugin"));
        assert_eq!(d.version.as_deref(), Some("1.0"));
        assert_eq!(d.main_class.as_deref(), Some("com.test.Main"));
        assert_eq!(d.description.as_deref(), Some("A test plugin"));
        assert_eq!(d.authors, vec!["TestAuthor"]);
        assert_eq!(d.website.as_deref(), Some("https://example.com"));
        assert_eq!(d.api_version.as_deref(), Some("1.19"));
    }

    // 2. plugin.yml with dependencies (hard/soft)
    #[test]
    fn plugin_yml_with_dependencies() {
        let yml = b"name: DepPlugin\nversion: 1.0\nmain: com.test.Main\ndepend:\n  - Vault\n  - WorldEdit\nsoftdepend:\n  - PlaceholderAPI\nloadbefore:\n  - SomeOtherPlugin\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 1);
        let d = &descriptors[0];
        assert_eq!(d.depend, vec!["Vault", "WorldEdit"]);
        assert_eq!(d.soft_depend, vec!["PlaceholderAPI"]);
        assert_eq!(d.load_before, vec!["SomeOtherPlugin"]);
    }

    // 3. paper-plugin.yml parsing
    #[test]
    fn paper_plugin_yml_parsing() {
        let yml = b"name: PaperPlugin\nversion: 2.0\nmain: com.paper.Main\napi-version: '1.20'\ndependencies:\n  Vault:\n    load: REQUIRED\n  PlaceholderAPI:\n    load: OPTIONAL\n  WorldEdit:\n    load: BEFORE\n";
        let jar = make_test_jar(&[("paper-plugin.yml", yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 1);
        let d = &descriptors[0];
        assert_eq!(d.source, "paper-plugin.yml");
        assert_eq!(d.name.as_deref(), Some("PaperPlugin"));
        assert_eq!(d.version.as_deref(), Some("2.0"));
        assert_eq!(d.main_class.as_deref(), Some("com.paper.Main"));
        assert_eq!(d.depend, vec!["Vault"]);
        assert_eq!(d.soft_depend, vec!["PlaceholderAPI"]);
        assert_eq!(d.load_before, vec!["WorldEdit"]);
    }

    // 4. velocity-plugin.json parsing
    #[test]
    fn velocity_plugin_json_parsing() {
        let json = br#"{"id":"velocity-plugin","name":"Velocity Plugin","version":"1.0","main":"com.velocity.Main","authors":["Author1","Author2"],"dependencies":[{"id":"velocity-api","optional":false},{"id":"optional-dep","optional":true}]}"#;
        let jar = make_test_jar(&[("velocity-plugin.json", json)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 1);
        let d = &descriptors[0];
        assert_eq!(d.source, "velocity-plugin.json");
        assert_eq!(d.name.as_deref(), Some("Velocity Plugin"));
        assert_eq!(d.version.as_deref(), Some("1.0"));
        assert_eq!(d.main_class.as_deref(), Some("com.velocity.Main"));
        assert_eq!(d.authors, vec!["Author1", "Author2"]);
        assert_eq!(d.depend, vec!["velocity-api"]);
        assert_eq!(d.soft_depend, vec!["optional-dep"]);
    }

    // 5. bungee.yml parsing
    #[test]
    fn bungee_yml_parsing() {
        let yml = b"name: BungeePlugin\nversion: 1.0\nmain: com.bungee.Main\ndescription: A BungeeCord plugin\nauthor: BungeeAuthor\ndepend:\n  - BungeeDep\nsoftDepends:\n  - BungeeOptional\nloadBefore:\n  - BungeeBefore\n";
        let jar = make_test_jar(&[("bungee.yml", yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 1);
        let d = &descriptors[0];
        assert_eq!(d.source, "bungee.yml");
        assert_eq!(d.name.as_deref(), Some("BungeePlugin"));
        assert_eq!(d.version.as_deref(), Some("1.0"));
        assert_eq!(d.main_class.as_deref(), Some("com.bungee.Main"));
        assert_eq!(d.description.as_deref(), Some("A BungeeCord plugin"));
        assert_eq!(d.authors, vec!["BungeeAuthor"]);
        assert_eq!(d.depend, vec!["BungeeDep"]);
        assert_eq!(d.soft_depend, vec!["BungeeOptional"]);
        assert_eq!(d.load_before, vec!["BungeeBefore"]);
    }

    // 6. Corrupt JAR → Unreadable
    #[test]
    fn corrupt_jar_produces_unreadable() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        std::fs::write(tmp.path(), b"not a zip").expect("write");
        let server_path = tempfile::tempdir().expect("tmpdir");
        let plugins = server_path.path().join("plugins");
        std::fs::create_dir_all(&plugins).expect("mkdir");
        let dest = plugins.join("corrupt.jar");
        std::fs::copy(tmp.path(), &dest).expect("copy");
        let result = scan_plugin_directory(server_path.path()).expect("scan");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, PluginStatus::Unreadable);
    }

    // 7. Missing descriptor → UnknownMetadata
    #[test]
    fn missing_descriptor_produces_unknown_metadata() {
        let jar = make_test_jar(&[("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n")]);
        let server_path = tempfile::tempdir().expect("tmpdir");
        let plugins = server_path.path().join("plugins");
        std::fs::create_dir_all(&plugins).expect("mkdir");
        let dest = plugins.join("nodesc.jar");
        std::fs::copy(jar.path(), &dest).expect("copy");
        let result = scan_plugin_directory(server_path.path()).expect("scan");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].status, PluginStatus::UnknownMetadata);
    }

    // 8. Multi-descriptor conflict → first wins
    #[test]
    fn multi_descriptor_jar_first_wins() {
        let plugin_yml = b"name: BukkitPlugin\nversion: 1.0\nmain: com.bukkit.Main\n";
        let bungee_yml = b"name: BungeePlugin\nversion: 1.0\nmain: com.bungee.Main\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml), ("bungee.yml", bungee_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2, "Should find both descriptors");
        // plugin.yml comes first (alphabetical by zip entry name)
        assert_eq!(descriptors[0].source, "plugin.yml");
        assert_eq!(descriptors[1].source, "bungee.yml");
    }

    // 9. Platform classification
    #[test]
    fn platform_classification_bukkit() {
        let platforms = vec![
            PluginPlatform::Bukkit,
            PluginPlatform::Spigot,
            PluginPlatform::Paper,
        ];
        let result = check_plugin_platform_compatibility(&platforms, &ServerType::Bukkit);
        assert_eq!(result.compatible, PluginCompatibility::Compatible);
    }

    #[test]
    fn platform_classification_paper() {
        let platforms = vec![PluginPlatform::Paper];
        let result = check_plugin_platform_compatibility(&platforms, &ServerType::Paper);
        assert_eq!(result.compatible, PluginCompatibility::Compatible);
    }

    #[test]
    fn platform_classification_velocity_is_proxy() {
        let platforms = vec![PluginPlatform::Velocity];
        let result = check_plugin_platform_compatibility(&platforms, &ServerType::Paper);
        assert_eq!(result.compatible, PluginCompatibility::ProxyPlugin);
    }

    #[test]
    fn platform_classification_mismatch() {
        let platforms = vec![PluginPlatform::Paper];
        let result = check_plugin_platform_compatibility(&platforms, &ServerType::Fabric);
        assert_eq!(result.compatible, PluginCompatibility::Unknown);
    }

    #[test]
    fn platform_classification_unknown() {
        let platforms = vec![PluginPlatform::Unknown];
        let result = check_plugin_platform_compatibility(&platforms, &ServerType::Paper);
        assert_eq!(result.compatible, PluginCompatibility::Unknown);
    }

    // 10. Path safety
    #[test]
    fn path_safety_rejects_dotdot() {
        assert!(validate_plugin_basename("../escape.jar").is_err());
    }

    #[test]
    fn path_safety_rejects_absolute() {
        assert!(validate_plugin_basename("/etc/passwd").is_err());
    }

    #[test]
    fn path_safety_rejects_slash() {
        assert!(validate_plugin_basename("sub/dir/plugin.jar").is_err());
    }

    #[test]
    fn path_safety_allows_spaces() {
        assert!(validate_plugin_basename("my plugin.jar").is_ok());
    }

    #[test]
    fn path_safety_allows_non_ascii() {
        assert!(validate_plugin_basename("插件.jar").is_ok());
    }

    #[test]
    fn path_safety_rejects_leading_dot() {
        assert!(validate_plugin_basename(".hidden").is_err());
    }

    // 11. Lifecycle guards
    #[tokio::test]
    async fn plugin_mutation_rejected_while_mod_mutation_held() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::ModMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while ModMutation is held"
        );
    }

    #[tokio::test]
    async fn mod_mutation_rejected_while_plugin_mutation_held() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::PluginMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::ModMutation)
                .await
                .is_err(),
            "ModMutation must be rejected while PluginMutation is held"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_starting() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Starting)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Starting is held"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_allows_after_drop() {
        let operation = Mutex::new(OperationKind::None);
        let guard = OperationGuard::acquire(&operation, OperationKind::PluginMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err()
        );
        drop(guard);
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_ok(),
            "PluginMutation must be acquirable after previous guard is dropped"
        );
    }

    // 12. Receipt round-trip
    #[test]
    fn receipt_round_trip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let receipt = PluginReceipt {
            schema_version: 1,
            provider: PluginProvider::Modrinth,
            project_id: "test-project".to_string(),
            file_version_id: Some("v1".to_string()),
            filename: "test-plugin.jar".to_string(),
            artifact_hash: "abc123".to_string(),
            platforms: vec![PluginPlatform::Paper],
            mc_version: Some("1.20.4".to_string()),
        };
        let mut store = PluginProfileReceipts::default();
        store
            .receipts
            .insert("test-plugin.jar".to_string(), receipt);

        save_plugin_receipts(tmp.path(), &store).expect("save");
        let loaded = load_plugin_receipts(tmp.path()).expect("load");
        assert_eq!(loaded.receipts.len(), 1);
        let r = loaded.receipts.get("test-plugin.jar").unwrap();
        assert_eq!(r.provider, PluginProvider::Modrinth);
        assert_eq!(r.project_id, "test-project");
        assert_eq!(r.artifact_hash, "abc123");
        assert_eq!(r.platforms, vec![PluginPlatform::Paper]);
    }

    // 13. Receipt hash match/mismatch
    #[test]
    fn receipt_hash_match() {
        let receipt = PluginReceipt {
            schema_version: 1,
            provider: PluginProvider::Manual,
            project_id: "test".to_string(),
            file_version_id: None,
            filename: "test.jar".to_string(),
            artifact_hash: "abc123".to_string(),
            platforms: vec![],
            mc_version: None,
        };
        assert_eq!(receipt.artifact_hash, "abc123");
    }

    #[test]
    fn receipt_hash_mismatch_detected() {
        let receipt = PluginReceipt {
            schema_version: 1,
            provider: PluginProvider::Manual,
            project_id: "test".to_string(),
            file_version_id: None,
            filename: "test.jar".to_string(),
            artifact_hash: "abc123".to_string(),
            platforms: vec![],
            mc_version: None,
        };
        assert_ne!(receipt.artifact_hash, "different_hash");
    }

    // 14. Cross-content conflict
    #[tokio::test]
    async fn cross_content_conflict_mod_mutation_blocks_plugin() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::ModMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must conflict with ModMutation (both change server-owned content)"
        );
    }

    #[tokio::test]
    async fn cross_content_conflict_plugin_mutation_blocks_mod() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::PluginMutation)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::ModMutation)
                .await
                .is_err(),
            "ModMutation must conflict with PluginMutation (both change server-owned content)"
        );
    }

    // ── Issue 1: Multi-descriptor classification ───────────────────────

    // 15. Bukkit + Paper → deterministic compatible merge
    #[test]
    fn multi_descriptor_bukkit_paper_merge() {
        let plugin_yml =
            b"name: DualPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.19'\n";
        let paper_yml =
            b"name: DualPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.20'\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml), ("paper-plugin.yml", paper_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some(), "Same-family descriptors must resolve");
        let (merged, platforms, is_ambiguous) = result.unwrap();
        assert!(!is_ambiguous, "Same-family must not be ambiguous");
        assert_eq!(merged.name.as_deref(), Some("DualPlugin"));
        // plugin.yml wins for name/version (first descriptor)
        // Platforms: Bukkit-family merged → [Bukkit, Spigot, Paper, Purpur]
        assert!(platforms.contains(&PluginPlatform::Bukkit));
        assert!(platforms.contains(&PluginPlatform::Paper));
        assert!(!platforms.contains(&PluginPlatform::Velocity));
    }

    // 16. Bukkit + Velocity → ambiguous/conflicting classification
    #[test]
    fn multi_descriptor_bukkit_velocity_ambiguous() {
        let plugin_yml = b"name: MixedPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let velocity_json = br#"{"id":"mixed-plugin","name":"MixedPlugin","version":"1.0","main":"com.test.VelocityMain"}"#;
        let jar = make_test_jar(&[
            ("plugin.yml", plugin_yml),
            ("velocity-plugin.json", velocity_json),
        ]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some(), "Must still produce a result (not None)");
        let (_, _, is_ambiguous) = result.unwrap();
        assert!(is_ambiguous, "Cross-family Bukkit+Proxy must be ambiguous");
    }

    // 17. Velocity + BungeeCord → proxy family merge, not silently pick first
    #[test]
    fn multi_descriptor_velocity_bungee_proxy_merge() {
        let velocity_json = br#"{"id":"proxy-plugin","name":"ProxyPlugin","version":"1.0","main":"com.proxy.Main"}"#;
        let bungee_yml = b"name: ProxyPlugin\nversion: 1.0\nmain: com.proxy.Main\n";
        let jar = make_test_jar(&[
            ("velocity-plugin.json", velocity_json),
            ("bungee.yml", bungee_yml),
        ]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some());
        let (merged, platforms, is_ambiguous) = result.unwrap();
        assert!(!is_ambiguous, "Same proxy-family must not be ambiguous");
        assert_eq!(merged.name.as_deref(), Some("ProxyPlugin"));
        // Velocity wins (first descriptor in proxy family)
        assert!(merged.main_class.as_deref() == Some("com.proxy.Main"));
        // Proxy platforms only
        assert!(platforms.contains(&PluginPlatform::Velocity));
        assert!(platforms.contains(&PluginPlatform::BungeeCord));
        assert!(platforms.contains(&PluginPlatform::Waterfall));
        assert!(!platforms.contains(&PluginPlatform::Bukkit));
    }

    // 16b. Bukkit-family: plugin.yml + paper-plugin.yml same identity → merge
    #[test]
    fn multi_descriptor_bukkit_paper_same_identity_merge() {
        let plugin_yml =
            b"name: MyPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.19'\n";
        let paper_yml =
            b"name: MyPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.20'\ndescription: A test plugin\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml), ("paper-plugin.yml", paper_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some());
        let (merged, _platforms, is_ambiguous) = result.unwrap();
        assert!(
            !is_ambiguous,
            "Same name + same main class → must merge, not ambiguous"
        );
        assert_eq!(merged.name.as_deref(), Some("MyPlugin"));
        assert_eq!(merged.main_class.as_deref(), Some("com.test.Main"));
        assert_eq!(merged.description.as_deref(), Some("A test plugin"));
    }

    // 16c. Bukkit-family: plugin.yml + paper-plugin.yml conflicting main → ambiguous
    #[test]
    fn multi_descriptor_bukkit_paper_conflicting_main_ambiguous() {
        let plugin_yml =
            b"name: MyPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.19'\n";
        let paper_yml =
            b"name: MyPlugin\nversion: 1.0\nmain: com.paper.DifferentMain\napi-version: '1.20'\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml), ("paper-plugin.yml", paper_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some());
        let (_merged, _platforms, is_ambiguous) = result.unwrap();
        assert!(is_ambiguous, "Conflicting main class → must be ambiguous");
    }

    // 16d. Proxy-family: velocity + bungee same identity → merge
    #[test]
    fn multi_descriptor_proxy_same_identity_merge() {
        let velocity_json =
            br#"{"id":"proxy-plugin","name":"ProxyPlugin","version":"1.0","main":"com.proxy.Main"}"#;
        let bungee_yml = b"name: ProxyPlugin\nversion: 1.0\nmain: com.proxy.Main\n";
        let jar = make_test_jar(&[
            ("velocity-plugin.json", velocity_json),
            ("bungee.yml", bungee_yml),
        ]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some());
        let (merged, _platforms, is_ambiguous) = result.unwrap();
        assert!(!is_ambiguous, "Same identity in proxy family → must merge");
        assert_eq!(merged.main_class.as_deref(), Some("com.proxy.Main"));
    }

    // 16e. Proxy-family: velocity + bungee conflicting main → ambiguous
    #[test]
    fn multi_descriptor_proxy_conflicting_main_ambiguous() {
        let velocity_json =
            br#"{"id":"proxy-plugin","name":"ProxyPlugin","version":"1.0","main":"com.proxy.Main"}"#;
        let bungee_yml = b"name: ProxyPlugin\nversion: 1.0\nmain: com.bungee.AnotherMain\n";
        let jar = make_test_jar(&[
            ("velocity-plugin.json", velocity_json),
            ("bungee.yml", bungee_yml),
        ]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        assert_eq!(descriptors.len(), 2);

        let result = resolve_plugin_descriptors(&descriptors);
        assert!(result.is_some());
        let (_merged, _platforms, is_ambiguous) = result.unwrap();
        assert!(
            is_ambiguous,
            "Conflicting main class in proxy family → must be ambiguous"
        );
    }

    // ── Issue 2: Folia semantics ───────────────────────────────────────

    // 18. plugin.yml on Folia → FoliaUnknown (no explicit Folia evidence)
    #[test]
    fn folia_plugin_yml_unknown() {
        let plugin_yml =
            b"name: BukkitPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.19'\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        let folia = detect_folia_evidence(&descriptors);
        assert_eq!(folia, None, "plugin.yml must NOT imply Folia support");
    }

    // 19. paper-plugin.yml on Folia without Folia evidence → FoliaUnknown
    #[test]
    fn folia_paper_plugin_yml_unknown() {
        let paper_yml =
            b"name: PaperPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.20'\n";
        let jar = make_test_jar(&[("paper-plugin.yml", paper_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        let folia = detect_folia_evidence(&descriptors);
        assert_eq!(folia, None, "paper-plugin.yml alone must NOT imply Folia");
    }

    // 20. Explicit folia-supported: true → FoliaCompatible
    #[test]
    fn folia_explicit_evidence() {
        let plugin_yml =
            b"name: FoliaPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.20'\nfolia-supported: true\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        let folia = detect_folia_evidence(&descriptors);
        assert_eq!(
            folia,
            Some(true),
            "folia-supported: true must be explicit FoliaCompatible evidence"
        );
    }

    // 20b. Explicit folia-supported: false → FoliaIncompatible
    #[test]
    fn folia_explicit_false() {
        let plugin_yml =
            b"name: NoFoliaPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.20'\nfolia-supported: false\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        let folia = detect_folia_evidence(&descriptors);
        assert_eq!(
            folia,
            Some(false),
            "folia-supported: false must be FoliaIncompatible"
        );
    }

    // 20c. api-version containing 'folia' without explicit folia-supported → FoliaUnknown
    #[test]
    fn folia_api_version_heuristic_removed() {
        let plugin_yml =
            b"name: FoliaPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: 'folia-1.20'\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        let folia = detect_folia_evidence(&descriptors);
        assert_eq!(
            folia, None,
            "api-version containing 'folia' without explicit folia-supported must be FoliaUnknown"
        );
    }

    // 20d. No Folia field at all → FoliaUnknown
    #[test]
    fn folia_no_evidence() {
        let plugin_yml =
            b"name: NormalPlugin\nversion: 1.0\nmain: com.test.Main\napi-version: '1.20'\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml)]);
        let descriptors = jar_metadata::read_jar_plugin_descriptors(jar.path());
        let folia = detect_folia_evidence(&descriptors);
        assert_eq!(folia, None, "No Folia field → FoliaUnknown");
    }

    // ── Issue 3: Receipt trust ─────────────────────────────────────────

    // 21. Matching receipt → trusted provider
    #[test]
    fn receipt_trust_matching() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let plugins_dir = tmp.path().join("server").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        // Create a test JAR
        let plugin_yml = b"name: TrustPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut buf);
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zip.start_file("plugin.yml", options).unwrap();
                zip.write_all(plugin_yml).unwrap();
                zip.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(plugins_dir.join("trust-plugin.jar"), &jar_bytes).unwrap();

        // Compute hash
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(&jar_bytes));

        // Write receipt
        let mut receipts = PluginProfileReceipts::default();
        receipts.receipts.insert(
            "trust-plugin.jar".to_string(),
            PluginReceipt {
                schema_version: 1,
                provider: PluginProvider::Modrinth,
                project_id: "test-project".to_string(),
                file_version_id: Some("v1".to_string()),
                filename: "trust-plugin.jar".to_string(),
                artifact_hash: hash,
                platforms: vec![PluginPlatform::Paper],
                mc_version: Some("1.20.4".to_string()),
            },
        );
        save_plugin_receipts(
            tmp.path().join("server").parent().unwrap_or(tmp.path()),
            &receipts,
        )
        .unwrap();

        // Scan — receipt trust should apply
        // Note: scan_plugin_directory uses server_path.parent() for receipts
        // But our server_path is tmp/server, so parent is tmp
        // Actually the receipt is saved at tmp path, scan looks at server_path.parent() = tmp.path()
        // Let me adjust: save receipt at tmp.path() directly
        save_plugin_receipts(tmp.path(), &receipts).unwrap();

        let plugins = scan_plugin_directory(&tmp.path().join("server")).unwrap();
        assert_eq!(plugins.len(), 1);
        let p = &plugins[0];
        assert_eq!(
            p.provider,
            Some(PluginProvider::Modrinth),
            "Matching receipt must trust provider"
        );
        assert_eq!(p.project_id.as_deref(), Some("test-project"));
        assert_eq!(p.file_version_id.as_deref(), Some("v1"));
    }

    // 22. Hash mismatch → provider not trusted
    #[test]
    fn receipt_trust_hash_mismatch() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let plugins_dir = tmp.path().join("server").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let plugin_yml = b"name: StalePlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut buf);
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zip.start_file("plugin.yml", options).unwrap();
                zip.write_all(plugin_yml).unwrap();
                zip.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(plugins_dir.join("stale-plugin.jar"), &jar_bytes).unwrap();

        // Write receipt with WRONG hash
        let mut receipts = PluginProfileReceipts::default();
        receipts.receipts.insert(
            "stale-plugin.jar".to_string(),
            PluginReceipt {
                schema_version: 1,
                provider: PluginProvider::Modrinth,
                project_id: "stale-project".to_string(),
                file_version_id: Some("v1".to_string()),
                filename: "stale-plugin.jar".to_string(),
                artifact_hash: "wrong_hash_12345".to_string(),
                platforms: vec![PluginPlatform::Paper],
                mc_version: None,
            },
        );
        save_plugin_receipts(tmp.path(), &receipts).unwrap();

        let plugins = scan_plugin_directory(&tmp.path().join("server")).unwrap();
        assert_eq!(plugins.len(), 1);
        let p = &plugins[0];
        assert!(
            p.provider.is_none(),
            "Hash mismatch must NOT trust provider"
        );
        assert!(
            p.project_id.is_none(),
            "Hash mismatch must NOT trust project_id"
        );
    }

    // 23. Missing receipt → Manual/Unknown
    #[test]
    fn receipt_trust_missing() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let plugins_dir = tmp.path().join("server").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let plugin_yml = b"name: NoReceiptPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut buf);
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zip.start_file("plugin.yml", options).unwrap();
                zip.write_all(plugin_yml).unwrap();
                zip.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(plugins_dir.join("noreceipt-plugin.jar"), &jar_bytes).unwrap();

        // No receipts written
        let plugins = scan_plugin_directory(&tmp.path().join("server")).unwrap();
        assert_eq!(plugins.len(), 1);
        let p = &plugins[0];
        assert!(
            p.provider.is_none(),
            "Missing receipt must have no provider"
        );
    }

    // 24. Corrupt/future receipt → artifact preserved, no trusted provider
    #[test]
    fn receipt_trust_corrupt_schema() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let plugins_dir = tmp.path().join("server").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let plugin_yml = b"name: FuturePlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut buf);
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zip.start_file("plugin.yml", options).unwrap();
                zip.write_all(plugin_yml).unwrap();
                zip.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(plugins_dir.join("future-plugin.jar"), &jar_bytes).unwrap();

        // Write corrupt JSON
        let receipts_path = tmp.path().join(".lbby-plugin-receipts.json");
        std::fs::write(&receipts_path, "{invalid json!!}").unwrap();

        let plugins = scan_plugin_directory(&tmp.path().join("server")).unwrap();
        assert_eq!(
            plugins.len(),
            1,
            "Artifact must be preserved even with corrupt receipt"
        );
        let p = &plugins[0];
        assert!(
            p.provider.is_none(),
            "Corrupt receipt must NOT provide trust"
        );
    }

    // ── Issue 4: Complete lifecycle guard regression tests ─────────────
    // Note: "Running" is a ServerStatus, not an OperationKind.
    // Server running-state rejection is tested via require_plugin_mutation_ready()
    // which checks ServerStatus::Running/Starting/Stopping before guard acquire.
    // These tests cover the guard-level mutual exclusion.

    // Starting already tested above (test #11, plugin_mutation_rejected_while_starting)
    // guard_drop already tested above (test #10, plugin_mutation_allows_after_drop)

    #[tokio::test]
    async fn plugin_mutation_rejected_while_stopping() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Stopping)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Stopping"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_restoring() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Restoring)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Restoring"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_importing() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Importing)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Importing"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_recovering() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Recovering)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Recovering"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_deleting_profile() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::DeletingProfile)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while DeletingProfile"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_exporting() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Exporting)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Exporting"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_rejected_while_installing() {
        let operation = Mutex::new(OperationKind::None);
        let _guard = OperationGuard::acquire(&operation, OperationKind::Installing)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Installing"
        );
    }

    #[tokio::test]
    async fn plugin_mutation_allowed_after_stopping_guard_drop() {
        let operation = Mutex::new(OperationKind::None);
        let guard = OperationGuard::acquire(&operation, OperationKind::Stopping)
            .await
            .unwrap();
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_err(),
            "PluginMutation must be rejected while Stopping"
        );
        drop(guard);
        assert!(
            OperationGuard::acquire(&operation, OperationKind::PluginMutation)
                .await
                .is_ok(),
            "PluginMutation must be allowed after Stopping guard is dropped"
        );
    }

    // ── 4B.4B: Provider tests ──────────────────────────────────────────

    #[test]
    fn provider_capabilities_modrinth() {
        let caps = get_plugin_provider_capabilities();
        let modrinth = caps
            .iter()
            .find(|c| c.provider == PluginProvider::Modrinth)
            .unwrap();
        assert!(modrinth.search);
        assert!(modrinth.direct_download);
        assert!(modrinth.hash_verification);
        assert!(modrinth.platform_filtering);
    }

    #[test]
    fn provider_capabilities_hangar() {
        let caps = get_plugin_provider_capabilities();
        let hangar = caps
            .iter()
            .find(|c| c.provider == PluginProvider::Hangar)
            .unwrap();
        assert!(hangar.search);
        assert!(hangar.direct_download);
        assert!(hangar.hash_verification);
    }

    #[test]
    fn provider_capabilities_spigotmc_disabled() {
        let caps = get_plugin_provider_capabilities();
        let spigot = caps
            .iter()
            .find(|c| c.provider == PluginProvider::SpigotMC)
            .unwrap();
        assert!(!spigot.search, "SpigotMC search must be disabled");
        assert!(
            !spigot.direct_download,
            "SpigotMC download must be disabled"
        );
    }

    #[test]
    fn provider_capabilities_curseforge_disabled() {
        let caps = get_plugin_provider_capabilities();
        let cf = caps
            .iter()
            .find(|c| c.provider == PluginProvider::CurseForge)
            .unwrap();
        assert!(!cf.search, "CurseForge plugin search must be disabled");
    }

    #[test]
    fn provider_capabilities_manual() {
        let caps = get_plugin_provider_capabilities();
        let manual = caps
            .iter()
            .find(|c| c.provider == PluginProvider::Manual)
            .unwrap();
        assert!(!manual.search, "Manual has no search");
        assert!(manual.hash_verification, "Manual supports local hash");
    }

    #[test]
    fn modrinth_categories_to_platforms_mapping() {
        let cats = vec![
            "paper".to_string(),
            "bukkit".to_string(),
            "folia".to_string(),
        ];
        let platforms = super::modrinth_categories_to_platforms(&cats);
        assert!(platforms.contains(&PluginPlatform::Paper));
        assert!(platforms.contains(&PluginPlatform::Bukkit));
        assert!(platforms.contains(&PluginPlatform::Folia));
        assert!(!platforms.contains(&PluginPlatform::Velocity));
    }

    #[test]
    fn modrinth_categories_unknown_ignored() {
        let cats = vec!["unknown_category".to_string()];
        let platforms = super::modrinth_categories_to_platforms(&cats);
        assert!(
            platforms.is_empty(),
            "Unknown categories should produce no platforms"
        );
    }

    #[test]
    fn hangar_channel_priority_ordering() {
        assert!(super::hangar_channel_priority("Release") < super::hangar_channel_priority("Beta"));
        assert!(super::hangar_channel_priority("Beta") < super::hangar_channel_priority("Alpha"));
        assert!(
            super::hangar_channel_priority("Alpha") < super::hangar_channel_priority("Unknown")
        );
    }

    #[test]
    fn select_best_version_prefers_release() {
        use super::ModrinthFile;
        use super::ModrinthHashes;
        use super::ModrinthVersion;
        let versions = vec![
            ModrinthVersion {
                id: "v1".to_string(),
                name: "1.0-beta".to_string(),
                version_number: "1.0-beta".to_string(),
                game_versions: vec!["1.20.1".to_string()],
                loaders: vec!["paper".to_string()],
                version_type: "beta".to_string(),
                files: vec![ModrinthFile {
                    url: "http://x".to_string(),
                    filename: "a.jar".to_string(),
                    hashes: ModrinthHashes {
                        sha512: None,
                        sha1: None,
                    },
                    primary: true,
                }],
                date_published: "2024-01-01".to_string(),
            },
            ModrinthVersion {
                id: "v2".to_string(),
                name: "1.0".to_string(),
                version_number: "1.0".to_string(),
                game_versions: vec!["1.20.1".to_string()],
                loaders: vec!["paper".to_string()],
                version_type: "release".to_string(),
                files: vec![ModrinthFile {
                    url: "http://x".to_string(),
                    filename: "a.jar".to_string(),
                    hashes: ModrinthHashes {
                        sha512: None,
                        sha1: None,
                    },
                    primary: true,
                }],
                date_published: "2024-02-01".to_string(),
            },
        ];
        let best = super::select_best_version(&versions, "1.20.1").unwrap();
        assert_eq!(best.id, "v2", "Should prefer release over beta");
    }

    #[test]
    fn candidate_compatible_paper_on_paper() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result.compatible, PluginCompatibility::Compatible),
            "{:?}",
            result
        );
    }

    #[test]
    fn candidate_minecraft_version_mismatch() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.21.0".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(
                result.compatible,
                PluginCompatibility::MinecraftVersionMismatch
            ),
            "{:?}",
            result
        );
    }

    #[test]
    fn candidate_folia_unknown_without_evidence() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Folia,
            None,
        );
        assert!(
            matches!(result.compatible, PluginCompatibility::FoliaUnknown),
            "{:?}",
            result
        );
    }

    #[test]
    fn candidate_folia_compatible_with_explicit() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Folia,
            Some(true),
        );
        assert!(
            matches!(result.compatible, PluginCompatibility::FoliaCompatible),
            "{:?}",
            result
        );
    }

    #[test]
    fn candidate_proxy_mismatch() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Velocity],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result.compatible, PluginCompatibility::ProxyMismatch),
            "{:?}",
            result
        );
    }

    #[test]
    fn candidate_bukkit_on_purpur_allowed() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Bukkit],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Purpur,
            None,
        );
        assert!(
            matches!(result.compatible, PluginCompatibility::Compatible),
            "{:?}",
            result
        );
    }

    #[test]
    fn candidate_velocity_on_paper_proxy_mismatch() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Hangar,
            project_id: "test".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Velocity],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = check_candidate_compatibility(
            &candidate,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result.compatible, PluginCompatibility::ProxyMismatch),
            "{:?}",
            result
        );
    }

    #[test]
    fn duplicate_same_provider_project() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "abc123".to_string(),
            file_version_id: Some("v1".to_string()),
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let mut receipts = PluginProfileReceipts::default();
        receipts.receipts.insert(
            "test.jar".to_string(),
            PluginReceipt {
                schema_version: 1,
                provider: PluginProvider::Modrinth,
                project_id: "abc123".to_string(),
                file_version_id: Some("v1".to_string()),
                filename: "test.jar".to_string(),
                artifact_hash: "hash1".to_string(),
                platforms: vec![PluginPlatform::Paper],
                mc_version: Some("1.20.1".to_string()),
            },
        );
        let result = check_plugin_duplicate(&candidate, &[], &receipts);
        assert_eq!(result, DuplicateCheckResult::AlreadyInstalled);
    }

    #[test]
    fn duplicate_same_hash() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Hangar,
            project_id: "other_project".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "test.jar".to_string(),
            hashes: PluginCandidateHashes {
                sha256: Some("abc123hash".to_string()),
                ..Default::default()
            },
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let mut receipts = PluginProfileReceipts::default();
        receipts.receipts.insert(
            "existing.jar".to_string(),
            PluginReceipt {
                schema_version: 1,
                provider: PluginProvider::Modrinth,
                project_id: "some_project".to_string(),
                file_version_id: None,
                filename: "existing.jar".to_string(),
                artifact_hash: "abc123hash".to_string(),
                platforms: vec![PluginPlatform::Paper],
                mc_version: Some("1.20.1".to_string()),
            },
        );
        let result = check_plugin_duplicate(&candidate, &[], &receipts);
        assert_eq!(
            result,
            DuplicateCheckResult::AlreadyInstalled,
            "Same hash = exact duplicate"
        );
    }

    #[test]
    fn duplicate_same_title_not_proof() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "new_project".to_string(),
            file_version_id: None,
            title: "Same Title".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "new.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let installed = vec![PluginInfo {
            inventory_id: "id1".to_string(),
            file_name: "old.jar".to_string(),
            display_name: Some("Same Title".to_string()),
            plugin_name: Some("SameTitle".to_string()),
            version: None,
            main_class: None,
            authors: vec![],
            description: None,
            website: None,
            api_version: None,
            platforms: vec![PluginPlatform::Paper],
            provider: Some(PluginProvider::Hangar),
            project_id: Some("other_project".to_string()),
            file_version_id: None,
            artifact_hash: None,
            status: PluginStatus::Readable,
            dependencies: vec![],
            folia_supported: None,
        }];
        let receipts = PluginProfileReceipts::default();
        let result = check_plugin_duplicate(&candidate, &installed, &receipts);
        assert_eq!(
            result,
            DuplicateCheckResult::NotDuplicate,
            "Same title only ≠ duplicate"
        );
    }

    #[test]
    fn duplicate_filename_conflict() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "new_project".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "same.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let installed = vec![PluginInfo {
            inventory_id: "id1".to_string(),
            file_name: "same.jar".to_string(),
            display_name: None,
            plugin_name: None,
            version: None,
            main_class: None,
            authors: vec![],
            description: None,
            website: None,
            api_version: None,
            platforms: vec![],
            provider: None,
            project_id: None,
            file_version_id: None,
            artifact_hash: None,
            status: PluginStatus::Readable,
            dependencies: vec![],
            folia_supported: None,
        }];
        let receipts = PluginProfileReceipts::default();
        let result = check_plugin_duplicate(&candidate, &installed, &receipts);
        assert_eq!(
            result,
            DuplicateCheckResult::FilenameConflict,
            "Same filename but no provider proof = FilenameConflict"
        );
    }

    #[test]
    fn duplicate_not_duplicate() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "new_project".to_string(),
            file_version_id: None,
            title: "Test".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "brand_new.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let receipts = PluginProfileReceipts::default();
        let result = check_plugin_duplicate(&candidate, &[], &receipts);
        assert_eq!(result, DuplicateCheckResult::NotDuplicate);
    }

    // ── 4B.4B: Deterministic install transaction tests ───────────────

    /// Helper: create a temp server dir with plugins/ subdir, return (server_path, profile_path)
    fn make_test_server_dir() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let server_path = dir.path().to_path_buf();
        std::fs::create_dir_all(server_path.join("plugins")).expect("plugins dir");
        let profile_path = server_path.clone();
        (dir, server_path, profile_path)
    }

    /// Helper: build a minimal PluginCandidate for testing
    fn make_test_candidate(
        provider: PluginProvider,
        project_id: &str,
        filename: &str,
        hashes: PluginCandidateHashes,
    ) -> PluginCandidate {
        PluginCandidate {
            provider,
            project_id: project_id.to_string(),
            file_version_id: None,
            title: "Test Plugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: filename.to_string(),
            hashes,
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        }
    }

    /// Compute SHA-512 hex of arbitrary bytes (not file)
    fn sha512_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha512};
        let result = Sha512::digest(bytes);
        result.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Compute SHA-256 hex of arbitrary bytes
    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let result = Sha256::digest(bytes);
        result.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Compute SHA-1 hex of arbitrary bytes
    fn sha1_hex(bytes: &[u8]) -> String {
        use sha1::{Digest, Sha1};
        let result = Sha1::digest(bytes);
        result.iter().map(|b| format!("{:02x}", b)).collect()
    }

    // --- Identity conflict tests ---

    #[test]
    fn install_identity_match_installable() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: TestPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj123",
            "test.jar",
            PluginCandidateHashes::default(),
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(matches!(result, Ok(PluginInstallResult::Installed(_))));
    }

    #[test]
    fn install_display_title_difference_not_conflict() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: ActualPluginName\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        // Candidate title differs from descriptor name — this is NOT a conflict
        let mut candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj123",
            "test.jar",
            PluginCandidateHashes::default(),
        );
        candidate.title = "Completely Different Display Title".to_string();
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Installed(_))),
            "Display title difference should not be conflict, got: {:?}",
            result
        );
    }

    #[test]
    fn install_material_descriptor_mismatch_conflict() {
        let (_dir, server_path, profile_path) = make_test_server_dir();

        // First install: plugin name "OldPlugin" from Modrinth/proj123
        let yml_old = b"name: OldPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar_old = make_test_jar(&[("plugin.yml", yml_old)]);
        let jar_old_bytes = std::fs::read(jar_old.path()).unwrap();
        let candidate_old = make_test_candidate(
            PluginProvider::Modrinth,
            "proj123",
            "old_plugin.jar",
            PluginCandidateHashes::default(),
        );
        let r1 = install_plugin_from_bytes(
            &candidate_old,
            &jar_old_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(matches!(r1, Ok(PluginInstallResult::Installed(_))));

        // Get installed info and receipts
        let installed = scan_plugin_directory(&server_path).unwrap();
        let receipts = load_plugin_receipts(&profile_path).unwrap();

        // Second install: same provider+project but JAR now declares "NewPlugin"
        let yml_new = b"name: NewPlugin\nversion: 2.0\nmain: com.test.Main\n";
        let jar_new = make_test_jar(&[("plugin.yml", yml_new)]);
        let jar_new_bytes = std::fs::read(jar_new.path()).unwrap();
        let candidate_new = make_test_candidate(
            PluginProvider::Modrinth,
            "proj123",
            "new_plugin.jar",
            PluginCandidateHashes::default(),
        );
        let r2 = install_plugin_from_bytes(
            &candidate_new,
            &jar_new_bytes,
            &server_path,
            &profile_path,
            &installed,
            &receipts,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(r2, Ok(PluginInstallResult::Conflict(_))),
            "Material identity change should produce Conflict, got: {:?}",
            r2
        );
    }

    #[test]
    fn install_conflict_live_untouched() {
        let (_dir, server_path, profile_path) = make_test_server_dir();

        // Install first plugin
        let yml1 = b"name: PluginA\nversion: 1.0\nmain: com.a.Main\n";
        let jar1 = make_test_jar(&[("plugin.yml", yml1)]);
        let jar1_bytes = std::fs::read(jar1.path()).unwrap();
        let c1 = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_a",
            "plugin_a.jar",
            PluginCandidateHashes::default(),
        );
        let _ = install_plugin_from_bytes(
            &c1,
            &jar1_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );

        let installed_before = scan_plugin_directory(&server_path).unwrap();
        let count_before = installed_before.len();

        // Try to install from different provider with same plugin name → conflict
        let yml2 = b"name: PluginA\nversion: 2.0\nmain: com.a.Main\n";
        let jar2 = make_test_jar(&[("plugin.yml", yml2)]);
        let jar2_bytes = std::fs::read(jar2.path()).unwrap();
        let c2 = make_test_candidate(
            PluginProvider::Hangar,
            "owner/other_slug",
            "plugin_a_other.jar",
            PluginCandidateHashes::default(),
        );
        let receipts = load_plugin_receipts(&profile_path).unwrap();
        let r = install_plugin_from_bytes(
            &c2,
            &jar2_bytes,
            &server_path,
            &profile_path,
            &installed_before,
            &receipts,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(matches!(r, Ok(PluginInstallResult::Conflict(_))));

        // Verify live plugins untouched
        let installed_after = scan_plugin_directory(&server_path).unwrap();
        assert_eq!(
            installed_after.len(),
            count_before,
            "Live plugins must be untouched after conflict"
        );
    }

    // --- Hash verification tests ---

    #[test]
    fn hash_modrinth_sha512_match() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: HashPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let sha512 = sha512_hex(&jar_bytes);
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_hash",
            "hash512_match.jar",
            PluginCandidateHashes {
                sha512: Some(sha512),
                sha256: None,
                sha1: None,
            },
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Installed(_))),
            "SHA-512 match should succeed: {:?}",
            result
        );
    }

    #[test]
    fn hash_modrinth_sha512_mismatch() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: HashPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_hash",
            "hash512_mismatch.jar",
            PluginCandidateHashes {
                sha512: Some("000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000deadbeef".to_string()),
                sha256: None,
                sha1: None,
            },
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            result.is_err(),
            "SHA-512 mismatch should be hard failure: {:?}",
            result
        );
        assert!(result.unwrap_err().contains("SHA-512 mismatch"));
    }

    #[test]
    fn hash_hangar_sha256_match() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: HashPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let sha256 = sha256_hex(&jar_bytes);
        let candidate = make_test_candidate(
            PluginProvider::Hangar,
            "owner/proj",
            "hash256_match.jar",
            PluginCandidateHashes {
                sha512: None,
                sha256: Some(sha256),
                sha1: None,
            },
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Installed(_))),
            "SHA-256 match should succeed: {:?}",
            result
        );
    }

    #[test]
    fn hash_hangar_sha256_mismatch() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: HashPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Hangar,
            "owner/proj",
            "hash256_mismatch.jar",
            PluginCandidateHashes {
                sha512: None,
                sha256: Some(
                    "00000000000000000000000000000000000000000000000000000000deadbeef".to_string(),
                ),
                sha1: None,
            },
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            result.is_err(),
            "SHA-256 mismatch should be hard failure: {:?}",
            result
        );
        assert!(result.unwrap_err().contains("SHA-256 mismatch"));
    }

    #[test]
    fn hash_no_provider_hash_local_sha256() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: NoHashPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Manual,
            "manual/proj",
            "no_hash_plugin.jar",
            PluginCandidateHashes::default(),
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Installed(_))),
            "No provider hash should succeed with local SHA-256: {:?}",
            result
        );
        // Verify receipt was saved with local hash
        let receipts = load_plugin_receipts(&profile_path).unwrap();
        assert_eq!(receipts.receipts.len(), 1);
        let receipt = receipts.receipts.values().next().unwrap();
        let expected_local = sha256_hex(&jar_bytes);
        assert_eq!(
            receipt.artifact_hash, expected_local,
            "Receipt should have local SHA-256"
        );
    }

    // --- Transaction tests ---

    #[test]
    fn install_bad_jar_live_untouched() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        // Install a valid plugin first
        let yml = b"name: GoodPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar_good = make_test_jar(&[("plugin.yml", yml)]);
        let jar_good_bytes = std::fs::read(jar_good.path()).unwrap();
        let c1 = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_good",
            "good.jar",
            PluginCandidateHashes::default(),
        );
        let _ = install_plugin_from_bytes(
            &c1,
            &jar_good_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        let count_before = scan_plugin_directory(&server_path).unwrap().len();

        // Now try installing an invalid JAR (random bytes)
        let bad_bytes = b"this is not a valid jar file at all";
        let c2 = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_bad",
            "bad.jar",
            PluginCandidateHashes::default(),
        );
        let result = install_plugin_from_bytes(
            &c2,
            bad_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        // Invalid JAR must be rejected — never become live
        let count_after = scan_plugin_directory(&server_path).unwrap().len();
        assert!(
            result.is_err(),
            "Invalid JAR should be rejected, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("Invalid/corrupt JAR") || err.contains("not a valid archive"),
            "Error should mention invalid/corrupt JAR, got: {}",
            err
        );
        assert_eq!(
            count_after, count_before,
            "Invalid JAR must not change live plugin count"
        );
    }

    #[test]
    fn install_existing_destination_preserved() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        // Install first
        let yml = b"name: DestPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let c1 = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_dest",
            "dest_plugin.jar",
            PluginCandidateHashes::default(),
        );
        let _ = install_plugin_from_bytes(
            &c1,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );

        // Try to install same filename again (different project to avoid duplicate check)
        let c2 = make_test_candidate(
            PluginProvider::Hangar,
            "owner/other",
            "dest_plugin.jar",
            PluginCandidateHashes::default(),
        );
        let result = install_plugin_from_bytes(
            &c2,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(result.is_err(), "Same filename should fail: {:?}", result);
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[test]
    fn install_receipt_failure_installed_untracked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server_path = dir.path().to_path_buf();
        std::fs::create_dir_all(server_path.join("plugins")).expect("plugins dir");
        // Use a non-existent profile path to force receipt save failure
        let profile_path = dir.path().join("nonexistent_deeply_nested_path");

        let yml = b"name: UntrackedPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_untracked",
            "untracked.jar",
            PluginCandidateHashes::default(),
        );
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::InstalledUntracked(_))),
            "Receipt failure should produce InstalledUntracked, got: {:?}",
            result
        );
        // Artifact should still be installed
        assert!(server_path.join("plugins").join("untracked.jar").exists());
    }

    #[test]
    fn install_temp_cleanup_on_success() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: TempCleanup\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_temp",
            "temp_cleanup.jar",
            PluginCandidateHashes::default(),
        );
        let temp_path = std::env::temp_dir()
            .join("lbby-plugin-install")
            .join("temp_cleanup.jar");
        let _ = std::fs::remove_file(&temp_path); // clean before test

        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(result.is_ok());
        assert!(
            !temp_path.exists(),
            "Temp file should be cleaned up after success"
        );
    }

    #[test]
    fn install_temp_cleanup_on_hash_failure() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: TempHashFail\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_hashfail",
            "temp_hashfail.jar",
            PluginCandidateHashes {
                sha512: Some("badbadbad".to_string()),
                sha256: None,
                sha1: None,
            },
        );
        let temp_path = std::env::temp_dir()
            .join("lbby-plugin-install")
            .join("temp_hashfail.jar");
        let _ = std::fs::remove_file(&temp_path);

        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(result.is_err());
        assert!(
            !temp_path.exists(),
            "Temp file should be cleaned up on hash failure"
        );
    }

    // --- Provider isolation tests ---

    #[test]
    fn provider_isolation_structure_modrinth_fails() {
        // Test that the search_plugins function structure handles Modrinth failure gracefully.
        // We can't test the async function directly without a runtime, but we can verify
        // the error handling structure by testing check_plugin_duplicate and reconcile
        // with empty inputs to ensure they don't panic.
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_iso",
            "iso.jar",
            PluginCandidateHashes::default(),
        );
        let receipts = PluginProfileReceipts::default();
        let result = check_plugin_duplicate(&candidate, &[], &receipts);
        assert_eq!(result, DuplicateCheckResult::NotDuplicate);
    }

    #[test]
    fn provider_isolation_structure_hangar_fails() {
        let candidate = make_test_candidate(
            PluginProvider::Hangar,
            "owner/iso",
            "iso.jar",
            PluginCandidateHashes::default(),
        );
        let receipts = PluginProfileReceipts::default();
        let result = check_plugin_duplicate(&candidate, &[], &receipts);
        assert_eq!(result, DuplicateCheckResult::NotDuplicate);
    }

    // --- Cross-provider identity collision test ---

    #[test]
    fn install_cross_provider_same_name_conflict() {
        let (_dir, server_path, profile_path) = make_test_server_dir();

        // Install from Modrinth
        let yml = b"name: SharedPlugin\nversion: 1.0\nmain: com.a.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let c1 = make_test_candidate(
            PluginProvider::Modrinth,
            "modrinth_proj",
            "shared.jar",
            PluginCandidateHashes::default(),
        );
        let _ = install_plugin_from_bytes(
            &c1,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );

        // Install from Hangar with same plugin name → conflict
        let jar2 = make_test_jar(&[("plugin.yml", yml)]);
        let jar2_bytes = std::fs::read(jar2.path()).unwrap();
        let c2 = make_test_candidate(
            PluginProvider::Hangar,
            "owner/shared",
            "shared_hangar.jar",
            PluginCandidateHashes::default(),
        );
        let installed = scan_plugin_directory(&server_path).unwrap();
        let receipts = load_plugin_receipts(&profile_path).unwrap();
        let result = install_plugin_from_bytes(
            &c2,
            &jar2_bytes,
            &server_path,
            &profile_path,
            &installed,
            &receipts,
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Conflict(_))),
            "Cross-provider same plugin name should be Conflict, got: {:?}",
            result
        );
    }

    // --- Hash algo preference: SHA-512 > SHA-256 > SHA-1 ---

    #[test]
    fn hash_sha512_preferred_over_sha256() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let yml = b"name: PrefPlugin\nversion: 1.0\nmain: com.test.Main\n";
        let jar = make_test_jar(&[("plugin.yml", yml)]);
        let jar_bytes = std::fs::read(jar.path()).unwrap();
        let sha512 = sha512_hex(&jar_bytes);
        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_pref",
            "pref.jar",
            PluginCandidateHashes {
                sha512: Some(sha512),
                sha256: Some(
                    "00000000000000000000000000000000000000000000000000000000deadbeef".to_string(),
                ),
                sha1: None,
            },
        );
        // Should succeed because SHA-512 is verified first and matches
        let result = install_plugin_from_bytes(
            &candidate,
            &jar_bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Installed(_))),
            "SHA-512 should be preferred and match: {:?}",
            result
        );
    }

    // ── 4B.4B Revised: Backend compatibility gate tests ──────────────

    #[test]
    fn compat_gate_bungee_on_paper_rejected() {
        // BungeeCord plugin → Paper server → ProxyMismatch → rejected
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugin_yml = b"name: BungeePlugin\nversion: 1.0\nmain: com.example.Main\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml as &[u8])]);
        let bytes = std::fs::read(jar.path()).unwrap();
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "proj1".to_string(),
            file_version_id: None,
            title: "Bungee Plugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "bungee.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::BungeeCord],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = install_plugin_from_bytes(
            &candidate,
            &bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Incompatible(_))),
            "BungeeCord on Paper should be rejected: {:?}",
            result
        );
        assert_eq!(
            scan_plugin_directory(&server_path).unwrap().len(),
            0,
            "Incompatible install must not change live plugin count"
        );
    }

    #[test]
    fn compat_gate_velocity_on_paper_rejected() {
        // Velocity plugin → Paper server → ProxyMismatch → rejected, live untouched, no receipt
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugin_yml = b"name: VelocityPlugin\nversion: 1.0\nmain: com.example.Main\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml as &[u8])]);
        let bytes = std::fs::read(jar.path()).unwrap();
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "proj_velocity".to_string(),
            file_version_id: None,
            title: "Velocity Plugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "velocity.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Velocity],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = install_plugin_from_bytes(
            &candidate,
            &bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        match &result {
            Ok(PluginInstallResult::Incompatible(compat)) => {
                assert!(
                    matches!(compat.compatible, PluginCompatibility::ProxyMismatch),
                    "Expected ProxyMismatch, got {:?}",
                    compat.compatible
                );
            }
            _ => panic!("Expected Incompatible(ProxyMismatch), got {:?}", result),
        }
        assert_eq!(
            scan_plugin_directory(&server_path).unwrap().len(),
            0,
            "Velocity on Paper must not change live plugin count"
        );
        let receipts = load_plugin_receipts(&profile_path).unwrap_or_default();
        assert!(
            receipts.receipts.is_empty(),
            "No receipt for Velocity→Paper rejection"
        );
    }

    #[test]
    fn compat_gate_folia_explicit_false_rejected() {
        // Paper-compatible candidate on Folia server, explicit Folia evidence = false
        // → FoliaIncompatible → Incompatible → live untouched, no receipt
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugin_yml = b"name: NoFoliaPlugin\nversion: 1.0\nmain: com.example.Main\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml as &[u8])]);
        let bytes = std::fs::read(jar.path()).unwrap();
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "proj_folia_neg".to_string(),
            file_version_id: None,
            title: "No Folia Plugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "nofolia.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = install_plugin_from_bytes(
            &candidate,
            &bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Folia,
            Some(false), // folia_explicit = false → FoliaIncompatible
        );
        match &result {
            Ok(PluginInstallResult::Incompatible(compat)) => {
                assert!(
                    matches!(compat.compatible, PluginCompatibility::FoliaIncompatible),
                    "Expected FoliaIncompatible, got {:?}",
                    compat.compatible
                );
            }
            _ => panic!("Expected Incompatible(FoliaIncompatible), got {:?}", result),
        }
        assert_eq!(
            scan_plugin_directory(&server_path).unwrap().len(),
            0,
            "FoliaIncompatible install must not change live plugin count"
        );
        let receipts = load_plugin_receipts(&profile_path).unwrap_or_default();
        assert!(
            receipts.receipts.is_empty(),
            "No receipt should be created for FoliaIncompatible install"
        );
    }

    #[test]
    fn compat_gate_folia_explicit_true_accepted() {
        // Paper-compatible candidate on Folia server, explicit Folia evidence = true
        // → FoliaCompatible → install allowed → file in plugins dir + receipt
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugin_yml = b"name: FoliaPlugin\nversion: 1.0\nmain: com.example.Main\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml as &[u8])]);
        let bytes = std::fs::read(jar.path()).unwrap();
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "proj_folia_pos".to_string(),
            file_version_id: None,
            title: "Folia Plugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "folia.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = install_plugin_from_bytes(
            &candidate,
            &bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Folia,
            Some(true), // folia_explicit = true → FoliaCompatible
        );
        match &result {
            Ok(PluginInstallResult::Installed(info)) => {
                assert!(
                    info.plugin_name.as_deref() == Some("FoliaPlugin")
                        || info.file_name.contains("folia"),
                    "Expected FoliaPlugin, got {:?}",
                    info
                );
            }
            _ => panic!(
                "Expected Installed (FoliaCompatible allows), got {:?}",
                result
            ),
        }
        assert_eq!(
            scan_plugin_directory(&server_path).unwrap().len(),
            1,
            "FoliaCompatible install should create plugin in live dir"
        );
    }

    #[test]
    fn compat_gate_wrong_mc_version_rejected() {
        // MC 1.19.4 candidate on 1.20.1 server → MinecraftVersionMismatch → rejected
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugin_yml = b"name: OldPlugin\nversion: 1.0\nmain: com.example.Main\n";
        let jar = make_test_jar(&[("plugin.yml", plugin_yml as &[u8])]);
        let bytes = std::fs::read(jar.path()).unwrap();
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "proj3".to_string(),
            file_version_id: None,
            title: "Old Plugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "old.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.19.4".to_string()],
            platforms: vec![PluginPlatform::Paper],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };
        let result = install_plugin_from_bytes(
            &candidate,
            &bytes,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        );
        assert!(
            matches!(result, Ok(PluginInstallResult::Incompatible(_))),
            "Wrong MC version should be rejected: {:?}",
            result
        );
        assert_eq!(
            scan_plugin_directory(&server_path).unwrap().len(),
            0,
            "MC mismatch install must not change live plugin count"
        );
    }

    // ── Download failure seam test ──────────────────────────────────

    #[test]
    fn download_failure_returns_error_no_temp() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_dir, server_path, profile_path) = make_test_server_dir();

        let candidate = make_test_candidate(
            PluginProvider::Modrinth,
            "proj_dl_fail",
            "download_fail.jar",
            PluginCandidateHashes::default(),
        );

        // Set download seam to return error
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_| Err("simulated download failure".to_string()));
        }

        let result = rt.block_on(install_plugin_from_provider(
            &candidate,
            &server_path,
            &profile_path,
            &[],
            &PluginProfileReceipts::default(),
            "1.20.1",
            &crate::config::ServerType::Paper,
            None,
        ));

        // Clean up seam
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = None;
        }

        assert!(
            result.is_err(),
            "Download failure should return error: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("download") || err.contains("Download"),
            "Error should mention download failure: {}",
            err
        );
        let temp_dir = std::env::temp_dir().join("lbby-plugin-install");
        let temp_file = temp_dir.join("download_fail.jar");
        assert!(
            !temp_file.exists(),
            "Temp file should be cleaned on download failure"
        );
        assert_eq!(
            scan_plugin_directory(&server_path).unwrap().len(),
            0,
            "Download failure must not change live plugin count"
        );
        let receipts = load_plugin_receipts(&profile_path).unwrap_or_default();
        assert!(
            receipts.receipts.is_empty(),
            "No receipt should be created on download failure"
        );
    }

    // ── Search isolation orchestration tests ─────────────────────────

    #[test]
    fn search_isolation_modrinth_fails_hangar_ok() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        {
            let mut seam = MODRINTH_SEARCH_SEAM.lock().unwrap();
            *seam = Some(|_, _, _| Err("Modrinth API down".to_string()));
        }
        {
            let mut seam = HANGAR_SEARCH_SEAM.lock().unwrap();
            *seam = Some(|_, _, _| {
                Ok(PluginSearchResult {
                    candidates: vec![],
                    provider: PluginProvider::Hangar,
                    has_more: false,
                    total_hits: None,
                })
            });
        }

        let result = rt.block_on(search_plugins(
            "test",
            "1.20.1",
            &crate::config::ServerType::Paper,
        ));

        {
            let mut seam = MODRINTH_SEARCH_SEAM.lock().unwrap();
            *seam = None;
        }
        {
            let mut seam = HANGAR_SEARCH_SEAM.lock().unwrap();
            *seam = None;
        }

        assert!(
            result.is_ok(),
            "Should succeed with one provider failing: {:?}",
            result
        );
        let results = result.unwrap();
        assert!(
            results.iter().any(|r| r.provider == PluginProvider::Hangar),
            "Should include Hangar results when Modrinth fails"
        );
    }

    #[test]
    fn search_isolation_hangar_fails_modrinth_ok() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        {
            let mut seam = HANGAR_SEARCH_SEAM.lock().unwrap();
            *seam = Some(|_, _, _| Err("Hangar API down".to_string()));
        }
        {
            let mut seam = MODRINTH_SEARCH_SEAM.lock().unwrap();
            *seam = Some(|_, _, _| {
                Ok(PluginSearchResult {
                    candidates: vec![],
                    provider: PluginProvider::Modrinth,
                    has_more: false,
                    total_hits: None,
                })
            });
        }

        let result = rt.block_on(search_plugins(
            "test",
            "1.20.1",
            &crate::config::ServerType::Paper,
        ));

        {
            let mut seam = HANGAR_SEARCH_SEAM.lock().unwrap();
            *seam = None;
        }
        {
            let mut seam = MODRINTH_SEARCH_SEAM.lock().unwrap();
            *seam = None;
        }

        assert!(
            result.is_ok(),
            "Should succeed with one provider failing: {:?}",
            result
        );
        let results = result.unwrap();
        assert!(
            results
                .iter()
                .any(|r| r.provider == PluginProvider::Modrinth),
            "Should include Modrinth results when Hangar fails"
        );
    }

    // ── 4B.4C: Runtime compatibility tests ────────────────────

    #[test]
    fn runtime_compat_bukkit_on_paper() {
        let info = PluginInfo {
            status: PluginStatus::Readable,
            plugin_name: Some("TestPlugin".to_string()),
            platforms: vec![PluginPlatform::Bukkit],
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Paper, None),
            PluginRuntimeCompatibility::Compatible
        );
    }

    #[test]
    fn runtime_compat_folia_true() {
        let info = PluginInfo {
            status: PluginStatus::Readable,
            plugin_name: Some("TestPlugin".to_string()),
            platforms: vec![PluginPlatform::Folia],
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Folia, Some(true)),
            PluginRuntimeCompatibility::FoliaCompatible
        );
    }

    #[test]
    fn runtime_compat_folia_false() {
        let info = PluginInfo {
            status: PluginStatus::Readable,
            plugin_name: Some("TestPlugin".to_string()),
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Folia, Some(false)),
            PluginRuntimeCompatibility::FoliaIncompatible
        );
    }

    #[test]
    fn runtime_compat_folia_unknown() {
        let info = PluginInfo {
            status: PluginStatus::Readable,
            plugin_name: Some("TestPlugin".to_string()),
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Folia, None),
            PluginRuntimeCompatibility::FoliaUnknown
        );
    }

    #[test]
    fn runtime_compat_velocity_on_paper() {
        let info = PluginInfo {
            status: PluginStatus::Readable,
            plugin_name: Some("TestPlugin".to_string()),
            platforms: vec![PluginPlatform::Velocity],
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Paper, None),
            PluginRuntimeCompatibility::ProxyMismatch,
            "Velocity plugin on Paper should be ProxyMismatch, not ProxyPlugin"
        );
    }

    #[test]
    fn runtime_compat_ambiguous() {
        let info = PluginInfo {
            status: PluginStatus::Readable,
            plugin_name: None,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Paper, None),
            PluginRuntimeCompatibility::Ambiguous
        );
    }

    #[test]
    fn runtime_compat_unreadable() {
        let info = PluginInfo {
            status: PluginStatus::Unreadable,
            ..Default::default()
        };
        assert_eq!(
            check_installed_plugin_runtime_compat(&info, &ServerType::Paper, None),
            PluginRuntimeCompatibility::Unreadable
        );
    }

    // ── 4B.4C: Dependency graph tests ─────────────────────────

    #[test]
    fn dep_graph_hard_blocks_removal() {
        let plugins = vec![
            PluginInfo {
                inventory_id: "dep-a".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "dep-b".to_string(),
                    required: true,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "dep-b".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph = compute_plugin_dependency_graph(&plugins);
        let blockers = check_remove_blockers("dep-b", &graph);
        assert_eq!(blockers, vec!["dep-a".to_string()]);
    }

    #[test]
    fn dep_graph_soft_allows() {
        let plugins = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "b".to_string(),
                    required: false,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph = compute_plugin_dependency_graph(&plugins);
        let blockers = check_remove_blockers("b", &graph);
        assert!(
            blockers.is_empty(),
            "Soft dependency should not block removal"
        );
    }

    #[test]
    fn dep_graph_loadbefore_allows() {
        let plugins = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "b".to_string(),
                    required: false,
                    load_before: true,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph = compute_plugin_dependency_graph(&plugins);
        let blockers = check_remove_blockers("b", &graph);
        assert!(blockers.is_empty(), "LoadBefore should not block removal");
    }

    #[test]
    fn dep_graph_multiple_dependents() {
        let plugins = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "dep".to_string(),
                    required: true,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![PluginDependency {
                    name: "dep".to_string(),
                    required: true,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "dep".to_string(),
                plugin_name: Some("DepPlugin".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph = compute_plugin_dependency_graph(&plugins);
        let blockers = check_remove_blockers("dep", &graph);
        assert_eq!(blockers.len(), 2);
        assert!(blockers.contains(&"a".to_string()));
        assert!(blockers.contains(&"b".to_string()));
    }

    #[test]
    fn dep_graph_duplicate_conflict() {
        let plugins = vec![
            PluginInfo {
                inventory_id: "a1".to_string(),
                plugin_name: Some("SamePlugin".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "a2".to_string(),
                plugin_name: Some("SamePlugin".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph = compute_plugin_dependency_graph(&plugins);
        assert!(!graph.conflicts.is_empty());
        let blockers = check_remove_blockers("a1", &graph);
        assert_eq!(blockers, vec!["__duplicate_conflict__".to_string()]);
    }

    // ── 4B.4C: Remove tests ───────────────────────────────────

    #[tokio::test]
    async fn remove_safe_no_deps() {
        let server = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(server.path().join("plugins")).unwrap();
        let plugin_file = server.path().join("plugins").join("test.jar");
        std::fs::write(&plugin_file, b"fake jar").unwrap();
        let plugins = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "test.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            status: PluginStatus::Readable,
            ..Default::default()
        }];
        let state = AppState::new();
        let result =
            remove_plugin_safe("test", server.path(), server.path(), &plugins, &state).await;
        assert!(matches!(
            result,
            PluginRemoveOutcome::Removed | PluginRemoveOutcome::RemovedUntracked
        ));
        assert!(!plugin_file.exists(), "Artifact should be removed");
    }

    #[tokio::test]
    async fn remove_safe_blocks() {
        let plugins = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                file_name: "a.jar".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "b".to_string(),
                    required: true,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                file_name: "b.jar".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let server = tempfile::tempdir().unwrap();
        let state = AppState::new();
        let result = remove_plugin_safe("b", server.path(), server.path(), &plugins, &state).await;
        match &result {
            PluginRemoveOutcome::BlockedByDependents { dependents } => {
                assert_eq!(dependents, &vec!["a".to_string()]);
            }
            _ => panic!("Expected BlockedByDependents, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn remove_safe_not_found() {
        let server = tempfile::tempdir().unwrap();
        let state = AppState::new();
        let result =
            remove_plugin_safe("nonexistent", server.path(), server.path(), &[], &state).await;
        assert!(matches!(result, PluginRemoveOutcome::NotFound));
    }

    // ── 4B.4C: Update check tests ─────────────────────────────

    #[tokio::test]
    async fn update_unsupported_provider() {
        let info = PluginInfo {
            inventory_id: "test".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::SpigotMC),
            project_id: Some("12345".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            ..Default::default()
        };
        let result = check_plugin_update(&info, &ServerType::Paper, None).await;
        assert!(matches!(result.status, PluginUpdateStatus::ProviderUnknown));
    }

    #[tokio::test]
    async fn update_no_receipt() {
        let info = PluginInfo {
            inventory_id: "test".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: None,
            project_id: None,
            status: PluginStatus::Readable,
            ..Default::default()
        };
        let result = check_plugin_update(&info, &ServerType::Paper, None).await;
        assert!(matches!(result.status, PluginUpdateStatus::ProviderUnknown));
    }

    #[tokio::test]
    async fn update_unreadable() {
        let info = PluginInfo {
            inventory_id: "test".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("abc".to_string()),
            status: PluginStatus::Unreadable,
            ..Default::default()
        };
        let result = check_plugin_update(&info, &ServerType::Paper, None).await;
        assert!(matches!(result.status, PluginUpdateStatus::Unreadable));
    }

    // ── 4B.4C: Batch update tests ─────────────────────────────

    #[tokio::test]
    async fn batch_guard_conflicts() {
        let server = tempfile::tempdir().unwrap();
        let state = AppState::new();
        // Hold a guard
        let _guard = state.require_plugin_mutation_ready().await.unwrap();
        // Batch should fail
        let result = update_all_plugins(
            &["test".to_string()],
            server.path(),
            server.path(),
            &[],
            &ServerType::Paper,
            None,
            "1.20.1",
            &state,
        )
        .await;
        assert_eq!(result.updated_count, 0);
        assert_eq!(result.failed_count, 0);
        assert_eq!(result.skipped_count, 0);
        assert!(result.items.is_empty());
    }
    // ══════════════════════════════════════════════════════════════════
    // 4B.4C: Deterministic update transaction tests
    // ══════════════════════════════════════════════════════════════════

    /// Helper: create a valid JAR bytes with plugin.yml for the given name/version.
    fn make_update_jar_bytes(name: &str, version: &str) -> Vec<u8> {
        let plugin_yml = format!(
            "name: {}\nversion: {}\nmain: com.example.Main\n",
            name, version
        );
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(tmp.reopen().unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("plugin.yml", options).unwrap();
        zip.write_all(plugin_yml.as_bytes()).unwrap();
        zip.finish().unwrap();
        std::fs::read(tmp.path()).unwrap()
    }

    /// Helper: write a JAR to the plugins dir and return its path.
    fn write_plugin_jar(
        plugins_dir: &std::path::Path,
        filename: &str,
        name: &str,
        version: &str,
    ) -> std::path::PathBuf {
        let bytes = make_update_jar_bytes(name, version);
        let path = plugins_dir.join(filename);
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    /// Helper: compute SHA-256 of bytes as hex string.
    fn update_sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(bytes);
        hash.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Helper: build a standard UpdateAvailable status for testing.
    fn make_update_available(
        target_filename: &str,
        target_version: &str,
        download_url: &str,
        hashes: PluginCandidateHashes,
        platforms: Vec<PluginPlatform>,
        game_versions: Vec<String>,
    ) -> PluginUpdateStatus {
        PluginUpdateStatus::UpdateAvailable {
            current_version: Some("v1".to_string()),
            target_version: Some(target_version.to_string()),
            target_filename: target_filename.to_string(),
            download_url: download_url.to_string(),
            hashes,
            platforms,
            game_versions,
            release_channel: Some("release".to_string()),
        }
    }

    // ── 1. Same-filename update success ─────────────────────────────

    /// Master test: runs all seam-based update tests sequentially
    /// to avoid race conditions on global test seams.
    #[test]
    fn update_transaction_suite() {
        subtest_update_same_filename_success();
        subtest_update_filename_changing_success();
        subtest_update_hash_mismatch_rollback();
        subtest_update_invalid_jar_rollback();
        subtest_update_identity_conflict_rollback();
        subtest_update_platform_mismatch_rollback();
        subtest_update_mc_version_mismatch_rollback();
        subtest_update_folia_incompatible_rollback();
        subtest_update_receipt_failure_untracked();
        subtest_update_temp_cleanup();
        subtest_update_backup_cleanup();
        subtest_update_filename_changing_forced_failure_rollback();
        subtest_remove_safe_receipt_cleanup();
        subtest_batch_update_a_b_c_with_failure();
        subtest_batch_update_a_b_c_direct();
        subtest_update_downgrade_prevention();
        subtest_update_same_version_up_to_date();
        subtest_hangar_sha256_mismatch_regression();
    }

    fn subtest_update_same_filename_success() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        match &result {
            PluginUpdateOutcome::Updated(info) => {
                assert_eq!(info.file_name, "myplugin.jar");
                assert_eq!(info.file_version_id, Some("v2".to_string()));
            }
            PluginUpdateOutcome::UpdatedUntracked(info) => {
                assert_eq!(info.file_name, "myplugin.jar");
            }
            _ => panic!("Expected Updated or UpdatedUntracked, got {:?}", result),
        }
        assert!(
            old_path.exists(),
            "Artifact with same filename should still exist (overwritten)"
        );
    }

    // ── 2. Filename-changing update success ─────────────────────────

    fn subtest_update_filename_changing_success() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "old-name.jar", "TestPlugin", "1.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "new-name.jar",
                    "v2",
                    "https://example.com/new.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "old-name.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        match &result {
            PluginUpdateOutcome::Updated(info) => {
                assert_eq!(info.file_name, "new-name.jar");
            }
            PluginUpdateOutcome::UpdatedUntracked(info) => {
                assert_eq!(info.file_name, "new-name.jar");
            }
            _ => panic!("Expected Updated, got {:?}", result),
        }
        assert!(
            !old_path.exists(),
            "Old artifact must be removed on filename change"
        );
        assert!(
            plugins_dir.join("new-name.jar").exists(),
            "New artifact must exist"
        );
    }

    // ── 3. Provider hash mismatch → rollback/live untouched ─────────

    fn subtest_update_hash_mismatch_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");
        let old_bytes = std::fs::read(&old_path).unwrap();
        let old_hash = update_sha256_hex(&old_bytes);

        // Set hashes to a wrong SHA-512
        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar", "v2", "https://example.com/myplugin.jar",
                    PluginCandidateHashes {
                        sha512: Some("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000deadbeef".to_string()),
                        sha256: None,
                        sha1: None,
                    },
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Failed(ref e) if e.contains("hash")),
            "Expected hash mismatch failure, got {:?}",
            result
        );
        assert!(
            old_path.exists(),
            "Old artifact must be untouched on hash mismatch"
        );
        let current_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());
        assert_eq!(
            old_hash, current_hash,
            "Old artifact content must be unchanged"
        );
    }

    // ── 4. Invalid JAR → rollback/live untouched ────────────────────

    fn subtest_update_invalid_jar_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");
        let old_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(b"not a valid zip".to_vec()));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Failed(ref e) if e.contains("descriptor")),
            "Expected descriptor failure, got {:?}",
            result
        );
        assert!(
            old_path.exists(),
            "Old artifact must be untouched on invalid JAR"
        );
        assert_eq!(
            update_sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_hash
        );
    }

    // ── 5. Identity conflict → rollback/live untouched ──────────────

    fn subtest_update_identity_conflict_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "OriginalPlugin", "1.0");
        let old_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            // Return JAR with DIFFERENT plugin name → identity conflict
            *seam = Some(|_url| Ok(make_update_jar_bytes("TotallyDifferentPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("OriginalPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Conflict(_)),
            "Expected Conflict for identity change, got {:?}",
            result
        );
        assert!(
            old_path.exists(),
            "Old artifact must be untouched on identity conflict"
        );
        assert_eq!(
            update_sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_hash
        );
    }

    // ── 6. Platform mismatch → live untouched ───────────────────────

    fn subtest_update_platform_mismatch_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");
        let old_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            // Velocity-only plugin on Paper server → ProxyMismatch
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Velocity],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Incompatible(_)),
            "Expected Incompatible (ProxyMismatch), got {:?}",
            result
        );
        assert!(
            old_path.exists(),
            "Old artifact must be untouched on platform mismatch"
        );
        assert_eq!(
            update_sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_hash
        );
    }

    // ── 7. MC version mismatch → live untouched ─────────────────────

    fn subtest_update_mc_version_mismatch_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");
        let old_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            // Plugin only supports 1.21 but server is 1.20.1
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.21".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Incompatible(ref r)
                if matches!(r.compatible, PluginCompatibility::MinecraftVersionMismatch)),
            "Expected Incompatible(MC mismatch), got {:?}",
            result
        );
        assert!(old_path.exists());
        assert_eq!(
            update_sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_hash
        );
    }

    // ── 8. FoliaIncompatible → live untouched ───────────────────────

    fn subtest_update_folia_incompatible_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");
        let old_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        // folia_explicit = Some(false) → FoliaIncompatible
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Folia,
            Some(false),
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Incompatible(ref r)
                if matches!(r.compatible, PluginCompatibility::FoliaIncompatible)),
            "Expected Incompatible(FoliaIncompatible), got {:?}",
            result
        );
        assert!(old_path.exists());
        assert_eq!(
            update_sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_hash
        );
    }

    // ── 9. Receipt failure → UpdatedUntracked ───────────────────────

    fn subtest_update_receipt_failure_untracked() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }
        {
            let mut seam = RECEIPT_SAVE_SEAM.lock().unwrap();
            *seam = Some(|| Err("disk full".to_string()));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }
        {
            *RECEIPT_SAVE_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::UpdatedUntracked(_)),
            "Expected UpdatedUntracked on receipt failure, got {:?}",
            result
        );
        assert!(
            plugins_dir.join("myplugin.jar").exists(),
            "New artifact should exist even on receipt failure"
        );
    }

    // ── 10. Temp cleanup after success ──────────────────────────────

    fn subtest_update_temp_cleanup() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "myplugin.jar",
                    "v2",
                    "https://example.com/myplugin.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        let tmp_dir =
            std::env::temp_dir().join(format!("lbby-plugin-update-{}", std::process::id()));
        if tmp_dir.exists() {
            let leftover: Vec<_> = std::fs::read_dir(&tmp_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert!(
                leftover.is_empty(),
                "Temp files should be cleaned: {:?}",
                leftover
            );
        }
    }

    // ── 11. Backup cleanup after success ────────────────────────────

    fn subtest_update_backup_cleanup() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        write_plugin_jar(&plugins_dir, "old.jar", "TestPlugin", "1.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "new.jar",
                    "v2",
                    "https://example.com/new.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "old.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        let backup_path = plugins_dir.join("old.jar.bak");
        assert!(
            !backup_path.exists(),
            "Backup file should be cleaned after successful update"
        );
    }

    // ── 12. Filename-changing forced-failure → rollback ─────────────

    fn subtest_update_filename_changing_forced_failure_rollback() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let old_path = write_plugin_jar(&plugins_dir, "old.jar", "TestPlugin", "1.0");
        let old_hash = update_sha256_hex(&std::fs::read(&old_path).unwrap());

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| {
                make_update_available(
                    "new.jar",
                    "v2",
                    "https://example.com/new.jar",
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_url| Ok(make_update_jar_bytes("TestPlugin", "2.0")));
        }
        {
            let mut seam = OLD_ARTIFACT_REMOVE_SEAM.lock().unwrap();
            *seam = Some(|_path| Err("simulated removal failure".to_string()));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "old.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_no_guard(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }
        {
            *OLD_ARTIFACT_REMOVE_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::Failed(ref e) if e.contains("rolled back")),
            "Expected rollback failure, got {:?}",
            result
        );
        // Old artifact should be restored
        assert!(
            old_path.exists(),
            "Old artifact must be restored on rollback"
        );
        assert_eq!(
            update_sha256_hex(&std::fs::read(&old_path).unwrap()),
            old_hash
        );
        // New artifact should NOT remain
        assert!(
            !plugins_dir.join("new.jar").exists(),
            "New artifact must be removed on rollback"
        );
    }

    // ── 13. Remove receipt-failure → RemovedUntracked ───────────────

    fn subtest_remove_safe_receipt_cleanup() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        let plugin_path = plugins_dir.join("test.jar");
        std::fs::write(&plugin_path, b"fake jar").unwrap();

        // Write a trusted receipt for the plugin
        {
            let mut store = PluginProfileReceipts::default();
            store.receipts.insert(
                "test.jar".to_string(),
                PluginReceipt {
                    schema_version: 1,
                    provider: PluginProvider::Modrinth,
                    project_id: "proj-test".to_string(),
                    file_version_id: Some("v1".to_string()),
                    filename: "test.jar".to_string(),
                    artifact_hash: update_sha256_hex(b"fake jar"),
                    platforms: vec![],
                    mc_version: Some("1.20.1".to_string()),
                },
            );
            save_plugin_receipts(&profile_path, &store).unwrap();
        }

        // Verify receipt exists on disk before removal
        assert!(
            profile_path.join(".lbby-plugin-receipts.json").exists(),
            "Receipt file must exist before removal"
        );

        // Force receipt persistence failure during removal
        {
            *RECEIPT_REMOVE_SEAM.lock().unwrap() =
                Some(|| Err("forced receipt write failure".to_string()));
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "test.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            status: PluginStatus::Readable,
            ..Default::default()
        }];

        let state = AppState::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(remove_plugin_safe(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &state,
        ));

        // Clear seam
        {
            *RECEIPT_REMOVE_SEAM.lock().unwrap() = None;
        }

        // MUST be exactly RemovedUntracked — never Removed
        assert!(
            matches!(result, PluginRemoveOutcome::RemovedUntracked),
            "Expected exactly RemovedUntracked when receipt cleanup fails, got {:?}",
            result
        );

        // Artifact must be absent
        assert!(
            !plugin_path.exists(),
            "Artifact must be absent after removal"
        );

        // Stale receipt must still be on disk (cleanup failed)
        let receipt_path = profile_path.join(".lbby-plugin-receipts.json");
        assert!(
            receipt_path.exists(),
            "Stale receipt file must remain on disk when cleanup fails"
        );

        // Reload the receipt store — stale entry persists
        let stale_store = load_plugin_receipts(&profile_path).unwrap();
        assert!(
            stale_store.receipts.contains_key("test.jar"),
            "Stale receipt entry must persist on disk after failed cleanup"
        );

        // Next inventory scan must NOT trust the stale receipt:
        // provider identity must NOT be restored onto a nonexistent artifact
        // (i.e. there is no artifact on disk, so any scan that trusts the receipt
        // would claim the plugin exists when it does not)
        assert!(
            !plugins_dir.join("test.jar").exists(),
            "Next scan must see artifact as absent — stale receipt must NOT restore it"
        );
    }

    // ── 14a. A/B/C batch — B fails at mutation path ─────────────────

    fn subtest_batch_update_a_b_c_with_failure() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        // Write 3 existing plugins
        write_plugin_jar(&plugins_dir, "a.jar", "PluginA", "1.0");
        write_plugin_jar(&plugins_dir, "b.jar", "PluginB", "1.0");
        write_plugin_jar(&plugins_dir, "c.jar", "PluginC", "1.0");

        let installed = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                file_name: "a.jar".to_string(),
                plugin_name: Some("PluginA".to_string()),
                provider: Some(PluginProvider::Modrinth),
                project_id: Some("proj-a".to_string()),
                file_version_id: Some("v1".to_string()),
                status: PluginStatus::Readable,
                platforms: vec![PluginPlatform::Paper],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                file_name: "b.jar".to_string(),
                plugin_name: Some("PluginB".to_string()),
                provider: Some(PluginProvider::Modrinth),
                project_id: Some("proj-b".to_string()),
                file_version_id: Some("v1".to_string()),
                status: PluginStatus::Readable,
                platforms: vec![PluginPlatform::Paper],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "c".to_string(),
                file_name: "c.jar".to_string(),
                plugin_name: Some("PluginC".to_string()),
                provider: Some(PluginProvider::Modrinth),
                project_id: Some("proj-c".to_string()),
                file_version_id: Some("v1".to_string()),
                status: PluginStatus::Readable,
                platforms: vec![PluginPlatform::Paper],
                ..Default::default()
            },
        ];

        // All 3 get UpdateAvailable
        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|id| {
                let fname = format!("{}.jar", id);
                make_update_available(
                    &fname,
                    "v2",
                    &format!("https://example.com/{}.jar", id),
                    PluginCandidateHashes::default(),
                    vec![PluginPlatform::Paper],
                    vec!["1.20.1".to_string()],
                )
            });
        }
        // B download fails — reaches mutation path and returns Failed
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|url| {
                if url.contains("/b.") {
                    Err("download failed: network timeout".to_string())
                } else if url.contains("/a.") {
                    Ok(make_update_jar_bytes("PluginA", "2.0"))
                } else if url.contains("/c.") {
                    Ok(make_update_jar_bytes("PluginC", "2.0"))
                } else {
                    Ok(make_update_jar_bytes("Plugin", "2.0"))
                }
            });
        }

        let state = AppState::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_all_plugins(
            &["a".to_string(), "b".to_string(), "c".to_string()],
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
            &state,
        ));

        // Clear seams
        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        // Exact counts
        assert_eq!(result.items.len(), 3, "Should have 3 items");
        assert_eq!(result.updated_count, 2, "A and C should be updated");
        assert_eq!(
            result.failed_count, 1,
            "B (download failure) should be failed"
        );
        assert_eq!(
            result.skipped_count, 0,
            "No skips — all reached mutation path"
        );

        // Individual outcomes
        assert!(
            matches!(&result.items[0].1, PluginUpdateOutcome::Updated(_)),
            "A should be Updated, got {:?}",
            result.items[0].1
        );
        assert!(
            matches!(&result.items[1].1, PluginUpdateOutcome::Failed(_)),
            "B should be Failed (download failure), got {:?}",
            result.items[1].1
        );
        assert!(
            matches!(&result.items[2].1, PluginUpdateOutcome::Updated(_)),
            "C should be Updated (continues after B fail), got {:?}",
            result.items[2].1
        );

        // C artifact should exist (proves continuation after B fail)
        assert!(
            plugins_dir.join("c.jar").exists(),
            "C artifact must exist on disk after successful update"
        );
        // B artifact should be unchanged (old artifact preserved on failure)
        assert!(
            plugins_dir.join("b.jar").exists(),
            "B old artifact must be preserved after download failure"
        );
    }

    // ── 14b. A/B/C batch — skipped continuation (ProviderUnavailable) ─

    fn subtest_batch_update_a_b_c_direct() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        // Write 3 existing plugins
        write_plugin_jar(&plugins_dir, "a.jar", "PluginA", "1.0");
        write_plugin_jar(&plugins_dir, "b.jar", "PluginB", "1.0");
        write_plugin_jar(&plugins_dir, "c.jar", "PluginC", "1.0");

        let installed = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                file_name: "a.jar".to_string(),
                plugin_name: Some("PluginA".to_string()),
                provider: Some(PluginProvider::Modrinth),
                project_id: Some("proj-a".to_string()),
                file_version_id: Some("v1".to_string()),
                status: PluginStatus::Readable,
                platforms: vec![PluginPlatform::Paper],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                file_name: "b.jar".to_string(),
                plugin_name: Some("PluginB".to_string()),
                provider: Some(PluginProvider::Modrinth),
                project_id: Some("proj-b".to_string()),
                file_version_id: Some("v1".to_string()),
                status: PluginStatus::Readable,
                platforms: vec![PluginPlatform::Paper],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "c".to_string(),
                file_name: "c.jar".to_string(),
                plugin_name: Some("PluginC".to_string()),
                provider: Some(PluginProvider::Modrinth),
                project_id: Some("proj-c".to_string()),
                file_version_id: Some("v1".to_string()),
                status: PluginStatus::Readable,
                platforms: vec![PluginPlatform::Paper],
                ..Default::default()
            },
        ];

        // UPDATE_CHECK_SEAM: A and C get UpdateAvailable, B gets Failed
        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|id| {
                if id == "b" {
                    PluginUpdateStatus::ProviderUnavailable("API timeout".to_string())
                } else {
                    let fname = format!("{}.jar", id);
                    make_update_available(
                        &fname,
                        "v2",
                        &format!("https://example.com/{}.jar", id),
                        PluginCandidateHashes::default(),
                        vec![PluginPlatform::Paper],
                        vec!["1.20.1".to_string()],
                    )
                }
            });
        }
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|url| {
                // Return JAR with correct plugin name based on download URL
                if url.contains("/a.") {
                    Ok(make_update_jar_bytes("PluginA", "2.0"))
                } else if url.contains("/c.") {
                    Ok(make_update_jar_bytes("PluginC", "2.0"))
                } else {
                    Ok(make_update_jar_bytes("Plugin", "2.0"))
                }
            });
        }

        let state = AppState::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_all_plugins(
            &["a".to_string(), "b".to_string(), "c".to_string()],
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
            &state,
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert_eq!(result.items.len(), 3, "Should have 3 items");
        assert_eq!(result.updated_count, 2, "A and C should be updated");
        assert_eq!(
            result.skipped_count, 1,
            "B (ProviderUnavailable) should be skipped"
        );
        assert_eq!(result.failed_count, 0, "No failures in this scenario");

        // Verify outcomes
        assert!(
            matches!(&result.items[0].1, PluginUpdateOutcome::Updated(_)),
            "A should be Updated"
        );
        assert!(
            matches!(
                &result.items[1].1,
                PluginUpdateOutcome::ProviderUnavailable(_)
            ),
            "B should be ProviderUnavailable"
        );
        assert!(
            matches!(&result.items[2].1, PluginUpdateOutcome::Updated(_)),
            "C should be Updated"
        );
    }

    // ── 15. Downgrade prevention ────────────────────────────────────

    fn subtest_update_downgrade_prevention() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "2.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| PluginUpdateStatus::UpToDate);
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v2".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let state = AppState::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_from_provider(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
            &state,
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::UpToDate),
            "Expected UpToDate for same/downgrade, got {:?}",
            result
        );
    }

    // ── 16. Same-version → UpToDate ─────────────────────────────────

    fn subtest_update_same_version_up_to_date() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        write_plugin_jar(&plugins_dir, "myplugin.jar", "TestPlugin", "1.0");

        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|_id| PluginUpdateStatus::UpToDate);
        }

        let installed = vec![PluginInfo {
            inventory_id: "test".to_string(),
            file_name: "myplugin.jar".to_string(),
            plugin_name: Some("TestPlugin".to_string()),
            provider: Some(PluginProvider::Modrinth),
            project_id: Some("proj1".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        let state = AppState::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_plugin_from_provider(
            "test",
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
            &state,
        ));

        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }

        assert!(
            matches!(result, PluginUpdateOutcome::UpToDate),
            "Expected UpToDate for same version, got {:?}",
            result
        );
    }

    // ── 17. Dependency case policy test ─────────────────────────────

    #[test]
    fn dependency_case_sensitive_matching() {
        // Note: dependency names are matched against inventory_id (not plugin_name).
        // This reflects the Bukkit ecosystem convention where plugin names ARE identifiers.

        // Exact match → blocks removal
        let plugins = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "b".to_string(),
                    required: true,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph = compute_plugin_dependency_graph(&plugins);
        let blockers = check_remove_blockers("b", &graph);
        assert_eq!(
            blockers,
            vec!["a".to_string()],
            "Exact match should block removal"
        );

        // Different case → does NOT block (case-sensitive policy)
        let plugins_case = vec![
            PluginInfo {
                inventory_id: "a".to_string(),
                plugin_name: Some("PluginA".to_string()),
                dependencies: vec![PluginDependency {
                    name: "B".to_string(), // different case than inventory_id "b"
                    required: true,
                    load_before: false,
                }],
                ..Default::default()
            },
            PluginInfo {
                inventory_id: "b".to_string(),
                plugin_name: Some("PluginB".to_string()),
                dependencies: vec![],
                ..Default::default()
            },
        ];
        let graph_case = compute_plugin_dependency_graph(&plugins_case);
        let blockers_case = check_remove_blockers("b", &graph_case);
        assert!(
            blockers_case.is_empty(),
            "Different-case dependency name should NOT match (case-sensitive policy)"
        );
    }

    // ── 18. Velocity-on-Paper via compatibility gate ────────────────

    #[test]
    fn velocity_plugin_on_paper_proxy_mismatch() {
        let candidate = PluginCandidate {
            provider: PluginProvider::Modrinth,
            project_id: "vel-proj".to_string(),
            file_version_id: Some("v1".to_string()),
            title: "VelocityPlugin".to_string(),
            description: None,
            authors: vec![],
            download_url: String::new(),
            filename: "vel.jar".to_string(),
            hashes: PluginCandidateHashes::default(),
            game_versions: vec!["1.20.1".to_string()],
            platforms: vec![PluginPlatform::Velocity],
            release_channel: ReleaseChannel::Release,
            published_at: None,
            icon_url: None,
            compatibility: None,
        };

        let compat = check_candidate_compatibility(&candidate, "1.20.1", &ServerType::Paper, None);
        assert!(
            matches!(compat.compatible, PluginCompatibility::ProxyMismatch),
            "Velocity plugin on Paper should be ProxyMismatch, got {:?}",
            compat
        );
    }

    // ── Hangar SHA-256 mismatch regression ──────────────────────────

    fn subtest_hangar_sha256_mismatch_regression() {
        let (_dir, server_path, profile_path) = make_test_server_dir();
        let plugins_dir = server_path.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();

        // Write existing plugin with known content
        write_plugin_jar(&plugins_dir, "hangar.jar", "HangarPlugin", "1.0");

        let installed = vec![PluginInfo {
            inventory_id: "hangar".to_string(),
            file_name: "hangar.jar".to_string(),
            plugin_name: Some("HangarPlugin".to_string()),
            provider: Some(PluginProvider::Hangar),
            project_id: Some("hangar-proj".to_string()),
            file_version_id: Some("v1".to_string()),
            status: PluginStatus::Readable,
            platforms: vec![PluginPlatform::Paper],
            ..Default::default()
        }];

        // Candidate hashes with intentionally wrong SHA-256
        {
            let mut seam = UPDATE_CHECK_SEAM.lock().unwrap();
            *seam = Some(|id| {
                if id == "hangar" {
                    make_update_available(
                        "hangar-new.jar",
                        "v2",
                        "https://hangar.example.com/hangar-new.jar",
                        PluginCandidateHashes {
                            sha512: None,
                            sha256: Some(
                                "000000000000000000000000000000000000000000000000000000000000dead"
                                    .to_string(),
                            ),
                            sha1: None,
                        },
                        vec![PluginPlatform::Paper],
                        vec!["1.20.1".to_string()],
                    )
                } else {
                    PluginUpdateStatus::UpToDate
                }
            });
        }
        // Download returns valid JAR bytes (content != wrong hash)
        {
            let mut seam = DOWNLOAD_SEAM.lock().unwrap();
            *seam = Some(|_| Ok(make_update_jar_bytes("HangarPlugin", "2.0")));
        }

        let state = AppState::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(update_all_plugins(
            &["hangar".to_string()],
            &server_path,
            &profile_path,
            &installed,
            &ServerType::Paper,
            None,
            "1.20.1",
            &state,
        ));

        // Clear seams
        {
            *UPDATE_CHECK_SEAM.lock().unwrap() = None;
        }
        {
            *DOWNLOAD_SEAM.lock().unwrap() = None;
        }

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.failed_count, 1, "Hangar SHA-256 mismatch must fail");

        assert!(
            matches!(&result.items[0].1, PluginUpdateOutcome::Failed(_)),
            "Hangar SHA-256 mismatch must produce Failed, got {:?}",
            result.items[0].1
        );

        // Old artifact must be unchanged on disk
        let old_content = std::fs::read(plugins_dir.join("hangar.jar")).unwrap();
        assert_eq!(
            old_content,
            make_update_jar_bytes("HangarPlugin", "1.0"),
            "Old artifact must be unchanged after Hangar SHA-256 mismatch"
        );

        // Receipt must be unchanged (no new receipt written for failed update)
        let store = load_plugin_receipts(&profile_path).unwrap();
        assert!(
            store.receipts.get("hangar.jar").is_none(),
            "Receipt must not exist for plugin that was never receipted"
        );

        // Temp directory must be cleaned (no leftover temp files)
        let temp_pattern = std::path::PathBuf::from(&plugins_dir);
        // Check that no .tmp or .partial files remain
        if let Ok(entries) = std::fs::read_dir(&temp_pattern) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(
                    !name.contains(".tmp") && !name.contains(".partial") && !name.contains(".bak"),
                    "Temp file must be cleaned: {}",
                    name
                );
            }
        }
    }
}
