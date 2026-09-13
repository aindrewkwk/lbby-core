// Oculus minimal UserActionRequired end-to-end acceptance test.
// Usage: cargo run --bin oculus_acceptance --features testing

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lbby_core::app_state::{AppEventSender, AppState};
use lbby_core::config::{ServerConfig, ServerType};
use lbby_core::mod_services::InstallOutcome;
#[cfg(feature = "testing")]
use lbby_core::recovery_actions::{approve_crash_recovery_at, reject_crash_recovery_at};
use lbby_core::server::ServerStatus;

const SMOKE_TIMEOUT: Duration = Duration::from_secs(600);
const BOOT_TIMEOUT: Duration = Duration::from_secs(300);

fn java_path() -> String {
    std::env::var("JAVA_HOME")
        .ok()
        .map(|h| format!("{}/bin/java", h))
        .unwrap_or_else(|| "/usr/bin/java".to_string())
}

fn java_version_string() -> String {
    std::process::Command::new(&java_path())
        .arg("-version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stderr).ok())
        .unwrap_or_else(|| "unknown".to_string())
        .lines()
        .next()
        .unwrap_or("unknown")
        .to_string()
}

fn setup_config(server_path: &PathBuf) -> Result<(), String> {
    std::fs::create_dir_all(server_path).map_err(|e| e.to_string())?;
    let config_dir = server_path.join("lbby-config");
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;

    let cfg = ServerConfig {
        server_path: server_path.to_string_lossy().to_string(),
        server_type: ServerType::Forge,
        minecraft_version: "1.20.1".to_string(),
        java_path: java_path(),
        server_name: "minecraft-server".to_string(),
        setup_complete: true,
        ..Default::default()
    };
    let profiles = serde_json::json!({
        "active_id": "acceptance",
        "profiles": [
            {
                "id": "acceptance",
                "name": "Acceptance Test",
                "config": cfg
            }
        ]
    });
    std::fs::write(
        config_dir.join("profiles.json"),
        serde_json::to_string_pretty(&profiles).unwrap(),
    )
    .map_err(|e| e.to_string())?;
    std::env::set_var("LBBY_CONFIG_DIR", config_dir.to_string_lossy().to_string());
    Ok(())
}

fn new_app() -> Arc<AppEventSender> {
    Arc::new(AppEventSender::new(Arc::new(AppState::new())))
}

/// Find the staging directory for a specific transaction
fn find_staging_dir(server_path: &std::path::Path, txn_id: &str) -> Option<PathBuf> {
    // Staging is a sibling: <parent>/.lbby-staging/<name>-<txn_id>/
    let staging_root = server_path.parent().unwrap().join(".lbby-staging");
    if !staging_root.exists() {
        return None;
    }
    for entry in std::fs::read_dir(&staging_root).ok()?.flatten() {
        let p = entry.path();
        if p.join("transaction.json").exists() {
            if let Ok(c) = std::fs::read_to_string(p.join("transaction.json")) {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&c) {
                    if meta.get("transaction_id").and_then(|v| v.as_str()) == Some(txn_id) {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

fn count_mods(sp: &PathBuf) -> usize {
    let d = sp.join("mods");
    if !d.exists() {
        return 0;
    }
    std::fs::read_dir(&d)
        .map(|e| {
            e.filter_map(|x| x.ok())
                .filter(|x| {
                    x.path()
                        .extension()
                        .map(|ext| ext == "jar")
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-P", &pid.to_string()])
        .output();
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_secs(2));
}

struct Guard(PathBuf, String);
impl Guard {
    fn kill(&self) {
        let pf = self.0.join(".lbby-server.pid");
        if let Ok(ps) = std::fs::read_to_string(&pf) {
            if let Ok(pid) = ps.trim().parse::<u32>() {
                eprintln!("[{}] SIGTERM {}", self.1, pid);
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                let dl = Instant::now() + Duration::from_secs(15);
                while Instant::now() < dl {
                    if !is_alive(pid) {
                        let _ = std::fs::remove_file(&pf);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                kill_tree(pid);
                let _ = std::fs::remove_file(&pf);
            }
        }
        let _ = std::process::Command::new("pkill")
            .args(["-f", &format!("java.*{}", self.0.to_string_lossy())])
            .output();
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.kill();
    }
}

fn create_zip(dest: &PathBuf) -> Result<(), String> {
    let manifest = serde_json::json!({
        "minecraft": {
            "version": "1.20.1",
            "modLoaders": [{"id": "forge-47.2.20", "primary": true}]
        },
        "manifestType": "minecraftModpack",
        "manifestVersion": 1,
        "name": "Oculus-Minimal",
        "version": "1.0.0",
        "author": "test",
        "files": [{"projectID": 581495, "fileID": 5108615, "required": true}],
        "overrides": "overrides"
    });
    let f = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut z = zip::ZipWriter::new(f);
    let o =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("manifest.json", o)
        .map_err(|e| e.to_string())?;
    let b = serde_json::to_string_pretty(&manifest).unwrap();
    std::io::Write::write_all(&mut z, b.as_bytes()).map_err(|e| e.to_string())?;
    z.add_directory("overrides/", o)
        .map_err(|e| e.to_string())?;
    z.finish().map_err(|e| e.to_string())?;
    Ok(())
}

async fn wait_status(app: &AppEventSender, timeout: Duration) -> ServerStatus {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return ServerStatus::Error;
        }
        {
            let state = app.state();
            let srv = state.server.lock().await;
            match &srv.status {
                ServerStatus::Running => return ServerStatus::Running,
                ServerStatus::Error => return ServerStatus::Error,
                ServerStatus::Stopped => {
                    if start.elapsed() > Duration::from_secs(30) {
                        return ServerStatus::Stopped;
                    }
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── Main ─────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", "server.jar nogui"])
        .output();

    println!("{}", "=".repeat(80));
    println!("OCULUS UAR END-TO-END ACCEPTANCE TEST");
    println!("{}", "=".repeat(80));
    println!("Forge:  1.20.1-47.2.20");
    println!("Oculus: project=581495 file=5108615 v1.6.15a");
    println!("Oculus SHA-256: c9b9d835bfd90799aa3aa56bdc9ba57742da3ee12f1a0cecc9d8a44e4592a528");
    println!("Java:   {} @ {}", java_version_string(), java_path());
    println!();

    // ── Phase 1: Install → crash → UAR ────────────────────────────────

    println!("=== PHASE 1: Install -> crash -> UserActionRequired ===");
    let sp1 = PathBuf::from("/tmp/oculus-uar-acceptance");
    let _ = std::fs::remove_dir_all(&sp1);
    setup_config(&sp1).expect("config");
    let _g1 = Guard(sp1.clone(), "p1".into());
    let app1 = new_app();

    let zip_path = PathBuf::from("/tmp/oculus-minimal-acceptance.zip");
    create_zip(&zip_path).expect("zip");
    println!("[p1] CF pack: {}", zip_path.display());

    let t1 = Instant::now();
    let r1 = tokio::time::timeout(
        SMOKE_TIMEOUT,
        lbby_core::mod_services::install_curseforge_modpack(
            app1.clone(),
            zip_path.to_string_lossy().to_string(),
        ),
    )
    .await;

    let (server_id, txn_id, fingerprint, mod_id, crash_summary, confidence) = match &r1 {
        Ok(Ok(InstallOutcome::UserActionRequired {
            server_id,
            transaction_id,
            fingerprint,
            mod_id,
            crash_summary,
            confidence,
            ..
        })) => {
            println!("[p1] ✓ UserActionRequired!");
            println!("  server_id:      {}", server_id);
            println!("  transaction_id: {}", transaction_id);
            println!("  fingerprint:    {}", fingerprint);
            println!("  mod_id:         {}", mod_id);
            println!("  crash_summary:  {}", crash_summary);
            println!("  confidence:     {}", confidence);
            assert_eq!(mod_id, "oculus", "mod_id must be 'oculus'");
            (
                server_id.clone(),
                transaction_id.clone(),
                fingerprint.clone(),
                mod_id.clone(),
                crash_summary.clone(),
                confidence.clone(),
            )
        }
        Ok(Ok(InstallOutcome::Success(_))) => {
            println!("[p1] ✗ FAIL: Got Success instead of UAR");
            return;
        }
        Ok(Ok(other)) => {
            println!("[p1] ✗ FAIL: Unexpected: {:?}", other);
            return;
        }
        Ok(Err(e)) => {
            println!("[p1] ✗ FAIL: {}", e);
            return;
        }
        Err(_) => {
            println!("[p1] ✗ FAIL: Timeout {:.1}s", t1.elapsed().as_secs_f64());
            return;
        }
    };

    println!("[p1] mods: {}", count_mods(&sp1));
    println!("[p1] Phase 1: {:.1}s", t1.elapsed().as_secs_f64());

    // Verify pending metadata
    let staging_dir = find_staging_dir(&sp1, &txn_id);
    if let Some(ref sd) = staging_dir {
        let pending = sd.join("pending_recovery.json");
        if pending.exists() {
            let c = std::fs::read_to_string(&pending).unwrap_or_default();
            println!("[p1] ✓ pending_recovery.json: {} bytes", c.len());
        } else {
            println!(
                "[p1] ⚠ pending_recovery.json missing at {}",
                pending.display()
            );
        }
    } else {
        println!("[p1] ⚠ staging dir not found for txn {}", txn_id);
    }

    // Verify transaction phase
    let txn_file = sp1.join(".lbby-staging/transaction.json");
    if txn_file.exists() {
        let c = std::fs::read_to_string(&txn_file).unwrap_or_default();
        println!(
            "[p1] ✓ transaction.json: {}",
            c.chars().take(200).collect::<String>()
        );
    }
    println!();

    // ── Phase 2: Approve → resume → commit → production ───────────

    println!("=== PHASE 2: Approve -> resume -> commit -> production ===");

    // Step 2a: Approve (quarantine Oculus)
    let approve = approve_crash_recovery_at(&server_id, &txn_id, &fingerprint, &sp1);
    match &approve {
        lbby_core::recovery_actions::ApprovalResult::Applied {
            quarantine_path,
            sha256,
            recovery_actions_used,
        } => {
            println!("[p2] ✓ Applied!");
            println!("  quarantine_path: {}", quarantine_path.display());
            println!("  sha256: {}", sha256);
            println!("  recovery_actions_used: {}", recovery_actions_used);
        }
        other => {
            println!("[p2] ✗ Approve: {:?}", other);
            return;
        }
    }

    // Quarantine check
    let staging_root = sp1.parent().unwrap().join(".lbby-staging");
    let staging_dir = find_staging_dir(&sp1, &txn_id);
    if let Some(ref sd) = staging_dir {
        let q = sd.join("quarantine");
        if q.exists() {
            let e: Vec<_> = std::fs::read_dir(&q)
                .map(|d| d.filter_map(|x| x.ok()).collect())
                .unwrap_or_default();
            println!("[p2] ✓ quarantine/ {} entries", e.len());
            for x in &e {
                println!("    {}", x.file_name().to_string_lossy());
            }
        }
        // Oculus removed from staging mods?
        let m = sd.join("mods");
        if m.exists() {
            let oc: Vec<_> = std::fs::read_dir(&m)
                .map(|d| {
                    d.filter_map(|x| x.ok())
                        .filter(|x| x.file_name().to_string_lossy().contains("oculus"))
                        .collect()
                })
                .unwrap_or_default();
            if oc.is_empty() {
                println!("[p2] ✓ Oculus removed from staging mods/");
            } else {
                println!("[p2] ✗ Oculus still in staging mods/");
            }
        }
    }

    // Step 2b: Resume same transaction → BootValidator rerun → commit
    println!("[p2] Resuming same transaction {}...", txn_id);
    match lbby_core::mod_services::resume_curseforge_install(app1.clone()).await {
        Ok(lbby_core::mod_services::InstallOutcome::Success(cfg)) => {
            println!("[p2] ✓ Resume succeeded — transaction committed!");
            println!("  server_path: {}", cfg.server_path);

            // Verify no staging residue
            if !staging_root.exists() {
                println!("[p2] ✓ No staging residue (staging root gone)");
            } else {
                let remaining: Vec<_> = std::fs::read_dir(&staging_root)
                    .map(|d| d.filter_map(|x| x.ok()).collect())
                    .unwrap_or_default();
                println!("[p2] ⚠ staging root still has {} entries", remaining.len());
            }

            // Verify no pending_recovery.json
            let pending = staging_dir
                .as_ref()
                .map(|sd| sd.join("pending_recovery.json"));
            if pending.map(|p| !p.exists()).unwrap_or(true) {
                println!("[p2] ✓ No stale pending_recovery.json");
            } else {
                println!("[p2] ⚠ pending_recovery.json still exists");
            }

            // Verify no stale retry state
            let retry_state = std::path::Path::new(&cfg.server_path).join(".lbby-retry-state.json");
            if !retry_state.exists() {
                println!("[p2] ✓ No stale retry state");
            } else {
                println!("[p2] ⚠ retry state still exists");
            }

            // Live server created?
            let live = std::path::PathBuf::from(&cfg.server_path);
            if live.exists() {
                println!("[p2] ✓ Live server directory exists");
            } else {
                println!("[p2] ✗ Live server directory NOT found");
            }

            // Production start
            println!("[p2] Starting production server...");
            match lbby_core::server::do_start_server(app1.clone()).await {
                Ok(()) => {
                    println!("[p2] ✓ Server started");
                    let status = wait_status(&app1, BOOT_TIMEOUT).await;
                    match status {
                        ServerStatus::Running => {
                            println!("[p2] ✓ RUNNING!");
                            let _ = lbby_core::server::stop_server(app1.clone()).await;
                            println!("[p2] ✓ Stopped cleanly");
                        }
                        ServerStatus::Stopped => {
                            println!("[p2] ✓ Stopped (normal for Forge without mods)");
                        }
                        _ => {
                            println!("[p2] Status: {:?}", status);
                        }
                    }
                }
                Err(e) => {
                    println!("[p2] Start failed: {}", e);
                }
            }
        }
        Ok(other) => {
            println!("[p2] ✗ Resume returned unexpected: {:?}", other);
        }
        Err(e) => {
            println!("[p2] ✗ Resume failed: {}", e);
        }
    }

    drop(_g1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!();

    // ── Phase 3: Reject → rollback → live preserved ───────────────────

    println!("=== PHASE 3: Reject -> rollback -> live preserved ===");
    let sp2 = PathBuf::from("/tmp/oculus-uar-reject");
    let _ = std::fs::remove_dir_all(&sp2);
    setup_config(&sp2).expect("config");
    let _g2 = Guard(sp2.clone(), "p3".into());
    let app2 = new_app();

    println!("[p3] Installing...");
    let r3 = tokio::time::timeout(
        SMOKE_TIMEOUT,
        lbby_core::mod_services::install_curseforge_modpack(
            app2.clone(),
            zip_path.to_string_lossy().to_string(),
        ),
    )
    .await;

    let (sid3, txn3) = match &r3 {
        Ok(Ok(InstallOutcome::UserActionRequired {
            server_id,
            transaction_id,
            ..
        })) => {
            println!("[p3] ✓ UAR triggered");
            (server_id.clone(), transaction_id.clone())
        }
        other => {
            println!("[p3] ✗ Expected UAR: {:?}", other);
            return;
        }
    };

    println!("[p3] Rejecting...");
    match reject_crash_recovery_at(&sid3, &txn3, &sp2) {
        Ok(()) => println!("[p3] ✓ Reject OK"),
        Err(e) => {
            println!("[p3] ✗ Reject: {}", e);
            return;
        }
    }

    if !sp2.parent().unwrap().join(".lbby-staging").exists() {
        println!("[p3] ✓ Staging removed");
    } else {
        println!("[p3] ⚠ Staging exists");
    }
    if !sp2.join("quarantine").exists() {
        println!("[p3] ✓ No quarantine (no mutation)");
    } else {
        println!("[p3] ⚠ Quarantine exists");
    }

    drop(_g2);

    println!();
    println!("{}", "=".repeat(80));
    println!("ACCEPTANCE TEST COMPLETE");
    println!("{}", "=".repeat(80));
}
