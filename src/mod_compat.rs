// mod_compat -- Unified mod compatibility classification.
//
// Phase 3A/3B.1: Data model + local deterministic classifier.
// Uses jar_metadata as the single metadata reader.
// No network calls. Inspects loader metadata inside JAR files only.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::jar_metadata::{self, DeclaredEnvironment, EnvironmentFinding, LoaderMetadataKind};

// -- Data model ----------------------------------------------------------

/// What to do with a mod in a dedicated server context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerCompatibility {
    /// Mod works on dedicated servers (may also work on client).
    ServerOk,
    /// Mod is client-only and should be excluded from server build.
    ClientOnly,
    /// Mod explicitly supports both client and server.
    Both,
    /// No deterministic evidence found. Mod is kept.
    Unknown,
}

/// How much to trust the classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilityConfidence {
    /// Deterministic metadata (loader manifest) found.
    Explicit,
    /// No deterministic evidence.
    None,
}

/// Source of the compatibility determination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilitySource {
    FabricMetadata,
    QuiltMetadata,
    ForgeMetadata,
    NeoForgeMetadata,
    ConflictingMetadata {
        sources: Vec<String>,
        details: String,
    },
    None,
}

/// Classification result for a single mod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModCompatibility {
    pub compatibility: ServerCompatibility,
    pub confidence: CompatibilityConfidence,
    pub source: CompatibilitySource,
    pub reason: String,
}

// -- Public API ----------------------------------------------------------

/// Classify a single mod JAR by inspecting its loader metadata.
///
/// Metadata precedence (first match wins):
///   1. Fabric `environment` in fabric.mod.json
///   2. Quilt `environment` in quilt.mod.json
///   3. Forge `clientSideOnly` in META-INF/mods.toml
///   4. NeoForge `clientSideOnly` in META-INF/neoforge.mods.toml
///
/// If multiple sources provide conflicting information, the result is
/// Unknown with a ConflictingMetadata source.
///
/// Filename-based heuristics are intentionally excluded.
/// Absence of metadata produces `Unknown / None`.
pub fn classify_mod_local(path: &Path) -> ModCompatibility {
    let metadata = jar_metadata::read_jar_mod_metadata(path);

    if metadata.environment_findings.is_empty() {
        return ModCompatibility {
            compatibility: ServerCompatibility::Unknown,
            confidence: CompatibilityConfidence::None,
            source: CompatibilitySource::None,
            reason: "No side metadata found in JAR".to_string(),
        };
    }

    // Convert each environment finding to a (Source, Compat, Reason) tuple.
    let findings: Vec<(CompatibilitySource, ServerCompatibility, String)> = metadata
        .environment_findings
        .iter()
        .map(|f| environment_finding_to_compat(f))
        .collect();

    resolve_multiple_findings(findings)
}

// -- Environment interpretation ------------------------------------------

/// Convert a single EnvironmentFinding to a compatibility classification.
fn environment_finding_to_compat(
    finding: &EnvironmentFinding,
) -> (CompatibilitySource, ServerCompatibility, String) {
    let source = match finding.source {
        LoaderMetadataKind::Fabric => CompatibilitySource::FabricMetadata,
        LoaderMetadataKind::Quilt => CompatibilitySource::QuiltMetadata,
        LoaderMetadataKind::Forge => CompatibilitySource::ForgeMetadata,
        LoaderMetadataKind::NeoForge => CompatibilitySource::NeoForgeMetadata,
    };
    let (compat, env_desc) = match finding.environment {
        DeclaredEnvironment::Client => (
            ServerCompatibility::ClientOnly,
            format!("{:?} Client", finding.source),
        ),
        DeclaredEnvironment::Server => (
            ServerCompatibility::ServerOk,
            format!("{:?} Server", finding.source),
        ),
        DeclaredEnvironment::Universal => (
            ServerCompatibility::Both,
            format!("{:?} Universal", finding.source),
        ),
    };
    (source, compat, format!("{}: {}", env_desc, finding.detail))
}

// -- Conflict resolution -------------------------------------------------

/// When multiple metadata files provide side information, resolve
/// conflicts by preferring the safe (non-destructive) result.
///
/// Rules:
/// - If all agree -> use that result.
/// - If sources disagree -> Unknown (not Both).
fn resolve_multiple_findings(
    findings: Vec<(CompatibilitySource, ServerCompatibility, String)>,
) -> ModCompatibility {
    // Check if all findings agree.
    let first_compat = findings[0].1;
    if findings.iter().all(|(_, c, _)| *c == first_compat) {
        let sources: Vec<String> = findings
            .iter()
            .map(|(s, _, r)| format!("{s:?}: {r}"))
            .collect();
        return ModCompatibility {
            compatibility: first_compat,
            confidence: CompatibilityConfidence::Explicit,
            source: findings.into_iter().next().unwrap().0,
            reason: sources.join("; "),
        };
    }

    // Findings disagree. Prefer the safe result.
    // Conflicting deterministic metadata -> Unknown.
    // "Both" is a positive compatibility assertion -- we cannot assert it
    // when sources disagree. "Unknown" is correct because we cannot safely
    // determine the actual runtime side. The mod is still KEPT (Unknown is
    // never auto-excluded), so this is non-destructive.
    let source_descriptions: Vec<String> = findings
        .iter()
        .map(|(s, c, r)| format!("{s:?}={c:?} ({r})"))
        .collect();

    ModCompatibility {
        compatibility: ServerCompatibility::Unknown,
        confidence: CompatibilityConfidence::Explicit,
        source: CompatibilitySource::ConflictingMetadata {
            sources: findings.iter().map(|(s, _, _)| format!("{s:?}")).collect(),
            details: source_descriptions.join(" vs "),
        },
        reason: format!(
            "Multiple metadata sources disagree: {}",
            source_descriptions.join(" vs ")
        ),
    }
}

// -- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: create a temporary JAR file containing the given
    /// (path_inside_jar, content) pairs.
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

    // -- Fabric tests ----------------------------------------------------

    #[test]
    fn fabric_client_environment_is_client_only() {
        let jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"client\"}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::FabricMetadata);
    }

    #[test]
    fn fabric_server_environment_is_server_ok() {
        let jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"server\"}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ServerOk);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::FabricMetadata);
    }

    #[test]
    fn fabric_universal_environment_is_both() {
        let jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"*\"}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Both);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::FabricMetadata);
    }

    #[test]
    fn fabric_no_environment_is_unknown() {
        let jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"test\",\"name\":\"Test Mod\"}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
        assert_eq!(result.confidence, CompatibilityConfidence::None);
    }

    // -- Quilt tests -----------------------------------------------------

    #[test]
    fn quilt_client_environment_is_client_only() {
        let jar = make_test_jar(&[(
            "quilt.mod.json",
            b"{\"schema_version\":1,\"quilt_loader\":{\"metadata\":{\"environment\":\"client\"}}}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::QuiltMetadata);
    }

    #[test]
    fn quilt_server_environment_is_server_ok() {
        let jar = make_test_jar(&[(
            "quilt.mod.json",
            b"{\"schema_version\":1,\"quilt_loader\":{\"metadata\":{\"environment\":\"server\"}}}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ServerOk);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::QuiltMetadata);
    }

    #[test]
    fn quilt_universal_environment_is_both() {
        let jar = make_test_jar(&[(
            "quilt.mod.json",
            b"{\"schema_version\":1,\"quilt_loader\":{\"metadata\":{\"environment\":\"*\"}}}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Both);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::QuiltMetadata);
    }

    #[test]
    fn quilt_alternate_environment_path() {
        // Quilt also supports quilt_loader/environment (without metadata/)
        let jar = make_test_jar(&[(
            "quilt.mod.json",
            b"{\"schema_version\":1,\"quilt_loader\":{\"environment\":\"client\"}}",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(result.source, CompatibilitySource::QuiltMetadata);
    }

    // -- Forge tests -----------------------------------------------------

    #[test]
    fn forge_client_side_only_true() {
        let jar = make_test_jar(&[(
            "META-INF/mods.toml",
            b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=true\n",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::ForgeMetadata);
    }

    #[test]
    fn forge_client_side_only_false_is_both() {
        let jar = make_test_jar(&[(
            "META-INF/mods.toml",
            b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=false\n",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Both);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert_eq!(result.source, CompatibilitySource::ForgeMetadata);
    }

    #[test]
    fn forge_no_client_side_only_field_is_unknown() {
        let jar = make_test_jar(&[(
            "META-INF/mods.toml",
            b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
        assert_eq!(result.confidence, CompatibilityConfidence::None);
    }

    #[test]
    fn neoforge_client_side_only_true() {
        let jar = make_test_jar(&[(
            "META-INF/neoforge.mods.toml",
            b"modLoader=\"javafml\"\nloaderVersion=\"[4,)\"\nclientSideOnly=true\n",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(result.source, CompatibilitySource::NeoForgeMetadata);
    }

    #[test]
    fn neoforge_no_client_side_only_is_unknown() {
        let jar = make_test_jar(&[(
            "META-INF/neoforge.mods.toml",
            b"modLoader=\"javafml\"\nloaderVersion=\"[4,)\"\n",
        )]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    // -- No metadata -----------------------------------------------------

    #[test]
    fn no_metadata_is_unknown() {
        let jar = make_test_jar(&[("dummy.txt", b"hello")]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
        assert_eq!(result.confidence, CompatibilityConfidence::None);
        assert_eq!(result.source, CompatibilitySource::None);
    }

    #[test]
    fn corrupted_jar_is_unknown() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        std::fs::write(tmp.path(), b"this is not a valid zip file").expect("write");
        let result = classify_mod_local(tmp.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
        assert_eq!(result.confidence, CompatibilityConfidence::None);
    }

    #[test]
    fn nonexistent_path_is_unknown() {
        let result = classify_mod_local(Path::new("/nonexistent/path/mod.jar"));
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    // -- Filename heuristics have ZERO effect ----------------------------

    #[test]
    fn filename_client_without_metadata_is_unknown() {
        let jar = make_test_jar(&[("dummy.txt", b"hello")]);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("client-only-mod-1.0.jar");
        std::fs::copy(jar.path(), &path).expect("copy");
        let result = classify_mod_local(&path);
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    #[test]
    fn filename_shader_without_metadata_is_unknown() {
        let jar = make_test_jar(&[("dummy.txt", b"hello")]);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shader-mod-2.0.jar");
        std::fs::copy(jar.path(), &path).expect("copy");
        let result = classify_mod_local(&path);
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    #[test]
    fn filename_gui_without_metadata_is_unknown() {
        let jar = make_test_jar(&[("dummy.txt", b"hello")]);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gui-helper-mod.jar");
        std::fs::copy(jar.path(), &path).expect("copy");
        let result = classify_mod_local(&path);
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    #[test]
    fn filename_render_without_metadata_is_unknown() {
        let jar = make_test_jar(&[("dummy.txt", b"hello")]);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("render-engine-mod.jar");
        std::fs::copy(jar.path(), &path).expect("copy");
        let result = classify_mod_local(&path);
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    // -- Conflicting metadata --------------------------------------------

    #[test]
    fn conflicting_fabric_client_and_forge_both_returns_unknown() {
        let jar = make_test_jar(&[
            (
                "fabric.mod.json",
                b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"client\"}",
            ),
            (
                "META-INF/mods.toml",
                b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=false\n",
            ),
        ]);
        let result = classify_mod_local(jar.path());
        // Conflicting deterministic metadata -> Unknown (not Both).
        // "Both" is a positive assertion; we cannot make it when sources disagree.
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        assert!(matches!(
            result.source,
            CompatibilitySource::ConflictingMetadata { .. }
        ));
    }

    #[test]
    fn conflicting_fabric_server_and_forge_client_only_returns_unknown() {
        let jar = make_test_jar(&[
            (
                "fabric.mod.json",
                b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"server\"}",
            ),
            (
                "META-INF/mods.toml",
                b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=true\n",
            ),
        ]);
        let result = classify_mod_local(jar.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
        assert!(matches!(
            result.source,
            CompatibilitySource::ConflictingMetadata { .. }
        ));
    }

    #[test]
    fn consistent_fabric_and_forge_both_sides() {
        let jar = make_test_jar(&[
            (
                "fabric.mod.json",
                b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"*\"}",
            ),
            (
                "META-INF/mods.toml",
                b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=false\n",
            ),
        ]);
        let result = classify_mod_local(jar.path());
        // Both sources agree: mod works on both sides.
        assert_eq!(result.compatibility, ServerCompatibility::Both);
        assert_eq!(result.confidence, CompatibilityConfidence::Explicit);
        // Source is the first one checked (FabricMetadata).
        assert_eq!(result.source, CompatibilitySource::FabricMetadata);
    }

    // -- Fabric-only (no environment) falls through to Forge -------------

    #[test]
    fn fabric_no_environment_falls_through_to_forge() {
        let jar = make_test_jar(&[
            (
                "fabric.mod.json",
                b"{\"schemaVersion\":1,\"id\":\"test\",\"name\":\"Test\"}",
            ),
            (
                "META-INF/mods.toml",
                b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\nclientSideOnly=true\n",
            ),
        ]);
        let result = classify_mod_local(jar.path());
        // fabric.mod.json has no environment -> falls through.
        // Forge has clientSideOnly=true -> ClientOnly.
        assert_eq!(result.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(result.source, CompatibilitySource::ForgeMetadata);
    }

    // -- Edge cases ------------------------------------------------------

    #[test]
    fn empty_jar_is_unknown() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let zip = zip::ZipWriter::new(tmp.reopen().expect("reopen"));
        zip.finish().expect("finish");
        let result = classify_mod_local(tmp.path());
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }

    #[test]
    fn fabric_unrecognized_environment_is_unknown() {
        let jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"both\"}",
        )]);
        let result = classify_mod_local(jar.path());
        // "both" is not a valid Fabric environment value.
        // Valid values: "client", "server", "*".
        assert_eq!(result.compatibility, ServerCompatibility::Unknown);
    }
}
