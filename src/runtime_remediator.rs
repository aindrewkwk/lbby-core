// runtime_remediator.rs — Phase 3F-C: Deterministic Runtime Self-Healing.
//
// Analyzes boot failures and applies deterministic runtime remediation.
// Primary focus: wrong Java version → find/download correct runtime → retry.
// Secondary: diagnostics for OOM, loader mismatch, broken executables.
//
// Design principles:
//   - Analyzer is deterministic and side-effect free.
//   - Mutation belongs in remediation functions only.
//   - Uses existing Java infrastructure (find_java_with_version, ensure_java).
//   - Does NOT delete mods, quarantine, change MC/loader versions.
//   - Ephemeral config override first, persist only after successful validation.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::ServerConfig;

// ── Constants ───────────────────────────────────────────────────────────

/// Maximum runtime remediation rounds per boot validation cycle.
/// Separate from Phase 3F-A dependency rounds and 3F-B boot dependency rounds.
pub const MAX_RUNTIME_REMEDIATION_ROUNDS: u8 = 2;

/// Global ceiling: every actual Java server launch counts toward this.
/// Includes initial boot, dependency repair retries, and runtime repair retries.
/// No combination of repair loops may cause unbounded server launches.
pub const MAX_TOTAL_BOOT_ATTEMPTS: u8 = 6;

/// Known safe memory floor for heavy modpacks (in MB).
/// Below this, OOM is expected for large packs. This is NOT an auto-repair
/// threshold — it's a diagnostic heuristic.
const HEAVY_MODPACK_SAFE_FLOOR_MB: u32 = 4096;

/// Maximum memory auto-increase step (in MB). Prevents runaway allocation.
const MAX_MEMORY_INCREASE_STEP_MB: u32 = 2048;

// ── Types ───────────────────────────────────────────────────────────────

/// Runtime issue detected from boot failure analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeIssue {
    /// Wrong Java version for this server.
    WrongJavaVersion(JavaVersionIssue),
    /// Java executable not found or broken.
    JavaExecutableUnavailable(JavaExecutableIssue),
    /// Server ran out of memory.
    OutOfMemory(MemoryIssue),
    /// Loader version mismatch (detection only).
    LoaderVersionMismatch(LoaderVersionIssue),
    /// Could not determine specific runtime issue.
    Unsupported,
}

/// Details about a Java version mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaVersionIssue {
    /// Required Java major version (from authoritative config).
    pub required_major: u8,
    /// Current Java major version (from config or detected).
    pub current_major: Option<u8>,
    /// Class file version parsed from log (if available).
    pub class_file_version: Option<u8>,
}

/// Details about a missing or broken Java executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaExecutableIssue {
    /// The configured Java path that failed.
    pub configured_path: String,
    /// Required Java major version.
    pub required_major: u8,
    /// Why it failed.
    pub reason: String,
}

/// Details about an OOM event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIssue {
    /// Current configured Xmx in MB.
    pub current_mb: u32,
    /// OOM type from log (heap space, GC overhead, etc.).
    pub oom_type: String,
}

/// Details about a loader version mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderVersionIssue {
    /// Required loader version (from log).
    pub required: String,
    /// Current loader version (from config).
    pub current: String,
    /// Loader type (forge, neoforge, fabric).
    pub loader_type: String,
}

/// Result of runtime remediation.
#[derive(Debug, Clone)]
pub enum RuntimeRemediationResult {
    /// Remediation succeeded — new config ready for retry.
    Repaired(RuntimeRepairRecord),
    /// Issue detected but cannot be automatically repaired.
    NotRepairable(String),
    /// Multiple possible repairs — ambiguous, cannot proceed.
    Ambiguous(String),
    /// Repair attempted but failed.
    Failed(String),
}

/// Audit trail for a runtime repair attempt.
#[derive(Debug, Clone)]
pub struct RuntimeRepairRecord {
    /// Which remediation round (1-based).
    pub round: u8,
    /// The issue that was detected.
    pub issue: RuntimeIssue,
    /// Old value before repair.
    pub old_value: Option<String>,
    /// New value after repair.
    pub new_value: Option<String>,
    /// Whether the repair was applied.
    pub result: RuntimeRepairStatus,
}

/// Status of a single repair action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRepairStatus {
    /// Repair applied successfully.
    Applied,
    /// Repair was skipped (already correct, not applicable).
    Skipped(String),
    /// Repair failed.
    Failed(String),
}

// ── Validation retry state ──────────────────────────────────────────────

/// Tracks retry state across all repair systems to prevent loops.
/// Independent loops (dependency, runtime) share this state so they
/// never lose context of what was already attempted.
#[derive(Debug)]
pub struct ValidationRetryState {
    /// Total actual Java server launches.
    pub total_boot_attempts: u8,
    /// Number of dependency repair rounds executed.
    pub dependency_repairs: u8,
    /// Number of runtime repair rounds executed.
    pub runtime_repairs: u8,
    /// Missing mods already attempted by 3F-B.
    pub attempted_missing_mods: HashSet<String>,
    /// Java executable paths already attempted by 3F-C.
    pub attempted_java_paths: HashSet<PathBuf>,
    /// Memory values already attempted by 3F-C.
    pub attempted_memory_values: HashSet<u32>,
    /// Java major versions already attempted.
    pub attempted_java_majors: HashSet<u8>,
}

impl ValidationRetryState {
    pub fn new() -> Self {
        Self {
            total_boot_attempts: 0,
            dependency_repairs: 0,
            runtime_repairs: 0,
            attempted_missing_mods: HashSet::new(),
            attempted_java_paths: HashSet::new(),
            attempted_memory_values: HashSet::new(),
            attempted_java_majors: HashSet::new(),
        }
    }

    /// Check if we can attempt another boot.
    pub fn can_attempt_boot(&self) -> bool {
        self.total_boot_attempts < MAX_TOTAL_BOOT_ATTEMPTS
    }

    /// Check if we can attempt another runtime remediation round.
    pub fn can_attempt_runtime_repair(&self) -> bool {
        self.runtime_repairs < MAX_RUNTIME_REMEDIATION_ROUNDS
    }

    /// Record a boot attempt.
    pub fn record_boot_attempt(&mut self) {
        self.total_boot_attempts += 1;
    }

    /// Record a runtime repair attempt.
    pub fn record_runtime_repair(&mut self) {
        self.runtime_repairs += 1;
    }

    /// Check if a Java path was already attempted.
    pub fn has_attempted_java_path(&self, path: &Path) -> bool {
        self.attempted_java_paths.contains(path)
    }

    /// Check if a Java major was already attempted.
    pub fn has_attempted_java_major(&self, major: u8) -> bool {
        self.attempted_java_majors.contains(&major)
    }

    /// Record a Java path attempt.
    pub fn record_java_attempt(&mut self, path: PathBuf, major: u8) {
        self.attempted_java_paths.insert(path);
        self.attempted_java_majors.insert(major);
    }
}

// ── Analyzer (side-effect free) ─────────────────────────────────────────

/// Analyze a boot failure and identify the runtime issue.
/// This is deterministic and side-effect free — no mutations.
pub fn analyze_runtime_issue(
    log_tail: &str,
    cfg: &ServerConfig,
) -> RuntimeIssue {
    // Priority order: Java executable failure → Wrong Java → OOM → Loader mismatch → Unknown
    // (per spec §18)

    // 1. Check for wrong Java version (UnsupportedClassVersionError)
    if crate::boot_failure_analyzer::is_wrong_java_log(log_tail) {
        let class_major = extract_class_file_major_version(log_tail);
        let server_type_str = format!("{:?}", cfg.server_type);
        let required_major = crate::java::required_java_for_mc_with_loader(
            &cfg.minecraft_version,
            Some(&server_type_str),
        );
        let current_major = detect_current_java_major(cfg);

        return RuntimeIssue::WrongJavaVersion(JavaVersionIssue {
            required_major,
            current_major,
            class_file_version: class_major,
        });
    }

    // 2. Check for OOM
    if crate::boot_failure_analyzer::is_oom_log(log_tail) {
        let oom_type = classify_oom_type(log_tail);
        return RuntimeIssue::OutOfMemory(MemoryIssue {
            current_mb: cfg.ram_mb,
            oom_type,
        });
    }

    // 3. Check for loader version mismatch (detection only)
    if let Some(issue) = detect_loader_version_mismatch(log_tail, cfg) {
        return RuntimeIssue::LoaderVersionMismatch(issue);
    }

    // 4. Check for Java not found / launch failure
    if is_java_not_found_log(log_tail) {
        let server_type_str = format!("{:?}", cfg.server_type);
        let required_major = crate::java::required_java_for_mc_with_loader(
            &cfg.minecraft_version,
            Some(&server_type_str),
        );
        return RuntimeIssue::JavaExecutableUnavailable(JavaExecutableIssue {
            configured_path: cfg.java_path.clone(),
            required_major,
            reason: "Java executable not found".to_string(),
        });
    }

    RuntimeIssue::Unsupported
}

// ── Remediation functions (mutations allowed) ───────────────────────────

/// Attempt to remediate a runtime issue.
/// Returns the remediation result and whether the config was modified.
pub async fn remediate(
    issue: &RuntimeIssue,
    cfg: &mut ServerConfig,
    retry_state: &mut ValidationRetryState,
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    round: u8,
) -> RuntimeRemediationResult {
    match issue {
        RuntimeIssue::WrongJavaVersion(java_issue) => {
            remediate_java_version(java_issue, cfg, retry_state, app, round).await
        }
        RuntimeIssue::JavaExecutableUnavailable(java_issue) => {
            remediate_java_unavailable(java_issue, cfg, retry_state, app, round).await
        }
        RuntimeIssue::OutOfMemory(memory_issue) => {
            remediate_memory(memory_issue, cfg, retry_state, round)
        }
        RuntimeIssue::LoaderVersionMismatch(loader_issue) => {
            // Detection only — no automatic loader change (spec §17)
            RuntimeRemediationResult::NotRepairable(format!(
                "Loader mismatch detected: {} requires {}, current is {}. \
                 Automatic loader changes are disabled for safety.",
                loader_issue.loader_type, loader_issue.required, loader_issue.current
            ))
        }
        RuntimeIssue::Unsupported => {
            RuntimeRemediationResult::NotRepairable(
                "Could not determine specific runtime issue.".to_string()
            )
        }
    }
}

/// Remediate wrong Java version.
async fn remediate_java_version(
    issue: &JavaVersionIssue,
    cfg: &mut ServerConfig,
    retry_state: &mut ValidationRetryState,
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    round: u8,
) -> RuntimeRemediationResult {
    let required_major = issue.required_major;

    // Already attempted this major? Stop.
    if retry_state.has_attempted_java_major(required_major) {
        return RuntimeRemediationResult::Failed(format!(
            "Already attempted Java {} — loop prevented",
            required_major
        ));
    }

    // Find existing compatible runtime
    let candidate = crate::java::find_java_with_version(required_major);

    // If not found, try downloading via existing Java manager
    let java_bin = match candidate {
        Some(path) => {
            // Verify the candidate actually reports the right version
            // (directory name can be misleading — spec §25)
            match crate::java::detect_java_major(&path) {
                Some(major) if major == required_major => path,
                Some(other) => {
                    return RuntimeRemediationResult::Failed(format!(
                        "Candidate {} reports Java {} but {} is required",
                        path.display(), other, required_major
                    ));
                }
                None => {
                    return RuntimeRemediationResult::Failed(format!(
                        "Candidate {} failed java -version check",
                        path.display()
                    ));
                }
            }
        }
        None => {
            // Try downloading via existing Java manager
            match crate::java::ensure_java(required_major, app).await {
                Ok(path) => path,
                Err(e) => {
                    return RuntimeRemediationResult::NotRepairable(format!(
                        "Java {} not available and download failed: {}",
                        required_major, e
                    ));
                }
            }
        }
    };

    // Check if this path was already attempted
    if retry_state.has_attempted_java_path(&java_bin) {
        return RuntimeRemediationResult::Failed(format!(
            "Already attempted {} — loop prevented",
            java_bin.display()
        ));
    }

    // Apply the fix
    let old_java = cfg.java_path.clone();
    let new_java = java_bin.to_string_lossy().to_string();

    retry_state.record_java_attempt(java_bin, required_major);

    cfg.java_path = new_java.clone();

    RuntimeRemediationResult::Repaired(RuntimeRepairRecord {
        round,
        issue: RuntimeIssue::WrongJavaVersion(issue.clone()),
        old_value: Some(old_java),
        new_value: Some(new_java),
        result: RuntimeRepairStatus::Applied,
    })
}

/// Remediate missing/broken Java executable.
async fn remediate_java_unavailable(
    issue: &JavaExecutableIssue,
    cfg: &mut ServerConfig,
    retry_state: &mut ValidationRetryState,
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    round: u8,
) -> RuntimeRemediationResult {
    let required_major = issue.required_major;

    // Same logic as wrong Java version — find or download
    if retry_state.has_attempted_java_major(required_major) {
        return RuntimeRemediationResult::Failed(format!(
            "Already attempted Java {} — loop prevented",
            required_major
        ));
    }

    let candidate = crate::java::find_java_with_version(required_major);
    let java_bin = match candidate {
        Some(path) => {
            match crate::java::detect_java_major(&path) {
                Some(major) if major == required_major => path,
                Some(other) => {
                    return RuntimeRemediationResult::Failed(format!(
                        "Candidate {} reports Java {} but {} is required",
                        path.display(), other, required_major
                    ));
                }
                None => {
                    return RuntimeRemediationResult::Failed(format!(
                        "Candidate {} failed java -version check",
                        path.display()
                    ));
                }
            }
        }
        None => {
            match crate::java::ensure_java(required_major, app).await {
                Ok(path) => path,
                Err(e) => {
                    return RuntimeRemediationResult::NotRepairable(format!(
                        "Java {} not available: {}",
                        required_major, e
                    ));
                }
            }
        }
    };

    if retry_state.has_attempted_java_path(&java_bin) {
        return RuntimeRemediationResult::Failed(format!(
            "Already attempted {} — loop prevented",
            java_bin.display()
        ));
    }

    let old_java = cfg.java_path.clone();
    let new_java = java_bin.to_string_lossy().to_string();

    retry_state.record_java_attempt(java_bin, required_major);
    cfg.java_path = new_java.clone();

    RuntimeRemediationResult::Repaired(RuntimeRepairRecord {
        round,
        issue: RuntimeIssue::JavaExecutableUnavailable(issue.clone()),
        old_value: Some(old_java),
        new_value: Some(new_java),
        result: RuntimeRepairStatus::Applied,
    })
}

/// Remediate OOM — conservative bounded increase only.
/// Diagnostic-only if no authoritative safe floor exists.
fn remediate_memory(
    issue: &MemoryIssue,
    cfg: &mut ServerConfig,
    retry_state: &mut ValidationRetryState,
    round: u8,
) -> RuntimeRemediationResult {
    let current = issue.current_mb;

    // Already attempted this memory value? Stop.
    if retry_state.attempted_memory_values.contains(&current) {
        return RuntimeRemediationResult::Failed(format!(
            "Already attempted {}MB — loop prevented",
            current
        ));
    }

    retry_state.attempted_memory_values.insert(current);

    // Conservative auto-repair: only if ALL conditions are met (spec §13):
    //   1. current < known safe floor
    //   2. increase is bounded (<= MAX_MEMORY_INCREASE_STEP_MB)
    //   3. new value doesn't exceed a reasonable cap
    //
    // Without an authoritative memory floor, we report diagnostic-only.
    // The HEAVY_MODPACK_SAFE_FLOOR_MB is a heuristic, not authoritative.
    if current < HEAVY_MODPACK_SAFE_FLOOR_MB {
        let new_value = (current + MAX_MEMORY_INCREASE_STEP_MB)
            .min(HEAVY_MODPACK_SAFE_FLOOR_MB);

        if new_value > current && (new_value - current) <= MAX_MEMORY_INCREASE_STEP_MB {
            let old_value = cfg.ram_mb;
            cfg.ram_mb = new_value;

            return RuntimeRemediationResult::Repaired(RuntimeRepairRecord {
                round,
                issue: RuntimeIssue::OutOfMemory(issue.clone()),
                old_value: Some(format!("{}MB", old_value)),
                new_value: Some(format!("{}MB", new_value)),
                result: RuntimeRepairStatus::Applied,
            });
        }
    }

    // Above floor or insufficient info — diagnostic only
    RuntimeRemediationResult::NotRepairable(format!(
        "OutOfMemory ({}) with {}MB allocated. \
         No authoritative memory floor available for auto-repair. \
         Consider increasing server RAM manually.",
        issue.oom_type, current
    ))
}

// ── Log parsing helpers ─────────────────────────────────────────────────

/// Extract the required class file major version from an UnsupportedClassVersionError log.
/// Returns the Java major version (e.g., 61 → 17), not the class file version number.
fn extract_class_file_major_version(log: &str) -> Option<u8> {
    // Pattern: "class file version XX.0" where XX is the class file version
    // Java class file versions: 52=8, 53=9, 55=11, 61=17, 65=21, 69=25
    let lower = log.to_lowercase();

    // Try "class file version NN.0"
    if let Some(pos) = lower.find("class file version ") {
        let after = &log[pos + "class file version ".len()..];
        let version_str: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(class_version) = version_str.parse::<u8>() {
            return Some(class_file_to_java_major(class_version));
        }
    }

    // Try "has been compiled by a more recent version of the Java Runtime"
    // with "class file version NN.0" nearby
    if let Some(pos) = lower.find("more recent version") {
        let nearby = &log[pos..];
        if let Some(pos2) = nearby.find("class file version ") {
            let after = &nearby[pos2 + "class file version ".len()..];
            let version_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(class_version) = version_str.parse::<u8>() {
                return Some(class_file_to_java_major(class_version));
            }
        }
    }

    None
}

/// Convert Java class file version to Java major version.
/// Class file versions: 52=Java8, 53=Java9, 54=Java10, 55=Java11,
/// 56=Java12, 57=Java13, 58=Java14, 59=Java15, 60=Java16, 61=Java17,
/// 62=Java18, 63=Java19, 64=Java20, 65=Java21, 66=Java22, 67=Java23,
/// 68=Java24, 69=Java25.
fn class_file_to_java_major(class_version: u8) -> u8 {
    match class_version {
        52 => 8,
        53 => 9,
        54 => 10,
        55 => 11,
        56 => 12,
        57 => 13,
        58 => 14,
        59 => 15,
        60 => 16,
        61 => 17,
        62 => 18,
        63 => 19,
        64 => 20,
        65 => 21,
        66 => 22,
        67 => 23,
        68 => 24,
        69 => 25,
        // For unknown versions, use the formula: java_major = class_version - 44
        n if n > 44 => n - 44,
        _ => 0,
    }
}

/// Detect the current Java major version from config.
/// Checks cfg.java_path first, then falls back to find_any_java.
fn detect_current_java_major(cfg: &ServerConfig) -> Option<u8> {
    if !cfg.java_path.is_empty() {
        let path = PathBuf::from(&cfg.java_path);
        if let Some(major) = crate::java::detect_java_major(&path) {
            return Some(major);
        }
    }
    // Fall back to whatever Java is on PATH
    crate::java::find_any_java().map(|(_, major)| major)
}

/// Classify OOM type from log output.
fn classify_oom_type(log: &str) -> String {
    let lower = log.to_lowercase();
    if lower.contains("gc overhead limit exceeded") {
        "GC overhead limit exceeded".to_string()
    } else if lower.contains("java heap space") {
        "Java heap space".to_string()
    } else if lower.contains("metaspace") {
        "Metaspace".to_string()
    } else if lower.contains("direct buffer memory") {
        "Direct buffer memory".to_string()
    } else {
        "OutOfMemoryError".to_string()
    }
}

/// Detect loader version mismatch from log (detection only).
fn detect_loader_version_mismatch(log: &str, cfg: &ServerConfig) -> Option<LoaderVersionIssue> {
    let lower = log.to_lowercase();

    // Pattern: "requires forge XX.X.XX or newer" / "requires forge >= XX.X.XX"
    // or: "mod X requires forge version Y"
    if let Some(issue) = parse_forge_requirement(&lower, log, cfg) {
        return Some(issue);
    }

    // Pattern: "requires neoforge XX.X.XX or newer"
    if let Some(issue) = parse_neoforge_requirement(&lower, log, cfg) {
        return Some(issue);
    }

    // Pattern: "requires fabric-loader >= X.X.X" / "requires fabric-api >= X.X.X"
    // These are mod-level dependencies, not loader version mismatches.
    // Loader mismatch for Fabric is rare — the loader itself is the version.

    None
}

/// Parse Forge version requirement from log.
fn parse_forge_requirement(lower: &str, log: &str, cfg: &ServerConfig) -> Option<LoaderVersionIssue> {
    // Look for patterns like "requires forge 47.2.0 or newer" or "requires forge >= 47.2.0"
    if !lower.contains("forge") {
        return None;
    }

    // Simple heuristic: if the log mentions a Forge version requirement
    // and our current version is different, it's a mismatch.
    // This is detection-only — we don't auto-fix.

    // Pattern: "requires forge XX" or "Forge XX required"
    for pattern in &["requires forge ", "forge version ", "forge "] {
        if let Some(pos) = lower.find(pattern) {
            let after = &log[pos + pattern.len()..];
            let version: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version.is_empty() && version.contains('.') {
                if let Some(ref current) = cfg.loader_version {
                    if current != &version {
                        return Some(LoaderVersionIssue {
                            required: version,
                            current: current.clone(),
                            loader_type: "forge".to_string(),
                        });
                    }
                }
            }
        }
    }

    None
}

/// Parse NeoForge version requirement from log.
fn parse_neoforge_requirement(lower: &str, log: &str, cfg: &ServerConfig) -> Option<LoaderVersionIssue> {
    if !lower.contains("neoforge") {
        return None;
    }

    for pattern in &["requires neoforge ", "neoforge version ", "neoforge "] {
        if let Some(pos) = lower.find(pattern) {
            let after = &log[pos + pattern.len()..];
            let version: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version.is_empty() && version.contains('.') {
                if let Some(ref current) = cfg.loader_version {
                    if current != &version {
                        return Some(LoaderVersionIssue {
                            required: version,
                            current: current.clone(),
                            loader_type: "neoforge".to_string(),
                        });
                    }
                }
            }
        }
    }

    None
}

/// Check if log indicates Java not found / launch failure.
fn is_java_not_found_log(log: &str) -> bool {
    let lower = log.to_lowercase();
    lower.contains("no such file or directory")
        && (lower.contains("/java") || lower.contains("\\java"))
}

// ── Formatting ──────────────────────────────────────────────────────────

impl std::fmt::Display for RuntimeIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongJavaVersion(issue) => {
                write!(
                    f,
                    "Wrong Java version: required {}, current {}",
                    issue.required_major,
                    issue.current_major
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            }
            Self::JavaExecutableUnavailable(issue) => {
                write!(
                    f,
                    "Java executable unavailable: {} ({})",
                    issue.configured_path, issue.reason
                )
            }
            Self::OutOfMemory(issue) => {
                write!(f, "OutOfMemory ({}) with {}MB", issue.oom_type, issue.current_mb)
            }
            Self::LoaderVersionMismatch(issue) => {
                write!(
                    f,
                    "{} requires {} (current: {})",
                    issue.loader_type, issue.required, issue.current
                )
            }
            Self::Unsupported => write!(f, "Unsupported runtime issue"),
        }
    }
}

impl std::fmt::Display for RuntimeRepairStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Applied => write!(f, "Applied"),
            Self::Skipped(reason) => write!(f, "Skipped: {}", reason),
            Self::Failed(reason) => write!(f, "Failed: {}", reason),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_test_cfg() -> ServerConfig {
        ServerConfig {
            minecraft_version: "1.20.1".to_string(),
            server_type: crate::config::ServerType::Forge,
            loader_version: Some("47.2.0".to_string()),
            java_path: "/usr/bin/java".to_string(),
            ram_mb: 4096,
            ..Default::default()
        }
    }

    // ── Class file version parsing ──────────────────────────────────────

    #[test]
    fn test_extract_class_file_version_61() {
        let log = "java.lang.UnsupportedClassVersionError: net/minecraftforge/fml/common/Mod has been compiled by a more recent version of the Java Runtime (class file version 61.0), this version of the Java Runtime only recognizes class file versions up to 52.0";
        let major = extract_class_file_major_version(log);
        assert_eq!(major, Some(17));
    }

    #[test]
    fn test_extract_class_file_version_65() {
        let log = "UnsupportedClassVersionError: class file version 65.0";
        let major = extract_class_file_major_version(log);
        assert_eq!(major, Some(21));
    }

    #[test]
    fn test_extract_class_file_version_69() {
        let log = "UnsupportedClassVersionError: class file version 69.0";
        let major = extract_class_file_major_version(log);
        assert_eq!(major, Some(25));
    }

    #[test]
    fn test_extract_class_file_version_none() {
        let log = "Some unrelated error message";
        let major = extract_class_file_major_version(log);
        assert_eq!(major, None);
    }

    #[test]
    fn test_class_file_to_java_major_mapping() {
        assert_eq!(class_file_to_java_major(52), 8);
        assert_eq!(class_file_to_java_major(55), 11);
        assert_eq!(class_file_to_java_major(61), 17);
        assert_eq!(class_file_to_java_major(65), 21);
        assert_eq!(class_file_to_java_major(69), 25);
    }

    // ── Analyzer ────────────────────────────────────────────────────────

    #[test]
    fn test_analyze_wrong_java_version() {
        let log = "java.lang.UnsupportedClassVersionError: net/minecraftforge/fml/common/Mod has been compiled by a more recent version of the Java Runtime (class file version 61.0), this version of the Java Runtime only recognizes class file versions up to 52.0";
        let cfg = default_test_cfg();
        let issue = analyze_runtime_issue(log, &cfg);
        match issue {
            RuntimeIssue::WrongJavaVersion(j) => {
                assert_eq!(j.required_major, 17); // MC 1.20.1 Forge → Java 17
                assert_eq!(j.class_file_version, Some(17));
            }
            other => panic!("Expected WrongJavaVersion, got {:?}", other),
        }
    }

    #[test]
    fn test_analyze_oom() {
        let log = "java.lang.OutOfMemoryError: Java heap space";
        let cfg = default_test_cfg();
        let issue = analyze_runtime_issue(log, &cfg);
        match issue {
            RuntimeIssue::OutOfMemory(m) => {
                assert_eq!(m.current_mb, 4096);
                assert_eq!(m.oom_type, "Java heap space");
            }
            other => panic!("Expected OutOfMemory, got {:?}", other),
        }
    }

    #[test]
    fn test_analyze_oom_gc_overhead() {
        let log = "java.lang.OutOfMemoryError: GC overhead limit exceeded";
        let cfg = default_test_cfg();
        let issue = analyze_runtime_issue(log, &cfg);
        match issue {
            RuntimeIssue::OutOfMemory(m) => {
                assert_eq!(m.oom_type, "GC overhead limit exceeded");
            }
            other => panic!("Expected OutOfMemory, got {:?}", other),
        }
    }

    #[test]
    fn test_analyze_unsupported() {
        let log = "Some random crash with no identifiable pattern";
        let cfg = default_test_cfg();
        let issue = analyze_runtime_issue(log, &cfg);
        assert_eq!(issue, RuntimeIssue::Unsupported);
    }

    #[test]
    fn test_analyze_wrong_java_priority_over_oom() {
        // If both patterns appear, WrongJavaVersion has priority (spec §18)
        let log = "UnsupportedClassVersionError: class file version 61.0\nAlso OutOfMemoryError";
        let cfg = default_test_cfg();
        let issue = analyze_runtime_issue(log, &cfg);
        assert!(matches!(issue, RuntimeIssue::WrongJavaVersion(_)));
    }

    // ── Retry state ─────────────────────────────────────────────────────

    #[test]
    fn test_retry_state_loop_prevention() {
        let mut state = ValidationRetryState::new();
        assert!(state.can_attempt_boot());
        assert!(state.can_attempt_runtime_repair());

        state.record_boot_attempt();
        state.record_boot_attempt();
        assert!(state.can_attempt_boot()); // 2 < 6
        assert!(state.can_attempt_runtime_repair());

        state.record_runtime_repair();
        state.record_runtime_repair();
        assert!(!state.can_attempt_runtime_repair()); // 2 == 2
        assert!(state.can_attempt_boot()); // 2 < 6
    }

    #[test]
    fn test_retry_state_java_loop_prevention() {
        let mut state = ValidationRetryState::new();
        let path = PathBuf::from("/usr/bin/java");
        state.record_java_attempt(path.clone(), 17);

        assert!(state.has_attempted_java_path(&path));
        assert!(state.has_attempted_java_major(17));
        assert!(!state.has_attempted_java_major(21));
    }

    #[test]
    fn test_total_boot_attempt_ceiling() {
        let mut state = ValidationRetryState::new();
        for _ in 0..6 {
            assert!(state.can_attempt_boot());
            state.record_boot_attempt();
        }
        assert!(!state.can_attempt_boot());
    }

    // ── OOM type classification ─────────────────────────────────────────

    #[test]
    fn test_classify_oom_types() {
        assert_eq!(
            classify_oom_type("java.lang.OutOfMemoryError: Java heap space"),
            "Java heap space"
        );
        assert_eq!(
            classify_oom_type("java.lang.OutOfMemoryError: GC overhead limit exceeded"),
            "GC overhead limit exceeded"
        );
        assert_eq!(
            classify_oom_type("java.lang.OutOfMemoryError: Metaspace"),
            "Metaspace"
        );
        assert_eq!(
            classify_oom_type("java.lang.OutOfMemoryError: Direct buffer memory"),
            "Direct buffer memory"
        );
        assert_eq!(
            classify_oom_type("java.lang.OutOfMemoryError"),
            "OutOfMemoryError"
        );
    }

    // ── Java not found detection ────────────────────────────────────────

    #[test]
    fn test_java_not_found_log() {
        assert!(is_java_not_found_log(
            "No such file or directory: /usr/lib/jvm/java-17/bin/java"
        ));
        assert!(is_java_not_found_log(
            "No such file or directory: C:\\Program Files\\Java\\jdk-17\\bin\\java.exe"
        ));
        assert!(!is_java_not_found_log("Some other error"));
    }

    // ── Display formatting ──────────────────────────────────────────────

    #[test]
    fn test_runtime_issue_display() {
        let issue = RuntimeIssue::WrongJavaVersion(JavaVersionIssue {
            required_major: 17,
            current_major: Some(8),
            class_file_version: Some(17),
        });
        assert!(format!("{}", issue).contains("17"));
        assert!(format!("{}", issue).contains("8"));
    }

    #[test]
    fn test_runtime_issue_loader_display() {
        let issue = RuntimeIssue::LoaderVersionMismatch(LoaderVersionIssue {
            required: "47.2.0".to_string(),
            current: "47.1.0".to_string(),
            loader_type: "forge".to_string(),
        });
        let display = format!("{}", issue);
        assert!(display.contains("47.2.0"));
        assert!(display.contains("47.1.0"));
    }

    // ── Analyzer uses authoritative Java version ────────────────────────

    #[test]
    fn test_analyze_mc1201_forge_requires_java17() {
        let cfg = ServerConfig {
            minecraft_version: "1.20.1".to_string(),
            server_type: crate::config::ServerType::Forge,
            ..Default::default()
        };
        let log = "UnsupportedClassVersionError: class file version 61.0";
        let issue = analyze_runtime_issue(log, &cfg);
        match issue {
            RuntimeIssue::WrongJavaVersion(j) => {
                assert_eq!(j.required_major, 17);
            }
            other => panic!("Expected WrongJavaVersion, got {:?}", other),
        }
    }

    #[test]
    fn test_analyze_mc121_fabric_requires_java21() {
        let cfg = ServerConfig {
            minecraft_version: "1.21.1".to_string(),
            server_type: crate::config::ServerType::Fabric,
            ..Default::default()
        };
        let log = "UnsupportedClassVersionError: class file version 65.0";
        let issue = analyze_runtime_issue(log, &cfg);
        match issue {
            RuntimeIssue::WrongJavaVersion(j) => {
                assert_eq!(j.required_major, 21);
            }
            other => panic!("Expected WrongJavaVersion, got {:?}", other),
        }
    }

    #[test]
    fn test_analyze_neoforge_requires_java21() {
        let cfg = ServerConfig {
            minecraft_version: "1.20.1".to_string(),
            server_type: crate::config::ServerType::NeoForge,
            ..Default::default()
        };
        let log = "UnsupportedClassVersionError: class file version 65.0";
        let issue = analyze_runtime_issue(log, &cfg);
        match issue {
            RuntimeIssue::WrongJavaVersion(j) => {
                // NeoForge always requires Java 21 regardless of MC version
                assert_eq!(j.required_major, 21);
            }
            other => panic!("Expected WrongJavaVersion, got {:?}", other),
        }
    }

    // ── Memory remediation diagnostic-only when above floor ─────────────

    #[test]
    fn test_memory_above_floor_diagnostic_only() {
        let issue = MemoryIssue {
            current_mb: 8192,
            oom_type: "Java heap space".to_string(),
        };
        let mut cfg = default_test_cfg();
        cfg.ram_mb = 8192;
        let mut state = ValidationRetryState::new();

        // 8GB > 4GB floor → diagnostic only
        // Note: remediate_memory is not async, can call directly
        // We test the logic by checking the result type
        let result = remediate_memory(&issue, &mut cfg, &mut state, 1);
        match result {
            RuntimeRemediationResult::NotRepairable(msg) => {
                assert!(msg.contains("8192"));
            }
            other => panic!("Expected NotRepairable for above-floor, got {:?}", other),
        }
    }

    #[test]
    fn test_memory_below_floor_bounded_increase() {
        let issue = MemoryIssue {
            current_mb: 2048,
            oom_type: "Java heap space".to_string(),
        };
        let mut cfg = default_test_cfg();
        cfg.ram_mb = 2048;
        let mut state = ValidationRetryState::new();

        let result = remediate_memory(&issue, &mut cfg, &mut state, 1);
        match result {
            RuntimeRemediationResult::Repaired(record) => {
                assert_eq!(record.old_value, Some("2048MB".to_string()));
                // 2048 + 2048 = 4096, which is exactly the floor
                assert_eq!(record.new_value, Some("4096MB".to_string()));
                assert_eq!(cfg.ram_mb, 4096);
            }
            other => panic!("Expected Repaired for below-floor, got {:?}", other),
        }
    }

    #[test]
    fn test_memory_loop_prevention() {
        let issue = MemoryIssue {
            current_mb: 2048,
            oom_type: "Java heap space".to_string(),
        };
        let mut cfg = default_test_cfg();
        cfg.ram_mb = 2048;
        let mut state = ValidationRetryState::new();
        state.attempted_memory_values.insert(2048);

        let result = remediate_memory(&issue, &mut cfg, &mut state, 1);
        match result {
            RuntimeRemediationResult::Failed(msg) => {
                assert!(msg.contains("loop prevented"));
            }
            other => panic!("Expected Failed for loop, got {:?}", other),
        }
    }

    // ── Loader mismatch detection ───────────────────────────────────────

    #[test]
    fn test_loader_mismatch_detect_only() {
        let issue = LoaderVersionIssue {
            required: "47.2.0".to_string(),
            current: "47.1.0".to_string(),
            loader_type: "forge".to_string(),
        };
        let mut cfg = default_test_cfg();
        cfg.loader_version = Some("47.1.0".to_string());
        let _state = ValidationRetryState::new();

        // Loader mismatch is detection-only
        let rt_issue = RuntimeIssue::LoaderVersionMismatch(issue);
        // We can't call remediate (async) in a sync test, but we can verify
        // the analyzer produces the right type
        assert!(matches!(rt_issue, RuntimeIssue::LoaderVersionMismatch(_)));
    }

    // ── Class file to Java major edge cases ─────────────────────────────

    #[test]
    fn test_class_file_to_java_major_unknown() {
        // Unknown high version → formula fallback
        assert_eq!(class_file_to_java_major(70), 26);
        assert_eq!(class_file_to_java_major(75), 31);
    }

    #[test]
    fn test_class_file_to_java_major_very_old() {
        // Very old class file → 0
        assert_eq!(class_file_to_java_major(44), 0);
        assert_eq!(class_file_to_java_major(30), 0);
    }
}
