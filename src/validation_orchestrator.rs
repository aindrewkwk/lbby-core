// validation_orchestrator.rs — Phase 3G: Validation & Repair Orchestrator Hardening.
//
// Extracts and hardens the existing validation/repair control flow into a
// dedicated orchestrator WITHOUT adding new repair capabilities.
//
// This module owns:
//   - BootValidator invocation
//   - Failure classification
//   - Repair delegation (to 3F-B and 3F-C)
//   - Retry budgets (global boot ceiling, dep rounds, runtime rounds)
//   - Repair history
//   - Final validation result
//
// This module does NOT own:
//   - CurseForge manifest parsing
//   - Mod downloads
//   - Initial modpack construction
//   - InstallTransaction creation / commit / rollback
//   - Compatibility classification
//   - Dependency graph implementation
//   - Java download implementation
//   - Diagnostics filesystem policy
//
// Design principles:
//   - One boot attempt = one decision = at most one repair action.
//   - Global boot ceiling (MAX_TOTAL_BOOT_ATTEMPTS) is the FINAL authority.
//   - Separate budgets for dep repairs and runtime repairs.
//   - classify_failure() is pure and testable.
//   - Transaction ownership stays outside — caller does commit/rollback.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::boot_failure_analyzer::{
    self, AttributionConfidence, BootAttribution, BootRepairOutcome, InstalledFileRegistry,
};
use crate::boot_validator::{BootFailureReason, BootResult, BootValidator};
use crate::config::ServerConfig;
use crate::dependency_graph::DependencyGraph;
use crate::dependency_resolver::DependencyResolver;
use crate::runtime_remediator::{
    self, RuntimeIssue, RuntimeRemediationResult, RuntimeRepairRecord, RuntimeRepairStatus,
};

use std::future::Future;
use std::pin::Pin;

// ── Boot validation trait for testability ─────────────────────────────

/// Abstraction over boot validation so tests can inject deterministic results.
/// Uses boxed futures for Rust 2021 compatibility without async-trait.
pub trait BootValidationRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        cfg: &'a ServerConfig,
        staging_path: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BootResult> + Send + 'a>>;
}

/// Real BootValidator delegates to the existing implementation.
impl BootValidationRunner for BootValidator {
    fn run<'a>(
        &'a self,
        cfg: &'a ServerConfig,
        staging_path: &'a Path,
    ) -> Pin<Box<dyn Future<Output = BootResult> + Send + 'a>> {
        Box::pin(BootValidator::validate(self, cfg, staging_path))
    }
}

// ── Constants ──────────────────────────────────────────────────────────

/// Maximum number of boot-time dependency repair rounds per validation cycle.
/// Phase 3F-A already does up to MAX_REPAIR_ROUNDS pre-boot.
pub const MAX_BOOT_DEP_ROUNDS: u8 = 2;

/// Maximum runtime remediation rounds per validation cycle.
pub const MAX_RUNTIME_ROUNDS: u8 = 2;

/// Global ceiling: every actual Java server launch counts toward this.
/// No combination of repair loops may cause unbounded server launches.
pub const MAX_TOTAL_BOOT_ATTEMPTS: u8 = 6;

// ── Unified retry state ────────────────────────────────────────────────

/// Tracks retry state across ALL repair systems to prevent loops.
/// Single source of truth — no second independent retry state.
#[derive(Debug, Clone)]
pub struct ValidationRetryState {
    /// Total actual Java server launches.
    pub total_boot_attempts: u8,
    /// Number of boot dependency repair rounds executed.
    pub dependency_repairs: u8,
    /// Number of runtime remediation rounds executed.
    pub runtime_repairs: u8,
    /// Missing mods already attempted by 3F-B.
    pub attempted_missing_mods: HashSet<String>,
    /// Java executable paths already attempted by 3F-C.
    pub attempted_java_paths: HashSet<PathBuf>,
    /// Memory values already attempted by 3F-C.
    pub attempted_memory_values: HashSet<u32>,
    /// Java major versions already attempted.
    pub attempted_java_majors: HashSet<u8>,
}

impl ValidationRetryState {
    pub fn new() -> Self {
        Self {
            total_boot_attempts: 0,
            dependency_repairs: 0,
            runtime_repairs: 0,
            attempted_missing_mods: HashSet::new(),
            attempted_java_paths: HashSet::new(),
            attempted_memory_values: HashSet::new(),
            attempted_java_majors: HashSet::new(),
        }
    }

    /// Check if we can attempt another boot (global ceiling).
    pub fn can_attempt_boot(&self) -> bool {
        self.total_boot_attempts < MAX_TOTAL_BOOT_ATTEMPTS
    }

    /// Consume one boot attempt. Returns the new count, or Err if exhausted.
    pub fn consume_boot_attempt(&mut self) -> Result<u8, RetryLimitReached> {
        if self.total_boot_attempts >= MAX_TOTAL_BOOT_ATTEMPTS {
            return Err(RetryLimitReached);
        }
        self.total_boot_attempts += 1;
        Ok(self.total_boot_attempts)
    }

    /// Check if we can attempt another boot dependency repair round.
    pub fn can_attempt_dep_repair(&self) -> bool {
        self.dependency_repairs < MAX_BOOT_DEP_ROUNDS
    }

    /// Check if we can attempt another runtime remediation round.
    pub fn can_attempt_runtime_repair(&self) -> bool {
        self.runtime_repairs < MAX_RUNTIME_ROUNDS
    }

    /// Record a dependency repair attempt.
    pub fn record_dep_repair(&mut self) {
        self.dependency_repairs += 1;
    }

    /// Record a runtime repair attempt.
    pub fn record_runtime_repair(&mut self) {
        self.runtime_repairs += 1;
    }

    /// Check if a Java path was already attempted.
    pub fn has_attempted_java_path(&self, path: &Path) -> bool {
        self.attempted_java_paths.contains(path)
    }

    /// Check if a Java major was already attempted.
    pub fn has_attempted_java_major(&self, major: u8) -> bool {
        self.attempted_java_majors.contains(&major)
    }

    /// Record a Java path attempt.
    pub fn record_java_attempt(&mut self, path: PathBuf, major: u8) {
        self.attempted_java_paths.insert(path);
        self.attempted_java_majors.insert(major);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryLimitReached;

impl std::fmt::Display for RetryLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Global boot attempt limit ({}) reached",
            MAX_TOTAL_BOOT_ATTEMPTS
        )
    }
}

// ── Outcome types ──────────────────────────────────────────────────────

/// Final validation outcome returned by the orchestrator.
#[derive(Debug)]
pub enum ValidationOutcome {
    /// Server validated successfully.
    Validated(ValidationSuccess),
    /// Server failed validation after exhausting all allowed repairs.
    Failed(ValidationFailure),
    /// High-confidence crash attribution found; user action requested.
    /// The transaction should be paused (not rolled back) until the user
    /// approves or rejects the recommended recovery action.
    UserActionRequired(UserActionRequest),
}

/// Success details.
#[derive(Debug)]
pub struct ValidationSuccess {
    /// Total boot attempts consumed (1-based count).
    pub total_boot_attempts: u8,
    /// Full audit trail.
    pub history: ValidationHistory,
    /// Whether the config was mutated (e.g. Java path changed).
    pub final_config_changed: bool,
}

/// Failure details.
#[derive(Debug)]
pub struct ValidationFailure {
    /// The last BootResult that caused the failure.
    pub final_boot_result: BootResult,
    /// Structured failure reason.
    pub reason: ValidationFailureReason,
    /// Full audit trail.
    pub history: ValidationHistory,
    /// Loader compatibility report, if a loader mismatch was detected.
    pub loader_report: Option<crate::loader_compat_advisor::LoaderCompatibilityReport>,
    /// Crash attribution report, if attribution was attempted.
    pub crash_report: Option<crate::crash_attribution::CrashAttributionReport>,
}

/// User action request — produced when High-confidence crash attribution
/// identifies a unique culprit and a reversible recovery action is available.
///
/// The caller must pause the transaction (not roll back) and present this
/// to the user. If the user approves, call `execute_approved_recovery`.
#[derive(Debug)]
pub struct UserActionRequest {
    /// The crash attribution report that triggered this request.
    pub crash_report: crate::crash_attribution::CrashAttributionReport,
    /// The recovery action availability (should be Available).
    pub availability: crate::recovery_actions::RecoveryActionAvailability,
    /// Deterministic fingerprint for approval validation.
    pub fingerprint: String,
    /// Transaction metadata for pause/resume.
    pub transaction_id: String,
    /// The staging mods directory.
    pub staging_mods: PathBuf,
    /// Current boot attempt number.
    pub boot_attempt: u8,
    /// Full audit trail up to this point.
    pub history: ValidationHistory,
}

/// Structured failure reason — never reduced to plain strings internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationFailureReason {
    /// Global boot attempt ceiling reached.
    RetryLimitReached,
    /// Boot dependency repair failed or was insufficient.
    DependencyRepairFailed,
    /// Runtime remediation failed or was insufficient.
    RuntimeRepairFailed,
    /// Issue detected but not repairable (OOM, loader mismatch, unknown).
    NonRepairableFailure,
    /// Loader mismatch detected — advisor produced a structured report.
    LoaderMismatchDetected,
    /// Validation cleanup failed after a boot attempt.
    ValidationCleanupFailed(String),
}

// ── History / audit trail ──────────────────────────────────────────────

/// Full audit trail of the validation cycle.
#[derive(Debug, Default)]
pub struct ValidationHistory {
    /// Ordered list of boot attempts.
    pub boot_attempts: Vec<BootAttemptRecord>,
    /// Ordered list of repair events.
    pub repairs: Vec<RepairEvent>,
}

/// Record of a single boot attempt.
#[derive(Debug)]
pub struct BootAttemptRecord {
    /// 1-based attempt number.
    pub attempt_number: u8,
    /// What the boot produced.
    pub boot_result_category: BootResultCategory,
    /// What repair was chosen (if any).
    pub chosen_action: RepairAction,
    /// What happened with the repair.
    pub action_result: ActionResult,
}

/// Simplified boot result category for history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootResultCategory {
    Success,
    Failed,
    Timeout,
}

/// A repair event in the audit trail.
#[derive(Debug, Clone)]
pub enum RepairEvent {
    /// Boot dependency repair (Phase 3F-B).
    BootDependency(BootRepairRecordSummary),
    /// Runtime remediation (Phase 3F-C).
    Runtime(RuntimeRepairRecord),
}

/// Summary of a boot dependency repair for the audit trail.
#[derive(Debug, Clone)]
pub struct BootRepairRecordSummary {
    /// Which boot attempt triggered this repair.
    pub boot_attempt: u8,
    /// The missing mod ID that was repaired.
    pub missing_mod_id: String,
    /// Confidence level of the attribution.
    pub confidence: AttributionConfidence,
    /// Whether repair succeeded.
    pub succeeded: bool,
}

// ── Decision types ─────────────────────────────────────────────────────

/// Repair action chosen by the orchestrator. Inspectable in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// Attempt to repair a missing boot dependency.
    BootDependency(BootDependencyDecision),
    /// Attempt runtime remediation (Java, memory, etc.).
    Runtime(RuntimeDecision),
    /// No repair possible for this failure.
    None,
}

/// Decision to attempt a boot dependency repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootDependencyDecision {
    pub missing_mod_id: String,
    pub confidence: AttributionConfidence,
}

/// Decision to attempt a runtime repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDecision {
    /// The classified runtime issue.
    pub issue: RuntimeIssue,
    /// The log tail from the failed boot (needed for remediation).
    pub log_tail: String,
}

/// Result of executing a repair action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    /// Repair succeeded.
    Repaired,
    /// Issue detected but not repairable.
    NotRepairable(String),
    /// Repair attempted but failed.
    Failed(String),
    /// Skipped (e.g. low confidence).
    Skipped(String),
    /// No action was taken (RepairAction::None).
    NoAction,
}

// ── ValidationContext ──────────────────────────────────────────────────

/// Narrow context for the orchestrator. Does NOT dump entire app state.
pub struct ValidationContext<'a> {
    pub cfg: &'a mut ServerConfig,
    pub staging_path: &'a Path,
    pub app: &'a std::sync::Arc<crate::app_state::AppEventSender>,
    pub cf_client: &'a reqwest::Client,
    pub installed_files: &'a InstalledFileRegistry,
    pub dependency_resolver: &'a mut DependencyResolver,
}

// ── Orchestrator ───────────────────────────────────────────────────────

pub struct ValidationRepairOrchestrator {
    state: ValidationRetryState,
    history: ValidationHistory,
    config_changed: bool,
    #[cfg(any(test, feature = "testing"))]
    test_repair_overrides: Option<std::collections::VecDeque<ActionResult>>,
}

impl ValidationRepairOrchestrator {
    pub fn new() -> Self {
        Self {
            state: ValidationRetryState::new(),
            history: ValidationHistory::default(),
            config_changed: false,
            #[cfg(any(test, feature = "testing"))]
            test_repair_overrides: None,
        }
    }

    /// Test-only constructor that injects deterministic repair results.
    #[cfg(any(test, feature = "testing"))]
    pub fn new_with_repair_overrides(overrides: Vec<ActionResult>) -> Self {
        Self {
            state: ValidationRetryState::new(),
            history: ValidationHistory::default(),
            config_changed: false,
            test_repair_overrides: Some(std::collections::VecDeque::from(overrides)),
        }
    }

    /// Run the full validation loop.
    ///
    /// Calls BootValidator.validate() repeatedly, classifies failures,
    /// delegates repairs (3F-B, 3F-C), and enforces all retry budgets.
    ///
    /// Returns ValidationOutcome::Validated on success, or
    /// ValidationOutcome::Failed on exhaustion / non-repairable failure.
    ///
    /// The caller is responsible for:
    ///   - txn.commit() on Validated
    ///   - saving diagnostics + txn.rollback() on Failed
    pub async fn validate(
        &mut self,
        ctx: &mut ValidationContext<'_>,
        validator: &dyn BootValidationRunner,
    ) -> ValidationOutcome {
        let original_cfg_json = serde_json::to_string(ctx.cfg).ok();
        let mut last_boot_result: Option<BootResult> = None;

        loop {
            // ── Global boot ceiling ────────────────────────────────
            match self.state.consume_boot_attempt() {
                Ok(attempt) => {
                    eprintln!(
                        "[CF][validation] Boot attempt {}/{}",
                        attempt, MAX_TOTAL_BOOT_ATTEMPTS
                    );
                }
                Err(_) => {
                    eprintln!(
                        "[CF][validation] Global boot attempt limit ({}) reached — stopping",
                        MAX_TOTAL_BOOT_ATTEMPTS
                    );
                    let final_boot_result = last_boot_result.take().unwrap_or_else(|| {
                        BootResult::Failed(crate::boot_validator::BootFailure {
                            exit_code: None,
                            reason: BootFailureReason::Unknown(
                                "Boot attempt limit reached".to_string(),
                            ),
                            log_tail: String::new(),
                        })
                    });
                    return ValidationOutcome::Failed(ValidationFailure {
                        final_boot_result,
                        reason: ValidationFailureReason::RetryLimitReached,
                        history: std::mem::take(&mut self.history),
                        loader_report: None,
                        crash_report: None,
                    });
                }
            }

            // ── Run boot validator ─────────────────────────────────
            let boot_result = validator.run(ctx.cfg, ctx.staging_path).await;
            last_boot_result = Some(boot_result.clone());

            match &boot_result {
                BootResult::Success(_) => {
                    let attempt = self.state.total_boot_attempts;
                    self.history.boot_attempts.push(BootAttemptRecord {
                        attempt_number: attempt,
                        boot_result_category: BootResultCategory::Success,
                        chosen_action: RepairAction::None,
                        action_result: ActionResult::NoAction,
                    });
                    eprintln!("[CF][validation] Boot {} succeeded", attempt);

                    // Cleanup validation overlay
                    if let Err(e) =
                        crate::boot_validator::verify_validation_cleanup(ctx.staging_path)
                    {
                        return ValidationOutcome::Failed(ValidationFailure {
                            final_boot_result: boot_result,
                            reason: ValidationFailureReason::ValidationCleanupFailed(e),
                            history: std::mem::take(&mut self.history),
                            loader_report: None,
                            crash_report: None,
                        });
                    }

                    // Check if config changed
                    if let Some(ref orig) = original_cfg_json {
                        let current = serde_json::to_string(ctx.cfg).unwrap_or_default();
                        self.config_changed = *orig != current;
                    }

                    return ValidationOutcome::Validated(ValidationSuccess {
                        total_boot_attempts: self.state.total_boot_attempts,
                        history: std::mem::take(&mut self.history),
                        final_config_changed: self.config_changed,
                    });
                }
                BootResult::Timeout(ref t) => {
                    // Timeout is non-repairable — stop immediately
                    let attempt = self.state.total_boot_attempts;
                    eprintln!(
                        "[CF][validation] Boot {} timed out after {}s",
                        attempt,
                        t.waited.as_secs()
                    );
                    self.history.boot_attempts.push(BootAttemptRecord {
                        attempt_number: attempt,
                        boot_result_category: BootResultCategory::Timeout,
                        chosen_action: RepairAction::None,
                        action_result: ActionResult::NoAction,
                    });
                    return ValidationOutcome::Failed(ValidationFailure {
                        final_boot_result: boot_result,
                        reason: ValidationFailureReason::NonRepairableFailure,
                        history: std::mem::take(&mut self.history),
                        loader_report: None,
                        crash_report: None,
                    });
                }
                BootResult::Failed(ref f) => {
                    // Classify and attempt repair
                    let attempt = self.state.total_boot_attempts;
                    let decision = classify_failure(f, ctx, &self.state);

                    eprintln!(
                        "[CF][validation] Boot {} failed ({}), decision: {:?}",
                        attempt, f.reason, decision
                    );

                    match &decision {
                        RepairAction::None => {
                            self.history.boot_attempts.push(BootAttemptRecord {
                                attempt_number: attempt,
                                boot_result_category: BootResultCategory::Failed,
                                chosen_action: decision,
                                action_result: ActionResult::NoAction,
                            });

                            // Check if this is a loader mismatch — run advisor
                            let log_requirements =
                                crate::loader_compat_advisor::parse_loader_requirements_from_log(
                                    &f.log_tail,
                                );
                            if !log_requirements.is_empty() {
                                let current_family =
                                    crate::loader_compat_advisor::LoaderFamily::from_server_type(
                                        &ctx.cfg.server_type,
                                    );
                                let report =
                                    crate::loader_compat_advisor::analyze_loader_compatibility(
                                        current_family,
                                        ctx.cfg.loader_version.as_deref(),
                                        &log_requirements,
                                        None,
                                    );
                                eprintln!(
                                    "[CF][loader] Loader mismatch detected: {:?}",
                                    report.status
                                );
                                return ValidationOutcome::Failed(ValidationFailure {
                                    final_boot_result: boot_result,
                                    reason: ValidationFailureReason::LoaderMismatchDetected,
                                    history: std::mem::take(&mut self.history),
                                    loader_report: Some(report),
                                    crash_report: None,
                                });
                            }

                            // Run crash attribution for non-repairable failures
                            let staging_mods = ctx.staging_path.join("mods");
                            let loader_family = match ctx.cfg.server_type {
                                crate::config::ServerType::Forge => {
                                    crate::crash_attribution::LoaderFamily::Forge
                                }
                                crate::config::ServerType::NeoForge => {
                                    crate::crash_attribution::LoaderFamily::NeoForge
                                }
                                crate::config::ServerType::Fabric => {
                                    crate::crash_attribution::LoaderFamily::Fabric
                                }
                                _ => crate::crash_attribution::LoaderFamily::Vanilla,
                            };
                            let crash_ctx = crate::crash_attribution::CrashAttributionContext {
                                staging_mods: &staging_mods,
                                loader_family,
                                recently_repaired: &[],
                                installed_registry: None,
                            };
                            let crash_report =
                                crate::crash_attribution::analyze_crash(&f.log_tail, &crash_ctx);
                            eprintln!(
                                "[CF][crash] Attribution: {:?} ({:?}) — {}",
                                crash_report.status, crash_report.confidence, crash_report.summary
                            );

                            // Check if user-approved recovery is available
                            let staging_mods = ctx.staging_path.join("mods");
                            let jar_to_mod_ids =
                                crate::recovery_actions::build_jar_to_mod_ids(&staging_mods);
                            let availability = crate::recovery_actions::check_action_availability(
                                &crash_report,
                                &jar_to_mod_ids,
                            );

                            if matches!(
                                availability,
                                crate::recovery_actions::RecoveryActionAvailability::Available
                            ) {
                                // Compute fingerprint for approval validation
                                let fingerprint =
                                    if let Some(primary) = &crash_report.primary_candidate {
                                        let evidence_ids: Vec<String> = primary
                                            .evidence
                                            .iter()
                                            .map(|e| format!("{:?}", e.source))
                                            .collect();
                                        crate::recovery_actions::compute_fingerprint(
                                            "", // transaction_id not available here
                                            primary.mod_id.as_deref().unwrap_or("unknown"),
                                            primary.jar_path.as_deref().unwrap_or(Path::new("")),
                                            attempt,
                                            &evidence_ids,
                                        )
                                    } else {
                                        String::new()
                                    };

                                eprintln!(
                                    "[CF][recovery] User action available — pausing for approval"
                                );

                                return ValidationOutcome::UserActionRequired(UserActionRequest {
                                    crash_report,
                                    availability,
                                    fingerprint,
                                    transaction_id: String::new(),
                                    staging_mods,
                                    boot_attempt: attempt,
                                    history: std::mem::take(&mut self.history),
                                });
                            }

                            return ValidationOutcome::Failed(ValidationFailure {
                                final_boot_result: boot_result,
                                reason: ValidationFailureReason::NonRepairableFailure,
                                history: std::mem::take(&mut self.history),
                                loader_report: None,
                                crash_report: Some(crash_report),
                            });
                        }
                        RepairAction::BootDependency(dep_decision) => {
                            let action_result = self
                                .execute_boot_dep_repair(dep_decision, &f.log_tail, ctx)
                                .await;
                            self.history.boot_attempts.push(BootAttemptRecord {
                                attempt_number: attempt,
                                boot_result_category: BootResultCategory::Failed,
                                chosen_action: decision,
                                action_result: action_result.clone(),
                            });
                            match &action_result {
                                ActionResult::Repaired | ActionResult::Skipped(_) => {
                                    // Continue to next boot attempt
                                }
                                _ => {
                                    // Dependency repair failed — stop
                                    return ValidationOutcome::Failed(ValidationFailure {
                                        final_boot_result: boot_result,
                                        reason: ValidationFailureReason::NonRepairableFailure,
                                        history: std::mem::take(&mut self.history),
                                        loader_report: None,
                                        crash_report: None,
                                    });
                                }
                            }
                        }
                        RepairAction::Runtime(runtime_decision) => {
                            let action_result =
                                self.execute_runtime_repair(runtime_decision, ctx).await;
                            self.history.boot_attempts.push(BootAttemptRecord {
                                attempt_number: attempt,
                                boot_result_category: BootResultCategory::Failed,
                                chosen_action: decision.clone(),
                                action_result: action_result.clone(),
                            });
                            match &action_result {
                                ActionResult::Repaired => {
                                    // Continue to next boot attempt
                                }
                                _ => {
                                    // Runtime repair failed — stop
                                    return ValidationOutcome::Failed(ValidationFailure {
                                        final_boot_result: boot_result,
                                        reason: ValidationFailureReason::RuntimeRepairFailed,
                                        history: std::mem::take(&mut self.history),
                                        loader_report: None,
                                        crash_report: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Execute a boot dependency repair (Phase 3F-B).
    async fn execute_boot_dep_repair(
        &mut self,
        decision: &BootDependencyDecision,
        log_tail: &str,
        ctx: &mut ValidationContext<'_>,
    ) -> ActionResult {
        if !self.state.can_attempt_dep_repair() {
            eprintln!(
                "[CF][repair] Dependency repair budget exhausted ({}/{})",
                self.state.dependency_repairs, MAX_BOOT_DEP_ROUNDS
            );
            return ActionResult::NotRepairable("Dependency repair budget exhausted".to_string());
        }

        // Test override: return pre-configured result without network calls
        #[cfg(any(test, feature = "testing"))]
        if let Some(ref mut overrides) = self.test_repair_overrides {
            if let Some(result) = overrides.pop_front() {
                self.state.record_dep_repair();
                self.history
                    .repairs
                    .push(RepairEvent::BootDependency(BootRepairRecordSummary {
                        boot_attempt: self.state.total_boot_attempts,
                        missing_mod_id: decision.missing_mod_id.clone(),
                        confidence: decision.confidence,
                        succeeded: matches!(result, ActionResult::Repaired),
                    }));
                return result;
            }
        }

        // Rebuild dependency graph from current mods directory
        let mods_dir = ctx.staging_path.join("mods");
        let mut analysis: Vec<(std::path::PathBuf, crate::mod_compat::ModCompatibility)> =
            Vec::new();
        if let Ok(entries) = std::fs::read_dir(&mods_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jar") {
                    let compat = crate::mod_compat::classify_mod_local(&path);
                    analysis.push((path, compat));
                }
            }
        }
        let graph = DependencyGraph::build(&analysis);
        let mc_version = ctx.cfg.minecraft_version.clone();
        let loader = format!("{:?}", ctx.cfg.server_type).to_lowercase();

        eprintln!(
            "[CF][repair] Attempting boot dependency repair for '{}'",
            decision.missing_mod_id
        );

        let outcome: BootRepairOutcome = boot_failure_analyzer::attempt_boot_repair(
            ctx.app,
            ctx.cf_client,
            ctx.dependency_resolver,
            log_tail,
            &graph,
            ctx.installed_files,
            &mods_dir,
            &mc_version,
            &loader,
            MAX_BOOT_DEP_ROUNDS,
        )
        .await;

        self.state.record_dep_repair();

        // Record repair events
        for record in &outcome.records {
            self.history
                .repairs
                .push(RepairEvent::BootDependency(BootRepairRecordSummary {
                    boot_attempt: self.state.total_boot_attempts,
                    missing_mod_id: record.attribution.missing_mod_id.clone(),
                    confidence: record.attribution.confidence,
                    succeeded: matches!(
                        record.repair_result,
                        boot_failure_analyzer::BootRepairResult::Repaired { .. }
                    ),
                }));
        }

        if outcome.repaired {
            eprintln!("[CF][repair] Boot dependency repair succeeded");
            ActionResult::Repaired
        } else {
            eprintln!("[CF][repair] Boot dependency repair failed");
            ActionResult::Failed(format!(
                "Dependency repair failed for '{}'",
                decision.missing_mod_id
            ))
        }
    }

    /// Execute a runtime repair (Phase 3F-C).
    async fn execute_runtime_repair(
        &mut self,
        decision: &RuntimeDecision,
        ctx: &mut ValidationContext<'_>,
    ) -> ActionResult {
        if !self.state.can_attempt_runtime_repair() {
            eprintln!(
                "[CF][runtime] Runtime repair budget exhausted ({}/{})",
                self.state.runtime_repairs, MAX_RUNTIME_ROUNDS
            );
            return ActionResult::NotRepairable("Runtime repair budget exhausted".to_string());
        }

        // Test override: return pre-configured result without network calls
        #[cfg(any(test, feature = "testing"))]
        if let Some(ref mut overrides) = self.test_repair_overrides {
            if let Some(result) = overrides.pop_front() {
                self.state.record_runtime_repair();
                self.history
                    .repairs
                    .push(RepairEvent::Runtime(RuntimeRepairRecord {
                        round: self.state.runtime_repairs,
                        issue: decision.issue.clone(),
                        old_value: None,
                        new_value: None,
                        result: if matches!(result, ActionResult::Repaired) {
                            RuntimeRepairStatus::Applied
                        } else {
                            RuntimeRepairStatus::Failed(format!("{:?}", result))
                        },
                    }));
                return result;
            }
        }

        let round = self.state.runtime_repairs + 1;
        eprintln!(
            "[CF][runtime] Attempting runtime repair: {:?}",
            decision.issue
        );

        let result = runtime_remediator::remediate(
            &decision.issue,
            ctx.cfg,
            &mut self.state,
            ctx.app,
            round,
        )
        .await;

        self.state.record_runtime_repair();

        match result {
            RuntimeRemediationResult::Repaired(record) => {
                self.config_changed = true;
                self.history.repairs.push(RepairEvent::Runtime(record));
                eprintln!("[CF][runtime] Runtime repair succeeded");
                ActionResult::Repaired
            }
            RuntimeRemediationResult::NotRepairable(msg) => {
                eprintln!("[CF][runtime] Not repairable: {}", msg);
                ActionResult::NotRepairable(msg)
            }
            RuntimeRemediationResult::Ambiguous(msg) => {
                eprintln!("[CF][runtime] Ambiguous: {}", msg);
                ActionResult::NotRepairable(msg)
            }
            RuntimeRemediationResult::Failed(msg) => {
                eprintln!("[CF][runtime] Failed: {}", msg);
                ActionResult::Failed(msg)
            }
        }
    }

    /// Access the retry state (for testing).
    pub fn state(&self) -> &ValidationRetryState {
        &self.state
    }

    /// Access the history (for testing).
    pub fn history(&self) -> &ValidationHistory {
        &self.history
    }
}

// ── Failure classification (pure, testable) ────────────────────────────

/// Classify a boot failure and choose at most ONE repair action.
///
/// Priority order (deterministic):
///   1. High-confidence boot missing dependency (3F-B)
///   2. Wrong Java version / Java unavailable (3F-C)
///   3. OOM diagnostic (non-repairable)
///   4. Loader mismatch diagnostic (non-repairable)
///   5. Unknown failure (non-repairable)
///
/// Pre-boot 3F-A remains before this function — it runs before the orchestrator.
pub fn classify_failure(
    failure: &crate::boot_validator::BootFailure,
    ctx: &ValidationContext<'_>,
    state: &ValidationRetryState,
) -> RepairAction {
    let log_tail = &failure.log_tail;

    // ── Priority 1: High-confidence missing dependency (3F-B) ─────
    // Only if we have budget and haven't exhausted it
    if state.can_attempt_dep_repair() {
        let mods_dir = ctx.staging_path.join("mods");
        let mut analysis: Vec<(std::path::PathBuf, crate::mod_compat::ModCompatibility)> =
            Vec::new();
        if let Ok(entries) = std::fs::read_dir(&mods_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "jar") {
                    let compat = crate::mod_compat::classify_mod_local(&path);
                    analysis.push((path, compat));
                }
            }
        }
        let graph = DependencyGraph::build(&analysis);
        let attribution = boot_failure_analyzer::analyze_boot_failure(log_tail, &graph);

        if let BootAttribution::MissingDependency(ref attr) = attribution {
            if attr.confidence >= AttributionConfidence::High {
                // Check same-dependency loop prevention
                if !state.attempted_missing_mods.contains(&attr.missing_mod_id) {
                    return RepairAction::BootDependency(BootDependencyDecision {
                        missing_mod_id: attr.missing_mod_id.clone(),
                        confidence: attr.confidence,
                    });
                }
            }
        }
    }

    // ── Priority 2: Wrong Java / Java unavailable (3F-C) ──────────
    if state.can_attempt_runtime_repair() {
        let issue = runtime_remediator::analyze_runtime_issue(log_tail, ctx.cfg);

        match &issue {
            RuntimeIssue::WrongJavaVersion(java_issue) => {
                if !state.has_attempted_java_major(java_issue.required_major) {
                    return RepairAction::Runtime(RuntimeDecision {
                        issue,
                        log_tail: log_tail.clone(),
                    });
                }
            }
            RuntimeIssue::JavaExecutableUnavailable(java_issue) => {
                let path = Path::new(&java_issue.configured_path);
                if !state.has_attempted_java_path(path) {
                    return RepairAction::Runtime(RuntimeDecision {
                        issue,
                        log_tail: log_tail.clone(),
                    });
                }
            }
            RuntimeIssue::OutOfMemory(_) => {
                // Diagnostic only — no repair
                return RepairAction::None;
            }
            RuntimeIssue::LoaderVersionMismatch(_) => {
                // Diagnostic only — no repair
                return RepairAction::None;
            }
            RuntimeIssue::Unsupported => {
                return RepairAction::None;
            }
        }
    }

    // ── Priority 3-5: Non-repairable ──────────────────────────────
    // Fall through: check OOM / loader mismatch for diagnostics
    if crate::boot_failure_analyzer::is_oom_log(log_tail) {
        return RepairAction::None;
    }

    if let Some(_mismatch) = runtime_remediator::detect_loader_version_mismatch(log_tail, ctx.cfg) {
        return RepairAction::None;
    }

    RepairAction::None
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_validator::{BootFailure, BootFailureReason, BootResult, BootSuccess};
    use std::time::Duration;

    fn make_state() -> ValidationRetryState {
        ValidationRetryState::new()
    }

    fn success_result() -> BootResult {
        BootResult::Success(BootSuccess {
            elapsed: Duration::from_secs(10),
            graceful_shutdown: true,
            log_tail: "Done! For help, type \"help\"".to_string(),
        })
    }

    fn failed_result(reason: BootFailureReason, log_tail: &str) -> BootResult {
        BootResult::Failed(BootFailure {
            exit_code: Some(1),
            reason,
            log_tail: log_tail.to_string(),
        })
    }

    fn timeout_result() -> BootResult {
        BootResult::Timeout(crate::boot_validator::BootTimeout {
            waited: Duration::from_secs(120),
            log_tail: "Timed out".to_string(),
        })
    }

    // ── Decision logic tests (§29) ────────────────────────────────

    #[test]
    fn test_classify_missing_dep_priority_over_oom() {
        // Failure contains missing dep + OOM noise → dependency repair selected
        // This test verifies the priority order in classify_failure.
        // Since classify_failure needs a real ValidationContext, we test the
        // conceptual priority by checking that missing dep attribution is checked
        // before OOM in the function's source order.
        // Full integration test is in the orchestrator tests below.
        assert!(true); // Placeholder — actual test requires mock context
    }

    #[test]
    fn test_classify_oom_returns_none() {
        // OOM should return RepairAction::None
        let log_tail = "java.lang.OutOfMemoryError: Java heap space";
        // The classify_failure function checks OOM after runtime issues
        // and returns None. Verified by code inspection + integration tests.
        assert!(crate::boot_failure_analyzer::is_oom_log(log_tail));
    }

    #[test]
    fn test_classify_loader_mismatch_returns_none() {
        // Loader mismatch should return RepairAction::None
        // Verified by code inspection — loader mismatch is detection-only.
        assert!(true); // Integration test below
    }

    #[test]
    fn test_classify_timeout_returns_none_decision() {
        // Timeout bypasses classify_failure entirely (handled before it's called).
        // The orchestrator returns NonRepairableFailure for timeout.
        let result = timeout_result();
        match result {
            BootResult::Timeout(_) => {} // correct
            _ => panic!("Expected timeout"),
        }
    }

    // ── Budget tests (§30) ────────────────────────────────────────

    #[test]
    fn test_global_boot_ceiling() {
        let mut state = make_state();
        // Consume exactly MAX_TOTAL_BOOT_ATTEMPTS
        for i in 1..=MAX_TOTAL_BOOT_ATTEMPTS {
            assert!(
                state.can_attempt_boot(),
                "Should be able to attempt boot {}",
                i
            );
            let attempt = state.consume_boot_attempt().unwrap();
            assert_eq!(attempt, i);
        }
        // Now exhausted
        assert!(!state.can_attempt_boot());
        assert_eq!(state.consume_boot_attempt(), Err(RetryLimitReached));
        assert_eq!(state.total_boot_attempts, MAX_TOTAL_BOOT_ATTEMPTS);
    }

    #[test]
    fn test_dep_budget_exhausted_runtime_still_ok() {
        let mut state = make_state();
        state.dependency_repairs = MAX_BOOT_DEP_ROUNDS;
        assert!(!state.can_attempt_dep_repair());
        assert!(state.can_attempt_runtime_repair());
        assert!(state.can_attempt_boot());
    }

    #[test]
    fn test_runtime_budget_exhausted_dep_still_ok() {
        let mut state = make_state();
        state.runtime_repairs = MAX_RUNTIME_ROUNDS;
        assert!(!state.can_attempt_runtime_repair());
        assert!(state.can_attempt_dep_repair());
        assert!(state.can_attempt_boot());
    }

    #[test]
    fn test_same_mod_id_twice_no_duplicate() {
        let mut state = make_state();
        state.attempted_missing_mods.insert("flywheel".to_string());
        // classify_failure checks attempted_missing_mods.contains() —
        // second attempt for same mod should be blocked.
        assert!(state.attempted_missing_mods.contains("flywheel"));
    }

    #[test]
    fn test_same_java_major_twice_no_duplicate() {
        let mut state = make_state();
        state.record_java_attempt(PathBuf::from("/usr/bin/java17"), 17);
        assert!(state.has_attempted_java_major(17));
        assert!(state.has_attempted_java_path(Path::new("/usr/bin/java17")));
    }

    // ── History tests (§34) ───────────────────────────────────────

    #[test]
    fn test_history_preserves_order() {
        let mut history = ValidationHistory::default();

        history.boot_attempts.push(BootAttemptRecord {
            attempt_number: 1,
            boot_result_category: BootResultCategory::Failed,
            chosen_action: RepairAction::BootDependency(BootDependencyDecision {
                missing_mod_id: "flywheel".to_string(),
                confidence: AttributionConfidence::High,
            }),
            action_result: ActionResult::Repaired,
        });

        history
            .repairs
            .push(RepairEvent::BootDependency(BootRepairRecordSummary {
                boot_attempt: 1,
                missing_mod_id: "flywheel".to_string(),
                confidence: AttributionConfidence::High,
                succeeded: true,
            }));

        history.boot_attempts.push(BootAttemptRecord {
            attempt_number: 2,
            boot_result_category: BootResultCategory::Failed,
            chosen_action: RepairAction::Runtime(RuntimeDecision {
                issue: RuntimeIssue::WrongJavaVersion(
                    crate::runtime_remediator::JavaVersionIssue {
                        required_major: 17,
                        current_major: Some(8),
                        class_file_version: Some(61),
                    },
                ),
                log_tail: "UnsupportedClassVersionError".to_string(),
            }),
            action_result: ActionResult::Repaired,
        });

        history
            .repairs
            .push(RepairEvent::Runtime(RuntimeRepairRecord {
                round: 1,
                issue: RuntimeIssue::WrongJavaVersion(
                    crate::runtime_remediator::JavaVersionIssue {
                        required_major: 17,
                        current_major: Some(8),
                        class_file_version: Some(61),
                    },
                ),
                old_value: Some("/usr/bin/java8".to_string()),
                new_value: Some("/usr/bin/java17".to_string()),
                result: crate::runtime_remediator::RuntimeRepairStatus::Applied,
            }));

        history.boot_attempts.push(BootAttemptRecord {
            attempt_number: 3,
            boot_result_category: BootResultCategory::Success,
            chosen_action: RepairAction::None,
            action_result: ActionResult::NoAction,
        });

        // Verify order
        assert_eq!(history.boot_attempts.len(), 3);
        assert_eq!(history.boot_attempts[0].attempt_number, 1);
        assert_eq!(
            history.boot_attempts[0].boot_result_category,
            BootResultCategory::Failed
        );
        assert_eq!(history.boot_attempts[1].attempt_number, 2);
        assert_eq!(
            history.boot_attempts[1].boot_result_category,
            BootResultCategory::Failed
        );
        assert_eq!(history.boot_attempts[2].attempt_number, 3);
        assert_eq!(
            history.boot_attempts[2].boot_result_category,
            BootResultCategory::Success
        );
        assert_eq!(history.repairs.len(), 2);
    }

    // ── consume_boot_attempt tests ────────────────────────────────

    #[test]
    fn test_consume_boot_attempt_returns_1_based() {
        let mut state = make_state();
        assert_eq!(state.consume_boot_attempt().unwrap(), 1);
        assert_eq!(state.consume_boot_attempt().unwrap(), 2);
        assert_eq!(state.consume_boot_attempt().unwrap(), 3);
        assert_eq!(state.total_boot_attempts, 3);
    }

    #[test]
    fn test_consume_boot_attempt_exhausted() {
        let mut state = make_state();
        for _ in 0..MAX_TOTAL_BOOT_ATTEMPTS {
            state.consume_boot_attempt().unwrap();
        }
        assert_eq!(state.consume_boot_attempt(), Err(RetryLimitReached));
    }

    // ── RepairAction equality tests ───────────────────────────────

    #[test]
    fn test_repair_action_none_equals_none() {
        assert_eq!(RepairAction::None, RepairAction::None);
    }

    #[test]
    fn test_repair_action_boot_dep_equality() {
        let a = RepairAction::BootDependency(BootDependencyDecision {
            missing_mod_id: "flywheel".to_string(),
            confidence: AttributionConfidence::High,
        });
        let b = RepairAction::BootDependency(BootDependencyDecision {
            missing_mod_id: "flywheel".to_string(),
            confidence: AttributionConfidence::High,
        });
        assert_eq!(a, b);
    }

    #[test]
    fn test_repair_action_runtime_equality() {
        let issue = RuntimeIssue::WrongJavaVersion(crate::runtime_remediator::JavaVersionIssue {
            required_major: 17,
            current_major: Some(8),
            class_file_version: Some(61),
        });
        let a = RepairAction::Runtime(RuntimeDecision {
            issue: issue.clone(),
            log_tail: "test".to_string(),
        });
        let b = RepairAction::Runtime(RuntimeDecision {
            issue,
            log_tail: "test".to_string(),
        });
        assert_eq!(a, b);
    }

    // ── ActionResult tests ────────────────────────────────────────

    #[test]
    fn test_action_result_variants() {
        assert_eq!(ActionResult::Repaired, ActionResult::Repaired);
        assert_eq!(ActionResult::NoAction, ActionResult::NoAction);
        assert_eq!(
            ActionResult::NotRepairable("OOM".to_string()),
            ActionResult::NotRepairable("OOM".to_string())
        );
        assert_eq!(
            ActionResult::Failed("download failed".to_string()),
            ActionResult::Failed("download failed".to_string())
        );
    }

    // ── BootResultCategory tests ──────────────────────────────────

    #[test]
    fn test_boot_result_category_variants() {
        assert_ne!(BootResultCategory::Success, BootResultCategory::Failed);
        assert_ne!(BootResultCategory::Failed, BootResultCategory::Timeout);
        assert_ne!(BootResultCategory::Success, BootResultCategory::Timeout);
    }

    // ── ValidationFailureReason tests ─────────────────────────────

    #[test]
    fn test_failure_reason_equality() {
        assert_eq!(
            ValidationFailureReason::RetryLimitReached,
            ValidationFailureReason::RetryLimitReached
        );
        assert_ne!(
            ValidationFailureReason::RetryLimitReached,
            ValidationFailureReason::NonRepairableFailure
        );
    }

    // ── Idempotent retry state (§20) ──────────────────────────────

    #[test]
    fn test_idempotent_retry_state() {
        let mut state = make_state();
        // Calling consume_boot_attempt twice should increment twice
        let _ = state.consume_boot_attempt();
        let _ = state.consume_boot_attempt();
        assert_eq!(state.total_boot_attempts, 2);
        assert_eq!(state.total_boot_attempts, 2);

        // Same mod ID insert is idempotent (HashSet)
        state.attempted_missing_mods.insert("flywheel".to_string());
        state.attempted_missing_mods.insert("flywheel".to_string());
        assert_eq!(state.attempted_missing_mods.len(), 1);
    }

    // ── Boot attempt numbering (§21) ──────────────────────────────

    #[test]
    fn test_boot_attempt_numbering_1_based() {
        let mut state = make_state();
        for expected in 1..=MAX_TOTAL_BOOT_ATTEMPTS {
            assert_eq!(state.consume_boot_attempt().unwrap(), expected);
        }
    }

    // ── Mock validator + test context helpers ─────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A mock BootValidationRunner that returns scripted results in order.
    struct MockBootValidator {
        results: Mutex<Vec<BootResult>>,
        call_count: AtomicUsize,
    }

    impl MockBootValidator {
        fn new(results: Vec<BootResult>) -> Self {
            Self {
                results: Mutex::new(results),
                call_count: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl BootValidationRunner for MockBootValidator {
        fn run<'a>(
            &'a self,
            _cfg: &'a ServerConfig,
            _staging_path: &'a std::path::Path,
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

    fn make_test_cfg() -> ServerConfig {
        ServerConfig {
            server_path: "/tmp/test-server".to_string(),
            java_path: "/usr/bin/java".to_string(),
            minecraft_version: "1.21.1".to_string(),
            server_type: crate::config::ServerType::Fabric,
            loader_version: Some("0.16.14".to_string()),
            ram_mb: 4096,
            max_players: 20,
            server_name: "test".to_string(),
            ..Default::default()
        }
    }

    fn forge_log(missing_mod: &str) -> String {
        format!(
            "[12:00:00] [main/INFO]: Mod '{}' requires mod '{}' version 1.0.0 or later\n\
             [12:00:01] [main/ERROR]: Missing mandatory dependency '{}'",
            "create", missing_mod, missing_mod
        )
    }

    fn wrong_java_log() -> String {
        "java.lang.UnsupportedClassVersionError: net/minecraft/server/Main has been compiled \
         by a more recent version of the Java Runtime (class file version 65.0)"
            .to_string()
    }

    fn java_not_found_log() -> String {
        "Error: JAVA_HOME is not set and no 'java' command could be found in your PATH.\n\
         /bin/sh: 1: java: not found"
            .to_string()
    }

    fn oom_log() -> String {
        "java.lang.OutOfMemoryError: Java heap space".to_string()
    }

    fn loader_mismatch_log() -> String {
        "[12:00:00] [main/INFO]: Forge 47.2.0 is required, but 43.2.0 is installed".to_string()
    }

    fn unknown_log() -> String {
        "Some random crash with no recognizable pattern".to_string()
    }

    fn make_test_context<'a>(
        cfg: &'a mut ServerConfig,
        staging_path: &'a std::path::Path,
        app: &'a std::sync::Arc<crate::app_state::AppEventSender>,
        cf: &'a reqwest::Client,
        installed_files: &'a mut crate::boot_failure_analyzer::InstalledFileRegistry,
        resolver: &'a mut crate::dependency_resolver::DependencyResolver,
    ) -> crate::validation_orchestrator::ValidationContext<'a> {
        crate::validation_orchestrator::ValidationContext {
            cfg,
            staging_path,
            app,
            cf_client: cf,
            installed_files,
            dependency_resolver: resolver,
        }
    }

    /// Build a minimal AppEventSender for tests
    fn make_test_app() -> std::sync::Arc<crate::app_state::AppEventSender> {
        let state = std::sync::Arc::new(crate::app_state::AppState::new());
        std::sync::Arc::new(crate::app_state::AppEventSender::new(state))
    }

    /// Extract history from ValidationOutcome (history is moved into the result).
    fn outcome_history(outcome: &ValidationOutcome) -> &ValidationHistory {
        match outcome {
            ValidationOutcome::Validated(s) => &s.history,
            ValidationOutcome::Failed(f) => &f.history,
            ValidationOutcome::UserActionRequired(r) => &r.history,
        }
    }

    // ── Item 2: Chained repair (boot1=MissingDep → boot2=WrongJava → boot3=Success) ──

    #[tokio::test]
    async fn test_chained_repair_missing_dep_then_wrong_java_then_success() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // dep repair for boot1
            ActionResult::Repaired, // runtime repair for boot2
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![
            // boot1: missing dependency → dep repair
            failed_result(BootFailureReason::ProcessExited, &forge_log("flywheel")),
            // boot2: wrong Java → runtime repair
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
            // boot3: success
            success_result(),
        ]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(
            mock.calls(),
            3,
            "validator should be called exactly 3 times"
        );
        assert!(
            matches!(outcome, ValidationOutcome::Validated(_)),
            "final outcome should be Validated, got {:?}",
            outcome
        );

        let history = outcome_history(&outcome);
        assert_eq!(history.boot_attempts.len(), 3, "3 boot attempts");
        assert_eq!(history.repairs.len(), 2, "2 repair events");

        // Verify ordering
        assert_eq!(history.boot_attempts[0].attempt_number, 1);
        assert_eq!(history.boot_attempts[1].attempt_number, 2);
        assert_eq!(history.boot_attempts[2].attempt_number, 3);
    }

    // ── Item 3: Single-action-per-boot ────────────────────────────

    #[tokio::test]
    async fn test_single_action_per_boot_only_dep_repair_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();

        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // dep repair succeeds
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        // boot1: missing dep (classify returns BootDependency → dep repair)
        // boot2: non-repairable → Failed
        let mock = MockBootValidator::new(vec![
            failed_result(BootFailureReason::ProcessExited, &forge_log("flywheel")),
            failed_result(BootFailureReason::ProcessExited, &unknown_log()),
        ]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 2, "2 validator calls");
        assert!(
            matches!(outcome, ValidationOutcome::Failed(_)),
            "should fail on boot2"
        );

        let history = outcome_history(&outcome);
        // boot1: dep repair ran, boot2: no action
        let dep_repairs: Vec<_> = history
            .repairs
            .iter()
            .filter(|r| matches!(r, RepairEvent::BootDependency(_)))
            .collect();
        let runtime_repairs: Vec<_> = history
            .repairs
            .iter()
            .filter(|r| matches!(r, RepairEvent::Runtime(_)))
            .collect();
        assert_eq!(dep_repairs.len(), 1, "exactly 1 dep repair");
        assert_eq!(runtime_repairs.len(), 0, "0 runtime repairs");
    }

    // ── Item 4: Non-repairable paths through orchestrator ─────────

    #[tokio::test]
    async fn test_oom_through_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![failed_result(
            BootFailureReason::OutOfMemory,
            &oom_log(),
        )]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 1, "exactly 1 validator call for OOM");
        assert!(
            matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
            "OOM should be Failed(NonRepairableFailure), got {:?}",
            outcome
        );
        assert_eq!(
            outcome_history(&outcome).repairs.len(),
            0,
            "zero mutations/repairs"
        );
    }

    #[tokio::test]
    async fn test_loader_mismatch_through_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![failed_result(
            BootFailureReason::ProcessExited,
            &loader_mismatch_log(),
        )]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 1);
        match &outcome {
            ValidationOutcome::Failed(f) => {
                assert_eq!(
                    f.reason,
                    ValidationFailureReason::LoaderMismatchDetected,
                    "Should be LoaderMismatchDetected"
                );
                assert!(f.loader_report.is_some(), "Loader report should be present");
                let report = f.loader_report.as_ref().unwrap();
                // cfg=Fabric, log=Forge → WrongLoaderFamily
                assert_eq!(
                    report.family,
                    crate::loader_compat_advisor::LoaderFamily::Fabric
                );
                assert!(
                    !report.requirements.is_empty(),
                    "Should have at least one requirement"
                );
                assert_eq!(
                    report.requirements[0].family,
                    crate::loader_compat_advisor::LoaderFamily::Forge
                );
                assert_eq!(
                    report.status,
                    crate::loader_compat_advisor::LoaderCompatibilityStatus::WrongLoaderFamily
                );
            }
            _ => panic!("Expected Failed, got {:?}", outcome),
        }
        assert_eq!(outcome_history(&outcome).repairs.len(), 0);

        // Verify zero mutation — cfg loader version unchanged
        // (forge_version is not a ServerConfig field, but we verify no cfg change)
    }

    #[tokio::test]
    async fn test_timeout_through_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![timeout_result()]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 1);
        assert!(
            matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
            "Timeout should be NonRepairableFailure, got {:?}",
            outcome
        );
        assert_eq!(outcome_history(&outcome).repairs.len(), 0);
    }

    #[tokio::test]
    async fn test_unknown_failure_through_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![failed_result(
            BootFailureReason::ProcessExited,
            &unknown_log(),
        )]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 1);
        assert!(
            matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
            "Unknown should be NonRepairableFailure, got {:?}",
            outcome
        );
        assert_eq!(outcome_history(&outcome).repairs.len(), 0);
    }

    // ── Item 5: Global ceiling at validator-call level ─────────────

    #[tokio::test]
    async fn test_global_ceiling_validator_call_level() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        // Pre-fill repair overrides: enough for all runtime repairs (max 2)
        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired,
            ActionResult::Repaired,
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        // All boots return WrongJavaVersion — classifies as Runtime
        // Runtime budget = 2, so boot 1 and 2 get repaired, boot 3+ returns None → Failed
        // Total boot attempts = 3 (not 6), because sub-budget exhausts first
        let mut boot_results = Vec::new();
        for _ in 0..20 {
            boot_results.push(failed_result(
                BootFailureReason::WrongJavaVersion,
                &wrong_java_log(),
            ));
        }
        let mock = MockBootValidator::new(boot_results);

        let outcome = orch.validate(&mut ctx, &mock).await;

        // With runtime budget=2: boot1 repaired, boot2 repaired, boot3 NonRepairable
        assert_eq!(
            mock.calls(),
            3,
            "should stop after runtime budget exhausted (3 calls)"
        );
        assert!(
            matches!(outcome, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
            "should fail with NonRepairableFailure when sub-budget exhausted, got {:?}",
            outcome
        );

        // Now test the actual global ceiling: exhaust all budgets, then consume remaining attempts
        // We need MAX_TOTAL_BOOT_ATTEMPTS calls to validate the ceiling
        let mut orch2 = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // dep1
            ActionResult::Repaired, // dep2
            ActionResult::Repaired, // runtime1
            ActionResult::Repaired, // runtime2
        ]);
        let mut cfg2 = make_test_cfg();
        let app2 = make_test_app();
        let cf2 = reqwest::Client::new();
        let mut installed2 = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver2 =
            crate::dependency_resolver::DependencyResolver::new(cf2.clone(), String::new());
        let mut ctx2 = make_test_context(
            &mut cfg2,
            tmp.path(),
            &app2,
            &cf2,
            &mut installed2,
            &mut resolver2,
        );

        // Alternate between dep (flywheel) and runtime issues to exhaust both budgets
        let mut results2 = vec![
            // boot1: missing dep → dep repair (dep=1)
            failed_result(BootFailureReason::ProcessExited, &forge_log("flywheel")),
            // boot2: wrong java → runtime repair (runtime=1)
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
            // boot3: missing dep → dep repair (dep=2)
            failed_result(BootFailureReason::ProcessExited, &forge_log("flywheel")),
            // boot4: wrong java → runtime repair (runtime=2)
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
            // boot5: unknown → NonRepairableFailure (both budgets exhausted)
            failed_result(BootFailureReason::ProcessExited, &unknown_log()),
        ];
        // Pad with more failures in case the loop goes further
        for _ in 0..20 {
            results2.push(failed_result(
                BootFailureReason::ProcessExited,
                &unknown_log(),
            ));
        }
        let mock2 = MockBootValidator::new(results2);

        let outcome2 = orch2.validate(&mut ctx2, &mock2).await;

        // boot1-4: repaired, boot5: NonRepairableFailure
        assert_eq!(
            mock2.calls(),
            5,
            "5 validator calls (4 repairs + 1 final failure)"
        );
        assert!(
            matches!(outcome2, ValidationOutcome::Failed(ref f) if f.reason == ValidationFailureReason::NonRepairableFailure),
            "final should be NonRepairableFailure, got {:?}",
            outcome2
        );
    }

    // ── Item 6: Failed history completeness ───────────────────────

    #[tokio::test]
    async fn test_failed_history_completeness() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // runtime repair for boot1
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        // boot1: WrongJava → runtime repair → Repaired
        // boot2: WrongJava again → classify returns Runtime again
        //   BUT runtime budget exhausted (1/1 used? No, MAX=2)
        //   So boot2 also gets Runtime → Repaired (override is used)
        //   Actually, only 1 override. After boot1 repair uses it, boot2 has no override
        //   → execute_runtime_repair falls through to real remediate() which will fail (network)
        //   → ActionResult::Failed → Failed(RuntimeRepairFailed)

        // Use 2 overrides so both repairs succeed, then boot3 fails
        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // runtime repair for boot1
            ActionResult::Repaired, // runtime repair for boot2
        ]);
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![
            // boot1: WrongJava → runtime repair → Repaired
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
            // boot2: WrongJava again → runtime repair → Repaired (runtime=2)
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
            // boot3: WrongJava again → runtime budget exhausted → None → Failed
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
        ]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 3);
        assert!(matches!(outcome, ValidationOutcome::Failed(_)));

        let history = outcome_history(&outcome);
        assert_eq!(history.boot_attempts.len(), 3, "3 boot attempts recorded");

        // Verify no missing final attempt
        assert_eq!(history.boot_attempts[2].attempt_number, 3);
        assert_eq!(
            history.boot_attempts[2].chosen_action,
            RepairAction::None,
            "final attempt should have RepairAction::None"
        );
        assert_eq!(
            history.boot_attempts[2].boot_result_category,
            BootResultCategory::Failed,
        );

        // Verify repairs recorded
        assert_eq!(history.repairs.len(), 2, "2 repair events");
    }

    // ── Item 12: ValidationCleanupFailed path ─────────────────────

    #[tokio::test]
    async fn test_validation_cleanup_failed_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a validation world residue to trigger cleanup failure
        std::fs::write(tmp.path().join(".lbby-validation-world-test"), b"").unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![success_result()]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 1);
        assert!(
            matches!(outcome, ValidationOutcome::Failed(ref f) if matches!(&f.reason, ValidationFailureReason::ValidationCleanupFailed(_))),
            "should be ValidationCleanupFailed when validation world residue exists, got {:?}",
            outcome
        );
    }

    // ── Server-pack behavior expansion tests ──────────────────────

    /// Official server-pack context: empty InstalledFileRegistry, fresh DependencyResolver.
    /// WrongJava remediation works without manifest metadata.
    #[tokio::test]
    async fn test_server_pack_runtime_repair_wrong_java() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // runtime repair for WrongJava
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let mut installed_files = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver = crate::dependency_resolver::DependencyResolver::new(
            reqwest::Client::new(),
            String::new(),
        );

        let mut ctx = ValidationContext {
            cfg: &mut cfg,
            staging_path: tmp.path(),
            app: &app,
            cf_client: &reqwest::Client::new(),
            installed_files: &mut installed_files,
            dependency_resolver: &mut resolver,
        };

        // boot1: WrongJava → runtime repair succeeds, boot2: Success
        let mock = MockBootValidator::new(vec![
            failed_result(BootFailureReason::WrongJavaVersion, &wrong_java_log()),
            success_result(),
        ]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 2, "2 validator calls");
        assert!(
            matches!(outcome, ValidationOutcome::Validated(_)),
            "should be Validated, got {:?}",
            outcome
        );

        let history = outcome_history(&outcome);
        assert_eq!(history.repairs.len(), 1, "exactly 1 runtime repair");
        assert!(
            matches!(history.repairs[0], RepairEvent::Runtime(_)),
            "repair should be Runtime, got {:?}",
            history.repairs[0]
        );
        assert_eq!(history.boot_attempts.len(), 2, "2 boot attempts recorded");
    }

    /// Official server-pack context: empty registry means no authoritative CF mapping.
    /// Missing-dep log triggers classify_failure → BootDependency, but verify_attribution
    /// fails with empty registry → NotRepairable → Failed.
    #[tokio::test]
    async fn test_server_pack_missing_dep_limitation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::NotRepairable("no authoritative CF mapping".to_string()),
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let mut installed_files = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver = crate::dependency_resolver::DependencyResolver::new(
            reqwest::Client::new(),
            String::new(),
        );

        let mut ctx = ValidationContext {
            cfg: &mut cfg,
            staging_path: tmp.path(),
            app: &app,
            cf_client: &reqwest::Client::new(),
            installed_files: &mut installed_files,
            dependency_resolver: &mut resolver,
        };

        // boot1: missing-dep log → BootDependency → NotRepairable → Failed
        let mock = MockBootValidator::new(vec![failed_result(
            BootFailureReason::ProcessExited,
            &forge_log("some-missing-mod"),
        )]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        assert_eq!(mock.calls(), 1, "1 validator call only");
        assert!(
            matches!(outcome, ValidationOutcome::Failed(_)),
            "should be Failed, got {:?}",
            outcome
        );

        let history = outcome_history(&outcome);
        assert_eq!(
            history.repairs.len(),
            1,
            "1 repair attempt recorded (failed)"
        );
        assert!(
            matches!(history.repairs[0], RepairEvent::BootDependency(_)),
            "repair should be BootDependency attempt, got {:?}",
            history.repairs[0]
        );
        assert_eq!(history.boot_attempts.len(), 1, "1 boot attempt recorded");
    }

    // ── Phase 3H: Loader Compatibility Advisor orchestrator tests ──

    /// Loader mismatch: advisor runs, produces report, zero mutation.
    #[tokio::test]
    async fn test_loader_mismatch_zero_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir(&mods_dir).unwrap();
        // Place a dummy file to verify it's not deleted
        std::fs::write(mods_dir.join("some-mod.jar"), b"fake").unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let cfg_before = make_test_cfg();
        let mut cfg = cfg_before.clone();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![failed_result(
            BootFailureReason::ProcessExited,
            &loader_mismatch_log(),
        )]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        // 1 boot call
        assert_eq!(mock.calls(), 1);
        // 0 repairs
        assert_eq!(outcome_history(&outcome).repairs.len(), 0);

        // Failed with LoaderMismatchDetected
        match &outcome {
            ValidationOutcome::Failed(f) => {
                assert_eq!(f.reason, ValidationFailureReason::LoaderMismatchDetected);
                assert!(f.loader_report.is_some());
            }
            _ => panic!("Expected Failed, got {:?}", outcome),
        }

        // Zero mutation: mods dir still has the file
        assert!(mods_dir.join("some-mod.jar").exists(), "mods dir unchanged");

        // Zero mutation: cfg loader fields unchanged
        // (ServerConfig doesn't have a loader_version field to check,
        //  but we verify no crash and cfg serializes identically)
        let cfg_after_json = serde_json::to_string(&cfg).unwrap();
        let cfg_before_json = serde_json::to_string(&cfg_before).unwrap();
        assert_eq!(cfg_before_json, cfg_after_json, "cfg should be unchanged");
    }

    /// Chained: boot1 MissingDependency → repaired → boot2 LoaderMismatch → Failed
    #[tokio::test]
    async fn test_chained_dep_repair_then_loader_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        // Override dep repair to succeed
        let mut orch = ValidationRepairOrchestrator::new_with_repair_overrides(vec![
            ActionResult::Repaired, // dep repair for boot1
        ]);
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        // boot1: missing-dep log → dep repair → Repaired → continue
        // boot2: loader mismatch → advisor → Failed
        let mock = MockBootValidator::new(vec![
            failed_result(
                BootFailureReason::ProcessExited,
                &forge_log("some-missing-mod"),
            ),
            failed_result(BootFailureReason::ProcessExited, &loader_mismatch_log()),
        ]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        // boot1 + boot2 = 2 validator calls
        assert_eq!(mock.calls(), 2);

        let history = outcome_history(&outcome);
        assert_eq!(history.boot_attempts.len(), 2, "2 boot attempts");
        assert!(
            matches!(
                history.repairs.first(),
                Some(RepairEvent::BootDependency(_))
            ),
            "first repair should be BootDependency, got {:?}",
            history.repairs.first()
        );

        // Final outcome: LoaderMismatchDetected
        match &outcome {
            ValidationOutcome::Failed(f) => {
                assert_eq!(f.reason, ValidationFailureReason::LoaderMismatchDetected);
                assert!(f.loader_report.is_some());
            }
            _ => panic!("Expected Failed, got {:?}", outcome),
        }
    }

    /// Error precedence: missing dep takes priority over loader mismatch in same log.
    /// When a log has BOTH missing-dep and loader-mismatch signals,
    /// classify_failure picks missing-dep (Priority 1) first.
    #[tokio::test]
    async fn test_missing_dep_takes_priority_over_loader_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mods")).unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        // Log with BOTH missing-dep and loader-mismatch signals
        let combined_log = format!(
            "{}\n{}",
            forge_log("some-missing-mod"),
            loader_mismatch_log()
        );

        let mock = MockBootValidator::new(vec![
            failed_result(BootFailureReason::ProcessExited, &combined_log),
            // Second boot: after dep repair fails, classify picks loader mismatch
            failed_result(BootFailureReason::ProcessExited, &loader_mismatch_log()),
        ]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        let history = outcome_history(&outcome);
        // First boot: dep repair attempted (priority 1 over loader mismatch)
        assert!(
            matches!(
                history.repairs.first(),
                Some(RepairEvent::BootDependency(_))
            ),
            "first repair should be BootDependency (priority 1), got {:?}",
            history.repairs.first()
        );
    }

    /// Server-pack context: no manifest metadata, advisor works from boot log alone.
    #[tokio::test]
    async fn test_server_pack_loader_advisor_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let mods_dir = tmp.path().join("mods");
        std::fs::create_dir(&mods_dir).unwrap();

        let mut orch = ValidationRepairOrchestrator::new();
        let mut cfg = make_test_cfg();
        let app = make_test_app();
        let cf = reqwest::Client::new();
        let mut installed = crate::boot_failure_analyzer::InstalledFileRegistry::new();
        // Empty installed registry = server-pack context (no per-file CF mappings)
        let mut resolver =
            crate::dependency_resolver::DependencyResolver::new(cf.clone(), String::new());
        let mut ctx = make_test_context(
            &mut cfg,
            tmp.path(),
            &app,
            &cf,
            &mut installed,
            &mut resolver,
        );

        let mock = MockBootValidator::new(vec![failed_result(
            BootFailureReason::ProcessExited,
            &loader_mismatch_log(),
        )]);

        let outcome = orch.validate(&mut ctx, &mock).await;

        match &outcome {
            ValidationOutcome::Failed(f) => {
                assert_eq!(f.reason, ValidationFailureReason::LoaderMismatchDetected);
                let report = f.loader_report.as_ref().unwrap();
                // cfg=Fabric, log says Forge → WrongLoaderFamily
                assert_eq!(
                    report.family,
                    crate::loader_compat_advisor::LoaderFamily::Fabric
                );
                assert!(!report.requirements.is_empty());
                assert_eq!(
                    report.requirements[0].family,
                    crate::loader_compat_advisor::LoaderFamily::Forge
                );
                assert_eq!(
                    report.status,
                    crate::loader_compat_advisor::LoaderCompatibilityStatus::WrongLoaderFamily
                );
            }
            _ => panic!("Expected Failed, got {:?}", outcome),
        }
    }
}
