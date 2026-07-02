use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde::Deserialize;


/// Map a Minecraft version like "1.20.1" to the Java major version that should run it.
/// Conservative — picks the highest Java the version is well-tested with.
pub fn required_java_for_mc(mc_version: &str) -> u8 {
    // Parse "1.X.Y" → minor = X
    let minor = mc_version
        .strip_prefix("1.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(20);

    // Minor patch (the .Y part) — needed to distinguish 1.20.4 from 1.20.5+
    let patch = mc_version
        .strip_prefix("1.")
        .and_then(|rest| rest.split('.').nth(1))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    match minor {
        0..=16 => 8,
        17 => 17,
        18 | 19 => 17,
        20 if patch <= 4 => 17,
        20 => 21,        // 1.20.5+
        _ => 21,         // 1.21.x and beyond
    }
}

fn push_unique(candidates: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        candidates.push(path);
    }
}

fn java_bin_from_home(home: impl AsRef<Path>) -> PathBuf {
    #[cfg(target_os = "windows")]
    return home.as_ref().join("bin\\java.exe");
    #[cfg(not(target_os = "windows"))]
    return home.as_ref().join("bin/java");
}

pub fn java_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![];
    let mut seen: HashSet<PathBuf> = HashSet::new();

    if let Ok(jh) = std::env::var("JAVA_HOME") {
        push_unique(&mut candidates, &mut seen, java_bin_from_home(jh));
    }

    #[cfg(target_os = "macos")]
    {
        push_unique(&mut candidates, &mut seen, PathBuf::from("/usr/bin/java"));
        if let Ok(home) = std::env::var("HOME") {
            push_unique(
                &mut candidates,
                &mut seen,
                PathBuf::from(format!("{}/.sdkman/candidates/java/current/bin/java", home)),
            );
        }
        if let Ok(entries) = std::fs::read_dir("/Library/Java/JavaVirtualMachines") {
            for entry in entries.flatten() {
                push_unique(
                    &mut candidates,
                    &mut seen,
                    entry.path().join("Contents/Home/bin/java"),
                );
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let mut where_cmd = std::process::Command::new("where");
        where_cmd.arg("java");
        crate::helpers::hide_std_child_window(&mut where_cmd);
        if let Ok(out) = where_cmd.output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let path = line.trim();
                if !path.is_empty() {
                    push_unique(&mut candidates, &mut seen, PathBuf::from(path));
                }
            }
        }

        for env_name in ["PROGRAMFILES", "ProgramW6432", "ProgramFiles(x86)"] {
            let Ok(pf) = std::env::var(env_name) else {
                continue;
            };
            for vendor in [
                "Java",
                "Eclipse Adoptium",
                "Microsoft",
                "Amazon Corretto",
                "Zulu",
                "BellSoft",
                "Semeru",
            ] {
                if let Ok(entries) = std::fs::read_dir(PathBuf::from(&pf).join(vendor)) {
                    for entry in entries.flatten() {
                        push_unique(&mut candidates, &mut seen, java_bin_from_home(entry.path()));
                    }
                }
            }
        }

        for shim in [
            "C:\\ProgramData\\Oracle\\Java\\javapath\\java.exe",
            "C:\\Program Files\\Common Files\\Oracle\\Java\\javapath\\java.exe",
            "C:\\Program Files (x86)\\Common Files\\Oracle\\Java\\java8path\\java.exe",
        ] {
            push_unique(&mut candidates, &mut seen, PathBuf::from(shim));
        }

        if let Some(local) = dirs::data_local_dir() {
            for vendor in ["Programs\\Eclipse Adoptium", "Programs\\Java"] {
                if let Ok(entries) = std::fs::read_dir(local.join(vendor)) {
                    for entry in entries.flatten() {
                        push_unique(&mut candidates, &mut seen, java_bin_from_home(entry.path()));
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let mut which_cmd = std::process::Command::new("which");
        which_cmd.arg("java");
        crate::helpers::hide_std_child_window(&mut which_cmd);
        if let Ok(out) = which_cmd.output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let path = line.trim();
                if !path.is_empty() {
                    push_unique(&mut candidates, &mut seen, PathBuf::from(path));
                }
            }
        }
        push_unique(&mut candidates, &mut seen, PathBuf::from("/usr/bin/java"));
        push_unique(&mut candidates, &mut seen, PathBuf::from("/usr/local/bin/java"));
    }

    candidates
}

/// Find an installed JDK matching the requested major version.
/// Searches app-bundled Java, JAVA_HOME, common install dirs, and known shims.
pub fn find_java_with_version(major: u8) -> Option<PathBuf> {
    let mut candidates = vec![bundled_java_bin(major)];
    candidates.extend(java_candidates());

    for path in candidates {
        if detect_java_major(&path) == Some(major) {
            return Some(path);
        }
    }
    None
}

pub fn find_any_java() -> Option<(PathBuf, u8)> {
    for path in java_candidates() {
        if let Some(major) = detect_java_major(&path) {
            return Some((path, major));
        }
    }
    None
}

/// Returns JAVA_HOME for a java binary path (the dir containing bin/java).
pub fn java_home_from_bin(bin: &std::path::Path) -> Option<PathBuf> {
    bin.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf())
}

/// Run `<java_bin> -version` and parse the major version number out of the
/// version string. Returns `None` if the binary doesn't exist, doesn't run,
/// or doesn't look like Java.
///
/// Java 8 prints `1.8.0_xxx`, Java 9+ prints `9.x.y` / `17.x.y` / `26+35-...`.
pub fn detect_java_major(bin: &std::path::Path) -> Option<u8> {
    if !bin.exists() {
        return None;
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("-version");
    crate::helpers::hide_std_child_window(&mut cmd);
    let out = cmd.output().ok()?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    for line in combined.lines() {
        if !line.contains("version") {
            continue;
        }
        // Some formats use quotes (`java version "17.0.10"`), some don't
        // (`openjdk 26 2026-03-17`). Handle both.
        let candidate = if let Some(start) = line.find('"') {
            let rest = &line[start + 1..];
            rest.split('"').next().unwrap_or("")
        } else {
            // pull the first token that starts with a digit
            line.split_whitespace()
                .find(|t| t.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
                .unwrap_or("")
        };
        if candidate.is_empty() {
            continue;
        }
        let major: Option<u8> = if candidate.starts_with("1.") {
            candidate.split('.').nth(1).and_then(|s| s.parse().ok())
        } else {
            // "17.0.10" → 17, "26+35-2893" → 26
            let head: String = candidate.chars().take_while(|c| c.is_ascii_digit()).collect();
            head.parse().ok()
        };
        if let Some(m) = major {
            return Some(m);
        }
    }
    None
}

pub fn bundled_java_dir(major: u8) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby")
        .join("java")
        .join(format!("temurin-{}", major))
}

// ── Adoptium JRE auto-download ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AdoptiumAsset {
    binary: AdoptiumBinary,
}

#[derive(Debug, Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}

#[derive(Debug, Deserialize)]
struct AdoptiumPackage {
    name: String,
    link: String,
}

fn adoptium_os() -> &'static str {
    #[cfg(target_os = "macos")]
    return "mac";
    #[cfg(target_os = "windows")]
    return "windows";
    #[cfg(target_os = "linux")]
    return "linux";
}

fn adoptium_arch() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    return "aarch64";
    #[cfg(target_arch = "x86_64")]
    return "x64";
    #[cfg(target_arch = "x86")]
    return "x86";
}

pub fn bundled_java_bin(major: u8) -> PathBuf {
    let dir = bundled_java_dir(major);
    #[cfg(target_os = "macos")]
    return dir.join("Contents/Home/bin/java");
    #[cfg(target_os = "windows")]
    return dir.join("bin/java.exe");
    #[cfg(target_os = "linux")]
    return dir.join("bin/java");
}

/// Download a JRE from the Adoptium API and extract it to `bundled_java_dir(major)`.
/// Emits progress events via the Tauri app handle.
async fn download_jre(major: u8, app: &std::sync::Arc<crate::app_state::AppEventSender>) -> Result<PathBuf, String> {
    let os = adoptium_os();
    let arch = adoptium_arch();
    let api_url = format!(
        "https://api.adoptium.net/v3/assets/latest/{}/hotspot?architecture={}&image_type=jre&os={}&vendor=eclipse",
        major, arch, os
    );

    // Query Adoptium for the download URL
    let client = reqwest::Client::new();
    let assets: Vec<AdoptiumAsset> = client
        .get(&api_url)
        .send()
        .await
        .map_err(|e| format!("Failed to query Adoptium API: {}", e))?
        .json()
        .await
        .map_err(|e| format!("Failed to parse Adoptium response: {}", e))?;

    let asset = assets.first()
        .ok_or_else(|| format!("No JRE {} available for {}/{}", major, os, arch))?;
    let download_url = &asset.binary.package.link;
    let file_name = &asset.binary.package.name;

    // Download to temp file
    let temp_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby")
        .join("java");
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let temp_file = temp_dir.join(format!(".download-{}", file_name));

    crate::helpers::download_to_file(app, download_url, &temp_file, &format!("Java {}", major)).await?;

    // Extract
    app.emit("install-progress", crate::helpers::InstallProgress {
        stage: "extract".into(),
        label: format!("Extracting Java {}…", major),
        current: 90,
        total: 100,
    }).ok();

    let dest = bundled_java_dir(major);
    // Clean up any previous partial install
    if dest.exists() {
        std::fs::remove_dir_all(&dest).ok();
    }

    #[cfg(target_os = "macos")]
    extract_tar_gz(&temp_file, &temp_dir, &dest, major)?;

    #[cfg(target_os = "windows")]
    extract_zip(&temp_file, &temp_dir, &dest, major)?;

    #[cfg(target_os = "linux")]
    extract_tar_gz(&temp_file, &temp_dir, &dest, major)?;

    // Clean up temp file
    std::fs::remove_file(&temp_file).ok();

    // Verify the binary exists
    let bin = bundled_java_bin(major);
    if !bin.exists() {
        return Err(format!("Java {} extracted but binary not found at {}", major, bin.display()));
    }

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&bin) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).ok();
        }
    }

    Ok(bin)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn extract_tar_gz(archive: &PathBuf, temp_dir: &PathBuf, dest: &PathBuf, major: u8) -> Result<(), String> {
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("Failed to open archive: {}", e))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);

    // Extract to a temp staging dir first, then find the JRE root
    let staging = temp_dir.join(format!(".staging-{}", major));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).ok();
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    tar.unpack(&staging).map_err(|e| format!("Failed to extract tar: {}", e))?;

    // Adoptium tarballs contain a single top-level dir like "jdk-17.0.19+10-jre"
    // We need to find the actual JRE root and move it to dest
    let entries: Vec<_> = std::fs::read_dir(&staging)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .collect();

    if entries.len() == 1 && entries[0].path().is_dir() {
        // Single directory inside — rename it to dest
        std::fs::rename(entries[0].path(), dest)
            .map_err(|e| format!("Failed to move JRE: {}", e))?;
    } else {
        // Multiple entries — rename staging to dest
        std::fs::rename(&staging, dest)
            .map_err(|e| format!("Failed to move JRE: {}", e))?;
        return Ok(());
    }

    // Clean up staging
    std::fs::remove_dir_all(&staging).ok();
    Ok(())
}

#[cfg(target_os = "windows")]
fn extract_zip(archive: &PathBuf, temp_dir: &PathBuf, dest: &PathBuf, major: u8) -> Result<(), String> {
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("Failed to open archive: {}", e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("Failed to read zip: {}", e))?;

    let staging = temp_dir.join(format!(".staging-{}", major));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).ok();
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    zip.extract(&staging).map_err(|e| format!("Failed to extract zip: {}", e))?;

    // Adoptium zips contain a single top-level dir like "jdk-17.0.19+10-jre"
    let entries: Vec<_> = std::fs::read_dir(&staging)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .collect();

    if entries.len() == 1 && entries[0].path().is_dir() {
        std::fs::rename(entries[0].path(), dest)
            .map_err(|e| format!("Failed to move JRE: {}", e))?;
    } else {
        std::fs::rename(&staging, dest)
            .map_err(|e| format!("Failed to move JRE: {}", e))?;
        return Ok(());
    }

    std::fs::remove_dir_all(&staging).ok();
    Ok(())
}

/// Ensure a bundled JRE for the given major version is available.
/// First checks if one already exists (system or bundled). If not, downloads
/// from Adoptium. Returns the path to the java binary.
pub async fn ensure_java(major: u8, app: &std::sync::Arc<crate::app_state::AppEventSender>) -> Result<PathBuf, String> {
    // 1. Check if we already have a matching Java (system or bundled)
    if let Some(path) = find_java_with_version(major) {
        return Ok(path);
    }

    // 2. Download from Adoptium
    download_jre(major, app).await
}
