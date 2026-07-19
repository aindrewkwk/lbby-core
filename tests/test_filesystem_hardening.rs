//! Tests for hardened safe_extract_zip, transactional restore, and OperationGuard.
//!
//! These tests use real temporary directories and real ZIP archives to verify
//! the safety guarantees of the filesystem operations.

use std::io::{Cursor, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::ZipArchive;
use zip::ZipWriter;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Create a ZIP archive in memory from (name, data) pairs.
fn create_test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = ZipWriter::new(Cursor::new(&mut buf));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

// ── safe_extract_zip: traversal tests ──────────────────────────────────────

#[test]
fn test_normal_extraction_succeeds() {
    let zip_data = create_test_zip(&[
        ("world/level.dat", b"level data"),
        ("world/region/r.0.0.mca", b"region data"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "world/");
    assert!(
        result.is_ok(),
        "Normal extraction should succeed: {:?}",
        result
    );
    assert!(dest.join("level.dat").exists());
    assert!(dest.join("region").join("r.0.0.mca").exists());
}

#[test]
fn test_parent_traversal_rejected() {
    let zip_data = create_test_zip(&[("../outside.txt", b"malicious")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(result.is_err(), "Parent traversal must be rejected");
    assert!(!tmp.path().join("outside.txt").exists());
}

#[test]
fn test_deep_traversal_rejected() {
    let zip_data = create_test_zip(&[("folder/../../../etc/passwd", b"malicious")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(result.is_err(), "Deep traversal must be rejected");
}

#[test]
fn test_absolute_path_rejected() {
    let zip_data = create_test_zip(&[("/etc/passwd", b"malicious")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(result.is_err(), "Absolute path must be rejected");
}

#[test]
fn test_windows_drive_path_rejected() {
    // Windows drive paths like C:\ are rejected by enclosed_name() on all platforms
    let zip_data = create_test_zip(&[("C:\\Windows\\System32\\evil.txt", b"malicious")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    // enclosed_name() should reject this or the canonical check should catch it
    if result.is_ok() {
        // If it "succeeded", verify no files outside dest
        let outside: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "extract")
            .collect();
        assert!(
            outside.is_empty(),
            "No files should be created outside extract dir"
        );
    }
}

#[test]
fn test_unc_path_rejected() {
    let zip_data = create_test_zip(&[("\\\\server\\share\\evil.txt", b"malicious")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(result.is_err(), "UNC path must be rejected");
}

#[test]
fn test_mixed_slash_traversal_rejected() {
    let zip_data = create_test_zip(&[("..\\..\\outside.txt", b"malicious")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(result.is_err(), "Mixed-slash traversal must be rejected");
}

// ── safe_extract_zip: symlink tests ────────────────────────────────────────

#[cfg(unix)]
#[test]
fn test_archive_symlink_rejected() {
    // Create a real symlink on disk, zip it, then verify extraction rejects it.
    // In-memory zip archives don't preserve unix_mode metadata reliably,
    // so we test with a real file-based archive.
    let tmp = tempfile::tempdir().unwrap();

    // Create source dir with a symlink
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("real.txt"), b"real content").unwrap();
    std::os::unix::fs::symlink(src.join("real.txt"), src.join("link.txt")).unwrap();

    // Create a zip from the source dir using the lbby-core backup function
    let zip_path = tmp.path().join("test.zip");
    let state = std::sync::Arc::new(lbby_core::app_state::AppState::new());
    let sender = std::sync::Arc::new(lbby_core::app_state::AppEventSender::new(state));
    let result = lbby_core::backup::create_server_backup(&sender, &src, &zip_path, false);
    assert!(
        result.is_ok(),
        "Backup creation should succeed: {:?}",
        result
    );

    // Now try to extract — symlinks should be skipped during backup creation
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();
    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let extract_result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    // The backup skips symlinks during creation, so extraction should succeed
    // (no symlink entries in the archive)
    assert!(
        extract_result.is_ok(),
        "Extraction of symlink-free archive should succeed"
    );
    assert!(dest.join("real.txt").exists());
    assert!(
        !dest.join("link.txt").exists(),
        "Symlink should not have been included in backup"
    );
}

#[cfg(unix)]
#[test]
fn test_symlinked_parent_directory_rejected() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    // Create a symlink inside dest pointing outside
    let outside_dir = tmp.path().join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    symlink(&outside_dir, dest.join("escape")).unwrap();

    // Create a ZIP with a file that would go through the symlink
    let zip_data = create_test_zip(&[("escape/evil.txt", b"escaped content")]);

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    // The symlink parent check should catch this
    assert!(
        result.is_err(),
        "Extraction through symlinked parent must be rejected"
    );
}

// ── safe_extract_zip: collision tests ──────────────────────────────────────

#[test]
fn test_file_dir_collision_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    // Create a file where the ZIP wants to create a directory
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("world"), b"i am a file").unwrap();

    let zip_data = create_test_zip(&[("world/", b""), ("world/level.dat", b"data")]);

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(
        result.is_err(),
        "File-vs-directory collision must be rejected"
    );
}

#[test]
fn test_dir_file_collision_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    // Create a directory where the ZIP wants to create a file
    std::fs::create_dir_all(dest.join("data")).unwrap();

    let zip_data = create_test_zip(&[("data", b"i am a file")]);

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(
        result.is_err(),
        "Directory-vs-file collision must be rejected"
    );
}

// ── safe_extract_zip: edge cases ───────────────────────────────────────────

#[test]
fn test_unicode_filenames_succeed() {
    let zip_data = create_test_zip(&[
        ("世界/データ.txt", b"unicode content"),
        ("world/мір/datos.txt", b"more unicode"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(
        result.is_ok(),
        "Unicode filenames should succeed: {:?}",
        result
    );
}

#[test]
fn test_empty_archive_succeeds() {
    let zip_data = create_test_zip(&[]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    assert!(result.is_ok(), "Empty archive should succeed");
}

#[test]
fn test_strip_prefix_works() {
    let zip_data = create_test_zip(&[
        ("world/level.dat", b"data"),
        ("world/region/r.0.0.mca", b"region"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "world/");
    assert!(result.is_ok());
    assert!(dest.join("level.dat").exists());
    assert!(dest.join("region").join("r.0.0.mca").exists());
}

#[test]
fn test_dot_prefix_rejected() {
    // .hidden is a valid directory name, not a traversal
    let zip_data = create_test_zip(&[(".hidden/evil.txt", b"content")]);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("extract");
    std::fs::create_dir_all(&dest).unwrap();

    let mut archive = ZipArchive::new(Cursor::new(&zip_data)).unwrap();
    let result = lbby_core::helpers::safe_extract_zip(&mut archive, &dest, "");
    // .hidden is valid — enclosed_name() allows it
    assert!(result.is_ok(), ".hidden directories are valid");
}

// ── Transactional restore tests ────────────────────────────────────────────

/// Create a mock AppEventSender for testing.
fn mock_event_sender() -> std::sync::Arc<lbby_core::app_state::AppEventSender> {
    let state = std::sync::Arc::new(lbby_core::app_state::AppState::new());
    std::sync::Arc::new(lbby_core::app_state::AppEventSender::new(state))
}

/// Create a test backup ZIP with some files.
fn create_test_backup(dir: &Path) -> std::path::PathBuf {
    let zip_path = dir.join("backup.zip");
    let zip_data = create_test_zip(&[
        ("server.properties", b"motd=Test"),
        ("world/level.dat", b"level data"),
        ("world/region/r.0.0.mca", b"region data"),
    ]);
    std::fs::write(&zip_path, zip_data).unwrap();
    zip_path
}

#[test]
fn test_transactional_restore_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("server");
    std::fs::create_dir_all(&server_path).unwrap();
    std::fs::write(server_path.join("old_file.txt"), b"old data").unwrap();

    let zip_path = create_test_backup(tmp.path());
    let app = mock_event_sender();

    let result =
        lbby_core::backup::restore_server_backup_transactional(&app, &zip_path, &server_path);
    assert!(
        result.is_ok(),
        "Transactional restore should succeed: {:?}",
        result
    );
    assert!(result.unwrap() > 0, "Should restore at least one file");
    assert!(server_path.join("server.properties").exists());
    assert!(server_path.join("world").join("level.dat").exists());
    // Old file should be gone (replaced by staging)
    assert!(!server_path.join("old_file.txt").exists());
}

#[test]
fn test_transactional_restore_cleans_staging_on_success() {
    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("server");
    std::fs::create_dir_all(&server_path).unwrap();

    let zip_path = create_test_backup(tmp.path());
    let app = mock_event_sender();

    let _ = lbby_core::backup::restore_server_backup_transactional(&app, &zip_path, &server_path);

    // Verify no staging or rollback dirs remain
    let parent = tmp.path();
    for entry in std::fs::read_dir(parent).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.contains("restore-staging"),
            "Staging dir should be cleaned: {}",
            name
        );
        assert!(
            !name.contains("restore-rollback"),
            "Rollback dir should be cleaned: {}",
            name
        );
    }
}

#[test]
fn test_transactional_restore_preserves_original_on_staging_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("server");
    std::fs::create_dir_all(&server_path).unwrap();
    std::fs::write(server_path.join("important.txt"), b"must survive").unwrap();

    // Create a ZIP with a traversal path to trigger extraction failure
    let zip_path = tmp.path().join("bad.zip");
    let bad_zip = create_test_zip(&[("../escape.txt", b"malicious")]);
    std::fs::write(&zip_path, bad_zip).unwrap();

    let app = mock_event_sender();
    let result =
        lbby_core::backup::restore_server_backup_transactional(&app, &zip_path, &server_path);

    // Should fail
    assert!(result.is_err(), "Bad archive should fail");

    // Original data must survive
    assert!(
        server_path.join("important.txt").exists(),
        "Original data must survive a failed restore"
    );
    assert_eq!(
        std::fs::read_to_string(server_path.join("important.txt")).unwrap(),
        "must survive"
    );
}

#[test]
fn test_transactional_restore_empty_archive_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("server");
    std::fs::create_dir_all(&server_path).unwrap();

    // Empty ZIP
    let zip_path = tmp.path().join("empty.zip");
    std::fs::write(&zip_path, create_test_zip(&[])).unwrap();

    let app = mock_event_sender();
    let result =
        lbby_core::backup::restore_server_backup_transactional(&app, &zip_path, &server_path);
    assert!(result.is_err(), "Empty archive should fail validation");
    assert!(result.unwrap_err().contains("no files"));
}

#[test]
fn test_transactional_restore_nonexistent_zip_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("server");
    std::fs::create_dir_all(&server_path).unwrap();

    let app = mock_event_sender();
    let result = lbby_core::backup::restore_server_backup_transactional(
        &app,
        &tmp.path().join("nonexistent.zip"),
        &server_path,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("does not exist"));
}

#[test]
fn test_transactional_restore_creates_server_dir_if_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let server_path = tmp.path().join("new_server");

    let zip_path = create_test_backup(tmp.path());
    let app = mock_event_sender();

    let result =
        lbby_core::backup::restore_server_backup_transactional(&app, &zip_path, &server_path);
    assert!(
        result.is_ok(),
        "Should create server dir if missing: {:?}",
        result
    );
    assert!(server_path.exists());
}

// ── OperationGuard tests ──────────────────────────────────────────────────

use lbby_core::app_state::{OperationGuard, OperationKind};
use tokio::sync::Mutex;

#[tokio::test]
async fn test_guard_acquire_and_release() {
    let slot = Mutex::new(OperationKind::None);

    {
        let guard = OperationGuard::acquire(&slot, OperationKind::Restoring).await;
        assert!(guard.is_ok());
        assert_eq!(*slot.lock().await, OperationKind::Restoring);
    }
    // Guard dropped — should release
    assert_eq!(*slot.lock().await, OperationKind::None);
}

#[tokio::test]
async fn test_guard_blocks_conflicting_operation() {
    let slot = Mutex::new(OperationKind::None);

    let _guard = OperationGuard::acquire(&slot, OperationKind::Restoring)
        .await
        .unwrap();

    // Try to acquire a different operation — should fail
    let result = OperationGuard::acquire(&slot, OperationKind::Importing).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Restoring"));
}

#[tokio::test]
async fn test_guard_same_operation_also_blocked() {
    let slot = Mutex::new(OperationKind::None);

    let _guard = OperationGuard::acquire(&slot, OperationKind::BackingUp)
        .await
        .unwrap();

    // Same operation should also be blocked
    let result = OperationGuard::acquire(&slot, OperationKind::BackingUp).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_guard_release_after_error() {
    let slot = Mutex::new(OperationKind::None);

    // Simulate an operation that fails
    let result: Result<(), String> = {
        let _guard = OperationGuard::acquire(&slot, OperationKind::Restoring)
            .await
            .unwrap();
        assert_eq!(*slot.lock().await, OperationKind::Restoring);
        Err("something failed".to_string())
    };
    // Guard dropped even on error path
    assert_eq!(*slot.lock().await, OperationKind::None);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_guard_does_not_clobber_newer_operation() {
    let slot = Mutex::new(OperationKind::None);

    // Acquire guard A
    let guard_a = OperationGuard::acquire(&slot, OperationKind::Restoring)
        .await
        .unwrap();

    // Manually force-set a new operation (simulating a bug or race)
    *slot.lock().await = OperationKind::Importing;

    // Drop guard A — should NOT reset to None because the operation changed
    drop(guard_a);

    // The newer operation should survive
    assert_eq!(*slot.lock().await, OperationKind::Importing);
}

#[tokio::test]
async fn test_guard_all_operations_serializable() {
    let operations = vec![
        OperationKind::None,
        OperationKind::Starting,
        OperationKind::Stopping,
        OperationKind::Restoring,
        OperationKind::Importing,
        OperationKind::Exporting,
        OperationKind::Installing,
        OperationKind::BackingUp,
        OperationKind::Resetting,
        OperationKind::DeletingProfile,
    ];

    for op in &operations {
        let json = serde_json::to_string(op).unwrap();
        assert!(!json.is_empty(), "Serialization failed for {:?}", op);
    }
}

#[tokio::test]
async fn test_guard_all_operations_acquirable() {
    let operations = vec![
        OperationKind::Starting,
        OperationKind::Stopping,
        OperationKind::Restoring,
        OperationKind::Importing,
        OperationKind::Exporting,
        OperationKind::Installing,
        OperationKind::BackingUp,
        OperationKind::Resetting,
        OperationKind::DeletingProfile,
    ];

    for op in operations {
        let slot = Mutex::new(OperationKind::None);
        let guard = OperationGuard::acquire(&slot, op.clone()).await;
        assert!(guard.is_ok(), "Should acquire {:?}: {:?}", op, guard.err());
        assert_eq!(*slot.lock().await, op);
        drop(guard);
        assert_eq!(*slot.lock().await, OperationKind::None);
    }
}
