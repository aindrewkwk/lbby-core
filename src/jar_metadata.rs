// jar_metadata -- Unified local JAR metadata reader.
//
// ONE parser for all loader metadata. Reused by mod_compat, dependency_graph,
// and future boot/crash tooling. Metadata extraction and compatibility
// interpretation are separate concerns -- this module only extracts.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

/// Dependency kind (required vs optional). Shared across modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyKind {
    Required,
    Optional,
}

/// Normalized dependency from loader metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedDependency {
    pub mod_id: String,
    pub version_requirement: Option<String>,
    pub kind: DependencyKind,
}

/// Which loader metadata format was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoaderMetadataKind {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

/// Raw environment declaration from loader metadata.
/// NOT a compatibility classification -- just what the metadata says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclaredEnvironment {
    Client,
    Server,
    Universal,
}

/// A single environment finding from one metadata source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentFinding {
    pub source: LoaderMetadataKind,
    pub environment: DeclaredEnvironment,
    pub detail: String,
}

/// Normalized metadata extracted from a single JAR file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JarModMetadata {
    pub loader: Option<LoaderMetadataKind>,
    pub mod_ids: Vec<String>,
    pub environment_findings: Vec<EnvironmentFinding>,
    pub dependencies: Vec<NormalizedDependency>,
}

/// Read normalized metadata from a JAR file.
///
/// Inspects: fabric.mod.json, quilt.mod.json, META-INF/mods.toml,
/// META-INF/neoforge.mods.toml.
///
/// Synchronous. No network calls. Returns empty metadata on any read failure.
pub fn read_jar_mod_metadata(path: &Path) -> JarModMetadata {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return empty_metadata(),
    };
    let mut jar = match zip::ZipArchive::new(file) {
        Ok(j) => j,
        Err(_) => return empty_metadata(),
    };

    let mut metadata = empty_metadata();

    // Fabric
    if let Some(text) = read_zip_text(&mut jar, "fabric.mod.json") {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            metadata.loader = Some(LoaderMetadataKind::Fabric);

            if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                metadata.mod_ids.push(id.to_string());
            }

            if let Some(env_str) = value.get("environment").and_then(|v| v.as_str()) {
                if let Some(env) = parse_fabric_environment(env_str) {
                    metadata.environment_findings.push(EnvironmentFinding {
                        source: LoaderMetadataKind::Fabric,
                        environment: env,
                        detail: format!("environment:\"{env_str}\""),
                    });
                }
            }

            parse_fabric_deps(&value, &mut metadata.dependencies);
        }
    }

    // Quilt
    if let Some(text) = read_zip_text(&mut jar, "quilt.mod.json") {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if metadata.loader.is_none() {
                metadata.loader = Some(LoaderMetadataKind::Quilt);
            }

            if metadata.mod_ids.is_empty() {
                if let Some(id) = value
                    .pointer("/quilt_loader/metadata/id")
                    .or_else(|| value.pointer("/quilt_loader/id"))
                    .and_then(|v| v.as_str())
                {
                    metadata.mod_ids.push(id.to_string());
                }
            }

            if let Some(env_str) = value
                .pointer("/quilt_loader/metadata/environment")
                .or_else(|| value.pointer("/quilt_loader/environment"))
                .and_then(|v| v.as_str())
            {
                if let Some(env) = parse_fabric_environment(env_str) {
                    metadata.environment_findings.push(EnvironmentFinding {
                        source: LoaderMetadataKind::Quilt,
                        environment: env,
                        detail: format!("environment:\"{env_str}\""),
                    });
                }
            }

            parse_quilt_deps(&value, &mut metadata.dependencies);
        }
    }

    // Forge / NeoForge
    for (metadata_path, kind) in &[
        ("META-INF/mods.toml", LoaderMetadataKind::Forge),
        ("META-INF/neoforge.mods.toml", LoaderMetadataKind::NeoForge),
    ] {
        if let Some(text) = read_zip_text(&mut jar, metadata_path) {
            if let Ok(value) = text.parse::<toml::Value>() {
                if metadata.loader.is_none() {
                    metadata.loader = Some(*kind);
                }

                // ALL entries in [[mods]] array
                if let Some(mods) = value.get("mods").and_then(|m| m.as_array()) {
                    for entry in mods {
                        if let Some(id) = entry.get("modId").and_then(|v| v.as_str()) {
                            if !metadata.mod_ids.contains(&id.to_string()) {
                                metadata.mod_ids.push(id.to_string());
                            }
                        }
                    }
                }

                if let Some(client_only) = value.get("clientSideOnly").and_then(|v| v.as_bool()) {
                    let env = if client_only {
                        DeclaredEnvironment::Client
                    } else {
                        DeclaredEnvironment::Universal
                    };
                    metadata.environment_findings.push(EnvironmentFinding {
                        source: *kind,
                        environment: env,
                        detail: format!("clientSideOnly={client_only}"),
                    });
                }

                parse_forge_deps(&value, &mut metadata.dependencies);
            }
        }
    }

    metadata
}

/// Check if a mod ID is a platform/loader dependency that should be
/// filtered from dependency tracking.
pub fn is_platform_id(mod_id: &str) -> bool {
    matches!(
        mod_id,
        "minecraft" | "forge" | "neoforge" | "fabricloader" | "fabric" | "quilt_loader"
    )
}

// -- Internal helpers ----------------------------------------------------

fn empty_metadata() -> JarModMetadata {
    JarModMetadata {
        loader: None,
        mod_ids: Vec::new(),
        environment_findings: Vec::new(),
        dependencies: Vec::new(),
    }
}

fn read_zip_text(jar: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut entry = jar.by_name(name).ok()?;
    let mut contents = String::new();
    entry.read_to_string(&mut contents).ok()?;
    Some(contents)
}

fn parse_fabric_environment(s: &str) -> Option<DeclaredEnvironment> {
    match s {
        "client" => Some(DeclaredEnvironment::Client),
        "server" => Some(DeclaredEnvironment::Server),
        "*" => Some(DeclaredEnvironment::Universal),
        _ => None,
    }
}

fn parse_fabric_deps(value: &serde_json::Value, out: &mut Vec<NormalizedDependency>) {
    if let Some(depends) = value.get("depends").and_then(|d| d.as_object()) {
        for (mod_id, version) in depends {
            if is_platform_id(mod_id) {
                continue;
            }
            out.push(NormalizedDependency {
                mod_id: mod_id.clone(),
                version_requirement: version.as_str().map(|s| s.to_string()),
                kind: DependencyKind::Required,
            });
        }
    }
    if let Some(suggests) = value.get("suggests").and_then(|d| d.as_object()) {
        for (mod_id, version) in suggests {
            if is_platform_id(mod_id) {
                continue;
            }
            out.push(NormalizedDependency {
                mod_id: mod_id.clone(),
                version_requirement: version.as_str().map(|s| s.to_string()),
                kind: DependencyKind::Optional,
            });
        }
    }
}

fn parse_quilt_deps(value: &serde_json::Value, out: &mut Vec<NormalizedDependency>) {
    let depends = value
        .pointer("/quilt_loader/depends")
        .or_else(|| value.pointer("/quilt_loader/metadata/depends"));
    let suggests = value
        .pointer("/quilt_loader/suggests")
        .or_else(|| value.pointer("/quilt_loader/metadata/suggests"));

    if let Some(depends) = depends.and_then(|d| d.as_object()) {
        for (mod_id, version) in depends {
            if is_platform_id(mod_id) {
                continue;
            }
            out.push(NormalizedDependency {
                mod_id: mod_id.clone(),
                version_requirement: quilt_version_string(version),
                kind: DependencyKind::Required,
            });
        }
    }
    if let Some(suggests) = suggests.and_then(|d| d.as_object()) {
        for (mod_id, version) in suggests {
            if is_platform_id(mod_id) {
                continue;
            }
            out.push(NormalizedDependency {
                mod_id: mod_id.clone(),
                version_requirement: quilt_version_string(version),
                kind: DependencyKind::Optional,
            });
        }
    }
}

fn quilt_version_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(obj) => obj
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn parse_forge_deps(value: &toml::Value, out: &mut Vec<NormalizedDependency>) {
    let Some(deps_table) = value.get("dependencies").and_then(|d| d.as_table()) else {
        return;
    };
    for (_key, dep_list) in deps_table {
        let Some(arr) = dep_list.as_array() else {
            continue;
        };
        for dep in arr {
            let mod_id = dep.get("modId").and_then(|v| v.as_str()).unwrap_or("");
            if mod_id.is_empty() || is_platform_id(mod_id) {
                continue;
            }
            let version_range = dep
                .get("versionRange")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mandatory = dep
                .get("mandatory")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let dep_type = dep.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let is_required = mandatory || dep_type.eq_ignore_ascii_case("required");

            out.push(NormalizedDependency {
                mod_id: mod_id.to_string(),
                version_requirement: version_range,
                kind: if is_required {
                    DependencyKind::Required
                } else {
                    DependencyKind::Optional
                },
            });
        }
    }
}

// -- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    // Helper: build a Fabric mod.json byte string from parts
    fn fabric_json(id: &str, env: &str, deps: &str, suggests: &str) -> Vec<u8> {
        let mut s = format!(
            "{{\"schemaVersion\":1,\"id\":\"{}\",\"environment\":\"{}\"",
            id, env
        );
        if !deps.is_empty() {
            s.push_str(&format!(",\"depends\":{{{}}}", deps));
        }
        if !suggests.is_empty() {
            s.push_str(&format!(",\"suggests\":{{{}}}", suggests));
        }
        s.push('}');
        s.into_bytes()
    }

    // Helper: build Forge mods.toml byte string
    fn forge_toml(mod_ids: &[&str], client_side: Option<bool>, deps: &str) -> Vec<u8> {
        let mut s = String::from("modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n");
        if let Some(val) = client_side {
            s.push_str(&format!("clientSideOnly={}\n", val));
        }
        for id in mod_ids {
            s.push_str(&format!("\n[[mods]]\nmodId=\"{}\"\n", id));
        }
        if !deps.is_empty() {
            s.push('\n');
            s.push_str(deps);
        }
        s.into_bytes()
    }

    // -- Multiple Forge mod IDs ------------------------------------------

    #[test]
    fn forge_multiple_mod_ids() {
        let toml = forge_toml(&["core", "api"], None, "");
        let jar = make_test_jar(&[("META-INF/mods.toml", &toml)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.mod_ids.len(), 2);
        assert!(meta.mod_ids.contains(&"core".to_string()));
        assert!(meta.mod_ids.contains(&"api".to_string()));
        assert_eq!(meta.loader, Some(LoaderMetadataKind::Forge));
    }

    // -- Fabric single mod ID --------------------------------------------

    #[test]
    fn fabric_single_mod_id() {
        let json = fabric_json("my_mod", "*", "", "");
        let jar = make_test_jar(&[("fabric.mod.json", &json)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.mod_ids, vec!["my_mod"]);
        assert_eq!(meta.loader, Some(LoaderMetadataKind::Fabric));
        assert_eq!(meta.environment_findings.len(), 1);
        assert_eq!(
            meta.environment_findings[0].environment,
            DeclaredEnvironment::Universal
        );
    }

    // -- Corrupt JAR -----------------------------------------------------

    #[test]
    fn corrupt_jar_returns_empty_metadata() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        std::fs::write(tmp.path(), b"not a zip file").expect("write");
        let meta = read_jar_mod_metadata(tmp.path());
        assert!(meta.mod_ids.is_empty());
        assert!(meta.dependencies.is_empty());
        assert!(meta.environment_findings.is_empty());
        assert!(meta.loader.is_none());
    }

    // -- Nonexistent path ------------------------------------------------

    #[test]
    fn nonexistent_path_returns_empty_metadata() {
        let meta = read_jar_mod_metadata(Path::new("/nonexistent/mod.jar"));
        assert!(meta.mod_ids.is_empty());
        assert!(meta.dependencies.is_empty());
    }

    // -- Dependencies preserved ------------------------------------------

    #[test]
    fn fabric_depends_and_suggests_preserved() {
        let json = fabric_json("test", "*", "\"req_dep\":\"1.0\"", "\"opt_dep\":\"2.0\"");
        let jar = make_test_jar(&[("fabric.mod.json", &json)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.dependencies.len(), 2);

        let req = meta
            .dependencies
            .iter()
            .find(|d| d.mod_id == "req_dep")
            .unwrap();
        assert_eq!(req.kind, DependencyKind::Required);
        assert_eq!(req.version_requirement.as_deref(), Some("1.0"));

        let opt = meta
            .dependencies
            .iter()
            .find(|d| d.mod_id == "opt_dep")
            .unwrap();
        assert_eq!(opt.kind, DependencyKind::Optional);
        assert_eq!(opt.version_requirement.as_deref(), Some("2.0"));
    }

    #[test]
    fn forge_required_and_optional_preserved() {
        let deps = "[[dependencies.test]]\nmodId=\"req_dep\"\nversionRange=\"[1.0,)\"\nmandatory=true\n\n[[dependencies.test]]\nmodId=\"opt_dep\"\nversionRange=\"[2.0,)\"\nmandatory=false\n";
        let toml = forge_toml(&["test"], None, deps);
        let jar = make_test_jar(&[("META-INF/mods.toml", &toml)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.dependencies.len(), 2);

        let req = meta
            .dependencies
            .iter()
            .find(|d| d.mod_id == "req_dep")
            .unwrap();
        assert_eq!(req.kind, DependencyKind::Required);
        assert_eq!(req.version_requirement.as_deref(), Some("[1.0,)"));

        let opt = meta
            .dependencies
            .iter()
            .find(|d| d.mod_id == "opt_dep")
            .unwrap();
        assert_eq!(opt.kind, DependencyKind::Optional);
    }

    // -- Platform IDs filtered -------------------------------------------

    #[test]
    fn platform_ids_excluded_from_dependencies() {
        let deps_str =
            "\"fabricloader\":\">=0.14\",\"fabric\":\"*\",\"minecraft\":\"1.20.1\",\"real_dep\":\"1.0\"";
        let json = fabric_json("test", "*", deps_str, "");
        let jar = make_test_jar(&[("fabric.mod.json", &json)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.dependencies.len(), 1);
        assert_eq!(meta.dependencies[0].mod_id, "real_dep");
    }

    // -- Forge environment -----------------------------------------------

    #[test]
    fn forge_client_side_only_true() {
        let toml = forge_toml(&["test"], Some(true), "");
        let jar = make_test_jar(&[("META-INF/mods.toml", &toml)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.environment_findings.len(), 1);
        assert_eq!(
            meta.environment_findings[0].environment,
            DeclaredEnvironment::Client
        );
    }

    #[test]
    fn forge_client_side_only_false() {
        let toml = forge_toml(&["test"], Some(false), "");
        let jar = make_test_jar(&[("META-INF/mods.toml", &toml)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.environment_findings.len(), 1);
        assert_eq!(
            meta.environment_findings[0].environment,
            DeclaredEnvironment::Universal
        );
    }

    // -- Quilt environment -----------------------------------------------

    #[test]
    fn quilt_client_environment() {
        let json = b"{\"schema_version\":1,\"quilt_loader\":{\"metadata\":{\"id\":\"test\",\"environment\":\"client\"},\"entrypoints\":{}}}";
        let jar = make_test_jar(&[("quilt.mod.json", json.as_slice())]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.environment_findings.len(), 1);
        assert_eq!(
            meta.environment_findings[0].environment,
            DeclaredEnvironment::Client
        );
    }

    // -- Empty JAR -------------------------------------------------------

    #[test]
    fn empty_jar_returns_empty_metadata() {
        let jar = make_test_jar(&[("dummy.txt", b"hello")]);
        let meta = read_jar_mod_metadata(jar.path());
        assert!(meta.mod_ids.is_empty());
        assert!(meta.dependencies.is_empty());
        assert!(meta.environment_findings.is_empty());
    }

    // -- Multiple findings -----------------------------------------------

    #[test]
    fn dual_metadata_jar_produces_multiple_findings() {
        let fabric = fabric_json("test", "client", "", "");
        let forge = forge_toml(&["test"], Some(true), "");
        let jar = make_test_jar(&[("fabric.mod.json", &fabric), ("META-INF/mods.toml", &forge)]);
        let meta = read_jar_mod_metadata(jar.path());
        assert_eq!(meta.environment_findings.len(), 2);
        assert!(meta
            .environment_findings
            .iter()
            .all(|f| f.environment == DeclaredEnvironment::Client));
    }
}
