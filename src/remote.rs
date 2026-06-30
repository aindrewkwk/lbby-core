use crate::{backup, config};
use crate::server::ServerStatus;
use std::path::PathBuf;
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Constant-time byte-slice comparison to prevent timing side-channel attacks
/// on token checks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteControlState {
    pub enabled: bool,
    pub running: bool,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub lan_url: String,
    pub public_url: String,
    pub url: String,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

pub fn local_lan_ip() -> String {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80))?;
            socket.local_addr()
        })
        .ok()
        .and_then(|addr| match addr.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => Some(ip.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// A single IPv4 address a friend could use to reach the Minecraft server,
/// tagged with what kind of network it belongs to so the UI can label it
/// (e.g. a Hamachi address vs the physical LAN address).
#[derive(Debug, Clone, Serialize)]
pub struct LanAddress {
    pub ip: String,
    pub adapter: String,
    /// One of: "local", "hamachi", "radmin", "tailscale", "zerotier", "vpn", "other".
    pub kind: String,
}

/// Classify an IPv4 + adapter name into a connection "kind". VPN tools use
/// well-known address ranges; we also sniff the adapter name as a fallback,
/// since ranges can be reconfigured (ZeroTier especially).
fn classify(ip: Ipv4Addr, adapter: &str) -> &'static str {
    let o = ip.octets();
    let name = adapter.to_lowercase();
    // Range-based detection first (most reliable for Hamachi/Radmin/Tailscale).
    if o[0] == 25 { return "hamachi"; }
    if o[0] == 26 { return "radmin"; }
    if o[0] == 100 && (64..=127).contains(&o[1]) { return "tailscale"; } // 100.64.0.0/10 CGNAT
    // Name-based detection for anything else.
    if name.contains("hamachi") { return "hamachi"; }
    if name.contains("radmin") { return "radmin"; }
    if name.contains("tailscale") { return "tailscale"; }
    if name.contains("zerotier") { return "zerotier"; }
    if name.contains("vpn") || name.contains("wireguard") || name.contains("tap") || name.contains("tun") {
        return "vpn";
    }
    // RFC1918 private ranges → physical/local network.
    let is_private = o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168);
    if is_private { "local" } else { "other" }
}

/// Every usable IPv4 address on this machine, VPN adapters included, ordered
/// physical-LAN first then VPNs then everything else. Loopback and link-local
/// (169.254.x) are skipped.
pub fn list_lan_addresses() -> Vec<LanAddress> {
    let mut out: Vec<LanAddress> = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() { continue; }
            let ip = match iface.ip() {
                IpAddr::V4(v4) => v4,
                IpAddr::V6(_) => continue,
            };
            if ip.is_loopback() || ip.is_link_local() { continue; }
            out.push(LanAddress {
                ip: ip.to_string(),
                adapter: iface.name.clone(),
                kind: classify(ip, &iface.name).to_string(),
            });
        }
    }
    // De-dupe (some adapters report the same IP twice).
    out.dedup_by(|a, b| a.ip == b.ip);
    // Stable, friendly ordering: local LAN first, then VPNs, then other.
    fn rank(kind: &str) -> u8 {
        match kind {
            "local" => 0,
            "hamachi" | "radmin" | "tailscale" | "zerotier" | "vpn" => 1,
            _ => 2,
        }
    }
    out.sort_by_key(|a| rank(&a.kind));
    out
}

pub fn state_from_config(running: bool) -> RemoteControlState {
    let cfg = config::load_config();
    let host = local_lan_ip();
    let token = cfg.remote_control_token;
    let lan_url = if cfg.remote_control_enabled && !token.is_empty() {
        format!(
            "http://{}:{}/?token={}",
            host, cfg.remote_control_port, token
        )
    } else {
        String::new()
    };
    let public_url = build_public_url(&cfg.remote_control_public_url, &token);
    let url = if !public_url.is_empty() {
        public_url.clone()
    } else {
        lan_url.clone()
    };

    RemoteControlState {
        enabled: cfg.remote_control_enabled,
        running,
        host,
        port: cfg.remote_control_port,
        token,
        lan_url,
        public_url,
        url,
    }
}

fn build_public_url(raw: &str, token: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || token.is_empty() {
        return String::new();
    }
    let base = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{}", trimmed)
    };
    if base.contains("token=") {
        base
    } else {
        let separator = if base.contains('?') { '&' } else { '?' };
        format!("{}{}token={}", base, separator, token)
    }
}

pub async fn sync(app: &std::sync::Arc<crate::app_state::AppEventSender>) -> Result<RemoteControlState, String> {
    let cfg = config::load_config();
    let state = app.state();
    let mut task = state.remote_control.lock().await;
    let mut active_token = state.remote_control_active_token.lock().await;

    // If the server is already running, check whether the token has changed.
    // A mismatch means the user regenerated the token (or Cloudflare auto-start
    // is about to spin up a tunnel with a different one); we must restart so the
    // server validates against the new token.
    let token_changed = cfg.remote_control_token != *active_token;
    if cfg.remote_control_enabled
        && !token_changed
        && task.is_some()
        && tokio::net::TcpStream::connect(("127.0.0.1", cfg.remote_control_port))
            .await
            .is_ok()
    {
        return Ok(state_from_config(true));
    }

    if let Some(handle) = task.take() {
        handle.abort();
        *active_token = String::new();
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    }

    if !cfg.remote_control_enabled {
        return Ok(state_from_config(false));
    }

    if cfg.remote_control_token.trim().is_empty() {
        return Err(
            "Remote control token is empty. Generate a token before enabling remote control."
                .to_string(),
        );
    }

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], cfg.remote_control_port));
    let listener = TcpListener::bind(bind_addr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "LAN Remote port {} is already in use. Turn LAN Remote off, choose another port such as {}, then turn it on again.",
                cfg.remote_control_port,
                cfg.remote_control_port.saturating_add(1)
            )
        } else {
            format!(
                "Remote control failed to bind port {}: {}",
                cfg.remote_control_port, e
            )
        }
    })?;

    let app_handle = app.clone();
    let token = cfg.remote_control_token.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                continue;
            };
            let app_request = app_handle.clone();
            let token_request = token.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    handle_connection(stream, app_request, token_request),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("[lbby] remote connection error: {}", e),
                    Err(_) => eprintln!("[lbby] remote connection timed out"),
                }
            });
        }
    });

    *task = Some(handle);
    *active_token = cfg.remote_control_token.clone();
    Ok(state_from_config(true))
}

/// Explicitly stop the remote-control server and wait until the port is
/// actually released. Unlike `sync()` (which just aborts and returns), this
/// retries the port-check in a tight loop so the caller is guaranteed the
/// port is free by the time it returns.
pub async fn stop(app: &std::sync::Arc<crate::app_state::AppEventSender>) -> Result<RemoteControlState, String> {
    let state = app.state();
    let mut task = state.remote_control.lock().await;
    let mut active_token = state.remote_control_active_token.lock().await;
    let port = config::load_config().remote_control_port;

    if let Some(handle) = task.take() {
        handle.abort();
        *active_token = String::new();
        // Wait for the port to actually become free (up to 2 s).
        for _ in 0..20 {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                break;
            }
        }
    }
    Ok(state_from_config(false))
}

pub async fn status(app: &std::sync::Arc<crate::app_state::AppEventSender>) -> RemoteControlState {
    let state = app.state();
    let running = if state.remote_control.lock().await.is_some() {
        let cfg = config::load_config();
        tokio::net::TcpStream::connect(("127.0.0.1", cfg.remote_control_port))
            .await
            .is_ok()
    } else {
        false
    };
    state_from_config(running)
}

async fn handle_connection(
    mut stream: TcpStream,
    app: std::sync::Arc<crate::app_state::AppEventSender>,
    token: String,
) -> Result<(), String> {
    let request = read_request(&mut stream).await?;
    let response = route_request(request, app, &token).await;
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| e.to_string())
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut data = Vec::new();
    let mut buf = [0_u8; 4096];
    let mut header_end = None;

    while data.len() < MAX_REQUEST_BYTES {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_header_end(&data) {
            header_end = Some(pos);
            break;
        }
    }

    let header_end = header_end.ok_or_else(|| "Invalid HTTP request".to_string())?;
    let headers_raw = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers_raw.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "Missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while data.len() < body_start + content_length && data.len() < MAX_REQUEST_BYTES {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }

    let body_end = (body_start + content_length).min(data.len());
    let body = data[body_start..body_end].to_vec();
    let (path, query) = parse_target(&target);

    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(key), percent_decode(value));
    }
    (path.to_string(), query)
}

fn percent_decode(input: &str) -> String {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn route_request(request: HttpRequest, app: std::sync::Arc<crate::app_state::AppEventSender>, token: &str) -> String {
    if request.path == "/" && request.method == "GET" {
        return html_response(remote_page());
    }

    // Public status endpoint — no auth required
    if request.path == "/status" && request.method == "GET" {
        return public_status_response(app).await;
    }

    if !authorized(&request, token) {
        return json_response(
            401,
            &serde_json::json!({
                "error": "Unauthorized. Provide Authorization: Bearer <token> or ?token=<token>."
            }),
        );
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/status") => remote_status_response(app).await,
        ("GET", "/api/console") => remote_console_response(app).await,
        ("POST", "/api/command") => remote_command_response(app, request.body).await,
        ("POST", "/api/server/start") => remote_start_server(app).await,
        ("POST", "/api/server/stop") => remote_stop_server(app).await,
        ("POST", "/api/server/restart") => remote_restart_server(app).await,
        ("GET", "/api/pregen") => remote_pregen_status(app).await,
        ("POST", "/api/pregen/start") => remote_pregen_start(app, request.body).await,
        ("POST", "/api/pregen/cancel") => remote_pregen_cancel(app).await,
        ("POST", "/api/backup") => remote_backup_create(app, request.body).await,
        _ => json_response(404, &serde_json::json!({ "error": "Not found" })),
    }
}

fn authorized(request: &HttpRequest, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let token_bytes = token.as_bytes();
    if request
        .query
        .get("token")
        .is_some_and(|v| constant_time_eq(v.as_bytes(), token_bytes))
    {
        return true;
    }
    request
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| constant_time_eq(v.trim().as_bytes(), token_bytes))
}

async fn remote_status_response(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    let state = app.state();
    let cfg = config::load_config();
    let status = state.server.lock().await.status.clone();
    let stats = state.stats.lock().await.clone();
    let players = state
        .online_players
        .lock()
        .await
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let playit = state.playit.lock().await.clone();
    json_response(
        200,
        &serde_json::json!({
            "app": "Lbby",
            "server_name": cfg.server_name,
            "minecraft_version": cfg.minecraft_version,
            "loader_version": cfg.loader_version,
            "server_type": cfg.server_type,
            "status": status,
            "stats": stats,
            "players": players,
            "playit": playit,
        }),
    )
}

/// Public status endpoint — no auth required. Returns basic server info
/// for sharing (e.g. a status page for friends to check if the server is up).
async fn public_status_response(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    let state = app.state();
    let cfg = config::load_config();
    let status = state.server.lock().await.status.clone();
    let player_count = state.online_players.lock().await.len();
    let is_online = matches!(status, ServerStatus::Running);

    json_response(
        200,
        &serde_json::json!({
            "app": "Lbby",
            "server_name": cfg.server_name,
            "version": cfg.minecraft_version,
            "loader": cfg.server_type,
            "online": is_online,
            "status": status,
            "players_online": player_count,
            "max_players": cfg.max_players,
            "motd": cfg.server_name,
        }),
    )
}

async fn remote_console_response(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    let state = app.state();
    let lines = state
        .console_buffer
        .lock()
        .await
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    json_response(200, &serde_json::json!({ "lines": lines }))
}

async fn remote_command_response(app: std::sync::Arc<crate::app_state::AppEventSender>, body: Vec<u8>) -> String {
    let cmd = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_string))
        .or_else(|| String::from_utf8(body).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if cmd.is_empty() {
        return json_response(400, &serde_json::json!({ "error": "Missing command" }));
    }

    let state = app.state();
    let mut srv = state.server.lock().await;
    let Some(stdin) = srv.stdin.as_mut() else {
        return json_response(
            409,
            &serde_json::json!({ "error": "Server is not running" }),
        );
    };

    match stdin.write_all(format!("{}\n", cmd).as_bytes()).await {
        Ok(_) => json_response(200, &serde_json::json!({ "ok": true })),
        Err(e) => json_response(500, &serde_json::json!({ "error": e.to_string() })),
    }
}

async fn remote_start_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    match crate::helpers::do_start_server(app).await {
        Ok(_) => json_response(200, &serde_json::json!({ "ok": true })),
        Err(e) => json_response(409, &serde_json::json!({ "error": e })),
    }
}

async fn remote_stop_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    // Snapshot status first, so we can return 409 if the server isn't running.
    let was_running = {
        let state = app.state();
        let srv = state.server.lock().await;
        matches!(srv.status, ServerStatus::Running | ServerStatus::Starting | ServerStatus::Stopping)
    };
    if !was_running {
        return json_response(409, &serde_json::json!({ "error": "Server is not running" }));
    }

    // "Kill everything but the Remote-Control tunnel": stop the Minecraft
    // server (graceful → force-kill descendants if it hangs) AND the
    // playit.gg tunnel. The LAN remote service we're answering this request
    // on stays up; Cloudflare quick-tunnel (also part of the remote-control
    // path) is intentionally left running.
    crate::remote_kill_server_and_playit(&app).await;
    json_response(200, &serde_json::json!({ "ok": true }))
}

async fn remote_restart_server(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    let stop_result = remote_stop_server(app.clone()).await;
    if !stop_result.starts_with("HTTP/1.1 200") {
        return stop_result;
    }
    // remote_stop_server now blocks until the server is fully Stopped
    // (with force-kill fallback), so we only need a brief settle pause
    // before starting again. Restarts also shouldn't re-spawn playit —
    // start_server logic handles tunnel state on its own.
    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    remote_start_server(app).await
}

// ── Pre-generation ──────────────────────────────────────────────────────────

async fn remote_pregen_status(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    let state = app.state();
    let pg = state.pregen.lock().await.clone();
    json_response(
        200,
        &serde_json::json!({
            "running": pg.running,
            "total": pg.total,
            "completed": pg.completed,
            "cancel_requested": pg.cancel_requested,
        }),
    )
}

async fn remote_pregen_start(app: std::sync::Arc<crate::app_state::AppEventSender>, body: Vec<u8>) -> String {
    let total = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("total").and_then(|t| t.as_u64()))
        .unwrap_or(0) as u32;
    if total == 0 {
        return json_response(400, &serde_json::json!({ "error": "total must be > 0" }));
    }
    // Reuse the same Tauri command logic via direct call.
    match crate::helpers::do_pregenerate_chunks(app, total).await {
        Ok(_) => json_response(200, &serde_json::json!({ "ok": true })),
        Err(e) => json_response(409, &serde_json::json!({ "error": e })),
    }
}

async fn remote_pregen_cancel(app: std::sync::Arc<crate::app_state::AppEventSender>) -> String {
    let state = app.state();
    let mut pg = state.pregen.lock().await;
    if pg.running {
        pg.cancel_requested = true;
    }
    json_response(200, &serde_json::json!({ "ok": true }))
}

// ── Backup ──────────────────────────────────────────────────────────────────

async fn remote_backup_create(app: std::sync::Arc<crate::app_state::AppEventSender>, body: Vec<u8>) -> String {
    // Optional body: { "include_logs": bool }. Default false (matches UI default).
    let include_logs = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("include_logs").and_then(|b| b.as_bool()))
        .unwrap_or(false);

    let cfg = config::load_config();
    let server_path = PathBuf::from(&cfg.server_path);
    let dest_dir = if cfg.backup_dir.is_empty() {
        dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(&cfg.backup_dir)
    };
    let filename = backup::default_backup_filename();
    let dest_path = dest_dir.join(&filename);
    let app_clone = app.clone();
    let dest_for_task = dest_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        backup::create_server_backup(&app_clone, &server_path, &dest_for_task, include_logs)
    })
    .await;
    match result {
        Ok(Ok((files, bytes))) => json_response(
            200,
            &serde_json::json!({
                "ok": true,
                "path": dest_path.to_string_lossy(),
                "files": files,
                "bytes": bytes,
            }),
        ),
        Ok(Err(e)) => json_response(500, &serde_json::json!({ "error": e })),
        Err(e) => json_response(500, &serde_json::json!({ "error": format!("Backup task panicked: {}", e) })),
    }
}

fn remote_page() -> &'static str {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <meta name="referrer" content="no-referrer" />
  <title>Lbby — Remote</title>
  <style>
    :root {
      --bg: #131216;
      --app-bg: #1c1a21;
      --app-bg-2: #131216;
      --surface: #221f29;
      --surface-2: rgba(255, 255, 255, 0.04);
      --border: rgba(255, 255, 255, 0.07);
      --border-strong: rgba(255, 255, 255, 0.14);
      --text: #f3f1f6;
      --text-muted: rgba(243, 241, 246, 0.62);
      --text-faint: rgba(243, 241, 246, 0.40);
      --accent: #9147ff;
      --accent-fg: #ffffff;
      --accent-2: #c2a4ff;
      --accent-glow: rgba(145, 71, 255, 0.38);
      --green: #43d17f;
      --yellow: #f5b73d;
      --red: #f2555a;
      --blue: #5b9dff;
      --radius: 12px;
      --radius-sm: 8px;
      --spring: cubic-bezier(0.34, 1.56, 0.64, 1);
      --spring-soft: cubic-bezier(0.22, 1.10, 0.42, 1);
    }
    * { box-sizing: border-box; }
    html, body { margin: 0; padding: 0; min-height: 100%; }
    body {
      font-family: "Segoe UI", -apple-system, BlinkMacSystemFont, system-ui, sans-serif;
      font-size: 14px;
      color: var(--text);
      background:
        radial-gradient(circle at 88% 4%, rgba(145, 71, 255, 0.14), transparent 40%),
        var(--app-bg-2);
      min-height: 100vh;
      -webkit-font-smoothing: antialiased;
    }
    main {
      max-width: 1080px;
      margin: 0 auto;
      padding: 24px 20px 48px;
      display: flex;
      flex-direction: column;
      gap: 18px;
    }

    /* ── Top bar ───────────────────────────────────────────────────────── */
    .topbar {
      display: flex;
      align-items: center;
      gap: 14px;
      padding: 12px 16px;
      border-radius: var(--radius);
      background: var(--surface);
      border: 1px solid var(--border);
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.30);
    }
    .logo {
      width: 38px; height: 38px;
      border-radius: 10px;
      background: var(--accent);
      color: #fff;
      display: flex; align-items: center; justify-content: center;
      font-weight: 900;
      font-size: 14px;
      letter-spacing: 0.5px;
      box-shadow: 0 4px 14px var(--accent-glow);
    }
    .brand-text {
      display: flex; flex-direction: column; line-height: 1.1;
    }
    .brand-name { font-weight: 800; font-size: 16px; }
    .brand-sub  { font-size: 11px; color: var(--text-faint); margin-top: 2px; letter-spacing: 0.4px; text-transform: uppercase; }
    .topbar .spacer { flex: 1; }
    .conn-pill {
      display: inline-flex; align-items: center; gap: 6px;
      padding: 5px 12px;
      font-size: 11px; font-weight: 700;
      letter-spacing: 0.5px; text-transform: uppercase;
      border-radius: 999px;
      background: rgba(255, 255, 255, 0.06);
      border: 1px solid var(--border);
      color: var(--text-muted);
    }
    .conn-pill .dot {
      width: 7px; height: 7px; border-radius: 50%;
      background: var(--text-faint);
    }
    .conn-pill.live .dot {
      background: var(--green);
      box-shadow: 0 0 8px rgba(74, 222, 128, 0.6);
      animation: pulse 1.8s ease-out infinite;
    }
    .conn-pill.live { color: var(--green); border-color: rgba(74, 222, 128, 0.32); }
    @keyframes pulse {
      0%,100% { box-shadow: 0 0 0 0 rgba(74, 222, 128, 0.55); }
      50%     { box-shadow: 0 0 0 6px rgba(74, 222, 128, 0); }
    }

    /* ── Cards ─────────────────────────────────────────────────────────── */
    .card {
      padding: 18px;
      border-radius: var(--radius);
      background: var(--surface);
      border: 1px solid var(--border);
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.30);
    }

    /* ── Hero: server name + stats + actions ───────────────────────────── */
    .hero {
      display: grid;
      grid-template-columns: 1fr auto;
      gap: 24px;
      align-items: stretch;
    }
    @media (max-width: 720px) { .hero { grid-template-columns: 1fr; } }
    .hero-name {
      font-size: clamp(28px, 6vw, 44px);
      line-height: 1;
      font-weight: 900;
      letter-spacing: -0.01em;
    }
    .hero-meta {
      color: var(--text-muted);
      font-size: 13px;
      margin-top: 10px;
      line-height: 1.5;
    }
    .hero-meta code {
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 12px;
      padding: 2px 6px;
      background: rgba(255, 255, 255, 0.06);
      border-radius: 6px;
      border: 1px solid var(--border);
    }
    .stat-grid {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 10px;
      margin-top: 16px;
    }
    .stat {
      padding: 10px 12px;
      background: var(--surface-2);
      border: 1px solid var(--border);
      border-radius: var(--radius-sm);
    }
    .stat-label {
      font-size: 10px; font-weight: 700; letter-spacing: 0.5px;
      color: var(--text-faint); text-transform: uppercase;
    }
    .stat-value {
      font-size: 18px; font-weight: 800; margin-top: 2px;
      font-variant-numeric: tabular-nums;
    }
    .actions {
      display: flex;
      flex-direction: column;
      gap: 8px;
      align-self: stretch;
      min-width: 200px;
    }
    @media (max-width: 720px) { .actions { flex-direction: row; flex-wrap: wrap; } }

    /* ── Buttons ───────────────────────────────────────────────────────── */
    button {
      font-family: inherit;
      font-size: 13px;
      font-weight: 600;
      cursor: pointer;
      border-radius: 999px;
      padding: 10px 18px;
      display: inline-flex;
      align-items: center;
      gap: 8px;
      transition: transform 0.18s var(--spring-soft),
                  background 0.2s ease,
                  border-color 0.2s ease,
                  color 0.2s ease,
                  opacity 0.2s ease;
    }
    button:active:not(:disabled) { transform: scale(0.96); }
    button:disabled { opacity: 0.45; cursor: not-allowed; }
    button svg { display: block; }

    .btn {
      background: rgba(255, 255, 255, 0.06);
      color: var(--text);
      border: 1px solid var(--border);
    }
    .btn:hover:not(:disabled) {
      background: rgba(255, 255, 255, 0.10);
      border-color: var(--border-strong);
    }
    .btn-primary {
      background: var(--accent);
      color: var(--accent-fg);
      border: 1px solid transparent;
    }
    .btn-primary:hover:not(:disabled) {
      background: #a366ff;
      box-shadow: 0 4px 16px var(--accent-glow);
    }
    .btn-danger {
      background: rgba(251, 113, 133, 0.10);
      color: var(--red);
      border: 1px solid rgba(251, 113, 133, 0.45);
    }
    .btn-danger:hover:not(:disabled) {
      background: var(--red);
      color: #fff;
      border-color: var(--red);
    }
    .btn-justify { justify-content: center; }

    /* ── Console ───────────────────────────────────────────────────────── */
    .card-head {
      display: flex; align-items: center; justify-content: space-between;
      margin-bottom: 12px;
    }
    .card-title {
      font-size: 11px; font-weight: 700; letter-spacing: 0.6px;
      color: var(--text-faint); text-transform: uppercase;
      display: flex; align-items: center; gap: 8px;
    }
    .console {
      background: rgba(0, 0, 0, 0.45);
      border: 1px solid var(--border);
      border-radius: var(--radius-sm);
      padding: 12px 14px;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 12px;
      line-height: 1.65;
      max-height: 50vh;
      min-height: 280px;
      overflow: auto;
      scrollbar-gutter: stable;
      white-space: pre-wrap;
      color: var(--text);
    }
    .console .err  { color: var(--red); }
    .console .warn { color: var(--yellow); }
    .console .ok   { color: var(--green); }
    .console .blue { color: var(--blue); }
    .console .muted{ color: var(--text-faint); }
    .console-empty { color: var(--text-faint); padding: 6px 2px; }

    /* ── Command input row ─────────────────────────────────────────────── */
    .cmd-row {
      display: flex;
      align-items: stretch;
      gap: 8px;
      margin-top: 12px;
    }
    .input-shell {
      flex: 1;
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 0 12px;
      background: rgba(0, 0, 0, 0.35);
      border: 1px solid var(--border);
      border-radius: 999px;
      transition: border-color 0.2s ease, background 0.2s ease;
    }
    .input-shell:focus-within {
      border-color: var(--border-strong);
      background: rgba(0, 0, 0, 0.55);
    }
    .input-shell .prompt {
      color: var(--text-muted);
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-weight: 700;
    }
    .input-shell input {
      flex: 1;
      border: 0;
      background: transparent;
      color: var(--text);
      padding: 11px 0;
      font-size: 13px;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      outline: none;
    }
    .input-shell input::placeholder { color: var(--text-faint); }

    /* ── Toast / action feedback ──────────────────────────────────────── */
    .toast {
      position: fixed;
      bottom: 24px; right: 24px;
      max-width: 360px;
      padding: 12px 14px;
      border-radius: var(--radius-sm);
      background: var(--surface);
      backdrop-filter: blur(40px) saturate(180%);
      border: 1px solid var(--border);
      color: var(--text);
      font-size: 12px;
      box-shadow: 0 12px 36px rgba(0,0,0,0.5);
      transform: translate(8px, 12px);
      opacity: 0;
      pointer-events: none;
      transition: transform 0.32s var(--spring), opacity 0.28s ease;
    }
    .toast.show {
      transform: translate(0, 0);
      opacity: 1;
    }
    .toast.err  { border-color: rgba(251,113,133,0.45); color: var(--red); }
    .toast.ok   { border-color: rgba(74,222,128,0.45); color: var(--green); }

    /* ── Animations ────────────────────────────────────────────────────── */
    @keyframes springInUp {
      0%   { opacity: 0; transform: translateY(14px) scale(0.97); }
      60%  { opacity: 1; transform: translateY(-2px) scale(1.01); }
      100% { opacity: 1; transform: translateY(0) scale(1); }
    }
    .topbar, .card { animation: springInUp 0.5s var(--spring) both; }
    .card:nth-of-type(1) { animation-delay: 60ms; }
    .card:nth-of-type(2) { animation-delay: 120ms; }
    .card:nth-of-type(3) { animation-delay: 180ms; }
    .card:nth-of-type(4) { animation-delay: 240ms; }

    /* ── Tools grid (pregen + backup) ─────────────────────────────────── */
    .tools-grid {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 18px;
    }
    @media (max-width: 720px) { .tools-grid { grid-template-columns: 1fr; } }
    .tool-row {
      display: flex;
      align-items: center;
      gap: 10px;
      flex-wrap: wrap;
    }
    .tool-row label {
      font-size: 11px;
      color: var(--text-faint);
      letter-spacing: 0.4px;
      text-transform: uppercase;
      font-weight: 700;
    }
    .tool-row select, .tool-row input[type="number"] {
      font-family: inherit;
      font-size: 13px;
      color: var(--text);
      background: rgba(0, 0, 0, 0.35);
      border: 1px solid var(--border);
      border-radius: 999px;
      padding: 8px 12px;
      outline: none;
      transition: border-color 0.2s ease, background 0.2s ease;
    }
    .tool-row select:focus, .tool-row input[type="number"]:focus {
      border-color: var(--border-strong);
      background: rgba(0, 0, 0, 0.55);
    }
    .tool-row input[type="number"] {
      width: 110px;
      font-variant-numeric: tabular-nums;
    }
    .tool-checkbox {
      display: inline-flex; align-items: center; gap: 6px;
      font-size: 12px; color: var(--text-muted);
    }
    .tool-checkbox input { accent-color: #fff; }

    /* progress bar */
    .progress {
      position: relative;
      height: 8px;
      width: 100%;
      background: rgba(255, 255, 255, 0.06);
      border: 1px solid var(--border);
      border-radius: 999px;
      overflow: hidden;
      margin-top: 10px;
    }
    .progress-fill {
      position: absolute; inset: 0 auto 0 0;
      width: 0%;
      background: linear-gradient(90deg, var(--accent), var(--accent-2));
      box-shadow: 0 0 12px var(--accent-glow);
      transition: width 0.4s var(--spring-soft);
    }
    .progress-meta {
      display: flex; justify-content: space-between;
      font-size: 11px;
      color: var(--text-muted);
      margin-top: 6px;
      font-variant-numeric: tabular-nums;
    }
  </style>
</head>
<body>
  <main>
    <header class="topbar">
      <div class="logo">L</div>
      <div class="brand-text">
        <span class="brand-name">Lbby Remote</span>
        <span class="brand-sub" id="brand-sub">Connecting…</span>
      </div>
      <span class="spacer"></span>
      <span class="conn-pill" id="conn-pill"><span class="dot"></span><span id="conn-text">Connecting</span></span>
    </header>

    <section class="card">
      <div class="hero">
        <div>
          <div class="card-title">Server</div>
          <div class="hero-name" id="server-name">Loading…</div>
          <div class="hero-meta" id="server-meta">—</div>
          <div class="stat-grid">
            <div class="stat">
              <div class="stat-label">TPS</div>
              <div class="stat-value" id="stat-tps">—</div>
            </div>
            <div class="stat">
              <div class="stat-label">Players</div>
              <div class="stat-value" id="stat-players">—</div>
            </div>
            <div class="stat">
              <div class="stat-label">Public address</div>
              <div class="stat-value" id="stat-public" style="font-size: 13px; word-break: break-all;">—</div>
            </div>
          </div>
        </div>
        <div class="actions">
          <button class="btn-primary btn-justify" id="btn-start" onclick="serverAction('start', event)">
            <svg width="14" height="14" viewBox="0 0 24 24" aria-hidden="true"><path fill="currentColor" d="M7 5l13 7-13 7z"/></svg>
            Start
          </button>
          <button class="btn btn-justify" id="btn-restart" onclick="serverAction('restart', event)">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v6h6"/></svg>
            Restart
          </button>
          <button class="btn-danger btn-justify" id="btn-stop" onclick="serverAction('stop', event)">
            <svg width="14" height="14" viewBox="0 0 24 24" aria-hidden="true"><rect x="6" y="6" width="12" height="12" rx="1.5" fill="currentColor"/></svg>
            Stop
          </button>
        </div>
      </div>
    </section>

    <section class="card">
      <div class="card-head">
        <div class="card-title">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="5" width="18" height="14" rx="2.4"/><path d="M6.5 9.3l3.2 2.7-3.2 2.7"/><path d="M12.6 15h4.6"/></svg>
          Console
        </div>
        <button class="btn" onclick="refresh()">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v6h6"/></svg>
          Refresh
        </button>
      </div>
      <div class="console" id="console" role="log" aria-live="polite"></div>
      <div class="cmd-row">
        <div class="input-shell">
          <span class="prompt">/</span>
          <input id="cmd" placeholder="Minecraft command, e.g. say hello" autocomplete="off" />
        </div>
        <button class="btn-primary" onclick="sendCommand()">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 12l18-9-7 18-3-8z"/></svg>
          Send
        </button>
      </div>
    </section>

    <section class="tools-grid">
      <div class="card">
        <div class="card-head">
          <div class="card-title">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.2"/><rect x="14" y="3" width="7" height="7" rx="1.2"/><rect x="3" y="14" width="7" height="7" rx="1.2"/><rect x="14" y="14" width="7" height="7" rx="1.2"/></svg>
            Pre-generate chunks
          </div>
        </div>
        <div class="tool-row">
          <label for="pregen-preset">Size</label>
          <select id="pregen-preset" onchange="onPregenPresetChange()">
            <option value="441">Small · 441 (~3 MB)</option>
            <option value="1024" selected>Medium · 1,024 (~7 MB)</option>
            <option value="4096">Large · 4,096 (~28 MB)</option>
            <option value="10000">Huge · 10,000 (~70 MB)</option>
            <option value="custom">Custom…</option>
          </select>
          <input type="number" id="pregen-custom" min="100" max="100000" step="100" placeholder="Chunks" style="display:none" />
        </div>
        <div class="progress" aria-hidden="true"><div class="progress-fill" id="pregen-fill"></div></div>
        <div class="progress-meta">
          <span id="pregen-status">Idle</span>
          <span id="pregen-count">0 / 0</span>
        </div>
        <div class="tool-row" style="margin-top: 12px; justify-content: flex-end;">
          <button class="btn" id="btn-pregen-cancel" onclick="cancelPregen()" style="display:none">Cancel</button>
          <button class="btn-primary" id="btn-pregen-start" onclick="startPregen()">
            <svg width="14" height="14" viewBox="0 0 24 24" aria-hidden="true"><path fill="currentColor" d="M7 5l13 7-13 7z"/></svg>
            Start
          </button>
        </div>
      </div>

      <div class="card">
        <div class="card-head">
          <div class="card-title">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M7 10l5 5 5-5"/><path d="M12 15V3"/></svg>
            Server backup
          </div>
        </div>
        <div class="hero-meta" style="margin-top: 0;">
          Saves a <code>.zip</code> snapshot of the world and config to the host's backup folder.
        </div>
        <label class="tool-checkbox" style="margin-top: 10px;">
          <input type="checkbox" id="backup-logs" />
          Include log files
        </label>
        <div class="progress-meta" style="margin-top: 10px;">
          <span id="backup-status">Ready</span>
          <span id="backup-info"></span>
        </div>
        <div class="tool-row" style="margin-top: 12px; justify-content: flex-end;">
          <button class="btn-primary" id="btn-backup" onclick="createBackup()">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z"/><path d="M17 21v-8H7v8"/><path d="M7 3v5h8"/></svg>
            Create backup
          </button>
        </div>
      </div>
    </section>
  </main>

  <div class="toast" id="toast"></div>

  <script>
    const token = new URLSearchParams(location.search).get("token") || "";
    // Strip the token from the URL bar so it is not leaked via Referer
    // headers or shared links. We store it in memory and send it via
    // the Authorization header on every request instead.
    if (token) {
      history.replaceState(null, "", location.pathname);
    }
    const $ = (id) => document.getElementById(id);

    async function api(path, opts = {}) {
      const headers = Object.assign({}, opts.headers || {}, {
        "Authorization": "Bearer " + token,
      });
      const res = await fetch(path, Object.assign({}, opts, { headers }));
      if (!res.ok) {
        const text = await res.text();
        throw new Error(text || ("HTTP " + res.status));
      }
      return res.json();
    }

    function setConnState(state, label) {
      const pill = $("conn-pill");
      const sub  = $("brand-sub");
      const text = $("conn-text");
      pill.classList.remove("live");
      if (state === "live") pill.classList.add("live");
      text.textContent = label;
      sub.textContent = label;
    }

    function statusLabel(s) {
      switch (s) {
        case "running":  return "Online";
        case "starting": return "Starting…";
        case "stopping": return "Stopping…";
        case "stopped":  return "Offline";
        default:         return s || "Unknown";
      }
    }

    function colorize(line) {
      const lower = line.toLowerCase();
      let cls = "";
      if (line.includes("[ERR]") || lower.includes("error") || lower.includes("exception")) cls = "err";
      else if (lower.includes("warn")) cls = "warn";
      else if (line.includes("Done") || line.includes("started")) cls = "ok";
      else if (line.startsWith("[lbby]") || line.startsWith("[mchost]")) cls = "blue";
      const escaped = line.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
      return cls ? `<span class="${cls}">${escaped}</span>` : escaped;
    }

    function showToast(message, kind) {
      const el = $("toast");
      el.className = "toast " + (kind || "");
      el.textContent = message;
      requestAnimationFrame(() => el.classList.add("show"));
      clearTimeout(showToast._t);
      showToast._t = setTimeout(() => el.classList.remove("show"), 2400);
    }

    let lastConsoleHeight = 0;
    async function refresh() {
      try {
        const status = await api("/api/status");
        const players = (status.players || []).length;
        const max = status.stats?.players_max ?? 0;
        const tps = status.stats?.tps ?? 0;
        const stateLabel = statusLabel(status.status);

        $("server-name").textContent = status.server_name || "Lbby Server";
        const ver = [status.minecraft_version, status.loader_version].filter(Boolean).join(" · ");
        const uptimeSec = status.stats?.uptime_seconds ?? 0;
        $("server-meta").innerHTML =
          `<code>${ver || "—"}</code> · ${stateLabel}` +
          (uptimeSec
            ? ` · uptime ${Math.floor(uptimeSec / 60)} min`
            : "");
        $("stat-tps").textContent = tps ? Number(tps).toFixed(1) : "—";
        $("stat-players").textContent = `${players} / ${max || "—"}`;
        $("stat-public").textContent = status.playit?.address || "—";

        // Action button availability
        const running = status.status === "running" || status.status === "starting";
        $("btn-start").disabled = running;
        $("btn-restart").disabled = !running;
        $("btn-stop").disabled = !running;

        const live = status.status === "running";
        setConnState(live ? "live" : "idle", live ? "Online" : stateLabel);

        const consoleData = await api("/api/console");
        const lines = (consoleData.lines || []).slice(-300);
        const el = $("console");
        if (lines.length === 0) {
          el.innerHTML = '<div class="console-empty">Server console output will appear here…</div>';
        } else {
          el.innerHTML = lines.map(colorize).join("\n");
        }
        // Autoscroll to bottom only when already pinned to the bottom
        const pinned = lastConsoleHeight === 0
          || (el.scrollTop + el.clientHeight + 20 >= lastConsoleHeight);
        if (pinned) el.scrollTop = el.scrollHeight;
        lastConsoleHeight = el.scrollHeight;
      } catch (e) {
        setConnState("idle", "Disconnected");
        $("server-meta").textContent = String(e);
      }
    }

    async function sendCommand() {
      const input = $("cmd");
      const cmd = input.value.trim();
      if (!cmd) return;
      input.disabled = true;
      try {
        await api("/api/command", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ cmd }),
        });
        input.value = "";
        showToast("Command sent", "ok");
        refresh();
      } catch (e) {
        showToast(String(e), "err");
      } finally {
        input.disabled = false;
        input.focus();
      }
    }

    async function serverAction(action, ev) {
      const btn = ev?.currentTarget;
      if (btn) btn.disabled = true;
      showToast(`${action}…`);
      try {
        await api("/api/server/" + action, { method: "POST" });
        showToast(`${action} OK`, "ok");
        setTimeout(refresh, 700);
      } catch (e) {
        showToast(String(e), "err");
      } finally {
        if (btn) btn.disabled = false;
      }
    }

    $("cmd").addEventListener("keydown", (e) => {
      if (e.key === "Enter") sendCommand();
    });

    // ── Pre-generation ───────────────────────────────────────────────
    function onPregenPresetChange() {
      const v = $("pregen-preset").value;
      $("pregen-custom").style.display = v === "custom" ? "" : "none";
    }

    function fmtBytes(n) {
      if (!n) return "";
      const u = ["B","KB","MB","GB"];
      let i = 0; let x = n;
      while (x >= 1024 && i < u.length - 1) { x /= 1024; i++; }
      return `${x.toFixed(x < 10 ? 1 : 0)} ${u[i]}`;
    }

    async function startPregen() {
      const preset = $("pregen-preset").value;
      let total = preset === "custom"
        ? parseInt($("pregen-custom").value || "0", 10)
        : parseInt(preset, 10);
      if (!total || total < 1) {
        showToast("Pick a size first", "err");
        return;
      }
      try {
        await api("/api/pregen/start", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ total }),
        });
        showToast("Pre-generation started", "ok");
        refreshPregen();
      } catch (e) {
        showToast(String(e), "err");
      }
    }

    async function cancelPregen() {
      try {
        await api("/api/pregen/cancel", { method: "POST" });
        showToast("Cancel requested");
      } catch (e) {
        showToast(String(e), "err");
      }
    }

    async function refreshPregen() {
      try {
        const pg = await api("/api/pregen");
        const pct = pg.total > 0 ? Math.min(100, (pg.completed / pg.total) * 100) : 0;
        $("pregen-fill").style.width = pct.toFixed(1) + "%";
        $("pregen-count").textContent = `${pg.completed.toLocaleString()} / ${pg.total.toLocaleString()}`;
        let label = "Idle";
        if (pg.running && pg.cancel_requested) label = "Cancelling…";
        else if (pg.running) label = `Generating · ${pct.toFixed(1)}%`;
        else if (pg.total > 0 && pg.completed >= pg.total) label = "Complete";
        $("pregen-status").textContent = label;
        $("btn-pregen-start").style.display = pg.running ? "none" : "";
        $("btn-pregen-cancel").style.display = pg.running ? "" : "none";
      } catch (e) { /* offline — ignore */ }
    }

    // ── Backup ───────────────────────────────────────────────────────
    async function createBackup() {
      const btn = $("btn-backup");
      const include_logs = $("backup-logs").checked;
      btn.disabled = true;
      $("backup-status").textContent = "Working…";
      $("backup-info").textContent = "";
      try {
        const res = await api("/api/backup", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ include_logs }),
        });
        $("backup-status").textContent = "Saved";
        $("backup-info").textContent = `${res.files.toLocaleString()} files · ${fmtBytes(res.bytes)}`;
        showToast("Backup saved on host", "ok");
      } catch (e) {
        $("backup-status").textContent = "Failed";
        showToast(String(e), "err");
      } finally {
        btn.disabled = false;
      }
    }

    refresh();
    refreshPregen();
    setInterval(refresh, 4000);
    setInterval(refreshPregen, 1500);
  </script>
</body>
</html>"#
}

fn html_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn json_response(status: u16, value: &serde_json::Value) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    let body = serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_string());
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    )
}
