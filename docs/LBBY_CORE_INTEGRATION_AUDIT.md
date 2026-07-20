# lbby-core Integration Audit (PLATFORM-010)

## 1. Current Public API Surface

### lib.rs Re-exports
- `AppState`, `ActionResult`, `AppEventSender`, `BannedIp`, `BannedPlayer`, `ModInfo`, `OperationKind`, `PregenState`, `ShutdownStatus`, `WhitelistEntry`
- `Game`, `ServerConfig`, `ServerType`
- `SafetyError`
- `remote_kill_server_and_playit`
- `PlayitState`
- `ServerManager`, `ServerStatus`
- `ServerStats`

### Key Modules
| Module | Purpose | Public API |
|--------|---------|-----------|
| `server.rs` | Server lifecycle (install/start/stop/restart) | `start_server()`, `stop_server()`, `do_install_server()`, `send_command()` |
| `config.rs` | Profile-based config (JSON on disk) | `load_config()`, `save_config()`, `active_profile_id()` |
| `java.rs` | Java discovery + auto-download | `required_java_for_mc()`, `find_java_with_version()`, `ensure_java()`, `detect_java_major()` |
| `version_fetch.rs` | MC/Loader version APIs | `fetch_mc_versions()`, `fetch_paper_versions()`, `fetch_paper_builds()`, etc. |
| `app_state.rs` | Shared state + event sender | `AppState::new()`, `AppEventSender::new()` |
| `helpers.rs` | Download, port check, progress | `download_to_file()`, `check_port_available()` |
| `errors.rs` | Structured error types | `SafetyError` enum with error codes |
| `backup.rs` | Backup/restore | Backup creation and restoration |

## 2. How to Call Server Install/Start/Stop

### Install Server
```rust
// Requires AppEventSender (for progress events) and ServerConfig
let app = Arc::new(AppEventSender::new(Arc::new(AppState::new())));
let cfg = ServerConfig { minecraft_version: "1.21".into(), server_type: ServerType::Paper, ... };
let result = do_install_server(app, cfg).await;
```

### Start Server
```rust
// Reads config from disk (profiles.json), requires AppEventSender
let app = Arc::new(AppEventSender::new(Arc::new(AppState::new())));
start_server(app).await;
```

### Stop Server
```rust
// Reads state from AppState.server mutex
stop_server(app).await;
```

### Key Coupling Points
1. **`do_start_server()`** calls `crate::config::load_config()` — reads from `~/.config/lbby/profiles.json`
2. **`do_install_server()`** takes `ServerConfig` directly — more flexible
3. **`ensure_java()`** takes `&Arc<AppEventSender>` — used for progress events only
4. **`download_to_file()`** takes `&Arc<AppEventSender>` — progress events
5. **All state** lives in `AppState` mutexes — no way to query without holding the lock

## 3. Minecraft Capabilities

### Supported Server Types
- Vanilla, Paper, Forge, Fabric, NeoForge, Bukkit, Spigot, Folia, Purpur, SpongeVanilla, SpongeForge

### Version Fetching
- Mojang manifest for vanilla versions
- PaperMC API v2 for Paper/Folia builds
- Forge promotions API
- Fabric meta API
- NeoForge Maven API
- Purpur API
- Sponge download API

### Installation Flow (Paper example)
1. Resolve Java version from MC version (`required_java_for_mc`)
2. Find or download Java (`ensure_java` → Adoptium API)
3. Fetch Paper build info from PaperMC API
4. Download server.jar
5. Write eula.txt, server.properties, create plugins/ dir
6. Validate server.jar exists and is >1KB

## 4. Java Discovery

### `java.rs` Functions
- `required_java_for_mc(mc_version) -> u8` — maps MC version to Java major (8/17/21)
- `java_candidates() -> Vec<PathBuf>` — scans JAVA_HOME, /usr/bin/java, SDKMAN, /Library/Java/JavaVirtualMachines, Windows paths
- `find_java_with_version(major) -> Option<PathBuf>` — finds matching JDK
- `detect_java_major(bin) -> Option<u8>` — runs `java -version` and parses
- `ensure_java(major, app) -> Result<PathBuf>` — find or download from Adoptium
- `bundled_java_dir(major) -> PathBuf` — `~/.local/share/lbby/java/temurin-{major}`
- `bundled_java_bin(major) -> PathBuf` — platform-specific binary path

### Coupling
`ensure_java()` requires `&Arc<AppEventSender>` for progress events. The actual download/extraction logic is self-contained.

## 5. Process Launch Workflow

### Start Flow (from `do_start_server`)
1. Load config from disk
2. Check server not already running
3. Mark status = Starting
4. Check port availability
5. Create server directory
6. Handle Forge/NeoForge JVM args
7. Clean stale session.lock
8. Resolve Java (find or download)
9. Build command: `java -Xmx{}M -Xms{}M -jar server.jar nogui`
10. Spawn with piped stdin/stdout/stderr
11. Store stdin handle in `ServerManager`
12. Spawn stdout reader task (detects "Done" line, parses TPS/player events)
13. Spawn wait task (detects exit, triggers auto-restart if configured)

### Stop Flow
1. Send "stop" command to stdin (graceful)
2. Wait up to 10 seconds
3. SIGKILL if grace period expires
4. Update status to Stopped

## 6. Configuration Workflow

### Profile System
- Config stored in `~/.config/lbby/profiles.json`
- Multiple profiles, each with isolated `server_path`
- Active profile ID tracked
- `load_config()` reads active profile's config
- `save_config()` writes back

### ServerConfig Fields (relevant to node_api)
- `server_path` — where server files live
- `java_path` — explicit Java binary path
- `minecraft_version` — e.g. "1.21"
- `server_type` — Vanilla/Paper/Forge/etc.
- `loader_version` — Forge/Fabric/Paper build
- `ram_mb` — JVM heap
- `max_players` — server.properties
- `setup_complete` — whether install succeeded
- `optimized_jvm_flags` — Aikar's flags

## 7. Error Model

### SafetyError (structured)
- `ServerRunning`, `ServerStarting`, `RestartPending`, `ConflictingOperation`
- `UnsafeArchiveEntry`, `InvalidArchive`, `RestoreValidationFailed`
- `PathOwnershipViolation`, `OperationCancelled`, `CrashLoopBlocked`
- Each has a stable `error_code()` string for frontend

### General Errors
- Most functions return `Result<T, String>` — ad-hoc error strings
- `do_install_server` returns `Result<ServerConfig, String>`
- `start_server` returns `Result<(), String>`

## 8. API Gaps for Node Integration

### Critical Gaps
1. **No standalone install function** — `do_install_server` needs `AppEventSender` for progress
2. **No standalone start function** — `do_start_server` reads config from disk via `load_config()`
3. **No standalone stop function** — `stop_server` reads state from `AppState` mutex
4. **No way to start with explicit parameters** — always reads from profile config
5. **No process handle return** — start spawns async tasks, doesn't return a Child handle
6. **AppState is monolithic** — contains unrelated state (playit, cloudflare, stats, console buffer)

### Feasible Workarounds
1. **No-op AppEventSender** — create a sender that discards events (progress not needed for node)
2. **Parameter-based install** — wrap `install_paper()` with explicit args instead of ServerConfig
3. **Direct Java command** — build the `java -jar server.jar` command without going through `do_start_server`
4. **Direct process management** — node already has its own ProcessSupervisor; just need to spawn Java

### Recommended Approach
Create `node_api.rs` that:
1. Uses existing `java.rs` functions directly (they're mostly standalone)
2. Uses existing `version_fetch.rs` functions (standalone async)
3. Creates a minimal paper download function (extracted from `install_paper`)
4. Builds and spawns Java command directly (bypassing `do_start_server`)
5. Uses `ensure_java` with a no-op event sender

This avoids rewriting lbby-core while providing a clean integration boundary.
