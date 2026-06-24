//! License verification.
//!
//! Lbby's tier gate. License Server issues Ed25519-signed JWTs; we verify
//! against a public key baked into the binary at compile time. Private key
//! never ships with the app.
//!
//! On-disk layout (per OS):
//!   - macOS:   ~/Library/Application Support/lbby/license.jwt
//!   - Linux:   ~/.local/share/lbby/license.jwt
//!   - Windows: %APPDATA%/lbby/license.jwt
//!
//! Offline grace: a JWT that expired within 25 % of its original lifetime is
//! still honored at its previous tier. A 30-day key gets ~7.5 days of grace;
//! a 1-year key gets ~91 days. This keeps paying users from getting locked
//! out by a short internet outage while still revoking eventually.
//!
//! Device binding: if the JWT carries a `dev` claim, we verify it matches the
//! current machine's fingerprint (a SHA-256 hash of `machine-uid`). Unbound
//! tokens (no `dev` claim) work everywhere — used for development and trials.

use ed25519_dalek::{Signature, VerifyingKey, Verifier};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

/// Grace period: 25% of the original license duration.
///   30-day key  → ~7.5 days grace
///   1-year key  → ~91 days grace
fn grace_seconds(iat: i64, exp: i64) -> i64 {
    let duration = exp.saturating_sub(iat);
    if duration > 0 {
        duration * 25 / 100
    } else {
        // Fallback if iat is missing/broken: 7 days.
        7 * 86_400
    }
}

/// Lbby tier hierarchy. Higher value = more access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Free,
    Novice,
    Master,
}

impl Tier {
    fn from_claim(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "master" => Tier::Master,
            "novice" => Tier::Novice,
            _ => Tier::Free,
        }
    }
}

/// Decoded JWT payload from the License Server.
#[derive(Debug, Deserialize, Serialize, Clone)]
struct Claims {
    /// User ID (Telegram user id, stringified).
    sub: String,
    /// Tier name: "novice" or "master".
    tier: String,
    /// Issued-at unix timestamp.
    #[serde(default)]
    iat: i64,
    /// Expiry unix timestamp.
    exp: i64,
    /// Issuer — must equal "lbby" or we reject.
    iss: String,
    /// Optional device fingerprint (SHA-256 of machine-uid, hex-encoded).
    /// If absent the JWT works on any machine — useful for trials & testing.
    #[serde(default)]
    dev: Option<String>,
    /// Optional human label shown in the UI.
    #[serde(default)]
    email: Option<String>,
    /// Optional Telegram username for display.
    #[serde(default)]
    tg_username: Option<String>,
    /// JWT id, used for future revocation.
    #[serde(default)]
    jti: Option<String>,
}

/// Public-facing license state returned to the frontend.
#[derive(Debug, Serialize, Clone, Default)]
pub struct LicenseState {
    pub tier: Tier,
    pub activated: bool,
    /// Subject (user id) from the JWT, if any.
    pub subject: Option<String>,
    /// Telegram handle if the server embedded it.
    pub tg_username: Option<String>,
    /// Email if the server embedded it.
    pub email: Option<String>,
    /// Expiry as ISO 8601 string.
    pub expires_at: Option<String>,
    /// True if the JWT has expired but is within the offline grace window.
    pub in_grace_period: bool,
    /// Human-readable status: "active", "grace", "expired", "invalid_signature", etc.
    pub status: String,
    /// Fingerprint of this machine. Useful for showing the user.
    pub device_id: String,
    /// Whether the active JWT is locked to this device (production tokens
    /// always are; dev/test tokens may be unbound). Frontend uses this to
    /// show the user whether their key would work if shared.
    pub device_bound: bool,
}

impl Default for Tier {
    fn default() -> Self { Tier::Free }
}

// ── Constants ───────────────────────────────────────────────────────────────

const ISSUER: &str = "lbby";
const LICENSE_FILE: &str = "license.jwt";
const REVOKED_URL: &str = "https://lbby-license-server.rstudio.workers.dev/revoked.json";
const REVOKED_CACHE_FILE: &str = "revoked.json";
const REVOKED_CACHE_MAX_AGE_SECS: u64 = 3600; // 1 hour

/// Public key in PEM form, baked into the binary at compile time. The matching
/// private key lives only on the License Server.
const PUBLIC_KEY_PEM: &str = include_str!("../keys/license-public.pem");

// ── On-disk storage ─────────────────────────────────────────────────────────

fn license_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lbby")
}

fn license_path() -> PathBuf {
    license_dir().join(LICENSE_FILE)
}

fn read_stored_token() -> Option<String> {
    fs::read_to_string(license_path()).ok().and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    })
}

fn write_stored_token(token: &str) -> Result<(), String> {
    let dir = license_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create license dir: {}", e))?;
    fs::write(license_path(), token).map_err(|e| format!("Couldn't save license: {}", e))
}

fn delete_stored_token() -> Result<(), String> {
    let path = license_path();
    if path.exists() {
        fs::remove_file(path).map_err(|e| format!("Couldn't delete license: {}", e))?;
    }
    Ok(())
}

// ── Dev tier override ────────────────────────────────────────────────────────
// Allows developers and testers to switch between Free/Novice/Master without
// a real license key. Only works in debug builds; the file is ignored in release.

fn dev_tier_path() -> PathBuf {
    license_dir().join("dev-tier.txt")
}

fn read_dev_tier() -> Option<Tier> {
    let raw = fs::read_to_string(dev_tier_path()).ok()?;
    let trimmed = raw.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "master" => Some(Tier::Master),
        "novice" => Some(Tier::Novice),
        "free" => Some(Tier::Free),
        _ => None,
    }
}

fn write_dev_tier(tier: Option<Tier>) -> Result<(), String> {
    let dir = license_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create config dir: {}", e))?;
    match tier {
        Some(t) => {
            let name = match t {
                Tier::Master => "master",
                Tier::Novice => "novice",
                Tier::Free => "free",
            };
            fs::write(dev_tier_path(), name).map_err(|e| format!("Couldn't write dev tier: {}", e))
        }
        None => {
            if dev_tier_path().exists() {
                fs::remove_file(dev_tier_path()).map_err(|e| format!("Couldn't delete dev tier: {}", e))?;
            }
            Ok(())
        }
    }
}

// ── Device fingerprint ──────────────────────────────────────────────────────

/// SHA-256 of the machine's hardware id, truncated to 32 hex chars. Used to
/// bind a license to a specific machine. We hash so we never store or
/// transmit the raw machine id.
///
/// Falls back through multiple identity sources so two machines where
/// `machine_uid` fails don't accidentally share the same fingerprint.
pub fn device_fingerprint() -> String {
    // 1. machine_uid crate (cross-platform, uses registry / IOPlatformUUID / dbus)
    if let Ok(id) = machine_uid::get() {
        if !id.is_empty() && id != "unknown" {
            return sha256_device(&id);
        }
    }

    // 2. /etc/machine-id (Linux, some macOS setups with systemd)
    #[cfg(unix)]
    {
        if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
            let id = id.trim();
            if !id.is_empty() {
                return sha256_device(id);
            }
        }
    }

    // 3. macOS: IOPlatformUUID via ioreg
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Some(start) = line.find("\"IOPlatformUUID\"") {
                    if let Some(eq) = line[start..].find('"') {
                        let rest = &line[start + eq + 1..];
                        if let Some(end) = rest.find('"') {
                            let uuid = &rest[..end];
                            if !uuid.is_empty() {
                                return sha256_device(uuid);
                            }
                        }
                    }
                }
            }
        }
    }

    // 4. Environment-based fallback — USER+HOME (unix) or COMPUTERNAME+USERNAME (Windows)
    //    Not as stable as hardware IDs but better than a shared constant.
    #[cfg(unix)]
    {
        let user = std::env::var("USER").unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        if !user.is_empty() || !home.is_empty() {
            return sha256_device(&format!("env:{}:{}", user, home));
        }
    }
    #[cfg(windows)]
    {
        let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
        let user = std::env::var("USERNAME").unwrap_or_default();
        if !computer.is_empty() || !user.is_empty() {
            return sha256_device(&format!("env:{}:{}", computer, user));
        }
    }

    // 5. Absolute last resort — shared across all machines that reach here.
    sha256hex(b"lbby-device:unknown")
}

/// Hash a device identifier string with the lbby-device prefix.
fn sha256_device(id: &str) -> String {
    sha256hex(format!("lbby-device:{}", id).as_bytes())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// SHA-256 hash truncated to 16 bytes (32 hex chars), matching original fingerprint format.
fn sha256hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    hex_lower(&digest[..16])
}

// ── Revocation list ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevokedEntry {
    jwt_id: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    revoked_at: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevokedList {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    generated_at: u64,
    entries: Vec<RevokedEntry>,
    #[serde(default)]
    signature: Option<String>,
}

/// Cached revocation list — fetched on startup, refreshed hourly.
static REVOKED_CACHE: std::sync::LazyLock<Mutex<Option<RevokedList>>> = std::sync::LazyLock::new(|| Mutex::new(None));

fn revoked_cache_path() -> PathBuf {
    license_dir().join(REVOKED_CACHE_FILE)
}

fn save_revoked_cache(list: &RevokedList) {
    if let Ok(json) = serde_json::to_string(list) {
        let _ = fs::write(revoked_cache_path(), json);
    }
}

fn load_revoked_cache() -> Option<RevokedList> {
    let raw = fs::read_to_string(revoked_cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Extract the raw 32-byte Ed25519 public key from the PEM SubjectPublicKeyInfo.
fn extract_ed25519_pubkey() -> Option<VerifyingKey> {
    let pem = PUBLIC_KEY_PEM.trim();
    let b64 = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<String>();
    let der = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64).ok()?;
    if der.len() < 44 { return None; }
    // SPKI: [0x30, len, 0x30, len, OID..., 0x03, 0x21, 0x00, <32 bytes key>]
    let key_bytes: [u8; 32] = der[12..44].try_into().ok()?;
    VerifyingKey::from_bytes(&key_bytes).ok()
}

/// Verify the Ed25519 signature on a revocation list.
fn verify_revocation_signature(list: &RevokedList) -> bool {
    let sig_b64 = match &list.signature {
        Some(s) => s,
        None => return false, // unsigned list is rejected
    };

    let key = match extract_ed25519_pubkey() {
        Some(k) => k,
        None => return false,
    };

    let sig_bytes = match base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, sig_b64) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Reconstruct the signed payload (everything except the signature field)
    let payload = serde_json::json!({
        "version": list.version,
        "generatedAt": list.generated_at,
        "entries": list.entries.iter().map(|e| serde_json::json!({
            "jwtId": e.jwt_id,
            "reason": e.reason,
            "revokedAt": e.revoked_at,
        })).collect::<Vec<_>>()
    });

    let payload_bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(_) => return false,
    };

    key.verify(&payload_bytes, &signature).is_ok()
}

/// Fetch the revocation list from the admin server. Runs on startup.
async fn fetch_revoked_list() -> Option<RevokedList> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client.get(REVOKED_URL).send().await.ok()?;
    let list: RevokedList = resp.json().await.ok()?;

    // Verify signature — reject tampered lists
    if !verify_revocation_signature(&list) {
        eprintln!("[license] Revocation list signature invalid — ignoring");
        return None;
    }

    Some(list)
}

/// Initialize the revocation cache. Call once on app startup.
pub async fn init_revocation() {
    // Load cached version first (instant, no network)
    let cached = load_revoked_cache();
    if let Some(ref list) = cached {
        if let Ok(mut guard) = REVOKED_CACHE.lock() {
            *guard = Some(RevokedList {
                version: list.version,
                generated_at: list.generated_at,
                entries: list.entries.iter().map(|e| RevokedEntry {
                    jwt_id: e.jwt_id.clone(),
                    reason: e.reason.clone(),
                    revoked_at: e.revoked_at,
                }).collect(),
                signature: list.signature.clone(),
            });
        }
    }

    // Fetch fresh version in background
    match fetch_revoked_list().await {
        Some(fresh) => {
            save_revoked_cache(&fresh);
            if let Ok(mut guard) = REVOKED_CACHE.lock() {
                *guard = Some(fresh);
            }
        }
        None => {
            // Network failed — use cached version if available
            if cached.is_none() {
                eprintln!("[license] Could not fetch revocation list and no cache available");
            }
        }
    }
}

/// Check if a JWT ID is in the revocation list.
fn is_jwt_revoked(jti: &str) -> bool {
    if let Ok(guard) = REVOKED_CACHE.lock() {
        if let Some(list) = &*guard {
            return list.entries.iter().any(|e| e.jwt_id == jti);
        }
    }
    false
}

// ── Verification ────────────────────────────────────────────────────────────

#[derive(Debug)]
enum VerifyOutcome {
    Active(Claims),
    InGrace(Claims),
    Invalid(String),
}

fn verify(token: &str) -> VerifyOutcome {
    let decoding_key = match DecodingKey::from_ed_pem(PUBLIC_KEY_PEM.as_bytes()) {
        Ok(k) => k,
        Err(e) => return VerifyOutcome::Invalid(format!("Bad public key: {}", e)),
    };

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[ISSUER]);
    validation.leeway = 60;
    // We handle expiry ourselves so we can implement offline grace cleanly.
    validation.validate_exp = false;

    let data = match jsonwebtoken::decode::<Claims>(token, &decoding_key, &validation) {
        Ok(d) => d,
        Err(e) => return VerifyOutcome::Invalid(format!("Invalid license: {}", e)),
    };

    let claims = data.claims;

    if claims.iss != ISSUER {
        return VerifyOutcome::Invalid("License was issued by an untrusted server.".to_string());
    }

    // Device binding check (skip if claim absent).
    if let Some(bound) = &claims.dev {
        let ours = device_fingerprint();
        if !bound.eq_ignore_ascii_case(&ours) {
            return VerifyOutcome::Invalid(
                "This license is bound to a different device. Activate it from this machine via the bot first.".to_string(),
            );
        }
    }

    // Revocation check — if the JWT ID is in the revoked list, reject it.
    if let Some(ref jti) = claims.jti {
        if is_jwt_revoked(jti) {
            return VerifyOutcome::Invalid(
                "This license has been revoked by an administrator.".to_string(),
            );
        }
    }

    let now = chrono::Utc::now().timestamp();
    let grace = grace_seconds(claims.iat, claims.exp);
    if now <= claims.exp {
        VerifyOutcome::Active(claims)
    } else if now <= claims.exp + grace {
        VerifyOutcome::InGrace(claims)
    } else {
        VerifyOutcome::Invalid("License expired past grace period.".to_string())
    }
}

fn iso8601(unix: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0).map(|dt| dt.to_rfc3339())
}

fn build_state(claims: Claims, status: &str, in_grace: bool) -> LicenseState {
    let bound = claims.dev.is_some();
    LicenseState {
        tier: Tier::from_claim(&claims.tier),
        activated: true,
        subject: Some(claims.sub),
        tg_username: claims.tg_username,
        email: claims.email,
        expires_at: iso8601(claims.exp),
        in_grace_period: in_grace,
        status: status.to_string(),
        device_id: device_fingerprint(),
        device_bound: bound,
    }
}

/// Returns the current effective license. Never errors — falls back to Free.
///
/// In debug builds, if a JWT is stored we verify it normally (including
/// expiry) so license expiration can be tested during development. When no
/// JWT is present the dev-tier override kicks in (defaults to Master).
pub fn current() -> LicenseState {
    // If a JWT is on disk, always verify it — even in debug builds — so
    // expiry / grace / device-binding can be tested during development.
    if let Some(token) = read_stored_token() {
        return match verify(&token) {
            VerifyOutcome::Active(c) => build_state(c, "active", false),
            VerifyOutcome::InGrace(c) => build_state(c, "grace", true),
            VerifyOutcome::Invalid(reason) => LicenseState {
                tier: Tier::Free,
                activated: false,
                status: format!("invalid: {}", reason),
                device_id: device_fingerprint(),
                device_bound: false,
                ..Default::default()
            },
        };
    }

    // No JWT stored — in debug builds fall back to the dev-tier override
    // (default Master) so developers aren't gated during day-to-day work.
    #[cfg(debug_assertions)]
    {
        let tier = read_dev_tier().unwrap_or(Tier::Master);
        return LicenseState {
            tier,
            activated: true,
            subject: Some("dev-mode".to_string()),
            status: "active (dev)".to_string(),
            device_id: device_fingerprint(),
            device_bound: false,
            ..Default::default()
        };
    }

    #[cfg(not(debug_assertions))]
    {
        LicenseState {
            tier: Tier::Free,
            status: "free".to_string(),
            device_id: device_fingerprint(),
            device_bound: false,
            ..Default::default()
        }
    }
}

/// Verifies + persists a JWT. Rejects anything below an "Active" verdict —
/// no point activating something that's already expired past grace.
pub fn activate(token: &str) -> Result<LicenseState, String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("Paste a license key first.".to_string());
    }
    match verify(trimmed) {
        VerifyOutcome::Active(c) => {
            write_stored_token(trimmed)?;
            Ok(build_state(c, "active", false))
        }
        VerifyOutcome::InGrace(_) => {
            Err("This license has expired. Renew it first, then activate the new key.".to_string())
        }
        VerifyOutcome::Invalid(reason) => Err(reason),
    }
}

/// Wipes the stored JWT (sign-out).
pub fn deactivate() -> Result<LicenseState, String> {
    delete_stored_token()?;
    Ok(current())
}

/// Returns true when running in a debug (dev) build. The frontend uses this
/// to decide whether to show the dev-tier override controls.
#[cfg(debug_assertions)]
pub fn is_dev_build() -> bool { true }

#[cfg(not(debug_assertions))]
pub fn is_dev_build() -> bool { false }

/// Sets or clears the dev-tier override. Only works in debug builds;
/// returns an error in release.
pub fn set_dev_tier(tier: Option<Tier>) -> Result<LicenseState, String> {
    if !is_dev_build() {
        return Err("Dev tier override is only available in debug builds.".to_string());
    }
    write_dev_tier(tier)?;
    Ok(current())
}

/// Helper for Tauri commands that need a minimum tier. Use as:
///     license::require(Tier::Master)?;
pub fn require(min: Tier) -> Result<(), String> {
    let state = current();
    if state.tier >= min {
        Ok(())
    } else {
        Err(format!(
            "This feature requires the {:?} tier. Open the License tab to upgrade.",
            min
        ))
    }
}

// ── Tier-based quotas ──────────────────────────────────────────────────────
// Resource limits per tier. Living here (not the database) so we can tune
// them without a migration. Mirrored in the frontend License perks list.

/// Max number of server profiles the user can have at once.
pub fn max_profiles(tier: Tier) -> u32 {
    match tier {
        Tier::Free => 1,
        Tier::Novice => 3,
        Tier::Master => u32::MAX,
    }
}

/// Max number of chunks the user can request in a single pre-generation run.
pub fn max_pregen_chunks(tier: Tier) -> u32 {
    match tier {
        Tier::Free => 441,       // small preset only
        Tier::Novice => 4_096,   // up to "Large" preset
        Tier::Master => u32::MAX,
    }
}

/// Whether the auto-backup scheduler is allowed to run at all.
pub fn auto_backup_allowed(tier: Tier) -> bool {
    tier >= Tier::Novice
}
