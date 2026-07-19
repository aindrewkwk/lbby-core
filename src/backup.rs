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
}

pub fn timestamp_ddmmyyyy_hhmmss() -> String {
    chrono::Local::now().format("%d-%m-%Y-%H%M%S").to_string()
}

pub fn default_backup_filename() -> String {
    format!("[Backup]-{}.zip", timestamp_ddmmyyyy_hhmmss())
}

const SKIP_NAMES_ANYWHERE: &[&str] = &[".DS_Store", "session.lock", "Thumbs.db"];
const SKIP_TOPLEVEL_DIRS: &[&str] = &[".git", "node_modules", "target"];

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

    if let Some(parent) = dest_zip.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create dest dir: {}", e))?;
    }
    let file = File::create(dest_zip).map_err(|e| format!("Cannot create zip: {}", e))?;
    let mut zip = ZipWriter::new(file);

    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut state = WalkState {
        files: 0,
        bytes: 0,
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

    zip.finish()
        .map_err(|e| format!("Failed to finalize zip: {}", e))?;

    // Final progress emit
    app.emit(
        "backup-progress",
        BackupProgress {
            files: state.files,
            bytes: state.bytes,
            current: "done".to_string(),
        },
    )
    .ok();

    Ok((state.files, state.bytes))
}

struct WalkState {
    files: u64,
    bytes: u64,
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

        // Skip symlinks to prevent traversing into directories outside the
        // server path (e.g., a symlinked world folder pointing to /).
        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            zip.add_directory(&zip_path, opts)
                .map_err(|e| format!("add_directory {}: {}", zip_path, e))?;
            walk_and_zip(base, &entry_rel, zip, opts, include_logs, false, state, app)?;
        } else if ft.is_file() {
            zip.start_file(&zip_path, opts)
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
                    },
                )
                .ok();
                state.last_emit = std::time::Instant::now();
            }
        }
        // Symlinks: ignored (avoid cycles, don't follow)
    }
    Ok(())
}

/// Extract a backup ZIP into the server directory, overwriting matching files.
/// Returns the number of files restored.
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
    std::fs::create_dir_all(server_path).map_err(|e| format!("Cannot create server dir: {}", e))?;

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
        let outpath = server_path.join(&safe_path);

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
        },
    )
    .ok();

    Ok(count)
}

/// Transactional restore: extract to staging, validate, then atomically swap.
///
/// # Safety guarantees
///
/// - Original directory is untouched until validation succeeds
/// - On failure before swap: original remains, staging cleaned
/// - On failure after swap: rollback restores original
/// - Rollback artifacts cleaned on success
///
/// # Stages
///
/// 1. Extract archive to `{server_path}.restore-staging-{timestamp}`
/// 2. Validate staging contains at least one expected file
/// 3. Rename original to `{server_path}.restore-rollback-{timestamp}`
/// 4. Rename staging to `{server_path}`
/// 5. Remove rollback directory
///
/// On failure at any stage, attempt to restore original state.
pub fn restore_server_backup_transactional(
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

    let ts = chrono::Local::now().format("%Y%m%d%H%M%S%3f").to_string();
    let staging_dir = PathBuf::from(format!("{}.restore-staging-{}", server_path.display(), ts));
    let rollback_dir = PathBuf::from(format!("{}.restore-rollback-{}", server_path.display(), ts));

    // Phase 1: Extract to staging
    let result = extract_to_staging(app, zip_path, &staging_dir);

    match result {
        Err(e) => {
            // Extraction failed — clean staging
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(format!("Staging extraction failed: {}", e));
        }
        Ok(count) => {
            // Phase 2: Validate staging has expected content
            if count == 0 {
                let _ = std::fs::remove_dir_all(&staging_dir);
                return Err("Backup archive contained no files".to_string());
            }

            // Phase 3: Swap — rename original to rollback, staging to live
            if server_path.exists() {
                if let Err(e) = std::fs::rename(server_path, &rollback_dir) {
                    let _ = std::fs::remove_dir_all(&staging_dir);
                    return Err(format!("Failed to rename original to rollback: {}", e));
                }
            }

            if let Err(e) = std::fs::rename(&staging_dir, server_path) {
                // Swap failed — try to restore rollback
                if rollback_dir.exists() {
                    if let Err(rb_err) = std::fs::rename(&rollback_dir, server_path) {
                        return Err(format!(
                            "CRITICAL: staging→live failed ({}), rollback also failed ({}). Rollback data at: {}",
                            e, rb_err, rollback_dir.display()
                        ));
                    }
                }
                let _ = std::fs::remove_dir_all(&staging_dir);
                return Err(format!("Failed to rename staging to live: {}", e));
            }

            // Phase 4: Success — clean rollback
            if rollback_dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&rollback_dir) {
                    // Non-fatal: rollback dir lingers but restore succeeded
                    eprintln!("Warning: failed to clean rollback dir: {}", e);
                }
            }

            Ok(count)
        }
    }
}

/// Extract a backup ZIP into a staging directory.
/// Returns the number of files extracted.
fn extract_to_staging(
    app: &std::sync::Arc<crate::app_state::AppEventSender>,
    zip_path: &Path,
    staging_dir: &Path,
) -> Result<u64, String> {
    std::fs::create_dir_all(staging_dir)
        .map_err(|e| format!("Cannot create staging dir: {}", e))?;

    let file = File::open(zip_path).map_err(|e| format!("Cannot open zip: {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Invalid zip: {}", e))?;

    crate::helpers::safe_extract_zip(&mut archive, staging_dir, "")
        .map_err(|e| format!("Safe extraction failed: {}", e))?;

    // Count extracted files
    let count = count_files_recursive(staging_dir);

    app.emit(
        "restore-progress",
        BackupProgress {
            files: count,
            bytes: 0,
            current: "staging_complete".to_string(),
        },
    )
    .ok();

    Ok(count)
}

/// Recursively count files in a directory.
fn count_files_recursive(dir: &Path) -> u64 {
    let mut count = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_file() {
                    count += 1;
                } else if ft.is_dir() {
                    count += count_files_recursive(&entry.path());
                }
            }
        }
    }
    count
}
