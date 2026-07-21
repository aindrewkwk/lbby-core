use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModLoaderKind {
    Forge,
    NeoForge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModLoaderLaunch {
    Script(PathBuf),
    LegacyJar(PathBuf),
}

impl ModLoaderKind {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
        }
    }

    fn library_dir(self, server_dir: &Path, version_key: &str) -> PathBuf {
        match self {
            Self::Forge => server_dir
                .join("libraries/net/minecraftforge/forge")
                .join(version_key),
            Self::NeoForge => server_dir
                .join("libraries/net/neoforged/neoforge")
                .join(version_key),
        }
    }
}

/// Resolve the launch artifact produced by a Forge-family installer.
///
/// Forge 1.17+ and NeoForge use generated run scripts plus an argument file
/// below `libraries/`. Older Forge releases use a root launcher jar. A root
/// `server.jar` is deliberately not required for the modern layout.
pub fn detect_modloader_launch(
    server_dir: &Path,
    kind: ModLoaderKind,
    version_key: &str,
) -> Result<ModLoaderLaunch, String> {
    let script_name = if cfg!(target_os = "windows") {
        "run.bat"
    } else {
        "run.sh"
    };
    let args_name = if cfg!(target_os = "windows") {
        "win_args.txt"
    } else {
        "unix_args.txt"
    };
    let script = server_dir.join(script_name);
    let args_file = kind.library_dir(server_dir, version_key).join(args_name);
    if script.is_file() && args_file.is_file() {
        return Ok(ModLoaderLaunch::Script(script));
    }

    let prefix = kind.prefix();
    for name in [
        format!("{prefix}-{version_key}.jar"),
        format!("{prefix}-{version_key}-universal.jar"),
    ] {
        let jar = server_dir.join(name);
        if jar.is_file() {
            return Ok(ModLoaderLaunch::LegacyJar(jar));
        }
    }

    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(server_dir) {
        found.extend(
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned()),
        );
    }
    found.sort();
    Err(format!(
        "{} installation is incomplete: expected {script_name} with {} or a legacy launcher jar in {}. Files found: {:?}",
        kind.prefix(),
        args_file.display(),
        server_dir.display(),
        found
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lbby-forge-test-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn accepts_modern_forge_without_server_jar() {
        let dir = temp_dir("modern");
        let version = "1.20.1-47.4.10";
        let script = if cfg!(target_os = "windows") {
            dir.join("run.bat")
        } else {
            dir.join("run.sh")
        };
        let args = if cfg!(target_os = "windows") {
            "win_args.txt"
        } else {
            "unix_args.txt"
        };
        std::fs::write(&script, "").unwrap();
        let args_path = dir
            .join("libraries/net/minecraftforge/forge")
            .join(version)
            .join(args);
        std::fs::create_dir_all(args_path.parent().unwrap()).unwrap();
        std::fs::write(args_path, "").unwrap();

        assert!(matches!(
            detect_modloader_launch(&dir, ModLoaderKind::Forge, version),
            Ok(ModLoaderLaunch::Script(_))
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn accepts_legacy_forge_jar() {
        let dir = temp_dir("legacy");
        let version = "1.16.5-36.2.39";
        std::fs::write(dir.join(format!("forge-{version}.jar")), b"jar").unwrap();
        assert!(matches!(
            detect_modloader_launch(&dir, ModLoaderKind::Forge, version),
            Ok(ModLoaderLaunch::LegacyJar(_))
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_partial_modern_layout() {
        let dir = temp_dir("partial");
        let script = if cfg!(target_os = "windows") {
            dir.join("run.bat")
        } else {
            dir.join("run.sh")
        };
        std::fs::write(script, "").unwrap();
        assert!(detect_modloader_launch(&dir, ModLoaderKind::Forge, "1.20.1-47.4.10").is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
