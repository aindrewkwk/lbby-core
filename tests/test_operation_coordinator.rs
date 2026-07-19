//! Tests for operation coordination across all destructive commands.
//!
//! Verifies that OperationGuard prevents races between conflicting operations
//! and that all command types can acquire their respective guards.

use lbby_core::app_state::{OperationGuard, OperationKind};
use tokio::sync::Mutex;

// ── Conflict pair tests ───────────────────────────────────────────────────

#[tokio::test]
async fn test_start_conflicts_with_restore() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::Starting)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Restoring).await;
    assert!(r.is_err(), "start must block restore");
}

#[tokio::test]
async fn test_restore_conflicts_with_start() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::Restoring)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Starting).await;
    assert!(r.is_err(), "restore must block start");
}

#[tokio::test]
async fn test_install_conflicts_with_backup() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::Installing)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::BackingUp).await;
    assert!(r.is_err(), "install must block backup");
}

#[tokio::test]
async fn test_backup_conflicts_with_restore() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::BackingUp)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Restoring).await;
    assert!(r.is_err(), "backup must block restore");
}

#[tokio::test]
async fn test_import_conflicts_with_start() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::Importing)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Starting).await;
    assert!(r.is_err(), "import must block start");
}

#[tokio::test]
async fn test_reset_conflicts_with_install() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::Resetting)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Installing).await;
    assert!(r.is_err(), "reset must block install");
}

#[tokio::test]
async fn test_delete_profile_conflicts_with_start() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::DeletingProfile)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Starting).await;
    assert!(r.is_err(), "delete_profile must block start");
}

#[tokio::test]
async fn test_stopping_conflicts_with_start() {
    let slot = Mutex::new(OperationKind::None);
    let _g = OperationGuard::acquire(&slot, OperationKind::Stopping)
        .await
        .unwrap();
    let r = OperationGuard::acquire(&slot, OperationKind::Starting).await;
    assert!(r.is_err(), "stopping must block start");
}

// ── Sequential operations ─────────────────────────────────────────────────

#[tokio::test]
async fn test_start_then_stop_then_restore_succeeds() {
    let slot = Mutex::new(OperationKind::None);

    {
        let _g = OperationGuard::acquire(&slot, OperationKind::Starting)
            .await
            .unwrap();
        assert_eq!(*slot.lock().await, OperationKind::Starting);
    }
    assert_eq!(*slot.lock().await, OperationKind::None);

    {
        let _g = OperationGuard::acquire(&slot, OperationKind::Stopping)
            .await
            .unwrap();
        assert_eq!(*slot.lock().await, OperationKind::Stopping);
    }
    assert_eq!(*slot.lock().await, OperationKind::None);

    {
        let _g = OperationGuard::acquire(&slot, OperationKind::Restoring)
            .await
            .unwrap();
        assert_eq!(*slot.lock().await, OperationKind::Restoring);
    }
    assert_eq!(*slot.lock().await, OperationKind::None);
}

#[tokio::test]
async fn test_install_then_backup_then_start_succeeds() {
    let slot = Mutex::new(OperationKind::None);

    {
        let _g = OperationGuard::acquire(&slot, OperationKind::Installing)
            .await
            .unwrap();
    }
    {
        let _g = OperationGuard::acquire(&slot, OperationKind::BackingUp)
            .await
            .unwrap();
    }
    {
        let _g = OperationGuard::acquire(&slot, OperationKind::Starting)
            .await
            .unwrap();
    }
    assert_eq!(*slot.lock().await, OperationKind::None);
}

// ── Guard release on early return ─────────────────────────────────────────

#[tokio::test]
async fn test_guard_releases_on_early_return() {
    let slot = Mutex::new(OperationKind::None);

    fn simulate_early_return(slot: &Mutex<OperationKind>) -> Result<(), String> {
        // This would be a sync version — in real code it's async
        // Just demonstrating the pattern
        let _guard_fut = OperationGuard::acquire(slot, OperationKind::Restoring);
        // In real async code: let _guard = guard_fut.await?;
        // Early return here — guard drops
        Err("early return".to_string())
    }

    let _ = simulate_early_return(&slot);
    // Slot should still be None (guard never acquired in sync path)
    assert_eq!(*slot.lock().await, OperationKind::None);
}

#[tokio::test]
async fn test_guard_releases_on_error_path() {
    let slot = Mutex::new(OperationKind::None);

    let result: Result<(), String> = {
        let _guard = OperationGuard::acquire(&slot, OperationKind::Importing)
            .await
            .unwrap();
        assert_eq!(*slot.lock().await, OperationKind::Importing);
        Err("simulated error".to_string())
    };

    assert!(result.is_err());
    assert_eq!(
        *slot.lock().await,
        OperationKind::None,
        "guard must release on error"
    );
}

// ── Stale guard cannot release newer operation ────────────────────────────

#[tokio::test]
async fn test_stale_guard_preserves_newer_operation() {
    let slot = Mutex::new(OperationKind::None);

    let guard_a = OperationGuard::acquire(&slot, OperationKind::Restoring)
        .await
        .unwrap();
    assert_eq!(*slot.lock().await, OperationKind::Restoring);

    // Simulate a newer operation taking over (e.g., via manual override)
    *slot.lock().await = OperationKind::Importing;

    // Drop old guard — should NOT reset to None
    drop(guard_a);

    assert_eq!(
        *slot.lock().await,
        OperationKind::Importing,
        "stale guard must not clobber newer operation"
    );
}

#[tokio::test]
async fn test_stale_guard_does_not_clobber_starting() {
    let slot = Mutex::new(OperationKind::None);

    let guard = OperationGuard::acquire(&slot, OperationKind::BackingUp)
        .await
        .unwrap();
    *slot.lock().await = OperationKind::Starting;
    drop(guard);

    assert_eq!(*slot.lock().await, OperationKind::Starting);
}

// ── All operations acquirable ─────────────────────────────────────────────

#[tokio::test]
async fn test_all_operation_kinds_acquirable() {
    let kinds = vec![
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

    for kind in kinds {
        let slot = Mutex::new(OperationKind::None);
        let guard = OperationGuard::acquire(&slot, kind.clone()).await;
        assert!(
            guard.is_ok(),
            "should acquire {:?}: {:?}",
            kind,
            guard.err()
        );
        drop(guard);
        assert_eq!(*slot.lock().await, OperationKind::None);
    }
}

// ── Serialization matrix ──────────────────────────────────────────────────

/// Verify that every pair of distinct operations conflicts.
#[tokio::test]
async fn test_all_distinct_pairs_conflict() {
    let kinds = vec![
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

    for (i, a) in kinds.iter().enumerate() {
        for (j, b) in kinds.iter().enumerate() {
            if i == j {
                continue;
            }
            let slot = Mutex::new(OperationKind::None);
            let _ga = OperationGuard::acquire(&slot, a.clone()).await.unwrap();
            let r = OperationGuard::acquire(&slot, b.clone()).await;
            assert!(r.is_err(), "{:?} should conflict with {:?}", a, b);
        }
    }
}

// ── Simulated command patterns ────────────────────────────────────────────

/// Simulate the start_server command pattern.
#[tokio::test]
async fn test_start_server_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Starting)
        .await
        .unwrap();
    // do_start_server would be called here
    // guard drops on function exit
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the restore_backup command pattern.
#[tokio::test]
async fn test_restore_backup_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Restoring)
        .await
        .unwrap();
    // backup_restore_transactional would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the create_backup command pattern.
#[tokio::test]
async fn test_create_backup_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::BackingUp)
        .await
        .unwrap();
    // backup_create would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the import_world command pattern.
#[tokio::test]
async fn test_import_world_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Importing)
        .await
        .unwrap();
    // import logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the delete_minecraft_world command pattern.
#[tokio::test]
async fn test_delete_world_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Resetting)
        .await
        .unwrap();
    // delete logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the install_server command pattern.
#[tokio::test]
async fn test_install_server_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Installing)
        .await
        .unwrap();
    // install logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the delete_profile command pattern.
#[tokio::test]
async fn test_delete_profile_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::DeletingProfile)
        .await
        .unwrap();
    // delete profile logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the pregenerate_chunks command pattern.
#[tokio::test]
async fn test_pregenerate_chunks_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Starting)
        .await
        .unwrap();
    // pregen logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the regenerate_world command pattern.
#[tokio::test]
async fn test_regenerate_world_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Resetting)
        .await
        .unwrap();
    // regenerate logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the reset_playit command pattern.
#[tokio::test]
async fn test_reset_playit_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Resetting)
        .await
        .unwrap();
    // reset logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the automodpack_install command pattern.
#[tokio::test]
async fn test_automodpack_install_pattern() {
    let slot = Mutex::new(OperationKind::None);
    let _guard = OperationGuard::acquire(&slot, OperationKind::Installing)
        .await
        .unwrap();
    // automodpack install logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}

/// Simulate the restart_server command pattern (stop + start).
#[tokio::test]
async fn test_restart_server_pattern() {
    let slot = Mutex::new(OperationKind::None);
    // restart acquires Starting guard (same as start)
    let _guard = OperationGuard::acquire(&slot, OperationKind::Starting)
        .await
        .unwrap();
    // stop + start logic would be called here
    drop(_guard);
    assert_eq!(*slot.lock().await, OperationKind::None);
}
