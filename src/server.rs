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
