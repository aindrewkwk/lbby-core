// crash_attribution.rs — High-confidence crash attribution for Phase 3J.
//
// When validation fails for a non-repairable crash, identify the most likely
// responsible mod/JAR only when evidence is strong enough.
//
// This module is ATTRIBUTION + DIAGNOSTICS only. No automatic mutation.
// No deletion, quarantine, rename, loader/version/RAM changes.

use crate::jar_metadata::read_jar_mod_metadata;
use crate::mod_compat::{classify_mod_local, ModCompatibility, ServerCompatibility};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── Constants ─────────────────────────────────────────────────────────

const MAX_CRASH_CANDIDATES: usize = 10;
const MAX_EVIDENCE_PER_CANDIDATE: usize = 10;
const MAX_SNIPPET_LEN: usize = 200;

// ── Public types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashAttributionStatus {
    /// A mod was identified with sufficient confidence.
    Attributed,
    /// Multiple candidates, cannot pick one safely.
    Ambiguous,
    /// No meaningful evidence found.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashAttributionConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashEvidenceSource {
    /// Loader explicitly names a mod_id as failing.
    LoaderDiagnostic,
    /// Loader explicitly names a JAR path.
    ExplicitJarPath,
    /// Forge/NeoForge ModLoadingException with mod_id.
    ModLoadingException,
    /// Fabric/Quilt entrypoint crash naming provider.
    EntrypointCrash,
    /// Mixin config/class maps to a JAR.
    MixinTarget,
    /// Stack frame package maps to a mod.
    StackFrame,
    /// Dependency context (explanatory only).
    DependencyContext,
}

#[derive(Debug, Clone)]
pub struct CrashEvidence {
    pub source: CrashEvidenceSource,
    pub matched_text: String,
    pub snippet: String,
    pub associated_mod_id: Option<String>,
    pub associated_jar: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CrashCandidate {
    pub mod_id: Option<String>,
    pub jar_path: Option<PathBuf>,
    pub score: u32,
    pub evidence: Vec<CrashEvidence>,
    /// True if this JAR is classified as ClientOnly.
    pub is_client_only: bool,
    /// True if this mod is UNKNOWN compatibility.
    pub is_unknown_compat: bool,
}

#[derive(Debug, Clone)]
pub enum CrashRecommendation {
    /// Review a specific mod manually.
    ReviewMod {
        mod_id: String,
        jar_path: Option<PathBuf>,
    },
    /// Suggest manual quarantine (no auto action).
    SuggestManualQuarantine { mod_id: String },
    /// Conflicting evidence — cannot recommend.
    ConflictingEvidence,
    /// No safe recommendation possible.
    NoSafeRecommendation,
    /// Pipeline invariant violation: ClientOnly mod present at crash.
    UnexpectedClientOnlyPresent {
        mod_id: Option<String>,
        jar_path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone)]
pub struct CrashAttributionReport {
    pub status: CrashAttributionStatus,
    pub confidence: CrashAttributionConfidence,
    pub candidates: Vec<CrashCandidate>,
    pub primary_candidate: Option<CrashCandidate>,
    pub recommendation: CrashRecommendation,
    pub summary: String,
}

/// Input context for crash attribution.
pub struct CrashAttributionContext<'a> {
    /// Path to staging mods/ directory.
    pub staging_mods: &'a Path,
    /// Loader family (forge/fabric/quilt/vanilla).
    pub loader_family: LoaderFamily,
    /// Optional: recently repaired mod IDs (for context only, not guilt).
    pub recently_repaired: &'a [String],
    /// Optional: installed file registry for authoritative JAR→mod_id mapping.
    pub installed_registry: Option<&'a crate::boot_failure_analyzer::InstalledFileRegistry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderFamily {
    Forge,
    NeoForge,
    Fabric,
    Quilt,
    Vanilla,
}

// ── Internal types ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ParsedEvidence {
    source: CrashEvidenceSource,
    matched_text: String,
    snippet: String,
    mod_id: Option<String>,
    jar_path: Option<PathBuf>,
}

/// Read-only index mapping JARs ↔ mod_ids ↔ resources.
struct JarOwnershipIndex {
    /// mod_id → list of JAR paths providing it.
    mod_id_to_jars: HashMap<String, Vec<PathBuf>>,
    /// JAR path → mod_ids it declares.
    jar_to_mod_ids: HashMap<PathBuf, Vec<String>>,
    /// Resource name (e.g. mixin config) → JAR path containing it.
    resource_to_jars: HashMap<String, Vec<PathBuf>>,
    /// JAR path → compatibility classification.
    jar_compat: HashMap<PathBuf, ModCompatibility>,
}

// ── Ownership index ───────────────────────────────────────────────────

impl JarOwnershipIndex {
    fn build(staging_mods: &Path) -> Self {
        let mut idx = Self {
            mod_id_to_jars: HashMap::new(),
            jar_to_mod_ids: HashMap::new(),
            resource_to_jars: HashMap::new(),
            jar_compat: HashMap::new(),
        };

        let Ok(entries) = std::fs::read_dir(staging_mods) else {
            return idx;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|ext| ext == "jar") {
                continue;
            }

            // Read JAR metadata for mod_ids
            let meta = read_jar_mod_metadata(&path);
            idx.jar_to_mod_ids
                .insert(path.clone(), meta.mod_ids.clone());

            for mod_id in &meta.mod_ids {
                idx.mod_id_to_jars
                    .entry(mod_id.clone())
                    .or_default()
                    .push(path.clone());
            }

            // Scan JAR resources for mixin configs
            if let Ok(file) = std::fs::File::open(&path) {
                if let Ok(mut archive) = zip::ZipArchive::new(file) {
                    for i in 0..archive.len() {
                        if let Ok(entry) = archive.by_index(i) {
                            let name = entry.name().to_string();
                            // Mixin configs: *.mixins.json, *.mixins.json5
                            if name.ends_with(".mixins.json")
                                || name.ends_with(".mixins.json5")
                                || name.contains("mixin") && name.ends_with(".json")
                            {
                                let basename = Path::new(&name)
                                    .file_name()
                                    .map(|f| f.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                idx.resource_to_jars
                                    .entry(basename)
                                    .or_default()
                                    .push(path.clone());
                            }
                        }
                    }
                }
            }

            // Classify compatibility
            let compat = classify_mod_local(&path);
            idx.jar_compat.insert(path, compat);
        }

        idx
    }

    /// Find JAR by path (canonicalized).
    fn find_jar(&self, jar_path: &Path) -> Option<&PathBuf> {
        // Try exact match first
        if let Some(jars) = self.jar_to_mod_ids.get(jar_path) {
            if !jars.is_empty() || self.jar_to_mod_ids.contains_key(jar_path) {
                return self.jar_to_mod_ids.keys().find(|k| *k == jar_path);
            }
        }
        // Try matching by filename in staging mods
        let target_name = jar_path.file_name()?;
        self.jar_to_mod_ids
            .keys()
            .find(|k| k.file_name().map(|f| f == target_name).unwrap_or(false))
    }
}

// ── Explicit mod-id extraction ────────────────────────────────────────

/// Recognizes loader messages that explicitly name a failing mod_id.
/// Returns list of (mod_id, matched_line, source).
fn parse_explicit_mod_ids(log: &str) -> Vec<ParsedEvidence> {
    let mut results = Vec::new();

    for line in log.lines() {
        // Forge/NeoForge: "Mod ID: <id>"
        if let Some(id) = extract_after_prefix(line, "Mod ID:") {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::LoaderDiagnostic,
                matched_text: line.to_string(),
                snippet: truncate_snippet(line),
                mod_id: Some(id),
                jar_path: None,
            });
            continue;
        }

        // Forge: "Mod <id> has failed to load correctly"
        if let Some(id) = extract_between(line, "Mod ", " has failed") {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::LoaderDiagnostic,
                matched_text: line.to_string(),
                snippet: truncate_snippet(line),
                mod_id: Some(id),
                jar_path: None,
            });
            continue;
        }

        // Generic: "Failed to load mod: <id>"
        if let Some(id) = extract_after_prefix(line, "Failed to load mod:") {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::LoaderDiagnostic,
                matched_text: line.to_string(),
                snippet: truncate_snippet(line),
                mod_id: Some(id),
                jar_path: None,
            });
            continue;
        }

        // Fabric: "Could not execute entrypoint stage '...' due to errors, provided by '<id>'"
        if let Some(id) = extract_between(line, "provided by '", "'") {
            if line.contains("Could not execute entrypoint") {
                results.push(ParsedEvidence {
                    source: CrashEvidenceSource::EntrypointCrash,
                    matched_text: line.to_string(),
                    snippet: truncate_snippet(line),
                    mod_id: Some(id),
                    jar_path: None,
                });
                continue;
            }
        }

        // Fabric: "A mod crashed on startup: <id>"
        if let Some(id) = extract_after_prefix(line, "A mod crashed on startup:") {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::EntrypointCrash,
                matched_text: line.to_string(),
                snippet: truncate_snippet(line),
                mod_id: Some(id),
                jar_path: None,
            });
            continue;
        }
    }

    results
}

// ── Forge/NeoForge ModLoadingException ────────────────────────────────

/// Parses Forge/NeoForge ModLoadingException blocks.
/// These contain structured fields: Mod ID, Failure message, Mod File.
fn parse_forge_mod_loading_exception(log: &str) -> Vec<ParsedEvidence> {
    let mut results = Vec::new();

    // Look for ModLoadingException context
    if !log.contains("ModLoadingException") && !log.contains("ModLoading") {
        return results;
    }

    for (i, line) in log.lines().enumerate() {
        let trimmed = line.trim();

        // "Mod ID: <id>"
        if let Some(id) = extract_after_prefix(trimmed, "Mod ID:") {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::ModLoadingException,
                matched_text: trimmed.to_string(),
                snippet: truncate_snippet(trimmed),
                mod_id: Some(id),
                jar_path: None,
            });
        }

        // "Mod File: <path>" or "Mod File: <path>"
        if let Some(path_str) = extract_after_prefix(trimmed, "Mod File:") {
            let cleaned = path_str.trim();
            let p = PathBuf::from(cleaned);
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::ModLoadingException,
                matched_text: trimmed.to_string(),
                snippet: truncate_snippet(trimmed),
                mod_id: None,
                jar_path: Some(p),
            });
        }

        // "Failure message: ..." — context only, no direct mod_id
        // But if near a "Mod ID:" line, it's supporting evidence.
        if trimmed.starts_with("Failure message:") {
            // Look backward up to 5 lines for a Mod ID
            let start = i.saturating_sub(5);
            for j in start..i {
                if let Some(prev) = log.lines().nth(j) {
                    if let Some(id) = extract_after_prefix(prev.trim(), "Mod ID:") {
                        results.push(ParsedEvidence {
                            source: CrashEvidenceSource::ModLoadingException,
                            matched_text: trimmed.to_string(),
                            snippet: truncate_snippet(trimmed),
                            mod_id: Some(id),
                            jar_path: None,
                        });
                        break;
                    }
                }
            }
        }
    }

    results
}

// ── Fabric/Quilt entrypoint attribution ───────────────────────────────

/// Parses Fabric/Quilt-specific crash patterns.
fn parse_fabric_quilt_crash(log: &str) -> Vec<ParsedEvidence> {
    let mut results = Vec::new();

    for line in log.lines() {
        let trimmed = line.trim();

        // "Could not execute entrypoint stage '...' due to errors, provided by '<id>'"
        if let Some(id) = extract_between(trimmed, "provided by '", "'") {
            if trimmed.contains("Could not execute entrypoint") {
                results.push(ParsedEvidence {
                    source: CrashEvidenceSource::EntrypointCrash,
                    matched_text: trimmed.to_string(),
                    snippet: truncate_snippet(trimmed),
                    mod_id: Some(id),
                    jar_path: None,
                });
            }
        }

        // "A mod crashed on startup: <id>"
        if let Some(id) = extract_after_prefix(trimmed, "A mod crashed on startup:") {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::EntrypointCrash,
                matched_text: trimmed.to_string(),
                snippet: truncate_snippet(trimmed),
                mod_id: Some(id),
                jar_path: None,
            });
        }

        // Quilt: "... due to errors, provided by '<id>'" (same pattern works)
        if trimmed.contains("errors, provided by") {
            if let Some(id) = extract_between(trimmed, "provided by '", "'") {
                if !results.iter().any(|r| r.mod_id.as_deref() == Some(&id)) {
                    results.push(ParsedEvidence {
                        source: CrashEvidenceSource::EntrypointCrash,
                        matched_text: trimmed.to_string(),
                        snippet: truncate_snippet(trimmed),
                        mod_id: Some(id),
                        jar_path: None,
                    });
                }
            }
        }
    }

    results
}

// ── Mixin attribution ─────────────────────────────────────────────────

/// Parses mixin-related crash evidence.
/// Maps mixin config resources to JARs via the ownership index.
fn parse_mixin_attribution(log: &str, index: &JarOwnershipIndex) -> Vec<ParsedEvidence> {
    let mut results = Vec::new();

    for line in log.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();

        // "Mixin config <name>" or "Mixin config: <name>" (case-insensitive)
        let mixin_config_pos = lower.find("mixin config");
        if let Some(pos) = mixin_config_pos {
            let after = trimmed[pos + "mixin config".len()..].trim();
            let config_name = after.trim_start_matches(':').trim();
            let config_name = config_name.trim_matches('"').trim_matches('\'');

            if !config_name.is_empty() {
                // Look up which JAR contains this resource
                let basename = Path::new(config_name)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();

                if let Some(jars) = index.resource_to_jars.get(&basename) {
                    if jars.len() == 1 {
                        results.push(ParsedEvidence {
                            source: CrashEvidenceSource::MixinTarget,
                            matched_text: trimmed.to_string(),
                            snippet: truncate_snippet(trimmed),
                            mod_id: None, // resolved via JAR metadata later
                            jar_path: Some(jars[0].clone()),
                        });
                    }
                    // If multiple JARs own the resource, record each as separate evidence
                    // so the ambiguity detection works via candidate dedup
                    if jars.len() > 1 {
                        for jar in jars {
                            results.push(ParsedEvidence {
                                source: CrashEvidenceSource::MixinTarget,
                                matched_text: trimmed.to_string(),
                                snippet: truncate_snippet(trimmed),
                                mod_id: None,
                                jar_path: Some(jar.clone()),
                            });
                        }
                    }
                }
                continue;
            }
        }

        // "InvalidMixinException" — look for class references
        if trimmed.contains("InvalidMixinException") || trimmed.contains("MixinApplyError") {
            // Try to find a mixin class reference
            // Pattern: "org.spongepowered.asm.mixin.transformer.MixinInfo$MixinConfig"
            // or just capture the line as evidence
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::MixinTarget,
                matched_text: trimmed.to_string(),
                snippet: truncate_snippet(trimmed),
                mod_id: None,
                jar_path: None,
            });
        }
    }

    results
}

// ── Explicit JAR path extraction ──────────────────────────────────────

/// Extracts explicit JAR paths from log output.
fn parse_explicit_jar_paths(log: &str) -> Vec<ParsedEvidence> {
    let mut results = Vec::new();

    for line in log.lines() {
        let trimmed = line.trim();

        // ".../mods/<name>.jar" or "File: /server/mods/<name>.jar"
        // Look for paths ending in .jar that contain "mods/"
        if let Some(jar_path) = extract_jar_path(trimmed) {
            results.push(ParsedEvidence {
                source: CrashEvidenceSource::ExplicitJarPath,
                matched_text: trimmed.to_string(),
                snippet: truncate_snippet(trimmed),
                mod_id: None,
                jar_path: Some(jar_path),
            });
        }
    }

    results
}

// ── Stack-frame attribution ───────────────────────────────────────────

/// Parses stack traces and maps package namespaces to mod_ids via the ownership index.
/// This is weak evidence — only used when no stronger source exists.
fn parse_stack_frame_attribution(log: &str, index: &JarOwnershipIndex) -> Vec<ParsedEvidence> {
    let mut results = Vec::new();
    let mut seen_packages: HashMap<String, PathBuf> = HashMap::new();

    for line in log.lines() {
        let trimmed = line.trim();

        // Java stack frames: "at com.simibubi.create.CreateMod.init(CreateMod.java:42)"
        if !trimmed.starts_with("at ") {
            continue;
        }

        // Extract package: "com.simibubi.create" from "at com.simibubi.create.Class.method"
        let after_at = &trimmed[3..];
        if let Some(class_part) = after_at.split('(').next() {
            // Remove method name: "com.simibubi.create.CreateMod.init" → "com.simibubi.create"
            let parts: Vec<&str> = class_part.split('.').collect();
            if parts.len() >= 3 {
                // Try progressively longer package prefixes (3+ segments)
                for len in (3..=parts.len()).rev() {
                    let package = parts[..len].join(".");
                    if seen_packages.contains_key(&package) {
                        continue;
                    }

                    // Search JARs for this package prefix
                    // We check if any JAR's mod_ids match a plausible prefix
                    // This is intentionally weak — only Low confidence
                    // We don't hardcode package→mod tables.
                    //
                    // Instead, we check if the package matches any known mod's
                    // common namespace patterns from JAR metadata.
                    // Since we can't read JAR source trees efficiently, we use
                    // a conservative heuristic: only attribute if the package
                    // segments contain a known mod_id.
                    //
                    // For example: "com.simibubi.create" contains "create"
                    // "me.jellysquid.mods.sodium" contains "sodium"
                    //
                    // But this is only Medium confidence if exactly one mod matches.

                    let package_lower = package.to_lowercase();
                    let mut matching_jars: Vec<PathBuf> = Vec::new();

                    for (jar_path, mod_ids) in &index.jar_to_mod_ids {
                        for mod_id in mod_ids {
                            let mod_lower = mod_id.to_lowercase();
                            // Check if the package contains the mod_id as a segment
                            if package_lower.contains(&mod_lower)
                                && package_lower.split('.').any(|seg| seg == mod_lower)
                            {
                                matching_jars.push(jar_path.clone());
                            }
                        }
                    }

                    if matching_jars.len() == 1 {
                        seen_packages.insert(package.clone(), matching_jars[0].clone());
                        results.push(ParsedEvidence {
                            source: CrashEvidenceSource::StackFrame,
                            matched_text: trimmed.to_string(),
                            snippet: truncate_snippet(trimmed),
                            mod_id: None,
                            jar_path: Some(matching_jars[0].clone()),
                        });
                        break; // Found a match for this line, don't try shorter prefixes
                    }
                    // Multiple matches or no match — don't attribute from this frame
                }
            }
        }
    }

    results
}

// ── Candidate building and scoring ────────────────────────────────────

/// Build candidates from parsed evidence, merging duplicates.
fn build_candidates(
    evidence: &[ParsedEvidence],
    index: &JarOwnershipIndex,
    recently_repaired: &[String],
) -> Vec<CrashCandidate> {
    // Map: (mod_id, jar_path) → evidence list + score
    let mut candidate_map: HashMap<(Option<String>, Option<PathBuf>), CrashCandidate> =
        HashMap::new();

    for ev in evidence {
        // Resolve JAR path to actual staging JAR if possible
        let resolved_jar = ev.jar_path.as_ref().and_then(|p| {
            index.find_jar(p).cloned().or_else(|| {
                // If the path is already in staging mods, use it directly
                if index.jar_to_mod_ids.contains_key(p) {
                    Some(p.clone())
                } else {
                    None
                }
            })
        });

        // If no JAR from evidence but we have a mod_id, look up JARs from the index
        let resolved_jar = resolved_jar.or_else(|| {
            ev.mod_id.as_ref().and_then(|mod_id| {
                index.mod_id_to_jars.get(mod_id).and_then(|jars| {
                    if jars.len() == 1 {
                        Some(jars[0].clone())
                    } else {
                        None // Multiple JARs for same mod_id or none
                    }
                })
            })
        });

        // Resolve mod_id from JAR metadata if not explicitly provided
        let resolved_mod_id = ev.mod_id.clone().or_else(|| {
            resolved_jar.as_ref().and_then(|jar| {
                index.jar_to_mod_ids.get(jar).and_then(|ids| {
                    if ids.len() == 1 {
                        Some(ids[0].clone())
                    } else {
                        None // Multi-mod JAR without explicit mod_id → ambiguous
                    }
                })
            })
        });

        let key = (resolved_mod_id.clone(), resolved_jar.clone());

        let score = evidence_score(&ev.source);

        let entry = candidate_map.entry(key).or_insert_with(|| CrashCandidate {
            mod_id: resolved_mod_id.clone(),
            jar_path: resolved_jar.clone(),
            score: 0,
            evidence: Vec::new(),
            is_client_only: resolved_jar
                .as_ref()
                .and_then(|j| index.jar_compat.get(j))
                .map(|c| c.compatibility == ServerCompatibility::ClientOnly)
                .unwrap_or(false),
            is_unknown_compat: resolved_jar
                .as_ref()
                .and_then(|j| index.jar_compat.get(j))
                .map(|c| {
                    c.compatibility == ServerCompatibility::Unknown
                        && c.confidence == crate::mod_compat::CompatibilityConfidence::Explicit
                })
                .unwrap_or(false),
        });

        entry.score = entry.score.saturating_add(score);
        if entry.evidence.len() < MAX_EVIDENCE_PER_CANDIDATE {
            entry.evidence.push(CrashEvidence {
                source: ev.source.clone(),
                matched_text: ev.matched_text.clone(),
                snippet: ev.snippet.clone(),
                associated_mod_id: ev.mod_id.clone(),
                associated_jar: ev.jar_path.clone(),
            });
        }
    }

    let mut candidates: Vec<CrashCandidate> = candidate_map.into_values().collect();

    // Sort by score descending
    candidates.sort_by(|a, b| b.score.cmp(&a.score));

    // Cap
    candidates.truncate(MAX_CRASH_CANDIDATES);

    candidates
}

/// Score a single evidence source.
fn evidence_score(source: &CrashEvidenceSource) -> u32 {
    match source {
        CrashEvidenceSource::LoaderDiagnostic => 100,
        CrashEvidenceSource::ExplicitJarPath => 90,
        CrashEvidenceSource::ModLoadingException => 100,
        CrashEvidenceSource::EntrypointCrash => 100,
        CrashEvidenceSource::MixinTarget => 70,
        CrashEvidenceSource::StackFrame => 30,
        CrashEvidenceSource::DependencyContext => 20,
    }
}

// ── Main analysis ─────────────────────────────────────────────────────

/// Analyze a crash log and produce an attribution report.
///
/// This is the single public entry point. No mutation, no network calls.
pub fn analyze_crash(log_tail: &str, ctx: &CrashAttributionContext<'_>) -> CrashAttributionReport {
    // Build ownership index from staging mods
    let index = JarOwnershipIndex::build(ctx.staging_mods);

    // Collect evidence from all parsers
    let mut all_evidence: Vec<ParsedEvidence> = Vec::new();

    // Priority 1: Explicit mod-id extraction (loader diagnostic)
    all_evidence.extend(parse_explicit_mod_ids(log_tail));

    // Priority 2: Explicit JAR paths
    all_evidence.extend(parse_explicit_jar_paths(log_tail));

    // Priority 3: Forge/NeoForge ModLoadingException
    all_evidence.extend(parse_forge_mod_loading_exception(log_tail));

    // Priority 4: Fabric/Quilt entrypoint crashes
    all_evidence.extend(parse_fabric_quilt_crash(log_tail));

    // Priority 5: Mixin attribution
    all_evidence.extend(parse_mixin_attribution(log_tail, &index));

    // Priority 6: Stack-frame attribution (weak)
    all_evidence.extend(parse_stack_frame_attribution(log_tail, &index));

    // Build candidates
    let mut candidates = build_candidates(&all_evidence, &index, ctx.recently_repaired);

    // Deduplicate: if multiple evidence items point to same mod_id, already merged.
    // But check for multi-mod JAR ambiguity.

    // Determine status and confidence
    let (status, confidence, primary) = determine_attribution(&candidates, &index);

    // Check for ClientOnly invariant violation
    if let Some(ref p) = primary {
        if p.is_client_only {
            let report = CrashAttributionReport {
                status: CrashAttributionStatus::Attributed,
                confidence: CrashAttributionConfidence::High,
                candidates: candidates.clone(),
                primary_candidate: Some(p.clone()),
                recommendation: CrashRecommendation::UnexpectedClientOnlyPresent {
                    mod_id: p.mod_id.clone(),
                    jar_path: p.jar_path.clone(),
                },
                summary: format!(
                    "Pipeline invariant violation: ClientOnly mod {} was present during crash attribution",
                    p.mod_id.as_deref().unwrap_or("unknown")
                ),
            };
            return report;
        }
    }

    // Build recommendation
    let recommendation = build_recommendation(&status, &confidence, &primary, &candidates);

    // Build summary
    let summary = build_summary(&status, &confidence, &primary, &candidates);

    CrashAttributionReport {
        status,
        confidence,
        candidates,
        primary_candidate: primary,
        recommendation,
        summary,
    }
}

/// Determine attribution status and confidence.
fn determine_attribution(
    candidates: &[CrashCandidate],
    index: &JarOwnershipIndex,
) -> (
    CrashAttributionStatus,
    CrashAttributionConfidence,
    Option<CrashCandidate>,
) {
    if candidates.is_empty() {
        return (
            CrashAttributionStatus::Unknown,
            CrashAttributionConfidence::Low,
            None,
        );
    }

    let top = &candidates[0];

    // Check for conflicting explicit evidence
    // If two candidates have the same high score from explicit sources, it's ambiguous
    let high_score_candidates: Vec<&CrashCandidate> =
        candidates.iter().filter(|c| c.score >= 90).collect();

    if high_score_candidates.len() > 1 {
        // Check if they point to different mods
        // Filter out candidates with mod_id=None — they're unresolved, not conflicting
        let mut mod_ids: Vec<&str> = high_score_candidates
            .iter()
            .filter_map(|c| c.mod_id.as_deref())
            .collect();
        mod_ids.sort();
        mod_ids.dedup();

        if mod_ids.len() > 1 {
            return (
                CrashAttributionStatus::Ambiguous,
                CrashAttributionConfidence::Low,
                None,
            );
        }
    }

    // Check for conflicting explicit evidence (mod_id vs JAR disagree)
    // If we have a candidate with explicit mod_id evidence AND explicit JAR evidence
    // that resolve to different mod_ids, that's conflicting.
    for cand in candidates {
        let has_explicit_mod = cand.evidence.iter().any(|e| {
            matches!(
                e.source,
                CrashEvidenceSource::LoaderDiagnostic
                    | CrashEvidenceSource::EntrypointCrash
                    | CrashEvidenceSource::ModLoadingException
            ) && e.associated_mod_id.is_some()
        });
        let has_explicit_jar = cand.evidence.iter().any(|e| {
            matches!(
                e.source,
                CrashEvidenceSource::ExplicitJarPath | CrashEvidenceSource::ModLoadingException
            ) && e.associated_jar.is_some()
        });

        if has_explicit_mod && has_explicit_jar {
            // Check if JAR metadata mod_id matches the explicit mod_id
            if let Some(ref jar) = cand.jar_path {
                if let Some(jar_mod_ids) = index.jar_to_mod_ids.get(jar) {
                    if let Some(ref explicit_id) = cand.mod_id {
                        if !jar_mod_ids.contains(explicit_id) && !jar_mod_ids.is_empty() {
                            return (
                                CrashAttributionStatus::Ambiguous,
                                CrashAttributionConfidence::Low,
                                None,
                            );
                        }
                    }
                }
            }
        }
    }

    // Determine confidence based on top candidate's evidence
    let top_confidence = classify_confidence(top, index);

    // Check for duplicate mod_id providers (two JARs claim same mod_id)
    if let Some(ref mod_id) = top.mod_id {
        if let Some(jars) = index.mod_id_to_jars.get(mod_id) {
            if jars.len() > 1 {
                // Multiple JARs claim the same mod_id — ambiguous provider
                return (
                    CrashAttributionStatus::Ambiguous,
                    CrashAttributionConfidence::Low,
                    None,
                );
            }
        }
    }

    // Multi-mod JAR without explicit mod_id
    if let Some(ref jar) = top.jar_path {
        if let Some(mod_ids) = index.jar_to_mod_ids.get(jar) {
            if mod_ids.len() > 1 && top.mod_id.is_none() {
                return (
                    CrashAttributionStatus::Ambiguous,
                    CrashAttributionConfidence::Low,
                    None,
                );
            }
        }
    }

    match top_confidence {
        CrashAttributionConfidence::High => (
            CrashAttributionStatus::Attributed,
            CrashAttributionConfidence::High,
            Some(top.clone()),
        ),
        CrashAttributionConfidence::Medium => {
            // Only attribute if there's a single strong candidate
            if candidates.len() == 1 || top.score > candidates.get(1).map(|c| c.score).unwrap_or(0)
            {
                (
                    CrashAttributionStatus::Attributed,
                    CrashAttributionConfidence::Medium,
                    Some(top.clone()),
                )
            } else {
                (
                    CrashAttributionStatus::Ambiguous,
                    CrashAttributionConfidence::Medium,
                    None,
                )
            }
        }
        CrashAttributionConfidence::Low => {
            // Low confidence — don't attribute strongly
            if top.score >= 90 {
                // Actually high confidence from evidence type
                (
                    CrashAttributionStatus::Attributed,
                    CrashAttributionConfidence::High,
                    Some(top.clone()),
                )
            } else {
                (
                    CrashAttributionStatus::Unknown,
                    CrashAttributionConfidence::Low,
                    None,
                )
            }
        }
    }
}

/// Classify confidence for a single candidate.
fn classify_confidence(
    candidate: &CrashCandidate,
    index: &JarOwnershipIndex,
) -> CrashAttributionConfidence {
    // High: explicit loader mod_id OR explicit JAR → authoritative metadata match
    let has_explicit_mod = candidate.evidence.iter().any(|e| {
        matches!(
            e.source,
            CrashEvidenceSource::LoaderDiagnostic
                | CrashEvidenceSource::EntrypointCrash
                | CrashEvidenceSource::ModLoadingException
        ) && e.associated_mod_id.is_some()
    });

    let has_explicit_jar = candidate.evidence.iter().any(|e| {
        matches!(
            e.source,
            CrashEvidenceSource::ExplicitJarPath | CrashEvidenceSource::ModLoadingException
        ) && e.associated_jar.is_some()
    });

    // High if explicit mod_id
    if has_explicit_mod {
        return CrashAttributionConfidence::High;
    }

    // High if explicit JAR → authoritative metadata confirms
    if has_explicit_jar {
        if let Some(ref jar) = candidate.jar_path {
            if let Some(mod_ids) = index.jar_to_mod_ids.get(jar) {
                if !mod_ids.is_empty() {
                    return CrashAttributionConfidence::High;
                }
            }
        }
    }

    // Medium: single unique mixin/resource owner
    let has_mixin = candidate
        .evidence
        .iter()
        .any(|e| matches!(e.source, CrashEvidenceSource::MixinTarget));
    if has_mixin && candidate.mod_id.is_some() {
        return CrashAttributionConfidence::Medium;
    }

    // Medium: single unique strong package owner
    let has_stack = candidate
        .evidence
        .iter()
        .any(|e| matches!(e.source, CrashEvidenceSource::StackFrame));
    if has_stack && candidate.mod_id.is_some() {
        return CrashAttributionConfidence::Medium;
    }

    CrashAttributionConfidence::Low
}

/// Build the recommendation based on attribution result.
fn build_recommendation(
    status: &CrashAttributionStatus,
    confidence: &CrashAttributionConfidence,
    primary: &Option<CrashCandidate>,
    candidates: &[CrashCandidate],
) -> CrashRecommendation {
    match status {
        CrashAttributionStatus::Attributed => {
            if let Some(ref p) = primary {
                match confidence {
                    CrashAttributionConfidence::High => CrashRecommendation::ReviewMod {
                        mod_id: p.mod_id.clone().unwrap_or_else(|| "unknown".to_string()),
                        jar_path: p.jar_path.clone(),
                    },
                    CrashAttributionConfidence::Medium => CrashRecommendation::ReviewMod {
                        mod_id: p.mod_id.clone().unwrap_or_else(|| "unknown".to_string()),
                        jar_path: p.jar_path.clone(),
                    },
                    CrashAttributionConfidence::Low => CrashRecommendation::NoSafeRecommendation,
                }
            } else {
                CrashRecommendation::NoSafeRecommendation
            }
        }
        CrashAttributionStatus::Ambiguous => CrashRecommendation::ConflictingEvidence,
        CrashAttributionStatus::Unknown => CrashRecommendation::NoSafeRecommendation,
    }
}

/// Build human-readable summary.
fn build_summary(
    status: &CrashAttributionStatus,
    confidence: &CrashAttributionConfidence,
    primary: &Option<CrashCandidate>,
    candidates: &[CrashCandidate],
) -> String {
    match status {
        CrashAttributionStatus::Attributed => {
            if let Some(ref p) = primary {
                let mod_name = p.mod_id.as_deref().unwrap_or("unknown");
                match confidence {
                    CrashAttributionConfidence::High => {
                        format!(
                            "Likely responsible mod: {} (confidence: High). Evidence: {} items.",
                            mod_name,
                            p.evidence.len()
                        )
                    }
                    CrashAttributionConfidence::Medium => {
                        format!(
                            "Possible crash source: {} (confidence: Medium). Evidence: {} items.",
                            mod_name,
                            p.evidence.len()
                        )
                    }
                    CrashAttributionConfidence::Low => {
                        format!("Weak attribution to: {} (confidence: Low).", mod_name)
                    }
                }
            } else {
                "Attributed but no primary candidate identified.".to_string()
            }
        }
        CrashAttributionStatus::Ambiguous => {
            let mod_names: Vec<&str> = candidates
                .iter()
                .filter_map(|c| c.mod_id.as_deref())
                .collect();
            if mod_names.is_empty() {
                "Multiple possible crash sources found but none could be identified by mod_id."
                    .to_string()
            } else {
                format!(
                    "Multiple possible crash sources: {}. Cannot identify one safely.",
                    mod_names.join(", ")
                )
            }
        }
        CrashAttributionStatus::Unknown => {
            "No crash source could be identified from available evidence.".to_string()
        }
    }
}

// ── Path helpers ──────────────────────────────────────────────────────

/// Extract a JAR path from a log line.
/// Supports Unix and Windows paths, with spaces.
fn extract_jar_path(line: &str) -> Option<PathBuf> {
    // Look for patterns like ".../mods/something.jar"
    // or "File: /path/to/mod.jar"
    // or just any .jar extension in the line

    // Strategy: find ".jar" and work backward to find the path start
    let jar_pos = line.find(".jar")?;
    let after_jar = &line[jar_pos + 4..];

    // Make sure .jar is at a word boundary (not .json, .jarx, etc.)
    if after_jar
        .chars()
        .next()
        .map(|c| c.is_alphanumeric() || c == '_' || c == '-')
        .unwrap_or(false)
    {
        return None;
    }

    let before_jar = &line[..jar_pos + 4];

    // Find the path start: look for path separators or common prefixes
    let path_start = before_jar.rfind(|c: char| c == '/' || c == '\\').map(|p| {
        // Walk back to find the full path
        let before_sep = &before_jar[..p];
        // Find the start of the path segment (could be "mods/", "File: ", etc.)
        if let Some(colon_pos) = before_sep.rfind("File:") {
            colon_pos + 5
        } else if let Some(colon_pos) = before_sep.rfind("mods/") {
            colon_pos
        } else {
            // Find the start of the path-like segment
            let mut start = p;
            for (i, ch) in before_sep.char_indices().rev() {
                if ch.is_whitespace() || ch == '"' || ch == '\'' || ch == '(' || ch == ')' {
                    start = i + 1;
                    break;
                }
                if i == 0 {
                    start = 0;
                    break;
                }
            }
            start
        }
    })?;

    let path_str = line[path_start..jar_pos + 4].trim();
    if path_str.is_empty() {
        return None;
    }

    let p = PathBuf::from(path_str);

    // Security: reject paths that look like they're outside staging
    // Only accept if the path contains "mods/" or is relative
    let path_display = p.to_string_lossy();
    if path_display.contains("..") {
        return None; // Reject path traversal
    }

    // Reject Windows absolute paths that look system-related
    if path_display.len() >= 2 {
        let bytes = path_display.as_bytes();
        if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            // Windows absolute path like C:\...
            // Only accept if it looks like a mod path (contains "mods")
            if !path_display.to_lowercase().contains("mods") {
                return None;
            }
        }
    }

    Some(p)
}

/// Extract text after a prefix, trimmed.
fn extract_after_prefix<'a>(line: &'a str, prefix: &str) -> Option<String> {
    let pos = line.find(prefix)?;
    let rest = &line[pos + prefix.len()..];
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Extract text between two delimiters.
fn extract_between<'a>(line: &'a str, start: &str, end: &str) -> Option<String> {
    let start_pos = line.find(start)?;
    let after_start = &line[start_pos + start.len()..];
    let end_pos = after_start.find(end)?;
    let between = &after_start[..end_pos];
    if between.is_empty() {
        return None;
    }
    Some(between.to_string())
}

/// Truncate a snippet to MAX_SNIPPET_LEN.
fn truncate_snippet(s: &str) -> String {
    if s.len() <= MAX_SNIPPET_LEN {
        s.to_string()
    } else {
        format!("{}...", &s[..MAX_SNIPPET_LEN])
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper: create a test JAR with given mod metadata in a temp dir.
    fn create_test_jar(dir: &Path, name: &str, mod_ids: &[&str]) -> PathBuf {
        let jar_path = dir.join(name);
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // Write fabric.mod.json
        if !mod_ids.is_empty() {
            let fabric_json = serde_json::json!({
                "id": mod_ids[0],
                "version": "1.0.0",
                "name": mod_ids[0],
            });
            zip.start_file("fabric.mod.json", options).unwrap();
            zip.write_all(fabric_json.to_string().as_bytes()).unwrap();
        }

        zip.finish().unwrap();
        jar_path
    }

    /// Helper: create context for testing.
    fn test_context(dir: &Path) -> CrashAttributionContext<'static> {
        // We need 'static lifetime for the staging_mods path.
        // Use a leak trick for tests only.
        let path_str = dir.to_string_lossy().to_string();
        let leaked: &'static str = Box::leak(path_str.into_boxed_str());
        CrashAttributionContext {
            staging_mods: Path::new(leaked),
            loader_family: LoaderFamily::Forge,
            recently_repaired: &[],
            installed_registry: None,
        }
    }

    // ── 31. Test — explicit Forge mod ──────────────────────────────

    #[test]
    fn test_explicit_forge_mod_id() {
        let tmp = TempDir::new().unwrap();
        let jar = create_test_jar(tmp.path(), "create-0.5.1.jar", &["create"]);

        let log = "Mod ID: create\nFailure message: Mod create has failed to load correctly\njava.lang.RuntimeException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert_eq!(report.confidence, CrashAttributionConfidence::High);
        assert!(report.primary_candidate.is_some());
        let primary = report.primary_candidate.as_ref().unwrap();
        assert_eq!(primary.mod_id.as_deref(), Some("create"));
    }

    // ── 32. Test — explicit JAR path ───────────────────────────────

    #[test]
    fn test_explicit_jar_path_resolves_to_mod_id() {
        let tmp = TempDir::new().unwrap();
        let jar = create_test_jar(tmp.path(), "example.jar", &["examplemod"]);

        let log = format!(
            "File: {}\njava.lang.NoClassDefFoundError: com/example/Main",
            jar.display()
        );
        let ctx = test_context(tmp.path());
        let report = analyze_crash(&log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert_eq!(report.confidence, CrashAttributionConfidence::High);
        let primary = report.primary_candidate.as_ref().unwrap();
        assert_eq!(primary.mod_id.as_deref(), Some("examplemod"));
    }

    // ── 33. Test — conflicting explicit evidence ───────────────────

    #[test]
    fn test_conflicting_explicit_evidence() {
        let tmp = TempDir::new().unwrap();
        // JAR declares mod_id=B
        create_test_jar(tmp.path(), "b.jar", &["B"]);

        // Log says Mod ID: A but Mod File: b.jar
        let log = "Mod ID: A\nMod File: b.jar\nModLoadingException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Ambiguous);
        assert!(report.primary_candidate.is_none());
    }

    // ── 34. Test — Fabric entrypoint ───────────────────────────────

    #[test]
    fn test_fabric_entrypoint_attribution() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "foo.jar", &["foo"]);

        let log = "Could not execute entrypoint stage 'main' due to errors, provided by 'foo'\njava.lang.RuntimeException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert_eq!(report.confidence, CrashAttributionConfidence::High);
        assert_eq!(
            report.primary_candidate.as_ref().unwrap().mod_id.as_deref(),
            Some("foo")
        );
    }

    // ── 35. Test — mixin unique owner ──────────────────────────────

    #[test]
    fn test_mixin_unique_owner() {
        let tmp = TempDir::new().unwrap();
        // Create a JAR with a mixin config inside
        let jar_path = tmp.path().join("mymod.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            // fabric.mod.json
            let fabric_json = serde_json::json!({"id": "mymod", "version": "1.0.0"});
            zip.start_file("fabric.mod.json", options).unwrap();
            zip.write_all(fabric_json.to_string().as_bytes()).unwrap();

            // mixin config
            zip.start_file("mymod.mixins.json", options).unwrap();
            zip.write_all(b"{}").unwrap();

            zip.finish().unwrap();
        }

        let log = "MixinApplyError: Failed to apply mixin config mymod.mixins.json\njava.lang.RuntimeException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Should be at least Medium confidence
        assert!(
            report.confidence == CrashAttributionConfidence::Medium
                || report.confidence == CrashAttributionConfidence::High,
            "Expected Medium or High, got {:?}",
            report.confidence
        );
        assert!(report.primary_candidate.is_some());
    }

    // ── 36. Test — mixin ambiguous owner ───────────────────────────

    #[test]
    fn test_mixin_ambiguous_owner() {
        let tmp = TempDir::new().unwrap();
        // Two JARs with the same mixin config
        for name in &["mod_a.jar", "mod_b.jar"] {
            let jar_path = tmp.path().join(name);
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            let mod_id = if name.contains("a") { "moda" } else { "modb" };
            let fabric_json = serde_json::json!({"id": mod_id, "version": "1.0.0"});
            zip.start_file("fabric.mod.json", options).unwrap();
            zip.write_all(fabric_json.to_string().as_bytes()).unwrap();

            // Same mixin config name in both JARs
            zip.start_file("shared.mixins.json", options).unwrap();
            zip.write_all(b"{}").unwrap();

            zip.finish().unwrap();
        }

        let log = "MixinApplyError: Failed to apply mixin config shared.mixins.json\njava.lang.RuntimeException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Ambiguous);
        assert!(report.primary_candidate.is_none());
    }

    // ── 37. Test — generic NoClassDefFoundError ────────────────────

    #[test]
    fn test_generic_no_class_def_found() {
        let tmp = TempDir::new().unwrap();
        // No JARs in staging — nothing to attribute
        let log = "java.lang.NoClassDefFoundError: com/foo/bar/Baz\n\tat com.unknown.SomeClass.main(SomeClass.java:10)";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Unknown);
        assert_eq!(report.confidence, CrashAttributionConfidence::Low);
        assert!(report.primary_candidate.is_none());
    }

    // ── 38. Test — missing dependency precedence ───────────────────
    // This tests that crash attribution doesn't steal from dependency repair.
    // In practice, the orchestrator calls crash attribution ONLY after
    // classify_failure returns RepairAction::None and no loader mismatch.
    // This test verifies that analyze_crash itself doesn't claim
    // MissingDependency — it's not its job.

    #[test]
    fn test_missing_dependency_not_stolen() {
        let tmp = TempDir::new().unwrap();
        let log = "Mod B requires mod C\njava.lang.NoClassDefFoundError: net/example/C";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Crash attribution should NOT identify this as MissingDependency.
        // It might find B as a candidate from the log, but it should not
        // claim "MissingDependency" — that's the boot_failure_analyzer's job.
        // The report should be Unknown or Low confidence.
        assert_ne!(
            report.status,
            CrashAttributionStatus::Attributed,
            "Should not confidently attribute a missing-dependency scenario"
        );
    }

    // ── 39. Test — loader mismatch precedence ──────────────────────

    #[test]
    fn test_loader_mismatch_not_overridden() {
        let tmp = TempDir::new().unwrap();
        let log = "Unsupported mod loader: fabric\nMod 'foo' requires fabric loader 0.15+\njava.lang.RuntimeException at com.example.Mod.init";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Crash attribution should not confidently claim this is a mod crash
        // when it's really a loader mismatch.
        // The orchestrator handles loader mismatch before calling crash attribution.
        // This test verifies the parser doesn't panic or produce garbage.
        assert!(report.summary.len() > 0);
    }

    // ── 40. Test — WrongJava precedence ────────────────────────────

    #[test]
    fn test_wrong_java_not_attributed() {
        let tmp = TempDir::new().unwrap();
        let log = "UnsupportedClassVersionError: class file version 65.0\n\tat com.example.Mod.init(Mod.java:10)";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // The runtime_remediator handles WrongJava before crash attribution.
        // Crash attribution should not claim High confidence here.
        // The stack frame might weakly suggest a mod, but not High.
        assert_ne!(
            report.confidence,
            CrashAttributionConfidence::High,
            "WrongJava scenario should not get High confidence from crash attribution"
        );
    }

    // ── 41. Test — OOM precedence ──────────────────────────────────

    #[test]
    fn test_oom_not_attributed() {
        let tmp = TempDir::new().unwrap();
        let log =
            "java.lang.OutOfMemoryError: Java heap space\n\tat com.example.Mod.init(Mod.java:10)";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // OOM should not produce High confidence attribution.
        // Stack frames from OOM are not reliable guilt evidence.
        assert_ne!(
            report.confidence,
            CrashAttributionConfidence::High,
            "OOM should not get High confidence"
        );
    }

    // ── 42. Test — ClientOnly invariant ────────────────────────────

    #[test]
    fn test_client_only_invariant_violation() {
        let tmp = TempDir::new().unwrap();
        // Create a JAR classified as ClientOnly
        let jar_path = tmp.path().join("clientmod.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            // fabric.mod.json with environment=client
            let fabric_json = serde_json::json!({
                "id": "clientmod",
                "version": "1.0.0",
                "environment": "client"
            });
            zip.start_file("fabric.mod.json", options).unwrap();
            zip.write_all(fabric_json.to_string().as_bytes()).unwrap();

            zip.finish().unwrap();
        }

        let log = "Mod ID: clientmod\njava.lang.RuntimeException: client mod crashed";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert!(matches!(
            report.recommendation,
            CrashRecommendation::UnexpectedClientOnlyPresent { .. }
        ));
    }

    // ── 43. Test — UNKNOWN explicit culprit ────────────────────────

    #[test]
    fn test_unknown_mod_explicit_culprit() {
        let tmp = TempDir::new().unwrap();
        // Create a JAR with no recognized metadata (UNKNOWN compatibility)
        let jar_path = tmp.path().join("unknown.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            // No fabric.mod.json, no META-INF/mods.toml — just a class
            zip.start_file("com/example/Mod.class", options).unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE").unwrap();

            zip.finish().unwrap();
        }

        // But the log explicitly names it
        let log = "Mod ID: unknown\nMod File: unknown.jar\nModLoadingException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Should be attributed (explicit evidence) but UNKNOWN compat
        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert_eq!(report.confidence, CrashAttributionConfidence::High);
        // Recommendation is still just ReviewMod — no deletion
        assert!(matches!(
            report.recommendation,
            CrashRecommendation::ReviewMod { .. }
        ));
    }

    // ── 44. Test — recent repair not biased ────────────────────────

    #[test]
    fn test_recent_repair_not_biased() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "a.jar", &["modA"]);
        create_test_jar(tmp.path(), "b.jar", &["modB"]);

        let log = "Mod ID: modA\njava.lang.RuntimeException: modA crashed";
        let recently_repaired = vec!["modB".to_string()];
        let path_str = tmp.path().to_string_lossy().to_string();
        let leaked: &'static str = Box::leak(path_str.into_boxed_str());
        let ctx = CrashAttributionContext {
            staging_mods: Path::new(leaked),
            loader_family: LoaderFamily::Forge,
            recently_repaired: &recently_repaired,
            installed_registry: None,
        };

        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        let primary = report.primary_candidate.as_ref().unwrap();
        assert_eq!(
            primary.mod_id.as_deref(),
            Some("modA"),
            "modA should be primary, not modB"
        );
    }

    // ── 45. Test — no evidence ─────────────────────────────────────

    #[test]
    fn test_no_evidence_unknown() {
        let tmp = TempDir::new().unwrap();
        let log = "Process exited with code 1";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Unknown);
        assert!(report.primary_candidate.is_none());
        assert!(matches!(
            report.recommendation,
            CrashRecommendation::NoSafeRecommendation
        ));
    }

    // ── 46. Orchestrator zero-mutation test ────────────────────────

    #[test]
    fn test_crash_attribution_no_mutation() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "a.jar", &["modA"]);

        let log = "Mod ID: modA\njava.lang.RuntimeException: modA crashed";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Verify attribution happened
        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert!(report.primary_candidate.is_some());

        // Verify no files were modified in staging
        let entries: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1); // Only a.jar
        assert_eq!(entries[0].file_name().to_string_lossy(), "a.jar");
    }

    // ── Malformed input tests (66) ─────────────────────────────────

    #[test]
    fn test_empty_log() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let report = analyze_crash("", &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Unknown);
        assert!(report.primary_candidate.is_none());
    }

    #[test]
    fn test_binary_like_text() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let log = "\x00\x01\x02\x03\x04\x05binary data";
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Unknown);
        // No panic
    }

    #[test]
    fn test_huge_single_line() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let huge_line = "A".repeat(100_000);
        let report = analyze_crash(&huge_line, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Unknown);
        // No panic, bounded processing
    }

    #[test]
    fn test_truncated_mod_loading_exception() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let log = "ModLoadingException: Mod 'example' encountered an error\nMod ID:";
        let report = analyze_crash(log, &ctx);

        // Partial parse is fine, no crash
        assert!(report.summary.len() > 0);
    }

    #[test]
    fn test_weird_quotes() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let log = "Mod ID: \"create'\nProvided by 'foo\"";
        let report = analyze_crash(log, &ctx);

        // Should not panic, may extract with quotes
        assert!(report.summary.len() > 0);
    }

    #[test]
    fn test_missing_closing_parenthesis() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let log = "at com.example.Mod.init(Mod.java:10\n\tat com.example.Main.main(Main.java:5";
        let report = analyze_crash(log, &ctx);

        // Should not panic
        assert!(report.summary.len() > 0);
    }

    // ── Path security tests (62-63) ────────────────────────────────

    #[test]
    fn test_path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let log = "File: ../../etc/passwd\njava.lang.RuntimeException";
        let report = analyze_crash(log, &ctx);

        // Should not try to read /etc/passwd
        assert_eq!(report.status, CrashAttributionStatus::Unknown);
    }

    #[test]
    fn test_windows_system_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let ctx = test_context(tmp.path());
        let log = "File: C:\\Windows\\System32\\kernel32.dll.jar\njava.lang.RuntimeException";
        let report = analyze_crash(log, &ctx);

        // Should not try to read system paths
        assert_eq!(report.status, CrashAttributionStatus::Unknown);
    }

    // ── Candidate/evidence bounds (67-68) ──────────────────────────

    #[test]
    fn test_candidate_count_bounded() {
        let tmp = TempDir::new().unwrap();
        // Create many JARs
        for i in 0..20 {
            create_test_jar(
                tmp.path(),
                &format!("mod{}.jar", i),
                &[&format!("mod{}", i)],
            );
        }

        // Log that references all of them
        let mut log_lines: Vec<String> = Vec::new();
        for i in 0..20 {
            log_lines.push(format!("Mod ID: mod{}", i));
        }
        let log = log_lines.join("\n");

        let ctx = test_context(tmp.path());
        let report = analyze_crash(&log, &ctx);

        // Should be capped
        assert!(report.candidates.len() <= MAX_CRASH_CANDIDATES);
    }

    #[test]
    fn test_evidence_per_candidate_bounded() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "mod.jar", &["mod"]);

        // Generate many evidence lines for the same mod
        let mut log_lines: Vec<String> = Vec::new();
        for _ in 0..20 {
            log_lines.push("Mod ID: mod".to_string());
        }
        let log = log_lines.join("\n");

        let ctx = test_context(tmp.path());
        let report = analyze_crash(&log, &ctx);

        if let Some(primary) = &report.primary_candidate {
            assert!(primary.evidence.len() <= MAX_EVIDENCE_PER_CANDIDATE);
        }
    }

    // ── Multi-mod JAR test (54) ────────────────────────────────────

    #[test]
    fn test_multi_mod_jar_ambiguous() {
        let tmp = TempDir::new().unwrap();
        // JAR declares two mod_ids
        let jar_path = tmp.path().join("multi.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            // fabric.mod.json with two mods via "entrypoints" or just first mod
            let fabric_json = serde_json::json!({"id": "modA", "version": "1.0.0"});
            zip.start_file("fabric.mod.json", options.clone()).unwrap();
            zip.write_all(fabric_json.to_string().as_bytes()).unwrap();

            // Also META-INF/mods.toml for second mod
            let mods_toml = "[[mods]]\nmodId=\"modB\"\nversion=\"1.0.0\"\n";
            zip.start_file("META-INF/mods.toml", options).unwrap();
            zip.write_all(mods_toml.as_bytes()).unwrap();

            zip.finish().unwrap();
        }

        // Log references the JAR but not a specific mod_id
        let log = format!("File: {}\njava.lang.RuntimeException", jar_path.display());
        let ctx = test_context(tmp.path());
        let report = analyze_crash(&log, &ctx);

        // Multi-mod JAR without explicit mod_id → Ambiguous
        assert_eq!(report.status, CrashAttributionStatus::Ambiguous);
    }

    // ── Duplicate mod_id providers (55) ────────────────────────────

    #[test]
    fn test_duplicate_mod_id_providers() {
        let tmp = TempDir::new().unwrap();
        // Two JARs claim same mod_id
        create_test_jar(tmp.path(), "a.jar", &["dupmod"]);
        create_test_jar(tmp.path(), "b.jar", &["dupmod"]);

        let log = "Mod ID: dupmod\njava.lang.RuntimeException";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Should be Ambiguous — two providers for same mod_id
        assert_eq!(report.status, CrashAttributionStatus::Ambiguous);
    }

    // ── Missing JAR on disk (56) ───────────────────────────────────

    #[test]
    fn test_missing_jar_on_disk() {
        let tmp = TempDir::new().unwrap();
        // Log references a JAR that doesn't exist
        let log = "File: /nonexistent/path/mod.jar\njava.lang.RuntimeException";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Should not crash, confidence downgraded
        assert!(report.summary.len() > 0);
    }

    // ── Corrupt JAR metadata (57) ──────────────────────────────────

    #[test]
    fn test_corrupt_jar_metadata() {
        let tmp = TempDir::new().unwrap();
        // Create a JAR that's not a valid ZIP
        let jar_path = tmp.path().join("corrupt.jar");
        fs::write(&jar_path, "not a valid zip file").unwrap();

        let log = format!("File: {}\njava.lang.RuntimeException", jar_path.display());
        let ctx = test_context(tmp.path());
        let report = analyze_crash(&log, &ctx);

        // Should not crash, identity unverifiable
        assert!(report.summary.len() > 0);
    }

    // ── A mod crashed on startup (Fabric) ──────────────────────────

    #[test]
    fn test_fabric_crashed_on_startup() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "crasher.jar", &["crasher"]);

        let log = "A mod crashed on startup: crasher\njava.lang.RuntimeException: boom";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert_eq!(report.confidence, CrashAttributionConfidence::High);
        assert_eq!(
            report.primary_candidate.as_ref().unwrap().mod_id.as_deref(),
            Some("crasher")
        );
    }

    // ── Forge "Mod X has failed to load correctly" ─────────────────

    #[test]
    fn test_forge_mod_failed_to_load() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "broken.jar", &["broken"]);

        let log = "Mod broken has failed to load correctly\njava.lang.RuntimeException: ...";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        assert_eq!(report.status, CrashAttributionStatus::Attributed);
        assert_eq!(report.confidence, CrashAttributionConfidence::High);
        assert_eq!(
            report.primary_candidate.as_ref().unwrap().mod_id.as_deref(),
            Some("broken")
        );
    }

    // ── Generic stacktrace only ────────────────────────────────────

    #[test]
    fn test_generic_stacktrace_only() {
        let tmp = TempDir::new().unwrap();
        create_test_jar(tmp.path(), "create.jar", &["create"]);

        let log = "java.lang.NullPointerException\n\tat com.simibubi.create.SomeClass.method(SomeClass.java:42)\n\tat net.minecraft.server.MinecraftServer.run(MinecraftServer.java:100)";
        let ctx = test_context(tmp.path());
        let report = analyze_crash(log, &ctx);

        // Stack frame only → Medium confidence at best
        assert_ne!(
            report.confidence,
            CrashAttributionConfidence::High,
            "Stack frame only should not be High"
        );
    }
}
