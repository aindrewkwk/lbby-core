//! Dashboard heartbeat — keeps the app in sync with web.rstudio.live.
//!
//! Every N minutes (configurable, default 5) the app POSTs a small JSON
//! payload to `/api/app/heartbeat`. The server responds with:
//!
//!   - `license_token` — a fresh JWT if the admin changed the user's tier
//!   - `revoked: true` — if the license was revoked server-side
//!
//! When either arrives the local `license.jwt` is updated automatically
//! so the UI reflects the new tier without a restart.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config;
use crate::license;

/// How often we retry after a failed heartbeat (shorter than normal interval).
const RETRY_SECS: u64 = 60;

/// Response shape from `POST /api/app/heartbeat`.
#[derive(Debug, Deserialize, Default)]
struct HeartbeatResponse {
    ok: Option<bool>,
    #[serde(default)]
    license_token: Option<String>,
    #[serde(default)]
    revoked: Option<bool>,
    #[serde(default)]
    actions: Vec<serde_json::Value>,
}

/// Payload we send to the heartbeat endpoint.
#[derive(Debug, Serialize)]
struct HeartbeatPayload<'a> {
    hostname: &'a str,
    app_version: &'a str,
    os: &'a str,
}

/// Runs the heartbeat loop. Call from a tokio::spawn in main.
/// `on_tier_change` is invoked (best-effort) whenever the tier changes,
/// so the UI can refresh.
pub async fn run(on_tier_change: Option<Arc<Mutex<dyn FnMut() + Send>>>) {
    loop {
        let cfg = config::load_config();

        // Bail out if dashboard sync is not configured
        if cfg.app_token.is_empty() || cfg.heartbeat_interval_secs == 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            continue;
        }

        let interval = tokio::time::Duration::from_secs(cfg.heartbeat_interval_secs.max(30));

        match send_heartbeat(&cfg).await {
            Ok(resp) => {
                let mut tier_changed = false;

                // Server issued a new license token (tier upgrade / refresh)
                if let Some(token) = resp.license_token {
                    match license::activate(&token) {
                        Ok(_) => {
                            eprintln!("[heartbeat] License updated from server");
                            tier_changed = true;
                        }
                        Err(e) => {
                            eprintln!("[heartbeat] Failed to save server license: {}", e);
                        }
                    }
                }

                // Server says the license was revoked
                if resp.revoked == Some(true) {
                    if let Err(e) = license::deactivate() {
                        eprintln!("[heartbeat] Failed to deactivate revoked license: {}", e);
                    } else {
                        eprintln!("[heartbeat] License revoked by server");
                        tier_changed = true;
                    }
                }

                // Notify the UI if anything changed
                if tier_changed {
                    if let Some(ref cb) = on_tier_change {
                        if let Ok(mut f) = cb.try_lock() {
                            f();
                        }
                    }
                }

                tokio::time::sleep(interval).await;
            }
            Err(e) => {
                eprintln!("[heartbeat] Failed: {}", e);
                // Retry sooner after a failure
                tokio::time::sleep(tokio::time::Duration::from_secs(RETRY_SECS)).await;
            }
        }
    }
}

/// Send a single heartbeat to the dashboard.
async fn send_heartbeat(cfg: &config::ServerConfig) -> Result<HeartbeatResponse, String> {
    let url = format!("{}/api/app/heartbeat", cfg.dashboard_url.trim_end_matches('/'));

    let hostname = get_hostname();

    let payload = HeartbeatPayload {
        hostname: &hostname,
        app_version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.app_token))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Server returned {}: {}", status, body));
    }

    resp.json::<HeartbeatResponse>()
        .await
        .map_err(|e| format!("Invalid response: {}", e))
}

/// Get the machine hostname without external crates.
fn get_hostname() -> String {
    // Try environment variables first (works on all platforms)
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() { return h; }
    }
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        if !h.is_empty() { return h; }
    }
    // Fallback: run `hostname` command
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
