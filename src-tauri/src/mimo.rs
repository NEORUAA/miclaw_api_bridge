use crate::auth::AuthState;
use crate::error::{BridgeError, Result};
use crate::storage::Storage;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use parking_lot::RwLock;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// All mimo PC traffic terminates at this host.
pub const MIMO_HOST: &str = "https://api.miclaw.xiaomi.net";

/// Resolved upstream base URL. `MIMO_HOST_OVERRIDE` exists for integration
/// tests / mock servers (plain-http upstreams); production leaves it unset.
fn mimo_host() -> String {
    std::env::var("MIMO_HOST_OVERRIDE").unwrap_or_else(|_| MIMO_HOST.to_string())
}

/// Xiaomi HyperConnect / Super XiaoAI uses the v2 PC OpenAI transport. The
/// billing router expects these query parameters on every LLM request.
pub const PATH_CHAT: &str =
    "/osbot/pc/llm/v2/chat/completions?bizId=xiaoai_pc&featureId=common&isFirstQuery=false";

/// OpenAI Responses-shaped endpoint on the migrated v2 PC transport.
pub const PATH_RESPONSES: &str =
    "/osbot/pc/llm/v2/responses?bizId=xiaoai_pc&featureId=common&isFirstQuery=false";

/// Commercialization quota used by Super XiaoAI's expert-mode UI.
pub const PATH_QUOTA: &str = "/osbot/pc/user/v2/status?bizId=xiaoai_pc&featureId=common";

/// MCP host service exposed by miclaw PC. Out of scope for the bridge today;
/// kept here so we don't accidentally collide with it.
#[allow(dead_code)]
pub const PATH_MCP_STREAMABLE: &str = "/osbot/pc/mcp/v1/streamable";

/// Default model selected by the migrated official desktop client.
pub const MODEL_DEFAULT: &str = "xiaomi/mimo-pro";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    pub owned_by: String,
    pub family: String,
}

/// Models confirmed against the migrated v2 PC channel on 2026-09-03. The
/// official client routes new sessions through `xiaomi/mimo` and
/// `xiaomi/mimo-pro`; provider-qualified legacy ids remain available.
///
/// Notes:
/// * The bridge passes `model` through verbatim — the upstream router
///   handles canonicalization (`xiaomi/mimo-claw-0301` echoes back as
///   `mimo-pro`, the `mimo-omni`/`mimo` aliases echo as `mimo`).
/// * `mimo`, `mimo-omni`, and `mimo-pro` remain working short aliases.
/// * The short `mimo-v2.5*` ids are intentionally excluded because v2 rejects
///   them without a provider; their `xiaomi/`-qualified forms work.
pub fn known_models() -> Vec<ModelInfo> {
    vec![
        // ── Models selected by the migrated official client ────────────────
        ModelInfo {
            id: "xiaomi/mimo".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "multimodal (text+vision+tools+thinking, 1M ctx, 128K out) [upstream: mimo]"
                .into(),
        },
        ModelInfo {
            id: "xiaomi/mimo-pro".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "reasoning (text+tools+thinking, 1M ctx, 128K out) [upstream: mimo-pro]".into(),
        },
        ModelInfo {
            id: "xiaomi/mimo-v2.5".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "provider-qualified v2.5 model [upstream: mimo-v2.5]".into(),
        },
        ModelInfo {
            id: "xiaomi/mimo-v2.5-pro".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "provider-qualified v2.5 reasoning model [upstream: mimo-v2.5-pro]".into(),
        },
        // ── Legacy provider-qualified ids still accepted by v2 ─────────────
        ModelInfo {
            id: "xiaomi/mimo-claw-0301".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "legacy reasoning snapshot [upstream: mimo-pro]".into(),
        },
        ModelInfo {
            id: "xiaomi/MiniMax-M2.5".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "general (text+tools, 128K ctx, 8K out) [upstream: MiniMax-M2.5]".into(),
        },
        ModelInfo {
            id: "xiaomi/kimi-k2.5".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "reasoning (text+tools+thinking, 128K ctx, 8K out) [upstream: kimi-k2.5]"
                .into(),
        },
        ModelInfo {
            id: "xiaomi/glm-5".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "general (text+tools, 128K ctx, 8K out) [upstream: glm-5]".into(),
        },
        // ── Short aliases accepted by the upstream router ──────────────────
        ModelInfo {
            id: "mimo".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "alias → xiaomi/mimo [upstream: mimo]".into(),
        },
        ModelInfo {
            id: "mimo-omni".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "alias → xiaomi/mimo [upstream: mimo]".into(),
        },
        ModelInfo {
            id: "mimo-pro".into(),
            object: "model".into(),
            owned_by: "xiaomi".into(),
            family: "alias → xiaomi/mimo-pro [upstream: mimo-pro]".into(),
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaSnapshot {
    pub membership_level: u64,
    pub membership_level_name: Option<String>,
    pub membership_type: Option<String>,
    pub membership_expire_at: Option<i64>,
    pub auto_renewal: Option<bool>,
    pub points_limit: u64,
    pub points_used: u64,
    pub points_remaining: u64,
    pub usage_ratio: f64,
    pub quota_reset_at: i64,
    pub can_upgrade: bool,
    pub abnormal: bool,
    pub status: String,
    pub observed_at: i64,
}

#[derive(Debug, Deserialize)]
struct QuotaEnvelope {
    code: i64,
    data: Option<QuotaData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaData {
    level: u64,
    #[serde(default)]
    level_name: Option<String>,
    #[serde(default)]
    expire_time: Option<i64>,
    #[serde(default)]
    auto_renewal: Option<bool>,
    credit_amount: u64,
    used_amount: u64,
    credit_expire_time: i64,
    can_upgrade: bool,
    #[serde(default)]
    abnormal: Option<u8>,
}

impl QuotaData {
    fn into_snapshot(self) -> QuotaSnapshot {
        let points_remaining = self.credit_amount.saturating_sub(self.used_amount);
        let usage_ratio = if self.credit_amount == 0 {
            1.0
        } else {
            (self.used_amount as f64 / self.credit_amount as f64).min(1.0)
        };
        let abnormal = self.abnormal == Some(1);
        let status = if abnormal {
            "available"
        } else if points_remaining == 0 {
            "exhausted"
        } else if points_remaining.saturating_mul(5) < self.credit_amount {
            "low"
        } else {
            "available"
        };
        let membership_type = match self.level {
            0 => Some("free"),
            5_000 => Some("starter"),
            10_000 => Some("standard"),
            20_000 => Some("professional"),
            30_000 => Some("ultimate"),
            _ => None,
        }
        .map(str::to_string);

        QuotaSnapshot {
            membership_level: self.level,
            membership_level_name: self.level_name,
            membership_type,
            membership_expire_at: self.expire_time,
            auto_renewal: self.auto_renewal,
            points_limit: self.credit_amount,
            points_used: self.used_amount,
            points_remaining,
            usage_ratio,
            quota_reset_at: self.credit_expire_time,
            can_upgrade: self.can_upgrade,
            abnormal,
            status: status.to_string(),
            observed_at: chrono::Utc::now().timestamp_millis(),
        }
    }
}

pub struct MimoClient {
    auth: Arc<RwLock<AuthState>>,
    storage: Option<Arc<Storage>>,
    /// One long-lived client for ALL mimo traffic. Previously a fresh client
    /// was built per request, which meant zero connection reuse (a full
    /// TCP+TLS handshake + DNS lookup on every call) and constant allocator
    /// churn (each client loads its own TLS roots / connector / pool) — on
    /// long-running deployments that showed up as connect stalls and steadily
    /// growing RSS. The auth token travels in per-request headers, so the
    /// client itself never needs rebuilding, even across token refreshes.
    client: reqwest::Client,
}

/// Total ceiling for one upstream call, generous because streamed
/// completions legitimately run for minutes (the old blanket 30s timeout
/// killed any generation slower than that — the dominant source of
/// "error sending request … 30000ms" failures). Override with
/// `MIMO_UPSTREAM_TIMEOUT_SECS`; connect has its own much tighter timeout.
fn upstream_timeout() -> Duration {
    std::env::var("MIMO_UPSTREAM_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(600))
}

fn build_mimo_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .gzip(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(upstream_timeout())
        // Dead peers are reaped via keepalive; idle pooled connections are
        // recycled for 90s so back-to-back requests skip the handshake.
        .tcp_keepalive(Duration::from_secs(60))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .build()
        .map_err(BridgeError::from)
}

impl MimoClient {
    pub fn new(auth: Arc<RwLock<AuthState>>, storage: Option<Arc<Storage>>) -> Result<Self> {
        Ok(Self {
            auth,
            storage,
            client: build_mimo_client()?,
        })
    }

    pub fn auth_handle(&self) -> Arc<RwLock<AuthState>> {
        self.auth.clone()
    }

    fn snapshot(&self) -> Result<crate::auth::Session> {
        let snap = self.auth.read().session.clone();
        if !snap.is_authenticated() {
            return Err(BridgeError::NotAuthenticated);
        }
        Ok(snap)
    }

    /// Headers accepted by the migrated PC transport: a `node` UA and the
    /// sid=miclaw serviceToken cookie. Include the account ids when present,
    /// matching the official client's service-token fetcher.
    fn build_headers(&self, session: &crate::auth::Session) -> Result<HeaderMap> {
        let cookie = session
            .cookie_header()
            .ok_or(BridgeError::NotAuthenticated)?;
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("node"),
        );
        h.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("*/*"),
        );
        h.insert(
            HeaderName::from_static("accept-language"),
            HeaderValue::from_static("*"),
        );
        h.insert(
            HeaderName::from_static("sec-fetch-mode"),
            HeaderValue::from_static("cors"),
        );
        h.insert(
            HeaderName::from_static("accept-encoding"),
            HeaderValue::from_static("gzip"),
        );
        h.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_str(&cookie).map_err(BridgeError::other)?,
        );
        Ok(h)
    }

    /// Forward a JSON body to mimo. Streaming is requested by the JSON body
    /// itself (`"stream": true`); upstream returns SSE in that case.
    ///
    /// On a 401 we transparently mint a fresh sid=miclaw serviceToken from
    /// the long-lived passToken and replay the request once.
    pub async fn post_json(&self, path: &str, body: Value) -> Result<reqwest::Response> {
        let resp = self.post_json_once(path, body.clone()).await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        tracing::warn!(
            target = "mimo",
            "{path} got 401, refreshing sid=miclaw serviceToken"
        );
        let _ = resp.bytes().await; // drain
        match self.refresh_service_token().await {
            Ok(()) => {
                tracing::info!(target = "mimo", "serviceToken refreshed, retrying once");
                self.post_json_once(path, body).await
            }
            Err(e) => {
                tracing::warn!(target = "mimo", "swap failed during 401 refresh: {e}");
                Err(BridgeError::NotAuthenticated)
            }
        }
    }

    async fn post_json_once(&self, path: &str, body: Value) -> Result<reqwest::Response> {
        let session = self.snapshot()?;
        let headers = self.build_headers(&session)?;
        // Diagnostic: cookie shape (lengths only, never values).
        if let Some(c) = headers.get("cookie").and_then(|v| v.to_str().ok()) {
            let parts: Vec<String> = c
                .split(';')
                .map(str::trim)
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    let k = it.next()?;
                    let v = it.next().unwrap_or("");
                    Some(format!("{k}(len={})", v.len()))
                })
                .collect();
            tracing::debug!(target = "mimo", "cookie shape: [{}]", parts.join(", "));
        }
        let resp = self
            .client
            .request(Method::POST, format!("{}{path}", mimo_host()))
            .headers(headers)
            .json(&body)
            .send()
            .await?;
        Ok(resp)
    }

    pub async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let resp = self.get_once(path).await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        tracing::warn!(
            target = "mimo",
            "{path} got 401, refreshing sid=miclaw serviceToken"
        );
        let _ = resp.bytes().await;
        match self.refresh_service_token().await {
            Ok(()) => self.get_once(path).await,
            Err(e) => {
                tracing::warn!(target = "mimo", "serviceToken refresh failed: {e}");
                Err(BridgeError::NotAuthenticated)
            }
        }
    }

    async fn get_once(&self, path: &str) -> Result<reqwest::Response> {
        let session = self.snapshot()?;
        let headers = self.build_headers(&session)?;
        self.client
            .request(Method::GET, format!("{}{path}", mimo_host()))
            .headers(headers)
            .send()
            .await
            .map_err(BridgeError::from)
    }

    /// Re-runs the sid=miclaw token mint using the persisted passToken.
    /// Returns `Err(NotAuthenticated)` when a full login is required.
    async fn refresh_service_token(&self) -> Result<()> {
        let session = self.auth.read().session.clone();
        if session.pass_token.is_none() {
            return Err(BridgeError::NotAuthenticated);
        }
        // The mint doesn't use the first arg today (it builds a jar-less
        // client); pass our shared client to keep the interface explicit.
        let next = crate::auth::login::mint_miclaw_service_token(&self.client, session).await?;
        let mut guard = self.auth.write();
        guard.session = next;
        if let Some(storage) = &self.storage {
            if let Err(error) = guard.save(storage) {
                tracing::warn!(
                    target = "mimo",
                    "refreshed serviceToken could not be persisted: {error}"
                );
            }
        }
        Ok(())
    }

    pub async fn post_stream(
        &self,
        path: &str,
        body: Value,
    ) -> Result<(
        reqwest::StatusCode,
        HeaderMap,
        BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>,
    )> {
        let resp = self.post_json(path, body).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let stream = resp.bytes_stream().boxed();
        Ok((status, headers, stream))
    }

    pub async fn chat(&self, body: Value) -> Result<reqwest::Response> {
        self.post_json(PATH_CHAT, body).await
    }

    pub async fn quota(&self) -> Result<QuotaSnapshot> {
        let response = self.get(PATH_QUOTA).await?;
        let status = response.status();
        if !status.is_success() {
            let _ = response.bytes().await;
            return Err(BridgeError::Proxy(format!(
                "quota endpoint returned {status}"
            )));
        }
        let envelope: QuotaEnvelope = response.json().await?;
        if envelope.code != 0 {
            return Err(BridgeError::Proxy(format!(
                "quota endpoint returned code {}",
                envelope.code
            )));
        }
        envelope
            .data
            .map(QuotaData::into_snapshot)
            .ok_or_else(|| BridgeError::Proxy("quota endpoint returned no data".into()))
    }

    pub fn quick_status(&self) -> AuthSnapshot {
        let auth = self.auth.read();
        AuthSnapshot {
            authenticated: auth.session.is_authenticated(),
            nick: auth.session.nick.clone(),
            user_id: auth.session.user_id.clone(),
            refreshed_at: auth.session.refreshed_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSnapshot {
    pub authenticated: bool,
    pub nick: Option<String>,
    pub user_id: Option<String>,
    pub refreshed_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_snapshot_matches_official_remaining_and_low_threshold() {
        let snapshot = QuotaData {
            level: 20_000,
            level_name: Some("加强".into()),
            expire_time: Some(1_796_140_799_999),
            auto_renewal: Some(false),
            credit_amount: 34_000,
            used_amount: 28_000,
            credit_expire_time: 1_790_870_399_999,
            can_upgrade: false,
            abnormal: None,
        }
        .into_snapshot();

        assert_eq!(snapshot.points_remaining, 6_000);
        assert_eq!(snapshot.membership_type.as_deref(), Some("professional"));
        assert_eq!(snapshot.status, "low");
    }

    #[test]
    fn quota_snapshot_clamps_overspend_to_zero() {
        let snapshot = QuotaData {
            level: 0,
            level_name: None,
            expire_time: None,
            auto_renewal: None,
            credit_amount: 100,
            used_amount: 120,
            credit_expire_time: 1,
            can_upgrade: true,
            abnormal: Some(0),
        }
        .into_snapshot();

        assert_eq!(snapshot.points_remaining, 0);
        assert_eq!(snapshot.usage_ratio, 1.0);
        assert_eq!(snapshot.status, "exhausted");
    }

    #[test]
    fn migrated_model_manifest_includes_provider_qualified_v25_ids() {
        let ids: Vec<String> = known_models().into_iter().map(|model| model.id).collect();
        assert!(ids.contains(&"xiaomi/mimo-v2.5".to_string()));
        assert!(ids.contains(&"xiaomi/mimo-v2.5-pro".to_string()));
        assert!(!ids.contains(&"mimo-v2.5".to_string()));
        assert!(!ids.contains(&"mimo-v2.5-pro".to_string()));
    }
}
