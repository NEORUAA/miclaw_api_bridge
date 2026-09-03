use crate::error::{BridgeError, Result};
use crate::storage::Storage;
use parking_lot::Mutex;
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use url::Url;

pub mod login;
pub mod refresh;
pub mod two_factor;

/// User-Agent used for the Xiaomi account OAuth dance. The Updater-KMP
/// reference project uses this exact Dalvik string and is known to work
/// end-to-end including SMS 2FA delivery. The mimo PC `node` UA goes
/// elsewhere — see `mimo.rs`.
pub const DEFAULT_USER_AGENT: &str =
    "Dalvik/2.1.0 (Linux; U; Android 16; 2509FPN0BC Build/BP2A.250605.031.A3)";

/// User-Agent used when asking Xiaomi Passport for the PC serviceToken. The
/// value only needs to identify a native PC client; inference calls use the
/// lightweight `node` user agent used by the official client transport.
pub const PC_USER_AGENT: &str = "miNative PC/Normal(Apple Mac16,10) Darwin/25.6.0 SDKV/0.0.1 MK/TWFjLW1pbmkubGFu L/zh-CN DEVT/UEM= DEVS/TWFj BRA/QXBwbGU= APP/Xiaomi miclaw APPV/0.0.1-beta.114+a3e3203f Chrome/144.0.7559.111";

/// Business `sid` used by Xiaomi HyperConnect / Super XiaoAI for LLM and
/// quota requests. Older miclaw builds used an `osbotapi` token here; the
/// migrated client explicitly discards that cached token and mints `miclaw`.
pub const MICLAW_SID: &str = "miclaw";

/// `sid` used for the password / 2FA leg. Verified empirically against the
/// reference KMP project and our own logs.
pub fn sid() -> String {
    std::env::var("MIMO_BRIDGE_SID").unwrap_or_else(|_| MICLAW_SID.to_string())
}

/// Compatibility constant — prefer `sid()`.
pub const SID: &str = "miclaw";

pub const ACCOUNT_HOST: &str = "https://account.xiaomi.com";
pub const SERVICE_LOGIN_URL: &str =
    "https://account.xiaomi.com/pass/serviceLogin?sid=xiaomihome&_json=true";
pub const SERVICE_LOGIN_AUTH2_URL: &str = "https://account.xiaomi.com/pass/serviceLoginAuth2";

/// Persisted snapshot of the Xiaomi account session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub user_id: Option<String>,
    pub c_user_id: Option<String>,
    pub pass_token: Option<String>,
    pub ssecurity: Option<String>,
    pub service_token: Option<String>,
    pub nick: Option<String>,
    /// Unix-ms when the session was last refreshed. Used by UI only.
    pub refreshed_at: Option<i64>,
}

impl Session {
    /// Returns true if we have at least a serviceToken — that's all the
    /// mimo PC endpoint actually needs. The full triplet is only required
    /// for the refresh path.
    pub fn is_authenticated(&self) -> bool {
        self.service_token.is_some()
    }

    /// Stricter check: full triplet present (needed before a refresh).
    #[allow(dead_code)]
    pub fn can_refresh(&self) -> bool {
        self.service_token.is_some() && self.pass_token.is_some() && self.user_id.is_some()
    }

    pub fn cookie_header(&self) -> Option<String> {
        match (&self.c_user_id, &self.user_id, &self.service_token) {
            (Some(c), Some(u), Some(t)) => {
                Some(format!("cUserId={c}; userId={u}; serviceToken={t}"))
            }
            (Some(c), None, Some(t)) => Some(format!("cUserId={c}; serviceToken={t}")),
            (None, None, Some(t)) => Some(format!("serviceToken={t}")),
            _ => None,
        }
    }
}

/// In-flight login flow state. Mirrors `LoginFlowStorage` from the KMP
/// reference: identity_session is server-issued during 2FA pre-flight and
/// must be reused (via a shared cookie jar) for sendTicket / verifyTicket.
///
/// `pending_account` / `pending_password_hash` carry the credentials we need
/// to replay the `serviceLoginAuth2` call after 2FA succeeds — at that
/// point the server returns ssecurity/passToken/userId/cUserId in one shot.
#[derive(Debug, Default, Clone)]
pub struct LoginFlowContext {
    pub identity_session: Option<String>,
    pub notification_url: Option<String>,
    pub two_factor_options: Vec<i32>,
    pub captcha_url: Option<String>,
    pub pending_account: Option<String>,
    pub pending_password_hash: Option<String>,
}

/// Authentication state plus the live HTTP transport.
///
/// Critical: the same `LoginTransport` (and therefore the same cookie jar)
/// MUST be used across `serviceLoginAuth2` → `identity/list` → `sendTicket`
/// → `verifyTicket`. Xiaomi's passport sets opaque session cookies during
/// `identity/list` that are required for the SMS dispatch to actually fire;
/// rebuilding the client between steps silently drops the SMS even though
/// the API returns 200.
pub struct AuthState {
    pub session: Session,
    pub flow: LoginFlowContext,
    pub transport: Mutex<Option<LoginTransport>>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            session: Session::default(),
            flow: LoginFlowContext::default(),
            transport: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("session", &self.session)
            .field("flow", &self.flow)
            .finish()
    }
}

#[derive(Clone)]
pub struct LoginTransport {
    pub client: reqwest::Client,
    pub jar: Arc<Jar>,
}

const SESSION_BLOB: &str = "session";
const KEYRING_SERVICE: &str = "com.neoruaa.miclaw-api-bridge";
const KEYRING_USER: &str = "session";
const DISABLE_KEYRING_ENV: &str = "MICLAW_API_BRIDGE_DISABLE_KEYRING";

impl AuthState {
    pub fn load(storage: &Storage) -> Result<Self> {
        // Prefer the OS keyring; fall back to the on-disk blob from earlier
        // versions so people who upgrade keep their session.
        let session = if keyring_disabled() {
            storage.load_blob(SESSION_BLOB)?.unwrap_or_default()
        } else {
            match keyring_load() {
                Ok(Some(s)) => s,
                _ => storage.load_blob(SESSION_BLOB)?.unwrap_or_default(),
            }
        };
        Ok(Self {
            session,
            flow: LoginFlowContext::default(),
            transport: Mutex::new(None),
        })
    }

    pub fn save(&self, storage: &Storage) -> Result<()> {
        if keyring_disabled() {
            storage.save_blob(SESSION_BLOB, &self.session)?;
            return Ok(());
        }

        if let Err(e) = keyring_save(&self.session) {
            tracing::warn!(
                target = "auth",
                "keyring write failed, falling back to disk: {e}"
            );
            storage.save_blob(SESSION_BLOB, &self.session)?;
        } else {
            // Successfully written to keyring — remove any stale plaintext.
            let _ = storage.delete_blob(SESSION_BLOB);
        }
        Ok(())
    }

    pub fn clear(storage: &Storage) -> Result<()> {
        if !keyring_disabled() {
            let _ = keyring_clear();
        }
        storage.delete_blob(SESSION_BLOB)?;
        Ok(())
    }

    /// Construct (or reuse) the long-lived HTTP transport for this login
    /// session. Returns the cloned client + jar; both are cheap to clone.
    pub fn ensure_transport(&self) -> Result<LoginTransport> {
        let mut guard = self.transport.lock();
        if let Some(t) = guard.as_ref() {
            return Ok(t.clone());
        }
        let transport = build_login_transport()?;
        *guard = Some(transport.clone());
        Ok(transport)
    }

    /// Drop the current transport — used after a successful login or logout
    /// so the next attempt starts from a fresh cookie jar.
    pub fn reset_transport(&self) {
        *self.transport.lock() = None;
    }
}

fn keyring_disabled() -> bool {
    std::env::var(DISABLE_KEYRING_ENV)
        .map(|v| {
            matches!(
                v.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| BridgeError::Storage(format!("keyring entry: {e}")))
}

fn keyring_load() -> Result<Option<Session>> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(s) if !s.is_empty() => match serde_json::from_str(&s) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(BridgeError::Storage(format!("keyring decode: {e}"))),
        },
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(BridgeError::Storage(format!("keyring read: {e}"))),
    }
}

fn keyring_save(session: &Session) -> Result<()> {
    let json = serde_json::to_string(session)?;
    keyring_entry()?
        .set_password(&json)
        .map_err(|e| BridgeError::Storage(format!("keyring write: {e}")))?;
    Ok(())
}

fn keyring_clear() -> Result<()> {
    match keyring_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(BridgeError::Storage(format!("keyring delete: {e}"))),
    }
}

/// Build a fresh HTTP client + cookie jar tuned for the Xiaomi passport.
///
/// Notes:
/// * `cookie_provider(jar)` makes the client respect Set-Cookie automatically
///   (mirrors KMP's `AcceptAllCookiesStorage`).
/// * No `Cookie` header is preset: the jar will be populated as the server
///   issues cookies through the OAuth flow.
fn build_login_transport() -> Result<LoginTransport> {
    let jar = Arc::new(Jar::default());
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
    let client = reqwest::Client::builder()
        .cookie_provider(jar.clone())
        .default_headers(headers)
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(BridgeError::from)?;
    Ok(LoginTransport { client, jar })
}

/// Build a fresh HTTP client equipped with a cookie jar that already contains
/// `passToken` / `userId` from the persisted session, and the Xiaomi UA. Used
/// by the refresh path which deliberately wants a clean jar seeded from disk.
pub fn build_refresh_client(session: &Session) -> Result<(reqwest::Client, Arc<Jar>)> {
    let jar = Arc::new(Jar::default());
    if let (Some(token), Some(uid)) = (&session.pass_token, &session.user_id) {
        let url: Url = ACCOUNT_HOST.parse().expect("static url");
        jar.add_cookie_str(
            &format!("passToken={token}; Domain=.xiaomi.com; Path=/"),
            &url,
        );
        jar.add_cookie_str(&format!("userId={uid}; Domain=.xiaomi.com; Path=/"), &url);
        if let Some(c) = &session.c_user_id {
            jar.add_cookie_str(&format!("cUserId={c}; Domain=.xiaomi.com; Path=/"), &url);
        }
    }
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
    let client = reqwest::Client::builder()
        .cookie_provider(jar.clone())
        .default_headers(headers)
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(BridgeError::from)?;
    Ok((client, jar))
}

/// Backwards-compatible alias used by the smoke test and the mimo client.
/// Both want a one-shot client built from a persisted `Session`; this is the
/// same code path as `build_refresh_client`.
pub fn build_http_client(session: &Session) -> Result<(reqwest::Client, Arc<Jar>)> {
    build_refresh_client(session)
}

/// Strip Xiaomi's anti-CSRF JSON prefix `&&&START&&&`.
pub fn strip_prefix(body: &str) -> &str {
    body.trim_start_matches("&&&START&&&")
}

#[allow(dead_code)]
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .find_map(|v| v.to_str().ok().and_then(|s| extract_cookie(s, name)))
}

fn extract_cookie(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let trimmed = part.trim();
        if let Some(rest) = trimmed.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Re-exports.
pub use login::login;
pub use refresh::refresh as refresh_session;
pub use two_factor::{send_ticket, verify_ticket};
