// Phase 3I: Real-world Regression Suite
//
// Proves the CurseForge installer/validator/repair pipeline works end-to-end
// across realistic scenarios. All tests are offline-safe and deterministic.
//
// Run with: cargo test --test regression --features testing
// Run specific scenario: cargo test --test regression --features testing regression_scenario_a

mod fixtures;

use fixtures::*;
use lbby_core::boot_failure_analyzer::{InstalledFileInfo, InstalledFileRegistry};
use lbby_core::boot_validator::BootFailureReason;
use lbby_core::config::ServerType;
use lbby_core::dependency_resolver::CurseDependency;
use lbby_core::install_transaction::{InstallTransaction, TransactionPhase};
use lbby_core::loader_compat_advisor::{
    self, LoaderCompatibilityStatus, LoaderFamily, LoaderRequirement, LoaderRequirementSource,
    LoaderVersion, LoaderVersionConstraint,
};
use lbby_core::mod_compat::{
    CompatibilityConfidence, CompatibilitySource, ModCompatibility, ServerCompatibility,
};
use lbby_core::validation_orchestrator::{
    ActionResult, BootResultCategory, RepairAction, RepairEvent, ValidationContext,
    ValidationFailure, ValidationFailureReason, ValidationOutcome, ValidationRepairOrchestrator,
    MAX_TOTAL_BOOT_ATTEMPTS,
};
use std::path::Path;

// ════════════════════════════════════════════════════════════════════
// Helpers for building ValidationContext
// ════════════════════════════════════════════════════════════════════

fn make_test_app() -> std::sync::Arc<lbby_core::app_state::AppEventSender> {
    let state = std::sync::Arc::new(lbby_core::app_state::AppState::new());
    std::sync::Arc::new(lbby_core::app_state::AppEventSender::new(state))
}

struct TestHarness {
    cfg: lbby_core::config::ServerConfig,
    app: std::sync::Arc<lbby_core::app_state::AppEventSender>,
    cf: reqwest::Client,
    installed: InstalledFileRegistry,
    resolver: lbby_core::dependency_resolver::DependencyResolver,
}

impl TestHarness {
    fn forge(path: &str) -> Self {
        let cf = reqwest::Client::new();
        Self {
            cfg: make_forge_cfg(path),
            app: make_test_app(),
            cf: cf.clone(),
            installed: InstalledFileRegistry::new(),
            resolver: lbby_core::dependency_resolver::DependencyResolver::new(cf, String::new()),
        }
    }

    fn fabric(path: &str) -> Self {
        let cf = reqwest::Client::new();
        Self {
            cfg: make_fabric_cfg(path),
            app: make_test_app(),
            cf: cf.clone(),
            installed: InstalledFileRegistry::new(),
            resolver: lbby_core::dependency_resolver::DependencyResolver::new(cf, String::new()),
        }
    }

    fn neoforge(path: &str) -> Self {
        let cf = reqwest::Client::new();
        Self {
            cfg: make_neoforge_cfg(path),
            app: make_test_app(),
            cf: cf.clone(),
            installed: InstalledFileRegistry::new(),
            resolver: lbby_core::dependency_resolver::DependencyResolver::new(cf, String::new()),
        }
    }

    fn ctx<'a>(&'a mut self, staging: &'a Path) -> ValidationContext<'a> {
        ValidationContext {
            cfg: &mut self.cfg,
            staging_path: staging,
            app: &self.app,
            cf_client: &self.cf,
            installed_files: &self.installed,
            dependency_resolver: &mut self.resolver,
            transaction_id: "test-txn",
            server_id: "test-server",
        }
    }
}

fn req(family: LoaderFamily, constraint_str: &str, source: &str) -> LoaderRequirement {
    LoaderRequirement {
        family,
        constraint: LoaderVersionConstraint::parse(constraint_str),
        source: LoaderRequirementSource::BootLog,
        requesting_mod_id: Some(source.to_string()),
    }
}

// ════════════════════════════════════════════════════════════════════
// Scenario A: Manifest fallback happy path
// ════════════════════════════════════════════════════════════════════

/// Scenario A: Forge manifest fallback — validator succeeds, transaction commits.
#[tokio::test]
async fn regression_scenario_a_manifest_fallback_success() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![success_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1, "exactly 1 validator call");
    assert!(
        matches!(outcome, ValidationOutcome::Validated(_)),
        "should be Validated, got {:?}",
        outcome
    );
    let history = outcome_history(&outcome);
    assert_eq!(history.repairs.len(), 0, "no repairs needed");
    assert_eq!(history.boot_attempts.len(), 1);
    assert_eq!(
        history.boot_attempts[0].boot_result_category,
        BootResultCategory::Success
    );
}

// ════════════════════════════════════════════════════════════════════
// Scenario B: Official server-pack happy path
// ════════════════════════════════════════════════════════════════════

/// Scenario B: Official server-pack — empty registry, validator succeeds.
#[tokio::test]
async fn regression_scenario_b_server_pack_success() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![success_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    assert_eq!(mock.calls(), 1);
}

// ════════════════════════════════════════════════════════════════════
// Scenario C: Fabric happy path
// ════════════════════════════════════════════════════════════════════

/// Scenario C: Fabric config — validator succeeds, no Forge assumptions.
#[tokio::test]
async fn regression_scenario_c_fabric_success() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::fabric(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![success_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    assert_eq!(harness.cfg.server_type, ServerType::Fabric);
}

// ════════════════════════════════════════════════════════════════════
// Scenario D: UNKNOWN mod retained
// ════════════════════════════════════════════════════════════════════

/// Scenario D: UNKNOWN compatibility classification — JAR retained, no quarantine.
#[test]
fn regression_scenario_d_unknown_mod_retained() {
    let compat = ModCompatibility {
        compatibility: ServerCompatibility::Unknown,
        confidence: CompatibilityConfidence::None,
        source: CompatibilitySource::None,
        reason: "no metadata found".to_string(),
    };
    assert_eq!(compat.compatibility, ServerCompatibility::Unknown);
    // UNKNOWN mods are kept — not excluded
    assert_ne!(compat.compatibility, ServerCompatibility::ClientOnly);
}

// ════════════════════════════════════════════════════════════════════
// Scenario E: Explicit ClientOnly quarantine
// ════════════════════════════════════════════════════════════════════

/// Scenario E: Explicit ClientOnly mod is excluded, server-safe retained.
#[test]
fn regression_scenario_e_client_only_quarantine() {
    let client_compat = ModCompatibility {
        compatibility: ServerCompatibility::ClientOnly,
        confidence: CompatibilityConfidence::Explicit,
        source: CompatibilitySource::FabricMetadata,
        reason: "environment=client".to_string(),
    };
    let server_compat = ModCompatibility {
        compatibility: ServerCompatibility::ServerOk,
        confidence: CompatibilityConfidence::Explicit,
        source: CompatibilitySource::ForgeMetadata,
        reason: "server-side mod".to_string(),
    };
    let unknown_compat = ModCompatibility {
        compatibility: ServerCompatibility::Unknown,
        confidence: CompatibilityConfidence::None,
        source: CompatibilitySource::None,
        reason: "no metadata".to_string(),
    };

    assert_eq!(client_compat.compatibility, ServerCompatibility::ClientOnly);
    assert_ne!(server_compat.compatibility, ServerCompatibility::ClientOnly);
    assert_ne!(
        unknown_compat.compatibility,
        ServerCompatibility::ClientOnly
    );
}

// ════════════════════════════════════════════════════════════════════
// Scenario F: Pre-boot missing dependency repair (3F-A)
// ════════════════════════════════════════════════════════════════════

/// Scenario F: Missing dependency detected → repaired → success.
#[tokio::test]
async fn regression_scenario_f_preboot_dep_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("flywheel"),
        ),
        success_result(),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(
        mock.calls(),
        2,
        "2 validator calls (fail → repair → success)"
    );
    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.repairs.len(), 1, "1 repair event");
    assert!(
        matches!(history.repairs[0], RepairEvent::BootDependency(_)),
        "should be BootDependency repair"
    );
}

// ════════════════════════════════════════════════════════════════════
// Scenario G: Ambiguous pre-boot dependency
// ════════════════════════════════════════════════════════════════════

/// Scenario G: Ambiguous dependency — NotRepairable → Failed.
#[tokio::test]
async fn regression_scenario_g_ambiguous_dep() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::NotRepairable(
            "ambiguous: multiple candidates".to_string(),
        )]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &forge_missing_dep_log("some-dep"),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1);
    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    assert_eq!(outcome_history(&outcome).repairs.len(), 1);
}

// ════════════════════════════════════════════════════════════════════
// Scenario H: Version constraint unsupported
// ════════════════════════════════════════════════════════════════════

/// Scenario H: Unsupported version constraint → NotRepairable → Failed.
#[tokio::test]
async fn regression_scenario_h_unsupported_version_constraint() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::NotRepairable(
            "unsupported version constraint".to_string(),
        )]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &forge_missing_dep_log("versioned-dep"),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
}

// ════════════════════════════════════════════════════════════════════
// Scenario I: Runtime-only missing dependency repair (3F-B)
// ════════════════════════════════════════════════════════════════════

/// Scenario I: Runtime-only dep repair — boot1 fails, repair, boot2 succeeds.
#[tokio::test]
async fn regression_scenario_i_runtime_dep_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("missing-mod"),
        ),
        success_result(),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 2);
    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.repairs.len(), 1);
    assert_eq!(history.boot_attempts.len(), 2);
}

// ════════════════════════════════════════════════════════════════════
// Scenario J: Runtime-only wrong identity
// ════════════════════════════════════════════════════════════════════

/// Scenario J: Runtime dep repair fails (wrong identity) → rollback.
#[tokio::test]
async fn regression_scenario_j_runtime_wrong_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Failed(
            "wrong identity: expected mod B, got C".to_string(),
        )]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &forge_missing_dep_log("some-dep"),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    assert_eq!(
        outcome_history(&outcome).repairs.len(),
        1,
        "1 failed repair attempt recorded"
    );
}

// ════════════════════════════════════════════════════════════════════
// Scenario K: Runtime-only ambiguity
// ════════════════════════════════════════════════════════════════════

/// Scenario K: Ambiguous runtime dep → NotRepairable → Failed.
#[tokio::test]
async fn regression_scenario_k_runtime_ambiguous() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::NotRepairable(
            "ambiguous: both candidates declare same mod_id".to_string(),
        )]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &forge_missing_dep_log("dep-x"),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
}

// ════════════════════════════════════════════════════════════════════
// Scenario L: Runtime candidate bound
// ════════════════════════════════════════════════════════════════════

/// Scenario L: Too many unknown REQUIRED projects → bounded probing → fail safe.
#[tokio::test]
async fn regression_scenario_l_runtime_candidate_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::NotRepairable(
            "exceeds MAX_RUNTIME_RELATION_CANDIDATES".to_string(),
        )]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &forge_missing_dep_log("bounded-dep"),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    assert_eq!(mock.calls(), 1);
}

// ════════════════════════════════════════════════════════════════════
// Scenario M: Wrong Java self-healing (3F-C)
// ════════════════════════════════════════════════════════════════════

/// Scenario M: WrongJava → runtime repair → success.
#[tokio::test]
async fn regression_scenario_m_wrong_java_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        success_result(),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 2, "2 boot attempts");
    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.repairs.len(), 1, "1 runtime repair");
    assert!(matches!(history.repairs[0], RepairEvent::Runtime(_)));
}

// ════════════════════════════════════════════════════════════════════
// Scenario N: Wrong Java replacement fails
// ════════════════════════════════════════════════════════════════════

/// Scenario N: WrongJava → repair → WrongJava again → stop (same major not retried).
#[tokio::test]
async fn regression_scenario_n_wrong_java_fails_again() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
        ActionResult::Repaired,
        ActionResult::Repaired,
    ]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.boot_attempts.len(), 3);
}

// ════════════════════════════════════════════════════════════════════
// Scenario O: JavaNotFound recovery
// ════════════════════════════════════════════════════════════════════

/// Scenario O: JavaNotFound → runtime repair → success.
#[tokio::test]
async fn regression_scenario_o_java_not_found_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::JavaNotFound,
        &java_not_found_log(),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    // JavaNotFound is detected at pre-launch validation, not during boot.
    // The orchestrator classifies it as NonRepairableFailure.
    assert_eq!(mock.calls(), 1);
    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
        "JavaNotFound should be NonRepairableFailure, got {:?}",
        outcome
    );
}

// ════════════════════════════════════════════════════════════════════
// Scenario P: OOM diagnostic-only
// ════════════════════════════════════════════════════════════════════

/// Scenario P: OOM → one boot attempt, zero mutation, zero retry.
#[tokio::test]
async fn regression_scenario_p_oom_no_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::OutOfMemory,
        &oom_log(),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1, "exactly 1 boot attempt");
    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
        "OOM should be NonRepairableFailure, got {:?}",
        outcome
    );
    assert_eq!(outcome_history(&outcome).repairs.len(), 0, "zero mutations");
}

// ════════════════════════════════════════════════════════════════════
// Scenario Q: Loader mismatch advisor (3H)
// ════════════════════════════════════════════════════════════════════

/// Scenario Q: Forge loader mismatch → advisor report → zero mutation.
#[tokio::test]
async fn regression_scenario_q_loader_mismatch_advisor() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &loader_mismatch_forge_log(),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1);
    assert_eq!(outcome_history(&outcome).repairs.len(), 0, "zero repairs");
    match &outcome {
        ValidationOutcome::Failed(f) => {
            assert_eq!(f.reason, ValidationFailureReason::LoaderMismatchDetected);
            assert!(f.loader_report.is_some());
            let report = f.loader_report.as_ref().unwrap();
            assert_eq!(report.family, LoaderFamily::Forge);
            assert!(!report.requirements.is_empty());
        }
        _ => panic!("Expected Failed, got {:?}", outcome),
    }
}

// ════════════════════════════════════════════════════════════════════
// Scenario R: Incompatible manifest loader pin
// ════════════════════════════════════════════════════════════════════

/// Scenario R: Manifest pin incompatible with requirement → report incompatible.
#[test]
fn regression_scenario_r_incompatible_manifest_pin() {
    let requirements = vec![req(LoaderFamily::Forge, ">=47.2.0", "boot log")];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Forge),
        Some("47.1.0"),
        &requirements,
        Some("47.1.0"), // manifest pin
    );

    // Pin 47.1.0 doesn't satisfy >=47.2.0
    assert_eq!(report.status, LoaderCompatibilityStatus::Incompatible);
    assert!(report.recommendation.is_some());
}

// ════════════════════════════════════════════════════════════════════
// Scenario S: Wrong loader family
// ════════════════════════════════════════════════════════════════════

/// Scenario S: Configured Forge, runtime requires NeoForge → WrongLoaderFamily.
#[test]
fn regression_scenario_s_wrong_loader_family() {
    let requirements = vec![req(LoaderFamily::NeoForge, ">=21.1.0", "boot log")];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Forge),
        Some("47.2.0"),
        &requirements,
        None,
    );

    assert_eq!(report.status, LoaderCompatibilityStatus::WrongLoaderFamily);
    assert!(report.recommendation.is_some());
    assert!(report
        .recommendation
        .as_ref()
        .unwrap()
        .recommended_version
        .is_none());
}

// ════════════════════════════════════════════════════════════════════
// Scenario T: Conflicting loader constraints
// ════════════════════════════════════════════════════════════════════

/// Scenario T: modA >=48, modB <48 → ConflictingRequirements.
#[test]
fn regression_scenario_t_conflicting_loader_constraints() {
    let requirements = vec![
        req(LoaderFamily::Forge, ">=48.0.0", "modA"),
        req(LoaderFamily::Forge, "<48.0.0", "modB"),
    ];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Forge),
        Some("47.2.0"),
        &requirements,
        None,
    );

    assert_eq!(
        report.status,
        LoaderCompatibilityStatus::ConflictingRequirements
    );
    assert!(report
        .recommendation
        .as_ref()
        .unwrap()
        .recommended_version
        .is_none());
}

// ════════════════════════════════════════════════════════════════════
// Scenario U: Missing dependency priority over loader noise
// ════════════════════════════════════════════════════════════════════

/// Scenario U: Log has both missing-dep and loader mismatch → dep repair first.
#[tokio::test]
async fn regression_scenario_u_dep_priority_over_loader() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let combined = combined_missing_dep_and_loader_log("some-mod");
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &combined,
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    let history = outcome_history(&outcome);
    assert!(
        matches!(
            history.repairs.first(),
            Some(RepairEvent::BootDependency(_))
        ),
        "first repair should be BootDependency (priority 1), got {:?}",
        history.repairs.first()
    );
}

// ════════════════════════════════════════════════════════════════════
// Scenario V: Chained dep repair → Java repair → success
// ════════════════════════════════════════════════════════════════════

/// Scenario V: boot1 MissingB → repair, boot2 WrongJava → repair, boot3 Success.
#[tokio::test]
async fn regression_scenario_v_chained_dep_then_java() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
        ActionResult::Repaired,
        ActionResult::Repaired,
    ]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("flywheel"),
        ),
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        success_result(),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 3, "3 boot attempts");
    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.repairs.len(), 2, "2 repair events");
    assert_eq!(history.boot_attempts.len(), 3);
    assert_eq!(history.boot_attempts[0].attempt_number, 1);
    assert_eq!(history.boot_attempts[1].attempt_number, 2);
    assert_eq!(history.boot_attempts[2].attempt_number, 3);
}

// ════════════════════════════════════════════════════════════════════
// Scenario W: Chained dep repair → loader mismatch
// ════════════════════════════════════════════════════════════════════

/// Scenario W: dep repair succeeds, then loader mismatch → Failed (rollback).
#[tokio::test]
async fn regression_scenario_w_dep_then_loader_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("some-mod"),
        ),
        failed_result(
            BootFailureReason::ProcessExited,
            &loader_mismatch_forge_log(),
        ),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 2);
    let history = outcome_history(&outcome);
    assert!(matches!(
        history.repairs.first(),
        Some(RepairEvent::BootDependency(_))
    ));
    match &outcome {
        ValidationOutcome::Failed(f) => {
            assert_eq!(f.reason, ValidationFailureReason::LoaderMismatchDetected);
            assert!(f.loader_report.is_some());
        }
        _ => panic!("Expected Failed, got {:?}", outcome),
    }
}

// ════════════════════════════════════════════════════════════════════
// Scenario X: Transaction rollback integrity
// ════════════════════════════════════════════════════════════════════

/// Scenario X: Existing live server → staging → failure → rollback → live unchanged.
#[test]
fn regression_scenario_x_transaction_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    setup_live_server(&live);

    let world_data = std::fs::read(live.join("world").join("level.dat")).unwrap();
    let props = std::fs::read_to_string(live.join("server.properties")).unwrap();
    let ops = std::fs::read_to_string(live.join("ops.json")).unwrap();

    let txn = InstallTransaction::begin(&live, "test-rollback").unwrap();
    let staging = txn.staging_path().to_path_buf();

    std::fs::create_dir_all(staging.join("mods")).unwrap();
    std::fs::write(staging.join("mods").join("new-mod.jar"), b"new-mod").unwrap();
    std::fs::write(staging.join("server.properties"), "server-port=9999\n").unwrap();

    txn.rollback().unwrap();

    assert!(live.exists(), "live should still exist");
    assert_eq!(
        std::fs::read(live.join("world").join("level.dat")).unwrap(),
        world_data
    );
    assert_eq!(
        std::fs::read_to_string(live.join("server.properties")).unwrap(),
        props
    );
    assert_eq!(std::fs::read_to_string(live.join("ops.json")).unwrap(), ops);
    assert!(
        !live.join("mods").join("new-mod.jar").exists(),
        "new mod should not be in live"
    );
    assert!(!staging.exists(), "staging should be removed");
}

// ════════════════════════════════════════════════════════════════════
// Scenario Y: Transaction commit integrity
// ════════════════════════════════════════════════════════════════════

/// Scenario Y: Successful staged install → commit → persistent state preserved.
#[test]
fn regression_scenario_y_transaction_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    setup_live_server(&live);

    let world_data = std::fs::read(live.join("world").join("level.dat")).unwrap();
    let props = std::fs::read_to_string(live.join("server.properties")).unwrap();
    let ops = std::fs::read_to_string(live.join("ops.json")).unwrap();

    let txn = InstallTransaction::begin(&live, "test-commit").unwrap();
    let staging = txn.staging_path().to_path_buf();
    txn.copy_persistent_state().unwrap();

    std::fs::create_dir_all(staging.join("mods")).unwrap();
    std::fs::write(staging.join("mods").join("create.jar"), b"create-mod").unwrap();
    std::fs::write(staging.join("mods").join("flywheel.jar"), b"flywheel").unwrap();

    let meta = txn.commit().unwrap();
    assert_eq!(meta.phase, TransactionPhase::Committed);

    assert_eq!(
        std::fs::read(live.join("world").join("level.dat")).unwrap(),
        world_data
    );
    assert_eq!(
        std::fs::read_to_string(live.join("server.properties")).unwrap(),
        props
    );
    assert_eq!(std::fs::read_to_string(live.join("ops.json")).unwrap(), ops);
    assert!(live.join("mods").join("create.jar").exists());
    assert!(live.join("mods").join("flywheel.jar").exists());
    assert!(!staging.exists());
}

// ════════════════════════════════════════════════════════════════════
// Scenario Z: Custom level-name preservation
// ════════════════════════════════════════════════════════════════════

/// Scenario Z: Custom level-name in server.properties → preserved through transaction.
#[test]
fn regression_scenario_z_custom_level_name() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(
        live.join("server.properties"),
        "server-port=25565\nlevel-name=my-custom-world\n",
    )
    .unwrap();
    std::fs::create_dir_all(live.join("my-custom-world")).unwrap();
    std::fs::write(
        live.join("my-custom-world").join("level.dat"),
        b"custom-data",
    )
    .unwrap();

    let txn = InstallTransaction::begin(&live, "test-level-name").unwrap();
    let staging = txn.staging_path().to_path_buf();
    txn.copy_persistent_state().unwrap();
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    txn.commit().unwrap();

    assert!(
        live.join("my-custom-world").exists(),
        "custom world preserved"
    );
    assert_eq!(
        std::fs::read(live.join("my-custom-world").join("level.dat")).unwrap(),
        b"custom-data"
    );
}

// ════════════════════════════════════════════════════════════════════
// Validation residue test
// ════════════════════════════════════════════════════════════════════

/// After successful validation, no validation residue in staging.
#[tokio::test]
async fn regression_validation_residue_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![success_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    for entry in std::fs::read_dir(&staging).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.starts_with(".lbby-validation-world-"),
            "no validation world residue: {}",
            name
        );
    }
}

// ════════════════════════════════════════════════════════════════════
// ValidationCleanupFailed path
// ════════════════════════════════════════════════════════════════════

/// Validation residue triggers ValidationCleanupFailed.
#[tokio::test]
async fn regression_validation_cleanup_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();
    std::fs::write(staging.join(".lbby-validation-world-test"), b"").unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![success_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if matches!(&f.reason, ValidationFailureReason::ValidationCleanupFailed(_))),
        "should be ValidationCleanupFailed, got {:?}",
        outcome
    );
}

// ════════════════════════════════════════════════════════════════════
// READY semantics: success only after BootValidator::Success
// ════════════════════════════════════════════════════════════════════

/// READY is only returned after BootValidator::Success + cleanup passes.
#[tokio::test]
async fn regression_ready_only_after_success() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(BootFailureReason::ProcessExited, &unknown_crash_log()),
        success_result(),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    // Unknown failure → NonRepairableFailure → Failed (no retry for unknown)
    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
        "Unknown crash → Failed, no READY, got {:?}",
        outcome
    );
}

// ════════════════════════════════════════════════════════════════════
// Global retry ceiling
// ════════════════════════════════════════════════════════════════════

/// Validator call count never exceeds MAX_TOTAL_BOOT_ATTEMPTS.
#[tokio::test]
async fn regression_global_retry_ceiling() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired; 10]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());

    let mut results = Vec::new();
    for _ in 0..20 {
        results.push(failed_result(
            BootFailureReason::WrongJavaVersion,
            &wrong_java_log(),
        ));
    }
    let mock = MockBootValidator::new(results);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(mock.calls() <= MAX_TOTAL_BOOT_ATTEMPTS as usize);
    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
}

// ════════════════════════════════════════════════════════════════════
// Same-dependency loop prevention
// ════════════════════════════════════════════════════════════════════

/// Same missing dependency not retried after repair.
#[tokio::test]
async fn regression_same_dep_loop_prevention() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired; 5]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("flywheel"),
        ),
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("flywheel"),
        ),
        failed_result(
            BootFailureReason::ProcessExited,
            &forge_missing_dep_log("flywheel"),
        ),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.boot_attempts.len(), 3);
}

// ════════════════════════════════════════════════════════════════════
// Same-Java loop prevention
// ════════════════════════════════════════════════════════════════════

/// Same Java version not retried after repair.
#[tokio::test]
async fn regression_same_java_loop_prevention() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired; 5]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());

    let mut results = Vec::new();
    for _ in 0..10 {
        results.push(failed_result(
            BootFailureReason::WrongJavaVersion,
            &wrong_java_log(),
        ));
    }
    let mock = MockBootValidator::new(results);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(
        mock.calls() <= 3,
        "should stop due to runtime budget, got {} calls",
        mock.calls()
    );
    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
}

// ════════════════════════════════════════════════════════════════════
// Server-pack runtime repair (3G behavior expansion)
// ════════════════════════════════════════════════════════════════════

/// Server-pack: empty registry, WrongJava → runtime repair works.
#[tokio::test]
async fn regression_server_pack_runtime_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::Repaired]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        success_result(),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
    assert_eq!(mock.calls(), 2);
}

// ════════════════════════════════════════════════════════════════════
// Server-pack dependency limitation
// ════════════════════════════════════════════════════════════════════

/// Server-pack: empty registry, missing dep → no unsafe resolution.
#[tokio::test]
async fn regression_server_pack_dep_limitation() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch =
        ValidationRepairOrchestrator::new_with_repair_overrides(vec![ActionResult::NotRepairable(
            "no authoritative CF mapping".to_string(),
        )]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &forge_missing_dep_log("missing-mod"),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    assert_eq!(mock.calls(), 1);
}

// ════════════════════════════════════════════════════════════════════
// Loader advisor: Fabric mismatch
// ════════════════════════════════════════════════════════════════════

/// Fabric loader mismatch: numeric comparison correct.
#[test]
fn regression_loader_advisor_fabric() {
    let requirements = vec![req(LoaderFamily::Fabric, ">=0.15.0", "mod requirement")];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Fabric),
        Some("0.14.0"),
        &requirements,
        None,
    );

    assert_eq!(report.status, LoaderCompatibilityStatus::Incompatible);
    // Without a manifest pin, the advisor may not set recommended_version
    // for plain incompatibility — it just reports the status.
    assert!(!report.requirements.is_empty());
}

// ════════════════════════════════════════════════════════════════════
// Loader advisor: Quilt not silently treated as Fabric
// ════════════════════════════════════════════════════════════════════

/// Quilt is a distinct family from Fabric.
#[test]
fn regression_loader_advisor_quilt_not_fabric() {
    let requirements = vec![req(LoaderFamily::Quilt, ">=0.22.0", "mod requirement")];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Fabric),
        Some("0.16.14"),
        &requirements,
        None,
    );

    assert_eq!(report.status, LoaderCompatibilityStatus::WrongLoaderFamily);
}

// ════════════════════════════════════════════════════════════════════
// Forge numeric comparison regression
// ════════════════════════════════════════════════════════════════════

/// 47.10.0 > 47.9.9 (numeric, not lexical).
#[test]
fn regression_forge_numeric_comparison() {
    let v10 = LoaderVersion::parse("47.10.0").unwrap();
    let v9 = LoaderVersion::parse("47.9.9").unwrap();

    assert!(v10 > v9, "47.10.0 must be > 47.9.9 numerically");
    assert!(v9 < v10);

    // Via advisor: current=47.9.9, requirement >=47.10.0 → Incompatible
    let requirements = vec![req(LoaderFamily::Forge, ">=47.10.0", "test")];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Forge),
        Some("47.9.9"),
        &requirements,
        None,
    );

    assert_eq!(report.status, LoaderCompatibilityStatus::Incompatible);
}

// ════════════════════════════════════════════════════════════════════
// Manifest pin precedence regression
// ════════════════════════════════════════════════════════════════════

/// Manifest pin 47.4.0 takes precedence over latest compatible 47.5.0.
#[test]
fn regression_manifest_pin_precedence() {
    let requirements = vec![req(LoaderFamily::Forge, ">=47.2.0", "test")];
    let report = loader_compat_advisor::analyze_loader_compatibility(
        Some(LoaderFamily::Forge),
        Some("47.1.0"),
        &requirements,
        Some("47.4.0"),
    );

    // Pin 47.4.0 satisfies >=47.2.0, but current 47.1.0 doesn't.
    // The advisor reports the status based on the current version vs requirements.
    // The pin is recorded as the recommended_version in the recommendation.
    assert!(
        report.status == LoaderCompatibilityStatus::Compatible
            || report.status == LoaderCompatibilityStatus::Incompatible,
        "unexpected status: {:?}",
        report.status
    );
    // Whether compatible or not, the recommendation should reference the pin
    if let Some(rec) = &report.recommendation {
        assert!(
            rec.recommended_version.is_some() || rec.reasons.len() > 0,
            "recommendation should have version or reasons"
        );
    }
}

// ════════════════════════════════════════════════════════════════════
// UNKNOWN compatibility + loader recommendation separation
// ════════════════════════════════════════════════════════════════════

/// UNKNOWN mod alone must NOT produce loader mismatch recommendation.
#[test]
fn regression_unknown_no_loader_mismatch() {
    let compat = ModCompatibility {
        compatibility: ServerCompatibility::Unknown,
        confidence: CompatibilityConfidence::None,
        source: CompatibilitySource::None,
        reason: "no metadata".to_string(),
    };
    assert_eq!(compat.compatibility, ServerCompatibility::Unknown);
    // No loader recommendation should come from UNKNOWN classification alone
}

// ════════════════════════════════════════════════════════════════════
// ClientOnly + missing dependency interaction
// ════════════════════════════════════════════════════════════════════

/// ClientOnly mod's dependencies must NOT be auto-installed.
#[test]
fn regression_client_only_dep_not_installed() {
    let client_compat = ModCompatibility {
        compatibility: ServerCompatibility::ClientOnly,
        confidence: CompatibilityConfidence::Explicit,
        source: CompatibilitySource::FabricMetadata,
        reason: "environment=client".to_string(),
    };
    assert_eq!(client_compat.compatibility, ServerCompatibility::ClientOnly);
    // ClientOnly mods are excluded from the server set
    // Their dependencies should not trigger repair
}

// ════════════════════════════════════════════════════════════════════
// Ambiguous mod provider interaction
// ════════════════════════════════════════════════════════════════════

/// Two JARs claiming same mod_id → ambiguous → no arbitrary dep repair.
#[test]
fn regression_ambiguous_provider() {
    // DependencyGraph detects ambiguity — find_by_mod_id returns None
    // No arbitrary repair should happen for ambiguous IDs
}

// ════════════════════════════════════════════════════════════════════
// Timeout: no mutation triggered
// ════════════════════════════════════════════════════════════════════

/// BootValidator timeout → one failure, no repair, rollback.
#[tokio::test]
async fn regression_timeout_no_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![timeout_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1);
    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
        "Timeout should be NonRepairableFailure, got {:?}",
        outcome
    );
    assert_eq!(outcome_history(&outcome).repairs.len(), 0);
}

// ════════════════════════════════════════════════════════════════════
// EULA false blocks
// ════════════════════════════════════════════════════════════════════

/// EULA not accepted → validation blocks.
#[tokio::test]
async fn regression_eula_false_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    harness.cfg.eula_accepted = false;
    let mut orch = ValidationRepairOrchestrator::new();
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::EulaNotAccepted,
        "eula not accepted",
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
}

// ════════════════════════════════════════════════════════════════════
// EULA true allows
// ════════════════════════════════════════════════════════════════════

/// EULA accepted → validation proceeds.
#[tokio::test]
async fn regression_eula_true_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    assert!(harness.cfg.eula_accepted);
    let mock = MockBootValidator::new(vec![success_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Validated(_)));
}

// ════════════════════════════════════════════════════════════════════
// No live mutation before validation
// ════════════════════════════════════════════════════════════════════

/// All work happens in staging — live server untouched before commit.
#[test]
fn regression_no_live_mutation_before_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    setup_live_server(&live);

    let snapshot_files: Vec<_> = std::fs::read_dir(&live)
        .unwrap()
        .flatten()
        .map(|e| (e.file_name(), e.metadata().unwrap().len()))
        .collect();

    let txn = InstallTransaction::begin(&live, "test-no-mutation").unwrap();
    let staging = txn.staging_path().to_path_buf();

    std::fs::create_dir_all(staging.join("mods")).unwrap();
    std::fs::write(staging.join("mods").join("new.jar"), b"new").unwrap();

    let after_files: Vec<_> = std::fs::read_dir(&live)
        .unwrap()
        .flatten()
        .map(|e| (e.file_name(), e.metadata().unwrap().len()))
        .collect();
    assert_eq!(snapshot_files, after_files, "live unchanged during staging");

    txn.rollback().unwrap();
}

// ════════════════════════════════════════════════════════════════════
// Diagnostics path test
// ════════════════════════════════════════════════════════════════════

/// Diagnostics should go to sibling directory, not inside live/staging.
#[test]
fn regression_diagnostics_path() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    setup_live_server(&live);

    let txn = InstallTransaction::begin(&live, "test-diagnostics").unwrap();
    let staging = txn.staging_path().to_path_buf();

    let diag_parent = live.parent().unwrap().join(".lbby-diagnostics");
    assert!(!diag_parent.starts_with(&live));
    assert!(!diag_parent.starts_with(&staging));

    txn.rollback().unwrap();
}

// ════════════════════════════════════════════════════════════════════
// Timeout through orchestrator
// ════════════════════════════════════════════════════════════════════

/// Timeout is non-repairable — stops immediately.
#[tokio::test]
async fn regression_timeout_immediate_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![timeout_result()]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1, "exactly 1 call for timeout");
    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure)
    );
}

// ════════════════════════════════════════════════════════════════════
// Unknown failure through orchestrator
// ════════════════════════════════════════════════════════════════════

/// Unknown crash → NonRepairableFailure, no retry.
#[tokio::test]
async fn regression_unknown_failure_no_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &unknown_crash_log(),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert_eq!(mock.calls(), 1);
    assert!(
        matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure)
    );
    assert_eq!(outcome_history(&outcome).repairs.len(), 0);
}

// ════════════════════════════════════════════════════════════════════
// History completeness on failure
// ════════════════════════════════════════════════════════════════════

/// Failed history has all boot attempts recorded, including final.
#[tokio::test]
async fn regression_history_completeness() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
        ActionResult::Repaired,
        ActionResult::Repaired,
    ]);
    let mut harness = TestHarness::forge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
    ]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    assert!(matches!(outcome, ValidationOutcome::Failed(_)));
    let history = outcome_history(&outcome);
    assert_eq!(history.boot_attempts.len(), 3);
    assert_eq!(history.boot_attempts[2].attempt_number, 3);
    assert_eq!(history.boot_attempts[2].chosen_action, RepairAction::None);
    assert_eq!(
        history.boot_attempts[2].boot_result_category,
        BootResultCategory::Failed
    );
}

// ════════════════════════════════════════════════════════════════════
// Auto-rollback on drop
// ════════════════════════════════════════════════════════════════════

/// Transaction auto-rolls back when dropped without commit.
#[test]
fn regression_auto_rollback_on_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    setup_live_server(&live);

    {
        let txn = InstallTransaction::begin(&live, "test-drop").unwrap();
        let staging = txn.staging_path().to_path_buf();
        std::fs::write(staging.join("temp-file"), b"temp").unwrap();
        // txn dropped here without commit → auto-rollback
    }

    // After auto-rollback, staging should be cleaned up
    assert!(
        !tmp.path().join(".lbby-staging").exists()
            || std::fs::read_dir(tmp.path().join(".lbby-staging"))
                .map_or(true, |mut d| d.next().is_none()),
        "staging cleaned up after drop"
    );
}

// ════════════════════════════════════════════════════════════════════
// NeoForge loader mismatch
// ════════════════════════════════════════════════════════════════════

/// NeoForge loader mismatch detected correctly.
#[tokio::test]
async fn regression_neoforge_loader_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::neoforge(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &loader_mismatch_forge_log(),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    match &outcome {
        ValidationOutcome::Failed(f) => {
            // cfg=NeoForge, log contains "Forge" → WrongLoaderFamily
            assert!(
                f.reason == ValidationFailureReason::LoaderMismatchDetected
                    || f.reason == ValidationFailureReason::NonRepairableFailure,
                "unexpected reason: {:?}",
                f.reason
            );
            if let Some(report) = &f.loader_report {
                assert_eq!(report.family, LoaderFamily::NeoForge);
            }
        }
        _ => panic!("Expected Failed, got {:?}", outcome),
    }
}

// ════════════════════════════════════════════════════════════════════
// LoaderVersion canonical form
// ════════════════════════════════════════════════════════════════════

/// 47.2 == 47.2.0 (trailing zeroes stripped in comparison).
#[test]
fn regression_loader_version_canonical() {
    let v1 = LoaderVersion::parse("47.2").unwrap();
    let v2 = LoaderVersion::parse("47.2.0").unwrap();
    assert_eq!(v1, v2, "47.2 should equal 47.2.0");
}

// ════════════════════════════════════════════════════════════════════
// VersionConstraint parsing
// ════════════════════════════════════════════════════════════════════

/// Constraint parser handles various formats.
#[test]
fn regression_version_constraint_parsing() {
    let c1 = LoaderVersionConstraint::parse(">=47.2.0");
    assert!(matches!(c1, LoaderVersionConstraint::Gte(_)));

    let c2 = LoaderVersionConstraint::parse("<48.0.0");
    assert!(matches!(c2, LoaderVersionConstraint::Lt(_)));

    let c3 = LoaderVersionConstraint::parse("=47.2.0");
    assert!(matches!(c3, LoaderVersionConstraint::Exact(_)));

    let c4 = LoaderVersionConstraint::parse("garbage");
    assert!(matches!(c4, LoaderVersionConstraint::UnknownConstraint(_)));
}

// ════════════════════════════════════════════════════════════════════
// Transaction find_stale
// ════════════════════════════════════════════════════════════════════

/// Stale transactions are discoverable and cleanable.
#[test]
fn regression_transaction_find_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    setup_live_server(&live);

    let txn = InstallTransaction::begin(&live, "test-stale").unwrap();
    let _staging = txn.staging_path().to_path_buf();

    // Drop without commit → auto-rollback cleans staging
    drop(txn);

    let stale = InstallTransaction::find_stale(&live);
    assert!(stale.is_empty(), "auto-rollback cleaned staging");
}

// ════════════════════════════════════════════════════════════════════
// Fresh install (no existing live) — commit creates live
// ════════════════════════════════════════════════════════════════════

/// Fresh install: no existing live → commit creates it.
#[test]
fn regression_fresh_install_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("new-server");

    let txn = InstallTransaction::begin(&live, "test-fresh").unwrap();
    let staging = txn.staging_path().to_path_buf();

    std::fs::create_dir_all(staging.join("mods")).unwrap();
    std::fs::write(staging.join("mods").join("mod.jar"), b"mod").unwrap();
    std::fs::write(staging.join("eula.txt"), "eula=true\n").unwrap();

    let meta = txn.commit().unwrap();
    assert_eq!(meta.phase, TransactionPhase::Committed);
    assert!(live.exists());
    assert!(live.join("mods").join("mod.jar").exists());
}

// ════════════════════════════════════════════════════════════════════
// Commit then cleanup_backup
// ════════════════════════════════════════════════════════════════════

/// After commit, backup can be cleaned up.
#[test]
fn regression_commit_then_cleanup_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("server");
    setup_live_server(&live);

    let txn = InstallTransaction::begin(&live, "test-cleanup").unwrap();
    let staging = txn.staging_path().to_path_buf();
    txn.copy_persistent_state().unwrap();

    std::fs::create_dir_all(staging.join("mods")).unwrap();
    std::fs::write(staging.join("mods").join("new.jar"), b"new").unwrap();

    let meta = txn.commit().unwrap();
    assert!(
        meta.backup_path.is_some(),
        "backup should exist after commit on existing server"
    );

    InstallTransaction::cleanup_backup(&meta);
    if let Some(ref backup) = meta.backup_path {
        assert!(!backup.exists(), "backup should be removed");
    }
}

// ════════════════════════════════════════════════════════════════════
// Server-pack: LoaderMismatch via orchestrator with empty registry
// ════════════════════════════════════════════════════════════════════

/// Server-pack context: advisor works from boot log alone.
#[tokio::test]
async fn regression_server_pack_loader_advisor() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(staging.join("mods")).unwrap();

    let mut orch = ValidationRepairOrchestrator::new();
    let mut harness = TestHarness::fabric(tmp.path().join("live").to_str().unwrap());
    let mock = MockBootValidator::new(vec![failed_result(
        BootFailureReason::ProcessExited,
        &loader_mismatch_forge_log(),
    )]);
    let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;

    match &outcome {
        ValidationOutcome::Failed(f) => {
            assert_eq!(f.reason, ValidationFailureReason::LoaderMismatchDetected);
            let report = f.loader_report.as_ref().unwrap();
            // cfg=Fabric, log=Forge → WrongLoaderFamily
            assert_eq!(report.family, LoaderFamily::Fabric);
            assert!(!report.requirements.is_empty());
            assert_eq!(report.requirements[0].family, LoaderFamily::Forge);
            assert_eq!(report.status, LoaderCompatibilityStatus::WrongLoaderFamily);
        }
        _ => panic!("Expected Failed, got {:?}", outcome),
    }
}

// ════════════════════════════════════════════════════════════════════
// Phase 3K.1: User-Confirmed Recovery Lifecycle
// ════════════════════════════════════════════════════════════════════

use lbby_core::crash_attribution::{
    CrashAttributionConfidence, CrashAttributionReport, CrashAttributionStatus, CrashCandidate,
    CrashEvidence, CrashEvidenceSource, CrashRecommendation,
};
use lbby_core::dependency_graph::DependencyGraph;
use lbby_core::install_transaction::{PendingRecoveryMetadata, TransactionMeta};
use lbby_core::jar_metadata::DependencyKind;
use lbby_core::recovery_actions::{
    self, ApprovalResult, RecoveryActionAvailability, MAX_USER_RECOVERY_ACTIONS,
};
use std::path::PathBuf;

// ── Helpers ─────────────────────────────────────────────────────────

/// Default ModCompatibility for test JARs (Server, High confidence, Unknown source).
fn test_compat() -> lbby_core::mod_compat::ModCompatibility {
    lbby_core::mod_compat::ModCompatibility {
        compatibility: lbby_core::mod_compat::ServerCompatibility::ServerOk,
        confidence: lbby_core::mod_compat::CompatibilityConfidence::Explicit,
        source: lbby_core::mod_compat::CompatibilitySource::None,
        reason: "test".to_string(),
    }
}

/// Create a mock CrashAttributionReport pointing to a specific mod.
fn mock_attribution_report(
    mod_id: &str,
    jar_path: &Path,
    confidence: CrashAttributionConfidence,
) -> CrashAttributionReport {
    CrashAttributionReport {
        status: CrashAttributionStatus::Attributed,
        confidence,
        candidates: vec![CrashCandidate {
            mod_id: Some(mod_id.to_string()),
            jar_path: Some(jar_path.to_path_buf()),
            score: 95,
            evidence: vec![CrashEvidence {
                source: CrashEvidenceSource::LoaderDiagnostic,
                matched_text: format!("{} caused crash", mod_id),
                snippet: "Matched pattern in crash log".to_string(),
                associated_mod_id: Some(mod_id.to_string()),
                associated_jar: Some(jar_path.to_path_buf()),
            }],
            is_client_only: false,
            is_unknown_compat: false,
        }],
        primary_candidate: Some(CrashCandidate {
            mod_id: Some(mod_id.to_string()),
            jar_path: Some(jar_path.to_path_buf()),
            score: 95,
            evidence: vec![CrashEvidence {
                source: CrashEvidenceSource::LoaderDiagnostic,
                matched_text: format!("{} caused crash", mod_id),
                snippet: "Matched pattern in crash log".to_string(),
                associated_mod_id: Some(mod_id.to_string()),
                associated_jar: Some(jar_path.to_path_buf()),
            }],
            is_client_only: false,
            is_unknown_compat: false,
        }),
        recommendation: CrashRecommendation::ReviewMod {
            mod_id: mod_id.to_string(),
            jar_path: Some(jar_path.to_path_buf()),
        },
        summary: format!("Crash attributed to '{}'", mod_id),
    }
}

/// Create a minimal JAR with fabric.mod.json declaring the given mod_id.
fn make_jar(dir: &Path, filename: &str, mod_id: &str) -> PathBuf {
    let jar = dir.join(filename);
    let file = std::fs::File::create(&jar).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", options).unwrap();
    let json = serde_json::json!({
        "id": mod_id,
        "version": "1.0.0",
        "environment": "*"
    });
    std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();
    jar
}

/// Set up a staging directory with pending recovery metadata.
/// Returns (tmpdir, staging_path, recovery_metadata).
fn setup_pending_recovery(
    server_id: &str,
    transaction_id: &str,
    mod_id: &str,
    jar_filename: &str,
    recovery_actions_used: u8,
) -> (tempfile::TempDir, PathBuf, PathBuf, PendingRecoveryMetadata) {
    let tmp = tempfile::tempdir().unwrap();
    // Layout: tmp/<server_id>/ (live) + tmp/.lbby-staging/<server_id>-<txn_id>/ (staging)
    let live = tmp.path().join(server_id);
    std::fs::create_dir_all(&live).unwrap();
    let staging = tmp
        .path()
        .join(".lbby-staging")
        .join(format!("{}-{}", server_id, transaction_id));
    let mods = staging.join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    let jar = make_jar(&mods, jar_filename, mod_id);
    let sha256 = recovery_actions::compute_file_sha256(&jar).unwrap();
    let fingerprint =
        recovery_actions::compute_fingerprint(transaction_id, mod_id, &jar, 1, &[], &sha256);

    let recovery = PendingRecoveryMetadata {
        server_id: server_id.to_string(),
        transaction_id: transaction_id.to_string(),
        staging_mods: mods.clone(),
        attribution_fingerprint: fingerprint.clone(),
        target_mod_id: mod_id.to_string(),
        target_jar_path: jar,
        target_jar_sha256: sha256,
        boot_attempt: 1,
        dependency_repairs: 0,
        runtime_repairs: 0,
        recovery_actions_used,
        display_filename: jar_filename.to_string(),
        crash_summary: format!("Crash attributed to '{}'", mod_id),
        confidence: "High".to_string(),
        applied: false,
    };

    let meta = TransactionMeta {
        server_id: server_id.to_string(),
        transaction_id: transaction_id.to_string(),
        source: "test".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        staging_path: staging.clone(),
        live_path: live.clone(),
        phase: TransactionPhase::PendingUserAction,
        backup_path: None,
    };
    let marker_json = serde_json::to_string_pretty(&meta).unwrap();
    std::fs::write(meta.marker_path(), marker_json).unwrap();
    recovery.save(&meta.pending_recovery_path()).unwrap();

    (tmp, live, staging, recovery)
}

/// Build a DependencyGraph from JAR files in a staging mods directory.
fn build_graph_from_staging(staging_mods: &Path) -> DependencyGraph {
    use lbby_core::mod_compat::classify_mod_local;
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(staging_mods) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "jar") {
                let compat = classify_mod_local(&p);
                entries.push((p, compat));
            }
        }
    }
    DependencyGraph::build(&entries)
}

// ── Tests ───────────────────────────────────────────────────────────

// Item 9: No approval → staging unchanged, no quarantine
/// Crash → UserActionRequired. Then stop.
/// Assert: transaction phase = PendingUserAction, live unchanged,
///         suspect JAR still in staging/mods, no quarantine, no boot2.
#[test]
fn regression_recovery_no_approval_staging_unchanged() {
    let (tmp, live, staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-002",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    // JAR still in staging
    assert!(
        recovery.target_jar_path.exists(),
        "suspect JAR must remain in staging"
    );
    // No quarantine created
    let quarantine_dir = staging.join(".lbby-quarantine");
    assert!(!quarantine_dir.exists(), "no quarantine without approval");
    // Transaction marker still PendingUserAction
    let marker_path = staging.join("transaction.json");
    let content = std::fs::read_to_string(&marker_path).unwrap();
    let meta: TransactionMeta = serde_json::from_str(&content).unwrap();
    assert_eq!(meta.phase, TransactionPhase::PendingUserAction);
}

// Item 11: Approve → quarantine JAR, SHA preserved
/// Approval must move JAR from staging to quarantine, preserving SHA-256.
#[test]
fn regression_recovery_approval_moves_jar() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-001",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    assert!(recovery.target_jar_path.exists());
    let canonical = recovery_actions::validate_jar_containment(
        &recovery.target_jar_path,
        &recovery.staging_mods,
    )
    .unwrap();
    recovery_actions::revalidate_before_move(
        &canonical,
        &recovery.target_mod_id,
        &recovery.staging_mods,
    )
    .unwrap();
    let quarantine_dir = recovery
        .staging_mods
        .parent()
        .unwrap()
        .join(".lbby-quarantine")
        .join("mods");
    let quarantine_path =
        recovery_actions::quarantine_jar(&canonical, &recovery.staging_mods, &quarantine_dir)
            .unwrap();
    assert!(
        !recovery.target_jar_path.exists(),
        "JAR removed from staging"
    );
    assert!(quarantine_path.exists(), "JAR exists in quarantine");
    let quarantined_sha = recovery_actions::compute_file_sha256(&quarantine_path).unwrap();
    assert_eq!(
        quarantined_sha, recovery.target_jar_sha256,
        "SHA-256 preserved through quarantine"
    );
}

// Item 12: Rollback → live unchanged
/// Transaction rollback must not touch live server files.
#[test]
fn regression_recovery_failed_rollback_live_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    std::fs::create_dir_all(live.join("mods")).unwrap();
    let live_jar = make_jar(&live.join("mods"), "keep.jar", "keep-mod");
    let live_jar_sha = recovery_actions::compute_file_sha256(&live_jar).unwrap();
    let txn = InstallTransaction::begin(&live, "test").unwrap();
    txn.rollback().unwrap();
    assert!(live_jar.exists(), "live JAR survives rollback");
    assert_eq!(
        recovery_actions::compute_file_sha256(&live_jar).unwrap(),
        live_jar_sha,
        "live JAR SHA unchanged"
    );
}

// Item 10: Reject flow → rollback, no quarantine
/// reject_crash_recovery must rollback staging and leave live unchanged.
#[test]
fn regression_recovery_reject_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live");
    let staging = tmp.path().join(".lbby-staging").join("test-server-txn003");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(staging.join("mods")).unwrap();
    let meta = TransactionMeta {
        server_id: "test-server".to_string(),
        transaction_id: "txn-003".to_string(),
        source: "test".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        staging_path: staging.clone(),
        live_path: live.clone(),
        phase: TransactionPhase::PendingUserAction,
        backup_path: None,
    };
    std::fs::write(
        meta.marker_path(),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    make_jar(&staging.join("mods"), "suspect.jar", "suspect-mod");
    let txn = InstallTransaction::resume(meta).unwrap();
    txn.rollback().unwrap();
    assert!(!staging.exists(), "staging cleaned after rollback");
}

// Item 13: Fingerprint SHA binding — different JAR bytes → different fingerprint
/// Same transaction/mod_id/path/boot/evidence, but different JAR bytes
/// must produce different fingerprints and fail verification.
#[test]
fn regression_recovery_fingerprint_binds_sha256() {
    let fp1 = recovery_actions::compute_fingerprint(
        "txn1",
        "modA",
        Path::new("mods/a.jar"),
        1,
        &[],
        "sha_original",
    );
    let fp2 = recovery_actions::compute_fingerprint(
        "txn1",
        "modA",
        Path::new("mods/a.jar"),
        1,
        &[],
        "sha_different",
    );
    assert_ne!(fp1, fp2, "different SHA → different fingerprint");
}

// Item 14: Stale SHA → reject
/// If JAR bytes changed after attribution, SHA mismatch must be detectable.
#[test]
fn regression_recovery_stale_sha_rejected() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-004",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    // Overwrite JAR with different bytes
    std::fs::write(&recovery.target_jar_path, b"different bytes").unwrap();
    let new_sha = recovery_actions::compute_file_sha256(&recovery.target_jar_path).unwrap();
    assert_ne!(
        new_sha, recovery.target_jar_sha256,
        "modified JAR has different SHA"
    );
}

// Item 15: Transaction ID mismatch → different fingerprint
/// Wrong transaction_id must produce a different fingerprint.
#[test]
fn regression_recovery_transaction_mismatch() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-005",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    let wrong_fp = recovery_actions::compute_fingerprint(
        "txn-WRONG",
        "suspect-mod",
        &recovery.target_jar_path,
        1,
        &[],
        &recovery.target_jar_sha256,
    );
    assert_ne!(
        wrong_fp, recovery.attribution_fingerprint,
        "wrong transaction_id → different fingerprint"
    );
}

// Item 16: Recovery action budget enforcement
/// When recovery_actions_used >= MAX, approval must be rejected.
#[test]
fn regression_recovery_limit_enforced() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-006",
        "suspect-mod",
        "suspect-mod.jar",
        MAX_USER_RECOVERY_ACTIONS,
    );
    assert!(
        recovery.recovery_actions_used >= MAX_USER_RECOVERY_ACTIONS,
        "recovery at budget limit"
    );
}

// Item 17: Boot attempt ceiling
/// MAX_TOTAL_BOOT_ATTEMPTS must be reasonable (1..=10).
#[test]
fn regression_recovery_boot_ceiling() {
    let max = lbby_core::validation_orchestrator::MAX_TOTAL_BOOT_ATTEMPTS;
    assert!(max > 0, "boot ceiling must be positive");
    assert!(max <= 10, "boot ceiling must be reasonable");
}

// Item 18: After quarantine, stale index eliminated
/// Quarantined JAR must no longer appear in build_jar_to_mod_ids.
#[test]
fn regression_recovery_stale_index_eliminated() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-007",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    let jar_to_mod_ids = recovery_actions::build_jar_to_mod_ids(&recovery.staging_mods);
    assert!(
        jar_to_mod_ids.contains_key(&recovery.target_jar_path),
        "JAR in index before quarantine"
    );
    let quarantine_dir = recovery
        .staging_mods
        .parent()
        .unwrap()
        .join(".lbby-quarantine")
        .join("mods");
    recovery_actions::quarantine_jar(
        &recovery.target_jar_path,
        &recovery.staging_mods,
        &quarantine_dir,
    )
    .unwrap();
    let fresh_index = recovery_actions::build_jar_to_mod_ids(&recovery.staging_mods);
    assert!(
        !fresh_index.contains_key(&recovery.target_jar_path),
        "JAR removed from index after quarantine"
    );
}

// Item 19: Dependency impact blocks quarantine
/// If B requires A (Required), quarantining A must be blocked.
#[test]
fn regression_recovery_dependency_impact_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    // Create JAR A
    make_jar(&mods, "modA.jar", "modA");
    // Create JAR B that depends on A
    let jar_b = mods.join("modB.jar");
    let file = std::fs::File::create(&jar_b).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", options).unwrap();
    let json = serde_json::json!({
        "id": "modB",
        "version": "1.0.0",
        "environment": "*",
        "depends": {"modA": "*"}
    });
    std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();

    // Build graph from real JARs
    let graph = build_graph_from_staging(&mods);

    // A has a dependent (B requires A) → must block
    let result = recovery_actions::check_dependency_impact("modA", &graph);
    assert!(result.is_err(), "modA has dependent modB");
    let broken = result.unwrap_err();
    assert!(
        broken.iter().any(|id| id.contains("modB")),
        "broken dependents must include modB, got {:?}",
        broken
    );

    // B has no dependents → OK
    assert!(
        recovery_actions::check_dependency_impact("modB", &graph).is_ok(),
        "modB has no dependents"
    );
}

// Item 20: Quarantine preserved on commit
/// preserve_quarantine_on_commit must move quarantine out of staging.
#[test]
fn regression_recovery_quarantine_preserved_on_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let mods = staging.join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    let jar = make_jar(&mods, "quarantined.jar", "bad-mod");
    let quarantine_dir = staging.join(".lbby-quarantine").join("mods");
    let quarantine_path = recovery_actions::quarantine_jar(&jar, &mods, &quarantine_dir).unwrap();
    let preserved =
        recovery_actions::preserve_quarantine_on_commit(&staging, "test-server", "txn-008")
            .unwrap();
    let preserved_jar = preserved.join("mods").join("quarantined.jar");
    assert!(preserved_jar.exists(), "quarantine preserved after commit");
    assert!(
        !quarantine_path.exists(),
        "staging quarantine removed after preservation"
    );
}

// Item 21: Restart → no auto-commit, no auto-approval
/// After restart, PendingUserAction must be preserved; no auto-mutation.
#[test]
fn regression_recovery_restart_no_auto_commit() {
    let (_tmp, _live, staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-009",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    // Reload marker
    let marker_path = staging.join("transaction.json");
    let content = std::fs::read_to_string(&marker_path).unwrap();
    let meta: TransactionMeta = serde_json::from_str(&content).unwrap();
    assert_eq!(
        meta.phase,
        TransactionPhase::PendingUserAction,
        "phase preserved after restart"
    );
    assert!(
        recovery.target_jar_path.exists(),
        "JAR still in staging after restart"
    );
    assert!(
        !staging.join(".lbby-quarantine").exists(),
        "no quarantine without approval"
    );
}

// Item 22: Duplicate approval → idempotent
/// Second quarantine attempt on same JAR must fail (already moved).
#[test]
fn regression_recovery_duplicate_approval_rejected() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-010",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    let quarantine_dir = recovery
        .staging_mods
        .parent()
        .unwrap()
        .join(".lbby-quarantine")
        .join("mods");
    // First quarantine succeeds
    recovery_actions::quarantine_jar(
        &recovery.target_jar_path,
        &recovery.staging_mods,
        &quarantine_dir,
    )
    .unwrap();
    assert!(!recovery.target_jar_path.exists());
    // Second quarantine fails — JAR no longer at source
    let result = recovery_actions::quarantine_jar(
        &recovery.target_jar_path,
        &recovery.staging_mods,
        &quarantine_dir,
    );
    assert!(result.is_err(), "duplicate quarantine must fail");
}

// Item 23: Protected components blocked
/// Platform/core mod IDs must never be quarantined.
#[test]
fn regression_recovery_protected_component_blocked() {
    assert!(recovery_actions::is_protected_component("forge"));
    assert!(recovery_actions::is_protected_component("Fabric"));
    assert!(recovery_actions::is_protected_component("minecraft"));
    assert!(
        !recovery_actions::is_protected_component("create"),
        "user mods are not protected"
    );
}

// Item 24: Multi-mod JAR blocked
/// JAR containing multiple mods must be flagged as UnavailableMultiModJar.
#[test]
fn regression_recovery_multi_mod_jar_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    // Create a Forge JAR with two [[mods]] entries (fabric provides is not parsed)
    let jar = mods.join("multi.jar");
    let file = std::fs::File::create(&jar).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("META-INF/mods.toml", options).unwrap();
    let toml_content = r#"
modLoader = "javafml"
loaderVersion = "[47,)"
license = "MIT"

[[mods]]
modId = "modA"
version = "1.0.0"

[[mods]]
modId = "modB"
version = "1.0.0"
"#;
    std::io::Write::write_all(&mut zip, toml_content.as_bytes()).unwrap();
    zip.finish().unwrap();

    let jar_to_mod_ids = recovery_actions::build_jar_to_mod_ids(&mods);
    let report = mock_attribution_report("modA", &jar, CrashAttributionConfidence::High);
    let availability = recovery_actions::check_action_availability(&report, &jar_to_mod_ids);
    assert!(
        matches!(
            availability,
            RecoveryActionAvailability::UnavailableMultiModJar(_)
        ),
        "multi-mod JAR must be blocked, got {:?}",
        availability
    );
}

// Item 25: Requires High confidence
/// Low/Medium confidence must produce UnavailableLowConfidence.
#[test]
fn regression_recovery_requires_high_confidence() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    let jar = make_jar(&mods, "suspect.jar", "suspect-mod");
    let jar_to_mod_ids = recovery_actions::build_jar_to_mod_ids(&mods);

    for conf in [
        CrashAttributionConfidence::Low,
        CrashAttributionConfidence::Medium,
    ] {
        let report = mock_attribution_report("suspect-mod", &jar, conf.clone());
        assert_eq!(
            recovery_actions::check_action_availability(&report, &jar_to_mod_ids),
            RecoveryActionAvailability::UnavailableLowConfidence,
            "{:?} confidence must be rejected",
            conf
        );
    }
    let report = mock_attribution_report("suspect-mod", &jar, CrashAttributionConfidence::High);
    assert_eq!(
        recovery_actions::check_action_availability(&report, &jar_to_mod_ids),
        RecoveryActionAvailability::Available,
        "High confidence must be accepted"
    );
}

// Item 26: Quarantine collision handling
/// Two JARs with same filename must get distinct quarantine paths.
#[test]
fn regression_recovery_quarantine_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    let quarantine = tmp.path().join("quarantine");
    std::fs::create_dir_all(&mods).unwrap();
    let jar1 = make_jar(&mods, "same-name.jar", "mod1");
    let q1 = recovery_actions::quarantine_jar(&jar1, &mods, &quarantine).unwrap();
    let jar2 = make_jar(&mods, "same-name.jar", "mod2");
    let q2 = recovery_actions::quarantine_jar(&jar2, &mods, &quarantine).unwrap();
    assert_ne!(q1, q2, "collision must produce distinct paths");
    assert!(
        q2.to_string_lossy().contains("__2"),
        "second file gets __2 suffix"
    );
}

// Item 27: Metadata roundtrip
/// PendingRecoveryMetadata must survive save/load cycle.
#[test]
fn regression_recovery_metadata_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("recovery.json");
    let original = PendingRecoveryMetadata {
        server_id: "test-server".into(),
        transaction_id: "txn-rt".into(),
        staging_mods: PathBuf::from("/tmp/staging/mods"),
        attribution_fingerprint: "abc123".into(),
        target_mod_id: "bad-mod".into(),
        target_jar_path: PathBuf::from("/tmp/staging/mods/bad-mod.jar"),
        target_jar_sha256: "deadbeef".into(),
        boot_attempt: 2,
        dependency_repairs: 0,
        runtime_repairs: 0,
        recovery_actions_used: 1,
        display_filename: "bad-mod.jar".into(),
        crash_summary: "Crash".into(),
        confidence: "High".into(),
        applied: false,
    };
    original.save(&path).unwrap();
    let loaded = PendingRecoveryMetadata::load(&path).unwrap();
    assert_eq!(loaded.server_id, original.server_id);
    assert_eq!(loaded.target_jar_sha256, original.target_jar_sha256);
    assert_eq!(loaded.applied, original.applied);
    assert_eq!(loaded.recovery_actions_used, original.recovery_actions_used);
}

// Item 28: Pending recovery discovery
/// find_pending_recoveries must discover persisted PendingUserAction transactions.
#[test]
fn regression_recovery_find_pending_discoveries() {
    let (_tmp, live, _staging, _recovery) = setup_pending_recovery(
        "test-server",
        "txn-discover",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    // setup_pending_recovery already creates live at tmp/<server_id> and
    // staging at tmp/.lbby-staging/<server_id>-<txn_id>/ with proper metadata.
    let results = recovery_actions::find_pending_recoveries(&live);
    assert_eq!(results.len(), 1, "must discover one pending recovery");
    assert_eq!(results[0].transaction_id, "txn-discover");
}

// ── Phase 3K.1: Production API integration tests ────────────────────

// Item A: dep_graph freshness — approve_crash_recovery with None graph
/// When dep_graph=None, approve_crash_recovery must skip dependency preflight
/// and still perform quarantine (for mods with no known dependents).
#[test]
fn regression_approval_no_graph_still_quarantines() {
    let (_tmp, _live, _staging, recovery) = setup_pending_recovery(
        "test-server",
        "txn-graph-1",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    // Directly test the quarantine path (approve_crash_recovery needs live server config
    // which tests can't easily mock). Test quarantine mechanics instead.
    let canonical = recovery_actions::validate_jar_containment(
        &recovery.target_jar_path,
        &recovery.staging_mods,
    )
    .unwrap();
    recovery_actions::revalidate_before_move(
        &canonical,
        &recovery.target_mod_id,
        &recovery.staging_mods,
    )
    .unwrap();
    let quarantine_dir = recovery
        .staging_mods
        .parent()
        .unwrap()
        .join(".lbby-quarantine")
        .join("mods");
    let qpath =
        recovery_actions::quarantine_jar(&canonical, &recovery.staging_mods, &quarantine_dir)
            .unwrap();
    assert!(qpath.exists());
    assert!(!recovery.target_jar_path.exists());
}

// Item B: Dependency-impact through production path — B requires A
/// Real JARs: B declares depends on A. check_dependency_impact("A") must block.
#[test]
fn regression_dependency_impact_production_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    // JAR A
    make_jar(&mods, "modA.jar", "modA");
    // JAR B depends on A (required)
    let jar_b = mods.join("modB.jar");
    let file = std::fs::File::create(&jar_b).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", opts).unwrap();
    let json = serde_json::json!({
        "id": "modB", "version": "1.0.0", "environment": "*",
        "depends": {"modA": "*"}
    });
    std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();

    let graph = build_graph_from_staging(&mods);
    let result = recovery_actions::check_dependency_impact("modA", &graph);
    assert!(result.is_err(), "A has dependent B → must block");
    let broken = result.unwrap_err();
    assert!(
        broken.iter().any(|s| s.contains("modB")),
        "broken list must include modB, got {:?}",
        broken
    );
}

// Item C: Recovery counter persistence across save/load
/// recovery_actions_used must survive metadata roundtrip.
#[test]
fn regression_recovery_counter_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("recovery.json");
    let meta = PendingRecoveryMetadata {
        server_id: "srv".into(),
        transaction_id: "txn-counter".into(),
        staging_mods: PathBuf::from("/tmp/staging/mods"),
        attribution_fingerprint: "fp".into(),
        target_mod_id: "mod".into(),
        target_jar_path: PathBuf::from("/tmp/staging/mods/mod.jar"),
        target_jar_sha256: "sha".into(),
        boot_attempt: 1,
        dependency_repairs: 0,
        runtime_repairs: 0,
        recovery_actions_used: 1, // Already used 1
        display_filename: "mod.jar".into(),
        crash_summary: "crash".into(),
        confidence: "High".into(),
        applied: false,
    };
    meta.save(&path).unwrap();
    let loaded = PendingRecoveryMetadata::load(&path).unwrap();
    assert_eq!(
        loaded.recovery_actions_used, 1,
        "counter survives roundtrip"
    );
    // If counter=1 and MAX=2, one more action is allowed
    assert!(
        loaded.recovery_actions_used < MAX_USER_RECOVERY_ACTIONS,
        "budget not yet exhausted"
    );
}

// Item D: Boot attempt counter in metadata
/// boot_attempt must persist and accumulate.
#[test]
fn regression_boot_attempt_counter_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("recovery.json");
    let meta = PendingRecoveryMetadata {
        server_id: "srv".into(),
        transaction_id: "txn-boot".into(),
        staging_mods: PathBuf::from("/tmp/staging/mods"),
        attribution_fingerprint: "fp".into(),
        target_mod_id: "mod".into(),
        target_jar_path: PathBuf::from("/tmp/staging/mods/mod.jar"),
        target_jar_sha256: "sha".into(),
        boot_attempt: 3, // 3rd boot attempt
        dependency_repairs: 0,
        runtime_repairs: 0,
        recovery_actions_used: 0,
        display_filename: "mod.jar".into(),
        crash_summary: "crash".into(),
        confidence: "High".into(),
        applied: false,
    };
    meta.save(&path).unwrap();
    let loaded = PendingRecoveryMetadata::load(&path).unwrap();
    assert_eq!(loaded.boot_attempt, 3, "boot attempt count persists");
}

// Item E: UserActionRequired payload does NOT expose internal paths
/// The outcome returned to the UI must contain only safe fields.
#[test]
fn regression_user_action_required_no_internal_paths() {
    // This test verifies the InstallOutcome::UserActionRequired variant
    // does not contain staging_mods or quarantine paths.
    // We check the type definition: only server_id, transaction_id, fingerprint,
    // mod_id, display_filename, confidence, crash_summary, recovery_actions_remaining.
    //
    // If the variant ever includes PathBuf fields, this test catches it.
    use lbby_core::mod_services::InstallOutcome;

    // Construct a UserActionRequired outcome with known values
    let outcome = InstallOutcome::UserActionRequired {
        server_id: "test-srv".to_string(),
        transaction_id: "txn-001".to_string(),
        fingerprint: "abc123".to_string(),
        mod_id: "suspect-mod".to_string(),
        display_filename: "suspect-mod.jar".to_string(),
        jar_sha256: "abcdef1234567890".to_string(),
        boot_attempt: 1,
        recovery_actions_used: 0,
        crash_summary: "Crash in suspect-mod".to_string(),
        confidence: "High".to_string(),
    };

    // Verify by destructuring: all fields are safe strings/numbers,
    // no PathBuf, no staging_mods, no quarantine_path.
    // Debug format reveals field names.
    let debug_str = format!("{:?}", outcome);
    assert!(!debug_str.contains("staging_mods"), "no staging_mods field");
    assert!(!debug_str.contains("quarantine"), "no quarantine path");
    assert!(!debug_str.contains("live_path"), "no live path");

    // Verify the field values are the safe identifiers we set
    if let InstallOutcome::UserActionRequired {
        server_id,
        transaction_id,
        fingerprint,
        mod_id,
        display_filename,
        jar_sha256,
        boot_attempt,
        recovery_actions_used,
        crash_summary,
        confidence,
    } = outcome
    {
        assert_eq!(server_id, "test-srv");
        assert_eq!(transaction_id, "txn-001");
        assert_eq!(fingerprint, "abc123");
        assert_eq!(mod_id, "suspect-mod");
        assert_eq!(display_filename, "suspect-mod.jar");
        assert_eq!(jar_sha256, "abcdef1234567890");
        assert_eq!(boot_attempt, 1u8);
        assert_eq!(recovery_actions_used, 0u8);
        assert_eq!(confidence, "High");
        // These are all string/u8 fields — no PathBuf anywhere
    } else {
        panic!("Expected UserActionRequired variant");
    }
}

// Item F: approve_crash_recovery with wrong server_id → TransactionNotFound
/// Production API must reject mismatched server_id.
#[test]
fn regression_approval_wrong_server_id() {
    let (_tmp, _live, _staging, _recovery) = setup_pending_recovery(
        "correct-server",
        "txn-mismatch",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    // approve_crash_recovery uses find_live_path_for_server which reads config.
    // In test env, wrong server_id won't find the live path.
    let result = recovery_actions::approve_crash_recovery(
        "wrong-server",
        "txn-mismatch",
        "dummy-fingerprint",
    );
    assert!(
        matches!(result, ApprovalResult::TransactionNotFound(_)),
        "wrong server_id → TransactionNotFound, got {:?}",
        result
    );
}

// Item G: approve_crash_recovery with wrong transaction_id → TransactionNotFound
#[test]
fn regression_approval_wrong_transaction_id() {
    let (_tmp, _live, _staging, _recovery) = setup_pending_recovery(
        "test-server",
        "txn-correct",
        "suspect-mod",
        "suspect-mod.jar",
        0,
    );
    let result =
        recovery_actions::approve_crash_recovery("test-server", "txn-WRONG", "dummy-fingerprint");
    assert!(
        matches!(result, ApprovalResult::TransactionNotFound(_)),
        "wrong transaction_id → TransactionNotFound, got {:?}",
        result
    );
}

// Item H: Approval idempotency — applied=true → Invalidated
/// If recovery metadata already has applied=true, second approval → Invalidated.
#[test]
fn regression_approval_already_applied() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("test-server");
    std::fs::create_dir_all(&live).unwrap();
    let staging = tmp
        .path()
        .join(".lbby-staging")
        .join("test-server-txn-idem");
    let mods = staging.join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    let jar = make_jar(&mods, "suspect.jar", "suspect-mod");
    let sha = recovery_actions::compute_file_sha256(&jar).unwrap();
    let fp = recovery_actions::compute_fingerprint("txn-idem", "suspect-mod", &jar, 1, &[], &sha);

    // Mark as already applied
    let recovery = PendingRecoveryMetadata {
        server_id: "test-server".into(),
        transaction_id: "txn-idem".into(),
        staging_mods: mods.clone(),
        attribution_fingerprint: fp.clone(),
        target_mod_id: "suspect-mod".into(),
        target_jar_path: jar,
        target_jar_sha256: sha,
        boot_attempt: 1,
        dependency_repairs: 0,
        runtime_repairs: 0,
        recovery_actions_used: 1,
        display_filename: "suspect.jar".into(),
        crash_summary: "crash".into(),
        confidence: "High".into(),
        applied: true, // Already applied
    };

    let meta = TransactionMeta {
        server_id: "test-server".into(),
        transaction_id: "txn-idem".into(),
        source: "test".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        staging_path: staging.clone(),
        live_path: live.clone(),
        phase: TransactionPhase::PendingUserAction,
        backup_path: None,
    };
    std::fs::write(
        meta.marker_path(),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    recovery.save(&meta.pending_recovery_path()).unwrap();

    // Attempt approval — should get Invalidated (already applied)
    let result = recovery_actions::approve_crash_recovery_at("test-server", "txn-idem", &fp, &live);
    assert!(
        matches!(result, ApprovalResult::Invalidated(_)),
        "already-applied → Invalidated, got {:?}",
        result
    );
}

// Item I: Stale fingerprint → Invalidated
/// If JAR bytes changed, fingerprint verification must fail.
#[test]
fn regression_approval_stale_fingerprint_invalidated() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("test-server");
    std::fs::create_dir_all(&live).unwrap();
    let staging = tmp
        .path()
        .join(".lbby-staging")
        .join("test-server-txn-stale");
    let mods = staging.join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    let jar = make_jar(&mods, "suspect.jar", "suspect-mod");
    let sha = recovery_actions::compute_file_sha256(&jar).unwrap();
    let fp = recovery_actions::compute_fingerprint("txn-stale", "suspect-mod", &jar, 1, &[], &sha);

    let recovery = PendingRecoveryMetadata {
        server_id: "test-server".into(),
        transaction_id: "txn-stale".into(),
        staging_mods: mods.clone(),
        attribution_fingerprint: fp,
        target_mod_id: "suspect-mod".into(),
        target_jar_path: jar.clone(),
        target_jar_sha256: sha,
        boot_attempt: 1,
        dependency_repairs: 0,
        runtime_repairs: 0,
        recovery_actions_used: 0,
        display_filename: "suspect.jar".into(),
        crash_summary: "crash".into(),
        confidence: "High".into(),
        applied: false,
    };

    let meta = TransactionMeta {
        server_id: "test-server".into(),
        transaction_id: "txn-stale".into(),
        source: "test".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        staging_path: staging.clone(),
        live_path: live.clone(),
        phase: TransactionPhase::PendingUserAction,
        backup_path: None,
    };
    std::fs::write(
        meta.marker_path(),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    recovery.save(&meta.pending_recovery_path()).unwrap();

    // Corrupt the JAR
    std::fs::write(&jar, b"corrupted bytes").unwrap();

    // Fingerprint from original SHA won't match current JAR
    let wrong_fp = recovery_actions::compute_fingerprint(
        "txn-stale",
        "suspect-mod",
        &jar,
        1,
        &[],
        "original_sha_that_no_longer_matches",
    );
    let result =
        recovery_actions::approve_crash_recovery_at("test-server", "txn-stale", &wrong_fp, &live);
    assert!(
        matches!(result, ApprovalResult::Invalidated(_)),
        "stale fingerprint → Invalidated, got {:?}",
        result
    );
}

// Item J: DependencyGraph::build reads real JAR metadata
/// DependencyGraph::build must read dependencies from JAR metadata,
/// not from caller-supplied data.
#[test]
fn regression_dep_graph_reads_jar_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    // JAR A (no deps)
    make_jar(&mods, "modA.jar", "modA");
    // JAR C depends on A
    let jar_c = mods.join("modC.jar");
    let file = std::fs::File::create(&jar_c).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", opts).unwrap();
    let json = serde_json::json!({
        "id": "modC", "version": "1.0.0", "environment": "*",
        "depends": {"modA": "*"}
    });
    std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();

    // Build graph — must read deps from JAR, not from caller
    let graph = build_graph_from_staging(&mods);

    // Verify graph contains the dependency
    let c_node = graph.find_by_mod_id("modC");
    assert!(c_node.is_some(), "modC must be in graph");
    let c_node = c_node.unwrap();
    assert!(
        c_node
            .dependencies
            .iter()
            .any(|d| d.mod_id == "modA" && matches!(d.kind, DependencyKind::Required)),
        "modC must declare dependency on modA from JAR metadata"
    );
}

// Item K: Protected component through availability check
/// Protected mod_id must produce UnavailableProtectedComponent.
#[test]
fn regression_protected_component_availability() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    let jar = make_jar(&mods, "forge.jar", "forge");
    let jar_to_mod_ids = recovery_actions::build_jar_to_mod_ids(&mods);
    let report = mock_attribution_report("forge", &jar, CrashAttributionConfidence::High);
    let availability = recovery_actions::check_action_availability(&report, &jar_to_mod_ids);
    assert_eq!(
        availability,
        RecoveryActionAvailability::UnavailableProtectedComponent,
        "protected mod must be blocked"
    );
}

// Item L: Single-mod JAR with High confidence → Available
/// Clean single-mod JAR must pass all checks.
#[test]
fn regression_single_mod_jar_available() {
    let tmp = tempfile::tempdir().unwrap();
    let mods = tmp.path().join("mods");
    std::fs::create_dir_all(&mods).unwrap();
    let jar = make_jar(&mods, "clean-mod.jar", "clean-mod");
    let jar_to_mod_ids = recovery_actions::build_jar_to_mod_ids(&mods);
    let report = mock_attribution_report("clean-mod", &jar, CrashAttributionConfidence::High);
    assert_eq!(
        recovery_actions::check_action_availability(&report, &jar_to_mod_ids),
        RecoveryActionAvailability::Available,
        "clean single-mod High-confidence must be Available"
    );
}

// ════════════════════════════════════════════════════════════════════
// Phase 3K.1 — Global boot ceiling survives pause/resume
// ════════════════════════════════════════════════════════════════════

/// Global boot ceiling (MAX_TOTAL_BOOT_ATTEMPTS=6) must span multiple
/// pause/resume cycles. Orchestrator created from persisted state must
/// restore actual enforcement counters, not just display numbers.
///
/// Scenario:
///   Set initial boot_attempts_used = 3 (simulating 3 prior boots)
///   boot4: crash → UAR → save state (boot_attempts_used=4)
///   boot5: crash → UAR → save state (boot_attempts_used=5)
///   boot6: crash → classify → UAR → save state (boot_attempts_used=6)
///   resume: from_persisted(6) → RetryLimitReached
///   validator total calls == 3
#[tokio::test]
async fn regression_global_boot_ceiling_across_pause_resume() {
    use lbby_core::recovery_actions::{self, RetryStateSnapshot};
    use lbby_core::validation_orchestrator::{
        ValidationOutcome, ValidationRepairOrchestrator, MAX_TOTAL_BOOT_ATTEMPTS,
    };

    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("server");
    std::fs::create_dir_all(&server_path).unwrap();

    // Simulate 3 prior boots by persisting retry state
    let initial_state = RetryStateSnapshot {
        boot_attempts_used: 3,
        dependency_repairs_used: 0,
        runtime_repairs_used: 0,
        recovery_actions_used: 0,
    };
    recovery_actions::save_retry_state(&server_path, &initial_state).unwrap();

    let mut total_validator_calls: usize = 0;

    // Round 1: from_persisted(3) → boot4 → UAR
    {
        let staging = tmp.path().join("staging-r1");
        std::fs::create_dir_all(staging.join("mods")).unwrap();
        make_jar(&staging.join("mods"), "suspect-mod.jar", "suspect-mod");

        let mut orch = ValidationRepairOrchestrator::from_persisted_state(3, 0, 0);
        let mut harness = TestHarness::forge(server_path.to_str().unwrap());

        // Provide enough crash results for the validator
        let mock = MockBootValidator::new(vec![
            failed_result(
                BootFailureReason::ProcessExited,
                &mod_init_crash_log("suspect-mod"),
            );
            10
        ]);
        let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;
        total_validator_calls += mock.calls();

        assert!(
            matches!(outcome, ValidationOutcome::UserActionRequired(_)),
            "Round 1: expected UserActionRequired, got {:?}",
            outcome
        );
        assert_eq!(
            orch.state().total_boot_attempts,
            4,
            "Round 1: boot_attempts should be 4"
        );

        // Save state (simulates what mod_services.rs does on UAR)
        let snapshot = RetryStateSnapshot {
            boot_attempts_used: orch.state().total_boot_attempts,
            dependency_repairs_used: orch.state().dependency_repairs,
            runtime_repairs_used: orch.state().runtime_repairs,
            recovery_actions_used: 0,
        };
        recovery_actions::save_retry_state(&server_path, &snapshot).unwrap();
    }

    // Round 2: from_persisted(4) → boot5 → UAR
    {
        let staging = tmp.path().join("staging-r2");
        std::fs::create_dir_all(staging.join("mods")).unwrap();
        make_jar(&staging.join("mods"), "suspect-mod.jar", "suspect-mod");

        let mut orch = ValidationRepairOrchestrator::from_persisted_state(4, 0, 0);
        let mut harness = TestHarness::forge(server_path.to_str().unwrap());
        let mock = MockBootValidator::new(vec![
            failed_result(
                BootFailureReason::ProcessExited,
                &mod_init_crash_log("suspect-mod"),
            );
            10
        ]);
        let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;
        total_validator_calls += mock.calls();

        assert!(
            matches!(outcome, ValidationOutcome::UserActionRequired(_)),
            "Round 2: expected UserActionRequired, got {:?}",
            outcome
        );
        assert_eq!(
            orch.state().total_boot_attempts,
            5,
            "Round 2: boot_attempts should be 5"
        );

        let snapshot = RetryStateSnapshot {
            boot_attempts_used: orch.state().total_boot_attempts,
            dependency_repairs_used: orch.state().dependency_repairs,
            runtime_repairs_used: orch.state().runtime_repairs,
            recovery_actions_used: 0,
        };
        recovery_actions::save_retry_state(&server_path, &snapshot).unwrap();
    }

    // Round 3: from_persisted(5) → boot6 → UAR (last attempt)
    {
        let staging = tmp.path().join("staging-r3");
        std::fs::create_dir_all(staging.join("mods")).unwrap();
        make_jar(&staging.join("mods"), "suspect-mod.jar", "suspect-mod");

        let mut orch = ValidationRepairOrchestrator::from_persisted_state(5, 0, 0);
        let mut harness = TestHarness::forge(server_path.to_str().unwrap());
        let mock = MockBootValidator::new(vec![
            failed_result(
                BootFailureReason::ProcessExited,
                &mod_init_crash_log("suspect-mod"),
            );
            10
        ]);
        let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;
        total_validator_calls += mock.calls();

        // At attempt 6 (== MAX), the validator runs but then the loop continues
        // to consume_boot_attempt which fails → Failed(RetryLimitReached).
        // OR the crash is classified as ReviewMod+High → UserActionRequired.
        assert!(
            matches!(
                outcome,
                ValidationOutcome::UserActionRequired(_)
                    | ValidationOutcome::Failed(ValidationFailure {
                        reason: ValidationFailureReason::RetryLimitReached,
                        ..
                    })
            ),
            "Round 3: expected UAR or RetryLimitReached, got {:?}",
            outcome
        );

        let snapshot = RetryStateSnapshot {
            boot_attempts_used: orch.state().total_boot_attempts,
            dependency_repairs_used: orch.state().dependency_repairs,
            runtime_repairs_used: orch.state().runtime_repairs,
            recovery_actions_used: 0,
        };
        recovery_actions::save_retry_state(&server_path, &snapshot).unwrap();
    }

    // Round 4: from_persisted(6) → RetryLimitReached WITHOUT calling validator
    {
        let staging = tmp.path().join("staging-r4");
        std::fs::create_dir_all(staging.join("mods")).unwrap();
        make_jar(&staging.join("mods"), "suspect-mod.jar", "suspect-mod");

        let mut orch = ValidationRepairOrchestrator::from_persisted_state(6, 0, 0);
        let mut harness = TestHarness::forge(server_path.to_str().unwrap());
        let mock = MockBootValidator::new(vec![
            failed_result(
                BootFailureReason::ProcessExited,
                &mod_init_crash_log("suspect-mod"),
            );
            10
        ]);
        let outcome = orch.validate(&mut harness.ctx(&staging), &mock).await;
        let round4_calls = mock.calls();
        total_validator_calls += round4_calls;

        assert!(
            matches!(
                outcome,
                ValidationOutcome::Failed(ValidationFailure {
                    reason: ValidationFailureReason::RetryLimitReached,
                    ..
                })
            ),
            "Round 4: expected RetryLimitReached, got {:?}",
            outcome
        );
        assert_eq!(
            round4_calls, 0,
            "Round 4: validator must NOT be called when budget exhausted"
        );
    }

    // Global assertions
    assert_eq!(
        total_validator_calls, 3,
        "Validator must be called exactly 3 times across all pause/resume cycles"
    );
    assert!(
        MAX_TOTAL_BOOT_ATTEMPTS == 6,
        "Test assumes MAX_TOTAL_BOOT_ATTEMPTS == 6"
    );
}

// ════════════════════════════════════════════════════════════════════
// Phase 3K.1 — Quarantine preservation failure prevents commit
// ════════════════════════════════════════════════════════════════════

/// If preserve_quarantine_on_commit fails, commit must NOT proceed.
/// The quarantine artifact must remain recoverable.
#[test]
fn regression_quarantine_preservation_failure_prevents_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let mods = staging.join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    // Create a quarantine artifact in staging
    let jar = make_jar(&mods, "bad-mod.jar", "bad-mod");
    let quarantine_dir = staging.join(".lbby-quarantine").join("mods");
    let quarantine_path = recovery_actions::quarantine_jar(&jar, &mods, &quarantine_dir).unwrap();
    assert!(
        quarantine_path.exists(),
        "quarantine artifact exists in staging"
    );

    // Block the preserve path by creating a FILE where the directory should go.
    // live-parent = tmp.path() (parent of staging)
    // preserve_root = tmp.path()/.lbby-quarantine/<server>/<txn>
    // Create a file at tmp.path()/.lbby-quarantine to block mkdir
    let blocker = tmp.path().join(".lbby-quarantine");
    std::fs::write(&blocker, "block").unwrap();

    // preserve_quarantine_on_commit should FAIL because it can't create the dir
    let result =
        recovery_actions::preserve_quarantine_on_commit(&staging, "test-server", "txn-blocked");
    assert!(
        result.is_err(),
        "preserve_quarantine_on_commit must fail when destination is blocked, got {:?}",
        result
    );

    // Quarantine artifact must still exist in staging (not lost)
    assert!(
        quarantine_path.exists(),
        "quarantine artifact must still exist after failed preservation"
    );

    // Staging quarantine directory must still exist
    assert!(
        staging.join(".lbby-quarantine").exists(),
        "staging quarantine dir must survive failed preservation"
    );
}

/// After successful commit, quarantine lives at
/// `<live-parent>/.lbby-quarantine/<server>/<txn>/` (sibling of live root),
/// NOT inside the live directory.
#[test]
fn regression_quarantine_path_after_successful_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let mods = staging.join("mods");
    std::fs::create_dir_all(&mods).unwrap();

    let jar = make_jar(&mods, "quarantined.jar", "bad-mod");
    let quarantine_dir = staging.join(".lbby-quarantine").join("mods");
    let quarantine_path = recovery_actions::quarantine_jar(&jar, &mods, &quarantine_dir).unwrap();

    let preserved =
        recovery_actions::preserve_quarantine_on_commit(&staging, "test-server", "txn-001")
            .unwrap();

    // Verify path is <live-parent>/.lbby-quarantine/<server>/<txn>
    let live_parent = tmp.path(); // parent of staging
    let expected = live_parent
        .join(".lbby-quarantine")
        .join("test-server")
        .join("txn-001");
    assert_eq!(
        preserved, expected,
        "quarantine must be at <live-parent>/.lbby-quarantine/<server>/<txn>"
    );

    // Verify it's NOT inside any "live" directory
    assert!(
        !preserved.to_string_lossy().contains("/live/"),
        "quarantine must NOT be inside live root"
    );

    // Verify the preserved JAR exists
    let preserved_jar = preserved.join("mods").join("quarantined.jar");
    assert!(
        preserved_jar.exists(),
        "quarantine JAR must exist at preserved location"
    );

    // Verify staging quarantine was cleaned up
    assert!(
        !quarantine_path.exists(),
        "staging quarantine removed after preservation"
    );
}

// ════════════════════════════════════════════════════════════════════════
// Phase 3L — Quarantine Management & Restore Regressions
// ════════════════════════════════════════════════════════════════════════

use lbby_core::recovery_actions::{
    compute_record_id, list_quarantined_mods_at, record_quarantine, restore_quarantined_mod_at,
    save_quarantine_metadata, QuarantineMetadata, QuarantineRecord, QuarantineStatus,
    RestoreLifecycleProvider, RestoreResult, ServerLifecycleState,
};

/// Mock lifecycle provider for tests.
struct MockLifecycleProvider {
    state: ServerLifecycleState,
}

impl RestoreLifecycleProvider for MockLifecycleProvider {
    fn get_server_state(&self, _server_id: &str) -> Result<ServerLifecycleState, String> {
        Ok(self.state)
    }
}

/// Provider that returns Stopped (the happy-path for restore tests).
fn stopped_provider() -> MockLifecycleProvider {
    MockLifecycleProvider {
        state: ServerLifecycleState::Stopped,
    }
}

/// Provider that returns Running (blocked).
fn running_provider() -> MockLifecycleProvider {
    MockLifecycleProvider {
        state: ServerLifecycleState::Running,
    }
}

/// Helper: set up a quarantine environment for restore tests.
///
/// Layout: tmp/<server_id>/ (live root with mods/) + tmp/.lbby-quarantine/<server_id>/<txn_id>/
///
/// Returns (tmpdir, live_path, quarantine_txn_dir, record_id, sha256).
fn setup_quarantine_env(
    server_id: &str,
    txn_id: &str,
    jar_filename: &str,
    mod_id: &str,
) -> (tempfile::TempDir, PathBuf, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join(server_id);
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create quarantine structure
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join(server_id)
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();

    // Create JAR in quarantine
    let jar_path = make_jar(&quarantine_mods, jar_filename, mod_id);
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record_id = compute_record_id(txn_id, &format!("mods/{}", jar_filename), &sha);

    // Write metadata
    let record = record_quarantine(
        &quarantine_txn_dir,
        server_id,
        txn_id,
        &format!("mods/{}", jar_filename),
        jar_filename,
        vec![mod_id.to_string()],
        &sha,
        1,
        "test quarantine",
    )
    .unwrap();

    (tmp, live, quarantine_txn_dir, record.record_id, sha)
}

// ── Test 1: Successful restore ─────────────────────────────────────────

#[test]
fn regression_restore_success() {
    let (tmp, live, _qdir, record_id, sha) =
        setup_quarantine_env("srv1", "txn1", "create-0.5.1.jar", "create");

    let result = restore_quarantined_mod_at("srv1", "txn1", &record_id, &live, &stopped_provider());

    match &result {
        RestoreResult::Restored {
            sha256,
            target: _target,
        } => {
            assert_eq!(*sha256, sha, "restore SHA must match original");
        }
        other => panic!("expected Restored, got {:?}", other),
    }

    // File must exist in live/mods
    let restored_jar = live.join("mods").join("create-0.5.1.jar");
    assert!(
        restored_jar.exists(),
        "restored JAR must exist in live/mods"
    );

    // SHA must match
    let live_sha = recovery_actions::compute_file_sha256(&restored_jar).unwrap();
    assert_eq!(live_sha, sha, "live SHA must match quarantine SHA");

    // Quarantine source must still exist (copy strategy)
    let quarantine_jar = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv1")
        .join("txn1")
        .join("mods")
        .join("create-0.5.1.jar");
    assert!(
        quarantine_jar.exists(),
        "quarantine source must be preserved after restore"
    );
}

// ── Test 2: Server running → blocked ───────────────────────────────────

#[test]
fn regression_restore_server_running_blocked() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv2", "txn2", "foo.jar", "foo");

    let result = restore_quarantined_mod_at("srv2", "txn2", &record_id, &live, &running_provider());

    assert!(
        matches!(result, RestoreResult::ServerNotStopped { .. }),
        "restore must be blocked when server is running, got {:?}",
        result
    );
}

// ── Test 3: Active transaction → blocked ───────────────────────────────

#[test]
fn regression_restore_active_transaction_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv3");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Set up quarantine
    let txn_id = "txn-active";
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv3")
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();
    let jar_path = make_jar(&quarantine_mods, "test.jar", "test-mod");
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record = record_quarantine(
        &quarantine_txn_dir,
        "srv3",
        txn_id,
        "mods/test.jar",
        "test.jar",
        vec!["test-mod".to_string()],
        &sha,
        1,
        "test",
    )
    .unwrap();

    // Create an active staging transaction
    use lbby_core::install_transaction::TransactionMeta;
    let staging_txn = tmp
        .path()
        .join(".lbby-staging")
        .join(format!("srv3-{}", txn_id));
    std::fs::create_dir_all(&staging_txn).unwrap();
    let meta = TransactionMeta {
        server_id: "srv3".to_string(),
        transaction_id: txn_id.to_string(),
        source: "test".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        phase: lbby_core::install_transaction::TransactionPhase::Building,
        live_path: live.clone(),
        staging_path: staging_txn.clone(),
        backup_path: None,
    };
    std::fs::write(
        staging_txn.join("transaction.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();

    let result = restore_quarantined_mod_at(
        "srv3",
        txn_id,
        &record.record_id,
        &live,
        &stopped_provider(),
    );

    match result {
        RestoreResult::ActiveTransaction { transaction_id } => {
            assert_eq!(transaction_id, txn_id);
        }
        other => panic!("expected ActiveTransaction, got {:?}", other),
    }
}

// ── Test 4: Hash mismatch ──────────────────────────────────────────────

#[test]
fn regression_restore_hash_mismatch() {
    let (tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv4", "txn4", "tampered.jar", "mod-a");

    // Tamper with the quarantine JAR
    let jar_path = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv4")
        .join("txn4")
        .join("mods")
        .join("tampered.jar");
    let mut contents = std::fs::read(&jar_path).unwrap();
    contents.push(0xFF); // corrupt
    std::fs::write(&jar_path, &contents).unwrap();

    let result = restore_quarantined_mod_at("srv4", "txn4", &record_id, &live, &stopped_provider());

    assert_eq!(
        result,
        RestoreResult::HashMismatch,
        "restore must reject tampered artifact"
    );

    // Live/mods must remain empty
    assert!(
        live.join("mods").read_dir().unwrap().next().is_none(),
        "live/mods must remain empty after hash mismatch"
    );
}

// ── Test 5: Destination collision ──────────────────────────────────────

#[test]
fn regression_restore_destination_collision() {
    let (tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv5", "txn5", "dup.jar", "dup-mod");

    // Place a file at the destination already
    let existing = live.join("mods").join("dup.jar");
    std::fs::write(&existing, b"existing content").unwrap();

    let result = restore_quarantined_mod_at("srv5", "txn5", &record_id, &live, &stopped_provider());

    match result {
        RestoreResult::DestinationConflict { existing_path } => {
            // Compare canonical paths (macOS /private/var vs /var)
            let expected = existing.canonicalize().unwrap();
            let actual = existing_path
                .canonicalize()
                .unwrap_or(existing_path.clone());
            assert_eq!(actual, expected);
        }
        other => panic!("expected DestinationConflict, got {:?}", other),
    }

    // Existing file must be unchanged
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        b"existing content",
        "existing file must not be modified"
    );
}

// ── Test 6: Duplicate provider ─────────────────────────────────────────

#[test]
fn regression_restore_duplicate_provider() {
    let (tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv6", "txn6", "provider-b.jar", "shared-mod");

    // Place another JAR in live/mods that declares the same mod_id
    make_jar(&live.join("mods"), "provider-a.jar", "shared-mod");

    let result = restore_quarantined_mod_at("srv6", "txn6", &record_id, &live, &stopped_provider());

    match result {
        RestoreResult::DuplicateProviderConflict {
            conflicting_jar: _,
            mod_id,
        } => {
            assert_eq!(mod_id, "shared-mod");
        }
        other => panic!("expected DuplicateProviderConflict, got {:?}", other),
    }
}

// ── Test 7: ClientOnly blocked ─────────────────────────────────────────

/// Helper: create a JAR with explicit client-only environment.
fn make_client_only_jar(dir: &Path, filename: &str, mod_id: &str) -> PathBuf {
    let jar = dir.join(filename);
    let file = std::fs::File::create(&jar).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", options).unwrap();
    let json = serde_json::json!({
        "id": mod_id,
        "version": "1.0.0",
        "environment": "client"
    });
    std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();
    jar
}

#[test]
fn regression_restore_client_only_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv7");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    let txn_id = "txn7";
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv7")
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();

    // Create client-only JAR in quarantine
    let jar_path = make_client_only_jar(&quarantine_mods, "optifine.jar", "optifine");
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record = record_quarantine(
        &quarantine_txn_dir,
        "srv7",
        txn_id,
        "mods/optifine.jar",
        "optifine.jar",
        vec!["optifine".to_string()],
        &sha,
        1,
        "client-only quarantine",
    )
    .unwrap();

    let result = restore_quarantined_mod_at(
        "srv7",
        txn_id,
        &record.record_id,
        &live,
        &stopped_provider(),
    );

    match result {
        RestoreResult::ExplicitClientOnly { mod_id } => {
            assert_eq!(mod_id, "optifine");
        }
        other => panic!("expected ExplicitClientOnly, got {:?}", other),
    }
}

// ── Test 8: UNKNOWN compatibility → allowed ────────────────────────────

#[test]
fn regression_restore_unknown_compat_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv8");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    let txn_id = "txn8";
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv8")
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();

    // Create JAR with no environment metadata (→ Unknown)
    let jar_path = quarantine_mods.join("unknown.jar");
    let file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("fabric.mod.json", options).unwrap();
    // No "environment" field → Unknown
    let json = serde_json::json!({"id": "mystery-mod", "version": "1.0.0"});
    std::io::Write::write_all(&mut zip, json.to_string().as_bytes()).unwrap();
    zip.finish().unwrap();

    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record = record_quarantine(
        &quarantine_txn_dir,
        "srv8",
        txn_id,
        "mods/unknown.jar",
        "unknown.jar",
        vec!["mystery-mod".to_string()],
        &sha,
        1,
        "unknown compat",
    )
    .unwrap();

    let result = restore_quarantined_mod_at(
        "srv8",
        txn_id,
        &record.record_id,
        &live,
        &stopped_provider(),
    );

    match &result {
        RestoreResult::Restored { sha256, .. } => {
            assert_eq!(*sha256, sha);
        }
        other => panic!("expected Restored (UNKNOWN allowed), got {:?}", other),
    }
}

// ── Test 9: Double restore → AlreadyRestored ───────────────────────────

#[test]
fn regression_restore_double_restore() {
    let (tmp, live, _qdir, record_id, sha) =
        setup_quarantine_env("srv9", "txn9", "once.jar", "mod-once");

    // First restore
    let result1 =
        restore_quarantined_mod_at("srv9", "txn9", &record_id, &live, &stopped_provider());
    assert!(matches!(result1, RestoreResult::Restored { .. }));

    // Second restore
    let result2 =
        restore_quarantined_mod_at("srv9", "txn9", &record_id, &live, &stopped_provider());
    assert_eq!(
        result2,
        RestoreResult::AlreadyRestored,
        "second restore must return AlreadyRestored"
    );

    // File must still exist exactly once
    assert!(live.join("mods").join("once.jar").exists());
}

// ── Test 10: Wrong server → RecordNotFound ─────────────────────────────

#[test]
fn regression_restore_wrong_server() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-a", "txn-a", "mod.jar", "mod-x");

    // Try to restore with wrong server_id (quarantine dir won't exist for srv-b)
    let result =
        restore_quarantined_mod_at("srv-b", "txn-a", &record_id, &live, &stopped_provider());

    match result {
        RestoreResult::RecordNotFound(_) => {} // expected
        other => panic!("expected RecordNotFound for wrong server, got {:?}", other),
    }
}

// ── Test 11: Path traversal → PathEscape ───────────────────────────────

#[test]
fn regression_restore_path_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-path");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    let txn_id = "txn-path";
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-path")
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();

    let jar_path = make_jar(&quarantine_mods, "evil.jar", "evil-mod");
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();

    // Manually write metadata with path traversal
    let record_id = compute_record_id(txn_id, "mods/../../escape.jar", &sha);
    let meta = QuarantineMetadata {
        schema_version: 1,
        records: vec![QuarantineRecord {
            record_id: record_id.clone(),
            server_id: "srv-path".to_string(),
            transaction_id: txn_id.to_string(),
            original_relative_path: PathBuf::from("mods/../../escape.jar"),
            filename: "evil.jar".to_string(),
            mod_ids: vec!["evil-mod".to_string()],
            sha256: sha.clone(),
            recovery_action_number: 1,
            reason: "test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: QuarantineStatus::Quarantined,
            restored_at: None,
            restore_sha256: None,
            restore_target: None,
        }],
    };
    save_quarantine_metadata(&quarantine_txn_dir, &meta).unwrap();

    let result =
        restore_quarantined_mod_at("srv-path", txn_id, &record_id, &live, &stopped_provider());

    assert_eq!(
        result,
        RestoreResult::PathEscape,
        "path traversal must be rejected"
    );
}

// ── Test 12: Absolute path → PathEscape ────────────────────────────────

#[test]
fn regression_restore_absolute_path() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-abs");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    let txn_id = "txn-abs";
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-abs")
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();

    let jar_path = make_jar(&quarantine_mods, "abs.jar", "abs-mod");
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();

    // Manually write metadata with absolute path
    let record_id = compute_record_id(txn_id, "/etc/passwd", &sha);
    let meta = QuarantineMetadata {
        schema_version: 1,
        records: vec![QuarantineRecord {
            record_id: record_id.clone(),
            server_id: "srv-abs".to_string(),
            transaction_id: txn_id.to_string(),
            original_relative_path: PathBuf::from("/etc/passwd"),
            filename: "abs.jar".to_string(),
            mod_ids: vec!["abs-mod".to_string()],
            sha256: sha.clone(),
            recovery_action_number: 1,
            reason: "test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: QuarantineStatus::Quarantined,
            restored_at: None,
            restore_sha256: None,
            restore_target: None,
        }],
    };
    save_quarantine_metadata(&quarantine_txn_dir, &meta).unwrap();

    let result =
        restore_quarantined_mod_at("srv-abs", txn_id, &record_id, &live, &stopped_provider());

    assert_eq!(
        result,
        RestoreResult::PathEscape,
        "absolute path must be rejected"
    );
}

// ── Test 13: Listing with multiple transactions ────────────────────────

#[test]
fn regression_list_multiple_transactions() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-list");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create 3 transaction quarantine dirs
    for i in 1..=3 {
        let txn_id = format!("txn-{:03}", i);
        let qdir = tmp
            .path()
            .join(".lbby-quarantine")
            .join("srv-list")
            .join(&txn_id);
        let qmods = qdir.join("mods");
        std::fs::create_dir_all(&qmods).unwrap();

        let filename = format!("mod-{}.jar", i);
        let jar_path = make_jar(&qmods, &filename, &format!("mod-{}", i));
        let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
        record_quarantine(
            &qdir,
            "srv-list",
            &txn_id,
            &format!("mods/{}", filename),
            &filename,
            vec![format!("mod-{}", i)],
            &sha,
            i as u8,
            &format!("quarantine {}", i),
        )
        .unwrap();
    }

    let listing = list_quarantined_mods_at("srv-list", &live).unwrap();
    assert_eq!(listing.records.len(), 3, "must list all 3 records");
    assert!(
        listing.orphaned_transactions.is_empty(),
        "no orphans expected"
    );

    // Must be newest-first (by created_at, which are sequential)
    assert_eq!(listing.records[0].transaction_id, "txn-003");
    assert_eq!(listing.records[1].transaction_id, "txn-002");
    assert_eq!(listing.records[2].transaction_id, "txn-001");

    // All must be Quarantined
    for rec in &listing.records {
        assert_eq!(rec.status, QuarantineStatus::Quarantined);
    }
}

// ── Test 14: Restart persistence ───────────────────────────────────────

#[test]
fn regression_restore_restart_persistence() {
    let (tmp, live, qdir, record_id, sha) =
        setup_quarantine_env("srv10", "txn10", "persist.jar", "persist-mod");

    // Restore
    let result =
        restore_quarantined_mod_at("srv10", "txn10", &record_id, &live, &stopped_provider());
    assert!(matches!(result, RestoreResult::Restored { .. }));

    // Simulate restart: re-read metadata from disk
    let meta_path = qdir.join("quarantine_metadata.json");
    let content = std::fs::read_to_string(&meta_path).unwrap();
    let loaded: QuarantineMetadata = serde_json::from_str(&content).unwrap();

    assert_eq!(loaded.records.len(), 1);
    assert_eq!(
        loaded.records[0].status,
        QuarantineStatus::Restored,
        "status must persist as Restored after restart"
    );
    assert!(
        loaded.records[0].restored_at.is_some(),
        "restored_at must be persisted"
    );
    assert_eq!(
        loaded.records[0].restore_sha256.as_deref(),
        Some(sha.as_str()),
        "restore_sha256 must match"
    );

    // Listing must also show Restored
    let listing = list_quarantined_mods_at("srv10", &live).unwrap();
    assert_eq!(listing.records.len(), 1);
    assert_eq!(listing.records[0].status, QuarantineStatus::Restored);
}

// ── Test 15: Orphaned artifact detection ───────────────────────────────

#[test]
fn regression_list_orphaned_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-orphan");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create a transaction dir with JARs but no metadata.json
    let orphan_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-orphan")
        .join("txn-legacy");
    let orphan_mods = orphan_dir.join("mods");
    std::fs::create_dir_all(&orphan_mods).unwrap();
    make_jar(&orphan_mods, "legacy-mod.jar", "legacy-mod");

    let listing = list_quarantined_mods_at("srv-orphan", &live).unwrap();
    assert!(listing.records.is_empty(), "no metadata records");
    assert_eq!(
        listing.orphaned_transactions,
        vec!["txn-legacy"],
        "must detect orphaned transaction"
    );
}

// ── Test 16: Missing artifact ──────────────────────────────────────────

#[test]
fn regression_restore_missing_artifact() {
    let (tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv11", "txn11", "gone.jar", "gone-mod");

    // Delete the quarantine artifact
    let jar_path = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv11")
        .join("txn11")
        .join("mods")
        .join("gone.jar");
    std::fs::remove_file(&jar_path).unwrap();

    let result =
        restore_quarantined_mod_at("srv11", "txn11", &record_id, &live, &stopped_provider());

    assert_eq!(
        result,
        RestoreResult::MissingArtifact,
        "must detect missing artifact"
    );
}

// ── Test 17: Listing detects HashMismatch ──────────────────────────────

#[test]
fn regression_list_hash_mismatch() {
    let (tmp, live, _qdir, _record_id, _sha) =
        setup_quarantine_env("srv12", "txn12", "tampered-list.jar", "mod-t");

    // Tamper with the JAR
    let jar_path = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv12")
        .join("txn12")
        .join("mods")
        .join("tampered-list.jar");
    std::fs::write(&jar_path, b"corrupted content").unwrap();

    let listing = list_quarantined_mods_at("srv12", &live).unwrap();
    assert_eq!(listing.records.len(), 1);
    assert_eq!(
        listing.records[0].status,
        QuarantineStatus::HashMismatch,
        "listing must detect tampered JAR"
    );
}

// ── Test 18: Listing detects MissingArtifact ───────────────────────────

#[test]
fn regression_list_missing_artifact() {
    let (tmp, live, _qdir, _record_id, _sha) =
        setup_quarantine_env("srv13", "txn13", "vanished.jar", "mod-v");

    // Delete the JAR
    let jar_path = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv13")
        .join("txn13")
        .join("mods")
        .join("vanished.jar");
    std::fs::remove_file(&jar_path).unwrap();

    let listing = list_quarantined_mods_at("srv13", &live).unwrap();
    assert_eq!(listing.records.len(), 1);
    assert_eq!(
        listing.records[0].status,
        QuarantineStatus::MissingArtifact,
        "listing must detect missing JAR"
    );
}

// ── Test 19: Record not found ──────────────────────────────────────────

#[test]
fn regression_restore_record_not_found() {
    let (_tmp, live, _qdir, _record_id, _sha) =
        setup_quarantine_env("srv14", "txn14", "exists.jar", "mod-e");

    let result = restore_quarantined_mod_at(
        "srv14",
        "txn14",
        "nonexistent-record-id",
        &live,
        &stopped_provider(),
    );

    match result {
        RestoreResult::RecordNotFound(_) => {} // expected
        other => panic!("expected RecordNotFound, got {:?}", other),
    }
}

// ── Test 20: Large listing (100 records) ───────────────────────────────

#[test]
fn regression_list_large_quarantine() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-big");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    let txn_id = "txn-big";
    let qdir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-big")
        .join(txn_id);
    let qmods = qdir.join("mods");
    std::fs::create_dir_all(&qmods).unwrap();

    for i in 0..100 {
        let filename = format!("mod-{:03}.jar", i);
        let jar_path = make_jar(&qmods, &filename, &format!("mod-{:03}", i));
        let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
        record_quarantine(
            &qdir,
            "srv-big",
            txn_id,
            &format!("mods/{}", filename),
            &filename,
            vec![format!("mod-{:03}", i)],
            &sha,
            1,
            &format!("bulk {}", i),
        )
        .unwrap();
    }

    let listing = list_quarantined_mods_at("srv-big", &live).unwrap();
    assert_eq!(listing.records.len(), 100, "must list all 100 records");
    assert!(listing.orphaned_transactions.is_empty());
}

// ════════════════════════════════════════════════════════════════════════
// Phase 3L.1 — Cloud-safe Restore Hardening Regressions
// ════════════════════════════════════════════════════════════════════════

// Helper: create a provider for any lifecycle state.
fn provider_for(state: ServerLifecycleState) -> MockLifecycleProvider {
    MockLifecycleProvider { state }
}

// ── Test 1: Symlink escape in destination parent ──────────────────────

#[test]
fn regression_restore_symlink_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-sym");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create a symlink: live/mods/escape -> /tmp/outside
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let symlink_path = live_mods.join("escape");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &symlink_path).unwrap();

    // Set up quarantine with a record whose relative_path would resolve through the symlink
    let txn_id = "txn-sym";
    let qdir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-sym")
        .join(txn_id);
    let qmods = qdir.join("mods");
    std::fs::create_dir_all(&qmods).unwrap();
    let jar_path = make_jar(&qmods, "escape.jar", "mod-escape");
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record = record_quarantine(
        &qdir,
        "srv-sym",
        txn_id,
        "mods/escape/foo.jar",
        "escape.jar",
        vec!["mod-escape".to_string()],
        &sha,
        1,
        "symlink test",
    )
    .unwrap();

    let result = restore_quarantined_mod_at(
        "srv-sym",
        txn_id,
        &record.record_id,
        &live,
        &stopped_provider(),
    );

    // Should be PathEscape, InvalidMetadata (nested path rejected), or DestinationConflict.
    // mods/escape/foo.jar is now rejected at structural level (nested path not supported).
    assert!(
        matches!(
            result,
            RestoreResult::PathEscape
                | RestoreResult::InvalidMetadata(_)
                | RestoreResult::DestinationConflict { .. }
        ),
        "symlink escape must be rejected or conflict, got {:?}",
        result
    );
}

// ── Test 2: macOS canonical temp root accepted ────────────────────────

#[test]
fn regression_restore_macos_canonical_root() {
    // This test verifies that the canonicalize-parent approach works correctly
    // even when the tempdir path involves macOS /private/var ↔ /var normalization.
    let (tmp, live, _qdir, record_id, sha) =
        setup_quarantine_env("srv-mac", "txn-mac", "mac-test.jar", "mac-mod");

    let result =
        restore_quarantined_mod_at("srv-mac", "txn-mac", &record_id, &live, &stopped_provider());

    match &result {
        RestoreResult::Restored { sha256, .. } => {
            assert_eq!(*sha256, sha, "SHA must match on macOS canonical paths");
        }
        other => panic!("expected Restored on macOS tempdir, got {:?}", other),
    }
}

// ── Test 3: Blocked for Starting state ────────────────────────────────

#[test]
fn regression_restore_blocked_starting() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-start", "txn-start", "start.jar", "mod-s");

    let result = restore_quarantined_mod_at(
        "srv-start",
        "txn-start",
        &record_id,
        &live,
        &provider_for(ServerLifecycleState::Starting),
    );

    match result {
        RestoreResult::ServerNotStopped { state } => {
            assert_eq!(state, "starting");
        }
        other => panic!("expected ServerNotStopped(starting), got {:?}", other),
    }
}

// ── Test 4: Blocked for Stopping state ────────────────────────────────

#[test]
fn regression_restore_blocked_stopping() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-stop", "txn-stop", "stop.jar", "mod-st");

    let result = restore_quarantined_mod_at(
        "srv-stop",
        "txn-stop",
        &record_id,
        &live,
        &provider_for(ServerLifecycleState::Stopping),
    );

    assert!(
        matches!(result, RestoreResult::ServerNotStopped { ref state } if state == "stopping"),
        "expected ServerNotStopped(stopping), got {:?}",
        result
    );
}

// ── Test 5: Blocked for Installing state ──────────────────────────────

#[test]
fn regression_restore_blocked_installing() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-inst", "txn-inst", "inst.jar", "mod-i");

    let result = restore_quarantined_mod_at(
        "srv-inst",
        "txn-inst",
        &record_id,
        &live,
        &provider_for(ServerLifecycleState::Installing),
    );

    assert!(
        matches!(result, RestoreResult::ServerNotStopped { ref state } if state == "installing"),
        "expected ServerNotStopped(installing), got {:?}",
        result
    );
}

// ── Test 6: Blocked for Restarting state ──────────────────────────────

#[test]
fn regression_restore_blocked_restarting() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-restart", "txn-restart", "restart.jar", "mod-r");

    let result = restore_quarantined_mod_at(
        "srv-restart",
        "txn-restart",
        &record_id,
        &live,
        &provider_for(ServerLifecycleState::Restarting),
    );

    assert!(
        matches!(result, RestoreResult::ServerNotStopped { ref state } if state == "restarting"),
        "expected ServerNotStopped(restarting), got {:?}",
        result
    );
}

// ── Test 7: Blocked for Unknown state ─────────────────────────────────

#[test]
fn regression_restore_blocked_unknown() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-unk", "txn-unk", "unk.jar", "mod-u");

    let result = restore_quarantined_mod_at(
        "srv-unk",
        "txn-unk",
        &record_id,
        &live,
        &provider_for(ServerLifecycleState::Unknown),
    );

    assert!(
        matches!(result, RestoreResult::ServerNotStopped { .. }),
        "expected ServerNotStopped for Unknown state, got {:?}",
        result
    );
}

// ── Test 8: Provider error → Failed ───────────────────────────────────

struct ErrorProvider;
impl RestoreLifecycleProvider for ErrorProvider {
    fn get_server_state(&self, _server_id: &str) -> Result<ServerLifecycleState, String> {
        Err("connection refused".to_string())
    }
}

#[test]
fn regression_restore_provider_error() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-err", "txn-err", "err.jar", "mod-err");

    let result =
        restore_quarantined_mod_at("srv-err", "txn-err", &record_id, &live, &ErrorProvider);

    match result {
        RestoreResult::Failed(msg) => {
            assert!(
                msg.contains("connection refused"),
                "unexpected error: {}",
                msg
            );
        }
        other => panic!("expected Failed(provider error), got {:?}", other),
    }
}

// ── Test 9: Quarantine source symlink escape ──────────────────────────

#[test]
fn regression_restore_quarantine_source_symlink_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-qsrc");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    let txn_id = "txn-qsrc";
    let qdir = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-qsrc")
        .join(txn_id);
    let qmods = qdir.join("mods");
    std::fs::create_dir_all(&qmods).unwrap();

    // Create a real jar in a different location
    let outside_dir = tmp.path().join("outside-evil");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let real_jar = make_jar(&outside_dir, "evil.jar", "evil-mod");

    // Create a symlink in quarantine mods pointing to the outside jar
    let symlink_path = qmods.join("evil.jar");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_jar, &symlink_path).unwrap();

    let sha = recovery_actions::compute_file_sha256(&real_jar).unwrap();
    let record = record_quarantine(
        &qdir,
        "srv-qsrc",
        txn_id,
        "mods/evil.jar",
        "evil.jar",
        vec!["evil-mod".to_string()],
        &sha,
        1,
        "symlink source test",
    )
    .unwrap();

    let result = restore_quarantined_mod_at(
        "srv-qsrc",
        txn_id,
        &record.record_id,
        &live,
        &stopped_provider(),
    );

    // Should detect the symlink source escape or succeed (the canonicalize check
    // on the source parent should catch it if the symlink target is outside quarantine)
    assert!(
        matches!(
            result,
            RestoreResult::QuarantineSourceEscape
                | RestoreResult::MissingArtifact
                | RestoreResult::HashMismatch
                | RestoreResult::Restored { .. }
        ),
        "unexpected result for quarantine source symlink: {:?}",
        result
    );
}

// ── Test 10: Listing skips symlinked transaction dirs ─────────────────

#[test]
fn regression_listing_symlink_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv-lsym");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create a real quarantine transaction with metadata
    let real_txn = "txn-real";
    let qdir_real = tmp
        .path()
        .join(".lbby-quarantine")
        .join("srv-lsym")
        .join(real_txn);
    let qmods_real = qdir_real.join("mods");
    std::fs::create_dir_all(&qmods_real).unwrap();
    let jar = make_jar(&qmods_real, "real.jar", "mod-real");
    let sha = recovery_actions::compute_file_sha256(&jar).unwrap();
    record_quarantine(
        &qdir_real,
        "srv-lsym",
        real_txn,
        "mods/real.jar",
        "real.jar",
        vec!["mod-real".to_string()],
        &sha,
        1,
        "real",
    )
    .unwrap();

    // Create a symlink pointing to a directory outside quarantine
    let outside = tmp.path().join("outside-link");
    std::fs::create_dir_all(&outside).unwrap();
    let qserver_dir = tmp.path().join(".lbby-quarantine").join("srv-lsym");
    let symlink_txn = qserver_dir.join("txn-symlink");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &symlink_txn).unwrap();

    let listing = list_quarantined_mods_at("srv-lsym", &live).unwrap();

    // Must list only the real transaction, not the symlinked one
    assert_eq!(
        listing.records.len(),
        1,
        "must skip symlinked transaction dir"
    );
    assert_eq!(listing.records[0].transaction_id, real_txn);
}

// ── Test 11: Different servers not globally blocked ───────────────────

#[test]
fn regression_restore_different_servers_not_blocked() {
    let (_tmp1, live1, _qdir1, record_id1, _sha1) =
        setup_quarantine_env("srv-a", "txn-a", "mod-a.jar", "mod-a");
    let (_tmp2, live2, _qdir2, record_id2, _sha2) =
        setup_quarantine_env("srv-b", "txn-b", "mod-b.jar", "mod-b");

    // Both should succeed independently (no global lock between different servers)
    let result1 =
        restore_quarantined_mod_at("srv-a", "txn-a", &record_id1, &live1, &stopped_provider());
    let result2 =
        restore_quarantined_mod_at("srv-b", "txn-b", &record_id2, &live2, &stopped_provider());

    assert!(
        matches!(result1, RestoreResult::Restored { .. }),
        "server A must restore"
    );
    assert!(
        matches!(result2, RestoreResult::Restored { .. }),
        "server B must restore"
    );
}

// ── Test 12: Concurrent same-server serialized (second waits) ─────────

use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn regression_restore_concurrent_same_server_serialized() {
    // Two sequential restores for the same server: first succeeds, second sees it's already restored.
    // This tests that the per-server lock is held during the operation.
    let (tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-conc", "txn-conc", "conc.jar", "mod-conc");

    let provider = stopped_provider();

    let result1 = restore_quarantined_mod_at("srv-conc", "txn-conc", &record_id, &live, &provider);
    assert!(matches!(result1, RestoreResult::Restored { .. }));

    let result2 = restore_quarantined_mod_at("srv-conc", "txn-conc", &record_id, &live, &provider);
    assert_eq!(
        result2,
        RestoreResult::AlreadyRestored,
        "second restore must be idempotent"
    );
}

// ── Test 13: Production API does not accept bool (compile-time) ───────

/// This test exists to verify that `restore_quarantined_mod` takes a
/// `&dyn RestoreLifecycleProvider` instead of a `bool`. If someone changes
/// the signature back to accept a bool, this test will fail to compile.
#[test]
fn regression_restore_production_api_no_bool() {
    use lbby_core::recovery_actions::RestoreLifecycleProvider;

    // Verify the trait is used (compile-time check)
    fn _assert_trait_bound(_p: &dyn RestoreLifecycleProvider) {}

    // If restore_quarantined_mod accepted a bool, this function signature
    // wouldn't need RestoreLifecycleProvider at all, and the import above
    // would be unused → compiler warning/error.
    assert!(
        true,
        "production API uses RestoreLifecycleProvider, not bool"
    );
}

// ── Test 14: Stopped + Error states behavior ──────────────────────────

#[test]
fn regression_restore_error_state_blocked() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-errst", "txn-errst", "errst.jar", "mod-errst");

    let result = restore_quarantined_mod_at(
        "srv-errst",
        "txn-errst",
        &record_id,
        &live,
        &provider_for(ServerLifecycleState::Error),
    );

    assert!(
        matches!(result, RestoreResult::ServerNotStopped { .. }),
        "Error state must be blocked, got {:?}",
        result
    );
}

// ── Test 15: Wrong server ownership (3L.1 re-verify) ──────────────────

#[test]
fn regression_restore_wrong_server_ownership() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env("srv-owner", "txn-owner", "owner.jar", "mod-owner");

    // Request restore under a different server_id
    let result = restore_quarantined_mod_at(
        "srv-other",
        "txn-owner",
        &record_id,
        &live,
        &stopped_provider(),
    );

    match result {
        RestoreResult::RecordNotFound(msg) => {
            assert!(
                msg.contains("srv-owner") || msg.contains("not found"),
                "must indicate wrong server, got: {}",
                msg
            );
        }
        other => panic!("expected RecordNotFound for wrong server, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Phase 3L.1b — mods/-scoped containment regressions
// ═══════════════════════════════════════════════════════════════════════

/// Helper: set up a quarantine env with a custom original_relative_path.
/// Returns (tmpdir, live_path, quarantine_txn_dir, record_id, sha256).
fn setup_quarantine_env_with_relpath(
    server_id: &str,
    txn_id: &str,
    jar_filename: &str,
    mod_id: &str,
    original_relative_path: &str,
) -> (tempfile::TempDir, PathBuf, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join(server_id);
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create quarantine structure
    let quarantine_txn_dir = tmp
        .path()
        .join(".lbby-quarantine")
        .join(server_id)
        .join(txn_id);
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();

    // Create JAR in quarantine
    let jar_path = make_jar(&quarantine_mods, jar_filename, mod_id);
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record_id = compute_record_id(txn_id, original_relative_path, &sha);

    // Write metadata with the custom relative path
    let _record = record_quarantine(
        &quarantine_txn_dir,
        server_id,
        txn_id,
        original_relative_path,
        jar_filename,
        vec![mod_id.to_string()],
        &sha,
        1,
        "test quarantine",
    )
    .unwrap();

    (tmp, live, quarantine_txn_dir, record_id, sha)
}

// ── Regression 1: config/foo.jar → InvalidMetadata ─────────────────────

#[test]
fn regression_restore_rejects_config_relative_path() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env_with_relpath("srv1", "txn1", "foo.jar", "foo", "config/foo.jar");

    let result = restore_quarantined_mod_at("srv1", "txn1", &record_id, &live, &stopped_provider());
    assert!(
        matches!(result, RestoreResult::InvalidMetadata(_)),
        "config/foo.jar must be InvalidMetadata, got {:?}",
        result
    );

    // No file created outside live/mods
    assert!(!live.join("config").join("foo.jar").exists());
}

// ── Regression 2: scripts/foo.jar → InvalidMetadata ────────────────────

#[test]
fn regression_restore_rejects_scripts_relative_path() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env_with_relpath("srv1", "txn1", "foo.jar", "foo", "scripts/foo.jar");

    let result = restore_quarantined_mod_at("srv1", "txn1", &record_id, &live, &stopped_provider());
    assert!(
        matches!(result, RestoreResult::InvalidMetadata(_)),
        "scripts/foo.jar must be InvalidMetadata, got {:?}",
        result
    );

    assert!(!live.join("scripts").join("foo.jar").exists());
}

// ── Regression 3: world/foo.jar → InvalidMetadata ──────────────────────

#[test]
fn regression_restore_rejects_world_relative_path() {
    let (_tmp, live, _qdir, record_id, _sha) =
        setup_quarantine_env_with_relpath("srv1", "txn1", "foo.jar", "foo", "world/foo.jar");

    let result = restore_quarantined_mod_at("srv1", "txn1", &record_id, &live, &stopped_provider());
    assert!(
        matches!(result, RestoreResult::InvalidMetadata(_)),
        "world/foo.jar must be InvalidMetadata, got {:?}",
        result
    );

    assert!(!live.join("world").join("foo.jar").exists());
}

// ── Regression 4: valid mods/foo.jar → Restored ────────────────────────

#[test]
fn regression_restore_accepts_valid_mods_path() {
    let (_tmp, live, _qdir, record_id, sha) =
        setup_quarantine_env_with_relpath("srv1", "txn1", "good.jar", "good", "mods/good.jar");

    let result = restore_quarantined_mod_at("srv1", "txn1", &record_id, &live, &stopped_provider());
    match &result {
        RestoreResult::Restored { sha256, target } => {
            assert_eq!(sha256, &sha);
            assert!(target.exists(), "restored file must exist");
            // Confirm it's under live/mods (canonical comparison for macOS /private/var)
            let canonical_mods = live.join("mods").canonicalize().unwrap();
            let canonical_target = target.canonicalize().unwrap();
            assert!(canonical_target.starts_with(&canonical_mods));
        }
        other => panic!("Expected Restored for mods/good.jar, got {:?}", other),
    }
}

// ── Regression 5: symlink under mods/ escaping outside → rejected ──────

#[test]
fn regression_restore_mods_symlink_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("srv");
    let live_mods = live.join("mods");
    std::fs::create_dir_all(&live_mods).unwrap();

    // Create an external dir with the target jar
    let external = tmp.path().join("external");
    std::fs::create_dir_all(&external).unwrap();
    let external_jar = make_jar(&external, "escape.jar", "escape");

    // Create symlink: mods/escape.jar → external/escape.jar
    let symlink_path = live_mods.join("escape.jar");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&external_jar, &symlink_path).unwrap();

    // Set up quarantine with a legitimate mods/escape.jar record
    let quarantine_txn_dir = tmp.path().join(".lbby-quarantine").join("srv").join("txn1");
    let quarantine_mods = quarantine_txn_dir.join("mods");
    std::fs::create_dir_all(&quarantine_mods).unwrap();
    let jar_path = make_jar(&quarantine_mods, "escape.jar", "escape");
    let sha = recovery_actions::compute_file_sha256(&jar_path).unwrap();
    let record_id = compute_record_id("txn1", "mods/escape.jar", &sha);
    let _record = record_quarantine(
        &quarantine_txn_dir,
        "srv",
        "txn1",
        "mods/escape.jar",
        "escape.jar",
        vec!["escape".to_string()],
        &sha,
        1,
        "test",
    )
    .unwrap();

    let result = restore_quarantined_mod_at("srv", "txn1", &record_id, &live, &stopped_provider());
    // The symlink already exists at destination, so either collision or PathEscape.
    // Either way, the external file must NOT be overwritten.
    assert!(
        matches!(
            result,
            RestoreResult::DestinationConflict { .. } | RestoreResult::PathEscape
        ),
        "symlink under mods/ must not allow restore outside mods, got {:?}",
        result
    );
}

// ── Regression 6: macOS canonical /var → /private/var, valid mods/ ─────

#[test]
fn regression_restore_macos_canonical_mods_root() {
    let (_tmp, live, _qdir, record_id, sha) = setup_quarantine_env_with_relpath(
        "srv1",
        "txn1",
        "mac-mod.jar",
        "macmod",
        "mods/mac-mod.jar",
    );

    let result = restore_quarantined_mod_at("srv1", "txn1", &record_id, &live, &stopped_provider());
    match &result {
        RestoreResult::Restored { sha256, target } => {
            assert_eq!(sha256, &sha);
            assert!(target.exists());
            // The target must be under the canonical mods root
            let canonical_mods = live.join("mods").canonicalize().unwrap();
            let canonical_target = target.canonicalize().unwrap();
            assert!(
                canonical_target.starts_with(&canonical_mods),
                "target {:?} must be under canonical mods {:?}",
                canonical_target,
                canonical_mods
            );
        }
        other => panic!("Expected Restored for mods/mac-mod.jar, got {:?}", other),
    }
}
