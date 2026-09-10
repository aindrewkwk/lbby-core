// install_transaction.rs — Transactional staging for CurseForge modpack installation.
//
// All filesystem work happens in a staging directory. The live server is only
// modified during the commit phase (atomic rename). On failure, staging is
// discarded and the live server remains untouched.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Transaction metadata (persisted as transaction.json) ────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransactionPhase {
    Building,
    PendingUserAction,
    Committing,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionMeta {
    pub server_id: String,
    pub transaction_id: String,
    pub source: String,
    pub created_at: String,
    pub phase: TransactionPhase,
    pub live_path: PathBuf,
    pub staging_path: PathBuf,
    pub backup_path: Option<PathBuf>,
}

impl TransactionMeta {
    pub fn marker_path(&self) -> PathBuf {
        self.staging_path.join("transaction.json")
    }
}

// ── Install transaction ─────────────────────────────────────────────────

pub struct InstallTransaction {
    meta: TransactionMeta,
}

impl InstallTransaction {
    /// Begin a new install transaction. Creates the staging directory and
    /// writes `transaction.json` inside it.
    ///
    /// `live_path` is the final server directory (the one the app uses).
    /// The staging directory is a sibling: `<parent>/.lbby-staging/<name>-<id>/`.
    pub fn begin(live_path: &Path, source: &str) -> Result<Self, String> {
        let txn_id = generate_txn_id();
        let server_name = live_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "server".to_string());

        let parent = live_path
            .parent()
            .ok_or_else(|| format!("Cannot determine parent of {}", live_path.display()))?;

        let staging_root = parent.join(".lbby-staging");
        let staging_path = staging_root.join(format!("{}-{}", server_name, txn_id));

        std::fs::create_dir_all(&staging_path)
            .map_err(|e| format!("Failed to create staging dir: {}", e))?;

        let meta = TransactionMeta {
            server_id: server_name,
            transaction_id: txn_id,
            source: source.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            phase: TransactionPhase::Building,
            live_path: live_path.to_path_buf(),
            staging_path,
            backup_path: None,
        };

        meta.save()?;

        eprintln!("[CF] Created install transaction {}", meta.transaction_id);
        eprintln!(
            "[CF] Building staged server at {}",
            meta.staging_path.display()
        );

        Ok(Self { meta })
    }

    /// Path to the staging directory where the new server is being built.
    pub fn staging_path(&self) -> &Path {
        &self.meta.staging_path
    }

    /// Path to the live server directory.
    pub fn live_path(&self) -> &Path {
        &self.meta.live_path
    }

    /// Transaction metadata.
    pub fn meta(&self) -> &TransactionMeta {
        &self.meta
    }

    /// Copy persistent user/server state from the live server into staging.
    ///
    /// Preserves: world directories, server.properties, ops.json, whitelist.json,
    /// banned-players.json, banned-ips.json, usercache.json.
    ///
    /// For fresh installs (live_path does not exist), this is a no-op.
    pub fn copy_persistent_state(&self) -> Result<(), String> {
        let live = &self.meta.live_path;
        if !live.exists() {
            return Ok(()); // Fresh install — nothing to preserve
        }

        let staging = &self.meta.staging_path;

        // Detect world directory name from server.properties (default: "world")
        let world_name = detect_world_name(live);
        let mut persistent_dirs = vec![world_name.clone()];
        // The End and Nether may use <world>_the_end / <world>_nether
        persistent_dirs.push(format!("{}_nether", world_name));
        persistent_dirs.push(format!("{}_the_end", world_name));

        let persistent_files = [
            "server.properties",
            "ops.json",
            "whitelist.json",
            "banned-players.json",
            "banned-ips.json",
            "usercache.json",
        ];

        // Copy persistent directories (world data)
        for dir_name in &persistent_dirs {
            let src = live.join(dir_name);
            if src.exists() && src.is_dir() {
                let dest = staging.join(dir_name);
                copy_dir_recursive(&src, &dest)?;
                eprintln!("[CF] Preserved world directory: {}", dir_name);
            }
        }

        // Copy persistent files
        for file_name in &persistent_files {
            let src = live.join(file_name);
            if src.exists() {
                let dest = staging.join(file_name);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create parent for {}: {}", file_name, e))?;
                }
                std::fs::copy(&src, &dest)
                    .map_err(|e| format!("Failed to preserve {}: {}", file_name, e))?;
            }
        }

        Ok(())
    }

    /// Commit the transaction: rename staging → live (with backup of old live).
    ///
    /// The commit sequence is:
    /// 1. Mark phase as Committing
    /// 2. Rename live → backup (if live exists)
    /// 3. Rename staging → live
    /// 4. Mark phase as Committed
    /// 5. Clean up staging marker
    ///
    /// At every point, the server path refers to either the old complete build
    /// or the new complete build — never a mixture.
    pub fn commit(mut self) -> Result<TransactionMeta, String> {
        eprintln!(
            "[CF] Preparing commit for transaction {}",
            self.meta.transaction_id
        );

        // Step 1: Mark as committing
        self.meta.phase = TransactionPhase::Committing;
        self.meta.save()?;

        // Step 2: Rename live → backup (if live exists)
        if self.meta.live_path.exists() {
            let backup_name = format!(
                "{}-{}-backup",
                self.meta.server_id, self.meta.transaction_id
            );
            let backup_path = self
                .meta
                .live_path
                .parent()
                .ok_or("Cannot determine parent for backup")?
                .join(".lbby-staging")
                .join(backup_name);

            std::fs::rename(&self.meta.live_path, &backup_path).map_err(|e| {
                // Attempt to roll back the phase marker
                self.meta.phase = TransactionPhase::Building;
                let _ = self.meta.save();
                format!("Failed to rename live → backup: {}", e)
            })?;

            self.meta.backup_path = Some(backup_path);
            self.meta.save()?;
            eprintln!("[CF] Previous server preserved as backup");
        }

        // Step 3: Rename staging → live
        std::fs::rename(&self.meta.staging_path, &self.meta.live_path).map_err(|e| {
            // Attempt to restore backup → live
            if let Some(ref backup) = self.meta.backup_path {
                if backup.exists() {
                    let _ = std::fs::rename(backup, &self.meta.live_path);
                    eprintln!("[CF] Restored previous server after failed commit");
                }
            }
            self.meta.phase = TransactionPhase::Building;
            let _ = self.meta.save();
            format!("Failed to rename staging → live: {}", e)
        })?;

        // Step 4: Mark as committed
        self.meta.phase = TransactionPhase::Committed;
        // Save the marker into the now-live directory (so it survives staging cleanup)
        let live_marker = self.meta.live_path.join(".lbby-transaction.json");
        let json = serde_json::to_string_pretty(&self.meta)
            .map_err(|e| format!("Failed to serialize transaction: {}", e))?;
        std::fs::write(&live_marker, json)
            .map_err(|e| format!("Failed to write commit marker: {}", e))?;

        // Step 5: Clean up the staging marker (staging dir is now live, so the old marker is gone)
        // The staging directory itself was renamed to live, so no cleanup needed for it.
        // But the .lbby-staging parent dir might be empty now — leave it.

        eprintln!("[CF] Transaction committed: {}", self.meta.transaction_id);
        Ok(self.meta.clone())
    }

    /// Roll back the transaction: remove staging, restore live if needed.
    ///
    /// Called when installation fails before commit. The live server is
    /// either untouched (phase=Building) or restored from backup (phase=Committing).
    pub fn rollback(&self) -> Result<(), String> {
        eprintln!(
            "[CF] Rolling back transaction {} (phase={:?})",
            self.meta.transaction_id, self.meta.phase
        );

        // Remove staging directory
        if self.meta.staging_path.exists() {
            std::fs::remove_dir_all(&self.meta.staging_path)
                .map_err(|e| format!("Failed to remove staging: {}", e))?;
            eprintln!("[CF] Removed staging directory");
        }

        // If we were in Committing phase, the live dir was already renamed to backup.
        // Restore it.
        if self.meta.phase == TransactionPhase::Committing {
            if let Some(ref backup) = self.meta.backup_path {
                if backup.exists() && !self.meta.live_path.exists() {
                    std::fs::rename(backup, &self.meta.live_path)
                        .map_err(|e| format!("Failed to restore backup: {}", e))?;
                    eprintln!("[CF] Restored previous server from backup");
                }
            }
        }

        // Clean up marker file
        let marker = self.meta.marker_path();
        if marker.exists() {
            let _ = std::fs::remove_file(&marker);
        }

        eprintln!("[CF] Live server was not modified");
        Ok(())
    }

    /// Find stale (incomplete) transactions for a given server path.
    ///
    /// Scans the `.lbby-staging` directory for transaction markers.
    /// Returns metadata for transactions that are in Building or Committing phase.
    /// Committed transactions are cleaned up automatically.
    pub fn find_stale(live_path: &Path) -> Vec<TransactionMeta> {
        let parent = match live_path.parent() {
            Some(p) => p,
            None => return vec![],
        };
        let staging_root = parent.join(".lbby-staging");
        if !staging_root.exists() {
            return vec![];
        }

        let mut stale = vec![];
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
                if let Ok(content) = std::fs::read_to_string(&marker) {
                    if let Ok(meta) = serde_json::from_str::<TransactionMeta>(&content) {
                        match meta.phase {
                            TransactionPhase::Building
                            | TransactionPhase::Committing
                            | TransactionPhase::PendingUserAction => {
                                stale.push(meta);
                            }
                            TransactionPhase::Committed => {
                                // Leftover from a completed transaction — clean up
                                let _ = std::fs::remove_dir_all(&path);
                            }
                        }
                    }
                }
            }
        }

        // Also check the live directory for a committed marker
        let live_marker = live_path.join(".lbby-transaction.json");
        if live_marker.exists() {
            if let Ok(content) = std::fs::read_to_string(&live_marker) {
                if let Ok(meta) = serde_json::from_str::<TransactionMeta>(&content) {
                    if meta.phase == TransactionPhase::Committed {
                        // Clean up committed marker
                        let _ = std::fs::remove_file(&live_marker);
                        // Clean up backup if it exists
                        if let Some(ref backup) = meta.backup_path {
                            if backup.exists() {
                                let _ = std::fs::remove_dir_all(backup);
                            }
                        }
                    }
                }
            }
        }

        stale
    }

    /// Clean up a committed transaction's backup directory.
    pub fn cleanup_backup(meta: &TransactionMeta) {
        if let Some(ref backup) = meta.backup_path {
            if backup.exists() {
                let _ = std::fs::remove_dir_all(backup);
            }
        }
        // Also clean up committed marker in live dir
        let live_marker = meta.live_path.join(".lbby-transaction.json");
        if live_marker.exists() {
            let _ = std::fs::remove_file(&live_marker);
        }
    }

    /// Pause the transaction for user action. Saves metadata to disk and
    /// consumes the transaction handle without rolling back.
    ///
    /// The staging directory remains valid. The caller holds the `TransactionMeta`
    /// and can later resume with `InstallTransaction::resume(meta)`.
    ///
    /// If the app crashes while paused, `find_stale()` will detect the
    /// `PendingUserAction` phase marker on next startup.
    pub fn pause(mut self) -> TransactionMeta {
        self.meta.phase = TransactionPhase::PendingUserAction;
        let _ = self.meta.save(); // best-effort; marker already exists
        let meta = self.meta.clone();
        eprintln!(
            "[CF] Transaction {} paused for user action",
            meta.transaction_id
        );
        // Prevent Drop from rolling back — we consumed the phase
        std::mem::forget(self);
        meta
    }

    /// Resume a paused transaction from its saved metadata.
    ///
    /// Returns `Err` if the staging directory or marker no longer exists.
    pub fn resume(meta: TransactionMeta) -> Result<Self, String> {
        if meta.phase != TransactionPhase::PendingUserAction {
            return Err(format!(
                "Cannot resume transaction in {:?} phase",
                meta.phase
            ));
        }
        if !meta.staging_path.exists() {
            return Err("Staging directory no longer exists".to_string());
        }
        if !meta.marker_path().exists() {
            return Err("Transaction marker no longer exists".to_string());
        }
        eprintln!(
            "[CF] Resumed transaction {} from PendingUserAction",
            meta.transaction_id
        );
        Ok(Self { meta })
    }
}

impl Drop for InstallTransaction {
    fn drop(&mut self) {
        if self.meta.phase == TransactionPhase::Building {
            eprintln!(
                "[CF] Transaction {} dropped without commit — rolling back",
                self.meta.transaction_id
            );
            if let Err(e) = self.rollback() {
                eprintln!("[CF] Rollback failed: {}", e);
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

impl TransactionMeta {
    fn save(&self) -> Result<(), String> {
        let path = self.marker_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create staging dir: {}", e))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize transaction: {}", e))?;
        std::fs::write(&path, json)
            .map_err(|e| format!("Failed to write transaction marker: {}", e))
    }
}

fn generate_txn_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn detect_world_name(server_dir: &Path) -> String {
    let props_path = server_dir.join("server.properties");
    if let Ok(content) = std::fs::read_to_string(&props_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('#') {
                if let Some(val) = trimmed.strip_prefix("level-name=") {
                    let name = val.trim();
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
    }
    "world".to_string()
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest)
        .map_err(|e| format!("Failed to create {}: {}", dest.display(), e))?;

    for entry in
        std::fs::read_dir(src).map_err(|e| format!("Failed to read {}: {}", src.display(), e))?
    {
        let entry = entry.map_err(|e| format!("Dir entry error: {}", e))?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)
                .map_err(|e| format!("Failed to copy {}: {}", src_path.display(), e))?;
        }
    }

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("lbby-test")
            .join("install_txn")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_server_dir(base: &Path, name: &str) -> PathBuf {
        let server = base.join(name);
        fs::create_dir_all(&server).unwrap();
        server
    }

    #[test]
    fn fresh_install_creates_staging() {
        let base = temp_dir("fresh_install_creates_staging");
        let live = make_server_dir(&base, "my-server");

        let txn = InstallTransaction::begin(&live, "curseforge-manifest").unwrap();

        assert!(txn.staging_path().exists());
        assert!(txn.staging_path().starts_with(base.join(".lbby-staging")));
        assert_eq!(txn.live_path(), &live);
        assert_eq!(txn.meta().phase, TransactionPhase::Building);

        // Cleanup
        txn.rollback().unwrap();
    }

    #[test]
    fn staging_path_is_sibling_of_live() {
        let base = temp_dir("staging_path_is_sibling");
        let live = make_server_dir(&base, "server-a");

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Staging should be under .lbby-staging, which is a sibling of the server
        assert!(staging.starts_with(base.join(".lbby-staging")));
        // NOT inside the live server
        assert!(!staging.starts_with(&live));

        txn.rollback().unwrap();
    }

    #[test]
    fn transaction_marker_persists() {
        let base = temp_dir("marker_persists");
        let live = make_server_dir(&base, "srv");

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let marker = txn.meta().marker_path();
        assert!(marker.exists());

        // Re-read the marker
        let content = fs::read_to_string(&marker).unwrap();
        let meta: TransactionMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(meta.transaction_id, txn.meta().transaction_id);
        assert_eq!(meta.phase, TransactionPhase::Building);

        txn.rollback().unwrap();
    }

    #[test]
    fn commit_swaps_staging_to_live() {
        let base = temp_dir("commit_swaps");
        let live = make_server_dir(&base, "srv");

        // Create some existing content in live
        fs::write(live.join("existing.txt"), "old data").unwrap();

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Write new content to staging
        fs::write(staging.join("new-mod.jar"), "new data").unwrap();
        fs::create_dir_all(staging.join("mods")).unwrap();
        fs::write(staging.join("mods").join("fabric-api.jar"), "mod data").unwrap();

        // Commit
        let meta = txn.commit().unwrap();

        // Live now contains the new content
        assert!(live.join("new-mod.jar").exists());
        assert!(live.join("mods").join("fabric-api.jar").exists());
        // Old content is gone (it's in backup)
        assert!(!live.join("existing.txt").exists());
        // Staging is gone (renamed to live)
        assert!(!staging.exists());
        assert_eq!(meta.phase, TransactionPhase::Committed);

        // Backup exists
        assert!(meta.backup_path.is_some());
        let backup = meta.backup_path.as_ref().unwrap();
        assert!(backup.exists());
        assert!(backup.join("existing.txt").exists());

        // Cleanup
        InstallTransaction::cleanup_backup(&meta);
    }

    #[test]
    fn fresh_install_commit_no_backup() {
        let base = temp_dir("fresh_no_backup");
        let live = base.join("new-server"); // Does NOT exist yet

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        fs::write(staging.join("server.jar"), "data").unwrap();

        let meta = txn.commit().unwrap();

        assert!(live.exists());
        assert!(live.join("server.jar").exists());
        assert!(meta.backup_path.is_none()); // No backup for fresh install

        InstallTransaction::cleanup_backup(&meta);
    }

    #[test]
    fn rollback_before_commit_preserves_live() {
        let base = temp_dir("rollback_preserves");
        let live = make_server_dir(&base, "srv");
        fs::write(live.join("keep-this.txt"), "important").unwrap();

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Write stuff to staging
        fs::write(staging.join("new.jar"), "new").unwrap();

        // Rollback instead of commit
        txn.rollback().unwrap();

        // Live is untouched
        assert!(live.exists());
        assert!(live.join("keep-this.txt").exists());
        assert_eq!(
            fs::read_to_string(live.join("keep-this.txt")).unwrap(),
            "important"
        );
        // Staging is gone
        assert!(!staging.exists());
    }

    #[test]
    fn rollback_during_commit_restores_live() {
        let base = temp_dir("rollback_during");
        let live = make_server_dir(&base, "srv");
        fs::write(live.join("original.txt"), "data").unwrap();

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();
        fs::write(staging.join("new.jar"), "new").unwrap();

        // Simulate crash during commit by manually moving live to backup
        // then calling rollback
        let backup = base.join(".lbby-staging").join(format!(
            "{}-{}-backup",
            txn.meta().server_id,
            txn.meta().transaction_id
        ));

        // Manually do step 2 of commit (rename live → backup)
        fs::rename(&live, &backup).unwrap();
        assert!(!live.exists());
        assert!(backup.exists());

        // Now the transaction is in "Committing" phase (simulate)
        // Rollback should restore backup → live
        let mut meta = txn.meta().clone();
        meta.phase = TransactionPhase::Committing;
        meta.backup_path = Some(backup.clone());
        // Write the updated marker
        let json = serde_json::to_string_pretty(&meta).unwrap();
        fs::write(meta.marker_path(), json).unwrap();

        // Create a new transaction-like object for rollback
        let txn2 = InstallTransaction { meta };
        txn2.rollback().unwrap();

        // Live is restored
        assert!(live.exists());
        assert!(live.join("original.txt").exists());
    }

    #[test]
    fn persistent_state_preserved_on_update() {
        let base = temp_dir("persistent_preserved");
        let live = make_server_dir(&base, "srv");

        // Simulate existing server with world and config
        let world_dir = live.join("world");
        fs::create_dir_all(&world_dir).unwrap();
        fs::write(world_dir.join("level.dat"), "world data").unwrap();
        fs::write(
            live.join("server.properties"),
            "motd=My Server\nonline-mode=false\n",
        )
        .unwrap();
        fs::write(live.join("ops.json"), r#"[{"name":"player1"}]"#).unwrap();
        fs::write(live.join("whitelist.json"), "[]").unwrap();

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Simulate new pack content in staging
        fs::create_dir_all(staging.join("mods")).unwrap();
        fs::write(staging.join("mods").join("new-mod.jar"), "mod").unwrap();
        // New pack also has a server.properties (default)
        fs::write(staging.join("server.properties"), "motd=Default\n").unwrap();

        // Copy persistent state
        txn.copy_persistent_state().unwrap();

        // World data is preserved in staging
        assert!(staging.join("world").join("level.dat").exists());
        assert_eq!(
            fs::read_to_string(staging.join("world").join("level.dat")).unwrap(),
            "world data"
        );

        // server.properties from LIVE overwrites the default in staging
        assert_eq!(
            fs::read_to_string(staging.join("server.properties")).unwrap(),
            "motd=My Server\nonline-mode=false\n"
        );

        // ops.json and whitelist.json are preserved
        assert!(staging.join("ops.json").exists());
        assert!(staging.join("whitelist.json").exists());

        txn.rollback().unwrap();
    }

    #[test]
    fn pack_managed_config_updated_on_commit() {
        let base = temp_dir("pack_config_updated");
        let live = make_server_dir(&base, "srv");

        // Existing config
        fs::create_dir_all(live.join("config")).unwrap();
        fs::write(live.join("config").join("example.toml"), "old_value = 1\n").unwrap();

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // New pack has updated config
        fs::create_dir_all(staging.join("config")).unwrap();
        fs::write(
            staging.join("config").join("example.toml"),
            "new_value = 42\n",
        )
        .unwrap();

        // No persistent state copy for config/ — it's pack-managed
        let meta = txn.commit().unwrap();

        // After commit, the new config is in live
        assert_eq!(
            fs::read_to_string(live.join("config").join("example.toml")).unwrap(),
            "new_value = 42\n"
        );

        InstallTransaction::cleanup_backup(&meta);
    }

    #[test]
    fn quarantine_isolation() {
        let base = temp_dir("quarantine_isolation");
        let live = make_server_dir(&base, "srv");
        fs::create_dir_all(live.join("mods")).unwrap();
        fs::write(live.join("mods").join("old-mod.jar"), "old").unwrap();

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Staging has new mods
        fs::create_dir_all(staging.join("mods")).unwrap();
        fs::write(staging.join("mods").join("new-mod.jar"), "new").unwrap();
        // Quarantine dir in staging
        fs::create_dir_all(staging.join(".lbby-client-only-mods")).unwrap();
        fs::write(
            staging.join(".lbby-client-only-mods").join("client.jar"),
            "client-only",
        )
        .unwrap();

        // Before commit, live is untouched
        assert!(live.join("mods").join("old-mod.jar").exists());
        assert!(!live.join(".lbby-client-only-mods").exists());

        let meta = txn.commit().unwrap();

        // After commit, staging content is in live
        assert!(live.join("mods").join("new-mod.jar").exists());
        assert!(live
            .join(".lbby-client-only-mods")
            .join("client.jar")
            .exists());
        // Old content is in backup
        assert!(!live.join("mods").join("old-mod.jar").exists());

        InstallTransaction::cleanup_backup(&meta);
    }

    #[test]
    fn duplicate_transactions_no_collision() {
        let base = temp_dir("dup_no_collision");
        let live = make_server_dir(&base, "srv");

        let txn1 = InstallTransaction::begin(&live, "test").unwrap();
        let txn2 = InstallTransaction::begin(&live, "test").unwrap();

        // Different transaction IDs
        assert_ne!(txn1.meta().transaction_id, txn2.meta().transaction_id);
        // Different staging paths
        assert_ne!(txn1.staging_path(), txn2.staging_path());
        // Both exist
        assert!(txn1.staging_path().exists());
        assert!(txn2.staging_path().exists());

        txn1.rollback().unwrap();
        txn2.rollback().unwrap();
    }

    #[test]
    fn stale_transaction_detection() {
        // Simulate a crash: manually create a staging dir + marker file
        // (Drop auto-rolls back, so we can't just drop a live InstallTransaction)
        let base = temp_dir("stale_detection");
        let live = make_server_dir(&base, "srv");

        let staging_root = base.join(".lbby-staging");
        let staging_path = staging_root.join("srv-deadbeef1234");
        fs::create_dir_all(&staging_path).unwrap();

        // Write a fake transaction marker
        let meta = TransactionMeta {
            server_id: "srv".to_string(),
            transaction_id: "deadbeef1234".to_string(),
            source: "test".to_string(),
            created_at: "2026-09-09T00:00:00Z".to_string(),
            phase: TransactionPhase::Building,
            live_path: live.clone(),
            staging_path: staging_path.clone(),
            backup_path: None,
        };
        let marker = staging_path.join("transaction.json");
        let json = serde_json::to_string_pretty(&meta).unwrap();
        fs::write(&marker, json).unwrap();

        // find_stale should detect it
        let stale = InstallTransaction::find_stale(&live);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].transaction_id, "deadbeef1234");
        assert_eq!(stale[0].phase, TransactionPhase::Building);
        // Staging dir should still exist (find_stale does NOT delete Building phase)
        assert!(staging_path.exists());

        // Manual cleanup
        fs::remove_dir_all(&staging_path).unwrap();
    }

    #[test]
    fn world_name_detection() {
        let base = temp_dir("world_name_detect");
        let server = make_server_dir(&base, "srv");

        // Default
        assert_eq!(detect_world_name(&server), "world");

        // Custom level-name
        fs::write(
            &server.join("server.properties"),
            "level-name=my_world\nmotd=test\n",
        )
        .unwrap();
        assert_eq!(detect_world_name(&server), "my_world");

        // With comments
        fs::write(
            &server.join("server.properties"),
            "# comment\nlevel-name=custom_world\n",
        )
        .unwrap();
        assert_eq!(detect_world_name(&server), "custom_world");
    }

    #[test]
    fn official_server_pack_staging() {
        let base = temp_dir("server_pack_staging");
        let live = make_server_dir(&base, "srv");

        let txn = InstallTransaction::begin(&live, "curseforge-server-pack").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Simulate server pack extraction to staging
        fs::create_dir_all(staging.join("libraries")).unwrap();
        fs::write(staging.join("server.jar"), "server").unwrap();
        fs::write(staging.join("eula.txt"), "eula=true").unwrap();

        let meta = txn.commit().unwrap();

        assert!(live.join("server.jar").exists());
        assert!(live.join("libraries").exists());

        InstallTransaction::cleanup_backup(&meta);
    }

    #[test]
    fn manifest_fallback_staging() {
        let base = temp_dir("manifest_fallback_staging");
        let live = make_server_dir(&base, "srv");

        let txn = InstallTransaction::begin(&live, "curseforge-manifest").unwrap();
        let staging = txn.staging_path().to_path_buf();

        // Simulate manifest-based install in staging
        fs::create_dir_all(staging.join("mods")).unwrap();
        fs::write(staging.join("mods").join("mod1.jar"), "m1").unwrap();
        fs::write(staging.join("mods").join("mod2.jar"), "m2").unwrap();
        fs::create_dir_all(staging.join("config")).unwrap();
        fs::write(staging.join("config").join("pack.toml"), "[settings]").unwrap();

        let meta = txn.commit().unwrap();

        assert!(live.join("mods").join("mod1.jar").exists());
        assert!(live.join("mods").join("mod2.jar").exists());
        assert!(live.join("config").join("pack.toml").exists());

        InstallTransaction::cleanup_backup(&meta);
    }

    #[test]
    fn commit_marker_in_live_dir() {
        let base = temp_dir("commit_marker");
        let live = make_server_dir(&base, "srv");

        let txn = InstallTransaction::begin(&live, "test").unwrap();
        fs::write(txn.staging_path().join("file.txt"), "data").unwrap();

        let meta = txn.commit().unwrap();

        // Committed marker should be in live dir
        let marker = live.join(".lbby-transaction.json");
        assert!(marker.exists());
        let content = fs::read_to_string(&marker).unwrap();
        let loaded: TransactionMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.phase, TransactionPhase::Committed);

        InstallTransaction::cleanup_backup(&meta);
    }
}
