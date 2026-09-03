use super::login::parse_session_fields;
use super::{strip_prefix, AuthState, LoginFlowContext, ACCOUNT_HOST, SERVICE_LOGIN_AUTH2_URL};
use crate::error::{BridgeError, Result};
use crate::storage::Storage;
use parking_lot::RwLock;
use serde_json::Value;
use std::sync::Arc;

/// 2FA flag → relative path on `account.xiaomi.com`.
fn paths_for(flag: i32) -> (&'static str, &'static str) {
    match flag {
        4 => (
            "/identity/auth/sendPhoneTicket",
            "/identity/auth/verifyPhone",
        ),
        // 8 = email; the rest fall back to email which is the most common.
        _ => (
            "/identity/auth/sendEmailTicket",
            "/identity/auth/verifyEmail",
        ),
    }
}

/// Send a 2FA ticket (SMS or email). Critical: this MUST run through the
/// same HTTP transport that `login()` already opened — otherwise the
/// server-issued `identity_session` cookie isn't carried and the SMS gateway
/// silently drops the request even though the API returns 200.
///
/// On server-side rejection (e.g. `code=70022` "验证码发送过多") we surface
/// the server's tips/desc as a `BridgeError::Login` so the UI can show it.
pub async fn send_ticket(state: &Arc<RwLock<AuthState>>, flag: i32) -> Result<bool> {
    let (transport, _id_session_hint) = {
        let guard = state.read();
        let id = guard.flow.identity_session.clone();
        (guard.ensure_transport()?, id)
    };
    let (send_path, _) = paths_for(flag);
    let url = format!("{ACCOUNT_HOST}{send_path}");
    let dc = chrono::Utc::now().timestamp_millis();
    let resp = transport
        .client
        .post(&url)
        .query(&[("_dc", dc.to_string())])
        .form(&[("_json", "true"), ("retry", "0"), ("icode", "")])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    tracing::debug!(
        target = "auth",
        "sendTicket flag={flag} status={status} body={text:?}"
    );
    if !status.is_success() {
        return Err(BridgeError::Login(format!("sendTicket http {status}")));
    }
    let body: Value = match serde_json::from_str(strip_prefix(&text)) {
        Ok(v) => v,
        Err(_) => return Err(BridgeError::Login("sendTicket: invalid response".into())),
    };
    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 {
        return Ok(true);
    }
    // Surface a friendly message. Xiaomi's payload looks like:
    //   {"code":70022,"tips":"验证码发送过多，请明天再试","desc":"发送验证码失败"}
    let pluck = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    let tips = pluck("tips");
    let desc = pluck("desc").or_else(|| pluck("description"));
    let msg = match (tips, desc) {
        (Some(t), Some(d)) if t != d => format!("{d}: {t}"),
        (Some(t), _) => t,
        (_, Some(d)) => d,
        _ => format!("发送验证码失败 (code={code})"),
    };
    Err(BridgeError::Login(format!("{msg} (code={code})")))
}

pub async fn verify_ticket(
    state: &Arc<RwLock<AuthState>>,
    storage: &Arc<Storage>,
    flag: i32,
    ticket: String,
) -> Result<()> {
    let (transport, account, password_hash) = {
        let guard = state.read();
        let acc =
            guard.flow.pending_account.clone().ok_or_else(|| {
                BridgeError::Login("no in-flight login (call login first)".into())
            })?;
        let hash = guard
            .flow
            .pending_password_hash
            .clone()
            .ok_or_else(|| BridgeError::Login("no in-flight password hash".into()))?;
        (guard.ensure_transport()?, acc, hash)
    };
    let (_, verify_path) = paths_for(flag);
    let url = format!("{ACCOUNT_HOST}{verify_path}");
    let dc = chrono::Utc::now().timestamp_millis();
    let resp = transport
        .client
        .post(&url)
        .query(&[("_dc", dc.to_string())])
        .form(&[
            ("_flag", flag.to_string()),
            ("ticket", ticket.clone()),
            ("trust", "true".into()),
            ("_json", "true".into()),
        ])
        .send()
        .await?;
    let status = resp.status();
    let resp_text = resp.text().await?;
    let body: Value = serde_json::from_str(strip_prefix(&resp_text)).map_err(|e| {
        let preview: String = resp_text.chars().take(400).collect();
        BridgeError::Login(format!(
            "verifyTicket parse: {e} (status={status}, body[..400]={preview:?})"
        ))
    })?;
    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 70014 || code == 7014 {
        return Err(BridgeError::VerificationCodeError);
    }
    if code != 0 {
        let desc = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Err(BridgeError::Login(format!(
            "verify code={code} desc={desc}"
        )));
    }
    // Verify response carries a `location` URL pointing at the trusted
    // landing page. We fetch it once so any cookies it sets land in the
    // jar — but DO NOT try to parse it as JSON, it's HTML (mi.com store).
    if let Some(location) = body.get("location").and_then(|v| v.as_str()) {
        let _ = transport.client.get(location).send().await; // best-effort
    }

    // Now replay serviceLoginAuth2 with the same cookie jar. The
    // identity_session is now flagged as "trusted" server-side, so the
    // server returns the full credential set in one shot.
    let auth2_form: Vec<(&str, String)> = vec![
        ("user", account),
        ("hash", password_hash),
        ("sid", super::sid()),
        ("_json", "true".into()),
        ("_locale", "zh_CN".into()),
    ];
    let auth2_resp = transport
        .client
        .post(SERVICE_LOGIN_AUTH2_URL)
        .form(&auth2_form)
        .send()
        .await?
        .text()
        .await?;
    let auth2_json: Value = serde_json::from_str(strip_prefix(&auth2_resp)).map_err(|e| {
        let preview: String = auth2_resp.chars().take(400).collect();
        BridgeError::Login(format!(
            "post-2fa auth2 parse: {e} (body[..400]={preview:?})"
        ))
    })?;
    tracing::debug!(
        target = "auth",
        "post-2fa auth2 raw keys: {:?}",
        auth2_json
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
    );
    let auth2_code = auth2_json
        .get("code")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    if auth2_code != 0 {
        let desc = auth2_json
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Err(BridgeError::Login(format!(
            "post-2fa auth2 code={auth2_code} desc={desc}"
        )));
    }
    let session_seed = parse_session_fields(&auth2_json);
    tracing::debug!(
        target = "auth",
        "post-2fa auth2 fields: userId={:?} cUserId={:?} hasPassToken={} hasSsecurity={} nick={:?}",
        session_seed.user_id,
        session_seed.c_user_id,
        session_seed.pass_token.is_some(),
        session_seed.ssecurity.is_some(),
        session_seed.nick,
    );
    // Re-issue serviceLogin with the passToken so the final token exactly
    // matches the migrated desktop client's sid=miclaw business session.
    let session = super::login::mint_miclaw_service_token(&transport.client, session_seed).await?;
    tracing::debug!(
        target = "auth",
        "final session: hasUserId={} hasCUserId={} hasPassToken={} hasServiceToken={} authenticated={}",
        session.user_id.is_some(),
        session.c_user_id.is_some(),
        session.pass_token.is_some(),
        session.service_token.is_some(),
        session.is_authenticated(),
    );

    {
        let mut guard = state.write();
        guard.session = session;
        guard.flow = LoginFlowContext::default();
        guard.save(storage)?;
        guard.reset_transport();
    }
    Ok(())
}

pub fn extract_query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let q = url.split_once('?').map(|x| x.1)?;
    for kv in q.split('&') {
        let (k, v) = kv.split_once('=')?;
        if k == key {
            return Some(v);
        }
    }
    None
}
