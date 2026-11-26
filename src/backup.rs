// ZIP backup of a Minecraft server directory.
// Filename format: [Backup]-DD-MM-YYYY-HHMMSS.zip
// Includes worlds, configs, mods, plugins, server.properties — basically everything except a few
// runtime/system files. Logs are excluded by default.
//
// Skipped paths:
//   - .DS_Store (macOS metadata)
//   - session.lock (active world lock)
//   - logs/ (unless include_logs = true)
//   - target/, node_modules/, .git/ (in case the user picked a weird path)

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::ZipWriter;

#[derive(Clone, serde::Serialize)]
struct BackupProgress {
    files: u64,
    bytes: u64,
    current: String,
    total_bytes: u64,
}

pub fn timestamp_ddmmyyyy_hhmmss() -> String {
    chrono::Local::now().format("%d-%m-%Y-%H%M%S").to_string()
}

pub fn default_backup_filename() -> String {
    format!("[Backup]-{}.zip", timestamp_ddmmyyyy_hhmmss())
}

const SKIP_NAMES_ANYWHERE: &[&str] = &[".DS_Store", "session.lock", "Thumbs.db"];
const SKIP_TOPLEVEL_DIRS: &[&str] = &[".git", "node_modules", "target"];

#[cfg(test)]
mod backup_creation_tests {
    use super::*;
    fn sender() -> std::sync::Arc<crate::app_state::AppEventSender> {
        std::sync::Arc::new(crate::app_state::AppEventSender::new(std::sync::Arc::new(crate::app_state::AppState::new())))
    }
    #[test]
    fn refuses_backup_inside_server_without_creating_zip() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("backup.zip");
        let err = create_server_backup(&sender(), dir.path(), &dest, false).unwrap_err();
        assert!(err.contains("outside the server directory"));
        assert!(!dest.exists());
    }
    #[test]
    fn progress_total_counts_only_included_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("world.dat"), b"12345").unwrap();
        std::fs::write(dir.path().join("session.lock"), b"ignored").unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/latest.log"), b"123").unwrap();
        assert_eq!(count_backup_bytes(dir.path(), false, true).unwrap(), 5);
        assert_eq!(count_backup_bytes(dir.path(), true, true).unwrap(), 8);
    }
    #[test]
    fn creates_a_readable_zip_outside_server() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        std::fs::create_dir(&server).unwrap();
        std::fs::write(server.join("level.dat"), b"world data").unwrap();
        let dest = dir.path().join("backup.zip");
        let (count, _) = create_server_backup(&sender(), &server, &dest, false).unwrap();
        assert_eq!(count, 1);
        let mut zip = zip::ZipArchive::new(File::open(dest).unwrap()).unwrap();
        let mut contents = String::new();
        zip.by_name("level.dat").unwrap().read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "world data");
    }
    #[test]
    fn backup_can_be_restored_to_a_separate_server_directory() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        std::fs::create_dir_all(server.join("world/region")).unwrap();
        std::fs::write(server.join("server.properties"), b"motd=test").unwrap();
        std::fs::write(server.join("world/region/r.0.0.mca"), b"saved-world").unwrap();
        let archive = dir.path().join("backup.zip");
        create_server_backup(&sender(), &server, &archive, false).unwrap();
        let restored = dir.path().join("restored");
        assert_eq!(restore_server_backup(&sender(), &archive, &restored).unwrap(), 2);
        assert_eq!(std::fs::read(restored.join("server.properties")).unwrap(), b"motd=test");
        assert_eq!(std::fs::read(restored.join("world/region/r.0.0.mca")).unwrap(), b"saved-world");
    }
    #[test]
    fn symbolic_link_does_not_produce_a_successful_incomplete_backup() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        std::fs::create_dir(&server).unwrap();
        let world = dir.path().join("world-data");
        std::fs::create_dir(&world).unwrap();
        let link = server.join("world");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&world, &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(&world, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(1314) {
                eprintln!("Skipping symlink test: Windows does not grant symlink creation privileges");
                return;
            }
            panic!("Could not create test symlink: {error}");
        }
        let dest = dir.path().join("backup.zip");
        assert!(create_server_backup(&sender(), &server, &dest, false).unwrap_err().contains("symbolic link"));
        assert!(!dest.exists());
    }
    #[test]
    fn publish_failure_removes_temporary_archive() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        std::fs::create_dir(&server).unwrap();
        std::fs::write(server.join("level.dat"), b"world data").unwrap();
        let dest = dir.path().join("backup.zip");
        std::fs::create_dir(&dest).unwrap();
        assert!(create_server_backup(&sender(), &server, &dest, false).is_err());
        assert!(dest.is_dir());
        assert!(!std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with(".lbby-backup-")));
    }
}

fn count_backup_bytes(base: &Path, include_logs: bool, top: bool) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(base).map_err(|e| format!("read_dir {}: {}", base.display(), e))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_NAMES_ANYWHERE.iter().any(|x| *x == name.as_ref()) ||
            (top && (SKIP_TOPLEVEL_DIRS.iter().any(|x| *x == name.as_ref()) || (!include_logs && name == "logs") || name == "forge-installer.jar.log")) { continue; }
        let kind = entry.file_type().map_err(|e| format!("file_type {}: {}", entry.path().display(), e))?;
        if kind.is_symlink() { return Err(format!("Cannot back up symbolic link: {}", entry.path().display())); }
        if kind.is_dir() { total = total.saturating_add(count_backup_bytes(&entry.path(), include_logs, false)?); }
        else if kind.is_file() { total = total.saturating_add(entry.metadata().map_err(|e| format!("metadata {}: {}", entry.path().display(), e))?.len()); }
    }
    Ok(total)
}

pub fn create_server_backup(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    server_path: &Path,
    dest_zip: &Path,
    include_logs: bool,
) -> Result<(u64, u64), String> {
    if !server_path.exists() {
        return Err(format!(
            "Server path does not exist: {}",
            server_path.display()
        ));
    }
    if !server_path.is_dir() {
        return Err(format!(
            "Server path is not a directory: {}",
            server_path.display()
        ));
    }

    let parent = dest_zip.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create dest dir: {}", e))?;
    let server_root = server_path.canonicalize().map_err(|e| format!("Cannot resolve server path: {}", e))?;
    let dest_parent = parent.canonicalize().map_err(|e| format!("Cannot resolve backup directory: {}", e))?;
    if dest_parent.starts_with(&server_root) {
        return Err("Backup destination must be outside the server directory".to_string());
    }
    // Write next to the destination and publish only a completed archive.
    // This preserves any prior backup if creating the new one fails.
    let total_bytes = count_backup_bytes(server_path, include_logs, true)?;
    let temp_path = dest_parent.join(format!(".lbby-backup-{}.tmp", uuid::Uuid::new_v4()));
    struct TempBackup(PathBuf);
    impl Drop for TempBackup { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let temp = TempBackup(temp_path);
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp.0)
        .map_err(|e| format!("Cannot create temporary zip: {}", e))?;
    let mut zip = ZipWriter::new(file);

    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut state = WalkState {
        files: 0,
        bytes: 0,
        total_bytes,
        last_emit: std::time::Instant::now(),
    };

    walk_and_zip(
        server_path,
        Path::new(""),
        &mut zip,
        opts,
        include_logs,
        true,
        &mut state,
        app,
    )?;

    let file = zip.finish().map_err(|e| format!("Failed to finalize zip: {}", e))?;
    file.sync_all().map_err(|e| format!("Failed to flush zip: {}", e))?;
    drop(file);
    std::fs::rename(&temp.0, dest_zip).map_err(|e| format!("Cannot publish backup: {}", e))?;

    // Final progress emit
    app.emit(
        "backup-progress",
        BackupProgress {
            files: state.files,
            bytes: state.bytes,
            current: "done".to_string(),
            total_bytes,
        },
    )
    .ok();

    Ok((state.files, state.bytes))
}

struct WalkState {
    files: u64,
    bytes: u64,
    total_bytes: u64,
    last_emit: std::time::Instant,
}

#[allow(clippy::too_many_arguments)]
fn walk_and_zip<W: Write + std::io::Seek>(
    base: &Path,
    rel: &Path,
    zip: &mut ZipWriter<W>,
    opts: SimpleFileOptions,
    include_logs: bool,
    is_top_level: bool,
    state: &mut WalkState,
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
) -> Result<(), String> {
    let abs = base.join(rel);
    for entry in
        std::fs::read_dir(&abs).map_err(|e| format!("read_dir {}: {}", abs.display(), e))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if SKIP_NAMES_ANYWHERE.iter().any(|x| *x == name_str.as_ref()) {
            continue;
        }
        if is_top_level {
            if SKIP_TOPLEVEL_DIRS.iter().any(|x| *x == name_str.as_ref()) {
                continue;
            }
            if !include_logs && name_str == "logs" {
                continue;
            }
            // The downloaded Forge installer log is huge and useless in a backup
            if name_str == "forge-installer.jar.log" {
                continue;
            }
        }

        let entry_rel = rel.join(&name);
        let zip_path = entry_rel.to_string_lossy().replace('\\', "/");

        let ft = entry
            .file_type()
            .map_err(|e| format!("file_type {}: {}", entry.path().display(), e))?;

        // Silently skipping a linked world or plugin directory would publish
        // an incomplete backup as successful. Refuse it rather than traverse
        // outside the server or omit data without warning.
        if ft.is_symlink() {
            return Err(format!("Cannot back up symbolic link: {}", entry.path().display()));
        }

        if ft.is_dir() {
            zip.add_directory(&zip_path, opts)
                .map_err(|e| format!("add_directory {}: {}", zip_path, e))?;
            walk_and_zip(base, &entry_rel, zip, opts, include_logs, false, state, app)?;
        } else if ft.is_file() {
            let file_len = entry.metadata().map_err(|e| format!("metadata {}: {}", entry.path().display(), e))?.len();
            zip.start_file(&zip_path, opts.large_file(file_len >= u32::MAX as u64))
                .map_err(|e| format!("start_file {}: {}", zip_path, e))?;
            let mut f = File::open(entry.path())
                .map_err(|e| format!("open {}: {}", entry.path().display(), e))?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = f
                    .read(&mut buf)
                    .map_err(|e| format!("read {}: {}", entry.path().display(), e))?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buf[..n])
                    .map_err(|e| format!("zip write: {}", e))?;
                state.bytes += n as u64;
                // Large region files and server jars may take minutes to compress.
                // Report bytes while copying, not only after the file finishes.
                if state.last_emit.elapsed().as_millis() >= 250 {
                    app.emit(
                        "backup-progress",
                        BackupProgress { files: state.files, bytes: state.bytes, current: zip_path.clone(), total_bytes: state.total_bytes },
                    ).ok();
                    state.last_emit = std::time::Instant::now();
                }
            }
            state.files += 1;

            // Throttled progress emits — every 250ms or every 32 files
            if state.last_emit.elapsed().as_millis() >= 250 || state.files.is_multiple_of(32) {
                app.emit(
                    "backup-progress",
                    BackupProgress {
                        files: state.files,
                        bytes: state.bytes,
                        current: zip_path.clone(),
                        total_bytes: state.total_bytes,
                    },
                )
                .ok();
                state.last_emit = std::time::Instant::now();
            }
        }
    }
    Ok(())
}

/// Extract a backup ZIP into the server directory using a transactional
/// staging approach. Returns the number of files restored.
///
/// Transactional flow:
///   1. Extract to a staging directory (`.restore-staging-{timestamp}`)
///   2. Rename original server dir → `.rollback-{timestamp}`
///   3. Rename staging → server dir (atomic on same filesystem)
///   4. On success, remove rollback dir
///   5. On failure, rollback: rename rollback → server dir, clean staging
///
/// Safety: rejects entries with absolute paths or `..` components (zip-slip).
pub fn restore_server_backup(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    zip_path: &Path,
    server_path: &Path,
) -> Result<u64, String> {
    if !zip_path.exists() {
        return Err(format!(
            "Backup file does not exist: {}",
            zip_path.display()
        ));
    }

    let ts = chrono::Local::now().format("%Y%m%d%H%M%S");
    let parent = server_path.parent().unwrap_or(Path::new("/"));
    let staging_path = parent.join(format!(".restore-staging-{}", ts));
    let rollback_path = parent.join(format!(".rollback-{}", ts));

    // ── Step 1: Extract ZIP into staging directory ──────────────────────
    std::fs::create_dir_all(&staging_path)
        .map_err(|e| format!("Cannot create staging dir: {}", e))?;

    let extract_result = extract_zip_to_dir(app, zip_path, &staging_path);

    if let Err(e) = extract_result {
        // Extraction failed — clean up staging dir and return error
        let _ = std::fs::remove_dir_all(&staging_path);
        return Err(e);
    }

    let count = extract_result.unwrap();

    // ── Step 2: Atomic swap ─────────────────────────────────────────────
    // Rename original → rollback (if original exists)
    if server_path.exists() {
        if let Err(e) = std::fs::rename(server_path, &rollback_path) {
            // Swap failed — clean up staging and return error
            let _ = std::fs::remove_dir_all(&staging_path);
            return Err(format!("Cannot rename server dir for rollback: {}", e));
        }
    }

    // Rename staging → server_path
    if let Err(e) = std::fs::rename(&staging_path, server_path) {
        // staging → server failed — rollback the original
        if rollback_path.exists() {
            if let Err(rb_err) = std::fs::rename(&rollback_path, server_path) {
                // Rollback also failed — critical state, leave staging in place
                return Err(format!(
                    "CRITICAL: staging→server failed ({}), rollback also failed ({}). \
                     Staging dir preserved at: {}",
                    e,
                    rb_err,
                    staging_path.display()
                ));
            }
        }
        return Err(format!("Cannot rename staging to server dir: {}", e));
    }

    // ── Step 3: Clean up rollback dir (best-effort) ─────────────────────
    let _ = std::fs::remove_dir_all(&rollback_path);

    Ok(count)
}

/// Compatibility entry point used by desktop clients. The primary restore
/// implementation is already transactional; keep the explicit name so older
/// and modularized callers share the same safe behavior.
pub fn restore_server_backup_transactional(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    zip_path: &Path,
    server_path: &Path,
) -> Result<u64, String> {
    restore_server_backup(app, zip_path, server_path)
}

/// Extract a ZIP archive into the given directory. Returns the file count.
/// Safety: rejects entries with absolute paths or `..` components (zip-slip).
fn extract_zip_to_dir(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    zip_path: &Path,
    dest_dir: &Path,
) -> Result<u64, String> {
    let file = File::open(zip_path).map_err(|e| format!("Cannot open zip: {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Invalid zip: {}", e))?;

    let mut count: u64 = 0;
    let mut bytes: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    let total_entries = archive.len();

    for i in 0..total_entries {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Read entry {}: {}", i, e))?;

        let safe_path: PathBuf = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue, // unsafe path (absolute or contains `..`) — skip
        };
        let outpath = dest_dir.join(&safe_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&outpath)
                .map_err(|e| format!("mkdir {}: {}", outpath.display(), e))?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
            }
            let mut out = File::create(&outpath)
                .map_err(|e| format!("create {}: {}", outpath.display(), e))?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = entry
                    .read(&mut buf)
                    .map_err(|e| format!("read entry: {}", e))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .map_err(|e| format!("write {}: {}", outpath.display(), e))?;
                bytes += n as u64;
            }
            count += 1;

            if last_emit.elapsed().as_millis() >= 250 || count.is_multiple_of(32) {
                app.emit(
                    "restore-progress",
                    BackupProgress {
                        files: count,
                        bytes,
                        current: safe_path.to_string_lossy().to_string(),
                        total_bytes: 0,
                    },
                )
                .ok();
                last_emit = std::time::Instant::now();
            }
        }
    }

    app.emit(
        "restore-progress",
        BackupProgress {
            files: count,
            bytes,
            current: "done".to_string(),
            total_bytes: 0,
        },
    )
    .ok();

    Ok(count)
}
