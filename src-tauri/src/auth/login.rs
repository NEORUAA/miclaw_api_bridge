use super::{strip_prefix, AuthState, LoginFlowContext, Session, SERVICE_LOGIN_AUTH2_URL};
use crate::auth::two_factor::extract_query_param;
use crate::error::{BridgeError, Result};
use crate::storage::Storage;
use md5::{Digest, Md5};
use parking_lot::RwLock;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LoginRequest {
    pub account: String,
    pub password: String,
    #[serde(default)]
    pub captcha: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LoginOutcome {
    /// Login succeeded; session has been written.
    Authenticated {
        nick: Option<String>,
    },
    /// 2FA required, the front-end should pick a method (4=phone, 8=email)
    /// from `options` then ask for a verification ticket.
    TwoFactorRequired {
        options: Vec<i32>,
    },
    /// Server requires a captcha. The URL points to an image returned by
    /// `account.xiaomi.com/pass/getCode`.
    CaptchaRequired {
        captcha_url: String,
    },
    Failed {
        code: i64,
        description: String,
    },
}

/// Run `serviceLoginAuth2` and either:
///   * stash a `Session` and return Authenticated, or
///   * detect 2FA / captcha and return the appropriate outcome.
///
/// The HTTP transport (and its cookie jar) is created lazily on `state` and
/// reused across the full 2FA flow — this is critical for SMS dispatch.
pub async fn login(
    state: &Arc<RwLock<AuthState>>,
    storage: &Arc<Storage>,
    req: LoginRequest,
) -> Result<LoginOutcome> {
    if req.account.is_empty() || req.password.is_empty() {
        return Err(BridgeError::Login("empty credentials".into()));
    }

    // Each fresh login attempt starts from an empty jar so leftovers from a
    // previous failed attempt don't contaminate the new one.
    {
        let guard = state.read();
        guard.reset_transport();
    }
    let transport = state.read().ensure_transport()?;
    let client = transport.client.clone();

    let hash = md5_upper(&req.password);
    // Mirror the KMP reference: keep the form minimal. The miclaw Android
    // build adds _sign/qs/callback for replay protection but those fields
    // are NOT required by the server and including them perturbs the SMS
    // dispatch path on some accounts. Drop them.
    let active_sid = super::sid();
    tracing::debug!(target = "auth", "serviceLoginAuth2 sid={active_sid}");
    let mut form: Vec<(&str, String)> = vec![
        ("user", req.account.clone()),
        ("hash", hash.clone()),
        ("sid", active_sid),
        ("_json", "true".into()),
        ("_locale", "zh_CN".into()),
    ];
    if let Some(c) = req.captcha.as_ref() {
        if !c.is_empty() {
            form.push(("captCode", c.clone()));
        }
    }
    let resp = client
        .post(SERVICE_LOGIN_AUTH2_URL)
        .form(&form)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(BridgeError::Login(format!(
            "serviceLoginAuth2 status {}",
            resp.status()
        )));
    }
    let resp_text = resp.text().await?;
    let body: Value = serde_json::from_str(strip_prefix(&resp_text)).map_err(|e| {
        let preview: String = resp_text.chars().take(400).collect();
        BridgeError::Login(format!("auth2 parse: {e} (body[..400]={preview:?})"))
    })?;

    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Captcha flow: server signals with a captchaUrl pointing to a JPG.
    if let Some(captcha_url) = body.get("captchaUrl").and_then(|v| v.as_str()) {
        let mut guard = state.write();
        guard.flow = LoginFlowContext {
            captcha_url: Some(captcha_url.to_string()),
            ..Default::default()
        };
        return Ok(LoginOutcome::CaptchaRequired {
            captcha_url: captcha_url.to_string(),
        });
    }

    let notification_url = body.get("notificationUrl").and_then(|v| v.as_str());
    if let Some(url) = notification_url {
        if !url.is_empty() && url != "null" {
            // 2FA required. Per KMP reference: the path segment Xiaomi uses
            // here is "fe/service/identity/authStart" — replace with
            // "identity/list" for the JSON form. We tolerate both observed
            // path forms just in case.
            let list_url = url
                .replace("fe/service/identity/authStart", "identity/list")
                .replace("fe/service/identityauthStart", "identity/list");
            tracing::debug!(target = "auth", "identity/list GET {list_url}");
            let list_resp = client.get(&list_url).send().await?;
            // identity_session is set by the server here. The reqwest cookie
            // jar will pick it up automatically; we also extract it for
            // diagnostics.
            let id_session = list_resp
                .cookies()
                .find(|c| c.name() == "identity_session")
                .map(|c| c.value().to_string());
            let status = list_resp.status();
            let body_text = list_resp.text().await?;
            let stripped = strip_prefix(&body_text);
            let list_json: Value = serde_json::from_str(stripped).map_err(|e| {
                let preview: String = body_text.chars().take(400).collect();
                BridgeError::Login(format!(
                    "identity/list parse: {e} (status={status}, body[..400]={preview:?})"
                ))
            })?;

            let two_factor_unsupported = list_json
                .get("twoFactorAuth")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if two_factor_unsupported {
                return Err(BridgeError::Login(
                    "this account requires hardware 2FA which is not supported".into(),
                ));
            }
            let options: Vec<i32> = list_json
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_i64().map(|n| n as i32))
                        .collect()
                })
                .unwrap_or_default();
            if options.is_empty() {
                return Err(BridgeError::Login(
                    "identity/list returned no options".into(),
                ));
            }

            let context = extract_query_param(url, "context").map(|s| s.to_string());
            let mut guard = state.write();
            guard.flow = LoginFlowContext {
                identity_session: id_session,
                notification_url: Some(url.to_string()),
                two_factor_options: options.clone(),
                captcha_url: None,
                // Stash credentials so verify_ticket can replay
                // serviceLoginAuth2 once the identity_session is trusted.
                pending_account: Some(req.account.clone()),
                pending_password_hash: Some(hash.clone()),
            };
            let _ = context;
            return Ok(LoginOutcome::TwoFactorRequired { options });
        }
    }

    // Single-step login: pull out final fields.
    if code != 0 {
        return Ok(LoginOutcome::Failed { code, description });
    }
    let session = parse_session_fields(&body);
    // Skip the auth2 location (it points at a sid=login-sid sts that 401s).
    // Mint the same sid=miclaw serviceToken used by the migrated Super XiaoAI
    // desktop client. The passToken from the auth response is long-lived.
    let session = mint_miclaw_service_token(&client, session).await?;

    {
        let mut guard = state.write();
        guard.session = session.clone();
        guard.flow = LoginFlowContext::default();
        guard.save(storage)?;
        // After a clean success, drop the transient transport so the next
        // login attempt starts fresh.
        guard.reset_transport();
    }

    Ok(LoginOutcome::Authenticated { nick: session.nick })
}

pub fn md5_upper(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let bytes = hasher.finalize();
    hex::encode_upper(bytes)
}

pub(crate) fn parse_session_fields(body: &Value) -> Session {
    let pluck_str = |k: &str| body.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    // userId comes back as a JSON number in some serviceLogin flows
    // returns `"userId": 1125349315`) and as a string in others, so accept
    // either form.
    let pluck_str_or_num = |k: &str| {
        body.get(k).and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    let mut nick = pluck_str("nick");
    if nick.as_deref().map(str::is_empty).unwrap_or(true) {
        nick = pluck_str("nickName");
    }
    Session {
        user_id: pluck_str_or_num("userId"),
        c_user_id: pluck_str("cUserId"),
        pass_token: pluck_str("passToken"),
        ssecurity: pluck_str("ssecurity"),
        nick,
        ..Default::default()
    }
}

/// Following `location` returns a 200 with `Set-Cookie: serviceToken=...`.
/// The serviceToken set-cookie can land on any hop in the redirect chain
/// (Xiaomi bounces through up to 3 different domains), so we disable
/// auto-redirects on this single GET and walk the chain manually,
/// inspecting Set-Cookie headers at every hop.
pub(crate) async fn finalize_with_location(
    client: &Client,
    location: String,
    mut session: Session,
) -> Result<Session> {
    let url = if location.contains("_userIdNeedEncrypt") {
        location
    } else if location.contains('?') {
        format!("{location}&_userIdNeedEncrypt=true")
    } else {
        format!("{location}?_userIdNeedEncrypt=true")
    };

    let token = follow_chain_for_service_token(client, &url, None).await?;
    session.service_token = Some(token);
    session.refreshed_at = Some(chrono::Utc::now().timestamp_millis());
    Ok(session)
}

/// Mint the `sid=miclaw` serviceToken used by Xiaomi HyperConnect / Super
/// XiaoAI. Replay `serviceLogin` with the passToken obtained during login,
/// then walk the redirect chain to capture the scoped serviceToken.
///
/// The official Electron build uses a strict two-phase flow:
///   Phase 1: GET pass/serviceLogin?sid=miclaw WITH the standard cookies
///            → parse loc / nonce / ssecurity
///   Phase 2: GET <loc>&clientSign=<sig> WITHOUT any cookies
///            where sig = url_encode(base64(sha1("nonce=N&ssecurity")))
///            → response carries Set-Cookie: serviceToken=...
///
/// nonce MUST be extracted from the RAW JSON response (not from parsed
/// JSON), otherwise JSON.parse / serde-json would silently truncate the
/// large integer to f64 precision.
pub(crate) async fn mint_miclaw_service_token(
    _client: &Client,
    mut session: Session,
) -> Result<Session> {
    let pass_token = session
        .pass_token
        .as_ref()
        .ok_or_else(|| BridgeError::Login("missing passToken before miclaw token mint".into()))?
        .clone();
    tracing::debug!(
        target = "auth",
        "miclaw token mint: passToken len={} hasV1Prefix={}",
        pass_token.len(),
        pass_token.starts_with("V1:")
    );

    // Build a fresh client with NO cookie jar so the Cookie header we set
    // manually is the only one sent. (When a jar is attached, reqwest
    // appends jar cookies after our explicit ones and the duplicate
    // passToken from a different domain causes 70016.)
    let bare = reqwest::Client::builder()
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(BridgeError::from)?;

    // Build the 7-cookie header that the macOS client sends.
    let device_id = stable_device_id(session.user_id.as_deref());
    let u_dev_id = stable_udev_id_for(session.user_id.as_deref().unwrap_or(""), &device_id);
    let mut cookie = format!("passToken={pass_token}");
    if let Some(uid) = &session.user_id {
        cookie.push_str(&format!("; userId={uid}"));
    }
    if let Some(c) = &session.c_user_id {
        cookie.push_str(&format!("; cUserId={c}"));
    }
    cookie.push_str(&format!("; deviceId={device_id}"));
    cookie.push_str(&format!("; uDevId={u_dev_id}"));
    cookie.push_str("; uLocale=zh_CN");
    cookie.push_str("; pass_ua=pc");

    // ---------- Phase 1 ----------
    let phase1_url = format!(
        "https://account.xiaomi.com/pass/serviceLogin?_locale=zh_CN&_snsNone=true&sid={}&_json=true",
        super::MICLAW_SID
    );
    tracing::debug!(
        target = "auth",
        "miclaw phase1 GET {phase1_url} (cookie_keys={})",
        cookie
            .split(';')
            .map(|s| s.split('=').next().unwrap_or("").trim())
            .collect::<Vec<_>>()
            .join(",")
    );
    let resp = bare
        .get(&phase1_url)
        .header(reqwest::header::USER_AGENT, super::PC_USER_AGENT)
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(BridgeError::Login(format!(
            "miclaw phase1 status {}",
            resp.status()
        )));
    }
    let raw_text = resp.text().await?;
    let body: Value = serde_json::from_str(strip_prefix(&raw_text)).map_err(|e| {
        let preview: String = raw_text.chars().take(400).collect();
        BridgeError::Login(format!(
            "miclaw phase1 parse: {e} (body[..400]={preview:?})"
        ))
    })?;
    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let desc = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Err(BridgeError::Login(format!(
            "miclaw phase1 code={code} desc={desc}"
        )));
    }
    let location = body
        .get("location")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::Login("miclaw phase1 missing location".into()))?
        .to_string();
    let ssecurity = body
        .get("ssecurity")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Extract nonce from the RAW response (JSON.parse loses precision on
    // the very large integer Xiaomi uses for nonce).
    let nonce = extract_raw_number(strip_prefix(&raw_text), "nonce")
        .ok_or_else(|| BridgeError::Login("miclaw phase1 missing nonce".into()))?;
    tracing::debug!(
        target = "auth",
        "miclaw phase1 ok: hasLocation=true ssecurityLen={} nonce={}",
        ssecurity.len(),
        nonce
    );

    // Refresh field set from this response — userId/ssecurity/etc. land here.
    let miclaw_seed = parse_session_fields(&body);
    if let Some(uid) = miclaw_seed.user_id {
        session.user_id = Some(uid);
    }
    if let Some(cid) = miclaw_seed.c_user_id {
        session.c_user_id = Some(cid);
    }
    if !ssecurity.is_empty() {
        session.ssecurity = Some(ssecurity.clone());
    }

    // ---------- Phase 2 ----------
    // sig = url_encode(base64(sha1("nonce=N&ssecurity")))
    let signature_input = if ssecurity.trim().is_empty() {
        format!("nonce={nonce}")
    } else {
        format!("nonce={nonce}&{ssecurity}")
    };
    use base64::Engine;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(signature_input.as_bytes());
    let digest = hasher.finalize();
    let signature_b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    let signature_url = url_form_encode(&signature_b64);

    let sep = if location.contains('?') { '&' } else { '?' };
    let phase2_url = format!("{location}{sep}clientSign={signature_url}");
    tracing::debug!(
        target = "auth",
        "miclaw phase2 GET (clientSign computed; len={})",
        signature_b64.len()
    );

    // Phase 2: NO Cookie header at all.
    let resp = bare
        .get(&phase2_url)
        .header(reqwest::header::USER_AGENT, super::PC_USER_AGENT)
        .send()
        .await?;
    let phase2_status = resp.status();
    let mut found_token: Option<String> = None;
    for hv in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        let raw = match hv.to_str() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let head = raw.split(';').next().unwrap_or("").trim();
        if let Some(rest) = head.strip_prefix("serviceToken=") {
            if !rest.is_empty() && rest != "EXPIRED" {
                found_token = Some(rest.to_string());
                break;
            }
        }
    }
    if found_token.is_none() {
        // Some hops respond with a 302 to a final URL that sets the cookie;
        // if that's the case follow exactly one redirect (no cookies).
        if phase2_status.is_redirection() {
            if let Some(loc) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| absolutize(&phase2_url, s))
            {
                let resp2 = bare
                    .get(&loc)
                    .header(reqwest::header::USER_AGENT, super::PC_USER_AGENT)
                    .send()
                    .await?;
                for hv in resp2.headers().get_all(reqwest::header::SET_COOKIE) {
                    let raw = match hv.to_str() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let head = raw.split(';').next().unwrap_or("").trim();
                    if let Some(rest) = head.strip_prefix("serviceToken=") {
                        if !rest.is_empty() && rest != "EXPIRED" {
                            found_token = Some(rest.to_string());
                            break;
                        }
                    }
                }
            }
        }
    }
    let token = found_token.ok_or_else(|| {
        BridgeError::Login(format!(
            "miclaw phase2 returned no serviceToken (status={phase2_status})"
        ))
    })?;
    tracing::debug!(
        target = "auth",
        "miclaw serviceToken acquired, len={}",
        token.len()
    );
    session.service_token = Some(token);
    session.refreshed_at = Some(chrono::Utc::now().timestamp_millis());
    Ok(session)
}

/// Locate `"<key>": <number>` in a raw JSON string and return the number's
/// textual form. Used for `nonce` whose magnitude breaks f64 precision.
fn extract_raw_number(json_str: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let pos = json_str.find(&pat)?;
    let after = &json_str[pos + pat.len()..];
    let colon = after.find(':')?;
    let tail = &after[colon + 1..];
    let trimmed = tail.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut end = 0;
    if bytes[0] == b'-' {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 || (end == 1 && bytes[0] == b'-') {
        return None;
    }
    Some(trimmed[..end].to_string())
}

/// URL-form-encode (application/x-www-form-urlencoded). Replaces every
/// non-unreserved char with %XX. Used for `clientSign=...`.
fn url_form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

/// `uDevId = sha1(userId + deviceId).toString('base64')` per the Electron
/// `service-token-manager.js`. Returns the standard-base64 form.
pub(crate) fn stable_udev_id_for(user_id: &str, device_id: &str) -> String {
    use base64::Engine;
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(user_id.as_bytes());
    hasher.update(device_id.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

async fn follow_chain_for_service_token(
    client: &Client,
    start_url: &str,
    force_ua: Option<&'static str>,
) -> Result<String> {
    use reqwest::header::{LOCATION, SET_COOKIE, USER_AGENT};
    use reqwest::redirect::Policy;

    let mut current = start_url.to_string();
    let mut found: Option<String> = None;
    let mut cookies_forwarded: Vec<String> = Vec::new();

    // STS for the macOS miclaw build is hosted at
    // `api.miclaw.xiaomi.net/mi-sso-api/sts` and is sensitive to UA — it
    // returns 401 unless the caller looks like the real PC client. Mirror
    // the macOS build's UA there. (For the Dalvik OAuth UA see the parent
    // module.)
    let bare = reqwest::Client::builder()
        .redirect(Policy::none())
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(BridgeError::from)?;

    // Copy cookies from the long-lived client by peeking at what it would
    // attach to the start URL. This brings along passToken/userId/etc.
    if let Ok(req) = client.get(&current).build() {
        if let Some(hv) = req.headers().get(reqwest::header::COOKIE) {
            if let Ok(s) = hv.to_str() {
                cookies_forwarded.push(s.to_string());
            }
        }
    }

    let mut last_body_preview: Option<String> = None;

    for hop in 0..8 {
        // Pick a UA per host: PC `node` for miclaw / mi.com STS endpoints,
        // Dalvik (the OAuth default) for account.xiaomi.com. The miclaw STS
        // endpoint specifically rejects Dalvik with 401. Callers that know
        // the whole chain is PC-only can force a UA via `force_ua`.
        let ua: &str = match force_ua {
            Some(u) => u,
            None if current.contains("miclaw.xiaomi.net")
                || current.contains("home.mi.com")
                || current.contains("hyperos.xiaomi.com") =>
            {
                "node"
            }
            None => crate::auth::DEFAULT_USER_AGENT,
        };
        // The hyperos sts endpoint authenticates exclusively via the
        // `auth=...` query string and rejects requests that also carry
        // session cookies (it sees them as a stale/forged session). HAR
        // confirms macOS miclaw sends NO Cookie and NO Referer header on
        // this hop — only User-Agent, Host, Connection.
        let suppress_cookies = current.contains("imapi.hyperos.xiaomi.com");
        let mut req = bare.get(&current).header(USER_AGENT, ua);
        if !suppress_cookies && !cookies_forwarded.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookies_forwarded.join("; "));
        }
        let resp = req.send().await?;
        let status = resp.status();
        // Scan Set-Cookie headers at this hop.
        let set_cookie_iter = resp.headers().get_all(SET_COOKIE);
        let mut new_cookies: Vec<String> = Vec::new();
        let mut cookie_names: Vec<String> = Vec::new();
        for hv in set_cookie_iter {
            let raw = match hv.to_str() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let head = raw.split(';').next().unwrap_or("").trim();
            if head.is_empty() {
                continue;
            }
            let name = head.split('=').next().unwrap_or("").to_string();
            if !name.is_empty() {
                cookie_names.push(name);
            }
            new_cookies.push(head.to_string());
            if let Some(rest) = head.strip_prefix("serviceToken=") {
                if !rest.is_empty() {
                    found = Some(rest.to_string());
                }
            }
        }
        let next_loc = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        tracing::debug!(
            target = "auth",
            "hop#{hop} status={status} ua={ua} url={current} setCookie={cookie_names:?} location={:?}",
            next_loc
        );

        if !status.is_success() && !status.is_redirection() {
            // Capture body for diagnostics.
            if let Ok(body) = resp.text().await {
                let preview: String = body.chars().take(400).collect();
                tracing::debug!(target = "auth", "hop#{hop} body[..400]={preview:?}");
                last_body_preview = Some(preview);
            }
            break;
        }

        if !new_cookies.is_empty() {
            for nc in &new_cookies {
                let name = nc.split('=').next().unwrap_or("");
                cookies_forwarded.retain(|c| {
                    c.split(';')
                        .next()
                        .map(|s| s.trim())
                        .and_then(|s| s.split('=').next())
                        .map(|n| n != name)
                        .unwrap_or(true)
                });
            }
            cookies_forwarded.extend(new_cookies);
        }
        if found.is_some() {
            tracing::debug!(target = "auth", "serviceToken captured at {current}");
        }
        if status.is_redirection() {
            match next_loc {
                Some(loc) => {
                    let abs = absolutize(&current, &loc);
                    current = abs;
                    continue;
                }
                None => break,
            }
        }
        break;
    }

    found.ok_or_else(|| {
        let suffix = last_body_preview
            .map(|s| format!(" (last body: {s})"))
            .unwrap_or_default();
        BridgeError::Login(format!("serviceToken missing after redirect{suffix}"))
    })
}

fn absolutize(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    if let Ok(u) = url::Url::parse(base) {
        if let Ok(j) = u.join(location) {
            return j.to_string();
        }
    }
    location.to_string()
}

/// Derive a stable `deviceId` cookie value of the form `pc_<32 hex>`. We
/// match macOS miclaw's algorithm exactly (extracted from the Electron
/// build's `deviceid.js`):
///   sn = node-machine-id (= IOPlatformUUID on macOS, machine-id on Linux,
///        BIOS SerialNumber on Windows)
///   deviceId = "pc_" + md5_hex(sn)
/// The result is cached for the lifetime of the process.
pub(crate) fn stable_device_id(_user_id: Option<&str>) -> String {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let slot = CACHE.get_or_init(|| Mutex::new(None));
    if let Some(v) = slot.lock().clone() {
        return v;
    }
    let sn = read_machine_id().unwrap_or_else(|| "anon".to_string());
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(sn.as_bytes());
    let hex = hex::encode(hasher.finalize());
    let id = format!("pc_{hex}");
    *slot.lock() = Some(id.clone());
    id
}

/// Read the OS-level "machine id" the same way `node-machine-id` does:
/// IOPlatformUUID on macOS, `/etc/machine-id` on Linux, BIOS SerialNumber
/// on Windows. The value is lowercased to match `node-machine-id`'s
/// behaviour, which is what miclaw's deviceId formula assumes.
fn read_machine_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let out = Command::new("ioreg")
            .args(["-d2", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("\"IOPlatformUUID\" = \"") {
                if let Some(end) = rest.find('"') {
                    return Some(rest[..end].to_lowercase());
                }
            }
        }
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/etc/machine-id") {
            let s = s.trim().to_lowercase();
            if !s.is_empty() {
                return Some(s);
            }
        }
        if let Ok(s) = std::fs::read_to_string("/var/lib/dbus/machine-id") {
            let s = s.trim().to_lowercase();
            if !s.is_empty() {
                return Some(s);
            }
        }
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let out = Command::new("wmic")
            .args(["bios", "get", "serialnumber"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let s = line.trim();
            if !s.is_empty() && !s.eq_ignore_ascii_case("serialnumber") {
                return Some(s.to_lowercase());
            }
        }
        return None;
    }
    #[allow(unreachable_code)]
    None
}

/// Same idea for `uDevId`. We follow miclaw's algorithm extracted from
/// `service-token-manager.js`: `base64(sha1(userId + deviceId))`. Use
/// [`stable_udev_id_for`] when both pieces are known; this convenience
/// wrapper falls back when only the userId is at hand.
#[allow(dead_code)]
pub(crate) fn stable_udev_id(user_id: Option<&str>) -> String {
    let device_id = stable_device_id(user_id);
    stable_udev_id_for(user_id.unwrap_or(""), &device_id)
}
