use crate::auth;
use crate::auth::login::{LoginOutcome, LoginRequest};
use crate::error::{BridgeError, Result};
use crate::mimo::{known_models, AuthSnapshot, ModelInfo, QuotaSnapshot};
use crate::proxy::ProxySnapshot;
use crate::state::BridgeState;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize, Serialize)]
pub struct SendTicketRequest {
    pub flag: i32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct VerifyTicketRequest {
    pub flag: i32,
    pub ticket: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SetPortRequest {
    pub port: u16,
}

pub async fn auth_status(state: &Arc<BridgeState>) -> Result<AuthSnapshot> {
    Ok(state.mimo.quick_status())
}

pub async fn quota(state: &Arc<BridgeState>) -> Result<QuotaSnapshot> {
    state.mimo.quota().await
}

pub async fn login(state: &Arc<BridgeState>, req: LoginRequest) -> Result<LoginOutcome> {
    auth::login(&state.auth, &state.storage, req).await
}

pub async fn send_two_factor_ticket(state: &Arc<BridgeState>, flag: i32) -> Result<bool> {
    auth::send_ticket(&state.auth, flag).await
}

pub async fn verify_two_factor(state: &Arc<BridgeState>, flag: i32, ticket: String) -> Result<()> {
    auth::verify_ticket(&state.auth, &state.storage, flag, ticket).await
}

pub async fn refresh_session(state: &Arc<BridgeState>) -> Result<AuthSnapshot> {
    auth::refresh_session(&state.auth, &state.storage).await?;
    Ok(state.mimo.quick_status())
}

pub async fn logout(state: &Arc<BridgeState>) -> Result<()> {
    {
        let mut guard = state.auth.write();
        guard.session = Default::default();
        guard.flow = Default::default();
        guard.reset_transport();
    }
    crate::auth::AuthState::clear(&state.storage)?;
    Ok(())
}

pub async fn set_proxy_port(state: &Arc<BridgeState>, port: u16) -> Result<ProxySnapshot> {
    if port < 1024 {
        return Err(BridgeError::Proxy("port must be >= 1024".into()));
    }
    state.storage.update_settings(|s| s.proxy_port = port)?;
    Ok(proxy_status(state))
}

pub fn proxy_status(state: &Arc<BridgeState>) -> ProxySnapshot {
    let configured = state.storage.settings().proxy_port;
    let bound = state.bound_addr();
    let active_port = bound.map(|a| a.port());
    ProxySnapshot {
        running: bound.is_some(),
        addr: bound.map(|a| a.to_string()),
        port: configured,
        active_port,
        restart_required: active_port.is_some_and(|p| p != configured),
    }
}

pub fn list_models() -> Vec<ModelInfo> {
    known_models()
}
