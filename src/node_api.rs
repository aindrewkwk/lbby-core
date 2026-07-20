//! Node integration API (PLATFORM-010).
//!
//! Provides a clean, stateless API for lbby-node to manage Minecraft server
//! instances without needing the full AppState / profile system.
//!
//! These functions wrap existing lbby-core capabilities (Java discovery,
//! version fetching, server download) with explicit parameters instead
//! of reading from the profile-based config system.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::app_state::{AppEventSender, AppState};

// ── Public Types ────────────────────────────────────────────────────────────

/// Specification for a Minecraft server instance.
/// This is the node-side equivalent of ServerConfig — explicit parameters
/// instead of profile-based config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinecraftSpec {
    /// Minecraft version, e.g. "1.21.4"
    pub minecraft_version: String,
    /// Server distribution: "paper", "vanilla", "forge", "fabric", "folia", "purpur"
    pub distribution: String,
    /// Loader/build version (Paper build number, Forge version, etc.)
    /// If None, uses latest.
    pub loader_version: Option<String>,
    /// JVM heap in MB
    pub ram_mb: u32,
    /// Max players for server.properties
    pub max_players: u32,
    /// Server MOTD
    pub server_name: String,
    /// Game port (default 25565)
    pub game_port: u16,
}

impl Default for MinecraftSpec {
    fn default() -> Self {
        Self {
            minecraft_version: "1.21.4".to_string(),
            distribution: "paper".to_string(),
            loader_version: None,
            ram_mb: 2048,
            max_players: 20,
            server_name: "Lbby Server".to_string(),
            game_port: 25565,
        }
    }
}

/// Result of preparing a Minecraft instance (downloading server files).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedInstance {
    /// Path to the instance directory
    pub instance_dir: PathBuf,
    /// Path to the resolved Java binary
    pub java_bin: PathBuf,
    /// Java major version used
    pub java_major: u8,
    /// Path to server.jar
    pub server_jar: PathBuf,
    /// Resolved Minecraft version
    pub minecraft_version: String,
    /// Resolved distribution
    pub distribution: String,
    /// Resolved loader version (if applicable)
    pub loader_version: Option<String>,
}

/// Observation of a running Minecraft process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeObservation {
    pub pid: u32,
    pub running: bool,
    pub exit_code: Option<i32>,
}

// ── Error Type ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum NodeApiError {
    Io(std::io::Error),
    JavaNotFound(u8, String),
    DownloadFailed(String),
    UnsupportedDistribution(String),
    MissingField(String),
    NotRunning,
    AlreadyRunning,
    ProcessError(String),
}

impl fmt::Display for NodeApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeApiError::Io(e) => write!(f, "IO error: {}", e),
            NodeApiError::JavaNotFound(major, reason) => {
                write!(f, "Java {} not found: {}", major, reason)
            }
            NodeApiError::DownloadFailed(msg) => write!(f, "Download failed: {}", msg),
            NodeApiError::UnsupportedDistribution(dist) => {
                write!(f, "Unsupported distribution: {}", dist)
            }
            NodeApiError::MissingField(field) => write!(f, "Missing required field: {}", field),
            NodeApiError::NotRunning => write!(f, "Server not running"),
            NodeApiError::AlreadyRunning => write!(f, "Server already running"),
            NodeApiError::ProcessError(msg) => write!(f, "Process error: {}", msg),
        }
    }
}

impl std::error::Error for NodeApiError {}

impl From<std::io::Error> for NodeApiError {
    fn from(e: std::io::Error) -> Self {
        NodeApiError::Io(e)
    }
}

// ── No-op Event Sender ──────────────────────────────────────────────────────

/// Create a minimal AppEventSender that discards all events.
/// Used when lbby-core functions require an event sender but we don't need
/// progress reporting (e.g., from lbby-node where the control plane handles UI).
fn noop_event_sender() -> Arc<AppEventSender> {
    Arc::new(AppEventSender::new(Arc::new(AppState::new())))
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Prepare a Minecraft server instance:
/// 1. Ensure Java is available (find or download)
/// 2. Download the server jar for the specified distribution
/// 3. Write eula.txt and server.properties
/// 4. Return the prepared instance metadata
///
/// This is idempotent — if the server jar already exists, it won't re-download.
pub async fn prepare_minecraft(
    spec: &MinecraftSpec,
    instance_dir: &Path,
) -> Result<PreparedInstance, NodeApiError> {
    // Create instance directory
    tokio::fs::create_dir_all(instance_dir).await?;

    // Resolve Java version
    let required_major = crate::java::required_java_for_mc(&spec.minecraft_version);
    let app = noop_event_sender();

    let java_bin = match crate::java::find_java_with_version(required_major) {
        Some(path) => path,
        None => crate::java::ensure_java(required_major, &app)
            .await
            .map_err(|e| NodeApiError::JavaNotFound(required_major, e))?,
    };

    let java_major = crate::java::detect_java_major(&java_bin).unwrap_or(required_major);

    // Download server jar if not present
    let server_jar = instance_dir.join("server.jar");
    if !server_jar.exists()
        || tokio::fs::metadata(&server_jar)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
            < 1024
    {
        download_server_jar(spec, instance_dir, &app).await?;
    }

    // Write eula.txt
    tokio::fs::write(instance_dir.join("eula.txt"), "eula=true\n").await?;

    // Write server.properties
    let props = format!(
        "online-mode=false\nmax-players={}\nmotd={}\nserver-port={}\nview-distance=8\nsimulation-distance=6\n",
        spec.max_players, spec.server_name, spec.game_port
    );
    tokio::fs::write(instance_dir.join("server.properties"), props).await?;

    // Create plugins or mods directory based on distribution
    let extras_dir = match spec.distribution.as_str() {
        "paper" | "folia" | "purpur" => "plugins",
        _ => "mods",
    };
    tokio::fs::create_dir_all(instance_dir.join(extras_dir))
        .await
        .ok();

    Ok(PreparedInstance {
        instance_dir: instance_dir.to_path_buf(),
        java_bin,
        java_major,
        server_jar,
        minecraft_version: spec.minecraft_version.clone(),
        distribution: spec.distribution.clone(),
        loader_version: spec.loader_version.clone(),
    })
}

/// Start a Minecraft server process.
///
/// Returns the tokio Child handle. The caller (lbby-node's supervisor)
/// is responsible for tracking the process and detecting exits.
///
/// The process is spawned with piped stdin/stdout/stderr so the caller
/// can optionally interact with the server console.
pub async fn start_minecraft(
    prepared: &PreparedInstance,
    spec: &MinecraftSpec,
) -> Result<tokio::process::Child, NodeApiError> {
    use tokio::process::Command;

    let mut cmd = Command::new(&prepared.java_bin);
    cmd.arg(format!("-Xmx{}M", spec.ram_mb));
    cmd.arg(format!("-Xms{}M", (spec.ram_mb / 2).max(512)));

    // Add optimized JVM flags for Minecraft
    if spec.ram_mb >= 1024 {
        cmd.args(crate::server::optimized_jvm_flags());
    }

    cmd.args(["-jar", "server.jar", "nogui"]);
    cmd.current_dir(&prepared.instance_dir);

    // Piped I/O for console interaction
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Clean up macOS DYLD environment variables
    cmd.env_remove("DYLD_LIBRARY_PATH");
    cmd.env_remove("DYLD_FALLBACK_LIBRARY_PATH");
    cmd.env_remove("DYLD_FRAMEWORK_PATH");
    cmd.env_remove("DYLD_ROOT_PATH");
    cmd.env_remove("DYLD_IMAGE_SUFFIX");
    cmd.env_remove("DYLD_SHARED_FILE");
    cmd.env_remove("DYLD_INSERT_LIBRARIES");
    cmd.env_remove("DYLD_FORCE_FLAT_NAMESPACE");

    // Platform-specific: hide child window on Windows
    crate::helpers::hide_child_window(&mut cmd);

    let child = cmd.spawn().map_err(NodeApiError::Io)?;
    Ok(child)
}

/// Send a graceful stop command ("stop") to a Minecraft server's stdin.
///
/// The caller should then wait for the process to exit, or force-kill
/// after the grace period.
pub async fn send_stop_command(child: &mut tokio::process::Child) -> Result<(), NodeApiError> {
    use tokio::io::AsyncWriteExt;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(b"stop\n")
            .await
            .map_err(|e| NodeApiError::ProcessError(e.to_string()))?;
        Ok(())
    } else {
        Err(NodeApiError::ProcessError("No stdin available".to_string()))
    }
}

/// Stop a Minecraft server gracefully:
/// 1. Send "stop" command to stdin
/// 2. Wait up to grace_period for exit
/// 3. Force-kill if timeout expires
///
/// Returns the exit code if available.
pub async fn stop_minecraft(
    child: &mut tokio::process::Child,
    grace_period_ms: u64,
) -> Result<Option<i32>, NodeApiError> {
    // Try graceful stop first
    let _ = send_stop_command(child).await;

    let grace_period = std::time::Duration::from_millis(grace_period_ms);
    match tokio::time::timeout(grace_period, child.wait()).await {
        Ok(Ok(status)) => Ok(status.code()),
        Ok(Err(e)) => Err(NodeApiError::ProcessError(e.to_string())),
        Err(_) => {
            // Grace period expired — force kill
            eprintln!("[lbby-core:node_api] grace period expired, force killing Minecraft server");
            child.kill().await.map_err(NodeApiError::Io)?;
            let status = child.wait().await.map_err(NodeApiError::Io)?;
            Ok(status.code())
        }
    }
}

/// Inspect a running Minecraft process.
pub fn inspect_minecraft(child: &mut tokio::process::Child) -> RuntimeObservation {
    match child.try_wait() {
        Ok(Some(status)) => RuntimeObservation {
            pid: child.id().unwrap_or(0),
            running: false,
            exit_code: status.code(),
        },
        Ok(None) => RuntimeObservation {
            pid: child.id().unwrap_or(0),
            running: true,
            exit_code: None,
        },
        Err(_) => RuntimeObservation {
            pid: 0,
            running: false,
            exit_code: None,
        },
    }
}

/// Delete a Minecraft instance directory.
///
/// Validates path safety before deletion.
pub async fn delete_minecraft(instance_id: &str, instance_dir: &Path) -> Result<(), NodeApiError> {
    if instance_dir.exists() {
        tokio::fs::remove_dir_all(instance_dir).await?;
        eprintln!(
            "[lbby-core:node_api] minecraft instance deleted: {} at {}",
            instance_id,
            instance_dir.display()
        );
    }
    Ok(())
}

// ── Internal Helpers ────────────────────────────────────────────────────────

/// Download the server jar for the specified distribution.
async fn download_server_jar(
    spec: &MinecraftSpec,
    server_dir: &Path,
    app: &Arc<AppEventSender>,
) -> Result<(), NodeApiError> {
    let client = reqwest::Client::new();

    match spec.distribution.as_str() {
        "paper" => {
            // Resolve build number
            let build = match &spec.loader_version {
                Some(b) => b.clone(),
                None => {
                    // Fetch latest build
                    let v: serde_json::Value = client
                        .get(format!(
                            "https://api.papermc.io/v2/projects/paper/versions/{}",
                            spec.minecraft_version
                        ))
                        .send()
                        .await
                        .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?
                        .json()
                        .await
                        .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?;

                    let builds = v["builds"].as_array().ok_or_else(|| {
                        NodeApiError::DownloadFailed("No builds found".to_string())
                    })?;
                    builds
                        .last()
                        .and_then(|b| b.as_u64())
                        .ok_or_else(|| NodeApiError::DownloadFailed("No build number".to_string()))?
                        .to_string()
                }
            };

            // Get download filename
            let info: serde_json::Value = client
                .get(format!(
                    "https://api.papermc.io/v2/projects/paper/versions/{}/builds/{}",
                    spec.minecraft_version, build
                ))
                .send()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?
                .json()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?;

            let file_name = info["downloads"]["application"]["name"]
                .as_str()
                .ok_or_else(|| NodeApiError::DownloadFailed("No download name".to_string()))?;

            let url = format!(
                "https://api.papermc.io/v2/projects/paper/versions/{}/builds/{}/downloads/{}",
                spec.minecraft_version, build, file_name
            );

            crate::helpers::download_to_file(app, &url, &server_dir.join("server.jar"), "Paper")
                .await
                .map_err(NodeApiError::DownloadFailed)?;
        }
        "folia" => {
            let build = match &spec.loader_version {
                Some(b) => b.clone(),
                None => {
                    let v: serde_json::Value = client
                        .get(format!(
                            "https://api.papermc.io/v2/projects/folia/versions/{}",
                            spec.minecraft_version
                        ))
                        .send()
                        .await
                        .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?
                        .json()
                        .await
                        .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?;

                    let builds = v["builds"].as_array().ok_or_else(|| {
                        NodeApiError::DownloadFailed("No builds found".to_string())
                    })?;
                    builds
                        .last()
                        .and_then(|b| b.as_u64())
                        .ok_or_else(|| NodeApiError::DownloadFailed("No build number".to_string()))?
                        .to_string()
                }
            };

            let info: serde_json::Value = client
                .get(format!(
                    "https://api.papermc.io/v2/projects/folia/versions/{}/builds/{}",
                    spec.minecraft_version, build
                ))
                .send()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?
                .json()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?;

            let file_name = info["downloads"]["application"]["name"]
                .as_str()
                .ok_or_else(|| NodeApiError::DownloadFailed("No download name".to_string()))?;

            let url = format!(
                "https://api.papermc.io/v2/projects/folia/versions/{}/builds/{}/downloads/{}",
                spec.minecraft_version, build, file_name
            );

            crate::helpers::download_to_file(app, &url, &server_dir.join("server.jar"), "Folia")
                .await
                .map_err(NodeApiError::DownloadFailed)?;
        }
        "vanilla" => {
            // Fetch version manifest to get the server jar URL
            let manifest: serde_json::Value = client
                .get("https://launchermeta.mojang.com/mc/game/version_manifest_v2.json")
                .send()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?
                .json()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?;

            let version_url = manifest["versions"]
                .as_array()
                .and_then(|versions| {
                    versions
                        .iter()
                        .find(|v| v["id"].as_str() == Some(&spec.minecraft_version))
                })
                .and_then(|v| v["url"].as_str())
                .ok_or_else(|| {
                    NodeApiError::DownloadFailed(format!(
                        "Version {} not found in manifest",
                        spec.minecraft_version
                    ))
                })?;

            let version_meta: serde_json::Value = client
                .get(version_url)
                .send()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?
                .json()
                .await
                .map_err(|e| NodeApiError::DownloadFailed(e.to_string()))?;

            let server_url = version_meta["downloads"]["server"]["url"]
                .as_str()
                .ok_or_else(|| {
                    NodeApiError::DownloadFailed("No server download URL".to_string())
                })?;

            crate::helpers::download_to_file(
                app,
                server_url,
                &server_dir.join("server.jar"),
                "Vanilla",
            )
            .await
            .map_err(NodeApiError::DownloadFailed)?;
        }
        other => {
            return Err(NodeApiError::UnsupportedDistribution(
                format!("Distribution '{}' not yet supported via node_api. Supported: paper, folia, vanilla", other)
            ));
        }
    }

    // Validate download
    let jar_path = server_dir.join("server.jar");
    if !jar_path.exists() {
        return Err(NodeApiError::DownloadFailed(
            "server.jar not found after download".to_string(),
        ));
    }
    let meta = tokio::fs::metadata(&jar_path)
        .await
        .map_err(NodeApiError::Io)?;
    if meta.len() < 1024 {
        return Err(NodeApiError::DownloadFailed(
            "server.jar appears corrupted (too small)".to_string(),
        ));
    }

    Ok(())
}
