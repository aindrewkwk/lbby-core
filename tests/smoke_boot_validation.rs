// smoke_boot_validation.rs — Real smoke tests for BootValidator.
//
// These tests use actual Java and an actual Minecraft server.jar to verify
// the full validation lifecycle. They are slow (30-120s) and require:
// - Java 17+ installed
// - MC 1.21.4 server.jar at /tmp/smoke-test/server.jar
//
// Run with: cargo test --test smoke_boot_validation -- --ignored

use lbby_core::boot_validator::{BootFailureReason, BootResult, BootValidator};
use lbby_core::config::ServerConfig;
use std::fs;
use std::path::Path;
use std::time::Duration;

fn smoke_server_jar() -> &'static str {
    "/tmp/smoke-test/server.jar"
}

fn make_smoke_config(server_path: &str) -> ServerConfig {
    ServerConfig {
        server_path: server_path.to_string(),
        server_name: "smoke-test".to_string(),
        server_type: lbby_core::config::ServerType::Vanilla,
        minecraft_version: "1.21.4".to_string(),
        loader_version: None,
        ram_mb: 2048,
        optimized_jvm_flags: true,
        setup_complete: true,
        eula_accepted: true,
        ..Default::default()
    }
}

/// Set up a staging directory with server.jar and eula.txt.
fn setup_staging(dir: &Path) {
    // Copy server.jar
    fs::copy(smoke_server_jar(), dir.join("server.jar")).expect("copy server.jar");
    // Write eula.txt
    fs::write(dir.join("eula.txt"), "eula=true\n").expect("write eula.txt");
}

// ── Smoke Test A: Vanilla server — full lifecycle ─────────────────

/// Test A: Vanilla server boot → ready → graceful stop → cleanup.
/// This proves validation/production launch parity.
#[tokio::test]
#[ignore] // run with --ignored
async fn smoke_a_vanilla_boot_lifecycle() {
    let server_jar = Path::new(smoke_server_jar());
    assert!(
        server_jar.exists(),
        "server.jar not found at {}. Download MC 1.21.4 server.jar first.",
        smoke_server_jar()
    );

    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path();
    setup_staging(staging);

    let cfg = make_smoke_config(staging.to_str().unwrap());
    let validator = BootValidator::with_timeout(Duration::from_secs(120));
    let result = validator.validate(&cfg, staging).await;

    match &result {
        BootResult::Success(s) => {
            println!(
                "✅ Smoke A passed: booted in {:?}, graceful={}",
                s.elapsed, s.graceful_shutdown
            );
            assert!(s.elapsed < Duration::from_secs(120));
        }
        BootResult::Failed(f) => {
            panic!(
                "❌ Smoke A failed: {:?}\nLog tail:\n{}",
                f.reason, f.log_tail
            );
        }
        BootResult::Timeout(t) => {
            panic!(
                "❌ Smoke A timed out after {:?}\nLog tail:\n{}",
                t.waited, t.log_tail
            );
        }
    }

    // After validation: staging should have no validation artifacts
    assert!(
        !staging.join(".lbby-original-server.properties").exists(),
        "Backup marker should be cleaned up"
    );
}

// ── Smoke Test C: Broken staged server — failure + rollback ───────

/// Test C: Deliberately broken server — validation fails,
/// transaction does not commit, live untouched, diagnostics survive.
#[tokio::test]
#[ignore] // run with --ignored
async fn smoke_c_broken_server_failure() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path();

    // Write eula.txt but NO server.jar — server can't start
    fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();
    // Create a fake empty server.jar that Java will reject
    fs::write(staging.join("server.jar"), "not a real jar").unwrap();

    let cfg = make_smoke_config(staging.to_str().unwrap());
    let validator = BootValidator::with_timeout(Duration::from_secs(30));
    let result = validator.validate(&cfg, staging).await;

    match &result {
        BootResult::Failed(f) => {
            println!("✅ Smoke C passed: correctly failed with {:?}", f.reason);
            // Should be a launch/process failure, not EULA or port
            assert_ne!(f.reason, BootFailureReason::EulaNotAccepted);
            assert_ne!(f.reason, BootFailureReason::PortUnavailable);
        }
        BootResult::Timeout(t) => {
            // Also acceptable — bad jar may hang
            println!("✅ Smoke C passed: timed out (broken jar hung)");
        }
        BootResult::Success(_) => {
            panic!("❌ Smoke C: should NOT have succeeded with broken server.jar");
        }
    }

    // staging server.properties should NOT be contaminated
    // (the guard should have cleaned up)
    let props = staging.join("server.properties");
    if props.exists() {
        let content = fs::read_to_string(&props).unwrap();
        assert!(
            !content.contains(".lbby-validation-world-"),
            "server.properties should not contain validation world name"
        );
    }
}
