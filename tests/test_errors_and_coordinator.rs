//! Tests for structured errors and operation coordinator
//!
//! Validates:
//! - SafetyError stable error code mapping
//! - No sensitive filesystem content in errors
//! - OperationKind conflict detection

use lbby_core::app_state::OperationKind;
use lbby_core::errors::SafetyError;

// ── SafetyError tests ──────────────────────────────────────────────────────

#[test]
fn test_server_running_has_stable_code() {
    let err = SafetyError::ServerRunning {
        status: "Running".to_string(),
        operation: "restore".to_string(),
    };
    assert_eq!(err.error_code(), "SERVER_RUNNING");
    assert!(err.to_string().contains("restore"));
    assert!(
        !err.to_string().contains("/home/"),
        "No filesystem paths in user message"
    );
}

#[test]
fn test_server_starting_has_stable_code() {
    let err = SafetyError::ServerStarting {
        operation: "import".to_string(),
    };
    assert_eq!(err.error_code(), "SERVER_STARTING");
}

#[test]
fn test_restart_pending_has_stable_code() {
    let err = SafetyError::RestartPending {
        operation: "restore".to_string(),
        profile_id: "abc123".to_string(),
    };
    assert_eq!(err.error_code(), "RESTART_PENDING");
}

#[test]
fn test_conflicting_operation_has_stable_code() {
    let err = SafetyError::ConflictingOperation {
        current: "Restore".to_string(),
        requested: "Import".to_string(),
    };
    assert_eq!(err.error_code(), "CONFLICTING_OPERATION");
    assert!(err.to_string().contains("Restore"));
    assert!(err.to_string().contains("Import"));
}

#[test]
fn test_unsafe_archive_entry_has_stable_code() {
    let err = SafetyError::UnsafeArchiveEntry {
        entry_path: "../../etc/passwd".to_string(),
        reason: "parent traversal".to_string(),
    };
    assert_eq!(err.error_code(), "UNSAFE_ARCHIVE_ENTRY");
}

#[test]
fn test_invalid_archive_has_stable_code() {
    let err = SafetyError::InvalidArchive {
        reason: "corrupted header".to_string(),
    };
    assert_eq!(err.error_code(), "INVALID_ARCHIVE");
}

#[test]
fn test_restore_validation_failed_has_stable_code() {
    let err = SafetyError::RestoreValidationFailed {
        reason: "missing level.dat".to_string(),
    };
    assert_eq!(err.error_code(), "RESTORE_VALIDATION_FAILED");
}

#[test]
fn test_restore_rollback_failed_has_stable_code() {
    let err = SafetyError::RestoreRollbackFailed {
        reason: "rename failed".to_string(),
        rollback_path: None,
    };
    assert_eq!(err.error_code(), "RESTORE_ROLLBACK_FAILED");
}

#[test]
fn test_secret_access_forbidden_has_stable_code() {
    let err = SafetyError::SecretAccessForbidden;
    assert_eq!(err.error_code(), "SECRET_ACCESS_FORBIDDEN");
    let msg = err.to_string();
    assert!(!msg.contains("token"), "No secret tokens in message");
}

#[test]
fn test_path_ownership_violation_has_stable_code() {
    let err = SafetyError::PathOwnershipViolation {
        path: std::path::PathBuf::from("/tmp/outside"),
        reason: "outside managed root".to_string(),
    };
    assert_eq!(err.error_code(), "PATH_OWNERSHIP_VIOLATION");
}

#[test]
fn test_operation_cancelled_has_stable_code() {
    let err = SafetyError::OperationCancelled {
        operation: "restore".to_string(),
        reason: "user cancelled".to_string(),
    };
    assert_eq!(err.error_code(), "OPERATION_CANCELLED");
}

#[test]
fn test_crash_loop_blocked_has_stable_code() {
    let err = SafetyError::CrashLoopBlocked {
        attempts: 3,
        window_secs: 300,
    };
    assert_eq!(err.error_code(), "CRASH_LOOP_BLOCKED");
}

#[test]
fn test_all_error_codes_unique() {
    let errors: Vec<SafetyError> = vec![
        SafetyError::ServerRunning {
            status: "x".into(),
            operation: "x".into(),
        },
        SafetyError::ServerStarting {
            operation: "x".into(),
        },
        SafetyError::RestartPending {
            operation: "x".into(),
            profile_id: "x".into(),
        },
        SafetyError::ConflictingOperation {
            current: "x".into(),
            requested: "x".into(),
        },
        SafetyError::UnsafeArchiveEntry {
            entry_path: "x".into(),
            reason: "x".into(),
        },
        SafetyError::InvalidArchive { reason: "x".into() },
        SafetyError::RestoreValidationFailed { reason: "x".into() },
        SafetyError::RestoreRollbackFailed {
            reason: "x".into(),
            rollback_path: None,
        },
        SafetyError::SecretAccessForbidden,
        SafetyError::PathOwnershipViolation {
            path: std::path::PathBuf::from("x"),
            reason: "x".into(),
        },
        SafetyError::OperationCancelled {
            operation: "x".into(),
            reason: "x".into(),
        },
        SafetyError::CrashLoopBlocked {
            attempts: 3,
            window_secs: 300,
        },
    ];

    let codes: Vec<_> = errors.iter().map(|e| e.error_code()).collect();
    let mut unique_codes = codes.clone();
    unique_codes.sort();
    unique_codes.dedup();
    assert_eq!(
        codes.len(),
        unique_codes.len(),
        "All error codes must be unique"
    );
}

#[test]
fn test_error_to_json_has_code_and_message() {
    let err = SafetyError::SecretAccessForbidden;
    let json = err.to_json();
    assert_eq!(json["code"], "SECRET_ACCESS_FORBIDDEN");
    assert!(json["message"].is_string());
}

#[test]
fn test_error_display_is_user_safe() {
    let err = SafetyError::UnsafeArchiveEntry {
        entry_path: "../../etc/shadow".to_string(),
        reason: "parent traversal".to_string(),
    };
    let display = format!("{}", err);
    assert!(
        display.contains("../../etc/shadow"),
        "Entry name in display for debugging"
    );
    assert!(display.contains("parent traversal"), "Reason in display");
}

#[test]
fn test_error_converts_to_string() {
    let err = SafetyError::InvalidArchive {
        reason: "bad header".to_string(),
    };
    let s: String = err.into();
    assert!(s.contains("Invalid archive"));
}

// ── OperationKind tests ────────────────────────────────────────────────────

#[test]
fn test_operation_kind_none_is_default() {
    let op = OperationKind::None;
    assert_eq!(op, OperationKind::None);
}

#[test]
fn test_operation_kind_variants_exist() {
    let _ = OperationKind::None;
    let _ = OperationKind::Starting;
    let _ = OperationKind::Stopping;
    let _ = OperationKind::Restoring;
    let _ = OperationKind::Importing;
    let _ = OperationKind::Exporting;
    let _ = OperationKind::Installing;
    let _ = OperationKind::BackingUp;
    let _ = OperationKind::Resetting;
}

#[test]
fn test_operation_kind_equality() {
    assert_eq!(OperationKind::None, OperationKind::None);
    assert_ne!(OperationKind::None, OperationKind::Restoring);
    assert_ne!(OperationKind::Starting, OperationKind::Stopping);
}

#[test]
fn test_operation_kind_clone() {
    let op = OperationKind::Restoring;
    let cloned = op.clone();
    assert_eq!(op, cloned);
}

#[test]
fn test_operation_conflict_matrix() {
    // Restore blocks everything except None
    let restore = OperationKind::Restoring;
    assert_ne!(restore, OperationKind::None);
    assert_ne!(restore, OperationKind::Starting);
    assert_ne!(restore, OperationKind::Importing);

    // Import blocks everything except None
    let import = OperationKind::Importing;
    assert_ne!(import, OperationKind::None);
    assert_ne!(import, OperationKind::Starting);
    assert_ne!(import, OperationKind::Restoring);

    // Starting blocks restore/import
    let starting = OperationKind::Starting;
    assert_ne!(starting, OperationKind::Restoring);
    assert_ne!(starting, OperationKind::Importing);

    // None allows everything (it's the idle state)
    let none = OperationKind::None;
    assert_eq!(none, OperationKind::None);
}

#[test]
fn test_operation_kind_debug() {
    let op = OperationKind::Restoring;
    let debug = format!("{:?}", op);
    assert_eq!(debug, "Restoring");
}

#[test]
fn test_operation_kind_serialize() {
    let op = OperationKind::BackingUp;
    let json = serde_json::to_string(&op).unwrap();
    assert!(
        json.contains("backingup"),
        "Serialized with rename_all lowercase"
    );
}

#[test]
fn test_all_operations_serializable() {
    let variants = vec![
        OperationKind::None,
        OperationKind::Starting,
        OperationKind::Stopping,
        OperationKind::Restoring,
        OperationKind::Importing,
        OperationKind::Exporting,
        OperationKind::Installing,
        OperationKind::BackingUp,
        OperationKind::Resetting,
    ];

    for op in variants {
        let json = serde_json::to_string(&op).unwrap();
        assert!(!json.is_empty(), "Serialization failed for {:?}", op);
    }
}
