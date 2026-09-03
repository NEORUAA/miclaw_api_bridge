//! Manual OAuth + chat smoke test.
//!
//! This test never runs in CI: it only fires when run with `--ignored`
//! and the env var `MIMO_BRIDGE_SMOKE_ACCOUNT` is set. It walks through
//! the real Xiaomi account login flow and (if requested) makes a one-shot
//! chat-completions call against `api.miclaw.xiaomi.net`.
//!
//! Usage (run manually from a terminal):
//!
//! ```bash
//! MIMO_BRIDGE_SMOKE_ACCOUNT=user@example.com \
//! MIMO_BRIDGE_SMOKE_PASSWORD='secret' \
//! MIMO_BRIDGE_SMOKE_CHAT=1                  # optional: also fire a chat
//! cargo test --test smoke_login -- --ignored --nocapture
//!
//! # If 2FA is required, the first run will print a prompt and exit;
//! # rerun with the ticket pinned via env:
//! MIMO_BRIDGE_SMOKE_2FA_FLAG=8 \
//! MIMO_BRIDGE_SMOKE_2FA_TICKET='123456' \
//! MIMO_BRIDGE_SMOKE_ACCOUNT=... MIMO_BRIDGE_SMOKE_PASSWORD=... \
//! cargo test --test smoke_login -- --ignored --nocapture
//! ```

use miclaw_api_bridge_lib::auth::{
    build_http_client, login as do_login,
    login::{LoginOutcome, LoginRequest},
    refresh_session, send_ticket, verify_ticket, AuthState, Session,
};
use miclaw_api_bridge_lib::error::Result;
use miclaw_api_bridge_lib::storage::Storage;
use parking_lot::RwLock;
use std::sync::Arc;

fn must_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn make_storage() -> Arc<Storage> {
    let base = std::env::temp_dir().join(format!(
        "miclaw-api-bridge-smoke-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let cfg = base.join("config");
    let data = base.join("data");
    Storage::for_paths(cfg, data).expect("storage for tests")
}

#[tokio::test]
#[ignore = "real-network smoke test, run manually"]
async fn smoke_login_chat() -> Result<()> {
    let account = match must_env("MIMO_BRIDGE_SMOKE_ACCOUNT") {
        Some(v) => v,
        None => {
            eprintln!("set MIMO_BRIDGE_SMOKE_ACCOUNT to run");
            return Ok(());
        }
    };
    let password =
        must_env("MIMO_BRIDGE_SMOKE_PASSWORD").expect("MIMO_BRIDGE_SMOKE_PASSWORD missing");

    let auth = Arc::new(RwLock::new(AuthState::default()));
    let storage = make_storage();

    let outcome = do_login(
        &auth,
        &storage,
        LoginRequest {
            account,
            password,
            captcha: None,
        },
    )
    .await?;

    match outcome {
        LoginOutcome::Authenticated { nick } => {
            eprintln!("✓ logged in nick={:?}", nick);
        }
        LoginOutcome::TwoFactorRequired { options } => {
            eprintln!("→ 2FA required, options={:?}", options);
            let pinned_flag =
                must_env("MIMO_BRIDGE_SMOKE_2FA_FLAG").and_then(|s| s.parse::<i32>().ok());
            let flag = pinned_flag
                .or_else(|| options.iter().copied().find(|f| *f == 8))
                .or_else(|| options.first().copied())
                .expect("no 2FA option available");
            match must_env("MIMO_BRIDGE_SMOKE_2FA_TICKET") {
                Some(ticket) => {
                    verify_ticket(&auth, &storage, flag, ticket).await?;
                    eprintln!("✓ 2FA verified");
                }
                None => {
                    let sent = send_ticket(&auth, flag).await?;
                    panic!(
                        "2FA ticket sent={sent} via flag={flag}; \
                         rerun with MIMO_BRIDGE_SMOKE_2FA_FLAG={flag} \
                         MIMO_BRIDGE_SMOKE_2FA_TICKET=<code>"
                    );
                }
            }
        }
        LoginOutcome::CaptchaRequired { captcha_url } => {
            panic!("captcha required: {captcha_url}");
        }
        LoginOutcome::Failed { code, description } => {
            panic!("login failed code={code} desc={description}");
        }
    }

    // Smoke a refresh once, just to make sure the persisted session is valid.
    refresh_session(&auth, &storage).await?;
    let snap = auth.read().session.clone();
    assert!(
        snap.is_authenticated(),
        "session must have a serviceToken after login"
    );
    eprintln!(
        "✓ session: userId={:?} cUserId={:?} hasToken={}",
        snap.user_id,
        snap.c_user_id,
        snap.service_token.is_some()
    );

    if must_env("MIMO_BRIDGE_SMOKE_CHAT").is_some() {
        smoke_chat(&snap).await?;
    }
    Ok(())
}

async fn smoke_chat(session: &Session) -> Result<()> {
    let (client, _) = build_http_client(session)?;
    let token = session.service_token.clone().expect("serviceToken");
    let cookie = match &session.c_user_id {
        Some(c) => format!("serviceToken={token}; cUserId={c}"),
        None => format!("serviceToken={token}"),
    };
    let body = serde_json::json!({
        "model": "mimo-omni",
        "stream": false,
        "messages": [{"role": "user", "content": "ping, reply with pong only"}],
    });
    let resp = client
        .post("https://api.miclaw.xiaomi.net/osbot/pc/llm/v2/chat/completions?bizId=xiaoai_pc&featureId=common&isFirstQuery=false")
        .header("user-agent", "node")
        .header("accept", "*/*")
        .header("cookie", cookie)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    eprintln!("--- chat status={status} ---\n{text}");
    Ok(())
}
