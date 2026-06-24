// Debug report exporter — collects app info, system specs, server logs, and crash reports
// into a single ZIP for easier troubleshooting.
//
// Output: debug-report-DD-MM-YYYY.zip
//
// Contents:
//   - app-info.txt    — version, locale, platform, server config (sanitized)
//   - system-info.txt — OS, CPU, RAM, disk usage
//   - server.properties (copied from server folder, if exists)
//   - logs/           — last 5 most recent log files from <server>/logs/
//   - crash-reports/  — copied from <server>/crash-reports/ if it exists
//   - playit.toml-redacted — playit secret file with the secret_key value redacted

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use sysinfo::{Disks, System};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::config::ServerConfig;

pub fn timestamp_ddmmyyyy() -> String {
    chrono::Local::now().format("%d-%m-%Y").to_string()
}

pub fn default_debug_filename() -> String {
    format!("debug-report-{}.zip", timestamp_ddmmyyyy())
}

pub fn export_debug_report(
    cfg: &ServerConfig,
    dest_zip: &Path,
) -> Result<u64, String> {
    if let Some(parent) = dest_zip.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = File::create(dest_zip).map_err(|e| format!("Cannot create zip: {}", e))?;
    let mut zip = ZipWriter::new(file);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut files: u64 = 0;

    // app-info.txt
    let app_info = build_app_info(cfg);
    write_text(&mut zip, "app-info.txt", &app_info, opts)?;
    files += 1;

    // system-info.txt
    let sys_info = build_system_info();
    write_text(&mut zip, "system-info.txt", &sys_info, opts)?;
    files += 1;

    // Copy server.properties + last few logs + crash reports
    let server_path = Path::new(&cfg.server_path);
    if server_path.exists() {
        // server.properties
        let props_path = server_path.join("server.properties");
        if props_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&props_path) {
                write_text(&mut zip, "server.properties", &content, opts)?;
                files += 1;
            }
        }

        // user_jvm_args.txt (Forge)
        let jvm = server_path.join("user_jvm_args.txt");
        if jvm.exists() {
            if let Ok(content) = std::fs::read_to_string(&jvm) {
                write_text(&mut zip, "user_jvm_args.txt", &content, opts)?;
                files += 1;
            }
        }

        // logs/ — last 5 files by mtime
        let logs_dir = server_path.join("logs");
        if logs_dir.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&logs_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .collect();
            entries.sort_by_key(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
            });
            entries.reverse();
            for entry in entries.into_iter().take(5) {
                let name = entry.file_name();
                let name_str = name.to_string_lossy().to_string();
                let dest = format!("logs/{}", name_str);
                if let Ok(()) = copy_file_into_zip(&mut zip, entry.path(), &dest, opts) {
                    files += 1;
                }
            }
        }

        // crash-reports/
        let crash_dir = server_path.join("crash-reports");
        if crash_dir.is_dir() {
            for entry in std::fs::read_dir(&crash_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
            {
                if !entry.path().is_file() {
                    continue;
                }
                let name = entry.file_name();
                let dest = format!("crash-reports/{}", name.to_string_lossy());
                if let Ok(()) = copy_file_into_zip(&mut zip, entry.path(), &dest, opts) {
                    files += 1;
                }
            }
        }
    }

    // playit.toml (redacted)
    let playit_path = crate::playit::secret_file_path();
    if playit_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&playit_path) {
            let redacted = redact_secret(&content);
            write_text(&mut zip, "playit.toml-redacted", &redacted, opts)?;
            files += 1;
        }
    }

    zip.finish().map_err(|e| format!("Finalize zip: {}", e))?;
    Ok(files)
}

fn redact_secret(toml: &str) -> String {
    toml.lines()
        .map(|l| {
            let trimmed = l.trim_start();
            if trimmed.starts_with("secret_key") {
                "secret_key = \"<REDACTED>\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_text<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    body: &str,
    opts: SimpleFileOptions,
) -> Result<(), String> {
    zip.start_file(name, opts)
        .map_err(|e| format!("start_file {}: {}", name, e))?;
    zip.write_all(body.as_bytes())
        .map_err(|e| format!("write {}: {}", name, e))?;
    Ok(())
}

fn copy_file_into_zip<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    src: std::path::PathBuf,
    dest_name: &str,
    opts: SimpleFileOptions,
) -> Result<(), String> {
    zip.start_file(dest_name, opts)
        .map_err(|e| format!("start_file {}: {}", dest_name, e))?;
    let mut f = File::open(&src).map_err(|e| format!("open {}: {}", src.display(), e))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        zip.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn build_app_info(cfg: &ServerConfig) -> String {
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z").to_string();
    format!(
        "Lbby Debug Report\n\
         ========================\n\
         Generated: {when}\n\
         App version: {ver}\n\
         OS: {os} ({family} / {arch})\n\
         \n\
         === Server Config ===\n\
         server_type: {st:?}\n\
         minecraft_version: {mc}\n\
         loader_version: {lv}\n\
         ram_mb: {ram}\n\
         max_players: {mp}\n\
         server_name: {sn}\n\
         setup_complete: {sc}\n\
         server_path: {sp}\n\
         java_path: {jp}\n",
        when = when,
        ver = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        family = std::env::consts::FAMILY,
        arch = std::env::consts::ARCH,
        st = cfg.server_type,
        mc = cfg.minecraft_version,
        lv = cfg.loader_version.as_deref().unwrap_or("(none)"),
        ram = cfg.ram_mb,
        mp = cfg.max_players,
        sn = cfg.server_name,
        sc = cfg.setup_complete,
        sp = cfg.server_path,
        jp = cfg.java_path,
    )
}

fn build_system_info() -> String {
    let mut sys = System::new_all();
    sys.refresh_all();
    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let hostname = System::host_name().unwrap_or_else(|| "unknown".to_string());
    let os_long = System::long_os_version().unwrap_or_else(|| "unknown".to_string());
    let kernel = System::kernel_version().unwrap_or_else(|| "unknown".to_string());

    let disks = Disks::new_with_refreshed_list();
    let disks_str = disks
        .iter()
        .map(|d| {
            format!(
                "  - {} mounted at {} ({} MB free / {} MB total)",
                d.name().to_string_lossy(),
                d.mount_point().display(),
                d.available_space() / 1024 / 1024,
                d.total_space() / 1024 / 1024,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "=== System Info ===\n\
         Hostname: {hostname}\n\
         OS: {os_long}\n\
         Kernel: {kernel}\n\
         CPU: {cpu_brand}\n\
         CPU cores (physical): {phys}\n\
         CPU cores (logical): {logi}\n\
         Total RAM: {total_ram} MB\n\
         Used RAM: {used_ram} MB\n\
         Free RAM: {free_ram} MB\n\
         \n\
         === Disks ===\n{disks_str}\n",
        phys = sys.physical_core_count().unwrap_or(0),
        logi = sys.cpus().len(),
        total_ram = sys.total_memory() / 1024 / 1024,
        used_ram = sys.used_memory() / 1024 / 1024,
        free_ram = sys.available_memory() / 1024 / 1024,
    )
}
