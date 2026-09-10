// recovery_actions.rs — User-approved, reversible recovery actions.
//
// After a High-confidence crash attribution, the user can approve temporarily
// disabling the suspected mod. This module handles:
// - Action eligibility checks
// - Attribution fingerprint verification
// - Path containment and symlink safety
// - Quarantine move (staging/mods → staging/.lbby-quarantine/mods/)
// - Dependency-impact preflight
// - Protected component rejection
//
// SAFETY INVARIANTS:
// - NO mutation without explicit user approval
// - NO auto-quarantine
// - NO cascade-disable of dependents
// - NO deletion — quarantine is MOVE only
// - NO network calls
// - Quarantine is transaction-scoped, not global

use crate::crash_attribution::{
    CrashAttributionConfidence, CrashAttributionReport, CrashAttributionStatus,
};
use crate::dependency_graph::DependencyGraph;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ── Constants ───────────────────────────────────────────────────────────

/// Maximum number of user-approved recovery actions per validation session.
pub const MAX_USER_RECOVERY_ACTIONS: u8 = 2;

/// Platform/core mod IDs that must never be disabled.
const PROTECTED_MOD_IDS: &[&str] = &[
    "forge",
    "neoforge",
    "fabricloader",
    "fabric-loader",
    "quilt_loader",
    "minecraft",
    "java",
    "fabric",
    "fabric-api",
    "fabric-api-base",
    "fabric-resource-loader-v0",
    "fabric-lifecycle-events-v1",
    "fabric-networking-api-v1",
    "fabric-registry-sync-v0",
    "fabric-model-loading-api-v1",
    "fabric-renderer-api-v1",
    "fabric-rendering-v1",
    "fabric-object-builder-api-v1",
    "fabric-game-rule-api-v1",
    "fabric-content-registries-v0",
    "fabric-message-api-v1",
    "fabric-screen-handler-api-v1",
    "fabric-transfer-api-v1",
    "fabric-convention-tags-v1",
    "fabric-sound-api-v1",
    "fabric-entity-events-v1",
    "fabric-data-generation-api-v1",
    "neoforge-event-hooks",
    "neoforge-registries",
];

// ── Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RecoveryActionKind {
    TemporarilyDisableMod,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RecoveryActionAvailability {
    /// Action available — High confidence, unique culprit, safe to disable.
    Available,
    /// Confidence too low (Medium/Low).
    UnavailableLowConfidence,
    /// Multiple candidates with different mod IDs.
    UnavailableAmbiguous,
    /// Target is a platform/core component.
    UnavailableProtectedComponent,
    /// Other retained mods have REQUIRED dependency on this mod.
    UnavailableDependencyImpact(Vec<String>),
    /// Single JAR provides multiple mods — user must understand full impact.
    UnavailableMultiModJar(Vec<String>),
    /// Attribution report is Unknown.
    UnavailableUnknown,
    /// JAR path missing or not resolvable.
    UnavailableMissingJar,
    /// Multi-provider: multiple JARs claim the same mod ID.
    UnavailableDuplicateProvider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryActionRequest {
    pub action: RecoveryActionKind,
    pub mod_id: String,
    pub jar_path: PathBuf,
    pub attribution_confidence: CrashAttributionConfidence,
    pub attribution_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecoveryActionResult {
    Applied(RecoveryRecord),
    Rejected(String),
    Invalidated(String),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub action: RecoveryActionKind,
    pub mod_id: String,
    pub original_path: PathBuf,
    pub quarantined_path: PathBuf,
    pub sha256: String,
    pub timestamp: String,
}

/// Approval payload from the frontend. Contains stable identifiers only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryApproval {
    pub transaction_id: String,
    pub attribution_fingerprint: String,
    pub action_kind: RecoveryActionKind,
}

/// Audit record for a user-approved recovery action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecoveryRecord {
    pub action_number: u8,
    pub mod_id: String,
    pub jar_filename: String,
    pub attribution_fingerprint: String,
    pub approved: bool,
    pub action_result: String,
    pub subsequent_boot_result: String,
}

// ── Fingerprint ─────────────────────────────────────────────────────────

/// Compute a deterministic fingerprint from stable attribution fields.
///
/// The fingerprint binds: transaction_id + mod_id + canonical jar path +
/// boot attempt number + crash evidence identifiers.
/// Used to detect stale approvals.
pub fn compute_fingerprint(
    transaction_id: &str,
    mod_id: &str,
    jar_path: &Path,
    boot_attempt: u8,
    evidence_ids: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(transaction_id.as_bytes());
    hasher.update(b"|");
    hasher.update(mod_id.as_bytes());
    hasher.update(b"|");
    hasher.update(jar_path.to_string_lossy().as_bytes());
    hasher.update(b"|");
    hasher.update([boot_attempt]);
    hasher.update(b"|");
    for eid in evidence_ids {
        hasher.update(eid.as_bytes());
        hasher.update(b",");
    }
    format!("{:x}", hasher.finalize())
}

/// Verify that an approval fingerprint matches the current attribution state.
pub fn verify_fingerprint(
    approval: &RecoveryApproval,
    report: &CrashAttributionReport,
    transaction_id: &str,
    jar_path: &Path,
    boot_attempt: u8,
) -> Result<(), String> {
    // Use primary_candidate if available, else first candidate
    let candidate = report
        .primary_candidate
        .as_ref()
        .or_else(|| report.candidates.first())
        .ok_or("No candidates in report")?;

    let mod_id = candidate.mod_id.as_deref().unwrap_or("unknown");
    let evidence_ids: Vec<String> = candidate
        .evidence
        .iter()
        .map(|e| format!("{:?}", e.source))
        .collect();

    let expected = compute_fingerprint(
        transaction_id,
        mod_id,
        jar_path,
        boot_attempt,
        &evidence_ids,
    );

    if approval.attribution_fingerprint == expected {
        Ok(())
    } else {
        Err("Attribution state changed since approval — fingerprint mismatch".to_string())
    }
}

// ── Action eligibility ──────────────────────────────────────────────────

/// Check whether a crash attribution report makes a recovery action available.
///
/// Returns `Available` only when ALL conditions are met:
/// - Status is Attributed
/// - Confidence is High
/// - Single unique candidate
/// - Candidate has authoritative jar_path
/// - Target is not a protected component
/// - JAR is not multi-mod (unless explicitly acknowledged)
pub fn check_action_availability(
    report: &CrashAttributionReport,
    jar_to_mod_ids: &std::collections::HashMap<PathBuf, Vec<String>>,
) -> RecoveryActionAvailability {
    // Must be Attributed
    if report.status != CrashAttributionStatus::Attributed {
        return match report.status {
            CrashAttributionStatus::Ambiguous => RecoveryActionAvailability::UnavailableAmbiguous,
            CrashAttributionStatus::Unknown => RecoveryActionAvailability::UnavailableUnknown,
            _ => RecoveryActionAvailability::UnavailableUnknown,
        };
    }

    // Must be High confidence
    if report.confidence != CrashAttributionConfidence::High {
        return RecoveryActionAvailability::UnavailableLowConfidence;
    }

    // Must have exactly one candidate — prefer primary_candidate
    let candidate = match report
        .primary_candidate
        .as_ref()
        .or_else(|| report.candidates.first())
    {
        Some(c) => c,
        None => return RecoveryActionAvailability::UnavailableUnknown,
    };

    if report.candidates.len() > 1 {
        // Check if there are multiple distinct mod_ids
        let mod_ids: HashSet<&str> = report
            .candidates
            .iter()
            .filter_map(|c| c.mod_id.as_deref())
            .collect();
        if mod_ids.len() > 1 {
            return RecoveryActionAvailability::UnavailableAmbiguous;
        }
    }

    // Must have jar_path
    let jar_path = match &candidate.jar_path {
        Some(p) if !p.as_os_str().is_empty() => p.clone(),
        _ => return RecoveryActionAvailability::UnavailableMissingJar,
    };

    // Protected component check
    let mod_id_str = candidate.mod_id.as_deref().unwrap_or("unknown");
    if is_protected_component(mod_id_str) {
        return RecoveryActionAvailability::UnavailableProtectedComponent;
    }

    // Multi-mod JAR check
    if let Some(mod_ids) = jar_to_mod_ids.get(&jar_path) {
        if mod_ids.len() > 1 {
            let others: Vec<String> = mod_ids
                .iter()
                .filter(|id| **id != mod_id_str)
                .cloned()
                .collect();
            if !others.is_empty() {
                return RecoveryActionAvailability::UnavailableMultiModJar(others);
            }
        }
    }

    // Duplicate provider check: are there multiple JARs claiming this mod_id?
    let mod_id_string = candidate.mod_id.clone().unwrap_or_default();
    let provider_count = jar_to_mod_ids
        .values()
        .filter(|ids| ids.contains(&mod_id_string))
        .count();
    if provider_count > 1 {
        return RecoveryActionAvailability::UnavailableDuplicateProvider;
    }

    RecoveryActionAvailability::Available
}

// ── Protected components ────────────────────────────────────────────────

/// Check if a mod_id is a platform/core component that must never be disabled.
pub fn is_protected_component(mod_id: &str) -> bool {
    let lower = mod_id.to_lowercase();
    PROTECTED_MOD_IDS
        .iter()
        .any(|protected| *protected == lower)
}

// ── Path containment ────────────────────────────────────────────────────

/// Validate that a JAR path is contained within the staging mods directory.
///
/// Checks:
/// - Path is inside staging/mods/
/// - No path traversal (..)
/// - No absolute paths outside staging
/// - No symlinks escaping staging
pub fn validate_jar_containment(jar_path: &Path, staging_mods: &Path) -> Result<PathBuf, String> {
    let path_str = jar_path.to_string_lossy();

    // Reject obvious path traversal
    if path_str.contains("..") {
        return Err("Path traversal detected (..)".to_string());
    }

    // Reject Windows absolute paths not under staging
    if path_str.len() >= 2 && path_str.as_bytes()[1] == b':' && !path_str.contains("mods") {
        return Err("Windows absolute path outside staging".to_string());
    }

    // Reject Unix absolute paths not under staging
    if jar_path.is_absolute() {
        let canonical_staging = staging_mods
            .canonicalize()
            .map_err(|e| format!("Cannot canonicalize staging mods: {}", e))?;
        let canonical_jar = jar_path
            .canonicalize()
            .map_err(|e| format!("Cannot canonicalize JAR path: {}", e))?;
        if !canonical_jar.starts_with(&canonical_staging) {
            return Err("Absolute path outside staging mods".to_string());
        }
    }

    // Canonicalize to resolve symlinks
    let canonical_mods = staging_mods
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize staging mods: {}", e))?;

    // For relative paths, resolve against staging mods
    let resolved = if jar_path.is_absolute() {
        jar_path.to_path_buf()
    } else {
        staging_mods.join(jar_path)
    };

    if !resolved.exists() {
        return Err(format!("JAR does not exist: {}", resolved.display()));
    }

    let canonical_resolved = resolved
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize resolved path: {}", e))?;

    // Final containment check after symlink resolution
    if !canonical_resolved.starts_with(&canonical_mods) {
        return Err("Symlink escape detected — JAR resolves outside staging mods".to_string());
    }

    Ok(canonical_resolved)
}

// ── TOCTOU revalidation ─────────────────────────────────────────────────

/// Revalidate the JAR just before moving it. Catches changes between
/// approval and action execution.
pub fn revalidate_before_move(
    jar_path: &Path,
    expected_mod_id: &str,
    staging_mods: &Path,
) -> Result<(), String> {
    // 1. JAR still exists
    if !jar_path.exists() {
        return Err("JAR no longer exists".to_string());
    }

    // 2. Still inside staging/mods
    let canonical_mods = staging_mods
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize staging mods: {}", e))?;
    let canonical_jar = jar_path
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize JAR: {}", e))?;
    if !canonical_jar.starts_with(&canonical_mods) {
        return Err("JAR moved outside staging mods".to_string());
    }

    // 3. Metadata still declares expected mod_id
    let metadata = crate::jar_metadata::read_jar_mod_metadata(jar_path);
    if !metadata.mod_ids.is_empty() && !metadata.mod_ids.contains(&expected_mod_id.to_string()) {
        return Err(format!(
            "JAR metadata no longer declares '{}': now {:?}",
            expected_mod_id, metadata.mod_ids
        ));
    }
    // If metadata can't be read (corrupt JAR), we proceed — the JAR might
    // still be the right one. The attribution already verified it.

    Ok(())
}

// ── Quarantine move ─────────────────────────────────────────────────────

/// Build a jar_to_mod_ids map from a staging mods directory.
/// Reads each JAR's metadata to extract mod IDs.
pub fn build_jar_to_mod_ids(staging_mods: &Path) -> HashMap<PathBuf, Vec<String>> {
    let mut map = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(staging_mods) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "jar") {
                let metadata = crate::jar_metadata::read_jar_mod_metadata(&path);
                if !metadata.mod_ids.is_empty() {
                    map.insert(path, metadata.mod_ids);
                }
            }
        }
    }
    map
}

/// Move a JAR from staging/mods to the quarantine area.
///
/// Returns the quarantine path. Does NOT delete the original — it's moved.
/// Handles filename collisions with deterministic suffix.
pub fn quarantine_jar(
    jar_path: &Path,
    staging_mods: &Path,
    quarantine_dir: &Path,
) -> Result<PathBuf, String> {
    // Ensure quarantine directory exists
    std::fs::create_dir_all(quarantine_dir)
        .map_err(|e| format!("Failed to create quarantine dir: {}", e))?;

    let filename = jar_path
        .file_name()
        .ok_or_else(|| "JAR path has no filename".to_string())?
        .to_string_lossy()
        .to_string();

    // Handle collision: foo.jar → foo__2.jar, foo__3.jar, etc.
    let dest = find_unique_quarantine_path(&filename, quarantine_dir)?;

    // Compute SHA-256 before move
    let sha256 = compute_file_sha256(jar_path)?;

    // Move (not copy, not delete — rename is atomic on same filesystem)
    std::fs::rename(jar_path, &dest).map_err(|e| {
        format!(
            "Failed to quarantine {} → {}: {}",
            jar_path.display(),
            dest.display(),
            e
        )
    })?;

    eprintln!(
        "[RECOVERY] Quarantined {} → {} (SHA-256: {})",
        jar_path.display(),
        dest.display(),
        sha256
    );

    Ok(dest)
}

/// Find a unique filename in the quarantine directory.
fn find_unique_quarantine_path(filename: &str, quarantine_dir: &Path) -> Result<PathBuf, String> {
    let base = quarantine_dir.join(filename);
    if !base.exists() {
        return Ok(base);
    }

    // Try foo__2.jar, foo__3.jar, ...
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "jar".to_string());
    let ext = Path::new(filename)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for i in 2..=99 {
        let candidate = quarantine_dir.join(format!("{}__{}{}", stem, i, ext));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "Too many collisions for {} in quarantine",
        filename
    ))
}

// ── SHA-256 ─────────────────────────────────────────────────────────────

/// Compute SHA-256 of a file.
pub fn compute_file_sha256(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Failed to read file: {}", e))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

// ── Dependency impact ───────────────────────────────────────────────────

/// Check if disabling a mod would break dependencies of other retained mods.
///
/// Returns Ok(()) if safe, or Err(list of dependent mod_ids) if disabling
/// would break REQUIRED dependencies.
pub fn check_dependency_impact(
    target_mod_id: &str,
    dep_graph: &DependencyGraph,
) -> Result<(), Vec<String>> {
    let dependents = dep_graph.dependents_of(target_mod_id);
    let broken: Vec<String> = dependents
        .into_iter()
        .filter(|node| {
            node.dependencies.iter().any(|dep| {
                dep.mod_id == target_mod_id
                    && matches!(dep.kind, crate::jar_metadata::DependencyKind::Required)
            })
        })
        .filter_map(|node| {
            node.mod_id
                .clone()
                .or_else(|| node.mod_ids.first().cloned())
        })
        .collect();

    if broken.is_empty() {
        Ok(())
    } else {
        Err(broken)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_staging_with_jar(mod_id: &str, filename: &str) -> (TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        let jar = mods.join(filename);

        // Create a minimal JAR with fabric.mod.json
        let file = std::fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("fabric.mod.json", options).unwrap();
        let json = serde_json::json!({
            "id": mod_id,
            "version": "1.0.0",
            "environment": "*"
        });
        std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
        zip.finish().unwrap();

        (tmp, jar)
    }

    fn make_staging_with_multi_mod_jar(mod_ids: &[&str], filename: &str) -> (TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        let jar = mods.join(filename);

        let file = std::fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("fabric.mod.json", options).unwrap();
        let json = serde_json::json!({
            "id": mod_ids[0],
            "version": "1.0.0",
            "environment": "*",
            "provides": mod_ids[1..].to_vec()
        });
        std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
        zip.finish().unwrap();

        (tmp, jar)
    }

    // ── Fingerprint tests ───────────────────────────────────────────

    #[test]
    fn test_fingerprint_deterministic() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/mymod.jar"), 1, &[]);
        let fp2 = compute_fingerprint("txn1", "mymod", Path::new("mods/mymod.jar"), 1, &[]);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_mod_id() {
        let fp1 = compute_fingerprint("txn1", "modA", Path::new("mods/a.jar"), 1, &[]);
        let fp2 = compute_fingerprint("txn1", "modB", Path::new("mods/a.jar"), 1, &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_jar() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 1, &[]);
        let fp2 = compute_fingerprint("txn1", "mymod", Path::new("mods/b.jar"), 1, &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_transaction() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 1, &[]);
        let fp2 = compute_fingerprint("txn2", "mymod", Path::new("mods/a.jar"), 1, &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_attempt() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 1, &[]);
        let fp2 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 2, &[]);
        assert_ne!(fp1, fp2);
    }

    // ── Protected component tests ───────────────────────────────────

    #[test]
    fn test_protected_forge() {
        assert!(is_protected_component("forge"));
        assert!(is_protected_component("Forge"));
        assert!(is_protected_component("FORGE"));
    }

    #[test]
    fn test_protected_fabric() {
        assert!(is_protected_component("fabricloader"));
        assert!(is_protected_component("fabric-loader"));
        assert!(is_protected_component("fabric"));
        assert!(is_protected_component("fabric-api"));
    }

    #[test]
    fn test_protected_minecraft() {
        assert!(is_protected_component("minecraft"));
        assert!(is_protected_component("java"));
    }

    #[test]
    fn test_not_protected() {
        assert!(!is_protected_component("create"));
        assert!(!is_protected_component("jei"));
        assert!(!is_protected_component("mymod"));
    }

    // ── Path containment tests ──────────────────────────────────────

    #[test]
    fn test_path_traversal_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        let result = validate_jar_containment(Path::new("../../etc/passwd"), &mods);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("traversal"));
    }

    #[test]
    fn test_windows_absolute_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        let result =
            validate_jar_containment(Path::new("C:\\Windows\\System32\\kernel32.dll.jar"), &mods);
        assert!(result.is_err());
    }

    #[test]
    fn test_symlink_escape_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        // Create a file outside staging
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let outside_jar = outside.join("evil.jar");
        std::fs::write(&outside_jar, b"evil").unwrap();

        // Create symlink inside staging pointing outside
        let symlink = mods.join("evil.jar");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_jar, &symlink).unwrap();

        let result = validate_jar_containment(&symlink, &mods);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("escape") || err.contains("outside"));
    }

    #[test]
    fn test_valid_jar_accepted() {
        let (tmp, jar) = make_staging_with_jar("mymod", "mymod.jar");
        let mods = tmp.path().join("mods");
        let result = validate_jar_containment(&jar, &mods);
        assert!(result.is_ok());
    }

    #[test]
    fn test_path_with_spaces_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("my mods");
        std::fs::create_dir_all(&mods).unwrap();
        let jar = mods.join("my mod.jar");
        std::fs::write(&jar, b"fake jar").unwrap();

        let result = validate_jar_containment(&jar, &mods);
        assert!(result.is_ok());
    }

    // ── Action availability tests ───────────────────────────────────

    #[test]
    fn test_high_confidence_unique_culprit_available() {
        let mut jar_to_mod_ids = std::collections::HashMap::new();
        jar_to_mod_ids.insert(PathBuf::from("mods/create.jar"), vec!["create".to_string()]);

        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Attributed,
            confidence: CrashAttributionConfidence::High,
            candidates: vec![crate::crash_attribution::CrashCandidate {
                mod_id: Some("create".to_string()),
                jar_path: Some(PathBuf::from("mods/create.jar")),
                score: 200,
                evidence: vec![],
                is_client_only: false,
                is_unknown_compat: false,
            }],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ReviewMod {
                mod_id: "create".to_string(),
                jar_path: Some(PathBuf::from("mods/create.jar")),
            },
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(avail, RecoveryActionAvailability::Available);
    }

    #[test]
    fn test_medium_confidence_unavailable() {
        let jar_to_mod_ids = std::collections::HashMap::new();
        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Attributed,
            confidence: CrashAttributionConfidence::Medium,
            candidates: vec![crate::crash_attribution::CrashCandidate {
                mod_id: Some("mymod".to_string()),
                jar_path: Some(PathBuf::from("mods/mymod.jar")),
                score: 70,
                evidence: vec![],
                is_client_only: false,
                is_unknown_compat: false,
            }],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ReviewMod {
                mod_id: "mymod".to_string(),
                jar_path: Some(PathBuf::from("mods/mymod.jar")),
            },
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(avail, RecoveryActionAvailability::UnavailableLowConfidence);
    }

    #[test]
    fn test_ambiguous_unavailable() {
        let jar_to_mod_ids = std::collections::HashMap::new();
        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Ambiguous,
            confidence: CrashAttributionConfidence::Low,
            candidates: vec![],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ConflictingEvidence,
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(avail, RecoveryActionAvailability::UnavailableAmbiguous);
    }

    #[test]
    fn test_unknown_unavailable() {
        let jar_to_mod_ids = std::collections::HashMap::new();
        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Unknown,
            confidence: CrashAttributionConfidence::Low,
            candidates: vec![],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::NoSafeRecommendation,
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(avail, RecoveryActionAvailability::UnavailableUnknown);
    }

    #[test]
    fn test_protected_component_rejected() {
        let mut jar_to_mod_ids = std::collections::HashMap::new();
        jar_to_mod_ids.insert(PathBuf::from("mods/forge.jar"), vec!["forge".to_string()]);

        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Attributed,
            confidence: CrashAttributionConfidence::High,
            candidates: vec![crate::crash_attribution::CrashCandidate {
                mod_id: Some("forge".to_string()),
                jar_path: Some(PathBuf::from("mods/forge.jar")),
                score: 200,
                evidence: vec![],
                is_client_only: false,
                is_unknown_compat: false,
            }],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ReviewMod {
                mod_id: "forge".to_string(),
                jar_path: Some(PathBuf::from("mods/forge.jar")),
            },
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(
            avail,
            RecoveryActionAvailability::UnavailableProtectedComponent
        );
    }

    #[test]
    fn test_multi_mod_jar_rejected() {
        let mut jar_to_mod_ids = std::collections::HashMap::new();
        jar_to_mod_ids.insert(
            PathBuf::from("mods/bundle.jar"),
            vec!["modA".to_string(), "modB".to_string()],
        );

        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Attributed,
            confidence: CrashAttributionConfidence::High,
            candidates: vec![crate::crash_attribution::CrashCandidate {
                mod_id: Some("modA".to_string()),
                jar_path: Some(PathBuf::from("mods/bundle.jar")),
                score: 200,
                evidence: vec![],
                is_client_only: false,
                is_unknown_compat: false,
            }],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ReviewMod {
                mod_id: "modA".to_string(),
                jar_path: Some(PathBuf::from("mods/bundle.jar")),
            },
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        match avail {
            RecoveryActionAvailability::UnavailableMultiModJar(others) => {
                assert_eq!(others, vec!["modB".to_string()]);
            }
            _ => panic!("Expected UnavailableMultiModJar, got {:?}", avail),
        }
    }

    // ── Quarantine move tests ───────────────────────────────────────

    #[test]
    fn test_quarantine_move_removes_source() {
        let (tmp, jar) = make_staging_with_jar("mymod", "mymod.jar");
        let mods = tmp.path().join("mods");
        let quarantine = tmp.path().join(".lbby-quarantine").join("mods");
        let original_bytes = std::fs::read(&jar).unwrap();

        let dest = quarantine_jar(&jar, &mods, &quarantine).unwrap();

        assert!(!jar.exists(), "Source should be removed");
        assert!(dest.exists(), "Destination should exist");
        assert_eq!(std::fs::read(&dest).unwrap(), original_bytes);
    }

    #[test]
    fn test_quarantine_preserves_bytes() {
        let (tmp, jar) = make_staging_with_jar("mymod", "mymod.jar");
        let mods = tmp.path().join("mods");
        let quarantine = tmp.path().join(".lbby-quarantine").join("mods");

        let original_bytes = std::fs::read(&jar).unwrap();
        let dest = quarantine_jar(&jar, &mods, &quarantine).unwrap();
        let quarantined_bytes = std::fs::read(&dest).unwrap();

        assert_eq!(original_bytes, quarantined_bytes);
    }

    #[test]
    fn test_quarantine_no_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        let quarantine = tmp.path().join(".lbby-quarantine").join("mods");
        std::fs::create_dir_all(&quarantine).unwrap();

        // Create jar in mods
        let jar = mods.join("foo.jar");
        std::fs::write(&jar, b"version1").unwrap();

        // Pre-create quarantine destination
        std::fs::write(quarantine.join("foo.jar"), b"existing").unwrap();

        let dest = quarantine_jar(&jar, &mods, &quarantine).unwrap();

        // Should have used a different name
        assert_ne!(dest, quarantine.join("foo.jar"));
        assert!(dest.file_name().unwrap().to_string_lossy().contains("__2"));

        // Original quarantine file untouched
        assert_eq!(
            std::fs::read(quarantine.join("foo.jar")).unwrap(),
            b"existing"
        );
    }

    // ── SHA-256 test ────────────────────────────────────────────────

    #[test]
    fn test_sha256_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.jar");
        std::fs::write(&file, b"hello world").unwrap();

        let hash1 = compute_file_sha256(&file).unwrap();
        let hash2 = compute_file_sha256(&file).unwrap();
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA-256 hex is 64 chars
    }

    // ── Dependency impact test ──────────────────────────────────────

    // Note: DependencyGraph is a complex type; we test the concept with
    // the actual graph in integration tests. Unit test here for the
    // protected component check which is the other guard.

    #[test]
    fn test_missing_jar_path_unavailable() {
        let jar_to_mod_ids = std::collections::HashMap::new();
        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Attributed,
            confidence: CrashAttributionConfidence::High,
            candidates: vec![crate::crash_attribution::CrashCandidate {
                mod_id: Some("mymod".to_string()),
                jar_path: None, // missing path
                score: 200,
                evidence: vec![],
                is_client_only: false,
                is_unknown_compat: false,
            }],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ReviewMod {
                mod_id: "mymod".to_string(),
                jar_path: None,
            },
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(avail, RecoveryActionAvailability::UnavailableMissingJar);
    }

    #[test]
    fn test_duplicate_provider_unavailable() {
        let mut jar_to_mod_ids = std::collections::HashMap::new();
        jar_to_mod_ids.insert(PathBuf::from("mods/a.jar"), vec!["mymod".to_string()]);
        jar_to_mod_ids.insert(PathBuf::from("mods/b.jar"), vec!["mymod".to_string()]);

        let report = CrashAttributionReport {
            status: CrashAttributionStatus::Attributed,
            confidence: CrashAttributionConfidence::High,
            candidates: vec![crate::crash_attribution::CrashCandidate {
                mod_id: Some("mymod".to_string()),
                jar_path: Some(PathBuf::from("mods/a.jar")),
                score: 200,
                evidence: vec![],
                is_client_only: false,
                is_unknown_compat: false,
            }],
            primary_candidate: None,
            summary: String::new(),
            recommendation: crate::crash_attribution::CrashRecommendation::ReviewMod {
                mod_id: "mymod".to_string(),
                jar_path: Some(PathBuf::from("mods/a.jar")),
            },
        };

        let avail = check_action_availability(&report, &jar_to_mod_ids);
        assert_eq!(
            avail,
            RecoveryActionAvailability::UnavailableDuplicateProvider
        );
    }
}
