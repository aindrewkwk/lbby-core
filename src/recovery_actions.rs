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
use std::sync::{Arc, Mutex, OnceLock};

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

// ── Server lifecycle state for restore safety (Phase 3L.1) ──────────

/// Authoritative server lifecycle state.
/// Maps to the existing `ServerStatus` in server.rs but is defined independently
/// so the recovery core does not depend on the Tauri server module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerLifecycleState {
    Stopped,
    Installing,
    DownloadingJava,
    DownloadingJar,
    Preparing,
    Starting,
    Running,
    Stopping,
    Restarting,
    Error,
    /// Provider could not determine the state.
    Unknown,
}

impl ServerLifecycleState {
    /// Whether restore is allowed from this state.
    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }

    /// Human-readable name for diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Installing => "installing",
            Self::DownloadingJava => "downloading_java",
            Self::DownloadingJar => "downloading_jar",
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Restarting => "restarting",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

/// Authoritative source of server lifecycle state.
///
/// The production implementation reads from the backend server manager
/// (Tauri `ServerHandle` for App, cloud node state for Cloud).
/// Tests use `MockLifecycleProvider`.
pub trait RestoreLifecycleProvider: Send + Sync {
    fn get_server_state(&self, server_id: &str) -> Result<ServerLifecycleState, String>;
}

// ── Per-server operation lock (Phase 3L.1) ─────────────────────────

/// Global per-server restore locks.
///
/// Each server_id gets its own `Mutex<()>`. Two restores for the same
/// server serialize; restores for different servers proceed in parallel.
static RESTORE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

/// Get (or create) the per-server lock Arc. Caller clones it and locks in
/// its own scope so the guard's lifetime is correct.
fn get_server_lock_arc(server_id: &str) -> Arc<Mutex<()>> {
    let locks = RESTORE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = locks.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(server_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
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
/// boot attempt number + crash evidence identifiers + JAR SHA-256.
/// Used to detect stale approvals.
pub fn compute_fingerprint(
    transaction_id: &str,
    mod_id: &str,
    jar_path: &Path,
    boot_attempt: u8,
    evidence_ids: &[String],
    jar_sha256: &str,
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
    hasher.update(b"|");
    hasher.update(jar_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Verify that an approval fingerprint matches the current attribution state.
pub fn verify_fingerprint(
    approval: &RecoveryApproval,
    report: &CrashAttributionReport,
    transaction_id: &str,
    jar_path: &Path,
    boot_attempt: u8,
    jar_sha256: &str,
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
        jar_sha256,
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

// ── Domain-specific approval API ──────────────────────────────────────

/// Result of a recovery approval attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalResult {
    /// Recovery applied successfully. JAR quarantined, ready for re-validation.
    Applied {
        quarantine_path: PathBuf,
        sha256: String,
        recovery_actions_used: u8,
    },
    /// Approval rejected — transaction rolled back, live unchanged.
    Rejected(String),
    /// Fingerprint no longer matches — JAR bytes or attribution changed.
    Invalidated(String),
    /// Recovery action budget exhausted.
    RecoveryLimitReached { used: u8, max: u8 },
    /// Dependency preflight failed — would break required dependents.
    DependencyImpact(Vec<String>),
    /// Transaction not found or already completed.
    TransactionNotFound(String),
    /// Metadata corruption or inconsistency.
    InvalidState(String),
    /// Internal failure during quarantine.
    Failed(String),
}

/// Find all pending recovery transactions for a server.
///
/// Scans the staging root for transactions in `PendingUserAction` phase
/// that have a `pending_recovery.json` file.
pub fn find_pending_recoveries(live_path: &Path) -> Vec<PendingRecoveryInfo> {
    use crate::install_transaction::{PendingRecoveryMetadata, TransactionMeta, TransactionPhase};

    let parent = match live_path.parent() {
        Some(p) => p,
        None => return vec![],
    };
    let staging_root = parent.join(".lbby-staging");
    let mut results = vec![];

    if let Ok(entries) = std::fs::read_dir(&staging_root) {
        for entry in entries.flatten() {
            let staging_path = entry.path();
            let marker = staging_path.join("transaction.json");
            if !marker.exists() {
                continue;
            }
            // Load transaction marker to check phase
            if let Ok(content) = std::fs::read_to_string(&marker) {
                if let Ok(meta) = serde_json::from_str::<TransactionMeta>(&content) {
                    if meta.phase == TransactionPhase::PendingUserAction {
                        let recovery_path = meta.pending_recovery_path();
                        if recovery_path.exists() {
                            if let Ok(recovery) = PendingRecoveryMetadata::load(&recovery_path) {
                                results.push(PendingRecoveryInfo {
                                    server_id: recovery.server_id.clone(),
                                    transaction_id: recovery.transaction_id.clone(),
                                    staging_path: staging_path.clone(),
                                    recovery,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    results
}

/// Information about a pending recovery transaction (for UI discovery).
#[derive(Debug, Clone)]
pub struct PendingRecoveryInfo {
    pub server_id: String,
    pub transaction_id: String,
    pub staging_path: PathBuf,
    pub recovery: crate::install_transaction::PendingRecoveryMetadata,
}

// ── Quarantine management types (Phase 3L) ─────────────────────────────

/// Current status of a quarantined artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuarantineStatus {
    /// Artifact is quarantined and available for restore.
    Quarantined,
    /// Artifact has been successfully restored to live/mods.
    Restored,
    /// Restore attempt failed (see RestoreRecord for details).
    RestoreFailed,
    /// Metadata file exists but artifact JAR is missing from disk.
    MissingArtifact,
    /// Artifact JAR exists but SHA-256 does not match metadata.
    HashMismatch,
}

/// Authoritative metadata for a quarantined artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineRecord {
    /// Stable record ID (derived from transaction_id + relative path + sha256).
    pub record_id: String,
    /// Server that owns this quarantine.
    pub server_id: String,
    /// Transaction that quarantined this artifact.
    pub transaction_id: String,
    /// Relative path the JAR occupied in live/mods (e.g. "mods/create-0.5.1.jar").
    pub original_relative_path: PathBuf,
    /// Filename of the quarantined JAR.
    pub filename: String,
    /// Mod IDs declared by this JAR at quarantine time.
    pub mod_ids: Vec<String>,
    /// SHA-256 of the quarantined JAR bytes.
    pub sha256: String,
    /// Which recovery action number produced this quarantine.
    pub recovery_action_number: u8,
    /// Human-readable reason for quarantine.
    pub reason: String,
    /// ISO-8601 timestamp of quarantine creation.
    pub created_at: String,
    /// Current status.
    pub status: QuarantineStatus,
    /// ISO-8601 timestamp if restored, None otherwise.
    pub restored_at: Option<String>,
    /// SHA-256 verified after restore (if restored).
    pub restore_sha256: Option<String>,
    /// Absolute path the artifact was restored to (if restored).
    pub restore_target: Option<PathBuf>,
}

/// Top-level quarantine metadata file (schema_version + records).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineMetadata {
    /// Schema version for future migrations.
    pub schema_version: u32,
    /// All quarantine records for this transaction.
    pub records: Vec<QuarantineRecord>,
}

impl QuarantineMetadata {
    /// Empty metadata with schema version 1.
    pub fn v1() -> Self {
        Self {
            schema_version: 1,
            records: Vec::new(),
        }
    }
}

/// Compute a stable record ID from transaction_id + relative path + sha256.
pub fn compute_record_id(transaction_id: &str, relative_path: &str, sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(transaction_id.as_bytes());
    hasher.update(b"|");
    hasher.update(relative_path.as_bytes());
    hasher.update(b"|");
    hasher.update(sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Result of a quarantine listing operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineListing {
    pub records: Vec<QuarantineRecord>,
    /// Transaction directories that had no metadata.json (legacy or corrupt).
    pub orphaned_transactions: Vec<String>,
}

/// Result of a restore attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreResult {
    /// Restore succeeded. Artifact copied to live/mods, quarantine preserved.
    Restored { sha256: String, target: PathBuf },
    /// Restore succeeded but metadata update failed. File is live; repairable on next listing.
    RestoredMetadataUpdateFailed { sha256: String, target: PathBuf },
    /// Record already restored (idempotency).
    AlreadyRestored,
    /// Quarantine metadata not found for this record_id.
    RecordNotFound(String),
    /// Quarantine metadata is corrupt or missing required fields.
    InvalidMetadata(String),
    /// Quarantined JAR no longer exists on disk.
    MissingArtifact,
    /// SHA-256 of quarantined JAR does not match metadata.
    HashMismatch,
    /// Target file already exists in live/mods.
    DestinationConflict { existing_path: PathBuf },
    /// Live mods already contains another JAR declaring the same mod_id.
    DuplicateProviderConflict {
        conflicting_jar: PathBuf,
        mod_id: String,
    },
    /// Server is not in the Stopped state (Running, Starting, Validating, etc.).
    ServerNotStopped { state: String },
    /// Server state changed during restore (TOCTOU re-check caught a race).
    StateChanged { new_state: String },
    /// Server has an active transaction (Building, Committing, PendingUserAction).
    ActiveTransaction { transaction_id: String },
    /// Target mod is explicitly ClientOnly — cannot restore to dedicated server.
    ExplicitClientOnly { mod_id: String },
    /// Target is a protected platform component.
    ProtectedComponent { mod_id: String },
    /// Quarantine artifact mod_ids do not match metadata mod_ids.
    IdentityMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    /// Restore path would escape live/mods (path traversal).
    PathEscape,
    /// Quarantine artifact source path escapes the expected quarantine directory.
    QuarantineSourceEscape,
    /// Internal failure during copy/verify/rename.
    Failed(String),
}

/// Audit record for a restore attempt (persisted in history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreRecord {
    pub record_id: String,
    pub server_id: String,
    pub transaction_id: String,
    pub mod_ids: Vec<String>,
    pub sha256: String,
    pub result: RestoreResult,
    pub timestamp: String,
}

/// Domain-specific approval entry point.
///
/// The frontend sends only stable identifiers: server_id, transaction_id, fingerprint.
/// The backend resolves everything else from persisted recovery metadata.
///
/// Flow:
/// 1. Find pending transaction by server_id + transaction_id
/// 2. Load and validate PendingRecoveryMetadata
/// 3. Verify fingerprint matches
/// 4. Enforce recovery action budget (MAX_USER_RECOVERY_ACTIONS)
/// 5. Revalidate JAR (exists, containment, SHA-256, metadata)
/// 6. Dependency impact preflight
/// 7. Quarantine JAR (move, not delete)
/// 8. Mark metadata as applied
/// 9. Return Applied with quarantine info
/// Core approval logic with explicit live_path (testable without config).
fn approve_crash_recovery_inner(
    server_id: &str,
    transaction_id: &str,
    fingerprint: &str,
    live_path: &Path,
) -> ApprovalResult {
    use crate::install_transaction::{PendingRecoveryMetadata, TransactionMeta};

    let live_path = live_path.to_path_buf();

    let parent = match live_path.parent() {
        Some(p) => p,
        None => return ApprovalResult::InvalidState("Live path has no parent".to_string()),
    };
    let staging_root = parent.join(".lbby-staging");

    // Find the specific transaction
    let mut found_staging: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&staging_root) {
        for entry in entries.flatten() {
            let staging_path = entry.path();
            let marker = staging_path.join("transaction.json");
            if !marker.exists() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&marker) {
                if let Ok(meta) = serde_json::from_str::<TransactionMeta>(&content) {
                    if meta.transaction_id == transaction_id && meta.server_id == server_id {
                        found_staging = Some(staging_path);
                        break;
                    }
                }
            }
        }
    }

    let staging_path = match found_staging {
        Some(p) => p,
        None => {
            return ApprovalResult::TransactionNotFound(format!(
                "Transaction {} not found for server {}",
                transaction_id, server_id
            ))
        }
    };

    // 2. Load PendingRecoveryMetadata
    let recovery_path = staging_path.join("pending_recovery.json");
    let recovery = match PendingRecoveryMetadata::load(&recovery_path) {
        Ok(r) => r,
        Err(e) => {
            return ApprovalResult::InvalidState(format!("Cannot load recovery metadata: {}", e))
        }
    };

    // Already applied? (idempotency guard)
    if recovery.applied {
        return ApprovalResult::Invalidated("Recovery already applied".to_string());
    }

    // Verify server + transaction match
    if recovery.server_id != server_id || recovery.transaction_id != transaction_id {
        return ApprovalResult::Invalidated(
            "Server/transaction mismatch in persisted metadata".to_string(),
        );
    }

    // 3. Verify fingerprint
    if recovery.attribution_fingerprint != fingerprint {
        return ApprovalResult::Invalidated(
            "Fingerprint mismatch — attribution state changed since approval".to_string(),
        );
    }

    // 4. Enforce recovery action budget
    if recovery.recovery_actions_used >= MAX_USER_RECOVERY_ACTIONS {
        return ApprovalResult::RecoveryLimitReached {
            used: recovery.recovery_actions_used,
            max: MAX_USER_RECOVERY_ACTIONS,
        };
    }

    // 5. Revalidate JAR
    let jar_path = &recovery.target_jar_path;
    let staging_mods = &recovery.staging_mods;

    // 5a. JAR exists
    if !jar_path.exists() {
        return ApprovalResult::Invalidated("Target JAR no longer exists".to_string());
    }

    // 5b. Canonical containment
    let canonical_jar = match validate_jar_containment(jar_path, staging_mods) {
        Ok(p) => p,
        Err(e) => return ApprovalResult::Invalidated(format!("Containment check failed: {}", e)),
    };

    // 5c. SHA-256 matches
    let current_sha256 = match compute_file_sha256(&canonical_jar) {
        Ok(h) => h,
        Err(e) => return ApprovalResult::Invalidated(format!("Cannot compute SHA-256: {}", e)),
    };
    if current_sha256 != recovery.target_jar_sha256 {
        return ApprovalResult::Invalidated(
            "JAR bytes changed since attribution — SHA-256 mismatch".to_string(),
        );
    }

    // 5d. JAR metadata still declares expected mod_id
    if let Err(e) = revalidate_before_move(&canonical_jar, &recovery.target_mod_id, staging_mods) {
        return ApprovalResult::Invalidated(format!("Pre-move revalidation failed: {}", e));
    }

    // 5e. Not protected
    if is_protected_component(&recovery.target_mod_id) {
        return ApprovalResult::Invalidated("Target is a protected component".to_string());
    }

    // 5f. Not multi-mod (check jar_to_mod_ids)
    let jar_to_mod_ids = build_jar_to_mod_ids(staging_mods);
    if let Some(mod_ids) = jar_to_mod_ids.get(&canonical_jar) {
        if mod_ids.len() > 1 {
            let others: Vec<String> = mod_ids
                .iter()
                .filter(|id| **id != recovery.target_mod_id)
                .cloned()
                .collect();
            if !others.is_empty() {
                return ApprovalResult::Invalidated(format!(
                    "JAR contains multiple mods: {:?}",
                    others
                ));
            }
        }
    }

    // 6. Dependency-impact preflight — ALWAYS build from staging to ensure freshness.
    // The caller-supplied graph (if any) is ignored; the backend owns this safety check.
    {
        use crate::mod_compat::classify_mod_local;
        let mut entries = Vec::new();
        if let Ok(rd) = std::fs::read_dir(staging_mods) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().map_or(false, |e| e == "jar") {
                    let compat = classify_mod_local(&p);
                    entries.push((p, compat));
                }
            }
        }
        let fresh_graph = DependencyGraph::build(&entries);
        if let Err(broken) = check_dependency_impact(&recovery.target_mod_id, &fresh_graph) {
            return ApprovalResult::DependencyImpact(broken);
        }
    }

    // 7. Quarantine JAR
    let quarantine_dir = staging_mods
        .parent()
        .unwrap_or(staging_mods)
        .join(".lbby-quarantine")
        .join("mods");

    let quarantine_path = match quarantine_jar(&canonical_jar, staging_mods, &quarantine_dir) {
        Ok(p) => p,
        Err(e) => return ApprovalResult::Failed(e),
    };

    // 8. Mark metadata as applied (idempotency)
    let mut updated_recovery = recovery.clone();
    updated_recovery.applied = true;
    if let Err(e) = updated_recovery.save(&recovery_path) {
        // Non-fatal — quarantine already happened. Log and continue.
        eprintln!(
            "[RECOVERY] Warning: failed to mark recovery as applied: {}",
            e
        );
    }

    eprintln!(
        "[RECOVERY] Approved recovery for '{}' in transaction {} — quarantined to {}",
        recovery.target_mod_id,
        transaction_id,
        quarantine_path.display()
    );

    ApprovalResult::Applied {
        quarantine_path,
        sha256: current_sha256,
        recovery_actions_used: recovery.recovery_actions_used + 1,
    }
}

/// Approve a pending recovery action (production entry point).
///
/// Resolves live_path from config. Returns TransactionNotFound if server
/// is not in config.
pub fn approve_crash_recovery(
    server_id: &str,
    transaction_id: &str,
    fingerprint: &str,
) -> ApprovalResult {
    let live_path = match find_live_path_for_server(server_id) {
        Some(p) => p,
        None => {
            return ApprovalResult::TransactionNotFound(format!(
                "No live path found for server '{}'",
                server_id
            ))
        }
    };
    approve_crash_recovery_inner(server_id, transaction_id, fingerprint, &live_path)
}

/// Approve a pending recovery action with explicit live_path (test-only).
#[cfg(feature = "testing")]
pub fn approve_crash_recovery_at(
    server_id: &str,
    transaction_id: &str,
    fingerprint: &str,
    live_path: &Path,
) -> ApprovalResult {
    approve_crash_recovery_inner(server_id, transaction_id, fingerprint, live_path)
}

/// Reject a pending recovery — rollback the transaction, live unchanged.
///
/// The frontend sends only stable identifiers. The backend resolves the
/// transaction and rolls it back.
pub fn reject_crash_recovery(server_id: &str, transaction_id: &str) -> Result<(), String> {
    use crate::install_transaction::{InstallTransaction, TransactionMeta};

    let live_path = match find_live_path_for_server(server_id) {
        Some(p) => p,
        None => return Err(format!("No live path found for server '{}'", server_id)),
    };
    reject_crash_recovery_inner(server_id, transaction_id, &live_path)
}

/// Reject a pending recovery with explicit live_path (testable without config).
#[cfg(feature = "testing")]
pub fn reject_crash_recovery_at(
    server_id: &str,
    transaction_id: &str,
    live_path: &Path,
) -> Result<(), String> {
    reject_crash_recovery_inner(server_id, transaction_id, live_path)
}

fn reject_crash_recovery_inner(
    server_id: &str,
    transaction_id: &str,
    live_path: &Path,
) -> Result<(), String> {
    use crate::install_transaction::{InstallTransaction, TransactionMeta};

    let parent = live_path.parent().ok_or("Live path has no parent")?;
    let staging_root = parent.join(".lbby-staging");

    // Find the specific transaction
    let mut found_staging: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&staging_root) {
        for entry in entries.flatten() {
            let staging_path = entry.path();
            let marker = staging_path.join("transaction.json");
            if !marker.exists() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&marker) {
                if let Ok(meta) = serde_json::from_str::<TransactionMeta>(&content) {
                    if meta.transaction_id == transaction_id && meta.server_id == server_id {
                        found_staging = Some(staging_path);
                        break;
                    }
                }
            }
        }
    }

    let staging_path = found_staging.ok_or_else(|| {
        format!(
            "Transaction {} not found for server {}",
            transaction_id, server_id
        )
    })?;

    // Load TransactionMeta and resume the transaction so we can roll it back
    let marker_path = staging_path.join("transaction.json");
    let meta_content = std::fs::read_to_string(&marker_path)
        .map_err(|e| format!("Failed to read transaction marker: {}", e))?;
    let meta: TransactionMeta = serde_json::from_str(&meta_content)
        .map_err(|e| format!("Failed to parse transaction marker: {}", e))?;
    let txn = InstallTransaction::resume(meta)?;
    txn.rollback()?;

    eprintln!(
        "[RECOVERY] Rejected recovery for transaction {} — rolled back",
        transaction_id
    );

    Ok(())
}

/// Find the live server path for a given server_id.
///
/// Scans the config to find the server directory. This is a helper that
/// avoids hardcoding paths.
fn find_live_path_for_server(server_id: &str) -> Option<PathBuf> {
    let cfg = crate::config::load_config();
    let server_path = PathBuf::from(&cfg.server_path);
    // server_path points to the server directory itself
    if server_path.exists() && server_path.file_name().map_or(false, |n| n == server_id) {
        return Some(server_path);
    }
    // Try as parent/servers/server_id
    let candidate = server_path.join(server_id);
    if candidate.exists() {
        return Some(candidate);
    }
    // Try parent directory
    if let Some(parent) = server_path.parent() {
        let candidate = parent.join(server_id);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

// ── Retry state persistence (cross-lifecycle) ────────────────────────

/// Persisted retry state for cross-pause/resume lifecycle tracking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RetryStateSnapshot {
    pub boot_attempts_used: u8,
    pub dependency_repairs_used: u8,
    pub runtime_repairs_used: u8,
    pub recovery_actions_used: u8,
}

fn retry_state_path(server_path: &Path) -> PathBuf {
    server_path.join(".lbby-retry-state.json")
}

/// Save orchestrator retry state to the server directory before pause.
pub fn save_retry_state(server_path: &Path, snapshot: &RetryStateSnapshot) -> Result<(), String> {
    let path = retry_state_path(server_path);
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| format!("Failed to serialize retry state: {}", e))?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write retry state: {}", e))
}

/// Load orchestrator retry state from a prior pause.
/// Returns None if no prior state exists (fresh lifecycle).
pub fn load_retry_state(server_path: &Path) -> Option<RetryStateSnapshot> {
    let path = retry_state_path(server_path);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Delete retry state after successful commit or rollback.
pub fn clear_retry_state(server_path: &Path) {
    let _ = std::fs::remove_file(retry_state_path(server_path));
}

/// Preserve quarantine artifact out of staging before transaction commit.
///
/// Moves quarantine contents from `<staging>/.lbby-quarantine/` to
/// `<live-parent>/.lbby-quarantine/<server-id>/<txn-id>/`.
///
/// Returns the new quarantine root path.
pub fn preserve_quarantine_on_commit(
    staging_path: &Path,
    server_id: &str,
    transaction_id: &str,
) -> Result<PathBuf, String> {
    let staging_quarantine = staging_path.join(".lbby-quarantine");
    if !staging_quarantine.exists() {
        // No quarantine to preserve
        return Ok(staging_quarantine);
    }

    let live_parent = staging_path.parent().ok_or("Staging path has no parent")?;
    let preserve_root = live_parent
        .join(".lbby-quarantine")
        .join(server_id)
        .join(transaction_id);

    std::fs::create_dir_all(&preserve_root)
        .map_err(|e| format!("Failed to create quarantine preserve dir: {}", e))?;

    // Move all contents from staging quarantine to preserve root
    move_dir_contents(&staging_quarantine, &preserve_root)?;

    eprintln!(
        "[RECOVERY] Preserved quarantine artifacts to {}",
        preserve_root.display()
    );

    Ok(preserve_root)
}

/// Recursively move contents of one directory into another.
fn move_dir_contents(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("Failed to read dir {}: {}", src.display(), e))?
    {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .map_err(|e| format!("Failed to create dir {}: {}", dst_path.display(), e))?;
            move_dir_contents(&src_path, &dst_path)?;
            std::fs::remove_dir(&src_path).ok(); // Remove empty source dir
        } else {
            std::fs::rename(&src_path, &dst_path).map_err(|e| {
                format!(
                    "Failed to move {} → {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                )
            })?;
        }
    }
    Ok(())
}

// ── Quarantine metadata persistence (Phase 3L) ────────────────────────

/// Path to the quarantine metadata file for a transaction.
fn quarantine_metadata_path(quarantine_txn_dir: &Path) -> PathBuf {
    quarantine_txn_dir.join("quarantine_metadata.json")
}

/// Load quarantine metadata from a transaction quarantine directory.
/// Returns None if file does not exist (not an error — legacy dir).
/// Returns Err if file exists but is corrupt.
fn load_quarantine_metadata(
    quarantine_txn_dir: &Path,
) -> Result<Option<QuarantineMetadata>, String> {
    let path = quarantine_metadata_path(quarantine_txn_dir);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read quarantine metadata: {}", e))?;
    let meta: QuarantineMetadata = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse quarantine metadata: {}", e))?;
    Ok(Some(meta))
}

/// Save quarantine metadata to a transaction quarantine directory.
pub fn save_quarantine_metadata(
    quarantine_txn_dir: &Path,
    meta: &QuarantineMetadata,
) -> Result<(), String> {
    std::fs::create_dir_all(quarantine_txn_dir)
        .map_err(|e| format!("Failed to create quarantine dir: {}", e))?;
    let path = quarantine_metadata_path(quarantine_txn_dir);
    let json = serde_json::to_string_pretty(meta)
        .map_err(|e| format!("Failed to serialize quarantine metadata: {}", e))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("Failed to write quarantine metadata: {}", e))?;
    Ok(())
}

/// Record a newly quarantined artifact into the transaction quarantine metadata.
///
/// Called after `preserve_quarantine_on_commit` to persist the record alongside
/// the preserved artifact. The `quarantine_txn_dir` is the preserve root for
/// this transaction (e.g. `<live-parent>/.lbby-quarantine/<server-id>/<txn-id>/`).
pub fn record_quarantine(
    quarantine_txn_dir: &Path,
    server_id: &str,
    transaction_id: &str,
    original_relative_path: &str,
    filename: &str,
    mod_ids: Vec<String>,
    sha256: &str,
    recovery_action_number: u8,
    reason: &str,
) -> Result<QuarantineRecord, String> {
    let record_id = compute_record_id(transaction_id, original_relative_path, sha256);
    let record = QuarantineRecord {
        record_id,
        server_id: server_id.to_string(),
        transaction_id: transaction_id.to_string(),
        original_relative_path: PathBuf::from(original_relative_path),
        filename: filename.to_string(),
        mod_ids,
        sha256: sha256.to_string(),
        recovery_action_number,
        reason: reason.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        status: QuarantineStatus::Quarantined,
        restored_at: None,
        restore_sha256: None,
        restore_target: None,
    };

    let mut meta =
        load_quarantine_metadata(quarantine_txn_dir)?.unwrap_or_else(QuarantineMetadata::v1);
    meta.records.push(record.clone());
    save_quarantine_metadata(quarantine_txn_dir, &meta)?;

    eprintln!(
        "[RECOVERY] Recorded quarantine: {} (record_id: {})",
        filename, record.record_id
    );

    Ok(record)
}

// ── Quarantine listing (Phase 3L) ─────────────────────────────────────

/// List all quarantined mods for a server.
///
/// Scans `<live-parent>/.lbby-quarantine/<server-id>/` for transaction
/// directories, loads metadata, verifies artifacts still exist and SHA matches.
///
/// Returns newest-first (by created_at) when timestamps are available.
pub fn list_quarantined_mods(server_id: &str) -> Result<QuarantineListing, String> {
    let live_path = find_live_path_for_server(server_id)
        .ok_or(format!("No live path found for server '{}'", server_id))?;
    list_quarantined_mods_at(server_id, &live_path)
}

/// List quarantined mods with explicit live_path (testable without config).
pub fn list_quarantined_mods_at(
    server_id: &str,
    live_path: &Path,
) -> Result<QuarantineListing, String> {
    let parent = live_path.parent().ok_or("Live path has no parent")?;
    let quarantine_server_dir = parent.join(".lbby-quarantine").join(server_id);

    if !quarantine_server_dir.exists() {
        return Ok(QuarantineListing {
            records: Vec::new(),
            orphaned_transactions: Vec::new(),
        });
    }

    let mut all_records = Vec::new();
    let mut orphaned = Vec::new();

    let entries = std::fs::read_dir(&quarantine_server_dir)
        .map_err(|e| format!("Failed to read quarantine dir: {}", e))?;

    for entry in entries.flatten() {
        // Skip symlinks — do not follow links outside the quarantine dir (Phase 3L.1)
        let ft = entry.file_type().ok();
        if ft.map_or(false, |f| f.is_symlink()) {
            continue;
        }
        let txn_dir = entry.path();
        if !txn_dir.is_dir() {
            continue;
        }
        let txn_name = txn_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        match load_quarantine_metadata(&txn_dir)? {
            Some(mut meta) => {
                // Verify each record: artifact exists, SHA matches
                for rec in &mut meta.records {
                    if rec.status == QuarantineStatus::Restored {
                        // Restored records keep their status
                        all_records.push(rec.clone());
                        continue;
                    }
                    let artifact_path = txn_dir.join("mods").join(&rec.filename);
                    if !artifact_path.exists() {
                        rec.status = QuarantineStatus::MissingArtifact;
                    } else {
                        match compute_file_sha256(&artifact_path) {
                            Ok(hash) if hash == rec.sha256 => {
                                rec.status = QuarantineStatus::Quarantined;
                            }
                            Ok(_) => {
                                rec.status = QuarantineStatus::HashMismatch;
                            }
                            Err(_) => {
                                rec.status = QuarantineStatus::MissingArtifact;
                            }
                        }
                    }
                    all_records.push(rec.clone());
                }
            }
            None => {
                // No metadata — check if there are JAR files (legacy or orphan)
                let mods_dir = txn_dir.join("mods");
                let has_jars_in_mods = if mods_dir.exists() {
                    std::fs::read_dir(&mods_dir)
                        .ok()
                        .map(|rd| {
                            rd.flatten()
                                .any(|e| e.path().extension().map_or(false, |ext| ext == "jar"))
                        })
                        .unwrap_or(false)
                } else {
                    false
                };
                let has_jars_at_root = std::fs::read_dir(&txn_dir)
                    .ok()
                    .map(|rd| {
                        rd.flatten()
                            .any(|e| e.path().extension().map_or(false, |ext| ext == "jar"))
                    })
                    .unwrap_or(false);
                if has_jars_in_mods || has_jars_at_root {
                    orphaned.push(txn_name);
                }
            }
        }
    }

    // Sort newest first by created_at
    all_records.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    Ok(QuarantineListing {
        records: all_records,
        orphaned_transactions: orphaned,
    })
}

// ── Active transaction scanning (Phase 3L) ────────────────────────────

/// Check whether a server has any active install transactions.
///
/// An active transaction is one in Building, Committing, or PendingUserAction phase.
/// Returns Ok(()) if no active transactions, or Err with the transaction_id if one exists.
fn check_no_active_transactions(live_path: &Path) -> Result<(), String> {
    use crate::install_transaction::{TransactionMeta, TransactionPhase};

    let parent = live_path.parent().ok_or("Live path has no parent")?;
    let staging_root = parent.join(".lbby-staging");

    if !staging_root.exists() {
        return Ok(());
    }

    if let Ok(entries) = std::fs::read_dir(&staging_root) {
        for entry in entries.flatten() {
            let staging_path = entry.path();
            let marker = staging_path.join("transaction.json");
            if !marker.exists() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&marker) {
                if let Ok(meta) = serde_json::from_str::<TransactionMeta>(&content) {
                    if meta.live_path == live_path {
                        match meta.phase {
                            TransactionPhase::Building
                            | TransactionPhase::Committing
                            | TransactionPhase::PendingUserAction => {
                                return Err(meta.transaction_id);
                            }
                            TransactionPhase::Committed => {}
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Duplicate provider check (Phase 3L) ──────────────────────────────

/// Scan live/mods for JARs that declare any of the given mod_ids.
/// Returns the first conflicting JAR and mod_id, or Ok(()) if no conflict.
fn check_duplicate_providers(
    live_mods: &Path,
    mod_ids: &[String],
) -> Result<(), (PathBuf, String)> {
    if !live_mods.exists() {
        return Ok(());
    }
    if let Ok(entries) = std::fs::read_dir(live_mods) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "jar") {
                let metadata = crate::jar_metadata::read_jar_mod_metadata(&path);
                for declared_id in &metadata.mod_ids {
                    if mod_ids.contains(declared_id) {
                        return Err((path, declared_id.clone()));
                    }
                }
            }
        }
    }
    Ok(())
}

// ── Path containment for restore (Phase 3L) ──────────────────────────

/// Validate that a restore destination is safely inside live_path/mods.
///
/// Rejects: path traversal, absolute external paths, symlink escapes.
/// `relative_path` is relative to `live_path` (e.g. "mods/create.jar").
fn validate_restore_destination(
    relative_path: &Path,
    live_path: &Path,
) -> Result<PathBuf, RestoreResult> {
    let path_str = relative_path.to_string_lossy();

    // Reject path traversal
    if path_str.contains("..") {
        return Err(RestoreResult::PathEscape);
    }

    // Reject absolute paths
    if relative_path.is_absolute() {
        return Err(RestoreResult::PathEscape);
    }

    // ── Structural validation: relative_path must start with "mods/" ──
    // Checked BEFORE filesystem resolution. A tampered persisted record like
    // "config/foo.jar" or "world/foo.jar" is rejected as InvalidMetadata —
    // the metadata is structurally wrong, not a filesystem escape.
    let rel_components: Vec<_> = relative_path.components().collect();
    if rel_components.is_empty() {
        return Err(RestoreResult::InvalidMetadata(
            "Empty relative_path".to_string(),
        ));
    }
    // First component must be "mods"
    match rel_components[0] {
        std::path::Component::Normal(c) if c == "mods" => {}
        _ => {
            return Err(RestoreResult::InvalidMetadata(format!(
                "relative_path '{}' does not start with mods/",
                path_str
            )))
        }
    }
    // Must have at least 2 components: mods/<filename>
    if rel_components.len() < 2 {
        return Err(RestoreResult::InvalidMetadata(format!(
            "relative_path '{}' must be mods/<filename>",
            path_str
        )));
    }
    // Reject nested subdirectories — restore only supports mods/*.jar
    if rel_components.len() > 2 {
        return Err(RestoreResult::InvalidMetadata(format!(
            "Nested paths not supported: '{}'. Only mods/<filename> allowed",
            path_str
        )));
    }

    // Resolve against live_path
    let destination = live_path.join(relative_path);

    // ── Canonical containment: must be under live/mods/ ───────────────
    let mods_root = live_path.join("mods");
    let canonical_mods = mods_root.canonicalize().map_err(|e| {
        RestoreResult::Failed(format!(
            "Failed to canonicalize mods root '{}': {}",
            mods_root.display(),
            e
        ))
    })?;

    let dest_parent = match destination.parent() {
        Some(p) => p,
        None => {
            return Err(RestoreResult::Failed(
                "Destination has no parent".to_string(),
            ))
        }
    };

    let canonical_parent = dest_parent.canonicalize().map_err(|e| {
        RestoreResult::Failed(format!(
            "Failed to canonicalize destination parent '{}': {}",
            dest_parent.display(),
            e
        ))
    })?;

    // The canonical parent must be a descendant of (or equal to) canonical mods root.
    if !canonical_parent.starts_with(&canonical_mods) {
        return Err(RestoreResult::PathEscape);
    }

    // Construct the final path under the canonical parent.
    let filename = destination.file_name().ok_or(RestoreResult::Failed(
        "Destination has no filename".to_string(),
    ))?;
    Ok(canonical_parent.join(filename))
}

// ── Quarantine source containment (Phase 3L.1) ─────────────────────

/// Validate that a quarantine artifact path resolves inside the expected
/// quarantine transaction directory. Even backend-written metadata must be
/// treated as untrusted after restart / manual edits.
fn validate_quarantine_source_containment(artifact_path: &Path, quarantine_txn_dir: &Path) -> bool {
    let canonical_txn_dir = match quarantine_txn_dir.canonicalize() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let canonical_artifact_parent = match artifact_path.parent().and_then(|p| p.canonicalize().ok())
    {
        Some(c) => c,
        None => return false,
    };
    canonical_artifact_parent.starts_with(&canonical_txn_dir)
}

// ── Atomic restore (Phase 3L / 3L.1) ───────────────────────────────

/// Perform an atomic restore: copy → verify SHA → TOCTOU re-check → rename.
///
/// 1. Write to temp file `.lbby-restore-<id>.tmp` (not ending in .jar)
/// 2. Verify SHA-256 of temp file matches expected
/// 3. Re-check authoritative server lifecycle (TOCTOU guard)
/// 4. Atomic rename temp → final destination
/// 5. Post-rename verification: hash the live file
///
/// On any failure: cleanup temp, return error. Quarantine source untouched.
fn atomic_restore_copy(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    record_id: &str,
    server_id: &str,
    lifecycle_provider: &dyn RestoreLifecycleProvider,
) -> Result<String, RestoreResult> {
    let parent = destination.parent().ok_or(RestoreResult::Failed(
        "Destination has no parent".to_string(),
    ))?;

    // ── Temp-file containment: must be under the same canonical mods root ──
    // Defense-in-depth: even though validate_restore_destination already ensures
    // the destination is under live/mods, we verify the temp file location too.
    let canonical_parent = parent.canonicalize().map_err(|e| {
        RestoreResult::Failed(format!(
            "Failed to canonicalize temp parent '{}': {}",
            parent.display(),
            e
        ))
    })?;

    // 1. Create temp file (NOT ending in .jar) — under the canonical parent
    let temp_path = canonical_parent.join(format!(".lbby-restore-{}.tmp", record_id));

    // Copy source → temp
    std::fs::copy(source, &temp_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        RestoreResult::Failed(format!("Failed to copy quarantine artifact to temp: {}", e))
    })?;

    // 2. Verify SHA of temp file
    let temp_sha = compute_file_sha256(&temp_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        RestoreResult::Failed(format!("Failed to hash temp file: {}", e))
    })?;

    if temp_sha != expected_sha256 {
        let _ = std::fs::remove_file(&temp_path);
        return Err(RestoreResult::Failed(format!(
            "SHA mismatch after copy: expected {}, got {}",
            expected_sha256, temp_sha
        )));
    }

    // 3. TOCTOU re-check: server must still be Stopped before we commit.
    match lifecycle_provider.get_server_state(server_id) {
        Ok(state) if !state.is_stopped() => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(RestoreResult::StateChanged {
                new_state: state.name().to_string(),
            });
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(RestoreResult::Failed(format!(
                "Failed to re-check server state: {}",
                e
            )));
        }
        _ => {} // Still stopped — proceed
    }

    // 4. Atomic rename temp → destination
    std::fs::rename(&temp_path, destination).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        RestoreResult::Failed(format!("Failed to rename temp to destination: {}", e))
    })?;

    // 5. Post-rename verification
    let live_sha = compute_file_sha256(destination).map_err(|e| {
        let _ = std::fs::remove_file(destination);
        RestoreResult::Failed(format!("Failed to verify restored file: {}", e))
    })?;

    if live_sha != expected_sha256 {
        let _ = std::fs::remove_file(destination);
        return Err(RestoreResult::Failed(format!(
            "Post-rename SHA mismatch: expected {}, got {}",
            expected_sha256, live_sha
        )));
    }

    Ok(live_sha)
}

// ── Restore API (Phase 3L / 3L.1) ─────────────────────────────────

/// Restore a quarantined mod (production entry point).
///
/// Resolves live_path from config. Queries authoritative lifecycle state
/// via the `RestoreLifecycleProvider` trait — frontend/API clients must NOT
/// pass filesystem paths or runtime booleans.
pub fn restore_quarantined_mod(
    server_id: &str,
    transaction_id: &str,
    record_id: &str,
    lifecycle_provider: &dyn RestoreLifecycleProvider,
) -> RestoreResult {
    let live_path = match find_live_path_for_server(server_id) {
        Some(p) => p,
        None => {
            return RestoreResult::RecordNotFound(format!(
                "No live path found for server '{}'",
                server_id
            ))
        }
    };
    restore_quarantined_mod_inner(
        server_id,
        transaction_id,
        record_id,
        &live_path,
        lifecycle_provider,
    )
}

/// Core restore implementation shared by production and test-only entry points.
fn restore_quarantined_mod_inner(
    server_id: &str,
    transaction_id: &str,
    record_id: &str,
    live_path: &Path,
    lifecycle_provider: &dyn RestoreLifecycleProvider,
) -> RestoreResult {
    // ── Acquire per-server lock (held until return) ──────────────────
    let _server_lock = get_server_lock_arc(server_id);
    let _server_guard = _server_lock.lock().unwrap_or_else(|e| e.into_inner());

    // 1. Server must be Stopped (authoritative lifecycle check)
    let lifecycle_state = match lifecycle_provider.get_server_state(server_id) {
        Ok(state) => state,
        Err(e) => return RestoreResult::Failed(format!("Failed to query server state: {}", e)),
    };
    if !lifecycle_state.is_stopped() {
        return RestoreResult::ServerNotStopped {
            state: lifecycle_state.name().to_string(),
        };
    }

    // 2. No active transactions
    if let Err(active_txn) = check_no_active_transactions(live_path) {
        return RestoreResult::ActiveTransaction {
            transaction_id: active_txn,
        };
    }

    // 3. Locate quarantine transaction directory
    let parent = match live_path.parent() {
        Some(p) => p,
        None => return RestoreResult::Failed("Live path has no parent".to_string()),
    };
    let quarantine_txn_dir = parent
        .join(".lbby-quarantine")
        .join(server_id)
        .join(transaction_id);

    if !quarantine_txn_dir.exists() {
        return RestoreResult::RecordNotFound(format!(
            "Quarantine directory not found for transaction {}",
            transaction_id
        ));
    }

    // 4. Load metadata
    let mut meta = match load_quarantine_metadata(&quarantine_txn_dir) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return RestoreResult::RecordNotFound(
                "No quarantine metadata for this transaction".to_string(),
            )
        }
        Err(e) => return RestoreResult::InvalidMetadata(e),
    };

    // 5. Find the record
    let rec_idx = match meta.records.iter().position(|r| r.record_id == record_id) {
        Some(i) => i,
        None => {
            return RestoreResult::RecordNotFound(format!(
                "Record '{}' not found in transaction {}",
                record_id, transaction_id
            ))
        }
    };

    let record = meta.records[rec_idx].clone();

    // 6. Server ownership check
    if record.server_id != server_id {
        return RestoreResult::RecordNotFound(format!(
            "Record belongs to server '{}', not '{}'",
            record.server_id, server_id
        ));
    }

    // 7. Transaction ownership check
    if record.transaction_id != transaction_id {
        return RestoreResult::RecordNotFound(format!(
            "Record belongs to transaction '{}', not '{}'",
            record.transaction_id, transaction_id
        ));
    }

    // 8. Already restored? (idempotency)
    if record.status == QuarantineStatus::Restored {
        return RestoreResult::AlreadyRestored;
    }

    // 9. Artifact exists
    let artifact_path = quarantine_txn_dir.join("mods").join(&record.filename);
    if !artifact_path.exists() {
        return RestoreResult::MissingArtifact;
    }

    // 10. Quarantine source containment (Phase 3L.1 — validate metadata isn't lying)
    if !validate_quarantine_source_containment(&artifact_path, &quarantine_txn_dir) {
        return RestoreResult::QuarantineSourceEscape;
    }

    // 11. SHA verification
    let current_sha = match compute_file_sha256(&artifact_path) {
        Ok(h) => h,
        Err(_) => return RestoreResult::MissingArtifact,
    };
    if current_sha != record.sha256 {
        return RestoreResult::HashMismatch;
    }

    // 12. Identity verification — read JAR metadata, compare mod_ids
    let jar_metadata = crate::jar_metadata::read_jar_mod_metadata(&artifact_path);
    let mut expected_ids = record.mod_ids.clone();
    expected_ids.sort();
    let mut actual_ids = jar_metadata.mod_ids.clone();
    actual_ids.sort();
    if !expected_ids.is_empty() && !actual_ids.is_empty() && expected_ids != actual_ids {
        return RestoreResult::IdentityMismatch {
            expected: expected_ids,
            actual: actual_ids,
        };
    }

    // 13. Protected component check
    for mod_id in &record.mod_ids {
        if is_protected_component(mod_id) {
            return RestoreResult::ProtectedComponent {
                mod_id: mod_id.clone(),
            };
        }
    }

    // 14. Path containment — validate original_relative_path resolves inside live_path
    let destination = match validate_restore_destination(&record.original_relative_path, live_path)
    {
        Ok(d) => d,
        Err(e) => return e, // Already a RestoreResult (PathEscape, InvalidMetadata, or Failed)
    };

    // 15. Destination collision — file already exists
    if destination.exists() {
        return RestoreResult::DestinationConflict {
            existing_path: destination,
        };
    }

    // 16. Duplicate provider — another JAR in live/mods declares same mod_id
    let live_mods = live_path.join("mods");
    if let Err((conflicting_jar, mod_id)) = check_duplicate_providers(&live_mods, &record.mod_ids) {
        return RestoreResult::DuplicateProviderConflict {
            conflicting_jar,
            mod_id,
        };
    }

    // 17. ExplicitClientOnly check
    let compat = crate::mod_compat::classify_mod_local(&artifact_path);
    if matches!(
        compat.compatibility,
        crate::mod_compat::ServerCompatibility::ClientOnly
    ) && matches!(
        compat.confidence,
        crate::mod_compat::CompatibilityConfidence::Explicit
    ) {
        if let Some(first_id) = record.mod_ids.first() {
            return RestoreResult::ExplicitClientOnly {
                mod_id: first_id.clone(),
            };
        }
    }

    // 18. Ensure live/mods exists
    if let Err(e) = std::fs::create_dir_all(&live_mods) {
        return RestoreResult::Failed(format!("Failed to create live/mods: {}", e));
    }

    // 19. Atomic restore: copy → verify → TOCTOU re-check → rename
    let restore_sha = match atomic_restore_copy(
        &artifact_path,
        &destination,
        &record.sha256,
        record_id,
        server_id,
        lifecycle_provider,
    ) {
        Ok(sha) => sha,
        Err(restore_result) => {
            // Record failure in metadata
            meta.records[rec_idx].status = QuarantineStatus::RestoreFailed;
            let _ = save_quarantine_metadata(&quarantine_txn_dir, &meta);
            return restore_result;
        }
    };

    // 20. Update metadata: mark as restored
    meta.records[rec_idx].status = QuarantineStatus::Restored;
    meta.records[rec_idx].restored_at = Some(chrono::Utc::now().to_rfc3339());
    meta.records[rec_idx].restore_sha256 = Some(restore_sha.clone());
    meta.records[rec_idx].restore_target = Some(destination.clone());

    if let Err(e) = save_quarantine_metadata(&quarantine_txn_dir, &meta) {
        // Metadata update failed but file IS restored. Return explicit variant.
        return RestoreResult::RestoredMetadataUpdateFailed {
            sha256: restore_sha,
            target: destination,
        };
    }

    RestoreResult::Restored {
        sha256: restore_sha,
        target: destination,
    }
}

/// Restore a quarantined mod with explicit live_path (testable without config).
///
/// **Test/development only** — the production API `restore_quarantined_mod`
/// resolves paths internally. Frontend and Cloud clients must not provide
/// filesystem paths.
#[cfg(any(test, feature = "testing"))]
pub fn restore_quarantined_mod_at(
    server_id: &str,
    transaction_id: &str,
    record_id: &str,
    live_path: &Path,
    lifecycle_provider: &dyn RestoreLifecycleProvider,
) -> RestoreResult {
    restore_quarantined_mod_inner(
        server_id,
        transaction_id,
        record_id,
        live_path,
        lifecycle_provider,
    )
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
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/mymod.jar"), 1, &[], "");
        let fp2 = compute_fingerprint("txn1", "mymod", Path::new("mods/mymod.jar"), 1, &[], "");
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_mod_id() {
        let fp1 = compute_fingerprint("txn1", "modA", Path::new("mods/a.jar"), 1, &[], "");
        let fp2 = compute_fingerprint("txn1", "modB", Path::new("mods/a.jar"), 1, &[], "");
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_jar() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 1, &[], "");
        let fp2 = compute_fingerprint("txn1", "mymod", Path::new("mods/b.jar"), 1, &[], "");
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_transaction() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 1, &[], "");
        let fp2 = compute_fingerprint("txn2", "mymod", Path::new("mods/a.jar"), 1, &[], "");
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_changes_with_attempt() {
        let fp1 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 1, &[], "");
        let fp2 = compute_fingerprint("txn1", "mymod", Path::new("mods/a.jar"), 2, &[], "");
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
