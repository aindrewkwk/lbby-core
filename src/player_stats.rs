use rusqlite::{Connection, Result as SqlResult, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;


#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlayerSummary {
    pub player_id: String,
    pub total_playtime_seconds: i64,
    pub last_seen: i64,
    pub deaths: u32,
    pub kills: u32,
    pub ip_address: Option<String>,
}

fn get_db_path(_app: &std::sync::Arc<crate::app_state::AppEventSender>) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("players.db")
}

fn init_db(conn: &Connection) -> SqlResult<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS player_stats (
            player_id TEXT PRIMARY KEY,
            total_playtime_seconds INTEGER DEFAULT 0,
            last_seen INTEGER DEFAULT 0,
            deaths INTEGER DEFAULT 0,
            kills INTEGER DEFAULT 0
        )",
        [],
    )?;

    // Add ip_address column if missing (migration for existing databases)
    let has_ip = conn
        .prepare("SELECT ip_address FROM player_stats LIMIT 0")
        .is_ok();
    if !has_ip {
        let _ = conn.execute("ALTER TABLE player_stats ADD COLUMN ip_address TEXT", []);
    }

    // Index on ip_address for fast ban lookups
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_player_stats_ip ON player_stats(ip_address)",
        [],
    )?;

    // Index on last_seen for efficient "recent players" queries
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_player_stats_last_seen ON player_stats(last_seen DESC)",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            player_id TEXT,
            start_time INTEGER,
            end_time INTEGER
        )",
        [],
    )?;
    Ok(())
}

/// Open a new SQLite connection to the player stats database.
///
/// NOTE: This opens a fresh connection on every call rather than caching a
/// persistent one. This is intentional — `rusqlite::Connection` is `!Send`,
/// so it cannot be stored in the async `AppState` (which requires `Send +
/// Sync`). For a local embedded database with WAL mode this is acceptable:
/// each open is cheap (~0.1 ms) and the write volume is low (a few rows per
/// player join/leave). If write throughput becomes a bottleneck, consider
/// wrapping the connection in a `tokio::sync::Mutex` behind a blocking
/// thread, or using `r2d2` / `deadpool` connection pooling.
fn get_conn(app: &std::sync::Arc<crate::app_state::AppEventSender>) -> Result<Connection, String> {
    let db_path = get_db_path(app);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(&db_path).map_err(|e| e.to_string())?;
    init_db(&conn).map_err(|e| e.to_string())?;
    Ok(conn)
}

pub async fn record_session_start(app: std::sync::Arc<crate::app_state::AppEventSender>, player_id: String, timestamp: i64) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let conn = get_conn(&app)?;
        conn.execute(
            "INSERT OR IGNORE INTO player_stats (player_id, last_seen) VALUES (?1, ?2)",
            params![player_id, timestamp],
        ).map_err(|e| e.to_string())?;

        conn.execute(
            "UPDATE player_stats SET last_seen = ?2 WHERE player_id = ?1",
            params![player_id, timestamp],
        ).map_err(|e| e.to_string())?;

        conn.execute(
            "INSERT INTO sessions (player_id, start_time, end_time) VALUES (?1, ?2, NULL)",
            params![player_id, timestamp],
        ).map_err(|e| e.to_string())?;

        app.emit("player-stats-updated", ()).ok();
        Ok(())
    }).await.map_err(|e| format!("Task failed: {}", e))?
}

pub async fn record_session_end(app: std::sync::Arc<crate::app_state::AppEventSender>, player_id: String, timestamp: i64, deaths: u32, kills: u32) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let conn = get_conn(&app)?;

        // Find the latest open session
        let mut stmt = conn.prepare("SELECT id, start_time FROM sessions WHERE player_id = ?1 AND end_time IS NULL ORDER BY start_time DESC LIMIT 1").map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![player_id]).map_err(|e| e.to_string())?;

        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let session_id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let start_time: i64 = row.get(1).map_err(|e| e.to_string())?;
            let duration = timestamp - start_time;
            let duration = if duration < 0 { 0 } else { duration };

            conn.execute(
                "UPDATE sessions SET end_time = ?2 WHERE id = ?1",
                params![session_id, timestamp],
            ).map_err(|e| e.to_string())?;

            conn.execute(
                "UPDATE player_stats SET total_playtime_seconds = total_playtime_seconds + ?2, last_seen = ?3, deaths = deaths + ?4, kills = kills + ?5 WHERE player_id = ?1",
                params![player_id, duration, timestamp, deaths, kills],
            ).map_err(|e| e.to_string())?;
        } else {
            // No open session, just update last_seen, deaths, kills
            conn.execute(
                "INSERT OR IGNORE INTO player_stats (player_id, last_seen, deaths, kills) VALUES (?1, ?2, ?3, ?4)",
                params![player_id, timestamp, deaths, kills],
            ).map_err(|e| e.to_string())?;

            conn.execute(
                "UPDATE player_stats SET last_seen = ?2, deaths = deaths + ?3, kills = kills + ?4 WHERE player_id = ?1",
                params![player_id, timestamp, deaths, kills],
            ).map_err(|e| e.to_string())?;
        }

        app.emit("player-stats-updated", ()).ok();
        Ok(())
    }).await.map_err(|e| format!("Task failed: {}", e))?
}

pub async fn list_all_summaries(app: std::sync::Arc<crate::app_state::AppEventSender>) -> Result<Vec<PlayerSummary>, String> {
    tokio::task::spawn_blocking(move || {
        let conn = get_conn(&app)?;
        let mut stmt = conn.prepare("SELECT player_id, total_playtime_seconds, last_seen, deaths, kills, ip_address FROM player_stats ORDER BY total_playtime_seconds DESC").map_err(|e| e.to_string())?;
        let iter = stmt.query_map([], |row| {
            Ok(PlayerSummary {
                player_id: row.get(0)?,
                total_playtime_seconds: row.get(1)?,
                last_seen: row.get(2)?,
                deaths: row.get(3)?,
                kills: row.get(4)?,
                ip_address: row.get(5)?,
            })
        }).map_err(|e| e.to_string())?;

        let mut summaries = Vec::new();
        for s in iter {
            summaries.push(s.map_err(|e| e.to_string())?);
        }

        Ok(summaries)
    }).await.map_err(|e| format!("Task failed: {}", e))?
}

pub async fn player_record_start(app: std::sync::Arc<crate::app_state::AppEventSender>, player_id: String, timestamp: i64) -> Result<(), String> {
    record_session_start(app, player_id, timestamp).await
}

pub async fn player_record_end(app: std::sync::Arc<crate::app_state::AppEventSender>, player_id: String, timestamp: i64, deaths: u32, kills: u32) -> Result<(), String> {
    record_session_end(app, player_id, timestamp, deaths, kills).await
}

/// Record a player's IP address (from "logged in" console line).
/// Updates the ip_address field for the player in player_stats.
pub async fn record_player_ip(app: std::sync::Arc<crate::app_state::AppEventSender>, player_id: String, ip: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let conn = get_conn(&app)?;
        // Upsert: ensure player row exists, then update IP
        conn.execute(
            "INSERT OR IGNORE INTO player_stats (player_id, ip_address) VALUES (?1, ?2)",
            params![player_id, ip],
        ).map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE player_stats SET ip_address = ?2 WHERE player_id = ?1",
            params![player_id, ip],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }).await.map_err(|e| format!("Task failed: {}", e))?
}

/// Get a player's last known IP address.
pub async fn get_player_ip(app: std::sync::Arc<crate::app_state::AppEventSender>, player_id: String) -> Result<Option<String>, String> {
    tokio::task::spawn_blocking(move || {
        let conn = get_conn(&app)?;
        let mut stmt = conn.prepare("SELECT ip_address FROM player_stats WHERE player_id = ?1").map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![player_id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let ip: Option<String> = row.get(0).map_err(|e| e.to_string())?;
            Ok(ip)
        } else {
            Ok(None)
        }
    }).await.map_err(|e| format!("Task failed: {}", e))?
}
