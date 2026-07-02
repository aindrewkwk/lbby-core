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
    for entry in std::fs::read_dir(&abs)
        .map_err(|e| format!("read_dir {}: {}", abs.display(), e))?
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
        return Err(format!("Backup file does not exist: {}", zip_path.display()));
    }
    std::fs::create_dir_all(server_path)
        .map_err(|e| format!("Cannot create server dir: {}", e))?;

    let file = File::open(zip_path).map_err(|e| format!("Cannot open zip: {}", e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("Invalid zip: {}", e))?;

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
                if n == 0 { break; }
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
        BackupProgress { files: count, bytes, current: "done".to_string() },
    )
    .ok();

    Ok(count)
}
