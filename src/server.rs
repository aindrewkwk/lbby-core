use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;

use crate::app_state::AppEventSender;
use crate::config::{ServerConfig, ServerType};
use crate::helpers::{InstallProgress, check_port_available, hide_child_window};
use crate::stats::ServerStats;

const RESTART_WINDOW_SECS: u64 = 300;
const MAX_RESTARTS_IN_WINDOW: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
}

pub struct ServerManager {
    pub status: ServerStatus,
    pub stdin: Option<ChildStdin>,
    pub pid: Option<u32>,
    /// True when the user explicitly clicked Stop (or Restart). False when the
    /// server exited on its own (crash). The wait task uses this to decide
    /// whether to trigger an auto-restart.
    pub stop_requested: bool,
}

impl ServerManager {
    pub fn new() -> Self {
        Self {
            status: ServerStatus::Stopped,
            stdin: None,
            pid: None,
            stop_requested: false,
        }
    }
}

/// Start the Minecraft/Terraria server.
///
/// Thin wrapper that calls `do_start_server` and resets status to Stopped
/// if the start fails while still in the Starting state.
/// Returns a boxed future to allow recursive calls (auto-restart).
pub fn start_server(app: Arc<AppEventSender>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move {
    let result = do_start_server(app.clone()).await;
    if result.is_err() {
        // If we set the status to Starting but the start failed, reset to
        // Stopped so the UI doesn't get stuck on "Starting" forever.
        let state = app.state();
        let mut srv = state.server.lock().await;
        if srv.status == ServerStatus::Starting {
            srv.status = ServerStatus::Stopped;
            srv.stdin = None;
            srv.pid = None;
            app.emit("server-status", ServerStatus::Stopped).ok();
        }
    }
    result
    })
}

/// The actual start logic, callable both from the public command and from the
/// auto-restart task on the wait-for-child handler.
pub async fn do_start_server(app: Arc<AppEventSender>) -> Result<(), String> {
    let cfg = crate::config::load_config();
    if !cfg.setup_complete {
        return Err("Server not set up yet".to_string());
    }
    {
        let state = app.state();
        let srv = state.server.lock().await;
        if srv.status != ServerStatus::Stopped {
            return Err("Server is already running".to_string());
        }
    }

    // Mark the server as Starting immediately — before any Java download or
    // version detection — so the frontend shows the "Starting" status pill
    // right away instead of staying on "Stopped" during network activity.
    {
        let state = app.state();
        let mut srv = state.server.lock().await;
        srv.status = ServerStatus::Starting;
        srv.stop_requested = false;
    }
    app.emit("server-status", ServerStatus::Starting).ok();

    let server_dir = PathBuf::from(&cfg.server_path);

    // ── Terraria / tModLoader start path ──────────────────────────────────
    if cfg.is_terraria() {
        return Err("Terraria start not yet migrated to lbby-core".to_string());
    }

    // Pre-flight: check port availability for Minecraft servers
    let port: u16 = cfg.default_port();
    check_port_available(port)?;

    std::fs::create_dir_all(&server_dir)
        .map_err(|e| format!("Failed to create server directory {}: {}", server_dir.display(), e))?;
    let ram = cfg.ram_mb;
    if matches!(cfg.server_type, ServerType::Forge | ServerType::NeoForge) {
        upsert_managed_jvm_args(&server_dir, &cfg)?;
    }

    // Pre-start cleanup: if a previous run left a `world/session.lock` and
    // an orphan `java`/`bash run.sh` is still holding it, kill it and
    // delete the lock so this start can succeed instead of hitting a
    // confusing `DirectoryLock$LockException` from Forge.
    if cfg.is_minecraft() {
        let lock_path = server_dir.join("world").join("session.lock");
        if lock_path.exists() {
            let pid_file = server_dir.join(".lbby-server.pid");
            #[cfg(unix)]
            {
                if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        kill_unix_process_tree(pid).await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            std::fs::remove_file(&pid_file).ok();
            if let Err(e) = std::fs::remove_file(&lock_path) {
                eprintln!("[lbby] could not remove stale session.lock: {}", e);
            } else {
                let s = app.state();
                s.push_console_line(
                    "[lbby] Removed stale world/session.lock from a previous run".to_string(),
                );
            }
        }
    }

    // Pick the Java version that matches the Minecraft version
    let required_major = crate::java::required_java_for_mc(&cfg.minecraft_version);
    let resolved_java = crate::java::find_java_with_version(required_major);
    let (java_bin, actual_major) = match resolved_java {
        Some(p) => (p, Some(required_major)),
        None => {
            let s = app.state();
            s.push_console_line(
                format!("[lbby] Java {} not found — downloading from Adoptium\u{2026}", required_major),
            );
            match crate::java::ensure_java(required_major, &app).await {
                Ok(path) => (path, Some(required_major)),
                Err(e) => {
                    eprintln!("[lbby] Java download failed: {}", e);
                    let fallback = if !cfg.java_path.is_empty() && std::path::Path::new(&cfg.java_path).exists() {
                        PathBuf::from(&cfg.java_path)
                    } else if cfg!(target_os = "windows") {
                        PathBuf::from("java.exe")
                    } else {
                        PathBuf::from("/usr/bin/java")
                    };
                    let actual = crate::java::detect_java_major(&fallback);
                    (fallback, actual)
                }
            }
        }
    };

    // Java version check
    let install_hint: &str = if cfg!(target_os = "macos") {
        "  \u{2022} Install via: brew install openjdk@17 (or download from https://adoptium.net)"
    } else if cfg!(target_os = "windows") {
        "  \u{2022} Download Eclipse Temurin from https://adoptium.net (pick the matching version)"
    } else {
        "  \u{2022} Install OpenJDK 17 from your distro's package manager or https://adoptium.net"
    };
    match actual_major {
        Some(m) if m < required_major => {
            return Err(format!(
                "Java too old. Minecraft {} (Forge/{}) needs Java {}, but {} has Java {}.\n{}",
                cfg.minecraft_version,
                cfg.loader_version.as_deref().unwrap_or("loader"),
                required_major,
                java_bin.display(),
                m,
                install_hint
            ));
        }
        Some(m) if m > required_major => {
            if matches!(cfg.server_type, ServerType::Forge | ServerType::NeoForge) {
                return Err(format!(
                    "Java {} is too new for Minecraft {} ({}). This server type needs Java {} — \
                     newer JVMs cause the server to hang silently.\n{}",
                    m, cfg.minecraft_version,
                    cfg.loader_version.as_deref().unwrap_or("loader"),
                    required_major, install_hint
                ));
            }
            let s = app.state();
            let warn = format!(
                "[lbby] \u{26a0} Using Java {} (newer than the recommended Java {} for MC {}). \
                 Most newer JVMs work, but if the server hangs at startup, install Java {}.",
                m, required_major, cfg.minecraft_version, required_major
            );
            s.push_console_line(warn.clone());
            app.emit("mc-line", &warn).ok();
        }
        _ => {}
    }

    let java_home = crate::java::java_home_from_bin(&java_bin);
    let banner = format!(
        "[lbby] Using Java {} at {}",
        actual_major.unwrap_or(required_major),
        java_bin.display()
    );
    {
        let s = app.state();
        s.push_console_line(banner.clone());
    }
    app.emit("mc-line", &banner).ok();

    let mut cmd = match cfg.server_type {
        ServerType::Forge | ServerType::NeoForge => {
            #[cfg(target_os = "windows")]
            let mut c = {
                let mut c = tokio::process::Command::new("cmd");
                c.args(["/c", "run.bat", "nogui"]);
                c
            };
            #[cfg(not(target_os = "windows"))]
            let mut c = {
                let mut c = tokio::process::Command::new("/bin/bash");
                c.args(["-c", "./run.sh nogui"]);
                c
            };
            if let Some(jh) = &java_home {
                c.env("JAVA_HOME", jh);
                let bin_dir = jh.join("bin");
                let sep = if cfg!(windows) { ";" } else { ":" };
                let new_path = match std::env::var("PATH") {
                    Ok(p) => format!("{}{}{}", bin_dir.display(), sep, p),
                    Err(_) => bin_dir.display().to_string(),
                };
                c.env("PATH", new_path);
            }
            c
        }
        _ => {
            let mut c = tokio::process::Command::new(&java_bin);
            c.arg(format!("-Xmx{}M", ram));
            c.arg(format!("-Xms{}M", (ram / 2).max(512)));
            if cfg.optimized_jvm_flags {
                c.args(optimized_jvm_flags());
            }
            c.args(["-jar", "server.jar", "nogui"]);
            c
        }
    };

    cmd.env_remove("DYLD_LIBRARY_PATH");
    cmd.env_remove("DYLD_FALLBACK_LIBRARY_PATH");
    cmd.env_remove("DYLD_FRAMEWORK_PATH");
    cmd.env_remove("DYLD_ROOT_PATH");
    cmd.env_remove("DYLD_IMAGE_SUFFIX");
    cmd.env_remove("DYLD_SHARED_FILE");
    cmd.env_remove("DYLD_INSERT_LIBRARIES");
    cmd.env_remove("DYLD_FORCE_FLAT_NAMESPACE");

    cmd.current_dir(&server_dir)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_child_window(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| format!("Failed to start server: {}", e))?;
    let stdin = child.stdin.take().ok_or("No stdin")?;
    let stdout = child.stdout.take().ok_or("No stdout")?;
    let stderr = child.stderr.take().ok_or("No stderr")?;
    let pid = child.id();
    drop(child); // Release the child handle — streams are taken, process runs independently

    {
        let state = app.state();
        let mut srv = state.server.lock().await;
        srv.stdin = Some(stdin);
        srv.pid = pid;
    }

    if let Some(p) = pid {
        let _ = std::fs::write(server_dir.join(".lbby-server.pid"), p.to_string());
    }

    let players_max = cfg.max_players;
    {
        let s = app.state();
        let mut st = s.stats.lock().await;
        *st = ServerStats::default();
        st.players_max = players_max;
        st.ram_max_mb = ram;
    }

    // Spawn stdout reader task — detects "Done" line, parses TPS / player events,
    // and feeds the console buffer.
    let app3 = app.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("Done") && line.contains("For help") {
                let s = app3.state();
                let mut srv = s.server.lock().await;
                if srv.status == ServerStatus::Starting {
                    srv.status = ServerStatus::Running;
                    app3.emit("server-status", ServerStatus::Running).ok();
                }
            }
            if let Some(tps) = parse_tps_line(&line) {
                let s = app3.state();
                let mut st = s.stats.lock().await;
                st.tps = tps;
                app3.emit("server-stats", st.clone()).ok();
            }
            if let Some(tick) = parse_gametime_line(&line) {
                let s = app3.state();
                let now = std::time::Instant::now();
                let mut last = s.last_gametime_sample.lock().await;
                if let Some((prev_tick, prev_inst)) = *last {
                    let elapsed_secs = now.duration_since(prev_inst).as_secs_f32();
                    let tick_delta = tick.saturating_sub(prev_tick) as f32;
                    if elapsed_secs > 0.5 && tick_delta >= 0.0 {
                        let tps = (tick_delta / elapsed_secs).clamp(0.0, 20.0);
                        let mut st = s.stats.lock().await;
                        st.tps = tps;
                        app3.emit("server-stats", st.clone()).ok();
                    }
                }
                *last = Some((tick, now));
            }
            // Extract IP from "logged in" lines (separate from "joined the game")
            if let Some((login_name, ip)) = parse_player_login_ip(&line) {
                let _ = crate::player_stats::record_player_ip(&app3, &login_name, &ip);
                app3.emit("player-ip-update", serde_json::json!({ "name": login_name, "ip": ip })).ok();
            }
            if let Some((name, is_join)) = parse_player_event(&line) {
                let s = app3.state();
                let mut players = s.online_players.lock().await;
                if is_join { players.insert(name.clone()); }
                else       { players.remove(&name); }
                let list: Vec<String> = players.iter().cloned().collect();
                let count = players.len() as u32;
                drop(players);

                if is_join {
                    s.record_player_join(name.clone());
                    app3.emit("recent-players-update", s.recent_players.lock().await.iter().cloned().collect::<Vec<_>>()).ok();
                }

                let mut st = s.stats.lock().await;
                st.players_online = count;
                app3.emit("server-stats", st.clone()).ok();
                app3.emit("players-update", &list).ok();
            }
            {
                let s = app3.state();
                s.push_console_line(line.clone());
            }
            app3.emit("mc-line", &line).ok();
        }
        // Stream ended — server process exited
        let s = app3.state();
        let was_unexpected = {
            let srv = s.server.lock().await;
            !srv.stop_requested
        };
        {
            let mut srv = s.server.lock().await;
            srv.status = ServerStatus::Stopped;
            srv.stdin = None;
            srv.pid = None;
        }
        app3.emit("server-status", ServerStatus::Stopped).ok();
        let cfg = crate::config::load_config();
        let _ = std::fs::remove_file(
            PathBuf::from(&cfg.server_path).join(".lbby-server.pid"),
        );
        let mut st = s.stats.lock().await;
        *st = ServerStats::default();
        app3.emit("server-stats", st.clone()).ok();
        let mut players = s.online_players.lock().await;
        players.clear();
        app3.emit("players-update", &Vec::<String>::new()).ok();
        drop(players);
        drop(st);

        // Auto-restart if enabled and exit was unexpected
        if was_unexpected && cfg.auto_restart {
            let now = std::time::Instant::now();
            let window = std::time::Duration::from_secs(RESTART_WINDOW_SECS);
            let allow = {
                let mut hist = s.recent_auto_restarts.lock().await;
                while let Some(&front) = hist.front() {
                    if now.duration_since(front) > window { hist.pop_front(); } else { break; }
                }
                if hist.len() >= MAX_RESTARTS_IN_WINDOW {
                    false
                } else {
                    hist.push_back(now);
                    true
                }
            };

            if !allow {
                let msg = format!(
                    "[lbby] Auto-restart disabled — server crashed {}+ times in {} seconds. Fix the issue and start manually.",
                    MAX_RESTARTS_IN_WINDOW, RESTART_WINDOW_SECS
                );
                s.push_console_line(msg.clone());
                app3.emit("mc-line", &msg).ok();
                return;
            }

            let msg = "[lbby] Server exited unexpectedly. Auto-restart in 3s…".to_string();
            s.push_console_line(msg.clone());
            app3.emit("mc-line", &msg).ok();
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let _ = crate::server::start_server(app3).await;
        }
    });

    // Spawn stderr reader task
    let app2 = app.clone();
    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let formatted = format!("[stderr] {}", line);
            let s = app2.state();
            s.push_console_line(formatted.clone());
            app2.emit("mc-line", &formatted).ok();
        }
    });

    // ── Stats poller task ──────────────────────────────────────────────────
    let app_stats = app.clone();
    let started_at = std::time::Instant::now();
    let server_dir_for_stats = server_dir.clone();
    let loader_label = cfg.server_type.label().to_string();
    let mods_folder = extras_folder(&cfg);
    let cfg_stats = cfg.clone();

    tokio::spawn(async move {
        let cfg = cfg_stats;
        let mut poller = crate::stats::StatsPoller::new();
        let mut tick: u32 = 0;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let s = app_stats.state();
            let server_pid = {
                let srv = s.server.lock().await;
                if srv.status == ServerStatus::Stopped { break; }
                match srv.pid { Some(p) => p, None => continue }
            };
            if let Some((cpu, ram_mb, disk_r, disk_w, disk_used, disk_total)) =
                poller.sample(server_pid, &server_dir_for_stats)
            {
                let mut st = s.stats.lock().await;
                st.cpu_percent = cpu;
                st.ram_used_mb = ram_mb;
                st.disk_read_kb_s = disk_r;
                st.disk_write_kb_s = disk_w;
                st.disk_used_mb = disk_used;
                st.disk_total_mb = disk_total;
                st.uptime_seconds = started_at.elapsed().as_secs();
                st.loader_label = loader_label.clone();
                if tick.is_multiple_of(5) {
                    st.mods_count = crate::stats::count_mods(&server_dir_for_stats.join(&mods_folder));
                    let world_dir = if cfg.is_terraria() {
                        server_dir_for_stats.join("Worlds")
                    } else {
                        server_dir_for_stats.join("world")
                    };
                    st.world_size_mb = crate::stats::dir_size_mb(&world_dir);
                }
                tick = tick.wrapping_add(1);
                app_stats.emit("server-stats", st.clone()).ok();
            }
        }
    });

    // ── Reset gametime tracker ─────────────────────────────────────────────
    {
        let state = app.state();
        *state.last_gametime_sample.lock().await = None;
    }

    // ── TPS query loop ─────────────────────────────────────────────────────
    let tps_cmd: Option<&'static str> = match cfg.server_type {
        ServerType::Paper | ServerType::Bukkit | ServerType::Spigot
        | ServerType::Folia | ServerType::Purpur => Some("tps\n"),
        ServerType::Forge | ServerType::NeoForge => Some("forge tps\n"),
        ServerType::Vanilla | ServerType::Fabric
        | ServerType::SpongeVanilla | ServerType::SpongeForge => Some("time query gametime\n"),
        ServerType::Terraria | ServerType::TModLoader => None,
    };
    if let Some(cmd_bytes) = tps_cmd {
        let app_tps = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
            loop {
                let s = app_tps.state();
                {
                    let mut srv = s.server.lock().await;
                    if srv.status == ServerStatus::Stopped { break; }
                    if srv.status == ServerStatus::Running {
                        if let Some(stdin) = &mut srv.stdin {
                            let _ = tokio::io::AsyncWriteExt::write_all(stdin, cmd_bytes.as_bytes()).await;
                        }
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            }
        });
    }

    Ok(())
}

/// Gracefully stop the Minecraft server.
///
/// Sends `stop` to stdin, waits up to 10 seconds for a clean exit, then
/// force-kills the entire process tree if it is still alive.
pub async fn stop_server(app: Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    let state = app.state();
    {
        let srv = state.server.lock().await;
        if srv.status == ServerStatus::Stopped {
            return Err("Server is not running".to_string());
        }
    }
    graceful_or_force_stop_server(&app, 10).await;
    Ok(())
}

/// Hard-kill variant — only waits 3 seconds before force-killing.
pub async fn force_stop_server(app: Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    graceful_or_force_stop_server(&app, 3).await;
    Ok(())
}

/// Restart the server: stop, wait a moment, then start again.
pub async fn restart_server(app: Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    stop_server(app.clone()).await?;
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    let result = crate::helpers::do_start_server(app.clone()).await;
    if result.is_err() {
        let state = app.state();
        let mut srv = state.server.lock().await;
        if srv.status == ServerStatus::Starting {
            srv.status = ServerStatus::Stopped;
            srv.stdin = None;
            srv.pid = None;
            app.emit("server-status", ServerStatus::Stopped).ok();
        }
    }
    result
}

/// Send a command string to the running server's stdin.
pub async fn send_command(app: Arc<crate::app_state::AppEventSender>, cmd: &str) -> Result<(), String> {
    let state = app.state();
    let mut srv = state.server.lock().await;
    if let Some(stdin) = &mut srv.stdin {
        stdin
            .write_all(format!("{}\n", cmd).as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    } else if srv.status == ServerStatus::Running {
        Err("Server stdin is not available".to_string())
    } else {
        Err("Server is not running".to_string())
    }
}

// ── Internal: graceful-or-force stop ───────────────────────────────────────

/// Send `stop` to the server, wait up to `grace_secs` for a clean exit, and
/// force-kill the entire process tree if it is still alive. Also clears stale
/// `world/session.lock` so the next start won't hit a lock error.
async fn graceful_or_force_stop_server(app: &Arc<crate::app_state::AppEventSender>, grace_secs: u64) {
    let state = app.state();

    // Phase 1: request graceful stop (if running)
    let pid_to_watch: Option<u32> = {
        let cfg = crate::config::load_config();
        let mut srv = state.server.lock().await;
        match srv.status {
            ServerStatus::Running | ServerStatus::Starting => {
                srv.stop_requested = true;
                if let Some(stdin) = &mut srv.stdin {
                    if cfg.is_terraria() {
                        stdin.write_all(b"save\n").await.ok();
                        stdin.write_all(b"exit\n").await.ok();
                    } else {
                        stdin.write_all(b"stop\n").await.ok();
                    }
                }
                srv.status = ServerStatus::Stopping;
                app.emit("server-status", ServerStatus::Stopping).ok();
                srv.pid
            }
            ServerStatus::Stopping => srv.pid,
            ServerStatus::Stopped => None,
        }
    };

    if pid_to_watch.is_none() {
        return;
    }

    // Phase 2: wait for the server to exit cleanly
    let max_iters = (grace_secs * 2).max(2);
    for _ in 0..max_iters {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        let stopped = {
            let srv = state.server.lock().await;
            srv.status == ServerStatus::Stopped || srv.pid.is_none()
        };
        if stopped {
            return;
        }
    }

    // Phase 3: force-kill the stuck process tree
    let stuck_pid = match state.server.lock().await.pid {
        Some(p) => p,
        None => return,
    };
    eprintln!(
        "[lbby] graceful stop timed out after {}s — force-killing PID {} and descendants",
        grace_secs, stuck_pid
    );

    #[cfg(unix)]
    {
        kill_unix_process_tree(stuck_pid).await;
    }
    #[cfg(windows)]
    {
        let mut cmd = tokio::process::Command::new("taskkill");
        cmd.args(["/PID", &stuck_pid.to_string(), "/T", "/F"]);
        crate::helpers::hide_child_window(&mut cmd);
        cmd.output().await.ok();
    }

    // Phase 4: clear stale session lock so next start can boot
    let cfg = crate::config::load_config();
    if cfg.is_minecraft() {
        let lock = std::path::PathBuf::from(&cfg.server_path).join("world").join("session.lock");
        if lock.exists() {
            std::fs::remove_file(&lock).ok();
        }
    }

    // Phase 5: reset state
    let mut srv = state.server.lock().await;
    srv.status = ServerStatus::Stopped;
    srv.stdin = None;
    srv.pid = None;
    app.emit("server-status", ServerStatus::Stopped).ok();
}

// ── Extracted generic server functions ────────────────────────────────────────

/// Validate that a Terraria server directory has the files needed to launch.
///
/// Checks for the server binary and (on Windows) companion DLLs that the
/// binary needs to load. Returns Ok(()) if everything looks good, or a
/// descriptive error if something is missing.
pub fn validate_terraria_deps(server_dir: &Path, server_type: &crate::config::ServerType) -> Result<(), String> {
    match server_type {
        crate::config::ServerType::TModLoader => {
            let tmod_dll = server_dir.join("tModLoader.dll");
            if !tmod_dll.exists() {
                return Err(format!(
                    "tModLoader.dll not found at {}. Please reinstall tModLoader from Settings \u{2192} Version.",
                    tmod_dll.display()
                ));
            }
            // Check for bundled dotnet
            let dotnet_dir = server_dir.join("dotnet");
            let dotnet_exe = if cfg!(target_os = "windows") {
                dotnet_dir.join("dotnet.exe")
            } else {
                dotnet_dir.join("dotnet")
            };
            if !dotnet_exe.exists() {
                // Will fall back to system dotnet — just warn, don't fail
                eprintln!("[lbby] Warning: bundled dotnet not found at {}, will try system dotnet", dotnet_exe.display());
            }
        }
        crate::config::ServerType::Terraria => {
            let binary = terraria_server_binary(server_dir);
            if !binary.exists() {
                // Check if tModLoader fallback is available
                let tmod_dll = server_dir.join("tModLoader.dll");
                if !tmod_dll.exists() {
                    return Err(format!(
                        "TerrariaServer binary not found at {}. Please reinstall the server from Settings \u{2192} Version.",
                        binary.display()
                    ));
                }
                // tModLoader fallback is available — that's fine
                return Ok(());
            }
            // Binary exists — check for companion DLLs on Windows
            #[cfg(target_os = "windows")]
            {
                let parent = binary.parent().unwrap_or(server_dir);
                let has_companion = parent.join("SDL2.dll").exists()
                    || parent.join("MonoGame.Framework.dll").exists()
                    || parent.join("FNA.dll").exists()
                    || parent.join("Terraria.exe").exists(); // full game install has Terraria.exe
                if !has_companion {
                    // Check if there are any .dll files at all (SteamCMD copy may use different names)
                    let has_any_dll = std::fs::read_dir(parent)
                        .ok()
                        .and_then(|mut entries| entries.find_map(|e| {
                            e.ok().filter(|e| e.path().extension().map_or(false, |ext| ext == "dll"))
                                .map(|_| true)
                        }))
                        .unwrap_or(false);
                    if !has_any_dll {
                        return Err(format!(
                            "TerrariaServer.exe is missing required companion files (DLLs). \
                             Please reinstall the server from Settings \u{2192} Version, or copy the \
                             full server directory from your Terraria Steam installation."
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Discover an existing world file in the server's Worlds directory.
///
/// Priority:
/// 1. Exact match: `{server_name}.wld`
/// 2. Any single `.wld` file in the directory
/// 3. None (caller should use -autocreate)
pub fn discover_world_file(server_dir: &Path, server_name: &str) -> Option<PathBuf> {
    let worlds_dir = server_dir.join("Worlds");

    // 1. Exact name match
    let exact = worlds_dir.join(format!("{}.wld", server_name));
    if exact.exists() {
        return Some(exact);
    }

    // 2. Any single .wld file
    let entries: Vec<PathBuf> = std::fs::read_dir(&worlds_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "wld"))
        .map(|e| e.path())
        .collect();

    match entries.len() {
        0 => None,
        1 => Some(entries.into_iter().next().unwrap()),
        _ => {
            // Multiple worlds — pick the most recently modified
            entries.into_iter().max_by_key(|p| {
                std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            })
        }
    }
}

/// Returns optimized JVM flags for Minecraft servers.
pub fn optimized_jvm_flags() -> &'static [&'static str] {
    &[
        "-XX:+UseG1GC",
        "-XX:+ParallelRefProcEnabled",
        "-XX:MaxGCPauseMillis=200",
        "-XX:+UnlockExperimentalVMOptions",
        "-XX:+DisableExplicitGC",
        "-XX:+AlwaysPreTouch",
        "-XX:G1NewSizePercent=30",
        "-XX:G1MaxNewSizePercent=40",
        "-XX:G1HeapRegionSize=8M",
        "-XX:G1ReservePercent=20",
        "-XX:G1HeapWastePercent=5",
        "-XX:G1MixedGCCountTarget=4",
        "-XX:InitiatingHeapOccupancyPercent=15",
        "-XX:G1MixedGCLiveThresholdPercent=90",
        "-XX:G1RSetUpdatingPauseTimePercent=5",
        "-XX:SurvivorRatio=32",
        "-XX:+PerfDisableSharedMem",
        "-XX:MaxTenuringThreshold=1",
    ]
}

/// Manages JVM args in user_jvm_args.txt, replacing only the managed block.
pub fn upsert_managed_jvm_args(server_dir: &Path, cfg: &crate::config::ServerConfig) -> Result<(), String> {
    std::fs::create_dir_all(server_dir)
        .map_err(|e| format!("Failed to create server directory {}: {}", server_dir.display(), e))?;
    let path = server_dir.join("user_jvm_args.txt");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let start = "# Lbby managed JVM flags - start";
    let end = "# Lbby managed JVM flags - end";

    let mut kept = Vec::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line.trim() == start {
            skipping = true;
            continue;
        }
        if line.trim() == end {
            skipping = false;
            continue;
        }
        if !skipping {
            kept.push(line.to_string());
        }
    }

    let mut managed = vec![
        start.to_string(),
        format!("-Xmx{}M", cfg.ram_mb),
        format!("-Xms{}M", (cfg.ram_mb / 2).max(512)),
    ];
    if cfg.optimized_jvm_flags {
        managed.extend(optimized_jvm_flags().iter().map(|s| s.to_string()));
    }
    managed.push(end.to_string());

    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let mut output = managed.join("\n");
    if !kept.is_empty() {
        output.push_str("\n\n");
        output.push_str(&kept.join("\n"));
    }
    output.push('\n');

    std::fs::write(&path, output)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

/// Check whether a string is a valid Minecraft player name (2-16 alphanumeric/underscore chars).
fn is_valid_player_name(s: &str) -> bool {
    let len = s.chars().count();
    (2..=16).contains(&len) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a player name out of a "joined the game" / "left the game" / "lost connection" line.
/// MC log format example: "...MinecraftServer/]: Fishgod212 joined the game"
pub fn parse_player_event(line: &str) -> Option<(String, bool)> {
    // Returns (name, is_join)
    let body = line.rsplit("]:").next()?.trim();
    if let Some(name) = body.strip_suffix(" joined the game") {
        let n = name.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), true)); }
    }
    if let Some(name) = body.strip_suffix(" left the game") {
        let n = name.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    if let Some(rest) = body.strip_suffix(": Disconnected") {
        if let Some(name) = rest.strip_suffix(" lost connection") {
            let n = name.trim();
            if is_valid_player_name(n) { return Some((n.to_string(), false)); }
        }
    }
    // Generic "<name> lost connection: <reason>"
    if let Some(idx) = body.find(" lost connection:") {
        let n = body[..idx].trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    None
}

/// Parse a player's IP address from a Minecraft "logged in" line.
/// MC log format: "...MinecraftServer/]: PlayerName[/192.168.1.1:25565] logged in with entity id ..."
/// Returns (player_name, ip_address) on success.
pub fn parse_player_login_ip(line: &str) -> Option<(String, String)> {
    let body = line.rsplit("]:").next()?.trim();
    // Must contain " logged in"
    if !body.contains(" logged in") {
        return None;
    }
    // Find the [/ip:port] part — look for "[/" after the player name
    let login_pos = body.find(" logged in")?;
    let before_login = &body[..login_pos];
    // Pattern: "PlayerName[/ip:port]"
    let bracket_start = before_login.find("[/")?;
    let name = before_login[..bracket_start].trim();
    if !is_valid_player_name(name) {
        return None;
    }
    let bracket_end = before_login[bracket_start..].find(']')?;
    let ip_port = &before_login[bracket_start + 2..bracket_start + bracket_end];
    // Strip the port — we only want the IP
    let ip = if let Some(colon_pos) = ip_port.rfind(':') {
        &ip_port[..colon_pos]
    } else {
        ip_port
    };
    // Skip local/private IPs (127.x, 0:0:0:0, etc.)
    if ip == "127.0.0.1" || ip == "0:0:0:0:0:0:0:1" || ip == "localhost" {
        return None;
    }
    Some((name.to_string(), ip.to_string()))
}

/// Parse Terraria/tModLoader player join/leave messages.
/// Terraria format: "<name> has joined." / "<name> has left."
/// tModLoader may also use: "<name> has joined" (no period)
pub fn parse_terraria_player_event(line: &str) -> Option<(String, bool)> {
    let trimmed = line.trim();
    // Join: "<name> has joined." or "<name> has joined"
    if let Some(rest) = trimmed.strip_suffix(" has joined.") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), true)); }
    }
    if let Some(rest) = trimmed.strip_suffix(" has joined") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), true)); }
    }
    // Leave: "<name> has left." or "<name> has left"
    if let Some(rest) = trimmed.strip_suffix(" has left.") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    if let Some(rest) = trimmed.strip_suffix(" has left") {
        let n = rest.trim();
        if is_valid_player_name(n) { return Some((n.to_string(), false)); }
    }
    None
}

/// Write common Terraria serverconfig.txt settings for both TModLoader and
/// vanilla-Terraria-fallback-to-TModLoader paths.  Creates the Worlds
/// directory, decides between `-world` (existing) and `-autocreate` (new),
/// and writes the key/value pairs via `update_config_keys`.
pub fn write_terraria_serverconfig(cfg: &crate::config::ServerConfig, server_dir: &std::path::Path) {
    let worlds_dir = server_dir.join("Worlds");
    let config_path = server_dir.join("serverconfig.txt");
    let _ = std::fs::create_dir_all(&worlds_dir);
    let mut updates = std::collections::HashMap::new();
    updates.insert("port".to_string(), cfg.default_port().to_string());
    updates.insert("maxplayers".to_string(), cfg.max_players.to_string());
    updates.insert("motd".to_string(), cfg.server_name.clone());
    updates.insert("worldname".to_string(), cfg.server_name.clone());
    updates.insert("difficulty".to_string(), cfg.terraria_difficulty.to_string());
    updates.insert("secure".to_string(), "0".to_string());
    updates.insert("npcstream".to_string(), "4".to_string());
    updates.insert("language".to_string(), "en-US".to_string());
    // New fields: seed, evil, password
    if !cfg.terraria_seed.is_empty() {
        updates.insert("seed".to_string(), cfg.terraria_seed.clone());
    }
    if cfg.terraria_evil > 0 {
        updates.insert("evil".to_string(), cfg.terraria_evil.to_string());
    }
    if !cfg.terraria_password.is_empty() {
        updates.insert("password".to_string(), cfg.terraria_password.clone());
    }
    // tModLoader-specific: modpath, modpack
    if cfg.server_type == crate::config::ServerType::TModLoader {
        if !cfg.tmod_modpath.is_empty() {
            updates.insert("modpath".to_string(), cfg.tmod_modpath.clone());
        }
        if !cfg.tmod_modpack.is_empty() {
            updates.insert("modpack".to_string(), cfg.tmod_modpack.clone());
        }
    }
    // Use discover_world_file to find existing worlds (exact name match first,
    // then any .wld file, then fall back to autocreate).
    if let Some(world_file) = discover_world_file(server_dir, &cfg.server_name) {
        updates.insert("world".to_string(), world_file.to_string_lossy().to_string());
    } else {
        updates.insert("autocreate".to_string(), cfg.terraria_world_size.to_string());
    }
    if let Err(e) = crate::terraria_config::update_config_keys(&config_path, &updates) {
        eprintln!("[lbby] Warning: failed to update serverconfig.txt: {}", e);
    }
}

/// Get the path to the TerrariaServer binary in a given directory.
/// Checks multiple possible locations (direct, inside .app bundle, subdirectories).
pub fn terraria_server_binary(server_dir: &Path) -> PathBuf {
    // Direct binary
    let direct = if cfg!(target_os = "windows") {
        server_dir.join("TerrariaServer.exe")
    } else {
        server_dir.join("TerrariaServer")
    };
    if direct.exists() {
        return direct;
    }

    // macOS: inside .app bundle
    let app_bundle = server_dir.join("Terraria Server.app/Contents/MacOS/TerrariaServer");
    if app_bundle.exists() {
        return app_bundle;
    }

    // Windows: TerrariaServer.exe might be in a subdirectory
    if cfg!(target_os = "windows") {
        let nested = server_dir.join("Windows").join("TerrariaServer.exe");
        if nested.exists() {
            return nested;
        }
    }

    // Linux: binary might have architecture suffix
    let linux64 = server_dir.join("Linux").join("TerrariaServer.bin.x86_64");
    if linux64.exists() {
        return linux64;
    }
    let linux32 = server_dir.join("Linux").join("TerrariaServer.bin.x86");
    if linux32.exists() {
        return linux32;
    }

    // Return the default path even if it doesn't exist (for error messages)
    direct
}

/// Find the TerrariaServer binary by searching common locations.
/// Used during installation to locate the binary after download/extraction.
pub fn find_terraria_binary(dir: &Path) -> Option<PathBuf> {
    let candidates = vec![
        dir.join("TerrariaServer"),
        dir.join("TerrariaServer.exe"),
        dir.join("Terraria Server.app/Contents/MacOS/TerrariaServer"),
        dir.join("Windows/TerrariaServer.exe"),
        dir.join("Linux/TerrariaServer.bin.x86_64"),
        dir.join("Linux/TerrariaServer.bin.x86"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Recursive search as last resort
    for entry in walkdir::WalkDir::new(dir).max_depth(3).into_iter().flatten() {
        let name = entry.file_name().to_string_lossy();
        if name == "TerrariaServer" || name == "TerrariaServer.exe" || name.starts_with("TerrariaServer.bin") {
            return Some(entry.path().to_path_buf());
        }
    }

    None
}

/// Parse a "The time is N" log line that responds to `time query gametime`.
/// Returns the in-game tick count.
pub fn parse_gametime_line(line: &str) -> Option<u64> {
    // Format: "[12:34:56] [Server thread/INFO]: The time is 12345"
    let body = line.rsplit("]:").next()?.trim();
    let rest = body.strip_prefix("The time is ")?;
    rest.trim().parse().ok()
}

/// Parse a TPS value out of an MC log line.
/// Recognised formats:
///   - Paper:  "TPS from last 1m, 5m, 15m: {color}*20.0, {color}*20.0, {color}*20.0"
///   - Forge:  "Overall: Mean tick time: 12.345 ms. Mean TPS: 20.000"
///   - NeoForge: same as Forge
pub fn parse_tps_line(line: &str) -> Option<f32> {
    if line.contains("Mean TPS:") {
        let after = line.rsplit("Mean TPS:").next()?;
        let tok = after.split_whitespace().next()?;
        let cleaned = tok.trim_end_matches('.');
        let v: f32 = cleaned.parse().ok()?;
        if v > 0.0 && v <= 25.0 { return Some(v); }
    }
    if line.contains("TPS from last") {
        // Strip Minecraft color codes ({color} + 1 char) and the "*" marker
        let after = line.split(':').nth(1)?;
        let cleaned: String = after.chars().filter(|c| !matches!(c, '\u{a7}' | '*')).collect();
        // Walk and grab the first plausible TPS value (1-25)
        let mut buf = String::new();
        for c in cleaned.chars() {
            if c.is_ascii_digit() || c == '.' { buf.push(c); }
            else if !buf.is_empty() {
                if let Ok(v) = buf.parse::<f32>() {
                    if v > 0.0 && v <= 25.0 { return Some(v); }
                }
                buf.clear();
            }
        }
        if !buf.is_empty() {
            if let Ok(v) = buf.parse::<f32>() { if v > 0.0 && v <= 25.0 { return Some(v); } }
        }
    }
    None
}

/// Read banned players from the server's ban storage (JSON for MC, txt for Terraria).
pub async fn get_banned_players() -> Result<Vec<crate::app_state::BannedPlayer>, String> {
    let cfg = crate::config::load_config();
    if cfg.is_terraria() {
        // Terraria stores bans in banlist.txt — one player name per line.
        let path = PathBuf::from(&cfg.server_path).join("banlist.txt");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let mut players: Vec<crate::app_state::BannedPlayer> = raw
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|name| crate::app_state::BannedPlayer {
                name: name.to_string(),
                uuid: String::new(),
                created: String::new(),
                source: "Server".to_string(),
                expires: "forever".to_string(),
                reason: String::new(),
            })
            .collect();
        players.sort_by_key(|a| a.name.to_lowercase());
        Ok(players)
    } else {
        // Minecraft stores bans in banned-players.json.
        let path = PathBuf::from(&cfg.server_path).join("banned-players.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut players: Vec<crate::app_state::BannedPlayer> = serde_json::from_str(&raw)
            .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
        players.sort_by_key(|a| a.name.to_lowercase());
        Ok(players)
    }
}

/// Read banned IPs from the server's banned-ips.json (Minecraft only).
/// Terraria does not support IP bans natively.
pub async fn get_banned_ips() -> Result<Vec<crate::app_state::BannedIp>, String> {
    let cfg = crate::config::load_config();
    if cfg.is_terraria() {
        // Terraria has no native IP ban support
        return Ok(Vec::new());
    }
    let path = PathBuf::from(&cfg.server_path).join("banned-ips.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut ips: Vec<crate::app_state::BannedIp> = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    ips.sort_by_key(|a| a.ip.clone());
    Ok(ips)
}

use std::sync::Mutex;
use std::time::SystemTime;

static PROPS_CACHE: Mutex<Option<(HashMap<String, String>, SystemTime)>> = Mutex::new(None);

/// Read and parse server.properties into a key-value map.
/// Uses in-memory cache with file-modification-time check.
pub fn get_server_properties() -> Result<HashMap<String, String>, String> {
    let cfg = crate::config::load_config();
    let path = PathBuf::from(&cfg.server_path).join("server.properties");

    // Check cache
    if let Ok(meta) = std::fs::metadata(&path) {
        if let Ok(modified) = meta.modified() {
            let cache = PROPS_CACHE.lock().unwrap();
            if let Some((ref cached, cached_time)) = *cache {
                if modified == cached_time {
                    return Ok(cached.clone());
                }
            }
        }
    }

    // Cache miss
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    // Update cache
    if let Ok(meta) = std::fs::metadata(&path) {
        if let Ok(modified) = meta.modified() {
            *PROPS_CACHE.lock().unwrap() = Some((map.clone(), modified));
        }
    }

    Ok(map)
}

/// Write a key-value map to server.properties.
pub fn save_server_properties(props: HashMap<String, String>) -> Result<(), String> {
    let cfg = crate::config::load_config();
    let path = PathBuf::from(&cfg.server_path).join("server.properties");
    let mut lines = vec!["# Minecraft server properties".to_string()];
    let mut sorted: Vec<_> = props.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in sorted {
        lines.push(format!("{}={}", k, v));
    }
    let result = std::fs::write(&path, lines.join("\n") + "\n").map_err(|e| e.to_string());
    // Invalidate cache
    *PROPS_CACHE.lock().unwrap() = None;
    result
}

/// Save an uploaded PNG as server-icon.png after validation.
pub async fn upload_server_icon(file_path: String) -> Result<(), String> {
    let cfg = crate::config::load_config();
    if cfg.server_path.trim().is_empty() {
        return Err("Server path is not configured".to_string());
    }
    let src = PathBuf::from(file_path);
    let bytes = tokio::fs::read(&src).await.map_err(|e| e.to_string())?;
    validate_server_icon_png(&bytes)?;
    let dest = PathBuf::from(&cfg.server_path).join("server-icon.png");
    tokio::fs::write(&dest, bytes)
        .await
        .map_err(|e| format!("Failed to write {}: {}", dest.display(), e))?;
    Ok(())
}

/// Validate that a PNG is exactly 64x64 pixels (Minecraft server icon requirement).
pub fn validate_server_icon_png(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("Server icon must be a PNG file".to_string());
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width != 64 || height != 64 {
        return Err(format!("Server icon must be exactly 64x64 pixels, got {}x{}", width, height));
    }
    Ok(())
}

/// Delete the world folder so the server generates a fresh one on next start.
/// This is needed when changing the seed — Minecraft won't apply a new seed
/// to an existing world.
pub async fn regenerate_world() -> Result<(), String> {
    let cfg = crate::config::load_config();
    if cfg.is_terraria() {
        return Err("World regeneration is not supported for Terraria. Delete the .wld file manually.".to_string());
    }
    let server_dir = std::path::PathBuf::from(&cfg.server_path);
    let world_dir = server_dir.join("world");
    if !world_dir.exists() {
        return Ok(());
    }
    // Backup the old world before deleting
    let backup_name = format!("world-backup-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let backup_dir = server_dir.join(&backup_name);
    tokio::fs::rename(&world_dir, &backup_dir)
        .await
        .map_err(|e| format!("Failed to backup world: {}", e))?;
    Ok(())
}

/// Returns the extras subfolder name for the given server type
/// (plugins for Bukkit-family, Mods for Terraria, mods for everything else).
pub fn extras_folder(cfg: &crate::config::ServerConfig) -> &'static str {
    match cfg.server_type {
        crate::config::ServerType::Paper | crate::config::ServerType::Bukkit | crate::config::ServerType::Spigot
        | crate::config::ServerType::Folia | crate::config::ServerType::Purpur => "plugins",
        crate::config::ServerType::Terraria | crate::config::ServerType::TModLoader => "Mods",
        _ => "mods",
    }
}

/// Kill `root_pid` and every descendant on Unix-like systems by walking
/// `pgrep -P` recursively. SIGTERM first, then SIGKILL after a brief grace.
#[cfg(unix)]
pub async fn kill_unix_process_tree(root_pid: u32) {
    fn collect_descendants(root: u32) -> Vec<u32> {
        let mut all = vec![root];
        let mut frontier = vec![root];
        while let Some(p) = frontier.pop() {
            if let Ok(out) = std::process::Command::new("pgrep").args(["-P", &p.to_string()]).output() {
                if out.status.success() {
                    for line in String::from_utf8_lossy(&out.stdout).lines() {
                        if let Ok(child) = line.trim().parse::<u32>() {
                            // Verify the child's parent is still `p` to guard against
                            // PID recycling: a PID returned by pgrep may have been
                            // reused by an unrelated process since we started.
                            let check = std::process::Command::new("ps")
                                .args(["-o", "ppid=", "-p", &child.to_string()])
                                .output();
                            if let Ok(chk_out) = check {
                                let ppid = String::from_utf8_lossy(&chk_out.stdout)
                                    .trim().parse::<u32>().unwrap_or(0);
                                if ppid != p { continue; }
                            }
                            all.push(child);
                            frontier.push(child);
                        }
                    }
                }
            }
        }
        all
    }
    let pids = collect_descendants(root_pid);
    // SIGTERM (15) the whole tree
    for pid in &pids {
        unsafe { libc::kill(*pid as i32, libc::SIGTERM); }
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    // SIGKILL (9) any survivors
    for pid in &pids {
        unsafe { libc::kill(*pid as i32, libc::SIGKILL); }
    }
}

// ── Server installation ──────────────────────────────────────────────────────

// Internal deserialization types for Minecraft version/installer APIs.

#[derive(Debug, Deserialize)]
struct VersionManifest {
    versions: Vec<VersionEntry>,
}

#[derive(Debug, Deserialize)]
struct VersionEntry {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct VersionData {
    downloads: Downloads,
}

#[derive(Debug, Deserialize)]
struct Downloads {
    server: Option<DownloadItem>,
}

#[derive(Debug, Deserialize)]
struct DownloadItem {
    url: String,
}

#[derive(Debug, Deserialize)]
struct PaperBuild {
    downloads: PaperDownloads,
}

#[derive(Debug, Deserialize)]
struct PaperDownloads {
    application: PaperApp,
}

#[derive(Debug, Deserialize)]
struct PaperApp {
    name: String,
}

// ── Download + progress helpers ──────────────────────────────────────────────

/// Download a file from a URL to a local path, emitting progress events.
async fn download_file(app: &Arc<AppEventSender>, url: &str, dest: &Path, label: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("MCHost/0.1")
        .build().map_err(|e| e.to_string())?;

    let resp = client.get(url).send().await.map_err(|e| format!("Download failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Download failed with HTTP {} for {}", resp.status(), url));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        let progress = if total > 0 { downloaded as f32 / total as f32 } else { 0.0 };
        app.emit("install-progress", InstallProgress {
            stage: "download".to_string(),
            label: format!("Downloading {}\u{2026} {:.1}%", label, progress * 100.0),
            current: downloaded as u32,
            total: total as u32,
        }).ok();
    }
    Ok(())
}

/// Emit a simple progress event (not download-related).
fn emit_progress(app: &Arc<AppEventSender>, message: &str, progress: f32) {
    app.emit("install-progress", InstallProgress {
        stage: "install".to_string(),
        label: message.to_string(),
        current: (progress * 100.0) as u32,
        total: 100,
    }).ok();
}

/// Find any system Java binary — used as a fallback when auto-download fails.
async fn check_java() -> Result<String, String> {
    for path in crate::java::java_candidates() {
        if path.exists() {
            if crate::java::detect_java_major(&path).is_some() {
                return Ok(path.to_string_lossy().to_string());
            }
        }
    }
    // Try bare "java" as last resort
    let bin = std::path::PathBuf::from("java");
    if crate::java::detect_java_major(&bin).is_some() {
        return Ok("java".to_string());
    }
    Err("Java not found. Please install Java 17+ (e.g. Eclipse Temurin).".to_string())
}

// ── Main install entry point ─────────────────────────────────────────────────

/// Install a game server based on the provided configuration.
///
/// Validates config, creates the server directory, dispatches to the
/// type-specific installer, writes common files (eula.txt, server.properties),
/// and marks setup as complete.
pub async fn do_install_server(app: Arc<AppEventSender>, mut cfg: ServerConfig) -> Result<ServerConfig, String> {
    let server_dir = PathBuf::from(&cfg.server_path);
    tokio::fs::create_dir_all(&server_dir).await.map_err(|e| e.to_string())?;

    // Terraria servers don't need Java — skip Java detection/download
    if cfg.server_type.needs_java() && cfg.java_path.is_empty() {
        // Try to auto-download the right Java version for this MC version.
        // If download fails, fall back to system Java but verify its version
        // is sufficient — never silently use a too-old JVM (especially for
        // Forge/NeoForge which hang silently on version mismatch).
        let required_major = crate::java::required_java_for_mc(&cfg.minecraft_version);
        cfg.java_path = match crate::java::ensure_java(required_major, &app).await {
            Ok(path) => path.to_string_lossy().to_string(),
            Err(download_err) => {
                let fallback = check_java().await.unwrap_or_else(|_| "java".to_string());
                let bin = std::path::PathBuf::from(&fallback);
                match crate::java::detect_java_major(&bin) {
                    Some(m) if m >= required_major => fallback,
                    _ => return Err(format!(
                        "Java {} is required for Minecraft {} but could not be installed or found. {}",
                        required_major, cfg.minecraft_version, download_err
                    )),
                }
            }
        };
    }

    match cfg.server_type {
        ServerType::Vanilla => install_vanilla(&app, &cfg, &server_dir).await?,
        ServerType::Paper => install_paper(&app, &cfg, &server_dir).await?,
        ServerType::Forge => install_forge(&app, &cfg, &server_dir).await?,
        ServerType::Fabric => install_fabric(&app, &cfg, &server_dir).await?,
        ServerType::NeoForge => install_neoforge(&app, &cfg, &server_dir).await?,
        ServerType::Folia => install_folia(&app, &cfg, &server_dir).await?,
        ServerType::Purpur => install_purpur(&app, &cfg, &server_dir).await?,
        ServerType::Bukkit => install_buildtools(&app, &cfg, &server_dir, "craftbukkit").await?,
        ServerType::Spigot => install_buildtools(&app, &cfg, &server_dir, "spigot").await?,
        ServerType::SpongeVanilla => install_sponge_vanilla(&app, &cfg, &server_dir).await?,
        ServerType::SpongeForge => install_sponge_forge(&app, &cfg, &server_dir).await?,
        ServerType::Terraria => install_terraria(&app, &cfg, &server_dir).await?,
        ServerType::TModLoader => install_tmodloader(&app, &cfg, &server_dir).await?,
    }

    // Clean old mods when switching modloader type so incompatible jars
    // from the previous loader don't linger and crash the new one.
    {
        let old_cfg = crate::config::load_config();
        let old_dir_name = match old_cfg.server_type {
            ServerType::Paper => "plugins",
            _ => "mods",
        };
        let new_dir_name = match cfg.server_type {
            ServerType::Paper => "plugins",
            _ => "mods",
        };
        if old_cfg.setup_complete && old_cfg.server_type != cfg.server_type {
            // Remove .jar files from the old directory
            let old_dir = server_dir.join(old_dir_name);
            if old_dir.exists() {
                for entry in std::fs::read_dir(&old_dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|x| x == "jar") {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            // If the directory name changed (mods <-> plugins), also clean the new one
            if old_dir_name != new_dir_name {
                let new_dir = server_dir.join(new_dir_name);
                if new_dir.exists() {
                    for entry in std::fs::read_dir(&new_dir).into_iter().flatten().flatten() {
                        let path = entry.path();
                        if path.extension().is_some_and(|x| x == "jar") {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
            eprintln!(
                "[lbby] Cleaned old {} mods \u{2014} switched from {:?} to {:?}",
                old_dir_name, old_cfg.server_type, cfg.server_type
            );
        }
    }

    // Common files — game-aware
    if cfg.is_minecraft() {
        // Minecraft: eula.txt + server.properties
        tokio::fs::write(server_dir.join("eula.txt"), "eula=true\n").await
            .map_err(|e| e.to_string())?;
        let (view_distance, simulation_distance) = match cfg.performance_preset.as_str() {
            "low_cpu" => (6, 4),
            "heavy_modpack" => (8, 5),
            "max_performance" => (10, 8),
            _ => (8, 6),
        };
        // Preserve existing server-port if server.properties already exists
        // (user may have changed it via Settings > Server Properties).
        let port = {
            let props_path = server_dir.join("server.properties");
            if props_path.exists() {
                if let Ok(existing) = std::fs::read_to_string(&props_path) {
                    existing.lines()
                        .find(|l| l.starts_with("server-port="))
                        .and_then(|l| l.split('=').nth(1))
                        .and_then(|v| v.trim().parse::<u16>().ok())
                        .unwrap_or(cfg.default_port())
                } else {
                    cfg.default_port()
                }
            } else {
                cfg.default_port()
            }
        };
        // Build server.properties content
        let mut props = format!(
            "online-mode=false\nmax-players={}\nmotd={}\nserver-port={}\nview-distance={}\nsimulation-distance={}\n",
            cfg.max_players, cfg.server_name, port, view_distance, simulation_distance
        );
        // Add seed if configured
        if !cfg.minecraft_seed.trim().is_empty() {
            props.push_str(&format!("level-seed={}\n", cfg.minecraft_seed.trim()));
        }
        tokio::fs::write(server_dir.join("server.properties"), props)
            .await.map_err(|e| e.to_string())?;
    } else if cfg.is_terraria() {
        // Terraria: serverconfig.txt + Worlds directory
        let worlds_dir = server_dir.join("Worlds");
        tokio::fs::create_dir_all(&worlds_dir).await.map_err(|e| e.to_string())?;
        let _world_path = worlds_dir.join(format!("{}.wld", cfg.server_name));
        let config_text = crate::terraria_config::generate_config(
            cfg.max_players,
            &cfg.server_name,
            "",  // no world path yet — use autocreate
            cfg.terraria_difficulty,
            cfg.terraria_world_size,
        );
        tokio::fs::write(
            server_dir.join("serverconfig.txt"),
            config_text,
        ).await.map_err(|e| e.to_string())?;
    }

    // Always create mods/plugins folder (game-aware)
    if cfg.is_minecraft() {
        let extras_dir = match cfg.server_type {
            ServerType::Forge | ServerType::Fabric | ServerType::NeoForge
            | ServerType::SpongeVanilla | ServerType::SpongeForge | ServerType::Vanilla => "mods",
            ServerType::Paper | ServerType::Bukkit | ServerType::Spigot
            | ServerType::Folia | ServerType::Purpur => "plugins",
            _ => "mods",
        };
        tokio::fs::create_dir_all(server_dir.join(extras_dir)).await.ok();
    } else if cfg.is_terraria() {
        // tModLoader uses a Mods directory
        tokio::fs::create_dir_all(server_dir.join("Mods")).await.ok();
    }

    // Validate that the server binary/jar exists before marking setup complete
    if cfg.is_minecraft() {
        let jar_path = server_dir.join("server.jar");
        if !jar_path.exists() {
            return Err("Installation completed but server.jar was not found. The installer may have failed.".to_string());
        }
        let meta = tokio::fs::metadata(&jar_path).await.map_err(|e| e.to_string())?;
        if meta.len() < 1024 {
            return Err("server.jar appears to be corrupted (too small). Try again.".to_string());
        }
    } else if cfg.is_terraria() {
        if cfg.server_type == ServerType::TModLoader {
            // tModLoader uses a shell script, not a standalone binary
            let script = if cfg!(target_os = "windows") {
                server_dir.join("start-tModLoaderServer.bat")
            } else {
                server_dir.join("start-tModLoaderServer.sh")
            };
            if !script.exists() {
                return Err("Installation completed but tModLoader start script was not found. The installer may have failed.".to_string());
            }
        } else {
            // Vanilla Terraria needs the binary — but install_terraria may
            // have used an existing tModLoader installation as a fallback,
            // so also accept tModLoader files.
            let binary = terraria_server_binary(&server_dir);
            let tmod_script = server_dir.join("start-tModLoaderServer.bat");
            let tmod_dll = server_dir.join("tModLoader.dll");
            if !binary.exists() && !tmod_script.exists() && !tmod_dll.exists() {
                return Err("Installation completed but TerrariaServer was not found. The installer may have failed.".to_string());
            }
        }
    }

    cfg.setup_complete = true;
    crate::config::save_config(&cfg)?;
    emit_progress(&app, "Setup complete!", 1.0);
    Ok(cfg)
}

// ── Type-specific installers ─────────────────────────────────────────────────

async fn install_vanilla(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    emit_progress(app, "Fetching version info\u{2026}", 0.05);
    let client = reqwest::Client::new();
    let manifest: VersionManifest = client
        .get("https://launchermeta.mojang.com/mc/game/version_manifest_v2.json")
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let url = manifest.versions.iter()
        .find(|v| v.id == cfg.minecraft_version)
        .map(|v| v.url.clone())
        .ok_or_else(|| format!("Version {} not found", cfg.minecraft_version))?;
    let data: VersionData = client.get(&url).send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let server_url = data.downloads.server.ok_or("No server download")?.url;
    emit_progress(app, "Downloading Minecraft server\u{2026}", 0.15);
    download_file(app, &server_url, &server_dir.join("server.jar"), "server.jar").await
}

async fn install_paper(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let build = cfg.loader_version.as_deref().ok_or("No build selected")?;
    let mc = &cfg.minecraft_version;
    emit_progress(app, "Fetching Paper build info\u{2026}", 0.1);
    let client = reqwest::Client::new();
    let info: PaperBuild = client
        .get(format!("https://api.papermc.io/v2/projects/paper/versions/{}/builds/{}", mc, build))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let url = format!(
        "https://api.papermc.io/v2/projects/paper/versions/{}/builds/{}/downloads/{}",
        mc, build, info.downloads.application.name
    );
    emit_progress(app, "Downloading Paper\u{2026}", 0.2);
    download_file(app, &url, &server_dir.join("server.jar"), "Paper").await
}

async fn install_forge(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let lv = cfg.loader_version.as_deref().ok_or("No Forge version")?;
    let url = format!(
        "https://maven.minecraftforge.net/net/minecraftforge/forge/{mc}-{fv}/forge-{mc}-{fv}-installer.jar",
        mc = cfg.minecraft_version, fv = lv
    );
    let installer = server_dir.join("forge-installer.jar");
    emit_progress(app, "Downloading Forge installer\u{2026}", 0.2);
    download_file(app, &url, &installer, "Forge installer").await?;
    emit_progress(app, "Running Forge installer (may take a minute)\u{2026}", 0.6);
    let mut cmd = tokio::process::Command::new(&cfg.java_path);
    cmd.args(["-jar", "forge-installer.jar", "--installServer"])
        .current_dir(server_dir);
    hide_child_window(&mut cmd);
    let out = cmd.output().await
        .map_err(|e| format!("Failed to run Forge installer: {}", e))?;
    if !out.status.success() {
        return Err(format!("Forge installer failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    tokio::fs::remove_file(&installer).await.ok();
    Ok(())
}

async fn install_fabric(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let loader = cfg.loader_version.as_deref().ok_or("No Fabric loader version")?;
    // Direct server-launcher download endpoint
    let url = format!(
        "https://meta.fabricmc.net/v2/versions/loader/{mc}/{loader}/1.0.1/server/jar",
        mc = cfg.minecraft_version, loader = loader
    );
    emit_progress(app, "Downloading Fabric server\u{2026}", 0.2);
    download_file(app, &url, &server_dir.join("server.jar"), "Fabric server").await
}

async fn install_neoforge(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let lv = cfg.loader_version.as_deref().ok_or("No NeoForge version")?;
    let url = format!(
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/{v}/neoforge-{v}-installer.jar",
        v = lv
    );
    let installer = server_dir.join("neoforge-installer.jar");
    emit_progress(app, "Downloading NeoForge installer\u{2026}", 0.2);
    download_file(app, &url, &installer, "NeoForge installer").await?;
    emit_progress(app, "Running NeoForge installer\u{2026}", 0.6);
    let mut cmd = tokio::process::Command::new(&cfg.java_path);
    cmd.args(["-jar", "neoforge-installer.jar", "--installServer"])
        .current_dir(server_dir);
    hide_child_window(&mut cmd);
    let out = cmd.output().await
        .map_err(|e| format!("Failed to run NeoForge installer: {}", e))?;
    if !out.status.success() {
        return Err(format!("NeoForge installer failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    tokio::fs::remove_file(&installer).await.ok();
    Ok(())
}

async fn install_folia(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let build = cfg.loader_version.as_deref().ok_or("No Folia build selected")?;
    let mc = &cfg.minecraft_version;
    emit_progress(app, "Fetching Folia build info\u{2026}", 0.1);
    let client = reqwest::Client::new();
    let info: PaperBuild = client
        .get(format!("https://api.papermc.io/v2/projects/folia/versions/{}/builds/{}", mc, build))
        .send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())?;
    let url = format!(
        "https://api.papermc.io/v2/projects/folia/versions/{}/builds/{}/downloads/{}",
        mc, build, info.downloads.application.name
    );
    emit_progress(app, "Downloading Folia\u{2026}", 0.2);
    download_file(app, &url, &server_dir.join("server.jar"), "Folia").await
}

async fn install_purpur(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let build = cfg.loader_version.as_deref().ok_or("No Purpur build selected")?;
    let mc = &cfg.minecraft_version;
    let url = format!(
        "https://api.purpurmc.org/v2/purpur/{}/{}/download",
        mc, build
    );
    emit_progress(app, "Downloading Purpur\u{2026}", 0.2);
    download_file(app, &url, &server_dir.join("server.jar"), "Purpur").await
}

async fn install_buildtools(
    app: &Arc<AppEventSender>,
    cfg: &ServerConfig,
    server_dir: &Path,
    product: &str, // "craftbukkit" or "spigot"
) -> Result<(), String> {
    let mc = &cfg.minecraft_version;

    // Verify Git is available before attempting BuildTools
    let git_check = tokio::process::Command::new(if cfg!(target_os = "windows") { "where" } else { "which" })
        .arg("git")
        .output().await;
    if !matches!(git_check.as_ref().map(|o| o.status.success()), Ok(true)) {
        return Err("BuildTools requires Git. Please install Git from https://git-scm.com/downloads and restart the app.".to_string());
    }

    // Run BuildTools in a temporary directory to keep the server dir clean
    let build_dir = std::env::temp_dir().join(format!("lbby-buildtools-{}", mc));
    tokio::fs::create_dir_all(&build_dir).await.map_err(|e| e.to_string())?;
    let buildtools_jar = build_dir.join("BuildTools.jar");

    // Download BuildTools.jar
    emit_progress(app, "Downloading BuildTools\u{2026}", 0.1);
    let bt_url = "https://hub.spigotmc.org/jenkins/job/BuildTools/lastSuccessfulBuild/artifact/target/BuildTools.jar";
    download_file(app, bt_url, &buildtools_jar, "BuildTools.jar").await?;

    // Run BuildTools
    let label = if product == "spigot" { "Spigot" } else { "CraftBukkit" };
    emit_progress(app, &format!("Compiling {} (this may take a few minutes)\u{2026}", label), 0.3);

    let mut cmd = tokio::process::Command::new(&cfg.java_path);
    cmd.arg("-jar").arg("BuildTools.jar")
        .arg("--rev").arg(mc)
        .current_dir(&build_dir);
    if product == "craftbukkit" {
        cmd.arg("--compile").arg("craftbukkit");
    }
    hide_child_window(&mut cmd);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(600), // 10 minutes
        cmd.output()
    ).await
    .map_err(|_| "BuildTools timed out after 10 minutes. Check your internet connection and try again.".to_string())?
    .map_err(|e| format!("Failed to run BuildTools: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Clean up temp dir before returning error
        tokio::fs::remove_dir_all(&build_dir).await.ok();
        return Err(format!("BuildTools failed:\n{}\n{}", stderr, stdout));
    }

    // Find the resulting server JAR in the temp dir and copy it to server_dir
    let prefix = if product == "spigot" { "spigot-" } else { "craftbukkit-" };
    let mut found = false;
    for entry in std::fs::read_dir(&build_dir).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) && name.ends_with(".jar") && !name.contains("original") {
            tokio::fs::copy(entry.path(), server_dir.join("server.jar")).await
                .map_err(|e| e.to_string())?;
            found = true;
            break;
        }
    }
    if !found {
        tokio::fs::remove_dir_all(&build_dir).await.ok();
        return Err(format!("BuildTools completed but no {}server.jar was found.", prefix));
    }

    // Clean up the temporary build directory
    tokio::fs::remove_dir_all(&build_dir).await.ok();
    Ok(())
}

async fn install_sponge_vanilla(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let version_tag = cfg.loader_version.as_deref()
        .ok_or("No SpongeVanilla version selected")?;
    let url = format!(
        "https://dl-api.spongepowered.org/v2/groups/org.spongepowered/artifacts/spongevanilla/versions/{}/assets/installer/download",
        version_tag
    );
    emit_progress(app, "Downloading SpongeVanilla server\u{2026}", 0.2);
    download_file(app, &url, &server_dir.join("server.jar"), "SpongeVanilla server").await
}

async fn install_sponge_forge(app: &Arc<AppEventSender>, cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    let mc = &cfg.minecraft_version;
    let sponge_ver = cfg.loader_version.as_deref().ok_or("No SpongeForge version selected")?;

    // Fetch the Sponge API to find the specific Forge version for this Sponge version
    let client = reqwest::Client::new();
    let api_url = format!(
        "https://dl-api.spongepowered.org/v2/groups/org.spongepowered/artifacts/spongeforge/versions?tags=minecraft:{}&limit=50",
        mc
    );
    let resp = client.get(&api_url).send().await.map_err(|e| e.to_string())?;
    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    // Find the matching Sponge version and extract its Forge version
    let artifacts = data.get("artifacts")
        .and_then(|v| v.as_object())
        .ok_or("Failed to parse Sponge versions")?;

    let mut forge_version = None;
    for (ver, info) in artifacts {
        if ver == sponge_ver {
            forge_version = info.get("tagValues")
                .and_then(|tv| tv.get("forge"))
                .and_then(|f| f.as_str())
                .map(|s| s.to_string());
            break;
        }
    }

    let forge_ver = forge_version
        .ok_or(format!("Could not find Forge version for SpongeForge {}", sponge_ver))?;

    // Install the specific Forge version required by this SpongeForge build
    let mut forge_cfg = cfg.clone();
    forge_cfg.loader_version = Some(forge_ver);
    install_forge(app, &forge_cfg, server_dir).await?;

    // Then download SpongeForge JAR into mods/
    let version_tag = cfg.loader_version.as_deref()
        .ok_or("No SpongeForge version selected")?;
    let url = format!(
        "https://dl-api.spongepowered.org/v2/groups/org.spongepowered/artifacts/spongeforge/versions/{}/assets/installer/download",
        version_tag
    );
    let mods_dir = server_dir.join("mods");
    tokio::fs::create_dir_all(&mods_dir).await.map_err(|e| e.to_string())?;
    emit_progress(app, "Downloading SpongeForge\u{2026}", 0.8);
    download_file(app, &url, &mods_dir.join("spongeforge.jar"), "SpongeForge").await?;
    Ok(())
}

// ── Terraria / tModLoader install ─────────────────────────────────────────────

async fn install_terraria(app: &Arc<AppEventSender>, _cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {

    emit_progress(app, "Setting up Terraria server\u{2026}", 0.1);

    // 0. Check if tModLoader is already installed (it includes Terraria server functionality)
    let mut tmodloader_paths = vec![
        dirs::home_dir().unwrap_or_default().join("terraria-server"),
    ];
    if cfg!(target_os = "windows") {
        if let Ok(pf) = std::env::var("ProgramFiles(x86)") {
            tmodloader_paths.push(PathBuf::from(pf).join("Steam/steamapps/common/tModLoader"));
        }
    } else {
        tmodloader_paths.push(dirs::home_dir().unwrap_or_default().join(".steam/steam/steamapps/common/tModLoader"));
    }
    for tmod_path in &tmodloader_paths {
        if tmod_path.join("start-tModLoaderServer.sh").exists() || tmod_path.join("start-tModLoaderServer.bat").exists() {
            // Skip if source and destination are the same directory
            if tmod_path.canonicalize().ok() == server_dir.canonicalize().ok() {
                emit_progress(app, "Server directory is already a tModLoader installation", 1.0);
                return Ok(());
            }
            emit_progress(app, "Found tModLoader installation, linking\u{2026}", 0.3);
            tokio::fs::create_dir_all(server_dir).await.map_err(|e| e.to_string())?;
            let src = tmod_path.to_string_lossy().to_string();
            let dst = server_dir.to_string_lossy().to_string();
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("cp")
                    .args(["-R", &format!("{}/.", src), &dst])
                    .output();
            }
            #[cfg(windows)]
            {
                let xcopy_ok = std::process::Command::new("xcopy")
                    .args([&format!("{}\\*", src), &dst, "/E", "/I", "/Y", "/Q"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !xcopy_ok {
                    let bat_src = tmod_path.join("start-tModLoaderServer.bat");
                    let bat_dst = server_dir.join("start-tModLoaderServer.bat");
                    if bat_src.exists() {
                        let _ = std::fs::copy(&bat_src, &bat_dst);
                    }
                    for entry in std::fs::read_dir(tmod_path).into_iter().flatten().flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".dll") || name.ends_with(".config") || name.ends_with(".xml") || name.ends_with(".bat") {
                            let dest_file = server_dir.join(&name);
                            if !dest_file.exists() {
                                let _ = std::fs::copy(entry.path(), dest_file);
                            }
                        }
                    }
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let sh = server_dir.join("start-tModLoaderServer.sh");
                if sh.exists() {
                    let _ = std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755));
                }
            }
            emit_progress(app, "Server installed (via tModLoader)", 1.0);
            return Ok(());
        }
    }

    // 1. Check if the server is bundled with an existing Terraria installation
    let terraria_dir = if cfg!(target_os = "macos") {
        dirs::home_dir().unwrap_or_default().join("Library/Application Support/Steam/steamapps/common/Terraria")
    } else if cfg!(target_os = "windows") {
        let prog_files = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".to_string());
        PathBuf::from(prog_files).join("Steam/steamapps/common/Terraria")
    } else {
        dirs::home_dir().unwrap_or_default().join(".steam/steam/steamapps/common/Terraria")
    };

    if terraria_dir.exists() {
        if let Some(found) = find_terraria_binary(&terraria_dir) {
            emit_progress(app, "Found bundled Terraria server, copying\u{2026}", 0.3);
            tokio::fs::create_dir_all(server_dir).await.map_err(|e| e.to_string())?;

            #[cfg(target_os = "windows")]
            {
                if let Some(parent) = found.parent() {
                    let src = format!("{}\\*", parent.to_string_lossy());
                    let dst = server_dir.to_string_lossy().to_string();
                    let status = std::process::Command::new("xcopy")
                        .args([&src, &dst, "/E", "/I", "/Y", "/Q"])
                        .status();
                    if !status.map(|s| s.success()).unwrap_or(false) {
                        let dest = terraria_server_binary(server_dir);
                        let _ = tokio::fs::copy(&found, &dest).await;
                        for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.ends_with(".dll") || name.ends_with(".config") || name.ends_with(".xml") {
                                let dest_file = server_dir.join(&name);
                                if !dest_file.exists() {
                                    let _ = std::fs::copy(entry.path(), dest_file);
                                }
                            }
                        }
                    }
                }
            }

            #[cfg(unix)]
            {
                let dest = terraria_server_binary(server_dir);
                tokio::fs::copy(&found, &dest).await.map_err(|e| format!("Failed to copy server: {}", e))?;
                if let Some(parent) = found.parent() {
                    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.ends_with(".dll") || name.ends_with(".config") || name.ends_with(".xml") {
                            let dest_file = server_dir.join(&name);
                            if !dest_file.exists() {
                                let _ = std::fs::copy(entry.path(), dest_file);
                            }
                        }
                    }
                }
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
            }

            emit_progress(app, "Terraria server installed", 1.0);
            return Ok(());
        }
    }

    // 2. Try SteamCMD
    if let Some(_steamcmd_path) = crate::steamcmd::find_steamcmd() {
        emit_progress(app, "Downloading via SteamCMD\u{2026}", 0.2);
        match crate::steamcmd::download_server(
            app,
            crate::steamcmd::TERRARIA_APP_ID,
            server_dir,
            "anonymous",
        ).await {
            Ok(()) => {
                let binary = terraria_server_binary(server_dir);
                if binary.exists() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755));
                    }
                    emit_progress(app, "Terraria server installed", 1.0);
                    return Ok(());
                }
                // Check if binary is in a subdirectory (SteamCMD sometimes nests)
                for entry in walkdir::WalkDir::new(server_dir).max_depth(3).into_iter().flatten() {
                    let name = entry.file_name().to_string_lossy();
                    if name == "TerrariaServer" || name == "TerrariaServer.exe" {
                        let dest = terraria_server_binary(server_dir);
                        let _ = std::fs::rename(entry.path(), &dest);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
                        }
                        emit_progress(app, "Terraria server installed", 1.0);
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                eprintln!("[lbby] SteamCMD failed: {}", e);
            }
        }
    }

    // 3. No method worked — provide helpful error
    let steamcmd_hint = if cfg!(target_os = "windows") {
        "Download SteamCMD from https://developer.valvesoftware.com/wiki/SteamCMD and extract it to a folder"
    } else if cfg!(target_os = "macos") {
        "Install SteamCMD: brew install steamcmd"
    } else {
        "Install SteamCMD: sudo apt install steamcmd (or your distro's equivalent)"
    };
    Err(
        format!("Could not install Terraria server. Please do one of the following:\n\n\
         \u{2022} {}\n\
         \u{2022} Install Terraria via Steam (the server is bundled with the game)\n\
         \u{2022} Download the server manually from terraria.org and place TerrariaServer in the server folder",
         steamcmd_hint)
    )
}

async fn install_tmodloader(app: &Arc<AppEventSender>, _cfg: &ServerConfig, server_dir: &Path) -> Result<(), String> {
    emit_progress(app, "Fetching tModLoader release info\u{2026}", 0.05);

    // Get the latest release from GitHub
    let client = reqwest::Client::new();
    let releases_url = "https://api.github.com/repos/tModLoader/tModLoader/releases/latest";
    let release: serde_json::Value = client
        .get(releases_url)
        .header("User-Agent", "Lbby-App")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch tModLoader releases: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse tModLoader release: {}", e))?;

    let tag = release["tag_name"]
        .as_str()
        .ok_or("Could not find tModLoader version tag")?;

    emit_progress(app, &format!("Downloading tModLoader {}\u{2026}", tag), 0.1);

    // Find the correct asset for this platform
    let assets = release["assets"]
        .as_array()
        .ok_or("No assets found in release")?;

    let asset_name = if cfg!(target_os = "windows") {
        "tModLoader.zip"
    } else if cfg!(target_os = "macos") {
        "tModLoader-mac.zip"
    } else {
        "tModLoader-Linux.zip"
    };

    // Try to find the platform-specific asset, fall back to generic
    let download_url = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(asset_name))
        .or_else(|| assets.iter().find(|a| a["name"].as_str() == Some("tModLoader.zip")))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or(format!("Could not find download URL for {}", asset_name))?;

    // Download the zip
    let resp = client
        .get(download_url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Download failed with HTTP {}", resp.status()));
    }

    let total = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut bytes = Vec::new();
    let mut downloaded = 0u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Download error: {}", e))?;
        bytes.extend_from_slice(&chunk);
        downloaded += chunk.len() as u64;
        if total > 0 {
            let progress = 0.1 + (downloaded as f64 / total as f64) * 0.7;
            emit_progress(app, &format!("Downloading tModLoader\u{2026} {:.0}%", downloaded as f64 / total as f64 * 100.0), progress as f32);
        }
    }

    emit_progress(app, "Extracting tModLoader\u{2026}", 0.8);

    // Save to temp file and extract
    let zip_path = server_dir.join("tmodloader-download.zip");
    tokio::fs::write(&zip_path, &bytes)
        .await
        .map_err(|e| format!("Failed to write zip: {}", e))?;

    // Extract
    let file = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("Invalid zip: {}", e))?;
    zip.extract(server_dir)
        .map_err(|e| format!("Failed to extract: {}", e))?;

    // Clean up zip
    let _ = tokio::fs::remove_file(&zip_path).await;

    // Verify the start script exists
    let start_script = if cfg!(target_os = "windows") {
        server_dir.join("start-tModLoaderServer.bat")
    } else {
        server_dir.join("start-tModLoaderServer.sh")
    };
    if !start_script.exists() {
        // Try looking in a subdirectory
        let alt = server_dir.join("tModLoader").join(if cfg!(target_os = "windows") {
            "start-tModLoaderServer.bat"
        } else {
            "start-tModLoaderServer.sh"
        });
        if alt.exists() {
            // Move contents up
            let src = server_dir.join("tModLoader");
            for entry in std::fs::read_dir(&src).map_err(|e| e.to_string())?.flatten() {
                let dest = server_dir.join(entry.file_name());
                let _ = std::fs::rename(entry.path(), dest);
            }
            let _ = std::fs::remove_dir(src);
        } else {
            return Err("tModLoader start script not found after extraction. The download may have failed.".to_string());
        }
    }

    // Make script executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = server_dir.join("start-tModLoaderServer.sh");
        if script.exists() {
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
    }

    // Create Mods directory and empty enabled.json
    let mods = crate::tmod_services::mods_dir(server_dir);
    tokio::fs::create_dir_all(&mods).await.map_err(|e| e.to_string())?;
    if !crate::tmod_services::enabled_json_path(server_dir).exists() {
        crate::tmod_services::write_enabled_json(server_dir, &[])?;
    }

    emit_progress(app, "tModLoader server installed", 1.0);
    Ok(())
}

// ── Pregenerate chunks ───────────────────────────────────────────────────────

pub async fn pregenerate_chunks(app: Arc<crate::app_state::AppEventSender>, total_chunks: u32) -> Result<(), String> {
    do_pregenerate_chunks(app, total_chunks).await
}

pub async fn do_pregenerate_chunks(app: Arc<crate::app_state::AppEventSender>, total_chunks: u32) -> Result<(), String> {
    let lic = crate::license::current();
    let cap = crate::license::max_pregen_chunks(lic.tier);
    if total_chunks > cap {
        return Err(format!(
            "Requested {} chunks exceeds your {:?} tier limit of {}. Upgrade for larger pre-generations.",
            total_chunks, lic.tier, cap
        ));
    }
    if total_chunks == 0 {
        return Err("total_chunks must be > 0".to_string());
    }
    {
        let s = app.state();
        let srv = s.server.lock().await;
        if srv.status != ServerStatus::Running {
            return Err("Server must be running to pre-generate chunks".to_string());
        }
    }
    {
        let s = app.state();
        let mut pg = s.pregen.lock().await;
        if pg.running {
            return Err("A pre-generation task is already running".to_string());
        }
        let mut side = (total_chunks as f64).sqrt().ceil() as u32;
        if side.is_multiple_of(2) { side += 1; }
        let total = side * side;
        *pg = crate::app_state::PregenState { running: true, total, completed: 0, cancel_requested: false };
        app.emit("pregen-update", pg.clone()).ok();
    }

    let app_task = app.clone();
    tokio::spawn(async move {
        let result = run_pregen(&app_task, total_chunks).await;
        let s = app_task.state();

        {
            let mut srv = s.server.lock().await;
            if srv.status == ServerStatus::Running {
                if let Some(stdin) = &mut srv.stdin {
                    let _ = tokio::io::AsyncWriteExt::write_all(stdin, b"forceload remove all\n").await;
                }
            }
        }

        let final_msg = match result {
            Ok(completed) => format!("[lbby] Pre-generation complete: {} chunks", completed),
            Err(e) if e == "cancelled" => "[lbby] Pre-generation cancelled".to_string(),
            Err(e) => format!("[lbby] Pre-generation failed: {}", e),
        };
        s.push_console_line(final_msg.clone());
        app_task.emit("mc-line", &final_msg).ok();

        let mut pg = s.pregen.lock().await;
        pg.running = false;
        pg.cancel_requested = false;
        app_task.emit("pregen-update", pg.clone()).ok();
    });

    Ok(())
}

async fn run_pregen(app: &Arc<crate::app_state::AppEventSender>, _requested_total: u32) -> Result<u32, String> {
    let s = app.state();
    let total = { s.pregen.lock().await.total };
    let side_len = (total as f64).sqrt() as u32;
    let half = (side_len / 2) as i32;

    const BATCH: i32 = 16;
    let mut completed: u32 = 0;

    let banner = format!(
        "[lbby] Pre-generating {} chunks ({}x{} grid centered on spawn)…",
        total, side_len, side_len
    );
    s.push_console_line(banner.clone());
    app.emit("mc-line", &banner).ok();

    let mut z = -half;
    while z <= half {
        let z_end = (z + BATCH - 1).min(half);
        let mut x = -half;
        while x <= half {
            {
                let pg = s.pregen.lock().await;
                if pg.cancel_requested { return Err("cancelled".to_string()); }
            }
            {
                let srv = s.server.lock().await;
                if srv.status != ServerStatus::Running {
                    return Err("Server stopped during pre-generation".to_string());
                }
            }

            let x_end = (x + BATCH - 1).min(half);
            let cmd = format!("forceload add {} {} {} {}\n", x, z, x_end, z_end);
            {
                let mut srv = s.server.lock().await;
                if let Some(stdin) = &mut srv.stdin {
                    tokio::io::AsyncWriteExt::write_all(stdin, cmd.as_bytes()).await.map_err(|e| e.to_string())?;
                }
            }

            let batch_chunks = ((x_end - x + 1) as u32) * ((z_end - z + 1) as u32);
            tokio::time::sleep(std::time::Duration::from_millis(
                (batch_chunks as u64 * 30).max(700),
            )).await;

            completed += batch_chunks;
            {
                let mut pg = s.pregen.lock().await;
                pg.completed = completed.min(total);
                app.emit("pregen-update", pg.clone()).ok();
            }

            if completed.is_multiple_of(1024) {
                let mut srv = s.server.lock().await;
                if let Some(stdin) = &mut srv.stdin {
                    let _ = tokio::io::AsyncWriteExt::write_all(stdin, b"forceload remove all\n").await;
                }
            }

            x += BATCH;
        }
        z += BATCH;
    }

    Ok(completed.min(total))
}

pub async fn cancel_pregenerate(state: &crate::app_state::AppState) -> Result<(), String> {
    let mut pg = state.pregen.lock().await;
    if pg.running {
        pg.cancel_requested = true;
    }
    Ok(())
}

pub async fn get_pregen_state(state: &crate::app_state::AppState) -> Result<crate::app_state::PregenState, String> {
    Ok(state.pregen.lock().await.clone())
}
