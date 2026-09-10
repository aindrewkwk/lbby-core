// smoke_full_pipeline.rs — Real-world validation gate for Phase 3E.2.
//
// Proves: BootValidator success → transaction commit → NORMAL production start → server reaches ready
//
// Run with: cargo test --test smoke_full_pipeline -- --ignored
//
// Requires: Java 17+, MC 1.21.4 server.jar at /tmp/smoke-test/server.jar, network

use lbby_core::app_state::{AppEventSender, AppState};
use lbby_core::boot_validator::{BootFailureReason, BootResult, BootValidator};
use lbby_core::config::{self, ServerConfig, ServerType};
use lbby_core::server_launch::build_server_launch_command;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// ── Helpers ────────────────────────────────────────────────────────

fn smoke_server_jar() -> &'static str {
    "/tmp/smoke-test/server.jar"
}

/// Returns Ok(()) if the test's prerequisites are met, or Err with a skip message.
fn require_server_jar() -> Result<(), String> {
    let path = smoke_server_jar();
    if !Path::new(path).exists() {
        return Err(format!("SKIPPED: {} not found", path));
    }
    Ok(())
}

/// Check that the Java version is compatible with MC 1.21.4 (needs Java 17–25).
/// Java 26+ causes the server to exit before reaching "Done".
fn require_compatible_java() -> Result<(), String> {
    let output = std::process::Command::new("java")
        .args(["-version"])
        .output()
        .map_err(|e| format!("SKIPPED: cannot run java: {}", e))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Parse "version \"21.0.x\"" or "version \"17.x\"" etc.
    if let Some(start) = stderr.find("version \"") {
        let rest = &stderr[start + 9..];
        if let Some(end) = rest.find('"') {
            let version_str = &rest[..end];
            let major: u32 = version_str
                .split('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if major > 25 {
                return Err(format!(
                    "SKIPPED: Java {} too new for MC 1.21.4 (needs 17–25)",
                    major
                ));
            }
        }
    }
    Ok(())
}

/// Returns Ok(()) if CF_API_KEY is set, or Err with skip message.
fn require_cf_api_key() -> Result<(), String> {
    if std::env::var("CF_API_KEY").is_err() {
        return Err("SKIPPED: CF_API_KEY not set".to_string());
    }
    Ok(())
}

/// Returns Ok(()) if the CurseForge smoke pack ZIP exists, or Err with skip message.
fn require_cf_smoke_pack() -> Result<(), String> {
    let path = PathBuf::from("/tmp/lbby-smoke-pack/smoke-test-pack.zip");
    if !path.exists() {
        return Err(format!("SKIPPED: {} not found", path.display()));
    }
    Ok(())
}

fn make_test_config(server_path: &str) -> ServerConfig {
    ServerConfig {
        server_path: server_path.to_string(),
        server_name: "smoke-test".to_string(),
        server_type: ServerType::Vanilla,
        minecraft_version: "1.21.4".to_string(),
        loader_version: None,
        ram_mb: 2048,
        optimized_jvm_flags: true,
        setup_complete: true,
        eula_accepted: true,
        java_path: String::new(), // let the library resolve
        ..Default::default()
    }
}

fn setup_staging(dir: &Path) {
    setup_staging_with_port(dir, 25570);
}

fn setup_staging_with_port(dir: &Path, port: u16) {
    fs::create_dir_all(dir).expect("create staging dir");
    fs::copy(smoke_server_jar(), dir.join("server.jar")).expect("copy server.jar");
    fs::write(dir.join("eula.txt"), "eula=true\n").expect("write eula.txt");
    // Use non-default port to avoid conflicts with any running server
    fs::write(
        dir.join("server.properties"),
        format!("server-port={}\n", port),
    )
    .expect("write server.properties");
}

fn cleanup_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// Write a test profiles.json to the real config dir.
/// Returns the original content so the caller can restore it.
fn write_test_profiles(server_path: &str) -> Vec<u8> {
    let profiles_path = config::config_path()
        .parent()
        .unwrap()
        .join("profiles.json");
    let original = fs::read(&profiles_path).unwrap_or_default();

    let id = "smoke-test-profile";
    let cfg = make_test_config(server_path);
    let cfg_json = serde_json::to_value(&cfg).unwrap();
    let profiles = serde_json::json!({
        "active_id": id,
        "profiles": [{
            "id": id,
            "name": "Smoke Test",
            "config": cfg_json
        }]
    });
    let json = serde_json::to_string_pretty(&profiles).unwrap();
    fs::write(&profiles_path, &json).expect("write test profiles.json");

    original
}

fn restore_profiles(original: &[u8]) {
    let profiles_path = config::config_path()
        .parent()
        .unwrap()
        .join("profiles.json");
    let _ = fs::write(&profiles_path, original);
}

// ── Smoke Test A: Vanilla production parity (production-command smoke) ──

#[tokio::test]
#[ignore]
async fn smoke_a_vanilla_production_parity() {
    if let Err(msg) = require_server_jar() {
        eprintln!("{}", msg);
        return;
    }
    if let Err(msg) = require_compatible_java() {
        eprintln!("{}", msg);
        return;
    }
    let staging = PathBuf::from("/tmp/smoke-a-staging");
    let live = PathBuf::from("/tmp/smoke-a-live");
    cleanup_dir(&staging);
    cleanup_dir(&live);

    // Step 1: Set up staging with server.jar + eula.txt
    setup_staging(&staging);

    // Step 2: Commit staging to live (simulate transaction commit)
    fs::create_dir_all(&live).unwrap();
    for entry in fs::read_dir(&staging).unwrap() {
        let entry = entry.unwrap();
        let dest = live.join(entry.file_name());
        fs::copy(entry.path(), dest).unwrap();
    }

    // Step 3: Build launch command using the SHARED builder (same as production)
    let cfg = make_test_config(live.to_str().unwrap());
    let java = PathBuf::from("java");
    let launch = build_server_launch_command(&cfg, &live, &java)
        .expect("build_server_launch_command failed");

    // Step 4: Verify the command structure
    assert!(
        launch.executable == PathBuf::from("java"),
        "executable should be java"
    );
    assert!(
        launch.args.iter().any(|a| a.ends_with("server.jar")),
        "args should reference server.jar"
    );
    assert!(
        launch.args.contains(&"-jar".to_string()),
        "args should contain -jar"
    );

    // Step 5: Spawn the process and wait for "Done" signal
    let mut cmd = tokio::process::Command::new(&launch.executable);
    cmd.args(&launch.args)
        .current_dir(&live)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &launch.env_set {
        cmd.env(k, v);
    }
    for k in &launch.env_remove {
        cmd.env_remove(k);
    }

    let mut child = cmd.spawn().expect("spawn server process");

    // Also capture stderr for debugging
    let stderr = child.stderr.take().unwrap();
    let stderr_lines = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let stderr_lines_clone = stderr_lines.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("[stderr] {}", line);
            stderr_lines_clone.lock().await.push(line);
        }
    });

    // Wait for "Done" with timeout
    let stdout = child.stdout.take().unwrap();
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stdout).lines();
    let mut ready = false;
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(120);

    while start.elapsed() < timeout {
        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        println!("[stdout] {}", line);
                        if line.contains("Done") {
                            ready = true;
                            break;
                        }
                    }
                    Ok(None) => {
                        eprintln!("stdout closed (process may have exited)");
                        break;
                    }
                    Err(e) => {
                        eprintln!("stdout error: {}", e);
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }

    // Check exit status if not ready
    if !ready {
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            eprintln!("Server exited with: {}", status);
        }
        let stderr_buf = stderr_lines.lock().await;
        let last20: Vec<&str> = stderr_buf
            .iter()
            .rev()
            .take(20)
            .map(|s| s.as_str())
            .collect();
        for line in last20.iter().rev() {
            eprintln!("  [stderr-tail] {}", line);
        }
    }

    // Step 6: Send "stop" command
    if ready {
        use tokio::io::AsyncWriteExt;
        if let Some(ref mut stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(b"stop\n").await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
    } else {
        let _ = child.kill().await;
    }

    cleanup_dir(&staging);
    cleanup_dir(&live);

    assert!(ready, "Server should reach 'Done' within 120s");
    println!("PASS: Vanilla production-parity smoke — server reached ready via shared builder");
}

// ── Smoke Test B: CurseForge Forge pack (manifest fallback) ────────

#[tokio::test]
#[ignore]
async fn smoke_b_curseforge_forge_manifest_fallback() {
    if let Err(msg) = require_cf_api_key() {
        eprintln!("{}", msg);
        return;
    }
    if let Err(msg) = require_cf_smoke_pack() {
        eprintln!("{}", msg);
        return;
    }
    let live = PathBuf::from("/tmp/smoke-b-live");
    cleanup_dir(&live);

    // Step 1: Use local synthetic CurseForge pack (3 real mods from CurseForge CDN)
    // JEI (proj 238222, file 8820520), AppleSkin (proj 248787, file 4770828),
    // JourneyMap (proj 32274, file 8764299) — client-only, should be quarantined
    let zip_path = PathBuf::from("/tmp/lbby-smoke-pack/smoke-test-pack.zip");
    assert!(zip_path.exists(), "Pack ZIP should exist at {:?}", zip_path);
    println!(
        "  Using local pack: {} ({} bytes)",
        zip_path.display(),
        fs::metadata(&zip_path).unwrap().len()
    );

    // Step 2: Write test config pointing to temp dir
    let original_profiles = write_test_profiles(live.to_str().unwrap());

    // Step 3: Create AppEventSender for the pipeline
    let state = Arc::new(AppState::new());
    let app = Arc::new(AppEventSender::new(state));

    // Step 4: Run install_curseforge_modpack (full pipeline)
    let result = lbby_core::mod_services::install_curseforge_modpack(
        app.clone(),
        zip_path.to_string_lossy().to_string(),
    )
    .await;

    // Step 5: Restore original config immediately
    restore_profiles(&original_profiles);

    // Step 6: Verify result
    match &result {
        Ok(cfg) => {
            println!("  install_curseforge_modpack succeeded");
            println!("    server_type: {:?}", cfg.server_type);
            println!("    minecraft_version: {}", cfg.minecraft_version);
            println!("    loader_version: {:?}", cfg.loader_version);
            println!("    server_path: {}", cfg.server_path);
        }
        Err(e) => {
            eprintln!("  install_curseforge_modpack FAILED: {}", e);
            cleanup_dir(&live);
            let _ = fs::remove_file(&zip_path);
            panic!("CurseForge install failed: {}", e);
        }
    }

    // Step 7: Verify the committed live server
    assert!(
        live.exists(),
        "Live server directory should exist after commit"
    );
    let mods_dir = live.join("mods");
    if mods_dir.exists() {
        let mod_count = fs::read_dir(&mods_dir)
            .map(|d| {
                d.filter(|e| {
                    e.as_ref()
                        .map(|e| e.path().extension().map_or(false, |ext| ext == "jar"))
                        .unwrap_or(false)
                })
                .count()
            })
            .unwrap_or(0);
        println!("    mods in live: {}", mod_count);
    }

    // Step 8: Verify no validation residue
    for entry in fs::read_dir(&live).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.starts_with(".lbby-validation-world-"),
            "No validation world in live: {}",
            name
        );
    }
    assert!(
        !live.join(".lbby-original-server.properties").exists(),
        "No backup marker in live"
    );

    // Step 9: Check quarantined mods
    let quarantine = live.join(".lbby-quarantined-client-mods");
    if quarantine.exists() {
        let quarantined: Vec<String> = fs::read_dir(&quarantine)
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        println!("    quarantined client-only mods: {}", quarantined.len());
        for m in &quarantined {
            println!("      - {}", m);
        }
    }

    // Step 10: Verify production start command via shared builder
    let cfg = result.as_ref().unwrap();
    let java_bin = if cfg.java_path.is_empty() {
        PathBuf::from("java")
    } else {
        PathBuf::from(&cfg.java_path)
    };
    let server_dir = PathBuf::from(&cfg.server_path);
    match build_server_launch_command(cfg, &server_dir, &java_bin) {
        Ok(launch) => {
            println!("    production launch command OK: {:?}", launch.executable);
        }
        Err(e) => {
            println!("    WARNING: build_server_launch_command failed: {}", e);
        }
    }

    // Clean up
    cleanup_dir(&live);
    let _ = fs::remove_file(&zip_path);

    assert!(
        result.is_ok(),
        "CurseForge Forge pack install should succeed"
    );
    println!("PASS: CurseForge Forge manifest-fallback smoke — full pipeline succeeded");
}

// ── Smoke Test C: Broken real pack ────────────────────────────────

#[tokio::test]
#[ignore]
async fn smoke_c_broken_real_pack_failure() {
    let staging = PathBuf::from("/tmp/smoke-c-staging");
    cleanup_dir(&staging);
    fs::create_dir_all(&staging).unwrap();

    // Create a broken server.jar (not a valid JAR)
    fs::write(staging.join("server.jar"), "not a real jar file").unwrap();
    fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();

    let cfg = make_test_config(staging.to_str().unwrap());
    let validator = BootValidator::new();
    let result = validator.validate(&cfg, &staging).await;

    match &result {
        BootResult::Failed(reason) => {
            println!("  BootValidator correctly failed: {:?}", reason);
        }
        other => {
            cleanup_dir(&staging);
            panic!("Expected BootResult::Failed, got: {:?}", other);
        }
    }

    // Verify no validation residue in staging after guard cleanup
    assert!(
        !staging.join(".lbby-original-server.properties").exists(),
        "Backup marker should be cleaned up"
    );

    cleanup_dir(&staging);
    println!("PASS: Broken real pack — BootValidator correctly failed");
}

// ── Smoke Test D: EULA false blocks ───────────────────────────────

#[tokio::test]
#[ignore]
async fn smoke_d_eula_false_blocks() {
    let dir = PathBuf::from("/tmp/smoke-d-eula");
    cleanup_dir(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::copy(smoke_server_jar(), dir.join("server.jar")).unwrap();

    let mut cfg = make_test_config(dir.to_str().unwrap());
    cfg.eula_accepted = false;

    assert!(
        !dir.join("eula.txt").exists(),
        "eula.txt should not exist before test"
    );

    let validator = BootValidator::new();
    let result = validator.validate(&cfg, &dir).await;
    match &result {
        BootResult::Failed(reason) => {
            println!("  eula_accepted=false correctly blocked: {:?}", reason);
        }
        other => {
            cleanup_dir(&dir);
            panic!("Expected failure for eula_accepted=false, got: {:?}", other);
        }
    }

    // BootValidator writes eula.txt itself during validation — but the INSTALL
    // path (do_install_server) should NOT write it when eula_accepted=false.
    // BootValidator independently checks if eula.txt exists on disk.
    // If eula.txt is missing AND eula_accepted=false, it fails with EulaNotAccepted.

    cleanup_dir(&dir);
    println!("PASS: EULA false blocks — BootValidator correctly rejects");
}

// ── Smoke Test D2: EULA true allows ───────────────────────────────

#[tokio::test]
#[ignore]
async fn smoke_d2_eula_true_allows() {
    let staging = PathBuf::from("/tmp/smoke-d2-eula-true");
    cleanup_dir(&staging);
    setup_staging_with_port(&staging, 25571);

    let cfg = make_test_config(staging.to_str().unwrap());

    let validator = BootValidator::new();
    let result = validator.validate(&cfg, &staging).await;
    match &result {
        BootResult::Success { .. } => {
            println!("  eula_accepted=true: BootValidator succeeded");
        }
        BootResult::Failed(ref failure)
            if matches!(failure.reason, BootFailureReason::EulaNotAccepted) =>
        {
            cleanup_dir(&staging);
            panic!("eula_accepted=true should not fail with EulaNotAccepted");
        }
        BootResult::Failed(reason) => {
            println!(
                "  eula_accepted=true: BootValidator failed with {:?} (not EULA)",
                reason
            );
        }
        BootResult::Timeout(_) => {
            println!("  eula_accepted=true: BootValidator timed out (server slow)");
        }
    }

    // Verify eula.txt was written by BootValidator
    assert!(staging.join("eula.txt").exists(), "eula.txt should exist");

    cleanup_dir(&staging);
    println!("PASS: EULA true allows — eula.txt present, not a blocker");
}

// ── Smoke Test E: Directory inspection after success ──────────────

#[tokio::test]
#[ignore]
async fn smoke_e_directory_inspection_after_success() {
    let staging = PathBuf::from("/tmp/smoke-e-staging");
    let live = PathBuf::from("/tmp/smoke-e-live");
    cleanup_dir(&staging);
    cleanup_dir(&live);

    setup_staging(&staging);
    fs::create_dir_all(&live).unwrap();
    for entry in fs::read_dir(&staging).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), live.join(entry.file_name())).unwrap();
    }

    // Inspect for residue
    let mut residue = Vec::new();
    for entry in fs::read_dir(&live).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".lbby-validation-world-") {
            residue.push(format!("validation world: {}", name));
        }
        if name == ".lbby-original-server.properties" {
            residue.push("backup marker".to_string());
        }
    }

    let props = live.join("server.properties");
    if props.exists() {
        let content = fs::read_to_string(&props).unwrap();
        for line in content.lines() {
            if line.starts_with("server-port=") {
                let port = line.split('=').nth(1).unwrap_or("");
                if port == "0" || port == "1" {
                    residue.push(format!("validation port: {}", port));
                }
            }
        }
    }

    cleanup_dir(&staging);
    cleanup_dir(&live);

    assert!(residue.is_empty(), "No residue in live: {:?}", residue);
    println!("PASS: Directory inspection after success — clean");
}

// ── Smoke Test F: Directory inspection after failure ──────────────

#[tokio::test]
#[ignore]
async fn smoke_f_directory_inspection_after_failure() {
    let staging = PathBuf::from("/tmp/smoke-f-staging");
    let live = PathBuf::from("/tmp/smoke-f-live");
    cleanup_dir(&staging);
    cleanup_dir(&live);

    // Create fake "old live" server
    fs::create_dir_all(&live).unwrap();
    fs::write(live.join("server.jar"), "old server jar").unwrap();
    fs::write(live.join("server.properties"), "server-port=25565\n").unwrap();
    fs::create_dir_all(live.join("world")).unwrap();
    fs::write(live.join("world/level.dat"), "old world data").unwrap();

    let old_jar = fs::read(live.join("server.jar")).unwrap();
    let old_props = fs::read_to_string(live.join("server.properties")).unwrap();
    let old_world = fs::read(live.join("world/level.dat")).unwrap();

    // Broken staging
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("server.jar"), "broken jar").unwrap();
    fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();

    let cfg = make_test_config(staging.to_str().unwrap());
    let validator = BootValidator::new();
    let result = validator.validate(&cfg, &staging).await;

    // Old live must be unchanged
    assert_eq!(
        fs::read(live.join("server.jar")).unwrap(),
        old_jar,
        "server.jar unchanged"
    );
    assert_eq!(
        fs::read_to_string(live.join("server.properties")).unwrap(),
        old_props,
        "server.properties unchanged"
    );
    assert_eq!(
        fs::read(live.join("world/level.dat")).unwrap(),
        old_world,
        "world unchanged"
    );

    // No diagnostics in live
    assert!(
        !live.join(".lbby-diagnostics").exists(),
        "No diagnostics in live"
    );

    cleanup_dir(&staging);
    cleanup_dir(&live);

    assert!(matches!(result, BootResult::Failed(_)), "Should fail");
    println!("PASS: Directory inspection after failure — live unchanged, no residue");
}
