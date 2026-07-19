//! Structured error types for safety-critical operations.
//!
//! These errors map to stable frontend error codes and human-readable messages.
//! They are used by the safety-critical paths: restore, import, auto-restart,
//! operation coordination, and path ownership.

use std::fmt;
use std::path::PathBuf;

/// Structured error for safety-critical operations.
#[derive(Debug, Clone)]
pub enum SafetyError {
    /// Server is currently running and the requested operation cannot proceed.
    ServerRunning { status: String, operation: String },
    /// Server is starting and the requested operation cannot proceed.
    ServerStarting { operation: String },
    /// A restart is pending and the requested operation cannot proceed.
    RestartPending {
        operation: String,
        profile_id: String,
    },
    /// A conflicting operation is already in progress.
    ConflictingOperation { current: String, requested: String },
    /// An unsafe archive entry was detected.
    UnsafeArchiveEntry { entry_path: String, reason: String },
    /// The archive is invalid or corrupted.
    InvalidArchive { reason: String },
    /// Restore validation failed before modifying live data.
    RestoreValidationFailed { reason: String },
    /// Restore rollback failed — original data may be partially restored.
    RestoreRollbackFailed {
        reason: String,
        rollback_path: Option<PathBuf>,
    },
    /// Access to the Playit secret is forbidden in production.
    SecretAccessForbidden,
    /// A path violates ownership boundaries.
    PathOwnershipViolation { path: PathBuf, reason: String },
    /// The operation was cancelled.
    OperationCancelled { operation: String, reason: String },
    /// Crash loop detected — too many restarts.
    CrashLoopBlocked { attempts: usize, window_secs: u64 },
}

impl fmt::Display for SafetyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SafetyError::ServerRunning { status, operation } => {
                write!(f, "Cannot {} while server is {}. Stop the server and wait for shutdown to complete.", operation, status)
            }
            SafetyError::ServerStarting { operation } => {
                write!(
                    f,
                    "Cannot {} while server is starting. Wait for startup to complete or fail.",
                    operation
                )
            }
            SafetyError::RestartPending {
                operation,
                profile_id,
            } => {
                write!(f, "Cannot {} while a restart is pending for profile '{}'. Cancel the restart first.", operation, profile_id)
            }
            SafetyError::ConflictingOperation { current, requested } => {
                write!(
                    f,
                    "Cannot {} while {} is in progress. Wait for it to complete.",
                    requested, current
                )
            }
            SafetyError::UnsafeArchiveEntry { entry_path, reason } => {
                write!(f, "Unsafe archive entry '{}': {}", entry_path, reason)
            }
            SafetyError::InvalidArchive { reason } => {
                write!(f, "Invalid archive: {}", reason)
            }
            SafetyError::RestoreValidationFailed { reason } => {
                write!(f, "Restore validation failed: {}", reason)
            }
            SafetyError::RestoreRollbackFailed {
                reason,
                rollback_path,
            } => {
                if let Some(path) = rollback_path {
                    write!(f, "Restore rollback failed at '{}': {}. Original data may be partially restored.", path.display(), reason)
                } else {
                    write!(
                        f,
                        "Restore rollback failed: {}. Original data may be partially restored.",
                        reason
                    )
                }
            }
            SafetyError::SecretAccessForbidden => {
                write!(
                    f,
                    "Access to raw secrets is not allowed. Use safe diagnostic endpoints instead."
                )
            }
            SafetyError::PathOwnershipViolation { path, reason } => {
                write!(
                    f,
                    "Path ownership violation at '{}': {}",
                    path.display(),
                    reason
                )
            }
            SafetyError::OperationCancelled { operation, reason } => {
                write!(f, "Operation '{}' cancelled: {}", operation, reason)
            }
            SafetyError::CrashLoopBlocked {
                attempts,
                window_secs,
            } => {
                write!(f, "Auto-restart disabled — server crashed {}+ times in {} seconds. Fix the issue and start manually.", attempts, window_secs)
            }
        }
    }
}

impl std::error::Error for SafetyError {}

/// Stable error codes for frontend consumption.
impl SafetyError {
    pub fn error_code(&self) -> &'static str {
        match self {
            SafetyError::ServerRunning { .. } => "SERVER_RUNNING",
            SafetyError::ServerStarting { .. } => "SERVER_STARTING",
            SafetyError::RestartPending { .. } => "RESTART_PENDING",
            SafetyError::ConflictingOperation { .. } => "CONFLICTING_OPERATION",
            SafetyError::UnsafeArchiveEntry { .. } => "UNSAFE_ARCHIVE_ENTRY",
            SafetyError::InvalidArchive { .. } => "INVALID_ARCHIVE",
            SafetyError::RestoreValidationFailed { .. } => "RESTORE_VALIDATION_FAILED",
            SafetyError::RestoreRollbackFailed { .. } => "RESTORE_ROLLBACK_FAILED",
            SafetyError::SecretAccessForbidden => "SECRET_ACCESS_FORBIDDEN",
            SafetyError::PathOwnershipViolation { .. } => "PATH_OWNERSHIP_VIOLATION",
            SafetyError::OperationCancelled { .. } => "OPERATION_CANCELLED",
            SafetyError::CrashLoopBlocked { .. } => "CRASH_LOOP_BLOCKED",
        }
    }

    /// Convert to a JSON-friendly Value for IPC.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.error_code(),
            "message": self.to_string(),
        })
    }
}

/// Convert SafetyError to String for Tauri command return types.
impl From<SafetyError> for String {
    fn from(err: SafetyError) -> String {
        err.to_string()
    }
}
