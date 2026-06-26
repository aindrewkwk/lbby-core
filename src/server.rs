use serde::{Deserialize, Serialize};
use tokio::process::ChildStdin;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
}

pub struct ServerManager {
    pub status: ServerStatus,
    pub stdin: Option<ChildStdin>,
    pub pid: Option<u32>,
    /// True when the user explicitly clicked Stop (or Restart). False when the
    /// server exited on its own (crash). The wait task uses this to decide
    /// whether to trigger an auto-restart.
    pub stop_requested: bool,
}

impl ServerManager {
    pub fn new() -> Self {
        Self {
            status: ServerStatus::Stopped,
            stdin: None,
            pid: None,
            stop_requested: false,
        }
    }
}

/// Stub: start the Minecraft server — full implementation to be migrated from monolithic lib.rs.
pub async fn start_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    let _ = app;
    Err("start_server not yet implemented in lbby-core".to_string())
}

/// Stub: stop the Minecraft server — full implementation to be migrated from monolithic lib.rs.
pub async fn stop_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<(), String> {
    let _ = app;
    Err("stop_server not yet implemented in lbby-core".to_string())
}
