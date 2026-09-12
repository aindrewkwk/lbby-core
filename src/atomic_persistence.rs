// atomic_persistence.rs — Crash-safe JSON persistence helper for recovery state.
//
// Phase 3N: All authoritative recovery/install metadata uses atomic writes
// to prevent partial overwrites from corrupting persisted state.
//
// Semantics:
//   serialize → write sibling .tmp file → flush → atomic rename → target
//   On failure, .tmp is cleaned up (best-effort) and target is untouched.

use serde::Serialize;
use std::io::Write;
use std::path::Path;

/// Current schema version for Phase 3N persistence.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Test-only seam: when set to `true`, `atomic_write_json` will always fail.
/// This lets tests deterministically trigger persistence failures without
/// needing to mock the filesystem.
#[cfg(test)]
static FORCE_ATOMIC_WRITE_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Guard that forces `atomic_write_json` to fail while alive.
/// Automatically resets on drop. Scoped for safety.
#[cfg(test)]
pub struct ForceWriteFailureGuard(());

#[cfg(test)]
impl ForceWriteFailureGuard {
    pub fn new() -> Self {
        FORCE_ATOMIC_WRITE_FAILURE.store(true, std::sync::atomic::Ordering::SeqCst);
        Self(())
    }
}

#[cfg(test)]
impl Drop for ForceWriteFailureGuard {
    fn drop(&mut self) {
        FORCE_ATOMIC_WRITE_FAILURE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Atomic JSON write: serialize → write to `.tmp` sibling → fsync → rename → target.
///
/// Guarantees:
/// - Target file is never partially overwritten (rename is atomic within same directory)
/// - `.tmp` file is never authoritative (readers ignore it)
/// - On failure, `.tmp` is cleaned up best-effort and target is untouched
/// - File is flushed before rename for durability
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    // Test-only failure seam
    #[cfg(test)]
    if FORCE_ATOMIC_WRITE_FAILURE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("forced write failure (test seam)".to_string());
    }

    let json =
        serde_json::to_string_pretty(value).map_err(|e| format!("Failed to serialize: {}", e))?;

    let tmp_path = path.with_extension("json.tmp");

    // Write to temp file
    let mut file = std::fs::File::create(&tmp_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("Failed to create temp file: {}", e)
    })?;

    file.write_all(json.as_bytes()).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("Failed to write temp file: {}", e)
    })?;

    // Flush to disk before rename
    file.flush().map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("Failed to flush temp file: {}", e)
    })?;

    // Sync to disk for durability (best-effort on all platforms)
    let _ = file.sync_all();

    // Atomic rename: temp → target
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("Failed to rename temp → target: {}", e)
    })?;

    Ok(())
}

/// Read and parse JSON from disk.
///
/// Returns Ok(None) if file does not exist (not an error — legacy/pre-install).
/// Returns Err if file exists but is corrupt or unreadable.
pub fn read_json_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let value: T = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    Ok(Some(value))
}

/// Check if a path is a temporary atomic-write file.
pub fn is_atomic_tmp_file(path: &Path) -> bool {
    path.extension().map_or(false, |ext| ext == "tmp")
        && path
            .file_stem()
            .map_or(false, |stem| stem.to_string_lossy().ends_with(".json"))
}

/// A trait for types that carry a schema_version field.
pub trait HasSchemaVersion {
    fn schema_version(&self) -> u32;
}

/// Read and parse versioned JSON from disk with schema validation.
///
/// Returns Ok(None) if file does not exist.
/// Returns Err if file is corrupt, unreadable, or has unsupported schema version.
/// Temp files (.json.tmp) are ignored — they are never authoritative.
pub fn read_versioned_json<T>(path: &Path) -> Result<Option<T>, String>
where
    T: serde::de::DeserializeOwned + HasSchemaVersion,
{
    // Ignore temp files
    if is_atomic_tmp_file(path) {
        return Ok(None);
    }
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let value: T = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    // Validate schema version
    if value.schema_version() > CURRENT_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported schema version {} (max supported: {}) in {}",
            value.schema_version(),
            CURRENT_SCHEMA_VERSION,
            path.display()
        ));
    }

    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestRecord {
        name: String,
        value: u32,
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");
        let record = TestRecord {
            name: "hello".into(),
            value: 42,
        };

        atomic_write_json(&path, &record).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: TestRecord = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn atomic_write_no_tmp_left_behind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");
        let record = TestRecord {
            name: "hello".into(),
            value: 42,
        };

        atomic_write_json(&path, &record).unwrap();

        let tmp_path = path.with_extension("json.tmp");
        assert!(
            !tmp_path.exists(),
            "tmp file should not exist after successful write"
        );
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");

        let record1 = TestRecord {
            name: "first".into(),
            value: 1,
        };
        atomic_write_json(&path, &record1).unwrap();

        let record2 = TestRecord {
            name: "second".into(),
            value: 2,
        };
        atomic_write_json(&path, &record2).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: TestRecord = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed, record2);
    }

    #[test]
    fn read_json_file_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");

        let result: Result<Option<TestRecord>, String> = read_json_file(&path);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn read_json_file_corrupt_returns_err() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, "not valid json {{{").unwrap();

        let result: Result<Option<TestRecord>, String> = read_json_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn is_atomic_tmp_file_recognizes_pattern() {
        assert!(is_atomic_tmp_file(Path::new("transaction.json.tmp")));
        assert!(is_atomic_tmp_file(Path::new(
            "/some/dir/pending_recovery.json.tmp"
        )));
        assert!(!is_atomic_tmp_file(Path::new("transaction.json")));
        assert!(!is_atomic_tmp_file(Path::new("some_file.tmp")));
        assert!(!is_atomic_tmp_file(Path::new("quarantine_metadata.json")));
    }

    #[test]
    fn legacy_unversioned_json_parses_with_default_schema() {
        // Simulate a pre-3N JSON without schema_version
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.json");
        std::fs::write(&path, r#"{"name":"legacy","value":99}"#).unwrap();

        let result: Option<TestRecord> = read_json_file(&path).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "legacy");
    }
}
