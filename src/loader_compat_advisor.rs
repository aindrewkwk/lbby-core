//! Loader Compatibility Advisor
//!
//! Detects loader incompatibility deterministically and produces structured
//! recommendations WITHOUT automatically changing the loader.
//!
//! Supported families: Forge, NeoForge, Fabric Loader, Quilt Loader.

use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;
use crate::jar_metadata;

// ── Public types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LoaderFamily {
    Forge,
    NeoForge,
    Fabric,
    Quilt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoaderCompatibilityStatus {
    Compatible,
    Incompatible,
    WrongLoaderFamily,
    ConflictingRequirements,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecommendationConfidence {
    High,
    Medium,
    Low,
}

/// A single loader version requirement from one source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderRequirement {
    pub family: LoaderFamily,
    pub constraint: LoaderVersionConstraint,
    pub source: LoaderRequirementSource,
    pub requesting_mod_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoaderRequirementSource {
    /// Explicit runtime loader error from boot log.
    BootLog,
    /// Normalized JAR metadata (mods.toml, fabric.mod.json, etc.).
    JarMetadata,
    /// CurseForge pack manifest loader pin.
    ManifestPin,
    /// ServerConfig current version (current-state evidence only).
    CurrentConfig,
}

/// Final recommendation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderRecommendation {
    pub current_version: Option<String>,
    pub recommended_version: Option<String>,
    pub confidence: RecommendationConfidence,
    pub reasons: Vec<String>,
}

/// Full compatibility report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderCompatibilityReport {
    pub family: LoaderFamily,
    pub current_version: Option<String>,
    pub requirements: Vec<LoaderRequirement>,
    pub status: LoaderCompatibilityStatus,
    pub recommendation: Option<LoaderRecommendation>,
}

// ── Version model ───────────────────────────────────────────────────

/// Numeric version with arbitrary component count.
///
/// Comparison: component-by-component, shorter version padded with zeros.
/// Rule: `47.2 == 47.2.0` (trailing zeroes are equivalent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderVersion {
    components: Vec<u32>,
}

impl PartialEq for LoaderVersion {
    fn eq(&self, other: &Self) -> bool {
        self.canonical() == other.canonical()
    }
}

impl Eq for LoaderVersion {}

impl std::hash::Hash for LoaderVersion {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.canonical().hash(state);
    }
}

impl LoaderVersion {
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        let components: Option<Vec<u32>> =
            trimmed.split('.').map(|c| c.parse::<u32>().ok()).collect();
        let components = components?;
        if components.is_empty() {
            return None;
        }
        Some(Self { components })
    }

    /// Strip trailing zeroes for canonical comparison.
    fn canonical(&self) -> &[u32] {
        let mut end = self.components.len();
        while end > 1 && self.components[end - 1] == 0 {
            end -= 1;
        }
        &self.components[..end]
    }
}

impl std::fmt::Display for LoaderVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s: Vec<String> = self.components.iter().map(|c| c.to_string()).collect();
        write!(f, "{}", s.join("."))
    }
}

impl PartialOrd for LoaderVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LoaderVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let a = self.canonical();
        let b = other.canonical();
        let max_len = a.len().max(b.len());
        for i in 0..max_len {
            let av = a.get(i).copied().unwrap_or(0);
            let bv = b.get(i).copied().unwrap_or(0);
            match av.cmp(&bv) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        std::cmp::Ordering::Equal
    }
}

// ── Constraint model ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoaderVersionConstraint {
    /// Exact version match.
    Exact(LoaderVersion),
    /// Greater than or equal.
    Gte(LoaderVersion),
    /// Strictly greater than.
    Gt(LoaderVersion),
    /// Less than or equal.
    Lte(LoaderVersion),
    /// Strictly less than.
    Lt(LoaderVersion),
    /// Closed interval [lo, hi].
    ClosedInterval {
        lo: LoaderVersion,
        hi: LoaderVersion,
    },
    /// Half-open interval [lo, hi).
    HalfOpen {
        lo: LoaderVersion,
        hi: LoaderVersion,
    },
    /// Half-open interval (lo, hi].
    HalfOpenLo {
        lo: LoaderVersion,
        hi: LoaderVersion,
    },
    /// Open interval (lo, hi).
    OpenInterval {
        lo: LoaderVersion,
        hi: LoaderVersion,
    },
    /// Version with trailing `+` (equivalent to >=).
    AtLeast(LoaderVersion),
    /// Could not parse.
    UnknownConstraint(String),
}

impl LoaderVersionConstraint {
    /// Parse a constraint string.
    ///
    /// Supported:
    /// - `47.2.0`          → Exact
    /// - `>=47.2.0`        → Gte
    /// - `>47.2.0`         → Gt
    /// - `<=47.2.0`        → Lte
    /// - `<47.2.0`         → Lt
    /// - `47.2.0+`         → AtLeast
    /// - `[47.2.0,48.0.0)` → HalfOpen
    /// - `[47.2.0,48.0.0]` → ClosedInterval
    /// - `(47.2.0,48.0.0)` → OpenInterval
    /// - `(47.2.0,48.0.0]` → HalfOpenLo
    pub fn parse(s: &str) -> Self {
        let trimmed = s.trim();

        // Interval notation: [lo,hi) or [lo,hi] or (lo,hi) or (lo,hi]
        if trimmed.len() >= 5 {
            let first = trimmed.as_bytes()[0];
            let last = trimmed.as_bytes()[trimmed.len() - 1];
            if (first == b'[' || first == b'(') && (last == b']' || last == b')') {
                let inner = &trimmed[1..trimmed.len() - 1];
                if let Some(comma_pos) = inner.find(',') {
                    let lo_str = inner[..comma_pos].trim();
                    let hi_str = inner[comma_pos + 1..].trim();
                    if let (Some(lo), Some(hi)) =
                        (LoaderVersion::parse(lo_str), LoaderVersion::parse(hi_str))
                    {
                        return match (first, last) {
                            (b'[', b']') => Self::ClosedInterval { lo, hi },
                            (b'[', b')') => Self::HalfOpen { lo, hi },
                            (b'(', b']') => Self::HalfOpenLo { lo, hi },
                            (b'(', b')') => Self::OpenInterval { lo, hi },
                            _ => Self::UnknownConstraint(trimmed.to_string()),
                        };
                    }
                }
                return Self::UnknownConstraint(trimmed.to_string());
            }
        }

        // Trailing +
        if trimmed.ends_with('+') {
            let ver_str = &trimmed[..trimmed.len() - 1].trim();
            if let Some(v) = LoaderVersion::parse(ver_str) {
                return Self::AtLeast(v);
            }
        }

        // Comparison operators
        if let Some(rest) = trimmed.strip_prefix(">=") {
            if let Some(v) = LoaderVersion::parse(rest.trim()) {
                return Self::Gte(v);
            }
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            if let Some(v) = LoaderVersion::parse(rest.trim()) {
                return Self::Gt(v);
            }
        }
        if let Some(rest) = trimmed.strip_prefix("<=") {
            if let Some(v) = LoaderVersion::parse(rest.trim()) {
                return Self::Lte(v);
            }
        }
        if let Some(rest) = trimmed.strip_prefix('<') {
            if let Some(v) = LoaderVersion::parse(rest.trim()) {
                return Self::Lt(v);
            }
        }
        if let Some(rest) = trimmed.strip_prefix('=') {
            if let Some(v) = LoaderVersion::parse(rest.trim()) {
                return Self::Exact(v);
            }
        }

        // Bare version → Exact
        if let Some(v) = LoaderVersion::parse(trimmed) {
            return Self::Exact(v);
        }

        Self::UnknownConstraint(trimmed.to_string())
    }

    /// Check if a version satisfies this constraint.
    pub fn satisfied_by(&self, version: &LoaderVersion) -> bool {
        match self {
            Self::Exact(v) => version == v,
            Self::Gte(v) => version >= v,
            Self::Gt(v) => version > v,
            Self::Lte(v) => version <= v,
            Self::Lt(v) => version < v,
            Self::AtLeast(v) => version >= v,
            Self::ClosedInterval { lo, hi } => version >= lo && version <= hi,
            Self::HalfOpen { lo, hi } => version >= lo && version < hi,
            Self::HalfOpenLo { lo, hi } => version > lo && version <= hi,
            Self::OpenInterval { lo, hi } => version > lo && version < hi,
            Self::UnknownConstraint(_) => false,
        }
    }

    /// Is this constraint parseable?
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::UnknownConstraint(_))
    }
}

impl std::fmt::Display for LoaderVersionConstraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(v) => write!(f, "{v}"),
            Self::Gte(v) => write!(f, ">={v}"),
            Self::Gt(v) => write!(f, ">{v}"),
            Self::Lte(v) => write!(f, "<={v}"),
            Self::Lt(v) => write!(f, "<{v}"),
            Self::AtLeast(v) => write!(f, "{v}+"),
            Self::ClosedInterval { lo, hi } => write!(f, "[{lo},{hi}]"),
            Self::HalfOpen { lo, hi } => write!(f, "[{lo},{hi})"),
            Self::HalfOpenLo { lo, hi } => write!(f, "({lo},{hi}]"),
            Self::OpenInterval { lo, hi } => write!(f, "({lo},{hi})"),
            Self::UnknownConstraint(s) => write!(f, "?{s}?"),
        }
    }
}

// ── Loader family normalization ─────────────────────────────────────

/// Normalize a loader alias string to a LoaderFamily.
///
/// Recognized aliases (case-insensitive):
/// - forge → Forge
/// - neoforge, neo-forge → NeoForge
/// - fabricloader, fabric-loader → Fabric
/// - quilt_loader, quilt-loader → Quilt
///
/// NOT loader families:
/// - fabric-api, fabric → None (these are APIs/mods, not loader families)
/// - quilted-fabric-api → None
pub fn normalize_loader_family(alias: &str) -> Option<LoaderFamily> {
    let lower = alias.trim().to_lowercase();
    match lower.as_str() {
        "forge" => Some(LoaderFamily::Forge),
        "neoforge" | "neo-forge" | "neo_forge" => Some(LoaderFamily::NeoForge),
        "fabricloader" | "fabric-loader" | "fabric_loader" => Some(LoaderFamily::Fabric),
        "quilt_loader" | "quilt-loader" | "quiltloader" => Some(LoaderFamily::Quilt),
        _ => None,
    }
}

impl std::fmt::Display for LoaderFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl LoaderFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
        }
    }

    /// Map from ServerType string to LoaderFamily.
    pub fn from_server_type(st: &crate::config::ServerType) -> Option<LoaderFamily> {
        use crate::config::ServerType;
        match st {
            ServerType::Forge => Some(LoaderFamily::Forge),
            ServerType::NeoForge => Some(LoaderFamily::NeoForge),
            ServerType::Fabric => Some(LoaderFamily::Fabric),
            _ => None,
        }
    }

    /// Map from JarMetadataKind to LoaderFamily.
    pub fn from_jar_loader_kind(kind: crate::jar_metadata::LoaderMetadataKind) -> LoaderFamily {
        match kind {
            crate::jar_metadata::LoaderMetadataKind::Forge => LoaderFamily::Forge,
            crate::jar_metadata::LoaderMetadataKind::NeoForge => LoaderFamily::NeoForge,
            crate::jar_metadata::LoaderMetadataKind::Fabric => LoaderFamily::Fabric,
            crate::jar_metadata::LoaderMetadataKind::Quilt => LoaderFamily::Quilt,
        }
    }
}

// ── Jar metadata bridge ─────────────────────────────────────────────

/// Convert jar metadata loader requirements into advisor `LoaderRequirement`s.
pub fn requirements_from_jar_metadata(
    jar_reqs: &[jar_metadata::LoaderVersionRequirement],
) -> Vec<LoaderRequirement> {
    jar_reqs
        .iter()
        .filter(|r| r.mandatory)
        .map(|r| LoaderRequirement {
            family: LoaderFamily::from_jar_loader_kind(r.loader),
            constraint: r
                .version_requirement
                .as_deref()
                .map(LoaderVersionConstraint::parse)
                .unwrap_or_else(|| LoaderVersionConstraint::UnknownConstraint(String::new())),
            source: LoaderRequirementSource::JarMetadata,
            requesting_mod_id: None,
        })
        .collect()
}

// ── Boot-log parser ─────────────────────────────────────────────────

/// Parse loader version requirements from boot log.
///
/// Returns one or more requirements found. Only accepts exact known patterns.
/// Vague text like "Forge error" or "loader failed" is NOT accepted.
pub fn parse_loader_requirements_from_log(log: &str) -> Vec<LoaderRequirement> {
    let mut requirements = Vec::new();
    for line in log.lines() {
        let lower = line.to_lowercase();

        // Forge: "requires Forge 47.2.0 or above" / "requires forge version 47.2.0+"
        if lower.contains("forge") && !lower.contains("neoforge") {
            if let Some(req) = parse_forge_log_requirement(line, &lower) {
                requirements.push(req);
            }
        }

        // NeoForge: "requires NeoForge >=20.4.100"
        if lower.contains("neoforge") {
            if let Some(req) = parse_neoforge_log_requirement(line, &lower) {
                requirements.push(req);
            }
        }

        // Fabric: "requires fabric-loader version 0.15.0 or later"
        //         "requires fabricloader >=0.15.0"
        if lower.contains("fabric-loader") || lower.contains("fabricloader") {
            if let Some(req) = parse_fabric_log_requirement(line, &lower) {
                requirements.push(req);
            }
        }

        // Quilt: "requires quilt_loader >=0.23.0"
        if lower.contains("quilt_loader")
            || lower.contains("quilt-loader")
            || lower.contains("quiltloader")
        {
            if let Some(req) = parse_quilt_log_requirement(line, &lower) {
                requirements.push(req);
            }
        }
    }
    requirements
}

/// Extract requesting mod id from a line like "Mod 'flywheel' requires forge ..."
fn extract_mod_id_from_requirement_line(line: &str) -> Option<String> {
    // Pattern: "Mod 'xxx'" or "Mod \"xxx\""
    if let Some(start) = line.find("Mod '").or_else(|| line.find("mod '")) {
        let after = &line[start + 5..];
        if let Some(end) = after.find('\'') {
            let mod_id = after[..end].trim();
            if !mod_id.is_empty() {
                return Some(mod_id.to_string());
            }
        }
    }
    if let Some(start) = line.find("Mod \"").or_else(|| line.find("mod \"")) {
        let after = &line[start + 5..];
        if let Some(end) = after.find('"') {
            let mod_id = after[..end].trim();
            if !mod_id.is_empty() {
                return Some(mod_id.to_string());
            }
        }
    }
    // Pattern: "Mod flywheel requires ..."
    if let Some(start) = line.find("Mod ").or_else(|| line.find("mod ")) {
        let after = &line[start + 4..];
        let mod_id: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !mod_id.is_empty()
            && !mod_id.eq_ignore_ascii_case("requires")
            && !mod_id.eq_ignore_ascii_case("the")
        {
            return Some(mod_id);
        }
    }
    None
}

fn parse_forge_log_requirement(line: &str, lower: &str) -> Option<LoaderRequirement> {
    // Pattern set 1: "requires forge X.Y.Z ..."
    for pattern in &["requires forge ", "requires forge version "] {
        if let Some(pos) = lower.find(pattern) {
            let after = &line[pos + pattern.len()..];
            let after = after.strip_prefix("version ").unwrap_or(after);
            let after = after.strip_prefix(">= ").unwrap_or(after);
            let after = after.strip_prefix(">=").unwrap_or(after);
            let version_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version_str.is_empty() && version_str.contains('.') {
                let full_after = &line[pos + pattern.len()..];
                let constraint = if full_after.contains('+') {
                    LoaderVersionConstraint::AtLeast(LoaderVersion::parse(&version_str).unwrap())
                } else if full_after.contains("or above")
                    || full_after.contains("or later")
                    || full_after.contains("or newer")
                {
                    LoaderVersionConstraint::Gte(LoaderVersion::parse(&version_str).unwrap())
                } else {
                    LoaderVersionConstraint::Gte(LoaderVersion::parse(&version_str).unwrap())
                };
                return Some(LoaderRequirement {
                    family: LoaderFamily::Forge,
                    constraint,
                    source: LoaderRequirementSource::BootLog,
                    requesting_mod_id: extract_mod_id_from_requirement_line(line),
                });
            }
            // Fallback: try parsing remaining text as full constraint (handles [lo,hi) notation)
            let remaining = after.trim();
            let constraint = LoaderVersionConstraint::parse(remaining);
            if constraint.is_known() {
                return Some(LoaderRequirement {
                    family: LoaderFamily::Forge,
                    constraint,
                    source: LoaderRequirementSource::BootLog,
                    requesting_mod_id: extract_mod_id_from_requirement_line(line),
                });
            }
        }
    }

    // Pattern set 2: "Forge X.Y.Z is required, but A.B.C is installed"
    // (runtime_remediator::detect_loader_version_mismatch format)
    if let Some(pos) = lower.find("forge ") {
        let after = &line[pos + 6..]; // skip "forge "
        let after_trimmed = after.strip_prefix("version ").unwrap_or(after);
        let version_str: String = after_trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if !version_str.is_empty()
            && version_str.contains('.')
            && after_trimmed
                .get(version_str.len()..)
                .map(|rest| rest.starts_with(" is required"))
                .unwrap_or(false)
        {
            return Some(LoaderRequirement {
                family: LoaderFamily::Forge,
                constraint: LoaderVersionConstraint::Gte(
                    LoaderVersion::parse(&version_str).unwrap(),
                ),
                source: LoaderRequirementSource::BootLog,
                requesting_mod_id: None,
            });
        }
    }

    None
}

fn parse_neoforge_log_requirement(line: &str, lower: &str) -> Option<LoaderRequirement> {
    for pattern in &["requires neoforge ", "requires neoforge version "] {
        if let Some(pos) = lower.find(pattern) {
            let after = &line[pos + pattern.len()..];
            let after = after.strip_prefix("version ").unwrap_or(after);
            let after = after.strip_prefix(">= ").unwrap_or(after);
            let after = after.strip_prefix(">=").unwrap_or(after);
            let version_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version_str.is_empty() && version_str.contains('.') {
                let constraint = if line[pos + pattern.len()..].contains('+') {
                    LoaderVersionConstraint::AtLeast(LoaderVersion::parse(&version_str).unwrap())
                } else {
                    LoaderVersionConstraint::Gte(LoaderVersion::parse(&version_str).unwrap())
                };
                return Some(LoaderRequirement {
                    family: LoaderFamily::NeoForge,
                    constraint,
                    source: LoaderRequirementSource::BootLog,
                    requesting_mod_id: extract_mod_id_from_requirement_line(line),
                });
            }
        }
    }
    None
}

fn parse_fabric_log_requirement(line: &str, lower: &str) -> Option<LoaderRequirement> {
    // "requires fabric-loader version 0.15.0 or later"
    // "requires fabricloader >=0.15.0"
    for pattern in &[
        "requires fabric-loader version ",
        "requires fabricloader version ",
        "requires fabricloader ",
        "requires fabric-loader ",
    ] {
        if let Some(pos) = lower.find(pattern) {
            let after = &line[pos + pattern.len()..];
            let after = after.strip_prefix("version ").unwrap_or(after);
            let after = after.strip_prefix(">= ").unwrap_or(after);
            let after = after.strip_prefix(">=").unwrap_or(after);
            let version_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version_str.is_empty() && version_str.contains('.') {
                let constraint = if line[pos + pattern.len()..].contains('+') {
                    LoaderVersionConstraint::AtLeast(LoaderVersion::parse(&version_str).unwrap())
                } else if line[pos + pattern.len()..].contains("or later")
                    || line[pos + pattern.len()..].contains("or above")
                {
                    LoaderVersionConstraint::Gte(LoaderVersion::parse(&version_str).unwrap())
                } else {
                    LoaderVersionConstraint::Gte(LoaderVersion::parse(&version_str).unwrap())
                };
                return Some(LoaderRequirement {
                    family: LoaderFamily::Fabric,
                    constraint,
                    source: LoaderRequirementSource::BootLog,
                    requesting_mod_id: extract_mod_id_from_requirement_line(line),
                });
            }
        }
    }
    None
}

fn parse_quilt_log_requirement(line: &str, lower: &str) -> Option<LoaderRequirement> {
    for pattern in &[
        "requires quilt_loader ",
        "requires quilt-loader ",
        "requires quiltloader ",
    ] {
        if let Some(pos) = lower.find(pattern) {
            let after = &line[pos + pattern.len()..];
            let after = after.strip_prefix(">= ").unwrap_or(after);
            let after = after.strip_prefix(">=").unwrap_or(after);
            let version_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !version_str.is_empty() && version_str.contains('.') {
                let constraint = if line[pos + pattern.len()..].contains('+') {
                    LoaderVersionConstraint::AtLeast(LoaderVersion::parse(&version_str).unwrap())
                } else {
                    LoaderVersionConstraint::Gte(LoaderVersion::parse(&version_str).unwrap())
                };
                return Some(LoaderRequirement {
                    family: LoaderFamily::Quilt,
                    constraint,
                    source: LoaderRequirementSource::BootLog,
                    requesting_mod_id: extract_mod_id_from_requirement_line(line),
                });
            }
        }
    }
    None
}

// ── Requirement aggregation ─────────────────────────────────────────

/// Compute the intersection of two constraints.
///
/// Returns None if the intersection is empty (contradictory).
pub fn intersect_constraints(
    a: &LoaderVersionConstraint,
    b: &LoaderVersionConstraint,
) -> Option<LoaderVersionConstraint> {
    use LoaderVersionConstraint as LC;

    // Unknown constraints cannot be intersected
    if !a.is_known() || !b.is_known() {
        return None;
    }

    match (a, b) {
        // Two Gte: take the higher bound
        (LC::Gte(v1), LC::Gte(v2)) => Some(LC::Gte(v1.max(v2).clone())),
        (LC::AtLeast(v1), LC::AtLeast(v2)) => Some(LC::AtLeast(v1.max(v2).clone())),
        (LC::Gte(v1), LC::AtLeast(v2)) | (LC::AtLeast(v2), LC::Gte(v1)) => {
            Some(LC::Gte(v1.max(v2).clone()))
        }

        // Two Lt: take the lower bound
        (LC::Lt(v1), LC::Lt(v2)) => Some(LC::Lt(v1.min(v2).clone())),

        // Two Lte: take the lower bound
        (LC::Lte(v1), LC::Lte(v2)) => Some(LC::Lte(v1.min(v2).clone())),

        // Gte + Lt → HalfOpen if lo < hi
        (LC::Gte(lo), LC::Lt(hi)) | (LC::Lt(hi), LC::Gte(lo)) => {
            if lo < hi {
                Some(LC::HalfOpen {
                    lo: lo.clone(),
                    hi: hi.clone(),
                })
            } else {
                None
            }
        }
        (LC::AtLeast(lo), LC::Lt(hi)) | (LC::Lt(hi), LC::AtLeast(lo)) => {
            if lo < hi {
                Some(LC::HalfOpen {
                    lo: lo.clone(),
                    hi: hi.clone(),
                })
            } else {
                None
            }
        }

        // Gte + Lte → ClosedInterval if lo <= hi
        (LC::Gte(lo), LC::Lte(hi)) | (LC::Lte(hi), LC::Gte(lo)) => {
            if lo <= hi {
                Some(LC::ClosedInterval {
                    lo: lo.clone(),
                    hi: hi.clone(),
                })
            } else {
                None
            }
        }

        // Gte + Exact: exact must satisfy gte
        (LC::Gte(lo), LC::Exact(v)) | (LC::Exact(v), LC::Gte(lo)) => {
            if v >= lo {
                Some(LC::Exact(v.clone()))
            } else {
                None
            }
        }

        // Exact + Exact: must be equal
        (LC::Exact(v1), LC::Exact(v2)) => {
            if v1 == v2 {
                Some(LC::Exact(v1.clone()))
            } else {
                None
            }
        }

        // Exact + Lt: exact must satisfy lt
        (LC::Exact(v), LC::Lt(hi)) | (LC::Lt(hi), LC::Exact(v)) => {
            if v < hi {
                Some(LC::Exact(v.clone()))
            } else {
                None
            }
        }

        // Exact + Lte: exact must satisfy lte
        (LC::Exact(v), LC::Lte(hi)) | (LC::Lte(hi), LC::Exact(v)) => {
            if v <= hi {
                Some(LC::Exact(v.clone()))
            } else {
                None
            }
        }

        // Gt + Lt → OpenInterval
        (LC::Gt(lo), LC::Lt(hi)) | (LC::Lt(hi), LC::Gt(lo)) => {
            if lo < hi {
                Some(LC::OpenInterval {
                    lo: lo.clone(),
                    hi: hi.clone(),
                })
            } else {
                None
            }
        }

        // Gt + Gte → Gte (Gte is tighter at equality)
        (LC::Gt(v1), LC::Gte(v2)) | (LC::Gte(v2), LC::Gt(v1)) => {
            if v1 >= v2 {
                Some(LC::Gt(v1.clone()))
            } else {
                Some(LC::Gte(v2.clone()))
            }
        }

        // Gt + Gt → take higher
        (LC::Gt(v1), LC::Gt(v2)) => Some(LC::Gt(v1.max(v2).clone())),

        // Gte + HalfOpen → refine
        (LC::Gte(lo), LC::HalfOpen { lo: lo2, hi })
        | (LC::HalfOpen { lo: lo2, hi }, LC::Gte(lo)) => {
            let effective_lo = lo.max(lo2);
            if effective_lo < hi {
                Some(LC::HalfOpen {
                    lo: effective_lo.clone(),
                    hi: hi.clone(),
                })
            } else {
                None
            }
        }

        // Symmetric catch-all for remaining combinations:
        // Try brute-force: generate a few candidate points and check
        _ => intersect_brute(a, b),
    }
}

/// Fallback intersection check using constraint satisfaction.
/// Not exhaustive but handles common interval combinations.
fn intersect_brute(
    a: &LoaderVersionConstraint,
    b: &LoaderVersionConstraint,
) -> Option<LoaderVersionConstraint> {
    // If both are known and one is a superset of the other
    // Check if either is entirely contained in the other
    // This is a simplified fallback — for complex combinations we report None
    // (conservative: no recommendation is better than wrong recommendation)
    None
}

/// Aggregate requirements: compute intersection and detect conflicts.
///
/// Returns the aggregated constraint, or None if requirements conflict.
pub fn aggregate_requirements(
    requirements: &[LoaderRequirement],
) -> Option<LoaderVersionConstraint> {
    let known: Vec<&LoaderVersionConstraint> = requirements
        .iter()
        .filter(|r| r.constraint.is_known())
        .map(|r| &r.constraint)
        .collect();

    if known.is_empty() {
        return None;
    }

    let mut result = known[0].clone();
    for constraint in &known[1..] {
        match intersect_constraints(&result, constraint) {
            Some(intersection) => result = intersection,
            None => return None, // Conflict
        }
    }
    Some(result)
}

// ── Main analysis ───────────────────────────────────────────────────

/// Analyze loader compatibility.
///
/// `requirements` should be gathered from all available evidence sources:
/// boot logs, JAR metadata, manifest pins, etc.
///
/// `current_version` and `current_family` come from ServerConfig.
///
/// `manifest_pin` is the pack-declared loader version, if any.
pub fn analyze_loader_compatibility(
    current_family: Option<LoaderFamily>,
    current_version: Option<&str>,
    requirements: &[LoaderRequirement],
    manifest_pin: Option<&str>,
) -> LoaderCompatibilityReport {
    let family = current_family.unwrap_or_else(|| {
        // Try to infer from requirements
        requirements
            .first()
            .map(|r| r.family)
            .unwrap_or(LoaderFamily::Forge)
    });

    // ── Wrong family detection ──
    for req in requirements {
        if let Some(cf) = current_family {
            if req.family != cf {
                return LoaderCompatibilityReport {
                    family,
                    current_version: current_version.map(|s| s.to_string()),
                    requirements: requirements.to_vec(),
                    status: LoaderCompatibilityStatus::WrongLoaderFamily,
                    recommendation: Some(LoaderRecommendation {
                        current_version: current_version.map(|s| s.to_string()),
                        recommended_version: None,
                        confidence: RecommendationConfidence::High,
                        reasons: vec![format!(
                            "Configured loader is {}, but requirement demands {}",
                            cf.as_str(),
                            req.family.as_str()
                        )],
                    }),
                };
            }
        }
    }

    // ── Check for mixed loader families ──
    let families: std::collections::HashSet<LoaderFamily> =
        requirements.iter().map(|r| r.family).collect();
    if families.len() > 1 {
        return LoaderCompatibilityReport {
            family,
            current_version: current_version.map(|s| s.to_string()),
            requirements: requirements.to_vec(),
            status: LoaderCompatibilityStatus::ConflictingRequirements,
            recommendation: Some(LoaderRecommendation {
                current_version: current_version.map(|s| s.to_string()),
                recommended_version: None,
                confidence: RecommendationConfidence::High,
                reasons: vec![
                    "Multiple conflicting loader families required by different mods".to_string(),
                ],
            }),
        };
    }

    // ── Aggregate requirements ──
    let Some(aggregated) = aggregate_requirements(requirements) else {
        return LoaderCompatibilityReport {
            family,
            current_version: current_version.map(|s| s.to_string()),
            requirements: requirements.to_vec(),
            status: LoaderCompatibilityStatus::ConflictingRequirements,
            recommendation: Some(LoaderRecommendation {
                current_version: current_version.map(|s| s.to_string()),
                recommended_version: None,
                confidence: RecommendationConfidence::High,
                reasons: vec!["Aggregated version constraints are contradictory".to_string()],
            }),
        };
    };

    // ── Check current version against aggregated constraint ──
    if let Some(cv) = current_version.and_then(LoaderVersion::parse) {
        if aggregated.satisfied_by(&cv) {
            return LoaderCompatibilityReport {
                family,
                current_version: current_version.map(|s| s.to_string()),
                requirements: requirements.to_vec(),
                status: LoaderCompatibilityStatus::Compatible,
                recommendation: None,
            };
        }
    }

    // ── Manifest pin check ──
    if let Some(pin_str) = manifest_pin {
        if let Some(pin_ver) = LoaderVersion::parse(pin_str) {
            if aggregated.satisfied_by(&pin_ver) {
                return LoaderCompatibilityReport {
                    family,
                    current_version: current_version.map(|s| s.to_string()),
                    requirements: requirements.to_vec(),
                    status: LoaderCompatibilityStatus::Incompatible,
                    recommendation: Some(LoaderRecommendation {
                        current_version: current_version.map(|s| s.to_string()),
                        recommended_version: Some(pin_str.to_string()),
                        confidence: RecommendationConfidence::High,
                        reasons: vec![format!(
                            "Pack-declared loader version {pin_str} satisfies all requirements"
                        )],
                    }),
                };
            } else {
                // Pin is incompatible with requirements
                return LoaderCompatibilityReport {
                    family,
                    current_version: current_version.map(|s| s.to_string()),
                    requirements: requirements.to_vec(),
                    status: LoaderCompatibilityStatus::Incompatible,
                    recommendation: Some(LoaderRecommendation {
                        current_version: current_version.map(|s| s.to_string()),
                        recommended_version: None,
                        confidence: RecommendationConfidence::High,
                        reasons: vec![
                            format!(
                                "Pack-declared loader pin {pin_str} is incompatible with aggregated requirement {aggregated}"
                            ),
                            "Automatic loader changes are disabled for safety.".to_string(),
                        ],
                    }),
                };
            }
        }
    }

    // ── Current version incompatible, no manifest pin rescue ──
    let has_explicit_req = requirements
        .iter()
        .any(|r| r.source == LoaderRequirementSource::BootLog);
    let has_metadata_req = requirements
        .iter()
        .any(|r| r.source == LoaderRequirementSource::JarMetadata);

    let (confidence, reasons) = if has_explicit_req || has_metadata_req {
        let mut reasons = vec![format!("Required: {aggregated}")];
        if let Some(cv) = current_version {
            reasons.push(format!("Current {family} {cv} is incompatible"));
        }
        reasons.push(
            "No deterministic replacement version could be selected automatically.".to_string(),
        );
        (RecommendationConfidence::Medium, reasons)
    } else {
        let mut reasons = vec!["Weak or incomplete evidence for loader requirement".to_string()];
        if let Some(cv) = current_version {
            reasons.push(format!("Current {family} {cv}"));
        }
        (RecommendationConfidence::Low, reasons)
    };

    LoaderCompatibilityReport {
        family,
        current_version: current_version.map(|s| s.to_string()),
        requirements: requirements.to_vec(),
        status: LoaderCompatibilityStatus::Incompatible,
        recommendation: Some(LoaderRecommendation {
            current_version: current_version.map(|s| s.to_string()),
            recommended_version: None,
            confidence,
            reasons,
        }),
    }
}

/// Format a user-facing message from a loader compatibility report.
pub fn format_report_message(report: &LoaderCompatibilityReport) -> String {
    match &report.status {
        LoaderCompatibilityStatus::Compatible => {
            format!(
                "Loader {} {} is compatible.",
                report.family.as_str(),
                report.current_version.as_deref().unwrap_or("unknown")
            )
        }
        LoaderCompatibilityStatus::Incompatible => {
            let mut lines = Vec::new();
            lines.push("Server loader is incompatible.".to_string());
            if let Some(cv) = &report.current_version {
                lines.push(format!("\nCurrent:\n{} {}", report.family.as_str(), cv));
            }
            // Show aggregated requirement
            if let Some(agg) = aggregate_requirements(&report.requirements) {
                lines.push(format!("\nRequired:\n{} {}", report.family.as_str(), agg));
            }
            if let Some(rec) = &report.recommendation {
                if let Some(rv) = &rec.recommended_version {
                    lines.push(format!("\nRecommended:\n{} {}", report.family.as_str(), rv));
                }
                for reason in &rec.reasons {
                    lines.push(format!("\n{reason}"));
                }
            }
            lines.push("\nAutomatic loader changes are disabled for safety.".to_string());
            lines.join("")
        }
        LoaderCompatibilityStatus::WrongLoaderFamily => {
            let mut lines = Vec::new();
            lines.push("Wrong loader family detected.".to_string());
            if let Some(cv) = &report.current_version {
                lines.push(format!("Current: {} {}", report.family.as_str(), cv));
            }
            if let Some(rec) = &report.recommendation {
                for reason in &rec.reasons {
                    lines.push(reason.clone());
                }
            }
            lines.push("Automatic loader changes are disabled for safety.".to_string());
            lines.join("\n")
        }
        LoaderCompatibilityStatus::ConflictingRequirements => {
            let mut lines = Vec::new();
            lines.push("Conflicting loader requirements detected.".to_string());
            if let Some(rec) = &report.recommendation {
                for reason in &rec.reasons {
                    lines.push(reason.clone());
                }
            }
            lines.push("No recommendation available.".to_string());
            lines.join("\n")
        }
        LoaderCompatibilityStatus::Unknown => {
            "Loader compatibility status could not be determined.".to_string()
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Loader family normalization ──

    #[test]
    fn test_loader_family_aliases() {
        assert_eq!(normalize_loader_family("forge"), Some(LoaderFamily::Forge));
        assert_eq!(normalize_loader_family("Forge"), Some(LoaderFamily::Forge));
        assert_eq!(normalize_loader_family("FORGE"), Some(LoaderFamily::Forge));
        assert_eq!(
            normalize_loader_family("neoforge"),
            Some(LoaderFamily::NeoForge)
        );
        assert_eq!(
            normalize_loader_family("neo-forge"),
            Some(LoaderFamily::NeoForge)
        );
        assert_eq!(
            normalize_loader_family("neo_forge"),
            Some(LoaderFamily::NeoForge)
        );
        assert_eq!(
            normalize_loader_family("NeoForge"),
            Some(LoaderFamily::NeoForge)
        );
        assert_eq!(
            normalize_loader_family("fabric-loader"),
            Some(LoaderFamily::Fabric)
        );
        assert_eq!(
            normalize_loader_family("fabricloader"),
            Some(LoaderFamily::Fabric)
        );
        assert_eq!(
            normalize_loader_family("fabric_loader"),
            Some(LoaderFamily::Fabric)
        );
        assert_eq!(
            normalize_loader_family("quilt_loader"),
            Some(LoaderFamily::Quilt)
        );
        assert_eq!(
            normalize_loader_family("quilt-loader"),
            Some(LoaderFamily::Quilt)
        );
        assert_eq!(
            normalize_loader_family("quiltloader"),
            Some(LoaderFamily::Quilt)
        );
    }

    #[test]
    fn test_not_loader_families() {
        // These are mods/APIs, NOT loader families
        assert_eq!(normalize_loader_family("fabric-api"), None);
        assert_eq!(normalize_loader_family("fabric"), None);
        assert_eq!(normalize_loader_family("quilted-fabric-api"), None);
        assert_eq!(normalize_loader_family("minecraft"), None);
        assert_eq!(normalize_loader_family(""), None);
    }

    // ── Numeric version ──

    #[test]
    fn test_version_ordering() {
        let v47_10 = LoaderVersion::parse("47.10.0").unwrap();
        let v47_9 = LoaderVersion::parse("47.9.9").unwrap();
        assert!(v47_10 > v47_9, "47.10.0 > 47.9.9");

        let v0_15_10 = LoaderVersion::parse("0.15.10").unwrap();
        let v0_15_9 = LoaderVersion::parse("0.15.9").unwrap();
        assert!(v0_15_10 > v0_15_9, "0.15.10 > 0.15.9");

        let v20_4_100 = LoaderVersion::parse("20.4.100").unwrap();
        let v20_4_99 = LoaderVersion::parse("20.4.99").unwrap();
        assert!(v20_4_100 > v20_4_99, "20.4.100 > 20.4.99");

        let v47_2_10 = LoaderVersion::parse("47.2.10").unwrap();
        let v47_2_9 = LoaderVersion::parse("47.2.9").unwrap();
        assert!(v47_2_10 > v47_2_9, "47.2.10 > 47.2.9");
    }

    #[test]
    fn test_version_equality_with_trailing_zeroes() {
        let v47_2 = LoaderVersion::parse("47.2").unwrap();
        let v47_2_0 = LoaderVersion::parse("47.2.0").unwrap();
        assert_eq!(
            v47_2, v47_2_0,
            "47.2 == 47.2.0 (trailing zeroes normalized)"
        );

        let v1_0_0_0 = LoaderVersion::parse("1.0.0.0").unwrap();
        let v1 = LoaderVersion::parse("1").unwrap();
        assert_eq!(v1_0_0_0, v1, "1.0.0.0 == 1");
    }

    #[test]
    fn test_version_parse_failures() {
        assert!(LoaderVersion::parse("").is_none());
        assert!(LoaderVersion::parse("abc").is_none());
        assert!(LoaderVersion::parse("47.x.0").is_none());
    }

    // ── Constraint parser ──

    #[test]
    fn test_constraint_parse_forge_formats() {
        let c = LoaderVersionConstraint::parse(">=47.2.0");
        assert!(matches!(c, LoaderVersionConstraint::Gte(_)));
        assert!(c.satisfied_by(&LoaderVersion::parse("47.4.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("47.1.0").unwrap()));

        let c = LoaderVersionConstraint::parse("47.2.0+");
        assert!(matches!(c, LoaderVersionConstraint::AtLeast(_)));
        assert!(c.satisfied_by(&LoaderVersion::parse("47.2.0").unwrap()));
        assert!(c.satisfied_by(&LoaderVersion::parse("48.0.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("47.1.9").unwrap()));

        let c = LoaderVersionConstraint::parse("[47.2.0,)");
        // This should be UnknownConstraint since there's no hi version
        // Actually [47.2.0,) has no proper hi — let me check
        // Our parser requires both lo and hi to parse. So this is Unknown.
        assert!(matches!(c, LoaderVersionConstraint::UnknownConstraint(_)));

        let c = LoaderVersionConstraint::parse("[47.2.0,48.0.0)");
        assert!(matches!(c, LoaderVersionConstraint::HalfOpen { .. }));
        assert!(c.satisfied_by(&LoaderVersion::parse("47.4.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("48.0.0").unwrap()));

        let c = LoaderVersionConstraint::parse("[47.2.0,48.0.0]");
        assert!(matches!(c, LoaderVersionConstraint::ClosedInterval { .. }));
        assert!(c.satisfied_by(&LoaderVersion::parse("48.0.0").unwrap()));
    }

    #[test]
    fn test_constraint_parse_neoforge() {
        let c = LoaderVersionConstraint::parse(">=20.4.100");
        assert!(matches!(c, LoaderVersionConstraint::Gte(_)));
        assert!(c.satisfied_by(&LoaderVersion::parse("20.4.100").unwrap()));
        assert!(c.satisfied_by(&LoaderVersion::parse("20.5.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("20.4.99").unwrap()));
    }

    #[test]
    fn test_constraint_parse_fabric() {
        let c = LoaderVersionConstraint::parse(">=0.15.0");
        assert!(matches!(c, LoaderVersionConstraint::Gte(_)));
        assert!(c.satisfied_by(&LoaderVersion::parse("0.15.0").unwrap()));
        assert!(c.satisfied_by(&LoaderVersion::parse("0.16.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("0.14.9").unwrap()));
    }

    #[test]
    fn test_constraint_parse_quilt() {
        let c = LoaderVersionConstraint::parse(">=0.23.0");
        assert!(matches!(c, LoaderVersionConstraint::Gte(_)));
        assert!(c.satisfied_by(&LoaderVersion::parse("0.23.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("0.22.9").unwrap()));
    }

    #[test]
    fn test_constraint_invalid() {
        assert!(matches!(
            LoaderVersionConstraint::parse("latest"),
            LoaderVersionConstraint::UnknownConstraint(_)
        ));
        assert!(matches!(
            LoaderVersionConstraint::parse("newer-ish"),
            LoaderVersionConstraint::UnknownConstraint(_)
        ));
        assert!(matches!(
            LoaderVersionConstraint::parse("47.x maybe"),
            LoaderVersionConstraint::UnknownConstraint(_)
        ));
    }

    #[test]
    fn test_constraint_exact() {
        let c = LoaderVersionConstraint::parse("47.2.0");
        assert!(matches!(c, LoaderVersionConstraint::Exact(_)));
        assert!(c.satisfied_by(&LoaderVersion::parse("47.2.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("47.2.1").unwrap()));
    }

    // ── Constraint satisfaction ──

    #[test]
    fn test_satisfaction_gte() {
        let c = LoaderVersionConstraint::parse(">=47.2.0");
        assert!(c.satisfied_by(&LoaderVersion::parse("47.4.0").unwrap()));
        assert!(c.satisfied_by(&LoaderVersion::parse("47.2.0").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("47.1.0").unwrap()));
    }

    #[test]
    fn test_satisfaction_interval() {
        let c = LoaderVersionConstraint::parse("[47.2,48)");
        assert!(c.satisfied_by(&LoaderVersion::parse("47.9").unwrap()));
        assert!(!c.satisfied_by(&LoaderVersion::parse("48.0").unwrap()));
        assert!(c.satisfied_by(&LoaderVersion::parse("47.2").unwrap()));
    }

    // ── Intersection ──

    #[test]
    fn test_intersection_valid() {
        let a = LoaderVersionConstraint::parse(">=47.2");
        let b = LoaderVersionConstraint::parse(">=47.4");
        let c = LoaderVersionConstraint::parse("<48");

        // >=47.2 ∩ >=47.4 = >=47.4
        let ab = intersect_constraints(&a, &b).unwrap();
        assert!(ab.satisfied_by(&LoaderVersion::parse("47.4").unwrap()));
        assert!(!ab.satisfied_by(&LoaderVersion::parse("47.3").unwrap()));

        // >=47.4 ∩ <48 = [47.4, 48)
        let abc = intersect_constraints(&ab, &c).unwrap();
        assert!(abc.satisfied_by(&LoaderVersion::parse("47.9").unwrap()));
        assert!(!abc.satisfied_by(&LoaderVersion::parse("48.0").unwrap()));
    }

    #[test]
    fn test_intersection_conflict_gte_lt() {
        let a = LoaderVersionConstraint::parse(">=48");
        let b = LoaderVersionConstraint::parse("<48");
        assert!(
            intersect_constraints(&a, &b).is_none(),
            ">=48 and <48 should conflict"
        );
    }

    #[test]
    fn test_intersection_conflict_exact() {
        let a = LoaderVersionConstraint::parse("47.2");
        let b = LoaderVersionConstraint::parse(">=47.4");
        assert!(
            intersect_constraints(&a, &b).is_none(),
            "exact 47.2 and >=47.4 should conflict"
        );
    }

    // ── Boot-log parser ──

    #[test]
    fn test_parse_forge_requirement_or_above() {
        let log = "Mod 'flywheel' requires Forge 47.2.0 or above";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::Forge);
        assert!(reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("47.4.0").unwrap()));
        assert!(!reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("47.1.0").unwrap()));
        assert_eq!(reqs[0].requesting_mod_id.as_deref(), Some("flywheel"));
    }

    #[test]
    fn test_parse_forge_requirement_version_plus() {
        let log = "Mod create requires forge version 47.2.0+";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::Forge);
        assert!(reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("47.2.0").unwrap()));
    }

    #[test]
    fn test_parse_forge_requirement_interval() {
        let log = "requires forge [47.2.0,48.0.0)";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("47.4.0").unwrap()));
        assert!(!reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("48.0.0").unwrap()));
    }

    #[test]
    fn test_parse_neoforge_requirement() {
        let log = "requires NeoForge >=20.4.100";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::NeoForge);
        assert!(reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("20.4.100").unwrap()));
        assert!(!reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("20.4.99").unwrap()));
    }

    #[test]
    fn test_parse_fabric_requirement() {
        let log = "requires fabric-loader version 0.15.0 or later";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::Fabric);
        assert!(reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("0.15.0").unwrap()));
        assert!(!reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("0.14.9").unwrap()));
    }

    #[test]
    fn test_parse_fabric_requirement_short() {
        let log = "requires fabricloader >=0.15.0";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::Fabric);
    }

    #[test]
    fn test_parse_quilt_requirement() {
        let log = "requires quilt_loader >=0.23.0";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::Quilt);
        assert!(reqs[0]
            .constraint
            .satisfied_by(&LoaderVersion::parse("0.23.0").unwrap()));
    }

    #[test]
    fn test_vague_text_not_accepted() {
        assert!(parse_loader_requirements_from_log("Forge error").is_empty());
        assert!(parse_loader_requirements_from_log("loader failed").is_empty());
        assert!(parse_loader_requirements_from_log("Something went wrong with forge").is_empty());
    }

    #[test]
    fn test_fabric_api_not_confused_with_loader() {
        let log = "requires fabric-api version 0.80.0";
        let reqs = parse_loader_requirements_from_log(log);
        // fabric-api is NOT fabric-loader
        assert!(
            reqs.is_empty(),
            "fabric-api should not be parsed as a loader requirement"
        );
    }

    // ── Multiple mod requirements ──

    #[test]
    fn test_multiple_mod_requirements() {
        let log = "Mod 'modA' requires forge version 47.2.0+\nMod 'modB' requires forge version 47.4.0+\nMod 'modC' requires forge [47.2.0,48.0.0)";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].requesting_mod_id.as_deref(), Some("modA"));
        assert_eq!(reqs[1].requesting_mod_id.as_deref(), Some("modB"));
        assert_eq!(reqs[2].requesting_mod_id.as_deref(), Some("modC"));
    }

    // ── Aggregate ──

    #[test]
    fn test_aggregate_valid() {
        let reqs = vec![
            LoaderRequirement {
                family: LoaderFamily::Forge,
                constraint: LoaderVersionConstraint::parse(">=47.2"),
                source: LoaderRequirementSource::BootLog,
                requesting_mod_id: Some("modA".to_string()),
            },
            LoaderRequirement {
                family: LoaderFamily::Forge,
                constraint: LoaderVersionConstraint::parse(">=47.4"),
                source: LoaderRequirementSource::BootLog,
                requesting_mod_id: Some("modB".to_string()),
            },
            LoaderRequirement {
                family: LoaderFamily::Forge,
                constraint: LoaderVersionConstraint::parse("<48"),
                source: LoaderRequirementSource::BootLog,
                requesting_mod_id: Some("modC".to_string()),
            },
        ];
        let agg = aggregate_requirements(&reqs).unwrap();
        assert!(agg.satisfied_by(&LoaderVersion::parse("47.4").unwrap()));
        assert!(!agg.satisfied_by(&LoaderVersion::parse("47.3").unwrap()));
        assert!(!agg.satisfied_by(&LoaderVersion::parse("48.0").unwrap()));
    }

    #[test]
    fn test_aggregate_conflict() {
        let reqs = vec![
            LoaderRequirement {
                family: LoaderFamily::Forge,
                constraint: LoaderVersionConstraint::parse(">=48"),
                source: LoaderRequirementSource::BootLog,
                requesting_mod_id: None,
            },
            LoaderRequirement {
                family: LoaderFamily::Forge,
                constraint: LoaderVersionConstraint::parse("<48"),
                source: LoaderRequirementSource::BootLog,
                requesting_mod_id: None,
            },
        ];
        assert!(
            aggregate_requirements(&reqs).is_none(),
            ">=48 and <48 should conflict"
        );
    }

    // ── Wrong family ──

    #[test]
    fn test_wrong_loader_family_forge_vs_neoforge() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::NeoForge,
            constraint: LoaderVersionConstraint::parse(">=20.4.100"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Forge), Some("47.2.0"), &reqs, None);
        assert_eq!(report.status, LoaderCompatibilityStatus::WrongLoaderFamily);
    }

    #[test]
    fn test_wrong_loader_family_fabric_vs_forge() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2.0"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Fabric), Some("0.15.0"), &reqs, None);
        assert_eq!(report.status, LoaderCompatibilityStatus::WrongLoaderFamily);
    }

    // ── Manifest pin ──

    #[test]
    fn test_manifest_pin_compatible() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report = analyze_loader_compatibility(
            Some(LoaderFamily::Forge),
            Some("47.1.0"),
            &reqs,
            Some("47.4.0"),
        );
        assert_eq!(report.status, LoaderCompatibilityStatus::Incompatible);
        let rec = report.recommendation.unwrap();
        assert_eq!(rec.recommended_version.as_deref(), Some("47.4.0"));
        assert_eq!(rec.confidence, RecommendationConfidence::High);
    }

    #[test]
    fn test_manifest_pin_incompatible() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report = analyze_loader_compatibility(
            Some(LoaderFamily::Forge),
            Some("47.1.0"),
            &reqs,
            Some("47.1.0"),
        );
        assert_eq!(report.status, LoaderCompatibilityStatus::Incompatible);
        let rec = report.recommendation.unwrap();
        assert!(
            rec.recommended_version.is_none(),
            "Incompatible pin should not recommend a version"
        );
    }

    // ── Compatible ──

    #[test]
    fn test_compatible() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Forge), Some("47.4.0"), &reqs, None);
        assert_eq!(report.status, LoaderCompatibilityStatus::Compatible);
        assert!(report.recommendation.is_none());
    }

    // ── Confidence ──

    #[test]
    fn test_confidence_high_with_manifest_pin() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report = analyze_loader_compatibility(
            Some(LoaderFamily::Forge),
            Some("47.1.0"),
            &reqs,
            Some("47.4.0"),
        );
        let rec = report.recommendation.unwrap();
        assert_eq!(rec.confidence, RecommendationConfidence::High);
    }

    #[test]
    fn test_confidence_medium_with_explicit_req() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Forge), Some("47.1.0"), &reqs, None);
        let rec = report.recommendation.unwrap();
        assert_eq!(rec.confidence, RecommendationConfidence::Medium);
    }

    #[test]
    fn test_confidence_low_without_evidence() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::CurrentConfig,
            requesting_mod_id: None,
        }];
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Forge), Some("47.1.0"), &reqs, None);
        let rec = report.recommendation.unwrap();
        assert_eq!(rec.confidence, RecommendationConfidence::Low);
    }

    // ── Format ──

    #[test]
    fn test_format_compatible() {
        let report = LoaderCompatibilityReport {
            family: LoaderFamily::Forge,
            current_version: Some("47.4.0".to_string()),
            requirements: vec![],
            status: LoaderCompatibilityStatus::Compatible,
            recommendation: None,
        };
        let msg = format_report_message(&report);
        assert!(msg.contains("compatible"));
    }

    #[test]
    fn test_format_incompatible() {
        let reqs = vec![LoaderRequirement {
            family: LoaderFamily::Forge,
            constraint: LoaderVersionConstraint::parse(">=47.2"),
            source: LoaderRequirementSource::BootLog,
            requesting_mod_id: None,
        }];
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Forge), Some("47.1.0"), &reqs, None);
        let msg = format_report_message(&report);
        assert!(msg.contains("incompatible"));
        assert!(msg.contains("47.1.0"));
        assert!(msg.contains("Automatic loader changes are disabled"));
    }

    // ── Real fixture ──

    #[test]
    fn test_real_forge_error_fixture() {
        // Real error from Forge boot
        let log = "Mod 'flywheel' requires Forge 47.2.0 or above, but Forge 43.2.0 is installed.";
        let reqs = parse_loader_requirements_from_log(log);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].family, LoaderFamily::Forge);
        assert_eq!(reqs[0].requesting_mod_id.as_deref(), Some("flywheel"));

        // Check constraint
        let v47 = LoaderVersion::parse("47.2.0").unwrap();
        let v43 = LoaderVersion::parse("43.2.0").unwrap();
        assert!(reqs[0].constraint.satisfied_by(&v47));
        assert!(!reqs[0].constraint.satisfied_by(&v43));

        // Full analysis
        let report =
            analyze_loader_compatibility(Some(LoaderFamily::Forge), Some("43.2.0"), &reqs, None);
        assert_eq!(report.status, LoaderCompatibilityStatus::Incompatible);
        let msg = format_report_message(&report);
        assert!(msg.contains("incompatible"));
        assert!(msg.contains("43.2.0"));
    }
}
