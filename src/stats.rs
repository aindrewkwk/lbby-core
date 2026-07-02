use serde::{Deserialize, Serialize};
use std::path::Path;
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerStats {
    pub cpu_percent: f32,
    pub ram_used_mb: u64,
    pub ram_max_mb: u32,
    pub disk_read_kb_s: f32,
    pub disk_write_kb_s: f32,
    pub disk_used_mb: u64,
    pub disk_total_mb: u64,
    /// 0.0 = unknown / not yet sampled / unsupported server type
    pub tps: f32,
    pub players_online: u32,
    pub players_max: u32,
    pub uptime_seconds: u64,
    /// Number of mod files in the mods/ or plugins/ directory (.jar for Minecraft, .tmod for Terraria).
    pub mods_count: u32,
    /// Total size of the world/ directory in MB.
    pub world_size_mb: u64,
    /// Human-readable loader name for frontend theming (e.g. "Forge", "Paper").
    pub loader_label: String,
    /// Current server status (stopped, starting, running, stopping)
    pub status: String,
    /// Active server profile/config name
    pub server_name: String,
}

/// Polls one process tree by PID and computes deltas for disk I/O against the previous sample.
/// Also samples free space on the disk that contains the server directory.
pub struct StatsPoller {
    sys: System,
    last_disk_read: u64,
    last_disk_write: u64,
    last_sample: std::time::Instant,
}

impl StatsPoller {
    pub fn new() -> Self {
        Self {
            sys: System::new(),
            last_disk_read: 0,
            last_disk_write: 0,
            last_sample: std::time::Instant::now(),
        }
    }

    /// Sample CPU/RAM/disk-I/O for the process tree rooted at `root_pid`,
    /// plus disk-space usage for the volume containing `server_path`.
    /// Returns (cpu%, ram_mb, disk_read_kb_s, disk_write_kb_s, disk_used_mb, disk_total_mb).
    pub fn sample(&mut self, root_pid: u32, server_path: &Path) -> Option<(f32, u64, f32, f32, u64, u64)> {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_memory()
                .with_disk_usage(),
        );

        // PID-reuse guard: before we trust `root_pid`, verify the process at
        // that PID is still ours. The MC server is launched as either
        // `bash run.sh nogui` (Forge / NeoForge) or `java -jar server.jar`
        // (Vanilla / Paper / Fabric). If the PID got recycled by the OS for an
        // unrelated process (e.g. a cloudflared zombie) the command line
        // won't match any of those, and we should report no stats — better
        // than rendering the dashboard with someone else's CPU usage.
        let root = Pid::from_u32(root_pid);
        // PID-reuse guard. Goal: reject obvious mismatches (PID recycled into
        // an unrelated process) without false-rejecting legitimate
        // `bash run.sh` — sysinfo's `cmd()` returns empty for those on macOS.
        // Strategy: only reject when we have *positive evidence* the process
        // is something else. Empty/unknown signals → trust the PID, since the
        // wait task would flip status to Stopped if it had actually exited.
        let still_ours = match self.sys.process(root) {
            Some(p) => {
                let name = p.name().to_str().unwrap_or("").to_string();
                let exe = p
                    .exe()
                    .and_then(|e| e.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let cmd_joined: String = p
                    .cmd()
                    .iter()
                    .filter_map(|os| os.to_str())
                    .collect::<Vec<_>>()
                    .join(" ");

                let name_lc = name.to_ascii_lowercase();

                // Known-bad names: processes that have shown up in PID-reuse
                // incidents and we'd never have spawned as the server root.
                let known_bad = matches!(
                    name_lc.as_str(),
                    "cloudflared" | "playit" | "ngrok" | "node" | "python" | "python3"
                );

                if known_bad {
                    eprintln!(
                        "[lbby] stats sampler: PID {} reused by '{}' (cmd: '{}'); reporting no stats",
                        root_pid, name, cmd_joined
                    );
                    false
                } else {
                    let _ = (exe, cmd_joined); // kept for future diagnostics
                    true
                }
            }
            None => false,
        };
        if !still_ours {
            return None;
        }

        // BFS through processes to find root + descendants
        let mut wanted: std::collections::HashSet<Pid> = std::collections::HashSet::new();
        wanted.insert(root);
        let mut changed = true;
        while changed {
            changed = false;
            for (pid, proc_) in self.sys.processes() {
                if let Some(parent) = proc_.parent() {
                    if wanted.contains(&parent) && !wanted.contains(pid) {
                        wanted.insert(*pid);
                        changed = true;
                    }
                }
            }
        }

        let mut total_cpu = 0.0;
        let mut total_ram: u64 = 0;
        let mut total_disk_read: u64 = 0;
        let mut total_disk_written: u64 = 0;
        let mut found_any = false;

        for pid in &wanted {
            if let Some(proc_) = self.sys.process(*pid) {
                total_cpu += proc_.cpu_usage();
                total_ram += proc_.memory();
                let du = proc_.disk_usage();
                total_disk_read += du.total_read_bytes;
                total_disk_written += du.total_written_bytes;
                found_any = true;
            }
        }
        if !found_any { return None; }

        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_sample).as_secs_f32().max(0.001);
        let read_delta = total_disk_read.saturating_sub(self.last_disk_read);
        let write_delta = total_disk_written.saturating_sub(self.last_disk_write);
        let read_kb_s = (read_delta as f32 / 1024.0) / elapsed;
        let write_kb_s = (write_delta as f32 / 1024.0) / elapsed;

        let first_sample = self.last_disk_read == 0 && self.last_disk_write == 0;
        self.last_disk_read = total_disk_read;
        self.last_disk_write = total_disk_written;
        self.last_sample = now;

        // Disk space: find the volume mount point containing server_path
        let (used_mb, total_mb) = disk_space_for_path(server_path).unwrap_or((0, 0));

        Some((
            total_cpu,
            total_ram / 1024 / 1024,
            if first_sample { 0.0 } else { read_kb_s },
            if first_sample { 0.0 } else { write_kb_s },
            used_mb,
            total_mb,
        ))
    }
}

/// Find the disk volume containing `path` and return (used_mb, total_mb).
fn disk_space_for_path(path: &Path) -> Option<(u64, u64)> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let disks = Disks::new_with_refreshed_list();
    // Pick the deepest mount point that is a prefix of our canonical path
    let mut best: Option<(u64, u64, usize)> = None;
    for disk in &disks {
        let mp = disk.mount_point();
        if canon.starts_with(mp) {
            let total = disk.total_space();
            let avail = disk.available_space();
            let used = total.saturating_sub(avail);
            let depth = mp.as_os_str().len();
            if best.map(|(_, _, d)| depth > d).unwrap_or(true) {
                best = Some((used / 1024 / 1024, total / 1024 / 1024, depth));
            }
        }
    }
    best.map(|(u, t, _)| (u, t))
}

/// Count mod files in the given directory (non-recursive).
/// Counts .jar files (Minecraft) and .tmod files (Terraria/tModLoader).
pub fn count_mods(dir: &Path) -> u32 {
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let ext = e.path().extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                    (ext == "jar" || ext == "tmod")
                        && e.path().file_stem().is_some_and(|n| {
                            let name = n.to_string_lossy().to_ascii_lowercase();
                            !name.starts_with('.') && !name.starts_with("original-")
                        })
                })
                .count() as u32
        })
        .unwrap_or(0)
}

/// Recursively sum the size of all files in a directory (in MB).
pub fn dir_size_mb(dir: &Path) -> u64 {
    if !dir.exists() {
        return 0;
    }
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = path.metadata() {
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += dir_size_mb_bytes(&path);
                }
            }
        }
    }
    total / 1024 / 1024
}

/// Internal helper that returns bytes instead of MB (avoids double division).
fn dir_size_mb_bytes(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = path.metadata() {
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += dir_size_mb_bytes(&path);
                }
            }
        }
    }
    total
}
