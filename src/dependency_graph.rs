// dependency_graph -- Mod dependency graph and exclusion planning.
//
// Phase 3B/3B.1: Additive module. Does NOT modify production installer paths.
// Classification and dependency validity are separate concepts.
// Compatibility is NEVER propagated through graph edges.
//
// Uses jar_metadata as the single metadata reader.

use crate::jar_metadata::{self, NormalizedDependency};
use crate::mod_compat::{ModCompatibility, ServerCompatibility};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Re-export for backward compatibility with existing test code.
pub use crate::jar_metadata::DependencyKind;
pub type ModDependency = NormalizedDependency;

// -- Types ---------------------------------------------------------------

/// A node in the dependency graph: one mod JAR with its identity,
/// compatibility classification, and dependency list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModNode {
    /// File path of the JAR.
    pub path: PathBuf,
    /// ALL mod IDs declared in this JAR's metadata.
    /// Empty if no mod ID could be detected.
    pub mod_ids: Vec<String>,
    /// Primary mod ID (first declared). Used for display.
    pub mod_id: Option<String>,
    /// Compatibility classification from the local classifier.
    pub compatibility: ModCompatibility,
    /// What this mod depends on (outgoing edges).
    pub dependencies: Vec<NormalizedDependency>,
}

// -- Dependency graph ----------------------------------------------------

/// A dependency graph built from a set of mod JARs.
///
/// Supports forward lookup (what does this mod need?) and reverse lookup
/// (which mods need this dependency?).
///
/// Supports multiple mod IDs per JAR: a JAR declaring both "core" and "api"
/// is reachable via either ID, but appears as a single node.
///
/// Detects duplicate mod IDs across different JARs: if two different files
/// declare the same mod ID, that ID is recorded as ambiguous.
pub struct DependencyGraph {
    /// All nodes in the graph, indexed by position.
    pub nodes: Vec<ModNode>,
    /// Forward index: mod_id -> indices into `nodes`.
    /// Multiple indices = ambiguous (duplicate ID across different JARs).
    /// Multiple mod_ids from the SAME JAR all point to the same single index.
    by_mod_id: HashMap<String, Vec<usize>>,
    /// File path -> index into `nodes`.
    by_path: HashMap<PathBuf, usize>,
}

impl DependencyGraph {
    /// Build a dependency graph from classified mod JARs.
    ///
    /// Each entry is a (path, compatibility) pair where compatibility
    /// comes from `classify_mod_local()`.
    ///
    /// Reads mod IDs and dependencies from JAR metadata via jar_metadata.
    /// Does NOT make any classification decisions -- uses the provided
    /// compatibility values as-is.
    ///
    /// Supports multiple mod IDs per JAR: all declared IDs index the same node.
    pub fn build(entries: &[(PathBuf, ModCompatibility)]) -> Self {
        let mut nodes = Vec::with_capacity(entries.len());
        let mut by_mod_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_path: HashMap<PathBuf, usize> = HashMap::new();

        for (path, compatibility) in entries {
            let meta = jar_metadata::read_jar_mod_metadata(path);

            let index = nodes.len();
            let mod_id = meta.mod_ids.first().cloned();

            // Register ALL mod IDs as index keys for this single node.
            for id in &meta.mod_ids {
                by_mod_id.entry(id.clone()).or_default().push(index);
            }

            nodes.push(ModNode {
                path: path.clone(),
                mod_ids: meta.mod_ids,
                mod_id,
                compatibility: compatibility.clone(),
                dependencies: meta.dependencies,
            });

            by_path.insert(path.clone(), index);
        }

        Self {
            nodes,
            by_mod_id,
            by_path,
        }
    }

    /// Find a node by its mod ID.
    /// Returns `None` if no node has that mod ID, or if the ID is ambiguous
    /// (declared by multiple different JARs).
    /// Works with ANY of the JAR's declared mod IDs.
    pub fn find_by_mod_id(&self, mod_id: &str) -> Option<&ModNode> {
        match self.by_mod_id.get(mod_id) {
            Some(indices) if indices.len() == 1 => Some(&self.nodes[indices[0]]),
            _ => None, // 0 or ambiguous
        }
    }

    /// Find all nodes that declare a given mod ID.
    /// Returns empty if no node has that mod ID.
    /// Returns multiple entries if the ID is ambiguous (duplicate across JARs).
    pub fn find_all_by_mod_id(&self, mod_id: &str) -> Vec<&ModNode> {
        self.by_mod_id
            .get(mod_id)
            .map(|indices| indices.iter().map(|&idx| &self.nodes[idx]).collect())
            .unwrap_or_default()
    }

    /// Check if a mod ID is ambiguous (declared by multiple different JARs).
    pub fn is_ambiguous(&self, mod_id: &str) -> bool {
        self.by_mod_id
            .get(mod_id)
            .is_some_and(|indices| indices.len() > 1)
    }

    /// Get all ambiguous mod IDs (declared by multiple different JARs).
    pub fn ambiguous_ids(&self) -> Vec<String> {
        self.by_mod_id
            .iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Find a node by its file path.
    pub fn find_by_path(&self, path: &Path) -> Option<&ModNode> {
        self.by_path.get(path).map(|&idx| &self.nodes[idx])
    }

    /// Which mods depend on a given mod ID? (reverse lookup)
    /// Returns indices of all nodes that list `mod_id` as a dependency.
    pub fn dependents_of(&self, mod_id: &str) -> Vec<&ModNode> {
        self.nodes
            .iter()
            .filter(|node| node.dependencies.iter().any(|dep| dep.mod_id == mod_id))
            .collect()
    }

    /// Which retained mods depend on a given mod ID?
    /// Only includes dependents whose compatibility is NOT ClientOnly
    /// (i.e. they will be present in the server build).
    pub fn retained_dependents_of(&self, mod_id: &str) -> Vec<&ModNode> {
        self.nodes
            .iter()
            .filter(|node| {
                node.compatibility.compatibility != ServerCompatibility::ClientOnly
                    && node.dependencies.iter().any(|dep| dep.mod_id == mod_id)
            })
            .collect()
    }

    /// Create an exclusion plan: which mods to exclude, which to retain,
    /// and what conflicts exist.
    ///
    /// Rules:
    /// - ClientOnly + Explicit confidence -> planned exclusion
    /// - Both / ServerOk / Unknown -> retain
    /// - Required dependency of a retained mod that is excluded -> conflict
    /// - Optional dependency of a retained mod that is excluded -> no conflict
    /// - Required dependency that doesn't exist in the build -> missing
    /// - Required dependency that maps to multiple provider JARs -> ambiguous
    pub fn create_plan(&self) -> DependencyPlan {
        let mut exclude: Vec<PathBuf> = Vec::new();
        let mut retain: Vec<PathBuf> = Vec::new();
        let mut conflicts: Vec<CompatibilityConflict> = Vec::new();
        let mut missing: Vec<MissingDependency> = Vec::new();
        let mut ambiguous: Vec<AmbiguousDependency> = Vec::new();

        // Phase 1: Classify each node as exclude or retain.
        for node in &self.nodes {
            if node.compatibility.compatibility == ServerCompatibility::ClientOnly
                && node.compatibility.confidence
                    == crate::mod_compat::CompatibilityConfidence::Explicit
            {
                exclude.push(node.path.clone());
            } else {
                retain.push(node.path.clone());
            }
        }

        // Build a set of excluded mod IDs for conflict checking.
        // Use find_by_path + flat_map to get ALL mod IDs from excluded nodes.
        let excluded_mod_ids: std::collections::HashSet<String> = exclude
            .iter()
            .filter_map(|path| self.find_by_path(path))
            .flat_map(|n| n.mod_ids.clone())
            .collect();

        // Build a set of all known mod IDs (from retained + excluded).
        let all_mod_ids: std::collections::HashSet<String> =
            self.nodes.iter().flat_map(|n| n.mod_ids.clone()).collect();

        // Phase 2: Check for dependency conflicts, missing, and ambiguous deps.
        for node in &self.nodes {
            // Only check retained mods -- excluded mods don't matter.
            if node.compatibility.compatibility == ServerCompatibility::ClientOnly
                && node.compatibility.confidence
                    == crate::mod_compat::CompatibilityConfidence::Explicit
            {
                continue;
            }

            for dep in &node.dependencies {
                if dep.kind == DependencyKind::Optional {
                    // Optional dependencies don't create blocking conflicts.
                    continue;
                }

                // Check ambiguity FIRST: multiple providers for this dep ID.
                if self.is_ambiguous(&dep.mod_id) {
                    let provider_paths: Vec<PathBuf> = self
                        .find_all_by_mod_id(&dep.mod_id)
                        .iter()
                        .map(|n| n.path.clone())
                        .collect();
                    ambiguous.push(AmbiguousDependency {
                        dependent_mod_id: node.mod_id.clone(),
                        dependency_mod_id: dep.mod_id.clone(),
                        dependent_path: node.path.clone(),
                        provider_paths,
                        version_requirement: dep.version_requirement.clone(),
                    });
                    continue;
                }

                if excluded_mod_ids.contains(&dep.mod_id) {
                    // Required dependency is excluded.
                    let dependency_path = self.find_by_mod_id(&dep.mod_id).map(|n| n.path.clone());

                    conflicts.push(CompatibilityConflict {
                        dependent_mod_id: node.mod_id.clone(),
                        dependency_mod_id: dep.mod_id.clone(),
                        dependent_path: node.path.clone(),
                        dependency_path,
                        reason: format!(
                            "Mod {:?} requires dependency '{}' which is classified as ClientOnly/Explicit",
                            node.mod_id.as_deref().unwrap_or("unknown"),
                            dep.mod_id
                        ),
                    });
                } else if !all_mod_ids.contains(&dep.mod_id) {
                    // Required dependency doesn't exist in the build at all.
                    missing.push(MissingDependency {
                        dependent_mod_id: node.mod_id.clone(),
                        dependency_mod_id: dep.mod_id.clone(),
                        dependent_path: node.path.clone(),
                        version_requirement: dep.version_requirement.clone(),
                    });
                }
            }
        }

        DependencyPlan {
            exclude,
            retain,
            conflicts,
            missing,
            ambiguous,
        }
    }
}

// -- Plan types ----------------------------------------------------------

/// The result of dependency-aware exclusion planning.
#[derive(Debug, Serialize, Deserialize)]
pub struct DependencyPlan {
    /// Mods to exclude (ClientOnly with Explicit confidence).
    pub exclude: Vec<PathBuf>,
    /// Mods to retain (ServerOk, Both, Unknown, or ClientOnly without
    /// Explicit confidence).
    pub retain: Vec<PathBuf>,
    /// Conflicts: a retained mod requires a dependency that is excluded.
    pub conflicts: Vec<CompatibilityConflict>,
    /// Missing: a retained mod requires a dependency not in the build.
    pub missing: Vec<MissingDependency>,
    /// Ambiguous: a required dependency maps to multiple provider JARs.
    pub ambiguous: Vec<AmbiguousDependency>,
}

/// A conflict where a retained mod depends on an excluded mod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityConflict {
    /// Mod ID of the dependent (the one that needs the dependency).
    pub dependent_mod_id: Option<String>,
    /// Mod ID of the excluded dependency.
    pub dependency_mod_id: String,
    /// File path of the dependent.
    pub dependent_path: PathBuf,
    /// File path of the excluded dependency, if known.
    pub dependency_path: Option<PathBuf>,
    /// Human-readable explanation.
    pub reason: String,
}

/// A required dependency that doesn't exist in the build at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingDependency {
    /// Mod ID of the dependent.
    pub dependent_mod_id: Option<String>,
    /// Mod ID of the missing dependency.
    pub dependency_mod_id: String,
    /// File path of the dependent.
    pub dependent_path: PathBuf,
    /// Version constraint, if any.
    pub version_requirement: Option<String>,
}

/// A required dependency that maps to multiple provider JARs.
/// No arbitrary winner is selected — both files remain until resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguousDependency {
    /// Mod ID of the dependent (the one that needs the dependency).
    pub dependent_mod_id: Option<String>,
    /// Mod ID of the ambiguous dependency.
    pub dependency_mod_id: String,
    /// File path of the dependent.
    pub dependent_path: PathBuf,
    /// File paths of all provider JARs that declare this mod ID.
    pub provider_paths: Vec<PathBuf>,
    /// Version constraint, if any.
    pub version_requirement: Option<String>,
}

// -- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mod_compat::{CompatibilityConfidence, CompatibilitySource};
    use std::io::Write;

    // -- Test helpers ----------------------------------------------------

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

    fn make_compat(compat: crate::mod_compat::ServerCompatibility) -> ModCompatibility {
        ModCompatibility {
            compatibility: compat,
            confidence: CompatibilityConfidence::Explicit,
            source: CompatibilitySource::FabricMetadata,
            reason: "test".to_string(),
        }
    }

    fn make_unknown_compat() -> ModCompatibility {
        ModCompatibility {
            compatibility: ServerCompatibility::Unknown,
            confidence: CompatibilityConfidence::None,
            source: CompatibilitySource::None,
            reason: "test".to_string(),
        }
    }

    /// Wrapper around a file at a known path, providing .path()
    /// like tempfile::NamedTempFile but at a caller-chosen location.
    struct OwnedPath(std::path::PathBuf);
    impl OwnedPath {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn close(self) -> std::io::Result<()> {
            // consumed, file stays on disk
            Ok(())
        }
    }
    impl Drop for OwnedPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Create a Fabric JAR with optional environment and mod ID.
    /// Uses `classify_mod_local()` to get real classification.
    fn create_fabric_jar(dir: &Path, name: &str, env: Option<&str>, mod_id: &str) -> OwnedPath {
        let env_str = env.unwrap_or("*");
        let json = format!(
            "{{\"schemaVersion\":1,\"id\":\"{}\",\"environment\":\"{}\"}}",
            mod_id, env_str
        );
        make_test_jar_in(dir, name, &[("fabric.mod.json", json.as_bytes())])
    }

    /// Create a Fabric JAR with dependencies.
    fn create_fabric_jar_with_deps(
        dir: &Path,
        name: &str,
        env: Option<&str>,
        mod_id: &str,
        deps: &[(&str, bool)], // (dep_id, required)
    ) -> OwnedPath {
        let env_str = env.unwrap_or("*");
        let mut dep_map = String::from("{");
        for (i, (dep_id, _required)) in deps.iter().enumerate() {
            if i > 0 {
                dep_map.push(',');
            }
            dep_map.push_str(&format!("\"{}\":\"1.0\"", dep_id));
        }
        dep_map.push('}');
        let json = format!(
            "{{\"schemaVersion\":1,\"id\":\"{}\",\"environment\":\"{}\",\"depends\":{}}}",
            mod_id, env_str, dep_map
        );
        make_test_jar_in(dir, name, &[("fabric.mod.json", json.as_bytes())])
    }

    /// Create a Forge JAR with optional clientSideOnly and dependencies.
    fn create_forge_jar(
        dir: &Path,
        name: &str,
        mod_id: &str,
        client_side_only: Option<bool>,
        deps: &[(&str, bool)],
    ) -> OwnedPath {
        let cso_line = match client_side_only {
            Some(true) => "clientSideOnly=true\n",
            Some(false) => "clientSideOnly=false\n",
            None => "",
        };
        let mut toml = format!(
            "modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"{}\"\n{}",
            mod_id, cso_line
        );
        for (dep_id, required) in deps {
            toml.push_str(&format!(
                "\n[[dependencies.{}]]\nmodId=\"{}\"\nmandatory={}\nversionRange=\"[1,)\"\n",
                mod_id, dep_id, required
            ));
        }
        make_test_jar_in(dir, name, &[("META-INF/mods.toml", toml.as_bytes())])
    }

    /// Create a test JAR at a known path (dir/name) and return OwnedPath.
    fn make_test_jar_in(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> OwnedPath {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (entry_name, data) in entries {
            zip.start_file(entry_name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        OwnedPath(path)
    }

    // -- Multiple Forge mod IDs ------------------------------------------

    #[test]
    fn forge_multiple_mod_ids_all_resolve_to_same_node() {
        let toml = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"core\"\n\n[[mods]]\nmodId=\"api\"\n";
        let jar = make_test_jar(&[("META-INF/mods.toml", toml)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        // Both IDs resolve to the same node
        let core = graph.find_by_mod_id("core");
        let api = graph.find_by_mod_id("api");
        assert!(core.is_some());
        assert!(api.is_some());
        assert_eq!(core.unwrap().path, api.unwrap().path);

        // Only one node in the graph
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].mod_ids.len(), 2);
    }

    #[test]
    fn dependency_on_any_multi_id_resolves() {
        let dep_jar = make_test_jar(&[(
            "META-INF/mods.toml",
            b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"core\"\n\n[[mods]]\nmodId=\"api\"\n",
        )]);

        let consumer_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"consumer\",\"environment\":\"*\",\"depends\":{\"api\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (dep_jar.path().to_path_buf(), make_unknown_compat()),
            (
                consumer_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
        ]);

        // Dependency on "api" resolves even though primary mod_id is "core"
        let consumer = graph.find_by_mod_id("consumer").unwrap();
        assert_eq!(consumer.dependencies.len(), 1);
        assert_eq!(consumer.dependencies[0].mod_id, "api");

        let resolved = graph.find_by_mod_id("api");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().mod_ids, vec!["core", "api"]);
    }

    // -- No duplicate nodes ----------------------------------------------

    #[test]
    fn multi_id_jar_appears_once_in_nodes() {
        let toml = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"a\"\n\n[[mods]]\nmodId=\"b\"\n\n[[mods]]\nmodId=\"c\"\n";
        let jar = make_test_jar(&[("META-INF/mods.toml", toml)]);
        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].mod_ids, vec!["a", "b", "c"]);
        assert!(graph.find_by_mod_id("a").is_some());
        assert!(graph.find_by_mod_id("b").is_some());
        assert!(graph.find_by_mod_id("c").is_some());
    }

    // -- Shared library retained when used by both sides -----------------

    #[test]
    fn shared_library_retained_when_used_by_both_sides() {
        let lib_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"shared_lib\",\"environment\":\"*\",\"depends\":{}}",
        )]);
        let client_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"client_mod\",\"environment\":\"client\",\"depends\":{\"shared_lib\":\"1.0\"}}",
        )]);
        let server_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"server_mod\",\"environment\":\"server\",\"depends\":{\"shared_lib\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                lib_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                client_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ClientOnly),
            ),
            (
                server_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ServerOk),
            ),
        ]);

        let plan = graph.create_plan();

        // Library should be retained (it's Both, not ClientOnly)
        assert!(plan.retain.contains(&lib_jar.path().to_path_buf()));
        // Client mod should be excluded
        assert!(plan.exclude.contains(&client_jar.path().to_path_buf()));
        // Server mod should be retained
        assert!(plan.retain.contains(&server_jar.path().to_path_buf()));
    }

    // -- Client dependency does not propagate classification -------------

    #[test]
    fn client_dependency_does_not_propagate_classification() {
        let shared_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"shared\",\"environment\":\"*\"}",
        )]);
        let client_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"client\",\"environment\":\"client\",\"depends\":{\"shared\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                shared_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                client_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ClientOnly),
            ),
        ]);

        // Shared mod should still be Both, not changed to ClientOnly
        let shared = graph.find_by_mod_id("shared").unwrap();
        assert_eq!(
            shared.compatibility.compatibility,
            ServerCompatibility::Both
        );
    }

    // -- Server mod requiring excluded ClientOnly dependency -------------

    #[test]
    fn server_requires_client_only_dependency_records_conflict() {
        let dep_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"client_dep\",\"environment\":\"client\"}",
        )]);
        let server_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"server_mod\",\"environment\":\"server\",\"depends\":{\"client_dep\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                dep_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ClientOnly),
            ),
            (
                server_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ServerOk),
            ),
        ]);

        let plan = graph.create_plan();

        // Dependency is excluded
        assert!(plan.exclude.contains(&dep_jar.path().to_path_buf()));
        // Server mod is retained (not excluded just because its dep is)
        assert!(plan.retain.contains(&server_jar.path().to_path_buf()));
        // But a conflict is recorded
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].dependency_mod_id, "client_dep");
    }

    // -- Optional excluded dependency: no blocking conflict --------------

    #[test]
    fn optional_client_dependency_no_blocking_conflict() {
        let dep_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"opt_dep\",\"environment\":\"client\"}",
        )]);
        let server_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"server_mod\",\"environment\":\"server\",\"suggests\":{\"opt_dep\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                dep_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ClientOnly),
            ),
            (
                server_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ServerOk),
            ),
        ]);

        let plan = graph.create_plan();

        // Optional dep is excluded
        assert!(plan.exclude.contains(&dep_jar.path().to_path_buf()));
        // No blocking conflict because dependency is optional
        assert!(plan.conflicts.is_empty());
    }

    // -- Missing required dependency -------------------------------------

    #[test]
    fn missing_required_dependency_recorded() {
        let jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"*\",\"depends\":{\"nonexistent_mod\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[(
            jar.path().to_path_buf(),
            make_compat(ServerCompatibility::Both),
        )]);

        let plan = graph.create_plan();
        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan.missing[0].dependency_mod_id, "nonexistent_mod");
    }

    // -- Circular dependency: no crash -----------------------------------

    #[test]
    fn circular_dependency_no_stack_overflow() {
        let jar_a = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"mod_a\",\"environment\":\"*\",\"depends\":{\"mod_b\":\"1.0\"}}",
        )]);
        let jar_b = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"mod_b\",\"environment\":\"*\",\"depends\":{\"mod_a\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                jar_a.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                jar_b.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
        ]);

        // Should not stack overflow or hang
        let plan = graph.create_plan();
        assert_eq!(plan.retain.len(), 2);
        assert!(plan.conflicts.is_empty());
    }

    // -- Unknown mod ID represented safely -------------------------------

    #[test]
    fn unknown_mod_id_represented_safely() {
        let jar = make_test_jar(&[("dummy.txt", b"no metadata here")]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        assert_eq!(graph.nodes.len(), 1);
        assert!(graph.nodes[0].mod_id.is_none());
        assert!(graph.nodes[0].mod_ids.is_empty());
        // Can still find by path
        assert!(graph.find_by_path(jar.path()).is_some());
    }

    // -- Filename not used as identity -----------------------------------

    #[test]
    fn filename_not_used_as_identity() {
        let jar = make_test_jar(&[("dummy.txt", b"no metadata")]);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("my-cool-mod-1.0.jar");
        std::fs::copy(jar.path(), &path).expect("copy");

        let graph = DependencyGraph::build(&[(path.clone(), make_unknown_compat())]);

        assert!(graph.nodes[0].mod_id.is_none());
        assert!(graph.find_by_mod_id("my-cool-mod").is_none());
    }

    // -- Dependency used by multiple retained mods -----------------------

    #[test]
    fn dependency_used_by_multiple_retained_mods() {
        let lib_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"lib\",\"environment\":\"*\"}",
        )]);
        let mod_a = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"mod_a\",\"environment\":\"*\",\"depends\":{\"lib\":\"1.0\"}}",
        )]);
        let mod_b = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"mod_b\",\"environment\":\"*\",\"depends\":{\"lib\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                lib_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                mod_a.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                mod_b.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
        ]);

        let dependents = graph.dependents_of("lib");
        assert_eq!(dependents.len(), 2);
    }

    // -- Forge required and optional dependencies ------------------------

    #[test]
    fn forge_required_and_optional_dependencies() {
        let toml = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"test\"\n\n[[dependencies.test]]\nmodId=\"req_dep\"\nversionRange=\"[1.0,)\"\nmandatory=true\n\n[[dependencies.test]]\nmodId=\"opt_dep\"\nversionRange=\"[2.0,)\"\nmandatory=false\n";
        let jar = make_test_jar(&[("META-INF/mods.toml", toml)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        let node = &graph.nodes[0];
        assert_eq!(node.dependencies.len(), 2);

        let req = node
            .dependencies
            .iter()
            .find(|d| d.mod_id == "req_dep")
            .unwrap();
        assert_eq!(req.kind, DependencyKind::Required);
        assert_eq!(req.version_requirement.as_deref(), Some("[1.0,)"));

        let opt = node
            .dependencies
            .iter()
            .find(|d| d.mod_id == "opt_dep")
            .unwrap();
        assert_eq!(opt.kind, DependencyKind::Optional);
    }

    // -- Fabric suggests are optional ------------------------------------

    #[test]
    fn fabric_suggests_are_optional() {
        let json = b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"*\",\"depends\":{\"req\":\"1.0\"},\"suggests\":{\"opt\":\"2.0\"}}";
        let jar = make_test_jar(&[("fabric.mod.json", json)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        let node = &graph.nodes[0];
        assert_eq!(node.dependencies.len(), 2);

        let req = node
            .dependencies
            .iter()
            .find(|d| d.mod_id == "req")
            .unwrap();
        assert_eq!(req.kind, DependencyKind::Required);

        let opt = node
            .dependencies
            .iter()
            .find(|d| d.mod_id == "opt")
            .unwrap();
        assert_eq!(opt.kind, DependencyKind::Optional);
    }

    // -- Missing and excluded are distinct -------------------------------

    #[test]
    fn missing_and_excluded_are_distinct() {
        // Excluded: exists in build, but ClientOnly
        let excluded_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"excluded_mod\",\"environment\":\"client\"}",
        )]);
        // Missing: referenced but not in build at all
        let consumer_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"consumer\",\"environment\":\"*\",\"depends\":{\"excluded_mod\":\"1.0\",\"ghost_mod\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                excluded_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ClientOnly),
            ),
            (
                consumer_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
        ]);

        let plan = graph.create_plan();

        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].dependency_mod_id, "excluded_mod");

        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan.missing[0].dependency_mod_id, "ghost_mod");
    }

    // -- Platform IDs not included as dependencies -----------------------

    #[test]
    fn platform_ids_not_included_as_dependencies() {
        let json = b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"*\",\"depends\":{\"fabricloader\":\">=0.14\",\"fabric\":\"*\",\"minecraft\":\"1.20.1\",\"real_dep\":\"1.0\"}}";
        let jar = make_test_jar(&[("fabric.mod.json", json)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        let node = &graph.nodes[0];
        assert_eq!(node.dependencies.len(), 1);
        assert_eq!(node.dependencies[0].mod_id, "real_dep");
    }

    #[test]
    fn forge_platform_ids_not_included() {
        let toml = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"test\"\n\n[[dependencies.test]]\nmodId=\"forge\"\nversionRange=\"[47,)\"\nmandatory=true\n\n[[dependencies.test]]\nmodId=\"minecraft\"\nversionRange=\"[1.20.1,1.21)\"\nmandatory=true\n\n[[dependencies.test]]\nmodId=\"neoforge\"\nversionRange=\"[20.4,)\"\nmandatory=true\n";
        let jar = make_test_jar(&[("META-INF/mods.toml", toml)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        assert!(graph.nodes[0].dependencies.is_empty());
    }

    // -- Retained dependents excludes client-only ------------------------

    #[test]
    fn retained_dependents_excludes_client_only() {
        let lib_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"shared_lib\",\"environment\":\"*\"}",
        )]);
        let client_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"client\",\"environment\":\"client\",\"depends\":{\"shared_lib\":\"1.0\"}}",
        )]);
        let server_jar = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"server\",\"environment\":\"server\",\"depends\":{\"shared_lib\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                lib_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                client_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ClientOnly),
            ),
            (
                server_jar.path().to_path_buf(),
                make_compat(ServerCompatibility::ServerOk),
            ),
        ]);

        let all_deps = graph.dependents_of("shared_lib");
        assert_eq!(all_deps.len(), 2);

        let retained_deps = graph.retained_dependents_of("shared_lib");
        assert_eq!(retained_deps.len(), 1);
        assert_eq!(retained_deps[0].mod_id, Some("server".to_string()));
    }

    // -- Version requirements preserved ----------------------------------

    #[test]
    fn version_requirements_preserved_in_graph() {
        let json = b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"*\",\"depends\":{\"dep_a\":\">=1.0\",\"dep_b\":\"[2.0,3.0)\"}}";
        let jar = make_test_jar(&[("fabric.mod.json", json)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        let node = &graph.nodes[0];
        let dep_a = node
            .dependencies
            .iter()
            .find(|d| d.mod_id == "dep_a")
            .unwrap();
        assert_eq!(dep_a.version_requirement.as_deref(), Some(">=1.0"));

        let dep_b = node
            .dependencies
            .iter()
            .find(|d| d.mod_id == "dep_b")
            .unwrap();
        assert_eq!(dep_b.version_requirement.as_deref(), Some("[2.0,3.0)"));
    }

    // -- Metadata consistency: normalized reader -> same classification --

    #[test]
    fn normalized_metadata_consistent_with_classification() {
        // A Fabric client-only JAR parsed through jar_metadata should
        // produce the same classification as the direct classifier.
        let json = b"{\"schemaVersion\":1,\"id\":\"test\",\"environment\":\"client\"}";
        let jar = make_test_jar(&[("fabric.mod.json", json)]);

        let meta = jar_metadata::read_jar_mod_metadata(jar.path());
        assert_eq!(meta.environment_findings.len(), 1);
        assert_eq!(
            meta.environment_findings[0].environment,
            jar_metadata::DeclaredEnvironment::Client
        );

        // And the classifier should agree
        let compat = crate::mod_compat::classify_mod_local(jar.path());
        assert_eq!(compat.compatibility, ServerCompatibility::ClientOnly);
    }

    // -- Duplicate mod ID detection ------------------------------------

    #[test]
    fn duplicate_mod_id_across_two_jars_is_detectable() {
        let jar_a = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"foo\",\"environment\":\"*\"}",
        )]);
        let jar_b = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"foo\",\"environment\":\"*\"}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                jar_a.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                jar_b.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
        ]);

        // ID "foo" is ambiguous
        assert!(graph.is_ambiguous("foo"));
        assert_eq!(graph.ambiguous_ids().len(), 1);
        assert_eq!(graph.ambiguous_ids()[0], "foo");

        // find_by_mod_id returns None for ambiguous IDs
        assert!(graph.find_by_mod_id("foo").is_none());

        // find_all_by_mod_id returns both providers
        let all = graph.find_all_by_mod_id("foo");
        assert_eq!(all.len(), 2);

        // Two nodes in the graph
        assert_eq!(graph.nodes.len(), 2);
    }

    #[test]
    fn ambiguous_dependency_recorded_in_plan() {
        let dep_a = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"lib\",\"environment\":\"*\"}",
        )]);
        let dep_b = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"lib\",\"environment\":\"*\"}",
        )]);
        let consumer = make_test_jar(&[(
            "fabric.mod.json",
            b"{\"schemaVersion\":1,\"id\":\"consumer\",\"environment\":\"*\",\"depends\":{\"lib\":\"1.0\"}}",
        )]);

        let graph = DependencyGraph::build(&[
            (
                dep_a.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                dep_b.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
            (
                consumer.path().to_path_buf(),
                make_compat(ServerCompatibility::Both),
            ),
        ]);

        let plan = graph.create_plan();

        // No arbitrary winner selected
        assert_eq!(plan.ambiguous.len(), 1);
        assert_eq!(plan.ambiguous[0].dependency_mod_id, "lib");
        assert_eq!(plan.ambiguous[0].provider_paths.len(), 2);
        // Both files remain (neither excluded nor arbitrarily dropped)
        assert_eq!(plan.retain.len(), 3);
        assert!(plan.exclude.is_empty());
    }

    #[test]
    fn multi_id_same_jar_not_ambiguous() {
        let toml = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n\n[[mods]]\nmodId=\"core\"\n\n[[mods]]\nmodId=\"api\"\n";
        let jar = make_test_jar(&[("META-INF/mods.toml", toml)]);

        let graph = DependencyGraph::build(&[(jar.path().to_path_buf(), make_unknown_compat())]);

        // Both IDs resolve without ambiguity (same JAR)
        assert!(!graph.is_ambiguous("core"));
        assert!(!graph.is_ambiguous("api"));
        assert!(graph.ambiguous_ids().is_empty());

        // Both resolve to the same node
        assert!(graph.find_by_mod_id("core").is_some());
        assert!(graph.find_by_mod_id("api").is_some());
    }

    // ====================================================================
    // Phase 3C integration tests: full pipeline
    //   classify → dependency graph → exclusion plan → quarantine
    //
    // These tests verify the CurseForge pipeline behavior without
    // needing network, app state, or the actual installer.
    // ====================================================================

    /// Confirmed client mod → downloaded, classified ClientOnly/Explicit,
    /// moved to quarantine, not present in active mods/.
    #[test]
    fn phase3c_confirmed_client_mod_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // Fabric environment="client" JAR
        let client_jar = create_fabric_jar(&mods_dir, "client-ui.jar", Some("client"), "client-ui");

        let analysis = vec![(
            client_jar.path().to_path_buf(),
            crate::mod_compat::classify_mod_local(client_jar.path()),
        )];

        assert_eq!(analysis[0].1.compatibility, ServerCompatibility::ClientOnly);
        assert_eq!(analysis[0].1.confidence, CompatibilityConfidence::Explicit);

        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // Should be excluded
        assert_eq!(plan.exclude.len(), 1);
        assert_eq!(plan.exclude[0], client_jar.path());

        // Simulate quarantine: move to .lbby-client-only-mods/
        let quarantine_dir = tmp.path().join(".lbby-client-only-mods");
        std::fs::create_dir_all(&quarantine_dir).unwrap();
        let dest = quarantine_dir.join("client-ui.jar");
        std::fs::rename(client_jar.path(), &dest).unwrap();

        // Source no longer in mods/
        assert!(!client_jar.path().exists());
        // But exists in quarantine
        assert!(dest.exists());
    }

    /// No environment metadata → Unknown → kept in mods/, not quarantined.
    #[test]
    fn phase3c_unknown_mod_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // Forge JAR with no clientSideOnly field
        let unknown_jar = create_forge_jar(&mods_dir, "mystery.jar", "mystery", None, &[]);

        let compat = crate::mod_compat::classify_mod_local(unknown_jar.path());
        assert_eq!(compat.compatibility, ServerCompatibility::Unknown);

        let analysis = vec![(unknown_jar.path().to_path_buf(), compat)];
        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // Should NOT be excluded
        assert!(plan.exclude.is_empty());
        // File still exists
        assert!(unknown_jar.path().exists());
    }

    /// Misleading filename: "super-client-library.jar" with no client metadata.
    /// Filename heuristics have ZERO influence → Unknown → KEPT.
    #[test]
    fn phase3c_misleading_filename_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let misleading_jar = create_forge_jar(
            &mods_dir,
            "super-client-library.jar",
            "super-lib",
            None, // no clientSideOnly
            &[],
        );

        let compat = crate::mod_compat::classify_mod_local(misleading_jar.path());
        // Filename does NOT influence classification
        assert_eq!(compat.compatibility, ServerCompatibility::Unknown);

        let analysis = vec![(misleading_jar.path().to_path_buf(), compat)];
        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        assert!(plan.exclude.is_empty());
        assert!(misleading_jar.path().exists());
    }

    /// Shared library scenario:
    ///   Client A → requires Library B
    ///   Server C → requires Library B
    /// Expected: A quarantined, B kept, C kept.
    #[test]
    fn phase3c_shared_library_kept_when_partially_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // A: client-only, depends on B
        let jar_a = create_fabric_jar_with_deps(
            &mods_dir,
            "client-a.jar",
            Some("client"),
            "client-a",
            &[("shared-lib", true)],
        );
        // B: universal
        let jar_b = create_fabric_jar(&mods_dir, "shared-lib.jar", None, "shared-lib");
        // C: universal, depends on B
        let jar_c = create_fabric_jar_with_deps(
            &mods_dir,
            "server-c.jar",
            None,
            "server-c",
            &[("shared-lib", true)],
        );

        let analysis = vec![
            (
                jar_a.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_a.path()),
            ),
            (
                jar_b.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_b.path()),
            ),
            (
                jar_c.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_c.path()),
            ),
        ];

        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // Only A should be excluded
        assert_eq!(plan.exclude.len(), 1);
        assert_eq!(plan.exclude[0], jar_a.path());

        // B should NOT be excluded (Both/universal)
        assert!(!plan.exclude.contains(&jar_b.path().to_path_buf()));
        // C should NOT be excluded (universal)
        assert!(!plan.exclude.contains(&jar_c.path().to_path_buf()));

        // No conflicts: B is not excluded, so C's dep on B is satisfied
        assert!(plan.conflicts.is_empty());
    }

    /// Server requires client-only dependency:
    ///   C requires D, D = ClientOnly/Explicit
    /// Expected: C kept, D quarantined, conflict recorded.
    #[test]
    fn phase3c_server_requires_client_only_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // D: client-only
        let jar_d = create_fabric_jar(&mods_dir, "client-dep.jar", Some("client"), "client-dep");
        // C: universal but requires D
        let jar_c = create_fabric_jar_with_deps(
            &mods_dir,
            "server-mod.jar",
            None,
            "server-mod",
            &[("client-dep", true)], // required dependency
        );

        let analysis = vec![
            (
                jar_d.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_d.path()),
            ),
            (
                jar_c.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_c.path()),
            ),
        ];

        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // D excluded
        assert_eq!(plan.exclude.len(), 1);
        assert_eq!(plan.exclude[0], jar_d.path());

        // C kept (universal)
        assert!(!plan.exclude.contains(&jar_c.path().to_path_buf()));

        // Conflict recorded: C requires D which was excluded
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].dependency_mod_id, "client-dep");
    }

    /// Missing dependency: required dep not in the build set.
    /// Diagnostic recorded; no crash.
    #[test]
    fn phase3c_missing_dependency_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // C depends on "nonexistent" which isn't in the set
        let jar_c = create_fabric_jar_with_deps(
            &mods_dir,
            "server-mod.jar",
            None,
            "server-mod",
            &[("nonexistent", true)],
        );

        let analysis = vec![(
            jar_c.path().to_path_buf(),
            crate::mod_compat::classify_mod_local(jar_c.path()),
        )];

        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // C kept
        assert!(plan.exclude.is_empty());
        // Missing dep recorded
        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan.missing[0].dependency_mod_id, "nonexistent");
    }

    /// Two JARs declare the same mod ID → ambiguity recorded,
    /// no arbitrary winner selected, no unrelated file deleted.
    #[test]
    fn phase3c_duplicate_provider_no_arbitrary_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let jar_1 = create_forge_jar(&mods_dir, "foo-v1.jar", "foo", None, &[]);
        let jar_2 = create_forge_jar(&mods_dir, "foo-v2.jar", "foo", None, &[]);
        // Third JAR depends on "foo"
        let jar_3 = create_fabric_jar_with_deps(
            &mods_dir,
            "consumer.jar",
            None,
            "consumer",
            &[("foo", true)],
        );

        let analysis = vec![
            (
                jar_1.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_1.path()),
            ),
            (
                jar_2.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_2.path()),
            ),
            (
                jar_3.path().to_path_buf(),
                crate::mod_compat::classify_mod_local(jar_3.path()),
            ),
        ];

        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // "foo" is ambiguous
        assert!(graph.is_ambiguous("foo"));
        assert_eq!(graph.ambiguous_ids().len(), 1);

        // No exclusions (all Unknown)
        assert!(plan.exclude.is_empty());

        // Ambiguous dependency recorded
        assert_eq!(plan.ambiguous.len(), 1);
        assert_eq!(plan.ambiguous[0].dependency_mod_id, "foo");
        assert_eq!(plan.ambiguous[0].provider_paths.len(), 2);

        // Both files still exist — no arbitrary deletion
        assert!(jar_1.path().exists());
        assert!(jar_2.path().exists());
        assert!(jar_3.path().exists());
    }

    /// Corrupt JAR → Unknown → kept, installer does not panic.
    #[test]
    fn phase3c_corrupt_jar_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // Write garbage bytes as a .jar
        let corrupt_path = mods_dir.join("corrupt.jar");
        std::fs::write(&corrupt_path, b"this is not a valid zip").unwrap();

        let compat = crate::mod_compat::classify_mod_local(&corrupt_path);
        // Corrupt → Unknown
        assert_eq!(compat.compatibility, ServerCompatibility::Unknown);

        let analysis = vec![(corrupt_path.clone(), compat)];
        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // Kept, not excluded
        assert!(plan.exclude.is_empty());
        assert!(corrupt_path.exists());
    }

    /// Hard-delete regression: excluded client mods must exist in quarantine,
    /// not disappear via remove_file().
    #[test]
    fn phase3c_hard_delete_regression() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        // Two client-only JARs
        let jar_a = create_fabric_jar(&mods_dir, "client-a.jar", Some("client"), "client-a");
        let jar_b = create_fabric_jar(&mods_dir, "client-b.jar", Some("client"), "client-b");
        // One universal JAR
        let jar_c = create_fabric_jar(&mods_dir, "universal-c.jar", None, "universal-c");

        let mut analysis = Vec::new();
        for entry in std::fs::read_dir(&mods_dir).unwrap().flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "jar") {
                let compat = crate::mod_compat::classify_mod_local(&entry.path());
                analysis.push((entry.path(), compat));
            }
        }

        let graph = DependencyGraph::build(&analysis);
        let plan = graph.create_plan();

        // Two excluded (client-only)
        assert_eq!(plan.exclude.len(), 2);

        // Simulate Phase 3C quarantine (rename, never delete)
        let quarantine_dir = tmp.path().join(".lbby-client-only-mods");
        std::fs::create_dir_all(&quarantine_dir).unwrap();
        for path in &plan.exclude {
            let file_name = path.file_name().unwrap().to_str().unwrap();
            let dest = quarantine_dir.join(file_name);
            std::fs::rename(path, &dest).unwrap();
            // Excluded file exists in quarantine
            assert!(
                dest.exists(),
                "Excluded file must exist in quarantine, not be deleted"
            );
        }

        // Universal mod still in mods/
        assert!(mods_dir.join("universal-c.jar").exists());

        // Quarantine has exactly the 2 excluded files
        let quarantine_count = std::fs::read_dir(&quarantine_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jar"))
            .count();
        assert_eq!(quarantine_count, 2);
    }
}
