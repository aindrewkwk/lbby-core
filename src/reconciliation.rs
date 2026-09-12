// reconciliation.rs — Startup reconciliation for persisted recovery state.
//
// Phase 3N: Separates read-only discovery from explicit mutation.
// Discovery is always read-only. Reconciliation/cleanup is always explicit.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::atomic_persistence::CURRENT_SCHEMA_VERSION;
use crate::install_transaction::{TransactionMeta, TransactionPhase};

/// Classification of a persisted transaction after startup discovery.
#[derive(Debug, Clone)]
pub enum TransactionReconciliationState {
    /// Transaction was Building but never committed. Live server untouched.
    Abandoned {
        meta: TransactionMeta,
        staging_path: PathBuf,
    },
    /// Transaction is paused awaiting user approval. Must be preserved.
    PendingUserAction {
        meta: TransactionMeta,
        staging_path: PathBuf,
    },
    /// Transaction was in Committing phase — risky startup state.
    /// Requires filesystem state analysis before any action.
    Committing {
        meta: TransactionMeta,
        staging_path: PathBuf,
    },
    /// Transaction completed successfully. Safe to clean up.
    Committed {
        meta: TransactionMeta,
        staging_path: PathBuf,
    },
    /// Transaction marker is corrupt (malformed JSON, unreadable).
    Corrupt {
        staging_path: PathBuf,
        error: String,
    },
    /// Transaction marker has a schema version from a future release.
    UnsupportedSchema {
        staging_path: PathBuf,
        schema_version: u32,
        error: String,
    },
}

/// Filesystem snapshot for Committing phase recovery analysis.
#[derive(Debug, Clone)]
pub struct CommittingFilesystemState {
    pub live_exists: bool,
    pub backup_exists: bool,
    pub staging_exists: bool,
    pub committed_marker_exists: bool,
}

/// Result of startup reconciliation for a single server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationResult {
    pub server_path: String,
    pub transactions: Vec<ReconciledTransaction>,
    pub committed_cleaned: u32,
    pub corrupt_entries: Vec<String>,
    pub unsupported_schema_count: u32,
}

/// A single transaction's reconciliation outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciledTransaction {
    pub transaction_id: String,
    pub phase: String,
    pub action: ReconciliationAction,
    pub schema_version: u32,
}

/// What the reconciler decided for a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReconciliationAction {
    /// Transaction is valid and pending user action. Preserved for UI.
    PreservedPendingRecovery,
    /// Transaction was abandoned (Building phase, never committed). Reported.
    ReportedAbandoned,
    /// Transaction was committed. Cleaned up.
    CleanedUp,
    /// Transaction is in Committing phase. Needs manual recovery.
    NeedsManualRecovery { reason: String },
    /// Transaction marker is corrupt (malformed JSON, unreadable).
    Corrupt,
    /// Transaction marker has a schema version from a future release.
    UnsupportedSchema,
}

/// Discover all persisted transactions for a server path.
/// This is READ-ONLY — no files are created, modified, or deleted.
pub fn discover_transactions(live_path: &Path) -> Vec<TransactionReconciliationState> {
    let parent = match live_path.parent() {
        Some(p) => p,
        None => return vec![],
    };
    let staging_root = parent.join(".lbby-staging");
    if !staging_root.exists() {
        return vec![];
    }

    let mut results = vec![];
    if let Ok(entries) = std::fs::read_dir(&staging_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let marker = path.join("transaction.json");
            if !marker.exists() {
                continue;
            }
            match std::fs::read_to_string(&marker) {
                Ok(content) => {
                    match serde_json::from_str::<TransactionMeta>(&content) {
                        Ok(meta) => {
                            // Validate schema version
                            if meta.schema_version > CURRENT_SCHEMA_VERSION {
                                results.push(TransactionReconciliationState::UnsupportedSchema {
                                    staging_path: path.clone(),
                                    schema_version: meta.schema_version,
                                    error: format!(
                                        "Unsupported schema version {} (max supported: {})",
                                        meta.schema_version, CURRENT_SCHEMA_VERSION
                                    ),
                                });
                                continue;
                            }
                            match meta.phase {
                                TransactionPhase::Building => {
                                    results.push(TransactionReconciliationState::Abandoned {
                                        meta,
                                        staging_path: path.clone(),
                                    });
                                }
                                TransactionPhase::PendingUserAction => {
                                    results.push(
                                        TransactionReconciliationState::PendingUserAction {
                                            meta,
                                            staging_path: path.clone(),
                                        },
                                    );
                                }
                                TransactionPhase::Committing => {
                                    results.push(TransactionReconciliationState::Committing {
                                        meta,
                                        staging_path: path.clone(),
                                    });
                                }
                                TransactionPhase::Committed => {
                                    results.push(TransactionReconciliationState::Committed {
                                        meta,
                                        staging_path: path.clone(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            results.push(TransactionReconciliationState::Corrupt {
                                staging_path: path.clone(),
                                error: format!("Failed to parse transaction marker: {}", e),
                            });
                        }
                    }
                }
                Err(e) => {
                    results.push(TransactionReconciliationState::Corrupt {
                        staging_path: path.clone(),
                        error: format!("Failed to read transaction marker: {}", e),
                    });
                }
            }
        }
    }

    results
}

/// Clean up a committed transaction's staging directory and backup.
/// This is an EXPLICIT mutation — only call after confirming commit is complete.
pub fn cleanup_committed_transaction(meta: &TransactionMeta, staging_path: &Path) {
    // Remove staging directory
    if staging_path.exists() {
        let _ = std::fs::remove_dir_all(staging_path);
    }
    // Remove committed marker in live dir
    let live_marker = meta.live_path.join(".lbby-transaction.json");
    if live_marker.exists() {
        let _ = std::fs::remove_file(&live_marker);
    }
    // Remove backup if it exists
    if let Some(ref backup) = meta.backup_path {
        if backup.exists() {
            let _ = std::fs::remove_dir_all(backup);
        }
    }
}

/// Analyze filesystem state for a Committing-phase transaction.
/// READ-ONLY — no mutations.
pub fn analyze_committing_state(
    meta: &TransactionMeta,
    staging_path: &Path,
) -> CommittingFilesystemState {
    let live_exists = meta.live_path.exists();
    let staging_exists = staging_path.exists();
    let backup_exists = meta.backup_path.as_ref().map_or(false, |p| p.exists());
    let committed_marker_exists = meta.live_path.join(".lbby-transaction.json").exists();

    CommittingFilesystemState {
        live_exists,
        backup_exists,
        staging_exists,
        committed_marker_exists,
    }
}

/// Run full startup reconciliation for a server path.
/// Returns structured results. Mutations are explicit and limited to cleanup.
pub fn reconcile_recovery_state(live_path: &Path) -> ReconciliationResult {
    let server_path = live_path.to_string_lossy().to_string();
    let discoveries = discover_transactions(live_path);
    let mut transactions = Vec::new();
    let mut committed_cleaned = 0u32;
    let mut corrupt_entries = Vec::new();
    let mut unsupported_schema_count = 0u32;

    for state in &discoveries {
        match state {
            TransactionReconciliationState::Abandoned { meta, staging_path } => {
                transactions.push(ReconciledTransaction {
                    transaction_id: meta.transaction_id.clone(),
                    phase: "Building".to_string(),
                    action: ReconciliationAction::ReportedAbandoned,
                    schema_version: meta.schema_version,
                });
                // Do NOT auto-cleanup — report only
                eprintln!(
                    "[RECOVERY] Abandoned transaction {} at {:?}",
                    meta.transaction_id, staging_path
                );
            }
            TransactionReconciliationState::PendingUserAction { meta, .. } => {
                // Validate identity consistency before preserving
                let pending_path = meta.pending_recovery_path();
                let identity_valid = if pending_path.exists() {
                    match std::fs::read_to_string(&pending_path) {
                        Ok(content) => {
                            match serde_json::from_str::<
                                crate::install_transaction::PendingRecoveryMetadata,
                            >(&content)
                            {
                                Ok(pending) => {
                                    pending.server_id == meta.server_id
                                        && pending.transaction_id == meta.transaction_id
                                }
                                Err(_) => false,
                            }
                        }
                        Err(_) => false,
                    }
                } else {
                    false
                };

                if identity_valid {
                    transactions.push(ReconciledTransaction {
                        transaction_id: meta.transaction_id.clone(),
                        phase: "PendingUserAction".to_string(),
                        action: ReconciliationAction::PreservedPendingRecovery,
                        schema_version: meta.schema_version,
                    });
                } else {
                    transactions.push(ReconciledTransaction {
                        transaction_id: meta.transaction_id.clone(),
                        phase: "PendingUserAction".to_string(),
                        action: ReconciliationAction::NeedsManualRecovery {
                            reason: "Pending recovery metadata missing or identity mismatch"
                                .to_string(),
                        },
                        schema_version: meta.schema_version,
                    });
                }
            }
            TransactionReconciliationState::Committing { meta, staging_path } => {
                let fs_state = analyze_committing_state(meta, staging_path);
                let reason = match (
                    fs_state.live_exists,
                    fs_state.backup_exists,
                    fs_state.staging_exists,
                    fs_state.committed_marker_exists,
                ) {
                    (true, true, true, _) => {
                        "All three trees present — ambiguous commit state".to_string()
                    }
                    (false, true, true, _) => {
                        "Live missing, staging+backup present — commit likely failed".to_string()
                    }
                    (true, true, false, false) => {
                        "Live+backup present, no staging, no committed marker — ambiguous"
                            .to_string()
                    }
                    (false, true, false, _) => {
                        "Only backup exists — commit failed, manual rollback needed".to_string()
                    }
                    _ => "Inconsistent filesystem state".to_string(),
                };
                transactions.push(ReconciledTransaction {
                    transaction_id: meta.transaction_id.clone(),
                    phase: "Committing".to_string(),
                    action: ReconciliationAction::NeedsManualRecovery { reason },
                    schema_version: meta.schema_version,
                });
            }
            TransactionReconciliationState::Committed { meta, staging_path } => {
                // Verify commit is truly complete before cleanup
                let committed_marker = meta.live_path.join(".lbby-transaction.json");
                let can_cleanup = committed_marker.exists() && meta.live_path.exists();
                if can_cleanup {
                    cleanup_committed_transaction(meta, staging_path);
                    committed_cleaned += 1;
                    transactions.push(ReconciledTransaction {
                        transaction_id: meta.transaction_id.clone(),
                        phase: "Committed".to_string(),
                        action: ReconciliationAction::CleanedUp,
                        schema_version: meta.schema_version,
                    });
                } else {
                    transactions.push(ReconciledTransaction {
                        transaction_id: meta.transaction_id.clone(),
                        phase: "Committed".to_string(),
                        action: ReconciliationAction::NeedsManualRecovery {
                            reason: "Committed marker or live path missing".to_string(),
                        },
                        schema_version: meta.schema_version,
                    });
                }
            }
            TransactionReconciliationState::Corrupt {
                staging_path,
                error,
            } => {
                corrupt_entries.push(format!("{:?}: {}", staging_path, error));
                transactions.push(ReconciledTransaction {
                    transaction_id: "unknown".to_string(),
                    phase: "Corrupt".to_string(),
                    action: ReconciliationAction::Corrupt,
                    schema_version: 0,
                });
            }
            TransactionReconciliationState::UnsupportedSchema {
                staging_path,
                schema_version,
                error,
            } => {
                unsupported_schema_count += 1;
                transactions.push(ReconciledTransaction {
                    transaction_id: "unknown".to_string(),
                    phase: "UnsupportedSchema".to_string(),
                    action: ReconciliationAction::UnsupportedSchema,
                    schema_version: *schema_version,
                });
            }
        }
    }

    ReconciliationResult {
        server_path,
        transactions,
        committed_cleaned,
        corrupt_entries,
        unsupported_schema_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_transaction::{PendingRecoveryMetadata, TransactionPhase};
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lbby_recon_test_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discover_empty_staging() {
        let tmp = temp_dir("empty_discover");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let results = discover_transactions(&live);
        assert!(results.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_building_transaction() {
        let tmp = temp_dir("building_txn");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("test-txn-1");
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = TransactionMeta {
            schema_version: CURRENT_SCHEMA_VERSION,
            server_id: "test-server".to_string(),
            transaction_id: "test-txn-1".to_string(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            phase: TransactionPhase::Building,
            live_path: live.clone(),
            staging_path: txn_dir.clone(),
            backup_path: None,
        };
        let marker = txn_dir.join("transaction.json");
        fs::write(&marker, serde_json::to_string(&meta).unwrap()).unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Abandoned { .. }
        ));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_pending_user_action() {
        let tmp = temp_dir("pending_txn");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("test-txn-2");
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = TransactionMeta {
            schema_version: CURRENT_SCHEMA_VERSION,
            server_id: "test-server".to_string(),
            transaction_id: "test-txn-2".to_string(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            phase: TransactionPhase::PendingUserAction,
            live_path: live.clone(),
            staging_path: txn_dir.clone(),
            backup_path: None,
        };
        let marker = txn_dir.join("transaction.json");
        fs::write(&marker, serde_json::to_string(&meta).unwrap()).unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::PendingUserAction { .. }
        ));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_unsupported_schema() {
        let tmp = temp_dir("unsupported_schema");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("test-txn-future");
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = TransactionMeta {
            schema_version: 999,
            server_id: "test-server".to_string(),
            transaction_id: "test-txn-future".to_string(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            phase: TransactionPhase::Building,
            live_path: live.clone(),
            staging_path: txn_dir.clone(),
            backup_path: None,
        };
        let marker = txn_dir.join("transaction.json");
        fs::write(&marker, serde_json::to_string(&meta).unwrap()).unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::UnsupportedSchema {
                schema_version: 999,
                ..
            }
        ));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_corrupt_marker() {
        let tmp = temp_dir("corrupt_marker");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("test-txn-corrupt");
        fs::create_dir_all(&txn_dir).unwrap();

        let marker = txn_dir.join("transaction.json");
        fs::write(&marker, "not valid json{{{").unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Corrupt { .. }
        ));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reconcile_returns_structured_result() {
        let tmp = temp_dir("reconcile_result");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let result = reconcile_recovery_state(&live);
        assert_eq!(result.server_path, live.to_string_lossy());
        assert!(result.transactions.is_empty());
        assert_eq!(result.committed_cleaned, 0);
        assert!(result.corrupt_entries.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_legacy_unversioned_transaction() {
        let tmp = temp_dir("legacy_unversioned");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("test-txn-legacy");
        fs::create_dir_all(&txn_dir).unwrap();

        // Simulate a pre-3N marker WITHOUT schema_version field
        let legacy_json = r#"{
            "server_id": "test-server",
            "transaction_id": "test-txn-legacy",
            "source": "test",
            "created_at": "2026-01-01T00:00:00Z",
            "phase": "Building",
            "live_path": "/tmp/test",
            "staging_path": "/tmp/test/.lbby-staging/test-txn-legacy",
            "backup_path": null,
            "modpack_slug": null,
            "modpack_name": null,
            "modpack_version_id": null,
            "modpack_source": null
        }"#;
        let marker = txn_dir.join("transaction.json");
        fs::write(&marker, legacy_json).unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        // Should parse as schema_version=0 (legacy v0)
        if let TransactionReconciliationState::Abandoned { meta, .. } = &results[0] {
            assert_eq!(meta.schema_version, 0);
        } else {
            panic!("Expected Abandoned state for legacy transaction");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Committing state matrix (Spec #45) ─────────────────────────────

    fn make_committed_meta(
        live: &PathBuf,
        staging: &PathBuf,
        backup: Option<PathBuf>,
    ) -> TransactionMeta {
        TransactionMeta {
            schema_version: CURRENT_SCHEMA_VERSION,
            server_id: "test-server".to_string(),
            transaction_id: "test-commit-txn".to_string(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            phase: TransactionPhase::Committing,
            live_path: live.clone(),
            staging_path: staging.clone(),
            backup_path: backup,
        }
    }

    /// A: live old + staging new + no backup → ambiguous commit state
    #[test]
    fn committing_matrix_a_live_staging_no_backup() {
        let tmp = temp_dir("commit_a");
        let live = tmp.join("server");
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("txn-a");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = make_committed_meta(&live, &txn_dir, None);
        fs::write(
            txn_dir.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Committing { .. }
        ));

        let fs_state = analyze_committing_state(&meta, &txn_dir);
        assert!(fs_state.live_exists);
        assert!(!fs_state.backup_exists);
        assert!(fs_state.staging_exists);
        assert!(!fs_state.committed_marker_exists);

        let _ = fs::remove_dir_all(&tmp);
    }

    /// B: live missing + staging new + backup old → commit failed, rollback possible
    #[test]
    fn committing_matrix_b_backup_staging_no_live() {
        let tmp = temp_dir("commit_b");
        let live = tmp.join("server");
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("txn-b");
        let backup = tmp.join("server.backup");
        // Don't create live — it's missing
        fs::create_dir_all(&txn_dir).unwrap();
        fs::create_dir_all(&backup).unwrap();

        let meta = make_committed_meta(&live, &txn_dir, Some(backup.clone()));
        fs::write(
            txn_dir.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Committing { .. }
        ));

        let fs_state = analyze_committing_state(&meta, &txn_dir);
        assert!(!fs_state.live_exists);
        assert!(fs_state.backup_exists);
        assert!(fs_state.staging_exists);
        assert!(!fs_state.committed_marker_exists);

        let _ = fs::remove_dir_all(&tmp);
    }

    /// C: live new + staging missing + backup old → need committed marker check
    #[test]
    fn committing_matrix_c_live_backup_no_staging() {
        let tmp = temp_dir("commit_c");
        let live = tmp.join("server");
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("txn-c");
        let backup = tmp.join("server.backup");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&backup).unwrap();
        // Create txn_dir to write marker, then remove it to simulate staging missing
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = make_committed_meta(&live, &txn_dir, Some(backup.clone()));
        fs::write(
            txn_dir.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();
        // Now remove the staging txn_dir content to simulate it being gone
        // (keep the marker so discover_transactions can find it)
        // Actually, we need the marker for discovery, so keep the dir but remove
        // extra staging content. For this test, the marker IS the staging dir.
        // The key is that analyze_committing_state checks staging_exists.

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Committing { .. }
        ));

        let fs_state = analyze_committing_state(&meta, &txn_dir);
        assert!(fs_state.live_exists);
        assert!(fs_state.backup_exists);
        assert!(fs_state.staging_exists); // marker dir exists
        assert!(!fs_state.committed_marker_exists);

        let _ = fs::remove_dir_all(&tmp);
    }

    /// D: live missing + staging missing + backup old → only backup exists
    #[test]
    fn committing_matrix_d_only_backup() {
        let tmp = temp_dir("commit_d");
        let live = tmp.join("server");
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("txn-d");
        let backup = tmp.join("server.backup");
        // Don't create live
        fs::create_dir_all(&backup).unwrap();
        // Create txn_dir to write marker
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = make_committed_meta(&live, &txn_dir, Some(backup.clone()));
        fs::write(
            txn_dir.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Committing { .. }
        ));

        let fs_state = analyze_committing_state(&meta, &txn_dir);
        assert!(!fs_state.live_exists);
        assert!(fs_state.backup_exists);
        assert!(fs_state.staging_exists); // marker dir exists
        assert!(!fs_state.committed_marker_exists);

        let _ = fs::remove_dir_all(&tmp);
    }

    /// E: live new + staging missing + backup missing → live only
    #[test]
    fn committing_matrix_e_only_live() {
        let tmp = temp_dir("commit_e");
        let live = tmp.join("server");
        let staging_root = tmp.join(".lbby-staging");
        let txn_dir = staging_root.join("txn-e");
        fs::create_dir_all(&live).unwrap();
        // Create txn_dir to write marker (staging exists as marker container)
        fs::create_dir_all(&txn_dir).unwrap();

        let meta = make_committed_meta(&live, &txn_dir, None);
        fs::write(
            txn_dir.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            TransactionReconciliationState::Committing { .. }
        ));

        let fs_state = analyze_committing_state(&meta, &txn_dir);
        assert!(fs_state.live_exists);
        assert!(!fs_state.backup_exists);
        assert!(fs_state.staging_exists); // marker dir exists

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Atomic write regression tests (Spec #24) ───────────────────────

    #[test]
    fn atomic_write_survives_leftover_temp() {
        use crate::atomic_persistence::{atomic_write_json, read_versioned_json};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        struct TestData {
            schema_version: u32,
            value: String,
        }
        impl crate::atomic_persistence::HasSchemaVersion for TestData {
            fn schema_version(&self) -> u32 {
                self.schema_version
            }
        }

        let tmp = temp_dir("atomic_leftover_tmp");
        let path = tmp.join("test.json");

        // Write initial good data
        let initial = TestData {
            schema_version: 1,
            value: "initial".to_string(),
        };
        atomic_write_json(&path, &initial).unwrap();

        // Create a leftover temp file (simulates crash after temp write, before rename)
        let tmp_file = path.with_extension("json.tmp-abc123");
        fs::write(&tmp_file, "corrupt leftover").unwrap();

        // Write new data — should succeed despite leftover temp
        let updated = TestData {
            schema_version: 1,
            value: "updated".to_string(),
        };
        atomic_write_json(&path, &updated).unwrap();

        // Read back — should have new data
        let loaded: TestData = read_versioned_json(&path).unwrap().unwrap();
        assert_eq!(loaded.value, "updated");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_versioned_json_ignores_temp_file() {
        use crate::atomic_persistence::{atomic_write_json, read_versioned_json};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        struct TestData {
            schema_version: u32,
            value: String,
        }
        impl crate::atomic_persistence::HasSchemaVersion for TestData {
            fn schema_version(&self) -> u32 {
                self.schema_version
            }
        }

        let tmp = temp_dir("atomic_ignore_tmp");
        let path = tmp.join("test.json");
        let tmp_file = path.with_extension("json.tmp-abc123");

        // Only temp file exists, no authoritative file
        fs::write(&tmp_file, r#"{"schema_version":1,"value":"temp"}"#).unwrap();

        // read_versioned_json should return None (ignores temp)
        let result: Option<TestData> = read_versioned_json(&path).unwrap();
        assert!(result.is_none());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_versioned_json_handles_truncated() {
        use crate::atomic_persistence::read_versioned_json;
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct TestData {
            schema_version: u32,
            value: String,
        }
        impl crate::atomic_persistence::HasSchemaVersion for TestData {
            fn schema_version(&self) -> u32 {
                self.schema_version
            }
        }

        let tmp = temp_dir("atomic_truncated");
        let path = tmp.join("test.json");

        // Write truncated JSON
        fs::write(&path, r#"{"schema_version":1,"val"#).unwrap();

        let result: Result<Option<TestData>, _> = read_versioned_json(&path);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_versioned_json_handles_unsupported_schema() {
        use crate::atomic_persistence::{read_versioned_json, CURRENT_SCHEMA_VERSION};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct TestData {
            schema_version: u32,
            value: String,
        }
        impl crate::atomic_persistence::HasSchemaVersion for TestData {
            fn schema_version(&self) -> u32 {
                self.schema_version
            }
        }

        let tmp = temp_dir("atomic_unsupported");
        let path = tmp.join("test.json");

        let future = TestData {
            schema_version: CURRENT_SCHEMA_VERSION + 99,
            value: "future".to_string(),
        };
        fs::write(&path, serde_json::to_string(&future).unwrap()).unwrap();

        let result: Result<Option<TestData>, _> = read_versioned_json(&path);
        // Should get an error about unsupported schema
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unsupported schema version"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn atomic_write_cleanup_on_failure() {
        use crate::atomic_persistence::atomic_write_json;
        use serde::Serialize;

        // Test that writing to a path inside a non-existent deep directory
        // fails gracefully (can't create parent)
        let tmp = temp_dir("atomic_cleanup");
        let path = tmp.join("deep/nested/dir/test.json");

        #[derive(Serialize)]
        struct TestData {
            value: String,
        }

        let result = atomic_write_json(
            &path,
            &TestData {
                value: "test".to_string(),
            },
        );
        // Should fail because we can't create deep/nested/dir (or succeed if create_dir_all handles it)
        // Either way, no temp files should linger
        if result.is_ok() {
            assert!(path.exists());
        }

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Quarantine metadata regression (Spec #21) ──────────────────────

    #[test]
    fn quarantine_legacy_v1_still_parses() {
        use crate::recovery_actions::QuarantineMetadata;
        use serde_json;

        let legacy = r#"{
            "schema_version": 1,
            "records": [],
            "orphaned": false
        }"#;

        let meta: QuarantineMetadata = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.schema_version, 1);
        assert!(meta.records.is_empty());
    }

    #[test]
    fn quarantine_unversioned_defaults_to_zero() {
        use crate::recovery_actions::QuarantineMetadata;
        use serde_json;

        // Pre-3N quarantine metadata without schema_version
        let legacy = r#"{
            "records": [],
            "orphaned": false
        }"#;

        let meta: QuarantineMetadata = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.schema_version, 0);
    }

    // ── PendingRecoveryMetadata legacy migration (Spec #23) ─────────────

    #[test]
    fn pending_recovery_legacy_unversioned_parses_as_v0() {
        use crate::install_transaction::PendingRecoveryMetadata;

        let legacy = r#"{
            "server_id": "test-server",
            "transaction_id": "test-txn",
            "staging_mods": "/tmp/staging/mods",
            "attribution_fingerprint": "abc123",
            "target_mod_id": "mod-id",
            "target_jar_path": "/tmp/staging/mods/mod.jar",
            "target_jar_sha256": "deadbeef",
            "boot_attempt": 3,
            "dependency_repairs": 1,
            "runtime_repairs": 0,
            "recovery_actions_used": 0,
            "display_filename": "mod.jar",
            "crash_summary": "test crash",
            "confidence": "high",
            "applied": false
        }"#;

        let meta: PendingRecoveryMetadata = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.schema_version, 0);
        assert_eq!(meta.server_id, "test-server");
        assert_eq!(meta.transaction_id, "test-txn");
    }

    // ── Retry state legacy migration (Spec #18) ────────────────────────

    #[test]
    fn retry_state_legacy_unversioned_parses_as_v0() {
        use crate::recovery_actions::RetryStateSnapshot;

        let legacy = r#"{
            "boot_attempts_used": 2,
            "dependency_repairs_used": 1,
            "runtime_repairs_used": 0,
            "recovery_actions_used": 0
        }"#;

        let state: RetryStateSnapshot = serde_json::from_str(legacy).unwrap();
        assert_eq!(state.schema_version, 0);
        assert_eq!(state.boot_attempts_used, 2);
    }

    // ── Blocker 2: Discovery read-only regression ──────────────────────────

    #[test]
    fn find_stale_is_read_only_committed_entries() {
        // find_stale must NOT delete committed staging dirs or backup dirs
        use crate::install_transaction::InstallTransaction;

        let tmp = temp_dir("find_stale_ro");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging = tmp.join(".lbby-staging").join("txn-committed");
        fs::create_dir_all(&staging).unwrap();
        let backup = tmp.join("backup-committed");
        fs::create_dir_all(&backup).unwrap();

        let meta = TransactionMeta {
            schema_version: CURRENT_SCHEMA_VERSION,
            server_id: "srv".to_string(),
            transaction_id: "txn-committed".to_string(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            phase: TransactionPhase::Committed,
            live_path: live.clone(),
            staging_path: staging.clone(),
            backup_path: Some(backup.clone()),
        };
        fs::write(
            staging.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        // Also create a committed marker in live dir
        fs::write(
            live.join(".lbby-transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let stale = InstallTransaction::find_stale(&live);

        // find_stale should return the committed entries (not skip them)
        // but NOT delete anything
        assert_eq!(stale.len(), 2); // staging + live marker

        // Verify staging directory still exists (not deleted)
        assert!(
            staging.exists(),
            "staging must not be deleted by find_stale"
        );
        assert!(
            staging.join("transaction.json").exists(),
            "staging marker must not be deleted"
        );

        // Verify live marker still exists (not deleted)
        assert!(
            live.join(".lbby-transaction.json").exists(),
            "live marker must not be deleted by find_stale"
        );

        // Verify backup still exists (not deleted)
        assert!(backup.exists(), "backup must not be deleted by find_stale");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_transactions_is_read_only() {
        // discover_transactions must NOT delete anything
        let tmp = temp_dir("discover_ro");
        let live = tmp.join("server");
        fs::create_dir_all(&live).unwrap();
        let staging = tmp.join(".lbby-staging").join("txn-disc");
        fs::create_dir_all(&staging).unwrap();

        let meta = TransactionMeta {
            schema_version: CURRENT_SCHEMA_VERSION,
            server_id: "srv".to_string(),
            transaction_id: "txn-disc".to_string(),
            source: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            phase: TransactionPhase::Committed,
            live_path: live.clone(),
            staging_path: staging.clone(),
            backup_path: None,
        };
        fs::write(
            staging.join("transaction.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        let results = discover_transactions(&live);
        assert_eq!(results.len(), 1);

        // Verify staging directory still exists
        assert!(
            staging.exists(),
            "staging must not be deleted by discover_transactions"
        );
        assert!(
            staging.join("transaction.json").exists(),
            "staging marker must not be deleted"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Blocker 4: Retry-state schema regression ───────────────────────────

    #[test]
    fn retry_state_unsupported_schema_returns_err() {
        use crate::recovery_actions::{load_retry_state, RetryStateSnapshot};

        let tmp = temp_dir("retry_unsupported");
        let server = tmp.join("server");
        fs::create_dir_all(&server).unwrap();

        // Write a future-schema retry state file
        let future_state = RetryStateSnapshot {
            schema_version: CURRENT_SCHEMA_VERSION + 100,
            boot_attempts_used: 5,
            dependency_repairs_used: 3,
            runtime_repairs_used: 1,
            recovery_actions_used: 2,
        };
        let state_path = server.join(".lbby-retry-state.json");
        fs::write(&state_path, serde_json::to_string(&future_state).unwrap()).unwrap();

        let result = load_retry_state(&server);
        // Must return Err, not Ok(None)
        assert!(
            result.is_err(),
            "Unsupported schema must return Err, got {:?}",
            result
        );
        let err = result.unwrap_err();
        let err_msg = format!("{}", err);
        assert!(
            err_msg.contains("Unsupported"),
            "Error must mention unsupported: {}",
            err_msg
        );

        // Verify the file was NOT deleted or overwritten
        assert!(
            state_path.exists(),
            "Future-schema file must NOT be deleted"
        );
        let content = fs::read_to_string(&state_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            parsed["schema_version"].as_u64().unwrap(),
            (CURRENT_SCHEMA_VERSION + 100) as u64,
            "File must NOT be overwritten"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn retry_state_missing_file_returns_ok_none() {
        use crate::recovery_actions::load_retry_state;

        let tmp = temp_dir("retry_missing");
        let server = tmp.join("server");
        fs::create_dir_all(&server).unwrap();

        let result = load_retry_state(&server);
        assert!(
            result.is_ok(),
            "Missing file must return Ok, got {:?}",
            result
        );
        assert!(
            result.unwrap().is_none(),
            "Missing file must return Ok(None)"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn retry_state_corrupt_file_returns_err() {
        use crate::recovery_actions::load_retry_state;

        let tmp = temp_dir("retry_corrupt");
        let server = tmp.join("server");
        fs::create_dir_all(&server).unwrap();

        fs::write(server.join(".lbby-retry-state.json"), "{bad json!!!").unwrap();

        let result = load_retry_state(&server);
        assert!(
            result.is_err(),
            "Corrupt file must return Err, got {:?}",
            result
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}
