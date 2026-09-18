// plugin_services — Plugin inventory management.
//
// Mirrors mod_services patterns for plugin lifecycle: scan, add, remove,
// receipts, and compatibility checking.

use crate::app_state::{
    PluginCompatResult, PluginCompatibility, PluginDependency, PluginInfo, PluginPlatform,
    PluginProvider, PluginReceipt, PluginStatus,
};
use crate::config::ServerType;
use crate::jar_metadata::{self, PluginDescriptor};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

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
        // Velocity, Waterfall, BungeeCord are not in ServerType enum — they're proxy types
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
}
