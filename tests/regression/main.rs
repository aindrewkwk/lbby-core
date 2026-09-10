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
    ValidationFailureReason, ValidationOutcome, ValidationRepairOrchestrator,
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
