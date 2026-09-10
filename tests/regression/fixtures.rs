// fixtures.rs — Shared mock types and builder helpers for Phase 3I regression suite.
//
// All fixtures are offline-safe, deterministic, and use temp directories.

use lbby_core::boot_failure_analyzer::InstalledFileRegistry;
use lbby_core::boot_validator::{
    BootFailure, BootFailureReason, BootResult, BootSuccess, BootTimeout,
};
use lbby_core::config::{ServerConfig, ServerType};
use lbby_core::dependency_resolver::CurseDependency;
use lbby_core::validation_orchestrator::{BootValidationRunner, ValidationContext};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// ── BootResult builders ──────────────────────────────────────────────

pub fn success_result() -> BootResult {
    BootResult::Success(BootSuccess {
        elapsed: Duration::from_secs(10),
        graceful_shutdown: true,
        log_tail: "Done! For help, type \"help\"".to_string(),
    })
}

pub fn failed_result(reason: BootFailureReason, log_tail: &str) -> BootResult {
    BootResult::Failed(BootFailure {
        exit_code: Some(1),
        reason,
        log_tail: log_tail.to_string(),
    })
}

pub fn timeout_result() -> BootResult {
    BootResult::Timeout(BootTimeout {
        waited: Duration::from_secs(120),
        log_tail: "Timed out".to_string(),
    })
}

// ── Log builders ─────────────────────────────────────────────────────

pub fn forge_missing_dep_log(missing_mod: &str) -> String {
    format!(
        "[12:00:00] [main/INFO]: Mod '{}' requires mod '{}' version 1.0.0 or later\n\
         [12:00:01] [main/ERROR]: Missing mandatory dependency '{}'",
        "create", missing_mod, missing_mod
    )
}

pub fn wrong_java_log() -> String {
    "java.lang.UnsupportedClassVersionError: net/minecraft/server/Main has been compiled \
     by a more recent version of the Java Runtime (class file version 65.0)"
        .to_string()
}

pub fn java_not_found_log() -> String {
    "Error: JAVA_HOME is not set and no 'java' command could be found in your PATH.\n\
     /bin/sh: 1: java: not found"
        .to_string()
}

pub fn oom_log() -> String {
    "java.lang.OutOfMemoryError: Java heap space".to_string()
}

pub fn loader_mismatch_forge_log() -> String {
    "[12:00:00] [main/INFO]: Forge 47.2.0 is required, but 43.2.0 is installed".to_string()
}

pub fn loader_mismatch_forge_ge_log() -> String {
    "[12:00:00] [main/INFO]: requires forge version 47.2.0 or above".to_string()
}

pub fn loader_mismatch_fabric_log() -> String {
    "[12:00:00] [main/INFO]: requires fabric-loader >=0.15.0".to_string()
}

pub fn loader_mismatch_quilt_log() -> String {
    "[12:00:00] [main/INFO]: requires quilt-loader >=0.22.0".to_string()
}

pub fn loader_mismatch_neoforge_log() -> String {
    "[12:00:00] [main/INFO]: NeoForge 21.1.0 is required".to_string()
}

pub fn unknown_crash_log() -> String {
    "Some random crash with no recognizable pattern".to_string()
}

pub fn combined_missing_dep_and_loader_log(missing_mod: &str) -> String {
    format!(
        "{}\n{}",
        forge_missing_dep_log(missing_mod),
        loader_mismatch_forge_log()
    )
}

// ── ServerConfig builders ────────────────────────────────────────────

pub fn make_forge_cfg(server_path: &str) -> ServerConfig {
    ServerConfig {
        server_path: server_path.to_string(),
        java_path: "/usr/bin/java".to_string(),
        minecraft_version: "1.20.1".to_string(),
        server_type: ServerType::Forge,
        loader_version: Some("47.2.0".to_string()),
        ram_mb: 4096,
        max_players: 20,
        server_name: "test".to_string(),
        eula_accepted: true,
        setup_complete: true,
        ..Default::default()
    }
}

pub fn make_fabric_cfg(server_path: &str) -> ServerConfig {
    ServerConfig {
        server_path: server_path.to_string(),
        java_path: "/usr/bin/java".to_string(),
        minecraft_version: "1.21.1".to_string(),
        server_type: ServerType::Fabric,
        loader_version: Some("0.16.14".to_string()),
        ram_mb: 4096,
        max_players: 20,
        server_name: "test".to_string(),
        eula_accepted: true,
        setup_complete: true,
        ..Default::default()
    }
}

pub fn make_neoforge_cfg(server_path: &str) -> ServerConfig {
    ServerConfig {
        server_path: server_path.to_string(),
        java_path: "/usr/bin/java".to_string(),
        minecraft_version: "1.21.1".to_string(),
        server_type: ServerType::NeoForge,
        loader_version: Some("21.1.0".to_string()),
        ram_mb: 4096,
        max_players: 20,
        server_name: "test".to_string(),
        eula_accepted: true,
        setup_complete: true,
        ..Default::default()
    }
}

// ── Mock BootValidationRunner ────────────────────────────────────────

/// A mock BootValidationRunner that returns scripted results in order.
/// Tracks call count for assertions.
pub struct MockBootValidator {
    results: Mutex<Vec<BootResult>>,
    call_count: AtomicUsize,
}

impl MockBootValidator {
    pub fn new(results: Vec<BootResult>) -> Self {
        Self {
            results: Mutex::new(results),
            call_count: AtomicUsize::new(0),
        }
    }
    pub fn calls(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl BootValidationRunner for MockBootValidator {
    fn run<'a>(
        &'a self,
        _cfg: &'a ServerConfig,
        _staging_path: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = BootResult> + Send + 'a>> {
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut results = self.results.lock().unwrap();
            if results.len() > 1 {
                results.remove(0)
            } else {
                results.first().cloned().unwrap_or_else(|| success_result())
            }
        })
    }
}

// ── Temp directory helpers ───────────────────────────────────────────

pub fn temp_server_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("lbby-regression").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn setup_live_server(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("server.properties"),
        "server-port=25565\nlevel-name=world\n",
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("ops.json"), "[]").unwrap();
    std::fs::write(dir.join("whitelist.json"), "[]").unwrap();
    std::fs::write(dir.join("banned-players.json"), "[]").unwrap();
    std::fs::write(dir.join("banned-ips.json"), "[]").unwrap();
    std::fs::write(dir.join("usercache.json"), "[]").unwrap();
    std::fs::create_dir_all(dir.join("world")).unwrap();
    std::fs::write(dir.join("world").join("level.dat"), b"fake-level-data").unwrap();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    std::fs::write(dir.join("mods").join("old-mod.jar"), b"old-mod").unwrap();
}

// ── Outcome extraction helpers ───────────────────────────────────────

pub fn outcome_history(
    outcome: &lbby_core::validation_orchestrator::ValidationOutcome,
) -> &lbby_core::validation_orchestrator::ValidationHistory {
    use lbby_core::validation_orchestrator::ValidationOutcome;
    match outcome {
        ValidationOutcome::Validated(s) => &s.history,
        ValidationOutcome::Failed(f) => &f.history,
        ValidationOutcome::UserActionRequired(r) => &r.history,
    }
}
