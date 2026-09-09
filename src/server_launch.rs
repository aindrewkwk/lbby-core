// server_launch.rs — Shared server launch command builder.
//
// Single canonical source for constructing a Minecraft server launch command.
// Both the production runtime (`do_start_server`) and the boot validator
// (`BootValidator`) consume this to guarantee identical launcher behavior.
//
// Ownership boundary:
//   This module owns: Java version resolution, JVM flags, loader detection,
//   server JAR selection, loader arguments, DYLD/env removal.
//   This module does NOT own: PID management, console piping, auto-restart,
//   validation timeout, ready detection, validation properties.

use std::path::{Path, PathBuf};

use crate::config::{ServerConfig, ServerType};
use crate::forge::{detect_modloader_launch, ModLoaderKind, ModLoaderLaunch};

// ── Public types ────────────────────────────────────────────────────────

/// Pure data describing how to start a Minecraft server process.
/// Both production and validation consume this — no runtime-specific
/// behavior leaks into the struct.
#[derive(Debug, Clone)]
pub struct ServerLaunchCommand {
    /// The executable to run (java binary, bash for scripts, or cmd.exe on Windows).
    pub executable: PathBuf,
    /// Command-line arguments (JVM flags, -jar, server.jar, nogui, etc.).
    pub args: Vec<String>,
    /// Environment variables to SET before spawning (e.g. JAVA_HOME, PATH).
    pub env_set: Vec<(String, String)>,
    /// Environment variables to REMOVE before spawning (DYLD_* on macOS).
    pub env_remove: Vec<String>,
}

/// The loader-specific launch strategy detected from the server directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchStrategy {
    /// Script-based launch (run.sh / run.bat) — used by modern Forge/NeoForge.
    Script,
    /// Direct jar launch (java -jar <jar> nogui) — used by older Forge,
    /// Fabric, Vanilla, Paper, Purpur, etc.
    DirectJar,
}

// ── Constants ───────────────────────────────────────────────────────────

/// DYLD environment variables that must be removed on macOS to prevent
/// library injection attacks and compatibility issues.
const DYLD_ENV_VARS: &[&str] = &[
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_ROOT_PATH",
    "DYLD_IMAGE_SUFFIX",
    "DYLD_SHARED_FILE",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_FORCE_FLAT_NAMESPACE",
];

// ── Shared builder ──────────────────────────────────────────────────────

/// Build a `ServerLaunchCommand` from config and a pre-resolved Java binary.
///
/// The caller is responsible for resolving `java_bin` (production may download;
/// validation just fails). This function handles everything else:
/// - JVM flag construction (heap size, optimized flags)
/// - Forge/NeoForge launcher detection via `detect_modloader_launch`
/// - Fabric/Vanilla server.jar selection
/// - DYLD env removal (macOS safety)
/// - JAVA_HOME derivation for script-based launchers
///
/// Returns Err if required loader version info is missing or launcher
/// detection fails.
pub fn build_server_launch_command(
    cfg: &ServerConfig,
    server_dir: &Path,
    java_bin: &Path,
) -> Result<ServerLaunchCommand, String> {
    let java_home = crate::java::java_home_from_bin(java_bin);
    let ram = if cfg.ram_mb < 512 { 4096 } else { cfg.ram_mb };

    let mut args: Vec<String> = Vec::new();
    let mut env_set: Vec<(String, String)> = Vec::new();
    let executable: PathBuf;

    match cfg.server_type {
        ServerType::Forge | ServerType::NeoForge => {
            let (kind, version_key) = if cfg.server_type == ServerType::Forge {
                (
                    ModLoaderKind::Forge,
                    format!(
                        "{}-{}",
                        cfg.minecraft_version,
                        cfg.loader_version.as_deref().ok_or("No Forge version")?
                    ),
                )
            } else {
                (
                    ModLoaderKind::NeoForge,
                    cfg.loader_version
                        .as_deref()
                        .ok_or("No NeoForge version")?
                        .to_string(),
                )
            };

            match detect_modloader_launch(server_dir, kind, &version_key)? {
                ModLoaderLaunch::Script(_) => {
                    // Script-based launch (run.sh / run.bat)
                    #[cfg(target_os = "windows")]
                    {
                        executable = PathBuf::from("cmd");
                        args.extend(["/c".into(), "run.bat".into(), "nogui".into()]);
                    }
                    #[cfg(not(target_os = "windows"))]
                    {
                        executable = PathBuf::from("/bin/bash");
                        args.extend(["-c".into(), "./run.sh nogui".into()]);
                    }
                    if let Some(jh) = &java_home {
                        env_set.push(("JAVA_HOME".into(), jh.display().to_string()));
                        let bin_dir = jh.join("bin");
                        let sep = if cfg!(windows) { ";" } else { ":" };
                        let new_path = match std::env::var("PATH") {
                            Ok(p) => format!("{}{}{}", bin_dir.display(), sep, p),
                            Err(_) => bin_dir.display().to_string(),
                        };
                        env_set.push(("PATH".into(), new_path));
                    }
                }
                ModLoaderLaunch::LegacyJar(jar) => {
                    executable = java_bin.to_path_buf();
                    args.push(format!("-Xmx{}M", ram));
                    args.push(format!("-Xms{}M", (ram / 2).max(512)));
                    if cfg.optimized_jvm_flags {
                        for flag in crate::server::optimized_jvm_flags() {
                            args.push(flag.to_string());
                        }
                    }
                    args.extend([
                        "-jar".into(),
                        jar.to_string_lossy().to_string(),
                        "nogui".into(),
                    ]);
                }
            }
        }
        _ => {
            // Fabric, Vanilla, Paper, Purpur, Folia, etc.
            // Canonical jar name: "server.jar"
            // - Fabric installer (install_fabric) downloads to server.jar
            // - Vanilla installer downloads to server.jar
            // - Paper/Purpur download to server.jar
            // - BuildTools copies to server.jar
            executable = java_bin.to_path_buf();
            args.push(format!("-Xmx{}M", ram));
            args.push(format!("-Xms{}M", (ram / 2).max(512)));
            if cfg.optimized_jvm_flags {
                for flag in crate::server::optimized_jvm_flags() {
                    args.push(flag.to_string());
                }
            }
            args.extend(["-jar".into(), "server.jar".into(), "nogui".into()]);
        }
    }

    // DYLD cleanup — REMOVE semantics (not set-to-empty).
    // Libraries check for variable existence, not just value.
    let env_remove: Vec<String> = DYLD_ENV_VARS.iter().map(|s| s.to_string()).collect();

    Ok(ServerLaunchCommand {
        executable,
        args,
        env_set,
        env_remove,
    })
}

/// Determine the required Java major version for the given server config.
///
/// This is the shared version-resolution logic used by both production
/// (which may download) and validation (which just fails).
pub fn required_java_major(cfg: &ServerConfig) -> u8 {
    let server_type_str = format!("{:?}", cfg.server_type);
    crate::java::required_java_for_mc_with_loader(&cfg.minecraft_version, Some(&server_type_str))
}

/// Determine the launch strategy (script vs direct jar) for the given config
/// and server directory. Returns None for non-Forge/NeoForge types (always
/// direct jar).
pub fn detect_launch_strategy(
    cfg: &ServerConfig,
    server_dir: &Path,
) -> Result<Option<LaunchStrategy>, String> {
    match cfg.server_type {
        ServerType::Forge | ServerType::NeoForge => {
            let (kind, version_key) = if cfg.server_type == ServerType::Forge {
                (
                    ModLoaderKind::Forge,
                    format!(
                        "{}-{}",
                        cfg.minecraft_version,
                        cfg.loader_version.as_deref().ok_or("No Forge version")?
                    ),
                )
            } else {
                (
                    ModLoaderKind::NeoForge,
                    cfg.loader_version
                        .as_deref()
                        .ok_or("No NeoForge version")?
                        .to_string(),
                )
            };
            match detect_modloader_launch(server_dir, kind, &version_key)? {
                ModLoaderLaunch::Script(_) => Ok(Some(LaunchStrategy::Script)),
                ModLoaderLaunch::LegacyJar(_) => Ok(Some(LaunchStrategy::DirectJar)),
            }
        }
        _ => Ok(None), // Non-Forge types always use direct jar
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerConfig, ServerType};

    /// Helper: minimal ServerConfig for testing.
    fn test_cfg(
        server_type: ServerType,
        mc_version: &str,
        loader_version: Option<&str>,
    ) -> ServerConfig {
        ServerConfig {
            server_path: "/tmp/test-server".into(),
            java_path: String::new(),
            minecraft_version: mc_version.into(),
            server_type,
            curseforge_api_key: None,
            loader_version: loader_version.map(|s| s.into()),
            ram_mb: 4096,
            max_players: 20,
            server_name: "Test".into(),
            minecraft_seed: String::new(),
            setup_complete: true,
            auto_restart: true,
            scheduled_restart_hours: 0,
            backup_interval_minutes: 0,
            backup_dir: String::new(),
            backup_include_logs: false,
            optimized_jvm_flags: true,
            performance_preset: "balanced".into(),
            remote_control_enabled: false,
            remote_control_port: 47992,
            remote_control_token: String::new(),
            remote_control_public_url: String::new(),
            cloudflare_remote_enabled: false,
            terraria_version: String::new(),
            tmodloader_version: String::new(),
            terraria_difficulty: 0,
            terraria_world_size: 2,
            terraria_seed: String::new(),
            terraria_evil: 0,
            terraria_password: String::new(),
            tmod_modpath: String::new(),
            tmod_modpack: String::new(),
            dashboard_url: "https://web.lbby.net".into(),
            app_token: String::new(),
            online_mode: None,
            heartbeat_interval_secs: 300,
            eula_accepted: false,
        }
    }

    #[test]
    fn vanilla_uses_server_jar() {
        let cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert_eq!(cmd.executable, java);
        assert!(cmd.args.contains(&"server.jar".to_string()));
        assert!(cmd.args.contains(&"nogui".to_string()));
        assert!(cmd.args.contains(&"-jar".to_string()));
        // Should have DYLD removals
        assert!(!cmd.env_remove.is_empty());
    }

    #[test]
    fn fabric_uses_server_jar() {
        // Fabric canonical path: install_fabric downloads to server.jar
        let cfg = test_cfg(ServerType::Fabric, "1.21.1", Some("0.16.0"));
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(cmd.args.contains(&"server.jar".to_string()));
        assert!(cmd.args.contains(&"-jar".to_string()));
    }

    #[test]
    fn optimized_jvm_flags_included_when_enabled() {
        let cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(cmd.args.contains(&"-XX:+UseG1GC".to_string()));
    }

    #[test]
    fn optimized_jvm_flags_excluded_when_disabled() {
        let mut cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        cfg.optimized_jvm_flags = false;
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(!cmd.args.contains(&"-XX:+UseG1GC".to_string()));
    }

    #[test]
    fn dyld_vars_in_env_remove() {
        let cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(cmd.env_remove.contains(&"DYLD_LIBRARY_PATH".to_string()));
        assert!(cmd
            .env_remove
            .contains(&"DYLD_INSERT_LIBRARIES".to_string()));
        assert_eq!(cmd.env_remove.len(), 8);
    }

    #[test]
    fn forge_requires_version() {
        let cfg = test_cfg(ServerType::Forge, "1.20.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let result = build_server_launch_command(&cfg, dir.path(), &java);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No Forge version"));
    }

    #[test]
    fn neoforge_requires_version() {
        let cfg = test_cfg(ServerType::NeoForge, "1.21.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let result = build_server_launch_command(&cfg, dir.path(), &java);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No NeoForge version"));
    }

    #[test]
    fn required_java_major_mc121() {
        let cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        assert_eq!(required_java_major(&cfg), 21);
    }

    #[test]
    fn required_java_major_mc120() {
        let cfg = test_cfg(ServerType::Vanilla, "1.20.1", None);
        assert_eq!(required_java_major(&cfg), 17);
    }

    #[test]
    fn required_java_major_fabric_same_as_vanilla() {
        let vanilla = test_cfg(ServerType::Vanilla, "1.21.1", None);
        let fabric = test_cfg(ServerType::Fabric, "1.21.1", Some("0.16.0"));
        assert_eq!(required_java_major(&vanilla), required_java_major(&fabric));
    }

    #[test]
    fn ram_default_when_zero() {
        let mut cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        cfg.ram_mb = 0;
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(cmd.args.contains(&"-Xmx4096M".to_string()));
    }

    #[test]
    fn ram_respected_when_set() {
        let mut cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        cfg.ram_mb = 8192;
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(cmd.args.contains(&"-Xmx8192M".to_string()));
    }

    #[test]
    fn non_forge_always_direct_jar_strategy() {
        let types = [ServerType::Vanilla, ServerType::Fabric, ServerType::Paper];
        for st in &types {
            let cfg = test_cfg(st.clone(), "1.21.1", None);
            let dir = tempfile::tempdir().unwrap();
            let result = detect_launch_strategy(&cfg, dir.path()).unwrap();
            assert_eq!(result, None, "{:?} should be None (always direct jar)", st);
        }
    }

    #[test]
    fn fabric_install_produces_server_jar() {
        // This test documents the canonical Fabric install behavior.
        // install_fabric() in server.rs downloads from meta.fabricmc.net
        // and saves to server.jar. There is no fabric-server-launch.jar
        // in Lbby's installation pipeline.
        //
        // If this invariant ever changes, both build_server_launch_command
        // and this test must be updated together.
        let cfg = test_cfg(ServerType::Fabric, "1.20.1", Some("0.16.0"));
        let dir = tempfile::tempdir().unwrap();
        // Create the canonical server.jar that install_fabric produces
        std::fs::write(dir.path().join("server.jar"), "fake").unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(
            cmd.args.iter().any(|a| a == "server.jar"),
            "Fabric must use server.jar — install_fabric downloads to server.jar"
        );
        assert!(
            !cmd.args.iter().any(|a| a.contains("fabric-server-launch")),
            "fabric-server-launch.jar is NOT produced by Lbby's install pipeline"
        );
    }

    #[test]
    fn no_dyld_set_to_empty() {
        // Verify we REMOVE DYLD vars, not set them to empty string.
        // Setting to "" is NOT equivalent — libraries check existence.
        let cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        for (k, v) in &cmd.env_set {
            assert!(
                !k.starts_with("DYLD_"),
                "DYLD vars must be in env_remove, not env_set. Found {}={}",
                k,
                v
            );
        }
    }

    #[test]
    fn paper_uses_server_jar() {
        let cfg = test_cfg(ServerType::Paper, "1.21.1", Some("100"));
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/usr/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert!(cmd.args.contains(&"server.jar".to_string()));
    }

    #[test]
    fn executable_is_java_bin() {
        let cfg = test_cfg(ServerType::Vanilla, "1.21.1", None);
        let dir = tempfile::tempdir().unwrap();
        let java = PathBuf::from("/opt/java/bin/java");
        let cmd = build_server_launch_command(&cfg, dir.path(), &java).unwrap();
        assert_eq!(cmd.executable, java);
    }
}
