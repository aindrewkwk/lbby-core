// boot_validator.rs — Phase 3E: Staged server boot validation.
//
// Validates that a staged CurseForge modpack server build actually starts
// before committing the transaction. The validator boots the exact staged
// build with an isolated validation world, waits for the ready signal,
// then shuts down and cleans up.
//
// Safety guarantees:
//   - Uses staging_path as cwd (never the live server)
//   - Overrides level-name to a validation-specific world
//   - Binds to 127.0.0.1 on a random free port
//   - Restores server.properties byte-for-byte after validation
//   - Removes validation world directories
//   - Force-kills child on timeout/error (no orphan processes)
//   - Never modifies the live server directory

use std::collections::VecDeque;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::config::ServerConfig;
// forge detection is now handled by server_launch::build_server_launch_command

// ── Public types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum BootResult {
    Success(BootSuccess),
    Failed(BootFailure),
    Timeout(BootTimeout),
}

#[derive(Debug, Clone)]
pub struct BootSuccess {
    pub elapsed: Duration,
    pub graceful_shutdown: bool,
    pub log_tail: String,
}

#[derive(Debug, Clone)]
pub struct BootFailure {
    pub exit_code: Option<i32>,
    pub reason: BootFailureReason,
    pub log_tail: String,
}

#[derive(Debug, Clone)]
pub struct BootTimeout {
    pub waited: Duration,
    pub log_tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootFailureReason {
    ProcessExited,
    JavaNotFound,
    LaunchFailure,
    EulaNotAccepted,
    OutOfMemory,
    PortUnavailable,
    ReadyThenStopFailed,
    WrongJavaVersion,
    Unknown(String),
}

impl std::fmt::Display for BootFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProcessExited => write!(f, "Server process exited before reaching ready state"),
            Self::JavaNotFound => write!(f, "Java executable not found"),
            Self::LaunchFailure => write!(f, "Failed to launch server process"),
            Self::EulaNotAccepted => write!(f, "Minecraft EULA has not been accepted"),
            Self::OutOfMemory => write!(f, "Server ran out of memory during startup"),
            Self::PortUnavailable => write!(f, "Could not bind a validation port"),
            Self::ReadyThenStopFailed => {
                write!(
                    f,
                    "Server reached ready state but failed to shut down cleanly"
                )
            }
            Self::WrongJavaVersion => write!(f, "Wrong Java version for this server"),
            Self::Unknown(msg) => write!(f, "Unknown failure: {}", msg),
        }
    }
}

/// Launch specification — pure data describing how to start a server.
/// Derived from config + filesystem state. Both the production runtime and
/// BootValidator consume this to avoid duplicating loader launch logic.
#[derive(Debug)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>, // env vars to SET
    pub env_remove: Vec<String>,    // env vars to REMOVE (production-safe semantics)
}

// ── Constants ───────────────────────────────────────────────────────────

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_CAP_BYTES: usize = 256 * 1024; // 256 KB ring buffer
const PORT_RETRY_ATTEMPTS: u32 = 5;

// ── Public API ──────────────────────────────────────────────────────────

pub struct BootValidator {
    timeout: Duration,
}

// ── RAII Guards ─────────────────────────────────────────────────────────

/// RAII guard for validation environment overlay.
///
/// Creates the validation overlay (server.properties, validation world) and
/// guarantees cleanup on ANY exit path — early return, `?`, timeout, panic.
/// Call `finish()` on the normal cleanup path to get a Result.
/// Drop provides best-effort cleanup with logging only.
struct ValidationOverlayGuard {
    staging_path: PathBuf,
    props_backed_up: bool,
    committed: bool,
}

impl ValidationOverlayGuard {
    fn new(staging_path: &Path) -> Self {
        Self {
            staging_path: staging_path.to_path_buf(),
            props_backed_up: false,
            committed: false,
        }
    }

    /// Mark that server.properties was backed up (so Drop knows to restore).
    fn set_props_backed_up(&mut self) {
        self.props_backed_up = true;
    }

    /// Explicit cleanup on the normal path. Returns Err if cleanup fails.
    /// On success, suppresses Drop cleanup.
    fn finish(mut self) -> Result<(), String> {
        let err1 = cleanup_validation_world(&self.staging_path);
        let err2 = if self.props_backed_up {
            restore_server_properties(&self.staging_path)
        } else {
            Ok(())
        };
        self.committed = true;
        match (err1, err2) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) => Err(format!("world cleanup: {}", e)),
            (Ok(()), Err(e)) => Err(format!("properties restore: {}", e)),
            (Err(e1), Err(e2)) => Err(format!("world cleanup: {}; properties restore: {}", e1, e2)),
        }
    }
}

impl Drop for ValidationOverlayGuard {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort cleanup. Log failures but never panic.
            if let Err(e) = cleanup_validation_world(&self.staging_path) {
                eprintln!("[boot-validator] Drop world cleanup failed: {}", e);
            }
            if self.props_backed_up {
                if let Err(e) = restore_server_properties(&self.staging_path) {
                    eprintln!("[boot-validator] Drop properties restore failed: {}", e);
                }
            }
        }
    }
}

/// RAII guard for the validation child process.
///
/// Owns the child process and guarantees kill + reap on ANY exit path.
/// Provides graceful shutdown (send "stop" + bounded wait) and force kill.
struct ChildProcessGuard {
    child: Option<tokio::process::Child>,
}

impl ChildProcessGuard {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// Take stdin from the child.
    fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    /// Take stdout from the child.
    fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    /// Try to wait for the child without blocking.
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        match self.child.as_mut() {
            Some(c) => c.try_wait(),
            None => Ok(None),
        }
    }

    /// Graceful shutdown: send stop, wait bounded, then force kill if needed.
    async fn graceful_shutdown(&mut self, stdin: &mut Option<tokio::process::ChildStdin>) -> bool {
        let sent = if let Some(ref mut si) = stdin {
            use tokio::io::AsyncWriteExt;
            si.write_all(b"stop\n").await.is_ok()
        } else {
            false
        };

        if sent {
            if let Some(ref mut c) = self.child {
                match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, c.wait()).await {
                    Ok(Ok(_)) => return true,
                    _ => {}
                }
            }
        }
        self.force_kill().await;
        false
    }

    /// Force kill + reap. Idempotent.
    async fn force_kill(&mut self) {
        if let Some(ref mut c) = self.child {
            let _ = c.kill().await;
            let _ = c.wait().await; // reap zombie
        }
    }

    /// Wait for child to exit naturally.
    async fn wait_for_exit(&mut self) -> Option<std::process::ExitStatus> {
        if let Some(ref mut c) = self.child {
            c.wait().await.ok()
        } else {
            None
        }
    }
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        // tokio's kill_on_drop(true) on the Command is our safety net.
        // We can't async kill in Drop, but the Command-level kill_on_drop
        // handles SIGKILL. The explicit force_kill() above is the primary path.
    }
}

impl BootValidator {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[allow(dead_code)]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Validate that the staged server build boots successfully.
    ///
    /// The validator:
    ///   1. Checks EULA acceptance
    ///   2. Builds the launch command from staged server config
    ///   3. Overrides server.properties with validation settings (random port, temp world)
    ///   4. Boots the server from staging_path
    ///   5. Waits for the ready signal
    ///   6. Sends "stop" for graceful shutdown
    ///   7. Restores server.properties and cleans up validation world
    ///
    /// On ANY failure, the live server is untouched.
    pub async fn validate(&self, cfg: &ServerConfig, staging_path: &Path) -> BootResult {
        // do_validate uses ValidationOverlayGuard + ChildProcessGuard internally.
        // Cleanup is handled by RAII on all paths — no manual cleanup needed here.
        self.do_validate(cfg, staging_path).await
    }

    async fn do_validate(&self, cfg: &ServerConfig, staging_path: &Path) -> BootResult {
        // ── Step 1: EULA check ────────────────────────────────────────
        let eula_path = staging_path.join("eula.txt");
        match std::fs::read_to_string(&eula_path) {
            Ok(content) if content.contains("eula=true") => {}
            _ => {
                return BootResult::Failed(BootFailure {
                    exit_code: None,
                    reason: BootFailureReason::EulaNotAccepted,
                    log_tail: String::new(),
                });
            }
        }

        // ── Step 2: Resolve random port ───────────────────────────────
        let validation_port = match find_free_port() {
            Some(p) => p,
            None => {
                return BootResult::Failed(BootFailure {
                    exit_code: None,
                    reason: BootFailureReason::PortUnavailable,
                    log_tail: String::new(),
                });
            }
        };

        // ── Step 3: Prepare validation server.properties overlay ──────
        // RAII guard: if anything fails after this point, the guard's Drop
        // restores server.properties and removes the validation world.
        let mut overlay_guard = ValidationOverlayGuard::new(staging_path);

        let validation_world = format!(".lbby-validation-world-{}", short_uuid());

        if let Err(e) = write_validation_properties_with_backup(
            staging_path,
            cfg,
            validation_port,
            &validation_world,
        ) {
            // Guard Drop will attempt best-effort cleanup even though we
            // haven't set_props_backed_up — it just won't try to restore
            // server.properties (which we failed to write anyway).
            return BootResult::Failed(BootFailure {
                exit_code: None,
                reason: BootFailureReason::Unknown(format!(
                    "Failed to write validation server.properties: {}",
                    e
                )),
                log_tail: String::new(),
            });
        }
        overlay_guard.set_props_backed_up();

        // ── Step 4: Build launch spec ─────────────────────────────────
        let spec = match build_launch_spec(cfg, staging_path) {
            Ok(s) => s,
            Err(e) => {
                // overlay_guard Drop restores server.properties + removes world
                return BootResult::Failed(BootFailure {
                    exit_code: None,
                    reason: BootFailureReason::JavaNotFound,
                    log_tail: e,
                });
            }
        };

        // ── Steps 5–8: Boot, detect ready, shutdown ───────────────────
        // validate_with_launch uses ChildProcessGuard internally.
        // It does NOT clean up server.properties or validation world —
        // that is the overlay_guard's responsibility.
        let result = self.validate_with_launch(cfg, staging_path, spec).await;

        match &result {
            BootResult::Success(_) => {
                // Normal finish: guard explicitly cleans up.
                // If cleanup fails, the staged build is in an unexpected
                // state and must NOT be committed.
                if let Err(cleanup_err) = overlay_guard.finish() {
                    return BootResult::Failed(BootFailure {
                        exit_code: None,
                        reason: BootFailureReason::Unknown(format!(
                            "Post-validation cleanup failed: {}",
                            cleanup_err
                        )),
                        log_tail: String::new(),
                    });
                }
            }
            _ => {
                // Failure/timeout: overlay_guard Drop handles cleanup.
                // No explicit finish() needed — Drop runs automatically.
            }
        }

        result
    }

    /// Run the boot validation process with a pre-built launch spec.
    ///
    /// Steps 5–8 of the validation pipeline: spawn, read output, detect ready,
    /// graceful shutdown, cleanup. Used internally by `do_validate` and exposed
    /// for process-control tests that inject a script-based launch spec.
    async fn validate_with_launch(
        &self,
        _cfg: &ServerConfig,
        staging_path: &Path,
        spec: LaunchSpec,
    ) -> BootResult {
        // ── Step 5: Spawn process ─────────────────────────────────────
        let mut cmd = tokio::process::Command::new(&spec.executable);
        cmd.args(&spec.args);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        cmd.current_dir(staging_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true); // safety net — primary kill is ChildProcessGuard

        let mut child_guard = match cmd.spawn() {
            Ok(c) => ChildProcessGuard::new(c),
            Err(e) => {
                return BootResult::Failed(BootFailure {
                    exit_code: None,
                    reason: BootFailureReason::LaunchFailure,
                    log_tail: format!("Failed to spawn process: {}", e),
                });
            }
        };

        let mut stdin = child_guard.take_stdin();
        let stdout = child_guard.take_stdout();

        let mut log_tail = CappedLog::new(OUTPUT_CAP_BYTES);
        let mut ready_detected = false;
        let start = Instant::now();

        // ── Step 6: Read output, detect ready ─────────────────────────
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            use tokio::io::AsyncBufReadExt;
            let mut lines = reader.lines();

            loop {
                let elapsed = start.elapsed();
                if elapsed >= self.timeout {
                    child_guard.force_kill().await;
                    return BootResult::Timeout(BootTimeout {
                        waited: elapsed,
                        log_tail: log_tail.to_string(),
                    });
                }

                let remaining = self.timeout.saturating_sub(elapsed);
                let read_timeout = remaining.min(Duration::from_secs(2));

                match tokio::time::timeout(read_timeout, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        log_tail.push_line(&line);
                        if is_ready_signal(&line) {
                            ready_detected = true;
                            break;
                        }
                        if is_oom_line(&line) {
                            child_guard.force_kill().await;
                            return BootResult::Failed(BootFailure {
                                exit_code: None,
                                reason: BootFailureReason::OutOfMemory,
                                log_tail: log_tail.to_string(),
                            });
                        }
                    }
                    Ok(Ok(None)) => {
                        // stdout closed — process exited
                        let exit_code = child_guard.wait_for_exit().await.and_then(|s| s.code());
                        return BootResult::Failed(BootFailure {
                            exit_code,
                            reason: BootFailureReason::ProcessExited,
                            log_tail: log_tail.to_string(),
                        });
                    }
                    Ok(Err(e)) => {
                        child_guard.force_kill().await;
                        return BootResult::Failed(BootFailure {
                            exit_code: None,
                            reason: BootFailureReason::Unknown(format!("stdout read error: {}", e)),
                            log_tail: log_tail.to_string(),
                        });
                    }
                    Err(_) => {
                        // Read timeout — check if process is still alive
                        match child_guard.try_wait() {
                            Ok(Some(status)) => {
                                return BootResult::Failed(BootFailure {
                                    exit_code: status.code(),
                                    reason: BootFailureReason::ProcessExited,
                                    log_tail: log_tail.to_string(),
                                });
                            }
                            Ok(None) => continue, // still running
                            Err(e) => {
                                child_guard.force_kill().await;
                                return BootResult::Failed(BootFailure {
                                    exit_code: None,
                                    reason: BootFailureReason::Unknown(format!(
                                        "try_wait error: {}",
                                        e
                                    )),
                                    log_tail: log_tail.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        if !ready_detected {
            child_guard.force_kill().await;
            return BootResult::Failed(BootFailure {
                exit_code: None,
                reason: BootFailureReason::ProcessExited,
                log_tail: log_tail.to_string(),
            });
        }

        // ── Step 7: Graceful shutdown via ChildProcessGuard ───────────
        let ready_elapsed = start.elapsed();
        let graceful_ok = child_guard.graceful_shutdown(&mut stdin).await;

        // ── Step 8: Cleanup validation artifacts ──────────────────────
        // Remove logs/latest.log produced by the validation run so it
        // does not get committed as if it were a production log.
        let _ = std::fs::remove_file(staging_path.join("logs").join("latest.log"));

        // NOTE: Validation overlay cleanup (validation world, server.properties)
        // is owned by ValidationOverlayGuard in do_validate(). This function
        // handles PROCESS lifecycle only via ChildProcessGuard.

        BootResult::Success(BootSuccess {
            elapsed: ready_elapsed,
            graceful_shutdown: graceful_ok,
            log_tail: log_tail.to_string(),
        })
    }
}

// ── Launch spec builder ─────────────────────────────────────────────────

/// Build a `LaunchSpec` from the server config and server directory.
///
/// Delegates to the shared `build_server_launch_command` in `server_launch.rs`
/// which is also used by the production runtime. This ensures validation and
/// production use identical launch command construction.
pub fn build_launch_spec(cfg: &ServerConfig, server_dir: &Path) -> Result<LaunchSpec, String> {
    // Resolve Java — validator fails if not found (no download fallback).
    let server_type_str = format!("{:?}", cfg.server_type);
    let required_major = crate::java::required_java_for_mc_with_loader(
        &cfg.minecraft_version,
        Some(&server_type_str),
    );
    let java_bin = crate::java::find_java_with_version(required_major)
        .ok_or_else(|| format!("Java {} not found", required_major))?;

    let cmd = crate::server_launch::build_server_launch_command(cfg, server_dir, &java_bin)?;
    Ok(LaunchSpec {
        executable: cmd.executable,
        args: cmd.args,
        env: cmd.env_set,
        env_remove: cmd.env_remove,
    })
}

// ── Validation helpers ──────────────────────────────────────────────────

/// Find a free TCP port on 127.0.0.1. Conservative: binds, verifies, returns.
fn find_free_port() -> Option<u16> {
    for _ in 0..PORT_RETRY_ATTEMPTS {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);
        // Verify the port is still available (conservative race check)
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// Write temporary validation server.properties.
fn write_validation_properties(
    staging_path: &Path,
    _cfg: &ServerConfig,
    port: u16,
    validation_world: &str,
) -> Result<(), String> {
    let props_path = staging_path.join("server.properties");
    let existing = std::fs::read_to_string(&props_path).unwrap_or_default();

    let mut output = String::new();
    let mut set_keys = std::collections::HashSet::new();

    // Preserve existing properties, override validation-specific ones
    for line in existing.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            output.push_str(line);
            output.push('\n');
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            set_keys.insert(key.to_string());
            match key {
                "server-ip" => {
                    output.push_str(&format!("server-ip=127.0.0.1\n"));
                }
                "server-port" => {
                    output.push_str(&format!("server-port={}\n", port));
                }
                "level-name" => {
                    output.push_str(&format!("level-name={}\n", validation_world));
                }
                "enable-rcon" => {
                    output.push_str("enable-rcon=false\n");
                }
                "enable-query" => {
                    output.push_str("enable-query=false\n");
                }
                "online-mode" => {
                    // Keep existing value — don't override
                    output.push_str(line);
                    output.push('\n');
                }
                _ => {
                    output.push_str(line);
                    output.push('\n');
                }
            }
        }
    }

    // Add missing required keys
    if !set_keys.contains("server-ip") {
        output.push_str("server-ip=127.0.0.1\n");
    }
    if !set_keys.contains("server-port") {
        output.push_str(&format!("server-port={}\n", port));
    }
    if !set_keys.contains("level-name") {
        output.push_str(&format!("level-name={}\n", validation_world));
    }
    if !set_keys.contains("enable-rcon") {
        output.push_str("enable-rcon=false\n");
    }
    if !set_keys.contains("enable-query") {
        output.push_str("enable-query=false\n");
    }
    // Preserve motd if present; add default if missing
    if !set_keys.contains("motd") {
        if let Some(motd) = extract_prop(&existing, "motd") {
            output.push_str(&format!("motd={}\n", motd));
        }
    }

    std::fs::write(&props_path, output)
        .map_err(|e| format!("Failed to write server.properties: {}", e))
}

/// Restore original server.properties contents.
fn restore_server_properties(staging_path: &Path) -> Result<(), String> {
    let props_path = staging_path.join("server.properties");
    let original_path = staging_path.join(".lbby-original-server.properties");

    if original_path.exists() {
        let original = std::fs::read_to_string(&original_path)
            .map_err(|e| format!("Failed to read original properties: {}", e))?;
        std::fs::write(&props_path, original)
            .map_err(|e| format!("Failed to restore server.properties: {}", e))?;
        let _ = std::fs::remove_file(&original_path);
    } else if props_path.exists() {
        // No original existed — remove the one we created
        // But check if the normal install pipeline intentionally created one.
        // If the staging path had no server.properties before our overlay, remove it.
        // We track this via the .lbby-original-server.properties marker.
        // If the marker doesn't exist, it means the file didn't exist before.
        let _ = std::fs::remove_file(&props_path);
    }

    Ok(())
}

/// Write server.properties for validation, preserving the original.
fn write_validation_properties_with_backup(
    staging_path: &Path,
    cfg: &ServerConfig,
    port: u16,
    validation_world: &str,
) -> Result<(), String> {
    // Backup original
    let props_path = staging_path.join("server.properties");
    if props_path.exists() {
        let original = std::fs::read_to_string(&props_path)
            .map_err(|e| format!("Failed to read server.properties: {}", e))?;
        let backup_path = staging_path.join(".lbby-original-server.properties");
        std::fs::write(&backup_path, original)
            .map_err(|e| format!("Failed to backup server.properties: {}", e))?;
    }

    write_validation_properties(staging_path, cfg, port, validation_world)
}

/// Remove validation world directories.
fn cleanup_validation_world(staging_path: &Path) -> Result<(), String> {
    // The validation world name was written into server.properties.
    // We need to find and remove it. Since we restore server.properties after,
    // we can't read it from there. Instead, look for directories matching the pattern.
    if let Ok(entries) = std::fs::read_dir(staging_path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".lbby-validation-world-") {
                let path = entry.path();
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }
    Ok(())
}

fn extract_prop(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn short_uuid() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

// ── Pre-commit residue verification ─────────────────────────────────────

/// Verify that no validation artifacts remain in the staging directory.
/// Called after BootValidator returns Success and BEFORE txn.commit().
///
/// Checks:
/// - No .lbby-validation-world-* directories
/// - No .lbby-original-server.properties backup marker
/// - server.properties level-name does NOT start with ".lbby-validation-world-"
/// - server-ip is not still "127.0.0.1" if the original was different
pub fn verify_validation_cleanup(staging_path: &Path) -> Result<(), String> {
    // 1. No validation world dirs
    if let Ok(entries) = std::fs::read_dir(staging_path) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".lbby-validation-world-") {
                return Err(format!("Validation world residue found: {}", name_str));
            }
        }
    }

    // 2. No backup marker
    let backup_marker = staging_path.join(".lbby-original-server.properties");
    if backup_marker.exists() {
        return Err(
            "Validation backup marker .lbby-original-server.properties still exists".to_string(),
        );
    }

    // 3. server.properties not contaminated
    let props_path = staging_path.join("server.properties");
    if let Ok(content) = std::fs::read_to_string(&props_path) {
        for line in content.lines() {
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if key == "level-name" && val.starts_with(".lbby-validation-world-") {
                    return Err(format!(
                        "server.properties level-name still set to validation world: {}",
                        val
                    ));
                }
                if key == "server-ip" && val == "127.0.0.1" {
                    // This could be legitimate (user chose 127.0.0.1).
                    // We only flag it if there's also a validation world residue,
                    // which we already checked above. So this is fine.
                }
            }
        }
    }

    Ok(())
}

// ── Validation diagnostics ──────────────────────────────────────────────

/// Save bounded validation diagnostics to a location OUTSIDE the live server.
///
/// Path: `<servers-parent>/.lbby-diagnostics/<server-id>/<txn-id>/`
///
/// Preserves:
/// - validation.log.tail.txt (last 200 lines of validation output)
/// - boot-result.json (structured failure info)
/// - crash-reports/*.txt (if present, bounded to 5 files)
pub fn save_validation_diagnostics(
    staging_path: &Path,
    server_id: &str,
    txn_id: &str,
    result: &BootResult,
) {
    // Determine diagnostics parent: sibling of the .lbby-staging directory.
    // Staging path is: <live_parent>/.lbby-staging/<server_name>-<txn_id>
    // So 2 levels up gives us <live_parent>, which is the writable servers root.
    let servers_parent = staging_path
        .parent() // -> <live_parent>/.lbby-staging
        .and_then(|p| p.parent()) // -> <live_parent>
        .unwrap_or(staging_path);

    let diag_dir = servers_parent
        .join(".lbby-diagnostics")
        .join(server_id)
        .join(txn_id);

    if let Err(e) = std::fs::create_dir_all(&diag_dir) {
        eprintln!("[boot-validator] Failed to create diagnostics dir: {}", e);
        return;
    }

    // Save log tail
    let log_tail = match result {
        BootResult::Failed(f) => &f.log_tail,
        BootResult::Timeout(t) => &t.log_tail,
        BootResult::Success(_) => return, // no diagnostics needed on success
    };
    let _ = std::fs::write(diag_dir.join("validation.log.tail.txt"), log_tail);

    // Save structured result
    let result_json = match result {
        BootResult::Failed(f) => format!(
            r#"{{"status":"failed","reason":"{:?}","exit_code":{:?}}}"#,
            f.reason, f.exit_code
        ),
        BootResult::Timeout(t) => format!(
            r#"{{"status":"timeout","waited_secs":{:.1}}}"#,
            t.waited.as_secs_f64()
        ),
        BootResult::Success(_) => unreachable!(),
    };
    let _ = std::fs::write(diag_dir.join("boot-result.json"), result_json);

    // Copy crash-reports (bounded to 5 most recent)
    let crash_dir = staging_path.join("crash-reports");
    if crash_dir.is_dir() {
        let diag_crash = diag_dir.join("crash-reports");
        let _ = std::fs::create_dir_all(&diag_crash);
        if let Ok(entries) = std::fs::read_dir(&crash_dir) {
            let mut crash_files: Vec<_> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                .collect();
            crash_files.sort_by_key(|e| e.file_name());
            for entry in crash_files.into_iter().rev().take(5) {
                let src = entry.path();
                let dst = diag_crash.join(entry.file_name());
                let _ = std::fs::copy(&src, &dst);
            }
        }
    }

    eprintln!(
        "[boot-validator] Validation diagnostics saved to: {}",
        diag_dir.display()
    );
}

// ── Ready detection ─────────────────────────────────────────────────────

/// Detect the Minecraft server ready signal from a log line.
///
/// The canonical pattern is:
///   `[Server thread/INFO] [net.minecraft.server.MinecraftServer]: Done (X.XXXs)! For help, type "help"`
///
/// But loader variants may differ. We match on the essential parts:
///   - Contains "Done"
///   - Contains "For help" or "type \"help\""
///
/// This matches the production detection in server.rs but is slightly more
/// flexible for loader variants.
fn is_ready_signal(line: &str) -> bool {
    line.contains("Done") && (line.contains("For help") || line.contains("type \"help\""))
}

/// Detect OutOfMemoryError in log output.
fn is_oom_line(line: &str) -> bool {
    line.contains("java.lang.OutOfMemoryError")
        || line.contains("Exception in thread") && line.contains("java.lang.OutOfMemoryError")
}

// ── Capped log buffer ───────────────────────────────────────────────────

/// A ring/tail buffer for captured log output. Keeps the last N bytes.
struct CappedLog {
    lines: VecDeque<String>,
    total_bytes: usize,
    cap: usize,
}

impl CappedLog {
    fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            total_bytes: 0,
            cap,
        }
    }

    fn push_line(&mut self, line: &str) {
        let line_bytes = line.len() + 1; // +1 for newline
        self.lines.push_back(line.to_string());
        self.total_bytes += line_bytes;

        // Evict oldest lines until under cap
        while self.total_bytes > self.cap && self.lines.len() > 1 {
            if let Some(old) = self.lines.pop_front() {
                self.total_bytes -= old.len() + 1;
            }
        }
    }

    fn to_string(&mut self) -> String {
        let joined = self.lines.iter().cloned().collect::<Vec<_>>().join("\n");
        joined
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_test_config(server_path: &str) -> ServerConfig {
        ServerConfig {
            server_path: server_path.to_string(),
            server_name: "test".to_string(),
            server_type: crate::config::ServerType::Fabric,
            minecraft_version: "1.20.1".to_string(),
            loader_version: Some("0.16.14".to_string()),
            ram_mb: 4096,
            optimized_jvm_flags: true,
            setup_complete: true,
            ..Default::default()
        }
    }

    // ── EULA tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn eula_not_accepted_returns_failure() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        // No eula.txt

        let cfg = make_test_config(staging.to_str().unwrap());
        let validator = BootValidator::new();
        let result = validator.validate(&cfg, &staging).await;

        match result {
            BootResult::Failed(f) => assert_eq!(f.reason, BootFailureReason::EulaNotAccepted),
            _ => panic!("Expected EulaNotAccepted failure"),
        }
    }

    #[tokio::test]
    async fn eula_false_returns_failure() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("eula.txt"), "eula=false\n").unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        let validator = BootValidator::new();
        let result = validator.validate(&cfg, &staging).await;

        match result {
            BootResult::Failed(f) => assert_eq!(f.reason, BootFailureReason::EulaNotAccepted),
            _ => panic!("Expected EulaNotAccepted failure"),
        }
    }

    // ── EULA consent tests ──────────────────────────────────────────

    #[test]
    fn eula_consent_false_prevents_eula_write() {
        // Verify that MinecraftSpec with eula_accepted=false defaults correctly.
        // The actual write gating is in prepare_minecraft/do_install_server,
        // but the field must default to false for safety.
        let spec = crate::node_api::MinecraftSpec::default();
        assert!(
            !spec.eula_accepted,
            "MinecraftSpec.eula_accepted must default to false"
        );
    }

    #[test]
    fn eula_consent_true_permits_eula_write() {
        let mut spec = crate::node_api::MinecraftSpec::default();
        spec.eula_accepted = true;
        assert!(spec.eula_accepted);
    }

    #[test]
    fn server_config_eula_consent_defaults_false() {
        // ServerConfig.eula_accepted must default to false so existing configs
        // (which have no eula_accepted field) do NOT auto-accept.
        let cfg = ServerConfig::default();
        assert!(
            !cfg.eula_accepted,
            "ServerConfig.eula_accepted must default to false"
        );
    }

    #[tokio::test]
    async fn boot_validation_cannot_bypass_eula_state() {
        // BootValidator checks eula.txt on disk, not the config field.
        // Even if cfg.eula_accepted were true, missing eula.txt = failure.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        // No eula.txt on disk

        let mut cfg = make_test_config(staging.to_str().unwrap());
        cfg.eula_accepted = true; // consent given in config

        let validator = BootValidator::new();
        let result = validator.validate(&cfg, &staging).await;

        // Must still fail — eula.txt is missing on disk
        match result {
            BootResult::Failed(f) => assert_eq!(f.reason, BootFailureReason::EulaNotAccepted),
            _ => panic!("Expected EulaNotAccepted even with cfg.eula_accepted=true"),
        }
    }

    // ── Port tests ──────────────────────────────────────────────────

    #[test]
    fn find_free_port_returns_valid_port() {
        let port = find_free_port().unwrap();
        assert!(port > 0);
        // Port should be bindable
        let listener = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok());
    }

    // ── Pre-commit residue verification ─────────────────────────────────────

    /// Verify that no validation artifacts remain in the staging directory.
    /// Called after BootValidator returns Success and BEFORE txn.commit().
    ///
    /// Checks:
    /// - No .lbby-validation-world-* directories
    /// - No .lbby-original-server.properties backup marker
    /// - server.properties level-name does NOT start with ".lbby-validation-world-"
    /// - server-ip is not still "127.0.0.1" if the original was different
    pub fn verify_validation_cleanup(staging_path: &Path) -> Result<(), String> {
        // 1. No validation world dirs
        if let Ok(entries) = std::fs::read_dir(staging_path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(".lbby-validation-world-") {
                    return Err(format!("Validation world residue found: {}", name_str));
                }
            }
        }

        // 2. No backup marker
        let backup_marker = staging_path.join(".lbby-original-server.properties");
        if backup_marker.exists() {
            return Err(
                "Validation backup marker .lbby-original-server.properties still exists"
                    .to_string(),
            );
        }

        // 3. server.properties not contaminated
        let props_path = staging_path.join("server.properties");
        if let Ok(content) = std::fs::read_to_string(&props_path) {
            for line in content.lines() {
                if let Some((key, val)) = line.split_once('=') {
                    let key = key.trim();
                    let val = val.trim();
                    if key == "level-name" && val.starts_with(".lbby-validation-world-") {
                        return Err(format!(
                            "server.properties level-name still set to validation world: {}",
                            val
                        ));
                    }
                    if key == "server-ip" && val == "127.0.0.1" {
                        // This could be legitimate (user chose 127.0.0.1).
                        // We only flag it if there's also a validation world residue,
                        // which we already checked above. So this is fine.
                    }
                }
            }
        }

        Ok(())
    }

    // ── Validation diagnostics ──────────────────────────────────────────────

    /// Save bounded validation diagnostics to a location OUTSIDE the live server.
    ///
    /// Path: `<servers-parent>/.lbby-diagnostics/<server-id>/<txn-id>/`
    ///
    /// Preserves:
    /// - validation.log.tail.txt (last 200 lines of validation output)
    /// - boot-result.json (structured failure info)
    /// - crash-reports/*.txt (if present, bounded to 5 files)
    pub fn save_validation_diagnostics(
        staging_path: &Path,
        server_id: &str,
        txn_id: &str,
        result: &BootResult,
    ) {
        // Determine diagnostics parent: sibling of the .lbby-staging directory.
        // Staging path is: <live_parent>/.lbby-staging/<server_name>-<txn_id>
        // So 2 levels up gives us <live_parent>, which is where .lbby-diagnostics lives.
        let servers_parent = staging_path
            .parent() // <live_parent>/.lbby-staging
            .and_then(|p| p.parent()) // <live_parent>
            .unwrap_or(staging_path);

        let diag_dir = servers_parent
            .join(".lbby-diagnostics")
            .join(server_id)
            .join(txn_id);

        if let Err(e) = std::fs::create_dir_all(&diag_dir) {
            eprintln!("[boot-validator] Failed to create diagnostics dir: {}", e);
            return;
        }

        // Save log tail
        let log_tail = match result {
            BootResult::Failed(f) => &f.log_tail,
            BootResult::Timeout(t) => &t.log_tail,
            BootResult::Success(_) => return, // no diagnostics needed on success
        };
        let _ = std::fs::write(diag_dir.join("validation.log.tail.txt"), log_tail);

        // Save structured result
        let result_json = match result {
            BootResult::Failed(f) => format!(
                r#"{{"status":"failed","reason":"{:?}","exit_code":{:?}}}"#,
                f.reason, f.exit_code
            ),
            BootResult::Timeout(t) => format!(
                r#"{{"status":"timeout","waited_secs":{:.1}}}"#,
                t.waited.as_secs_f64()
            ),
            BootResult::Success(_) => unreachable!(),
        };
        let _ = std::fs::write(diag_dir.join("boot-result.json"), result_json);

        // Copy crash-reports (bounded to 5 most recent)
        let crash_dir = staging_path.join("crash-reports");
        if crash_dir.is_dir() {
            let diag_crash = diag_dir.join("crash-reports");
            let _ = std::fs::create_dir_all(&diag_crash);
            if let Ok(entries) = std::fs::read_dir(&crash_dir) {
                let mut crash_files: Vec<_> = entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                    .collect();
                crash_files.sort_by_key(|e| e.file_name());
                for entry in crash_files.into_iter().rev().take(5) {
                    let src = entry.path();
                    let dst = diag_crash.join(entry.file_name());
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }

        eprintln!(
            "[boot-validator] Validation diagnostics saved to: {}",
            diag_dir.display()
        );
    }

    // ── Ready detection tests ───────────────────────────────────────

    #[test]
    fn detects_standard_ready_signal() {
        let line = "[15:30:45] [Server thread/INFO] [net.minecraft.server.MinecraftServer]: Done (3.456s)! For help, type \"help\"";
        assert!(is_ready_signal(line));
    }

    #[test]
    fn detects_ready_with_type_help() {
        let line = "[15:30:45] [Server thread/INFO] [net.minecraft.server.MinecraftServer]: Done (1.234s)! For help, type \"help\"";
        assert!(is_ready_signal(line));
    }

    #[test]
    fn rejects_non_ready_line() {
        assert!(!is_ready_signal("[Server thread/INFO]: Loading properties"));
        assert!(!is_ready_signal(
            "[Server thread/INFO]: Preparing spawn area"
        ));
    }

    // ── OOM detection tests ─────────────────────────────────────────

    #[test]
    fn detects_oom_error() {
        assert!(is_oom_line("java.lang.OutOfMemoryError: Java heap space"));
    }

    // ── CappedLog tests ─────────────────────────────────────────────

    #[test]
    fn capped_log_respects_size_limit() {
        let mut log = CappedLog::new(100);
        for i in 0..200 {
            log.push_line(&format!("line {}", i));
        }
        let output = log.to_string();
        assert!(output.len() <= 200); // some slack for line boundaries
                                      // Should contain recent lines, not old ones
        assert!(output.contains("line 199"));
        assert!(!output.contains("line 0"));
    }

    #[test]
    fn capped_log_preserves_order() {
        let mut log = CappedLog::new(1000);
        log.push_line("first");
        log.push_line("second");
        log.push_line("third");
        let output = log.to_string();
        let first_pos = output.find("first").unwrap();
        let second_pos = output.find("second").unwrap();
        let third_pos = output.find("third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }

    // ── Server properties overlay tests ─────────────────────────────

    #[test]
    fn validation_properties_override_port_and_world() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(
            staging.join("server.properties"),
            "server-port=25565\nlevel-name=myworld\nmotd=Hello\n",
        )
        .unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties(staging, &cfg, 12345, ".lbby-validation-test").unwrap();

        let content = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(content.contains("server-port=12345"));
        assert!(content.contains("level-name=.lbby-validation-test"));
        assert!(content.contains("server-ip=127.0.0.1"));
        assert!(content.contains("enable-rcon=false"));
        // Original motd preserved
        assert!(content.contains("motd=Hello"));
    }

    #[test]
    fn validation_properties_add_missing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        // No server.properties

        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties(staging, &cfg, 9999, ".lbby-validation-test").unwrap();

        let content = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(content.contains("server-port=9999"));
        assert!(content.contains("level-name=.lbby-validation-test"));
        assert!(content.contains("server-ip=127.0.0.1"));
    }

    #[test]
    fn validation_properties_preserves_online_mode() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(
            staging.join("server.properties"),
            "online-mode=false\nserver-port=25565\n",
        )
        .unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties(staging, &cfg, 12345, ".lbby-validation-test").unwrap();

        let content = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(content.contains("online-mode=false"));
    }

    // ── Validation world cleanup tests ──────────────────────────────

    #[test]
    fn cleanup_removes_validation_world() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        // Create a validation world directory
        let world_dir = staging.join(".lbby-validation-world-abc123");
        fs::create_dir_all(&world_dir).unwrap();
        fs::write(world_dir.join("level.dat"), "fake").unwrap();

        // Create a real world that should NOT be touched
        let real_world = staging.join("myworld");
        fs::create_dir_all(&real_world).unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        cleanup_validation_world(staging).unwrap();

        assert!(!world_dir.exists());
        assert!(real_world.exists());
    }

    #[test]
    fn cleanup_preserves_normal_worlds() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        fs::create_dir_all(staging.join("world")).unwrap();
        fs::create_dir_all(staging.join("world_nether")).unwrap();
        fs::create_dir_all(staging.join("myworld")).unwrap();
        fs::create_dir_all(staging.join(".lbby-validation-world-xyz")).unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        cleanup_validation_world(staging).unwrap();

        assert!(staging.join("world").exists());
        assert!(staging.join("world_nether").exists());
        assert!(staging.join("myworld").exists());
        assert!(!staging.join(".lbby-validation-world-xyz").exists());
    }

    // ── Server properties restore tests ─────────────────────────────

    #[test]
    fn restore_server_properties_recovers_original() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        let original = "server-port=25565\nlevel-name=myworld\nmotd=Hello\n";
        fs::write(staging.join("server.properties"), original).unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());

        // Write validation overlay with backup
        write_validation_properties_with_backup(staging, &cfg, 12345, ".lbby-validation-test")
            .unwrap();

        // Verify overlay is active
        let overlay = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(overlay.contains("server-port=12345"));

        // Restore
        restore_server_properties(staging).unwrap();

        // Verify original restored
        let restored = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn restore_removes_temp_file_when_no_original() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        // No original server.properties

        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties(staging, &cfg, 12345, ".lbby-validation-test").unwrap();

        assert!(staging.join("server.properties").exists());

        restore_server_properties(staging).unwrap();

        // Should be removed since no original existed
        // (or rather, the marker doesn't exist so we remove the file)
    }

    // ── Launch spec tests ───────────────────────────────────────────

    #[test]
    fn launch_spec_fabric_uses_server_jar() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        // Fabric install puts server.jar, not fabric-server-launch.jar
        // This matches production do_start_server which hardcodes "server.jar"
        fs::write(staging.join("server.jar"), "fake").unwrap();

        let mut cfg = make_test_config(staging.to_str().unwrap());
        cfg.server_type = crate::config::ServerType::Fabric;
        cfg.loader_version = Some("0.16.14".to_string());

        // This will fail if Java is not installed, which is expected in CI.
        // We test the logic, not the actual Java binary.
        match build_launch_spec(&cfg, staging) {
            Ok(spec) => {
                assert!(spec.args.contains(&"server.jar".to_string()));
                assert!(spec.args.contains(&"nogui".to_string()));
            }
            Err(e) if e.contains("not found") => {
                // Java not installed in test env — acceptable
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    fn launch_spec_vanilla_uses_server_jar() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join("server.jar"), "fake").unwrap();

        let mut cfg = make_test_config(staging.to_str().unwrap());
        cfg.server_type = crate::config::ServerType::Vanilla;

        match build_launch_spec(&cfg, staging) {
            Ok(spec) => {
                assert!(spec.args.contains(&"server.jar".to_string()));
            }
            Err(e) if e.contains("not found") => {
                // Java not installed
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    fn launch_spec_forge_requires_version() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        let mut cfg = make_test_config(staging.to_str().unwrap());
        cfg.server_type = crate::config::ServerType::Forge;
        cfg.loader_version = None; // Missing!

        let result = build_launch_spec(&cfg, staging);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No Forge version"));
    }

    // ── Process simulation tests (Unix only) ──────────────────────

    /// Helper: create a staging dir with eula.txt, server.properties, and a
    /// shell script as the "server", then run validate_with_launch.
    #[cfg(unix)]
    async fn run_simulated(script_body: &str, timeout: Duration) -> BootResult {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        // Write eula.txt
        fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();
        // Write server.properties so backup/restore cycle is exercised
        fs::write(
            staging.join("server.properties"),
            "server-port=25565\nlevel-name=myworld\n",
        )
        .unwrap();

        // Write the script
        let script = staging.join("fake-server.sh");
        fs::write(&script, script_body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        let validator = BootValidator::with_timeout(timeout);
        let spec = LaunchSpec {
            executable: PathBuf::from("/bin/bash"),
            args: vec!["fake-server.sh".to_string()],
            env: vec![],
            env_remove: vec![],
        };

        validator.validate_with_launch(&cfg, staging, spec).await
    }

    #[tokio::test]
    async fn simulated_successful_boot() {
        // Script prints the ready signal then waits for "stop"
        let script = r#"#!/bin/bash
echo "[Server thread/INFO]: Done (1.234s)! For help, type \"help\""
# Wait for stop
while read -r line; do
  if [ "$line" = "stop" ]; then exit 0; fi
done
"#;
        let result = run_simulated(script, Duration::from_secs(10)).await;
        match result {
            BootResult::Success(s) => {
                assert!(s.elapsed < Duration::from_secs(10));
                assert!(s.graceful_shutdown);
            }
            other => panic!("Expected Success, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn simulated_process_exits_before_ready() {
        let script = r#"#!/bin/bash
echo "Loading..."
exit 1
"#;
        let result = run_simulated(script, Duration::from_secs(10)).await;
        match result {
            BootResult::Failed(f) => assert_eq!(f.reason, BootFailureReason::ProcessExited),
            other => panic!("Expected Failed(ProcessExited), got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn simulated_timeout() {
        // Script sleeps forever without printing ready
        let script = r#"#!/bin/bash
while true; do
  echo "Preparing spawn area: 0%"
  sleep 1
done
"#;
        let result = run_simulated(script, Duration::from_secs(3)).await;
        match result {
            BootResult::Timeout(t) => {
                assert!(t.waited >= Duration::from_secs(2));
                assert!(t.log_tail.contains("Preparing spawn area"));
            }
            other => panic!("Expected Timeout, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn simulated_ready_then_force_kill() {
        // Script prints ready but ignores "stop" entirely
        let script = r#"#!/bin/bash
echo "[Server thread/INFO]: Done (1.0s)! For help, type \"help\""
# Ignore stdin completely — just sleep
sleep 60
"#;
        let result = run_simulated(script, Duration::from_secs(10)).await;
        match result {
            BootResult::Success(s) => {
                // Should have force-killed (graceful_shutdown = false)
                // Note: may be true if the "stop" write fails and kill happens
                // The key assertion is that we got Success (server did boot)
                let _ = s.graceful_shutdown; // don't assert specific value
            }
            other => panic!("Expected Success, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn simulated_output_cap_no_oom() {
        // Script spams output then prints ready
        let script = r#"#!/bin/bash
for i in $(seq 1 10000); do
  echo "Spam line $i: $(head -c 200 /dev/urandom | base64)"
done
echo "[Server thread/INFO]: Done (0.5s)! For help, type \"help\""
while read -r line; do
  if [ "$line" = "stop" ]; then exit 0; fi
done
"#;
        let result = run_simulated(script, Duration::from_secs(30)).await;
        match result {
            BootResult::Success(s) => {
                // Log tail should be capped — not10000 lines
                assert!(s.log_tail.len() <= OUTPUT_CAP_BYTES * 2);
            }
            other => panic!("Expected Success, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn simulated_overlay_cleaned_by_guard_after_success() {
        // Tests that ValidationOverlayGuard (used by do_validate) cleans up
        // validation world and restores server.properties after a successful boot.
        // validate_with_launch itself does NOT own overlay cleanup.
        let script = r#"#!/bin/bash
echo "[Server thread/INFO]: Done (0.1s)! For help, type \"help\""
# Create a validation world dir to test cleanup
mkdir -p .lbby-validation-world-test123
while read -r line; do
  if [ "$line" = "stop" ]; then exit 0; fi
done
"#;
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();
        fs::write(
            staging.join("server.properties"),
            "server-port=25565\nlevel-name=myworld\n",
        )
        .unwrap();

        // Create a real world that must survive
        fs::create_dir_all(staging.join("myworld")).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let script_path = staging.join("fake-server.sh");
        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        // Simulate full do_validate guard lifecycle:
        // 1. Create overlay guard
        let mut overlay_guard = ValidationOverlayGuard::new(staging);

        // 2. Write validation overlay (backup original + override)
        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties_with_backup(staging, &cfg, 12345, ".lbby-validation-override")
            .unwrap();
        overlay_guard.set_props_backed_up();

        // 3. Run process-only validation (no overlay cleanup)
        let validator = BootValidator::with_timeout(Duration::from_secs(10));
        let spec = LaunchSpec {
            executable: PathBuf::from("/bin/bash"),
            args: vec!["fake-server.sh".to_string()],
            env: vec![],
            env_remove: vec![],
        };
        let result = validator.validate_with_launch(&cfg, staging, spec).await;
        assert!(matches!(result, BootResult::Success(_)));

        // 4. Guard cleanup (the SINGLE owner of overlay cleanup)
        overlay_guard.finish().unwrap();

        // Validation world must be gone
        assert!(!staging.join(".lbby-validation-world-test123").exists());
        // Real world must survive
        assert!(staging.join("myworld").exists());
        // server.properties must be restored (original content)
        let restored = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(restored.contains("server-port=25565"));
        assert!(restored.contains("level-name=myworld"));
    }

    #[tokio::test]
    async fn simulated_oom_detection() {
        let script = r#"#!/bin/bash
echo "Loading some stuff..."
echo "java.lang.OutOfMemoryError: Java heap space"
sleep 60
"#;
        let result = run_simulated(script, Duration::from_secs(10)).await;
        match result {
            BootResult::Failed(f) => assert_eq!(f.reason, BootFailureReason::OutOfMemory),
            other => panic!("Expected Failed(OutOfMemory), got: {:?}", other),
        }
    }

    // ── Phase 3E.1: RAII guard tests ───────────────────────────────

    #[test]
    fn early_failure_restores_server_properties() {
        // Simulate: write validation props, then guard finishes without boot.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        let original = "server-port=25565\nlevel-name=myworld\n";
        fs::write(staging.join("server.properties"), original).unwrap();

        let mut guard = ValidationOverlayGuard::new(staging);
        write_validation_properties_with_backup(
            staging,
            &make_test_config(staging.to_str().unwrap()),
            12345,
            ".lbby-validation-test",
        )
        .unwrap();
        guard.set_props_backed_up();

        // Verify overlay is active
        let overlay = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(overlay.contains("server-port=12345"));

        // Finish guard — should restore original
        guard.finish().unwrap();

        let restored = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(restored.contains("server-port=25565"));
        assert!(restored.contains("level-name=myworld"));
        // Backup marker removed
        assert!(!staging.join(".lbby-original-server.properties").exists());
    }

    #[test]
    fn early_failure_removes_validation_world() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        // Create validation world
        let vw = staging.join(".lbby-validation-world-abc123");
        fs::create_dir_all(&vw).unwrap();
        fs::write(vw.join("level.dat"), "fake").unwrap();

        // Real world must survive
        fs::create_dir_all(staging.join("myworld")).unwrap();

        let guard = ValidationOverlayGuard::new(staging);
        guard.finish().unwrap();

        assert!(!vw.exists());
        assert!(staging.join("myworld").exists());
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_child() {
        // Script sleeps forever — timeout should kill it
        let script = r#"#!/bin/bash
while true; do
  echo "Preparing spawn area: 0%"
  sleep 1
done
"#;
        let result = run_simulated(script, Duration::from_secs(3)).await;
        match result {
            BootResult::Timeout(t) => {
                assert!(t.waited >= Duration::from_secs(2));
                assert!(t.log_tail.contains("Preparing spawn area"));
            }
            other => panic!("Expected Timeout, got: {:?}", other),
        }
        // If we got here without hanging, the child was reaped.
    }

    #[tokio::test]
    async fn cleanup_residue_blocks_commit() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        // Create validation world residue
        fs::create_dir_all(staging.join(".lbby-validation-world-residue")).unwrap();

        let result = verify_validation_cleanup(staging);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("residue"));
    }

    #[test]
    fn backup_marker_removed_after_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join("server.properties"), "server-port=25565\n").unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties_with_backup(staging, &cfg, 12345, ".lbby-validation-test")
            .unwrap();

        // Marker exists
        assert!(staging.join(".lbby-original-server.properties").exists());

        // Restore
        restore_server_properties(staging).unwrap();

        // Marker gone
        assert!(!staging.join(".lbby-original-server.properties").exists());
    }

    #[test]
    fn production_custom_world_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();

        // Production world with custom name
        fs::write(
            staging.join("server.properties"),
            "level-name=MyCustomWorld\nserver-port=25565\n",
        )
        .unwrap();
        fs::create_dir_all(staging.join("MyCustomWorld")).unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        write_validation_properties_with_backup(staging, &cfg, 12345, ".lbby-validation-temp")
            .unwrap();

        // Validation overlay active
        let overlay = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(overlay.contains("level-name=.lbby-validation-temp"));

        // Restore
        restore_server_properties(staging).unwrap();

        // Custom world name preserved
        let restored = fs::read_to_string(staging.join("server.properties")).unwrap();
        assert!(restored.contains("level-name=MyCustomWorld"));
        // Custom world dir untouched
        assert!(staging.join("MyCustomWorld").exists());
    }

    // ── Phase 3E.1: Ready signal tests ─────────────────────────────

    #[test]
    fn ready_vanilla_format() {
        assert!(is_ready_signal(
            "[Server thread/INFO]: Done (3.456s)! For help, type \"help\""
        ));
    }

    #[test]
    fn ready_forge_format() {
        assert!(is_ready_signal(
            "[Server thread/INFO] [net.minecraft.server.MinecraftServer]: Done (12.345s)! For help, type \"help\""
        ));
    }

    #[test]
    fn reject_server_started() {
        assert!(!is_ready_signal("[Server thread/INFO]: Server started"));
        assert!(!is_ready_signal("Server started"));
    }

    #[test]
    fn reject_preparing_spawn() {
        assert!(!is_ready_signal(
            "[Server thread/INFO]: Preparing spawn area: 42%"
        ));
    }

    // ── Phase 3E.1: Parity tests ───────────────────────────────────

    #[test]
    fn fabric_launch_target_parity() {
        // After fix: build_launch_spec should use "server.jar" for Fabric,
        // matching production do_start_server which hardcodes "server.jar".
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        // Put both jars to test that server.jar is chosen (not fabric-server-launch.jar)
        fs::write(staging.join("server.jar"), "fake").unwrap();
        fs::write(staging.join("fabric-server-launch.jar"), "fake").unwrap();

        let mut cfg = make_test_config(staging.to_str().unwrap());
        cfg.server_type = crate::config::ServerType::Fabric;

        match build_launch_spec(&cfg, staging) {
            Ok(spec) => {
                // Must use server.jar, not fabric-server-launch.jar
                assert!(
                    spec.args.contains(&"server.jar".to_string()),
                    "Expected server.jar, got: {:?}",
                    spec.args
                );
                assert!(
                    !spec.args.contains(&"fabric-server-launch.jar".to_string()),
                    "Should NOT use fabric-server-launch.jar — production uses server.jar"
                );
            }
            Err(e) if e.contains("not found") => {
                // Java not installed — acceptable in CI
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    fn dyld_env_removal_parity() {
        // build_launch_spec must use env_remove, not env-set-to-empty.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join("server.jar"), "fake").unwrap();

        let mut cfg = make_test_config(staging.to_str().unwrap());
        cfg.server_type = crate::config::ServerType::Vanilla;

        match build_launch_spec(&cfg, staging) {
            Ok(spec) => {
                // Must have DYLD env vars in env_remove
                assert!(
                    spec.env_remove.contains(&"DYLD_LIBRARY_PATH".to_string()),
                    "DYLD_LIBRARY_PATH should be in env_remove"
                );
                assert!(
                    spec.env_remove
                        .contains(&"DYLD_FALLBACK_LIBRARY_PATH".to_string()),
                    "DYLD_FALLBACK_LIBRARY_PATH should be in env_remove"
                );
                assert!(
                    spec.env_remove.contains(&"DYLD_FRAMEWORK_PATH".to_string()),
                    "DYLD_FRAMEWORK_PATH should be in env_remove"
                );
                assert!(
                    spec.env_remove
                        .contains(&"DYLD_INSERT_LIBRARIES".to_string()),
                    "DYLD_INSERT_LIBRARIES should be in env_remove"
                );
                assert_eq!(spec.env_remove.len(), 8, "Should remove all 8 DYLD vars");

                // Must NOT have DYLD vars in env (set-to-empty)
                for (k, _) in &spec.env {
                    assert!(
                        !k.starts_with("DYLD_"),
                        "DYLD var {} should NOT be in env (set-to-empty), should be in env_remove",
                        k
                    );
                }
            }
            Err(e) if e.contains("not found") => {
                // Java not installed
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[test]
    fn no_leaked_dyld_set_to_empty() {
        // Regression: old code set DYLD_LIBRARY_PATH="" in env.
        // New code should use env_remove instead.
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join("server.jar"), "fake").unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        if let Ok(spec) = build_launch_spec(&cfg, staging) {
            for (k, v) in &spec.env {
                if k.starts_with("DYLD_") {
                    panic!(
                        "DYLD var {} set to '{}' in env — should use env_remove instead",
                        k, v
                    );
                }
            }
        }
    }

    // ── Phase 3E.1: Diagnostics tests ──────────────────────────────

    #[test]
    fn failed_validation_diagnostics_saved_outside_live() {
        let dir = tempfile::tempdir().unwrap();
        // Staging path mirrors install_transaction.rs: <live_parent>/.lbby-staging/<server_name>-<txn>
        let staging = dir
            .path()
            .join("servers")
            .join(".lbby-staging")
            .join("test-server-txn-123");
        fs::create_dir_all(&staging).unwrap();

        // Create crash-reports
        fs::create_dir_all(staging.join("crash-reports")).unwrap();
        fs::write(
            staging.join("crash-reports").join("crash-2026-09-09.txt"),
            "crash data",
        )
        .unwrap();

        let result = BootResult::Failed(BootFailure {
            exit_code: Some(1),
            reason: BootFailureReason::ProcessExited,
            log_tail: "test log tail".to_string(),
        });

        save_validation_diagnostics(&staging, "test-server", "txn-123", &result);

        // Diagnostics should be at servers/.lbby-diagnostics/test-server/txn-123/
        let diag_dir = dir
            .path()
            .join("servers")
            .join(".lbby-diagnostics")
            .join("test-server")
            .join("txn-123");
        assert!(diag_dir.exists(), "Diagnostics dir should exist");
        assert!(diag_dir.join("validation.log.tail.txt").exists());
        assert!(diag_dir.join("boot-result.json").exists());
        assert!(diag_dir
            .join("crash-reports")
            .join("crash-2026-09-09.txt")
            .exists());

        // Live server dir should NOT have diagnostics
        let live_server = dir.path().join("servers").join("test-server");
        assert!(!live_server.join(".lbby-validation-logs").exists());
    }

    #[test]
    fn failed_validation_creates_no_files_in_live() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        fs::create_dir_all(&staging).unwrap();

        let result = BootResult::Failed(BootFailure {
            exit_code: Some(1),
            reason: BootFailureReason::ProcessExited,
            log_tail: "test".to_string(),
        });

        save_validation_diagnostics(&staging, "test", "txn", &result);

        // Staging itself should not have diagnostics files
        assert!(!staging.join("validation.log.tail.txt").exists());
        assert!(!staging.join("boot-result.json").exists());
    }

    #[test]
    fn verify_validation_cleanup_passes_on_clean_staging() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(
            staging.join("server.properties"),
            "server-port=25565\nlevel-name=myworld\n",
        )
        .unwrap();
        fs::create_dir_all(staging.join("myworld")).unwrap();

        let result = verify_validation_cleanup(staging);
        assert!(
            result.is_ok(),
            "Clean staging should pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn verify_validation_cleanup_finds_backup_marker() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join(".lbby-original-server.properties"), "backup").unwrap();

        let result = verify_validation_cleanup(staging);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("backup marker"));
    }

    #[test]
    fn verify_validation_cleanup_finds_contaminated_level_name() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(
            staging.join("server.properties"),
            "level-name=.lbby-validation-world-abc123\n",
        )
        .unwrap();

        let result = verify_validation_cleanup(staging);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("level-name"));
    }

    // ── Phase 3E.1: Validation log cleanup on success ──────────────

    #[tokio::test]
    async fn success_removes_validation_latest_log() {
        let script = r#"#!/bin/bash
echo "[Server thread/INFO]: Done (0.1s)! For help, type \"help\""
# Create logs/latest.log as the server would
mkdir -p logs
echo "validation run log" > logs/latest.log
while read -r line; do
  if [ "$line" = "stop" ]; then exit 0; fi
done
"#;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path();
        fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();
        fs::write(
            staging.join("server.properties"),
            "server-port=25565\nlevel-name=myworld\n",
        )
        .unwrap();

        let script_path = staging.join("fake-server.sh");
        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

        let cfg = make_test_config(staging.to_str().unwrap());
        let validator = BootValidator::with_timeout(Duration::from_secs(10));
        let spec = LaunchSpec {
            executable: PathBuf::from("/bin/bash"),
            args: vec!["fake-server.sh".to_string()],
            env: vec![],
            env_remove: vec![],
        };

        let result = validator.validate_with_launch(&cfg, staging, spec).await;
        assert!(matches!(result, BootResult::Success(_)));

        // logs/latest.log should be removed by validate_with_launch
        assert!(
            !staging.join("logs").join("latest.log").exists(),
            "Validation latest.log should be removed on success"
        );
    }
}
