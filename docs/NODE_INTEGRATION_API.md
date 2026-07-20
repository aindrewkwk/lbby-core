# lbby-core Node Integration API

## Overview

The `node_api` module provides a clean, stateless API for lbby-node to manage Minecraft server instances without needing the full `AppState` / profile system that lbby-core's original API requires.

## Why a Separate Module?

lbby-core's original functions (`start_server`, `do_install_server`, `stop_server`) are tightly coupled to:
- `AppState` (shared mutex-based state for the Tauri app)
- `AppEventSender` (broadcast channel for UI events)
- Profile-based config system (`~/.config/lbby/profiles.json`)

lbby-node needs to manage multiple independent instances on a headless server, so we need parameter-based functions that don't read from disk-based config.

## Public API

### Types

```rust
/// Specification for a Minecraft server instance.
pub struct MinecraftSpec {
    pub minecraft_version: String,  // e.g. "1.21.4"
    pub distribution: String,       // "paper", "vanilla", "folia"
    pub loader_version: Option<String>,
    pub ram_mb: u32,
    pub max_players: u32,
    pub server_name: String,
    pub game_port: u16,
}

/// Result of preparing a Minecraft instance.
pub struct PreparedInstance {
    pub instance_dir: PathBuf,
    pub java_bin: PathBuf,
    pub java_major: u8,
    pub server_jar: PathBuf,
    pub minecraft_version: String,
    pub distribution: String,
    pub loader_version: Option<String>,
}

/// Observation of a running process.
pub struct RuntimeObservation {
    pub pid: u32,
    pub running: bool,
    pub exit_code: Option<i32>,
}
```

### Functions

```rust
/// Prepare a Minecraft server instance (download Java + server.jar + config files).
/// Idempotent — skips download if server.jar already exists.
pub async fn prepare_minecraft(
    spec: &MinecraftSpec,
    instance_dir: &Path,
) -> Result<PreparedInstance, NodeApiError>;

/// Start a Minecraft server process. Returns the Child handle.
pub async fn start_minecraft(
    prepared: &PreparedInstance,
    spec: &MinecraftSpec,
) -> Result<tokio::process::Child, NodeApiError>;

/// Send "stop" command to a Minecraft server's stdin.
pub async fn send_stop_command(
    child: &mut tokio::process::Child,
) -> Result<(), NodeApiError>;

/// Stop a Minecraft server gracefully (stop command + grace period + SIGKILL).
pub async fn stop_minecraft(
    child: &mut tokio::process::Child,
    grace_period_ms: u64,
) -> Result<Option<i32>, NodeApiError>;

/// Inspect a running process.
pub fn inspect_minecraft(child: &mut tokio::process::Child) -> RuntimeObservation;

/// Delete an instance directory.
pub async fn delete_minecraft(
    instance_id: &str,
    instance_dir: &Path,
) -> Result<(), NodeApiError>;
```

## Usage from lbby-node

```rust
use lbby_core::node_api::{self, MinecraftSpec};

let spec = MinecraftSpec {
    minecraft_version: "1.21.4".to_string(),
    distribution: "paper".to_string(),
    ram_mb: 2048,
    max_players: 20,
    server_name: "My Server".to_string(),
    game_port: 25565,
    ..Default::default()
};

let instance_dir = PathBuf::from("/var/lib/lbby-node/instances/my-server");

// Prepare (download server files)
let prepared = node_api::prepare_minecraft(&spec, &instance_dir).await?;

// Start
let mut child = node_api::start_minecraft(&prepared, &spec).await?;

// Inspect
let obs = node_api::inspect_minecraft(&mut child);
assert!(obs.running);

// Stop
let exit_code = node_api::stop_minecraft(&mut child, 10_000).await?;

// Delete
node_api::delete_minecraft("my-server", &instance_dir).await?;
```

## Error Model

```rust
pub enum NodeApiError {
    Io(std::io::Error),
    JavaNotFound(u8, String),      // (required_major, reason)
    DownloadFailed(String),
    UnsupportedDistribution(String),
    MissingField(String),
    NotRunning,
    AlreadyRunning,
    ProcessError(String),
}
```

## Internal Design

- Uses a **no-op `AppEventSender`** for functions that need it (e.g., `ensure_java`, `download_to_file`). Progress events are discarded since lbby-node doesn't need them.
- Wraps existing lbby-core functions (`java::find_java_with_version`, `java::ensure_java`, `helpers::download_to_file`, `server::optimized_jvm_flags`) — does not rewrite them.
- The `download_server_jar` internal function handles Paper, Folia, and Vanilla distributions using their respective APIs.

## Supported Distributions

| Distribution | Status | Notes |
|---|---|---|
| `paper` | ✅ | PaperMC API v2 |
| `folia` | ✅ | PaperMC API v2 (folia project) |
| `vanilla` | ✅ | Mojang version manifest |
| `forge` | 🔜 | Requires installer flow |
| `fabric` | 🔜 | Requires launcher meta |
| `purpur` | 🔜 | Purpur API |
