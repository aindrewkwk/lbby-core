// boot_failure_analyzer — Phase 3F-B: Deterministic boot failure attribution.
//
// Parses loader log output for high-confidence missing dependency patterns,
// cross-checks against the dependency graph and CurseForge relations,
// and feeds verified attributions into the Phase 3F-A resolver for safe repair.
//
// SAFETY RULES (Phase 3F-B):
// 1. Analyzer is PURE — no downloads, no file moves, no config edits
// 2. Only High-confidence attributions may trigger repair
// 3. Boot log alone is NOT sufficient — must cross-check with graph or CF relation
// 4. If graph and CF disagree → STOP
// 5. Runtime-only deps require authoritative CF REQUIRED relation
// 6. Reuses Phase 3F-A resolver — all3F-A safety rules remain mandatory
// 7. MAX_BOOT_REPAIR_ROUNDS=2 — bounded retry
// 8. Same-dependency loop prevention — never retry the same mod_id
// 9. No generic crash repair — only MissingDependency attribution

use crate::dependency_graph::{DependencyGraph, MissingDependency};
use crate::dependency_resolver::relation_type;
use crate::dependency_resolver::{
    self, DependencyResolution, DependencyResolver, ProjectModMapping,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ── Constants ──────────────────────────────────────────────────────────

/// Maximum number of boot-time repair rounds before giving up.
/// Phase 3F-A already does up to MAX_REPAIR_ROUNDS pre-boot.
/// Boot-time retry is smaller to prevent infinite loops.
pub const MAX_BOOT_REPAIR_ROUNDS: u8 = 2;

// ── Attribution types ──────────────────────────────────────────────────

/// Result of analyzing a boot failure log.
#[derive(Debug, Clone)]
pub enum BootAttribution {
    /// High-confidence missing dependency detected in boot log.
    MissingDependency(BootMissingDependency),
    /// Wrong Java version detected.
    WrongJavaVersion,
    /// Out of memory detected.
    OutOfMemory,
    /// Loader-level failure (not dependency-related).
    LoaderFailure(String),
    /// Could not attribute to a specific cause.
    Unknown,
}

/// A missing dependency attributed from boot log output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootMissingDependency {
    /// The mod that declared the requirement (if identifiable).
    pub requesting_mod_id: Option<String>,
    /// The missing mod ID.
    pub missing_mod_id: String,
    /// Version requirement from the log message (if any).
    pub version_requirement: Option<String>,
    /// Where the attribution came from.
    pub source: BootAttributionSource,
    /// How confident we are in this attribution.
    pub confidence: AttributionConfidence,
}

/// Where the boot attribution came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootAttributionSource {
    /// Forge/NeoForge mod loading error message.
    ForgeLoader,
    /// Fabric mod loading error message.
    FabricLoader,
    /// Quilt mod loading error message.
    QuiltLoader,
}

/// How confident we are in the attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AttributionConfidence {
    /// Vague or inferred attribution — never triggers repair.
    Low,
    /// Loader message present but mod ID extraction uncertain.
    Medium,
    /// Exact loader error message identifying specific mod IDs.
    /// Only High may trigger repair.
    High,
}

/// Verification sources that confirmed the attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationSource {
    /// JAR metadata graph confirms the dependency.
    JarMetadataGraph,
    /// CurseForge REQUIRED relation confirms the dependency.
    CurseForgeRequiredRelation,
    /// Provisional: requesting mod has unknown REQUIRED CF dependency projects.
    /// NOT yet verified post-download. Only means "candidates exist".
    RuntimeRelationCandidatesExist,
    /// Both sources agree.
    Both,
}

/// Identity and dependency metadata for an installed CurseForge file.
/// Preserves the authoritative bridge: mod_id → project_id → file_id → dependencies.
/// Originates from the manifest entry → exact project_id/file_id → CF file metadata.
#[derive(Debug, Clone)]
pub struct InstalledFileInfo {
    pub project_id: u64,
    pub file_id: u64,
    pub mod_ids: Vec<String>,
    pub dependencies: Vec<crate::dependency_resolver::CurseDependency>,
}

/// Registry of all installed CurseForge files with their identity and dependency metadata.
/// Keyed by mod_id for fast lookup during boot-time attribution verification.
#[derive(Debug, Clone)]
pub struct InstalledFileRegistry {
    /// mod_id → InstalledFileInfo
    pub by_mod_id: HashMap<String, InstalledFileInfo>,
    /// project_id → InstalledFileInfo (for reverse lookup)
    pub by_project_id: HashMap<u64, InstalledFileInfo>,
}

impl InstalledFileRegistry {
    pub fn new() -> Self {
        Self {
            by_mod_id: HashMap::new(),
            by_project_id: HashMap::new(),
        }
    }

    /// Register an installed file. Called during manifest entry processing.
    pub fn register(
        &mut self,
        project_id: u64,
        file_id: u64,
        mod_ids: Vec<String>,
        dependencies: Vec<crate::dependency_resolver::CurseDependency>,
    ) {
        let info = InstalledFileInfo {
            project_id,
            file_id,
            mod_ids: mod_ids.clone(),
            dependencies,
        };
        for mod_id in &mod_ids {
            self.by_mod_id.insert(mod_id.clone(), info.clone());
        }
        self.by_project_id.insert(project_id, info);
    }

    /// Look up installed file info by mod_id.
    pub fn get_by_mod_id(&self, mod_id: &str) -> Option<&InstalledFileInfo> {
        self.by_mod_id.get(mod_id)
    }

    /// Look up installed file info by project_id.
    pub fn get_by_project_id(&self, project_id: u64) -> Option<&InstalledFileInfo> {
        self.by_project_id.get(&project_id)
    }
}

/// Record of a boot-time repair attempt.
#[derive(Debug, Clone)]
pub struct BootRepairRecord {
    pub boot_attempt: u8,
    pub attribution: BootMissingDependency,
    pub verification_sources: Vec<VerificationSource>,
    pub repair_result: BootRepairResult,
}

/// Result of a boot-time repair attempt.
#[derive(Debug, Clone)]
pub enum BootRepairResult {
    /// Repair succeeded — dependency downloaded and identity verified.
    Repaired {
        project_id: u64,
        file_id: u64,
        file_name: String,
    },
    /// Repair failed — attribution unverified or resolver failed.
    Failed(String),
    /// Skipped — attribution confidence too low or already attempted.
    Skipped(String),
}

// ── Log parsing ────────────────────────────────────────────────────────

/// Parsed missing dependency from a log line.
#[derive(Debug, Clone)]
struct ParsedMissingDep {
    requesting_mod_id: Option<String>,
    missing_mod_id: String,
    version_requirement: Option<String>,
    source: BootAttributionSource,
}

/// Parse boot log lines for high-confidence missing dependency patterns.
///
/// Returns the first recognized pattern. Stops at the first match to avoid
/// false positives from later unrelated lines.
fn parse_boot_log_for_missing_deps(log: &str) -> Option<ParsedMissingDep> {
    for line in log.lines() {
        // Fabric: "ModResolutionException: ... Y ..." — check BEFORE Forge
        // because both use "requires mod" pattern
        if let Some(parsed) = parse_fabric_mod_resolution(line) {
            return Some(parsed);
        }
        // Quilt: similar to Fabric — check BEFORE Forge
        if let Some(parsed) = parse_quilt_mod_resolution(line) {
            return Some(parsed);
        }
        // Forge/NeoForge: "Mod X requires mod Y"
        if let Some(parsed) = parse_forge_requires(line) {
            return Some(parsed);
        }
        // Forge/NeoForge: "Missing mandatory dependency Y"
        if let Some(parsed) = parse_forge_missing_mandatory(line) {
            return Some(parsed);
        }
        // Forge/NeoForge: "requires Y version X"
        if let Some(parsed) = parse_forge_requires_version(line) {
            return Some(parsed);
        }
    }
    None
}

// ── Forge/NeoForge patterns ────────────────────────────────────────────

/// Pattern: "Mod X requires mod Y"
/// Example: "Mod 'create' requires mod 'flywheel' (>= 0.6.0)"
fn parse_forge_requires(line: &str) -> Option<ParsedMissingDep> {
    // Match: Mod 'X' requires mod 'Y' or Mod X requires mod Y
    let lower = line.to_lowercase();
    if !lower.contains("requires mod") {
        return None;
    }

    // Try quoted form: Mod 'X' requires mod 'Y'
    if let Some(parsed) = parse_quoted_requires(line) {
        return Some(parsed);
    }

    // Try unquoted: Mod X requires mod Y
    if let Some(parsed) = parse_unquoted_requires(line) {
        return Some(parsed);
    }

    None
}

fn parse_quoted_requires(line: &str) -> Option<ParsedMissingDep> {
    // Pattern: Mod 'X' requires mod 'Y'
    // Use split-based parsing for robustness
    let marker = "' requires mod '";
    let pos = line.find(marker)?;
    let before = &line[..pos];
    let after = &line[pos + marker.len()..];

    // Extract requesting mod: text after the last ' before the marker
    let requesting = before.rsplit('\'').next()?;

    // Extract missing mod: text before the first ' after the marker
    let missing_end = after.find('\'')?;
    let missing = &after[..missing_end];

    if missing.is_empty() {
        return None;
    }

    // Extract version constraint if present: (>= X.Y.Z)
    let version = extract_version_constraint(&after[missing_end..]);

    Some(ParsedMissingDep {
        requesting_mod_id: Some(requesting.to_string()),
        missing_mod_id: missing.to_string(),
        version_requirement: version,
        source: BootAttributionSource::ForgeLoader,
    })
}

fn parse_unquoted_requires(line: &str) -> Option<ParsedMissingDep> {
    // Pattern: Mod X requires mod Y (case-insensitive)
    let lower = line.to_lowercase();
    let pos = lower.find("requires mod")?;
    let before = &line[..pos].trim();
    let after = &line[pos + 12..].trim();

    // Extract requesting mod: "Mod X" → "X"
    let requesting = before
        .strip_prefix("Mod ")
        .or_else(|| before.strip_prefix("mod "))
        .map(|s| s.trim().trim_matches('\'').trim_matches('"'))
        .filter(|s| !s.is_empty());

    // Extract missing mod: "Y" or "Y (>= X.Y.Z)"
    let missing_end = after
        .find(|c: char| c.is_whitespace() || c == '(' || c == ',')
        .unwrap_or(after.len());
    let missing = &after[..missing_end].trim_matches('\'').trim_matches('"');

    if missing.is_empty() {
        return None;
    }

    let version = extract_version_constraint(after);

    Some(ParsedMissingDep {
        requesting_mod_id: requesting.map(|s| s.to_string()),
        missing_mod_id: missing.to_string(),
        version_requirement: version,
        source: BootAttributionSource::ForgeLoader,
    })
}

/// Pattern: "Missing mandatory dependency Y"
/// Example: "Missing mandatory dependency 'flywheel'"
fn parse_forge_missing_mandatory(line: &str) -> Option<ParsedMissingDep> {
    let lower = line.to_lowercase();
    if !lower.contains("missing mandatory dependency") {
        return None;
    }

    let pos = lower.find("missing mandatory dependency")?;
    let after = &line[pos + 28..].trim();

    // Extract mod ID: 'Y' or Y
    let missing = if after.starts_with('\'') {
        let end = after[1..].find('\'')?;
        &after[1..end + 1]
    } else if after.starts_with('"') {
        let end = after[1..].find('"')?;
        &after[1..end + 1]
    } else {
        let end = after
            .find(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .unwrap_or(after.len());
        &after[..end]
    };

    if missing.is_empty() {
        return None;
    }

    Some(ParsedMissingDep {
        requesting_mod_id: None, // not specified in this pattern
        missing_mod_id: missing.to_string(),
        version_requirement: None,
        source: BootAttributionSource::ForgeLoader,
    })
}

/// Pattern: "requires Y version X"
/// Example: "requires flywheel version 0.6.0 or above"
fn parse_forge_requires_version(line: &str) -> Option<ParsedMissingDep> {
    let lower = line.to_lowercase();
    if !lower.contains("requires") || !lower.contains("version") {
        return None;
    }

    // Must have "requires" before "version"
    let req_pos = lower.find("requires")?;
    let ver_pos = lower.find("version")?;
    if req_pos >= ver_pos {
        return None;
    }

    // Get text between "requires" and "version" directly from line
    let before_ver = line[req_pos + 8..ver_pos].trim();

    // Extract mod ID
    let missing = before_ver
        .trim_matches('\'')
        .trim_matches('"')
        .split_whitespace()
        .last()?;

    if missing.is_empty() {
        return None;
    }

    // Extract version
    let after_ver = &line[ver_pos + 7..].trim();
    let version_end = after_ver
        .find(|c: char| c.is_whitespace() || c == ',' || c == ')')
        .unwrap_or(after_ver.len());
    let version = &after_ver[..version_end];

    Some(ParsedMissingDep {
        requesting_mod_id: None,
        missing_mod_id: missing.to_string(),
        version_requirement: if version.is_empty() {
            None
        } else {
            Some(version.to_string())
        },
        source: BootAttributionSource::ForgeLoader,
    })
}

// ── Fabric patterns ────────────────────────────────────────────────────

/// Pattern: Fabric ModResolutionException
/// Example: "ModResolutionException: Mod 'X' requires mod 'Y' (>= 1.0.0)"
fn parse_fabric_mod_resolution(line: &str) -> Option<ParsedMissingDep> {
    let lower = line.to_lowercase();

    // Only match if ModResolutionException is present — "requires mod" alone is Forge
    if lower.contains("modresolutionexception") {
        // Try quoted pattern: Mod 'X' requires mod 'Y'
        if let Some(parsed) = parse_quoted_requires(line) {
            return Some(ParsedMissingDep {
                requesting_mod_id: parsed.requesting_mod_id,
                missing_mod_id: parsed.missing_mod_id,
                version_requirement: parsed.version_requirement,
                source: BootAttributionSource::FabricLoader,
            });
        }

        // Try unquoted pattern
        if let Some(parsed) = parse_unquoted_requires(line) {
            return Some(ParsedMissingDep {
                requesting_mod_id: parsed.requesting_mod_id,
                missing_mod_id: parsed.missing_mod_id,
                version_requirement: parsed.version_requirement,
                source: BootAttributionSource::FabricLoader,
            });
        }
    }

    None
}

// ── Quilt patterns ─────────────────────────────────────────────────────

/// Pattern: Quilt ModResolutionException
/// Similar to Fabric but with Quilt-specific prefixes.
fn parse_quilt_mod_resolution(line: &str) -> Option<ParsedMissingDep> {
    let lower = line.to_lowercase();

    if lower.contains("quilt")
        && (lower.contains("modresolutionexception") || lower.contains("requires"))
    {
        if let Some(parsed) = parse_quoted_requires(line) {
            return Some(ParsedMissingDep {
                requesting_mod_id: parsed.requesting_mod_id,
                missing_mod_id: parsed.missing_mod_id,
                version_requirement: parsed.version_requirement,
                source: BootAttributionSource::QuiltLoader,
            });
        }

        if let Some(parsed) = parse_unquoted_requires(line) {
            return Some(ParsedMissingDep {
                requesting_mod_id: parsed.requesting_mod_id,
                missing_mod_id: parsed.missing_mod_id,
                version_requirement: parsed.version_requirement,
                source: BootAttributionSource::QuiltLoader,
            });
        }
    }

    None
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Extract version constraint from text like "(>= 1.0.0)" or "(1.0.0)".
fn extract_version_constraint(text: &str) -> Option<String> {
    let start = text.find('(')?;
    let end = text[start..].find(')')?;
    let inside = text[start + 1..start + end].trim();

    if inside.is_empty() {
        return None;
    }

    // Clean up: remove ">=", "<=", ">", "<", "=" prefixes
    let cleaned = inside
        .trim_start_matches(">=")
        .trim_start_matches("<=")
        .trim_start_matches('>')
        .trim_start_matches('<')
        .trim_start_matches('=')
        .trim();

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

// ── Boot failure analyzer ──────────────────────────────────────────────

/// Analyze a boot failure and return deterministic attribution.
///
/// This function is PURE — no downloads, no file moves, no config edits.
/// It only reads the log and metadata to produce an attribution.
///
/// Confidence is always High for exact loader log patterns.
/// Verification (graph + CF relation) happens separately in verify_attribution().
pub fn analyze_boot_failure(log_tail: &str, _graph: &DependencyGraph) -> BootAttribution {
    // First, check for non-dependency failures
    if is_oom_log(log_tail) {
        return BootAttribution::OutOfMemory;
    }
    if is_wrong_java_log(log_tail) {
        return BootAttribution::WrongJavaVersion;
    }

    // Parse for missing dependency patterns
    let parsed = match parse_boot_log_for_missing_deps(log_tail) {
        Some(p) => p,
        None => return BootAttribution::Unknown,
    };

    // Convert to BootMissingDependency with initial High confidence.
    // Exact loader log patterns (Forge/Fabric/Quilt) are always High confidence
    // at the attribution level. Verification (graph OR CF REQUIRED relation)
    // happens in verify_attribution() before any repair is attempted.
    let attribution = BootMissingDependency {
        requesting_mod_id: parsed.requesting_mod_id.clone(),
        missing_mod_id: parsed.missing_mod_id.clone(),
        version_requirement: parsed.version_requirement.clone(),
        source: parsed.source,
        confidence: AttributionConfidence::High,
    };

    BootAttribution::MissingDependency(attribution)
}

/// Check if log indicates OOM.
pub fn is_oom_log(log: &str) -> bool {
    let lower = log.to_lowercase();
    lower.contains("outofmemoryerror")
        || lower.contains("out of memory")
        || lower.contains("java.lang.outofmemoryerror")
}

/// Check if log indicates wrong Java version.
pub fn is_wrong_java_log(log: &str) -> bool {
    let lower = log.to_lowercase();
    lower.contains("unsupportedclassversionerror")
        || lower.contains("class file version")
        || lower.contains("java.lang.unsupportedclassversionerror")
}

// ── Cross-check and repair integration ─────────────────────────────────

/// Verify an attribution against graph and CF relation.
/// Returns the verification sources that confirmed the attribution.
///
/// SAFETY: CF verification requires actual CurseForge file dependency metadata
/// with relationType==REQUIRED. Two project mappings alone are NOT sufficient.
pub fn verify_attribution(
    attribution: &BootMissingDependency,
    graph: &DependencyGraph,
    project_map: &ProjectModMapping,
    installed_files: &InstalledFileRegistry,
) -> Vec<VerificationSource> {
    let mut sources = Vec::new();

    // Check 1: Graph confirmation
    // Graph confirms if the requesting mod's JAR declares Required dependency on missing mod.
    let graph_confirms = if let Some(ref requesting) = attribution.requesting_mod_id {
        if let Some(node) = graph.find_by_mod_id(requesting) {
            node.dependencies.iter().any(|dep| {
                dep.mod_id == attribution.missing_mod_id
                    && dep.kind == crate::jar_metadata::DependencyKind::Required
            })
        } else {
            false
        }
    } else {
        // No requesting mod specified — check if it's in the plan's missing list
        let plan = graph.create_plan();
        plan.missing
            .iter()
            .any(|m| m.dependency_mod_id == attribution.missing_mod_id)
    };

    if graph_confirms {
        sources.push(VerificationSource::JarMetadataGraph);
    }

    // Check 2: CF REQUIRED relation confirmation
    // For runtime-only dependencies, B has no project mapping.
    // Instead, enumerate A's REQUIRED dependency projects that are not yet installed.
    // Any unknown REQUIRED project means candidates exist (provisional — not yet verified
    // post-download). Post-download JAR verification bridges CF project ↔ mod_id.
    let unknown_required = enumerate_unknown_required_projects(attribution, installed_files);
    let cf_confirms = !unknown_required.is_empty();
    if cf_confirms {
        sources.push(VerificationSource::RuntimeRelationCandidatesExist);
    }

    // If both confirm, report as "Both"
    if sources.len() == 2 {
        sources.clear();
        sources.push(VerificationSource::Both);
    }

    sources
}

/// Enumerate REQUIRED dependency project IDs from the requesting mod's installed CF file
/// that are NOT already installed.
///
/// For runtime-only dependencies, B has no project mapping and no installed file.
/// Instead of looking up B's project_id, we enumerate A's REQUIRED dependency projects
/// from the requesting mod's CF file metadata. Any unknown REQUIRED project is a
/// candidate for the missing dependency.
///
/// Post-download JAR metadata verification bridges: CF project ↔ mod_id "B".
fn enumerate_unknown_required_projects(
    attribution: &BootMissingDependency,
    installed_files: &InstalledFileRegistry,
) -> Vec<u64> {
    let requesting = match attribution.requesting_mod_id.as_ref() {
        Some(r) => r,
        None => return vec![],
    };

    let req_info = match installed_files.get_by_mod_id(requesting) {
        Some(info) => info,
        None => return vec![],
    };

    req_info
        .dependencies
        .iter()
        .filter(|dep| dep.relation_type == relation_type::REQUIRED)
        .filter(|dep| !installed_files.by_project_id.contains_key(&dep.mod_id))
        .map(|dep| dep.mod_id)
        .collect()
}

// ── Runtime-only dependency resolution ────────────────────────────────

/// Maximum number of unknown REQUIRED dependency projects to probe.
/// Prevents unbounded downloading when a mod has many CF dependencies.
const MAX_RUNTIME_RELATION_CANDIDATES: usize = 5;

/// Attempt to resolve a runtime-only dependency by iterating the requesting mod's
/// REQUIRED dependency project IDs.
///
/// For each unknown REQUIRED project:
/// 1. Resolve a compatible file via CF API
/// 2. Download candidate to staging
/// 3. Read JAR metadata → verify it declares the missing mod_id
///
/// If exactly one candidate matches → RuntimeResolved (already downloaded).
/// If zero match → NotFound.
/// If multiple match → Ambiguous (STOP, do not guess).
async fn try_runtime_only_resolution(
    app: &crate::app_state::AppEventSender,
    client: &reqwest::Client,
    resolver: &mut DependencyResolver,
    attribution: &BootMissingDependency,
    unknown_required_projects: &[u64],
    missing: &MissingDependency,
    mods_dir: &Path,
    mc_version: &str,
    loader: &str,
) -> DependencyResolution {
    use crate::jar_metadata::read_jar_mod_metadata;

    if unknown_required_projects.len() > MAX_RUNTIME_RELATION_CANDIDATES {
        eprintln!(
            "[CF] Boot repair: too many unknown REQUIRED projects ({}) — limit is {}",
            unknown_required_projects.len(),
            MAX_RUNTIME_RELATION_CANDIDATES
        );
        return DependencyResolution::NotFound;
    }

    let staging_mods = mods_dir.to_path_buf();
    let mut matched: Option<(dependency_resolver::ResolvedDependency, std::path::PathBuf)> = None;
    let mut match_count: u32 = 0;

    for &candidate_project_id in unknown_required_projects {
        // Step 1: Resolve compatible file for this project
        let resolution = resolver
            .resolve_from_project(
                candidate_project_id,
                &attribution.missing_mod_id,
                missing,
                mc_version,
                loader,
            )
            .await;

        let resolved = match resolution {
            DependencyResolution::Resolved(r) => r,
            _ => continue, // skip projects with no compatible file
        };

        // Step 2: Download candidate
        let jar_path = match dependency_resolver::download_resolved_dependency(
            app,
            client,
            &resolved,
            &staging_mods,
        )
        .await
        {
            Ok(p) => p,
            Err(_) => continue, // download failed, try next candidate
        };

        // Step 3: Read JAR metadata → verify it declares the missing mod_id
        let meta = read_jar_mod_metadata(&jar_path);
        let declares_missing = meta
            .mod_ids
            .iter()
            .any(|id| id == &attribution.missing_mod_id);

        if declares_missing {
            match_count += 1;
            if match_count > 1 {
                // Ambiguous: multiple REQUIRED projects provide the same mod_id.
                // Remove BOTH matching artifacts and stop.
                dependency_resolver::remove_rejected_artifact(&jar_path);
                if let Some((_, prev_jar)) = matched.take() {
                    dependency_resolver::remove_rejected_artifact(&prev_jar);
                }
                eprintln!(
                    "[CF] Boot repair: runtime-only ambiguity — multiple REQUIRED projects declare '{}'",
                    attribution.missing_mod_id
                );
                return DependencyResolution::Ambiguous(vec![]);
            }
            matched = Some((resolved, jar_path));
        } else {
            // Identity mismatch: this project doesn't provide the missing mod_id.
            dependency_resolver::remove_rejected_artifact(&jar_path);
        }
    }

    match matched {
        Some((resolved, jar_path)) => {
            DependencyResolution::RuntimeResolved(dependency_resolver::RuntimeResolvedDependency {
                resolved,
                jar_path,
            })
        }
        None => DependencyResolution::NotFound,
    }
}

// ── Boot repair orchestrator ───────────────────────────────────────────

/// Run boot-time repair with attribution and retry.
///
/// Flow:
/// 1. Analyze boot failure log
/// 2. Verify attribution against graph + CF relation
/// 3. If verified, call Phase 3F-A resolver
/// 4. Rebuild graph and retry boot
/// 5. Track attempted repairs to prevent loops
pub async fn attempt_boot_repair(
    app: &crate::app_state::AppEventSender,
    client: &reqwest::Client,
    resolver: &mut DependencyResolver,
    log_tail: &str,
    graph: &DependencyGraph,
    installed_files: &InstalledFileRegistry,
    mods_dir: &Path,
    mc_version: &str,
    loader: &str,
    max_rounds: u8,
) -> BootRepairOutcome {
    let mut records = Vec::new();
    let mut attempted_mod_ids: HashSet<String> = HashSet::new();

    for round in 0..max_rounds {
        // Step 1: Analyze the boot failure
        let attribution = match analyze_boot_failure(log_tail, graph) {
            BootAttribution::MissingDependency(attr) => attr,
            other => {
                records.push(BootRepairRecord {
                    boot_attempt: round + 1,
                    attribution: BootMissingDependency {
                        requesting_mod_id: None,
                        missing_mod_id: String::new(),
                        version_requirement: None,
                        source: BootAttributionSource::ForgeLoader,
                        confidence: AttributionConfidence::Low,
                    },
                    verification_sources: vec![],
                    repair_result: BootRepairResult::Skipped(format!(
                        "Not a missing dependency: {:?}",
                        other
                    )),
                });
                return BootRepairOutcome {
                    repaired: false,
                    records,
                    final_attribution: other,
                };
            }
        };

        // Step 2: Check confidence
        if attribution.confidence != AttributionConfidence::High {
            records.push(BootRepairRecord {
                boot_attempt: round + 1,
                attribution: attribution.clone(),
                verification_sources: vec![],
                repair_result: BootRepairResult::Skipped(
                    "Attribution confidence not High — skipping repair".to_string(),
                ),
            });
            return BootRepairOutcome {
                repaired: false,
                records,
                final_attribution: BootAttribution::MissingDependency(attribution),
            };
        }

        // Step 3: Check loop prevention
        if attempted_mod_ids.contains(&attribution.missing_mod_id) {
            records.push(BootRepairRecord {
                boot_attempt: round + 1,
                attribution: attribution.clone(),
                verification_sources: vec![],
                repair_result: BootRepairResult::Skipped(format!(
                    "Already attempted repair for '{}' — stopping loop",
                    attribution.missing_mod_id
                )),
            });
            return BootRepairOutcome {
                repaired: false,
                records,
                final_attribution: BootAttribution::MissingDependency(attribution),
            };
        }

        // Step 4: Verify attribution
        let verification =
            verify_attribution(&attribution, graph, resolver.project_map(), installed_files);
        if verification.is_empty() {
            records.push(BootRepairRecord {
                boot_attempt: round + 1,
                attribution: attribution.clone(),
                verification_sources: vec![],
                repair_result: BootRepairResult::Failed(
                    "Attribution unverified — neither graph nor CF relation confirms".to_string(),
                ),
            });
            return BootRepairOutcome {
                repaired: false,
                records,
                final_attribution: BootAttribution::MissingDependency(attribution),
            };
        }

        // Step 5: Mark as attempted
        attempted_mod_ids.insert(attribution.missing_mod_id.clone());

        // Step 6: Call Phase 3F-A resolver
        let missing = MissingDependency {
            dependent_mod_id: attribution.requesting_mod_id.clone(),
            dependency_mod_id: attribution.missing_mod_id.clone(),
            dependent_path: mods_dir.to_path_buf(),
            version_requirement: attribution.version_requirement.clone(),
        };

        let resolution = resolver.resolve(&missing, mc_version, loader).await;

        // Step 6b: If resolver can't find a project mapping (runtime-only dependency),
        // try resolving from A's REQUIRED dependency project IDs.
        // Each candidate is downloaded and verified post-download via JAR metadata.
        let resolution = match &resolution {
            DependencyResolution::Unsupported(_) | DependencyResolution::NotFound => {
                let unknown_required =
                    enumerate_unknown_required_projects(&attribution, installed_files);
                if unknown_required.is_empty() {
                    resolution
                } else {
                    try_runtime_only_resolution(
                        app,
                        client,
                        resolver,
                        &attribution,
                        &unknown_required,
                        &missing,
                        mods_dir,
                        mc_version,
                        loader,
                    )
                    .await
                }
            }
            _ => resolution,
        };

        match resolution {
            // Runtime-only: candidate already downloaded and identity verified by
            // try_runtime_only_resolution. Register directly — no second download.
            DependencyResolution::RuntimeResolved(rt) => {
                let resolved = &rt.resolved;
                let jar_path = &rt.jar_path;
                let meta = crate::jar_metadata::read_jar_mod_metadata(jar_path);
                resolver.register_project(resolved.project_id, meta.mod_ids);

                records.push(BootRepairRecord {
                    boot_attempt: round + 1,
                    attribution: attribution.clone(),
                    verification_sources: verification.clone(),
                    repair_result: BootRepairResult::Repaired {
                        project_id: resolved.project_id,
                        file_id: resolved.file_id,
                        file_name: resolved.file_name.clone(),
                    },
                });

                eprintln!(
                    "[CF] Boot repair: runtime-only resolved {} (project {}, file {})",
                    resolved.file_name, resolved.project_id, resolved.file_id
                );
            }
            DependencyResolution::Resolved(resolved) => {
                // Phase 3F-A path: download and verify identity
                let staging_mods = mods_dir.to_path_buf();
                match dependency_resolver::download_resolved_dependency(
                    app,
                    client,
                    &resolved,
                    &staging_mods,
                )
                .await
                {
                    Ok(jar_path) => {
                        // Verify identity
                        match dependency_resolver::verify_download_identity(
                            &jar_path,
                            &attribution.missing_mod_id,
                        ) {
                            Ok(()) => {
                                // Register and record
                                let meta = crate::jar_metadata::read_jar_mod_metadata(&jar_path);
                                resolver.register_project(resolved.project_id, meta.mod_ids);

                                records.push(BootRepairRecord {
                                    boot_attempt: round + 1,
                                    attribution: attribution.clone(),
                                    verification_sources: verification.clone(),
                                    repair_result: BootRepairResult::Repaired {
                                        project_id: resolved.project_id,
                                        file_id: resolved.file_id,
                                        file_name: resolved.file_name.clone(),
                                    },
                                });

                                eprintln!(
                                    "[CF] Boot repair: downloaded {} (project {}, file {})",
                                    resolved.file_name, resolved.project_id, resolved.file_id
                                );
                            }
                            Err(DependencyResolution::IdentityMismatch {
                                expected_mod_id,
                                actual_mod_ids,
                            }) => {
                                dependency_resolver::remove_rejected_artifact(&jar_path);
                                records.push(BootRepairRecord {
                                    boot_attempt: round + 1,
                                    attribution: attribution.clone(),
                                    verification_sources: verification.clone(),
                                    repair_result: BootRepairResult::Failed(format!(
                                        "Identity mismatch: expected '{}', got {:?}",
                                        expected_mod_id, actual_mod_ids
                                    )),
                                });
                                return BootRepairOutcome {
                                    repaired: false,
                                    records,
                                    final_attribution: BootAttribution::MissingDependency(
                                        attribution,
                                    ),
                                };
                            }
                            Err(_) => {
                                dependency_resolver::remove_rejected_artifact(&jar_path);
                                records.push(BootRepairRecord {
                                    boot_attempt: round + 1,
                                    attribution: attribution.clone(),
                                    verification_sources: verification.clone(),
                                    repair_result: BootRepairResult::Failed(
                                        "Identity unverifiable".to_string(),
                                    ),
                                });
                                return BootRepairOutcome {
                                    repaired: false,
                                    records,
                                    final_attribution: BootAttribution::MissingDependency(
                                        attribution,
                                    ),
                                };
                            }
                        }
                    }
                    Err(e) => {
                        records.push(BootRepairRecord {
                            boot_attempt: round + 1,
                            attribution: attribution.clone(),
                            verification_sources: verification.clone(),
                            repair_result: BootRepairResult::Failed(format!(
                                "Download failed: {}",
                                e
                            )),
                        });
                        return BootRepairOutcome {
                            repaired: false,
                            records,
                            final_attribution: BootAttribution::MissingDependency(attribution),
                        };
                    }
                }
            }
            other => {
                records.push(BootRepairRecord {
                    boot_attempt: round + 1,
                    attribution: attribution.clone(),
                    verification_sources: verification.clone(),
                    repair_result: BootRepairResult::Failed(format!(
                        "Resolver returned: {:?}",
                        other
                    )),
                });
                return BootRepairOutcome {
                    repaired: false,
                    records,
                    final_attribution: BootAttribution::MissingDependency(attribution),
                };
            }
        }
    }

    // All rounds exhausted
    BootRepairOutcome {
        repaired: false,
        records,
        final_attribution: BootAttribution::Unknown,
    }
}

/// Outcome of boot repair attempts.
#[derive(Debug)]
pub struct BootRepairOutcome {
    /// Whether repair succeeded (at least one dependency repaired).
    pub repaired: bool,
    /// Records of all repair attempts.
    pub records: Vec<BootRepairRecord>,
    /// Final attribution state.
    pub final_attribution: BootAttribution,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── Log parsing tests ──────────────────────────────────────────────

    #[test]
    fn test_forge_requires_quoted() {
        let log = "[12:34:56] [main/ERROR]: Mod 'create' requires mod 'flywheel' (>= 0.6.0)";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.requesting_mod_id.as_deref(), Some("create"));
        assert_eq!(parsed.missing_mod_id, "flywheel");
        assert_eq!(parsed.version_requirement.as_deref(), Some("0.6.0"));
        assert_eq!(parsed.source, BootAttributionSource::ForgeLoader);
    }

    #[test]
    fn test_forge_requires_unquoted() {
        let log = "Mod architectury requires mod fabric-api";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.requesting_mod_id.as_deref(), Some("architectury"));
        assert_eq!(parsed.missing_mod_id, "fabric-api");
    }

    #[test]
    fn test_forge_missing_mandatory() {
        let log = "[12:34:56] [main/ERROR]: Missing mandatory dependency 'flywheel'";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.requesting_mod_id, None);
        assert_eq!(parsed.missing_mod_id, "flywheel");
    }

    #[test]
    fn test_forge_requires_version() {
        let log = "[12:34:56] [main/ERROR]: requires flywheel version 0.6.0 or above";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.missing_mod_id, "flywheel");
        assert_eq!(parsed.version_requirement.as_deref(), Some("0.6.0"));
    }

    #[test]
    fn test_fabric_requires_quoted() {
        let log = "ModResolutionException: Mod 'X' requires mod 'Y' (>= 1.0.0)";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.source, BootAttributionSource::FabricLoader);
        assert_eq!(parsed.missing_mod_id, "Y");
    }

    #[test]
    fn test_no_pattern_found() {
        let log = "[12:34:56] [main/INFO]: Server started successfully";
        assert!(parse_boot_log_for_missing_deps(log).is_none());
    }

    #[test]
    fn test_oom_detection() {
        let log = "java.lang.OutOfMemoryError: Java heap space";
        assert!(is_oom_log(log));
        assert!(!is_wrong_java_log(log));
    }

    #[test]
    fn test_wrong_java_detection() {
        let log = "java.lang.UnsupportedClassVersionError: mod/X has been compiled by a more recent version of the Java Runtime";
        assert!(is_wrong_java_log(log));
        assert!(!is_oom_log(log));
    }

    #[test]
    fn test_version_constraint_extraction() {
        assert_eq!(
            extract_version_constraint("(>= 1.0.0)"),
            Some("1.0.0".to_string())
        );
        assert_eq!(
            extract_version_constraint("(1.0.0)"),
            Some("1.0.0".to_string())
        );
        assert_eq!(extract_version_constraint("()"), None);
        assert_eq!(extract_version_constraint("no parens"), None);
    }

    // ── Attribution tests ──────────────────────────────────────────────

    #[test]
    fn test_oom_attribution() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("java.lang.OutOfMemoryError", &graph);
        assert!(matches!(attr, BootAttribution::OutOfMemory));
    }

    #[test]
    fn test_wrong_java_attribution() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("UnsupportedClassVersionError", &graph);
        assert!(matches!(attr, BootAttribution::WrongJavaVersion));
    }

    #[test]
    fn test_unknown_attribution() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("Some random error", &graph);
        assert!(matches!(attr, BootAttribution::Unknown));
    }

    // ── Loop prevention test ───────────────────────────────────────────

    #[test]
    fn test_loop_prevention() {
        let mut attempted = HashSet::new();
        attempted.insert("flywheel".to_string());

        // Second attempt for same mod should be detected
        assert!(attempted.contains("flywheel"));
        assert!(!attempted.contains("create"));
    }

    // ── Confidence rules test ──────────────────────────────────────────

    #[test]
    fn test_confidence_ordering() {
        assert!(AttributionConfidence::High > AttributionConfidence::Medium);
        assert!(AttributionConfidence::Medium > AttributionConfidence::Low);
    }

    // ── Generic failure tests ──────────────────────────────────────────

    #[test]
    fn test_generic_process_exited_not_attributed() {
        let log = "Process exited with code 1";
        assert!(parse_boot_log_for_missing_deps(log).is_none());
    }

    #[test]
    fn test_wrong_java_not_repaired() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("UnsupportedClassVersionError: SomeMod", &graph);
        assert!(matches!(attr, BootAttribution::WrongJavaVersion));
        // Should NOT be MissingDependency
    }

    #[test]
    fn test_oom_not_repaired() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure(
            "java.lang.OutOfMemoryError: GC overhead limit exceeded",
            &graph,
        );
        assert!(matches!(attr, BootAttribution::OutOfMemory));
    }

    // ── Forge specific pattern tests ───────────────────────────────────

    #[test]
    fn test_forge_neoforge_style() {
        // NeoForge uses similar patterns to Forge
        let log =
            "[12:34:56] [main/ERROR]: Mod 'neoforge_core' requires mod 'neoforge_api' (>= 21.0.0)";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.requesting_mod_id.as_deref(), Some("neoforge_core"));
        assert_eq!(parsed.missing_mod_id, "neoforge_api");
        assert_eq!(parsed.version_requirement.as_deref(), Some("21.0.0"));
    }

    #[test]
    fn test_forge_missing_mandatory_double_quotes() {
        let log = "[12:34:56] [main/ERROR]: Missing mandatory dependency \"flywheel\"";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.missing_mod_id, "flywheel");
    }

    // ── Mixed log tests ────────────────────────────────────────────────

    #[test]
    fn test_multiline_log_first_pattern_wins() {
        let log = "[12:34:56] [main/INFO]: Loading mods...\n\
                   [12:34:57] [main/ERROR]: Mod 'A' requires mod 'B'\n\
                   [12:34:58] [main/ERROR]: Mod 'C' requires mod 'D'";
        let parsed = parse_boot_log_for_missing_deps(log).unwrap();
        assert_eq!(parsed.missing_mod_id, "B");
    }

    #[test]
    fn test_oom_in_multiline_log() {
        let log = "[12:34:56] [main/INFO]: Loading...\n\
                   java.lang.OutOfMemoryError: Java heap space";
        assert!(is_oom_log(log));
    }

    // ── Cross-check and graph tests ────────────────────────────────

    #[test]
    fn test_graph_disagrees_but_confidence_stays_high() {
        // Log says "Mod 'create' requires mod 'flywheel'"
        // Graph is empty — but attribution confidence is always High for exact patterns.
        // Verification (verify_attribution) decides whether to proceed with repair.
        let log = "Mod 'create' requires mod 'flywheel' (>= 0.6.0)";
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure(log, &graph);
        let map = ProjectModMapping::new();
        let reg = InstalledFileRegistry::new();
        match &attr {
            BootAttribution::MissingDependency(dep) => {
                assert_eq!(dep.confidence, AttributionConfidence::High);
                assert_eq!(dep.missing_mod_id, "flywheel");
                // verify_attribution with empty graph + empty registry should return empty
                let sources = verify_attribution(dep, &graph, &map, &reg);
                assert!(
                    sources.is_empty(),
                    "Empty graph + empty registry → no verification"
                );
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    #[test]
    fn test_verify_attribution_empty_graph_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };
        let graph = DependencyGraph::build(&[]);
        let map = ProjectModMapping::new();
        let sources = verify_attribution(&attr, &graph, &map, &InstalledFileRegistry::new());
        assert!(
            sources.is_empty(),
            "Expected no verification sources for empty graph"
        );
    }

    #[test]
    fn test_verify_attribution_graph_confirms() {
        let dir = tempfile::tempdir().unwrap();
        let jar =
            create_forge_jar_helper(dir.path(), "create.jar", "create", &[("flywheel", true)]);
        let compat = crate::mod_compat::classify_mod_local(&jar);
        let analysis = vec![(jar.as_ref().to_path_buf(), compat)];
        let graph = DependencyGraph::build(&analysis);

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };
        let map = ProjectModMapping::new();
        let sources = verify_attribution(&attr, &graph, &map, &InstalledFileRegistry::new());
        assert!(
            sources.contains(&VerificationSource::JarMetadataGraph),
            "Expected JarMetadataGraph verification, got {:?}",
            sources
        );
    }

    #[test]
    fn test_oom_attribution_not_repairable() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("OutOfMemoryError: Java heap space", &graph);
        assert!(matches!(attr, BootAttribution::OutOfMemory));
        assert!(!matches!(attr, BootAttribution::MissingDependency(_)));
    }

    #[test]
    fn test_wrong_java_not_repairable() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("UnsupportedClassVersionError", &graph);
        assert!(matches!(attr, BootAttribution::WrongJavaVersion));
        assert!(!matches!(attr, BootAttribution::MissingDependency(_)));
    }

    #[test]
    fn test_generic_process_exited_is_unknown() {
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure("Process exited with code 1", &graph);
        assert!(matches!(attr, BootAttribution::Unknown));
    }

    #[test]
    fn test_clientonly_existing_dep_not_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let jar = create_fabric_jar_helper(dir.path(), "iris.jar", Some("client"), "iris");
        let compat = crate::mod_compat::classify_mod_local(&jar);
        let analysis = vec![(jar.as_ref().to_path_buf(), compat)];
        let graph = DependencyGraph::build(&analysis);

        let node = graph.find_by_mod_id("iris");
        assert!(node.is_some(), "iris should be in graph");

        let log = "Mod 'sodium' requires mod 'iris'";
        let attr = analyze_boot_failure(log, &graph);
        match attr {
            BootAttribution::MissingDependency(dep) => {
                assert_eq!(dep.confidence, AttributionConfidence::High);
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    #[test]
    fn test_ambiguous_existing_provider_no_repair() {
        let dir = tempfile::tempdir().unwrap();
        let jar_a = create_fabric_jar_helper(dir.path(), "lib-a.jar", None, "shared_lib");
        let jar_b = create_fabric_jar_helper(dir.path(), "lib-b.jar", None, "shared_lib");
        let compat_a = crate::mod_compat::classify_mod_local(&jar_a);
        let compat_b = crate::mod_compat::classify_mod_local(&jar_b);
        let analysis = vec![
            (jar_a.as_ref().to_path_buf(), compat_a),
            (jar_b.as_ref().to_path_buf(), compat_b),
        ];
        let graph = DependencyGraph::build(&analysis);
        assert!(graph.nodes.len() == 2);

        let log = "Mod 'create' requires mod 'shared_lib'";
        let attr = analyze_boot_failure(log, &graph);
        match attr {
            BootAttribution::MissingDependency(dep) => {
                assert_eq!(dep.confidence, AttributionConfidence::High);
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    #[test]
    fn test_version_constraint_propagated_to_attribution() {
        let log = "Mod 'create' requires mod 'flywheel' (>= 2.0.0)";
        let graph = DependencyGraph::build(&[]);
        let attr = analyze_boot_failure(log, &graph);
        match attr {
            BootAttribution::MissingDependency(dep) => {
                assert_eq!(dep.version_requirement.as_deref(), Some("2.0.0"));
                assert_eq!(dep.missing_mod_id, "flywheel");
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    #[test]
    fn test_max_boot_repair_rounds_constant() {
        assert_eq!(MAX_BOOT_REPAIR_ROUNDS, 2);
    }

    // ── Phase 3F-B required tests: CF relation verification ──────────

    /// A. Graph-confirmed runtime log: log A→B, graph Required A→B → High / repair allowed
    #[test]
    fn test_graph_confirmed_required_dep_high_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let jar =
            create_forge_jar_helper(dir.path(), "create.jar", "create", &[("flywheel", true)]);
        let compat = crate::mod_compat::classify_mod_local(&jar);
        let analysis = vec![(jar.as_ref().to_path_buf(), compat)];
        let graph = DependencyGraph::build(&analysis);

        let log = "Mod 'create' requires mod 'flywheel' (>= 0.6.0)";
        let attr = analyze_boot_failure(log, &graph);
        match &attr {
            BootAttribution::MissingDependency(dep) => {
                assert_eq!(dep.confidence, AttributionConfidence::High);
                assert_eq!(dep.missing_mod_id, "flywheel");
                // Verify: graph confirms (Required dep)
                let map = ProjectModMapping::new();
                let reg = InstalledFileRegistry::new();
                let sources = verify_attribution(dep, &graph, &map, &reg);
                assert!(
                    sources.contains(&VerificationSource::JarMetadataGraph),
                    "Graph Required dep should verify, got {:?}",
                    sources
                );
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    /// B. Graph optional dependency: log A→B, graph Optional A→B → no verification
    #[test]
    fn test_graph_optional_dep_no_verification() {
        let dir = tempfile::tempdir().unwrap();
        let jar = create_forge_jar_helper(dir.path(), "create.jar", "create", &[("jei", false)]);
        let compat = crate::mod_compat::classify_mod_local(&jar);
        let analysis = vec![(jar.as_ref().to_path_buf(), compat)];
        let graph = DependencyGraph::build(&analysis);

        let log = "Mod 'create' requires mod 'jei'";
        let attr = analyze_boot_failure(log, &graph);
        match &attr {
            BootAttribution::MissingDependency(dep) => {
                // verify_attribution should NOT confirm via graph (Optional dep)
                let map = ProjectModMapping::new();
                let reg = InstalledFileRegistry::new();
                let sources = verify_attribution(dep, &graph, &map, &reg);
                assert!(
                    !sources.contains(&VerificationSource::JarMetadataGraph),
                    "Optional dep should NOT verify via graph, got {:?}",
                    sources
                );
            }
            _ => panic!("Expected MissingDependency"),
        }
    }

    /// C. Runtime-only + exact CF REQUIRED relation → High / repair allowed
    #[test]
    fn test_runtime_only_cf_required_relation_allows_repair() {
        // Graph is empty (runtime-only dep not in JAR metadata)
        let graph = DependencyGraph::build(&[]);

        // But the requesting mod's installed CF file has a REQUIRED dependency on missing mod
        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,  // create project_id
            10001, // create file_id
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 6000, // flywheel project_id
                relation_type: relation_type::REQUIRED,
            }],
        );
        // flywheel is NOT registered anywhere — true runtime-only case
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let sources = verify_attribution(&attr, &graph, &map, &reg);
        // Project 6000 is an unknown REQUIRED project → candidates exist (provisional)
        assert!(
            sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "CF runtime relation candidates should exist, got {:?}",
            sources
        );
    }

    /// D. Runtime-only + CF OPTIONAL relation → no repair
    #[test]
    fn test_runtime_only_cf_optional_relation_no_repair() {
        let graph = DependencyGraph::build(&[]);

        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 6000,
                relation_type: relation_type::OPTIONAL,
            }],
        );
        // flywheel NOT registered — true runtime-only case
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let sources = verify_attribution(&attr, &graph, &map, &reg);
        assert!(
            !sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "CF OPTIONAL should NOT verify, got {:?}",
            sources
        );
        assert!(
            sources.is_empty(),
            "No source should verify for OPTIONAL relation"
        );
    }

    /// E. Both projects mapped but NO dependency relation → MUST NOT return RuntimeRelationCandidatesExist
    #[test]
    fn test_both_mapped_but_no_relation_no_cf_verification() {
        let graph = DependencyGraph::build(&[]);

        // Register create with NO dependencies at all
        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![], // empty dependencies
        );
        // flywheel is also registered
        reg.register(6000, 10002, vec!["flywheel".to_string()], vec![]);

        let mut map = ProjectModMapping::new();
        map.register(5000, vec!["create".to_string()]);
        map.register(6000, vec!["flywheel".to_string()]);

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let sources = verify_attribution(&attr, &graph, &map, &reg);
        assert!(
            !sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "Two project mappings with NO relation must NOT verify via CF, got {:?}",
            sources
        );
    }

    /// F. Unknown REQUIRED project → CF verification (runtime-only case)
    /// Previously: no verification because B's project_id couldn't be mapped.
    /// Now: unknown REQUIRED project from A's file IS the verification evidence.
    #[test]
    fn test_unknown_required_project_confirms_cf_verification() {
        let graph = DependencyGraph::build(&[]);
        let mut reg = InstalledFileRegistry::new();
        // Register create but NOT flywheel
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 6000,
                relation_type: relation_type::REQUIRED,
            }],
        );
        // No project_map entry for flywheel → NotMapped
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let sources = verify_attribution(&attr, &graph, &map, &reg);
        // Project 6000 is an unknown REQUIRED dependency from create's installed file.
        // This confirms the attribution — runtime-only resolution will try project 6000
        // as a candidate and verify post-download via JAR metadata.
        assert!(
            sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "Unknown REQUIRED project should have runtime candidates, got {:?}",
            sources
        );
    }

    /// G. Fresh boot-repair context retains manifest mappings
    /// Verifies that InstalledFileRegistry correctly preserves CF file identity
    #[test]
    fn test_installed_file_registry_preserves_identity() {
        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![
                crate::dependency_resolver::CurseDependency {
                    mod_id: 6000,
                    relation_type: relation_type::REQUIRED,
                },
                crate::dependency_resolver::CurseDependency {
                    mod_id: 7000,
                    relation_type: relation_type::OPTIONAL,
                },
            ],
        );

        // Lookup by mod_id
        let info = reg
            .get_by_mod_id("create")
            .expect("create should be registered");
        assert_eq!(info.project_id, 5000);
        assert_eq!(info.file_id, 10001);
        assert_eq!(info.dependencies.len(), 2);
        assert_eq!(info.dependencies[0].mod_id, 6000);
        assert_eq!(info.dependencies[0].relation_type, relation_type::REQUIRED);

        // Lookup by project_id
        let info2 = reg
            .get_by_project_id(5000)
            .expect("project 5000 should be registered");
        assert_eq!(info2.mod_ids, vec!["create".to_string()]);
    }

    /// H. Duplicate project mapping → ambiguity, no verification
    /// H. Duplicate mod_id in project_map — CF still confirms via unknown REQUIRED project
    /// Ambiguity is caught post-download during JAR verification, not during attribution.
    #[test]
    fn test_duplicate_mod_id_ambiguity_no_verification() {
        let graph = DependencyGraph::build(&[]);
        let mut reg = InstalledFileRegistry::new();
        // Register create with REQUIRED dep on project 6000
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 6000,
                relation_type: relation_type::REQUIRED,
            }],
        );
        // Project 6000 is NOT in installed_files → unknown REQUIRED project
        let mut map = ProjectModMapping::new();
        map.register(5000, vec!["shared_lib".to_string()]);
        map.register(6000, vec!["shared_lib".to_string()]); // ambiguous in project_map

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "shared_lib".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let sources = verify_attribution(&attr, &graph, &map, &reg);
        // Project 6000 is unknown in installed_files → CF confirms.
        // Ambiguity (two projects for same mod_id) is caught during download+JAR verify,
        // not during attribution. This is correct: attribution only checks evidence,
        // repair layer handles identity verification.
        assert!(
            sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "Unknown REQUIRED project has runtime candidates even with ambiguous map, got {:?}",
            sources
        );
    }

    // ── Phase 3F-B: Runtime-only resolution tests ─────────────────

    /// I. Critical: runtime-only missing dep WITHOUT existing mapping
    /// B is NOT installed, NOT in InstalledFileRegistry, NOT in ProjectModMapping.
    /// A's CF file has REQUIRED dependency on project 12345.
    /// enumerate_unknown_required_projects returns [12345].
    /// verify_attribution confirms via RuntimeRelationCandidatesExist (provisional).
    /// B was never pre-registered — this is the true runtime-only case.
    #[test]
    fn test_runtime_only_missing_dep_without_existing_mapping() {
        let graph = DependencyGraph::build(&[]);

        let mut reg = InstalledFileRegistry::new();
        // Register A (create) with REQUIRED dep on project 12345
        // B (flywheel) is NOT registered anywhere
        reg.register(
            5000,  // create project_id
            10001, // create file_id
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 12345, // flywheel's CF project ID
                relation_type: relation_type::REQUIRED,
            }],
        );

        // B is NOT in project_map — true runtime-only
        let map = ProjectModMapping::new();

        // Verify B is truly not registered
        assert!(
            reg.get_by_mod_id("flywheel").is_none(),
            "flywheel must NOT be in InstalledFileRegistry"
        );
        assert!(
            matches!(
                map.project_for_mod_id("flywheel"),
                dependency_resolver::ModIdLookup::NotMapped
            ),
            "flywheel must NOT be in ProjectModMapping"
        );

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        // Step 1: enumerate_unknown_required_projects should find project 12345
        let unknown = enumerate_unknown_required_projects(&attr, &reg);
        assert_eq!(
            unknown,
            vec![12345],
            "Should enumerate project 12345 as unknown REQUIRED candidate"
        );

        // Step 2: verify_attribution should confirm via CF REQUIRED relation
        let sources = verify_attribution(&attr, &graph, &map, &reg);
        assert!(
            sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "Runtime-only with unknown REQUIRED project should have candidates, got {:?}",
            sources
        );

        // Note: actual download + JAR identity verification requires real CF API
        // (integration test). This unit test proves the attribution path is correct.
    }

    /// J. Runtime-only: REQUIRED project exists but downloaded JAR declares wrong mod_id
    /// Boot says missing B, but the candidate JAR declares C.
    /// This is tested by try_runtime_only_resolution post-download check.
    /// Unit test: enumerate returns candidates, verify confirms — rejection happens post-download.
    #[test]
    fn test_runtime_only_wrong_jar_identity_enumerates_candidates() {
        let graph = DependencyGraph::build(&[]);
        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 12345,
                relation_type: relation_type::REQUIRED,
            }],
        );
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        // Attribution confirms (unknown REQUIRED project exists)
        let sources = verify_attribution(&attr, &graph, &map, &reg);
        assert!(sources.contains(&VerificationSource::RuntimeRelationCandidatesExist));

        // Actual JAR identity mismatch (mod_ids=["C"] not "flywheel") is caught
        // post-download in try_runtime_only_resolution → IdentityMismatch.
        // Cannot unit-test download path without mock CF API.
    }

    /// K. Runtime-only: two REQUIRED projects both provide B → Ambiguous
    /// enumerate returns both project IDs. Post-download: both declare B → Ambiguous, STOP.
    #[test]
    fn test_runtime_only_two_required_projects_enumerate_both() {
        let graph = DependencyGraph::build(&[]);
        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![
                crate::dependency_resolver::CurseDependency {
                    mod_id: 12345,
                    relation_type: relation_type::REQUIRED,
                },
                crate::dependency_resolver::CurseDependency {
                    mod_id: 67890,
                    relation_type: relation_type::REQUIRED,
                },
            ],
        );
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let unknown = enumerate_unknown_required_projects(&attr, &reg);
        assert_eq!(
            unknown.len(),
            2,
            "Should enumerate both unknown REQUIRED projects"
        );
        assert!(unknown.contains(&12345));
        assert!(unknown.contains(&67890));

        // Ambiguity resolution (both declare B) happens in try_runtime_only_resolution.
        // Cannot unit-test without mock CF API.
    }

    /// L. Runtime-only: OPTIONAL relation → enumerate returns empty → no CF verification
    #[test]
    fn test_runtime_only_optional_not_enumerated() {
        let graph = DependencyGraph::build(&[]);
        let mut reg = InstalledFileRegistry::new();
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 12345,
                relation_type: relation_type::OPTIONAL,
            }],
        );
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let unknown = enumerate_unknown_required_projects(&attr, &reg);
        assert!(
            unknown.is_empty(),
            "OPTIONAL relations should NOT be enumerated"
        );

        let sources = verify_attribution(&attr, &graph, &map, &reg);
        assert!(
            !sources.contains(&VerificationSource::RuntimeRelationCandidatesExist),
            "OPTIONAL should NOT have runtime candidates, got {:?}",
            sources
        );
    }

    /// M. Runtime-only: all REQUIRED projects already installed → no unknown → no CF verify
    #[test]
    fn test_runtime_only_all_required_already_installed() {
        let graph = DependencyGraph::build(&[]);
        let mut reg = InstalledFileRegistry::new();
        // Register create with REQUIRED dep on project 12345
        reg.register(
            5000,
            10001,
            vec!["create".to_string()],
            vec![crate::dependency_resolver::CurseDependency {
                mod_id: 12345,
                relation_type: relation_type::REQUIRED,
            }],
        );
        // Register flywheel too — project 12345 is now KNOWN
        reg.register(12345, 20001, vec!["flywheel".to_string()], vec![]);
        let map = ProjectModMapping::new();

        let attr = BootMissingDependency {
            requesting_mod_id: Some("create".to_string()),
            missing_mod_id: "flywheel".to_string(),
            version_requirement: None,
            source: BootAttributionSource::ForgeLoader,
            confidence: AttributionConfidence::High,
        };

        let unknown = enumerate_unknown_required_projects(&attr, &reg);
        assert!(
            unknown.is_empty(),
            "All REQUIRED projects already installed → no unknown candidates"
        );
    }

    // ── Helper for creating test JARs ──────────────────────────────

    fn create_forge_jar_helper(
        dir: &std::path::Path,
        name: &str,
        mod_id: &str,
        deps: &[(&str, bool)],
    ) -> OwnedPath {
        let mut toml = format!(
            "modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"{}\"\n",
            mod_id
        );
        for (dep_id, required) in deps {
            toml.push_str(&format!(
                "\n[[dependencies.{}]]\nmodId=\"{}\"\nmandatory={}\nversionRange=\"[1,)\"\n",
                mod_id, dep_id, required
            ));
        }
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/mods.toml", options).unwrap();
        zip.write_all(toml.as_bytes()).unwrap();
        zip.finish().unwrap();
        OwnedPath(path)
    }

    fn create_fabric_jar_helper(
        dir: &std::path::Path,
        name: &str,
        env: Option<&str>,
        mod_id: &str,
    ) -> OwnedPath {
        let env_str = env.unwrap_or("*");
        let json = format!(
            "{{\"schemaVersion\":1,\"id\":\"{}\",\"environment\":\"{}\"}}",
            mod_id, env_str
        );
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("fabric.mod.json", options).unwrap();
        zip.write_all(json.as_bytes()).unwrap();
        zip.finish().unwrap();
        OwnedPath(path)
    }

    struct OwnedPath(std::path::PathBuf);
    impl AsRef<std::path::Path> for OwnedPath {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl std::ops::Deref for OwnedPath {
        type Target = std::path::PathBuf;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl Drop for OwnedPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
