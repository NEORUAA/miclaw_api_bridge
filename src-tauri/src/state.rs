use crate::auth::AuthState;
use crate::error::Result;
use crate::mimo::MimoClient;
use crate::storage::Storage;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct BridgeState {
    pub storage: Arc<Storage>,
    pub auth: Arc<RwLock<AuthState>>,
    pub mimo: Arc<MimoClient>,
    pub proxy: Arc<crate::proxy::ProxyController>,
    pub logs: Arc<LogHub>,
    pub security: Arc<crate::security::Security>,
    pub usage: Arc<crate::usage::UsageStore>,
    bound_addr: RwLock<Option<SocketAddr>>,
}

impl BridgeState {
    pub fn new() -> Result<Arc<Self>> {
        let storage = Storage::new()?;
        Self::with_storage(storage)
    }

    pub fn with_storage(storage: Arc<Storage>) -> Result<Arc<Self>> {
        let auth = Arc::new(RwLock::new(AuthState::load(&storage)?));
        let mimo = Arc::new(MimoClient::new(auth.clone())?);
        let logs = Arc::new(LogHub::new(500));
        let emitter = LogEmitter::new(logs.clone());
        let security = crate::security::Security::load(storage.clone())?;
        let usage = crate::usage::UsageStore::load(storage.clone());
        let proxy = Arc::new(crate::proxy::ProxyController::new(
            mimo.clone(),
            emitter,
            usage.clone(),
        ));
        Ok(Arc::new(Self {
            storage,
            auth,
            mimo,
            proxy,
            logs,
            security,
            usage,
            bound_addr: RwLock::new(None),
        }))
    }

    pub fn set_bound_addr(&self, addr: SocketAddr) {
        *self.bound_addr.write() = Some(addr);
    }

    pub fn clear_bound_addr(&self) {
        *self.bound_addr.write() = None;
    }

    pub fn bound_addr(&self) -> Option<SocketAddr> {
        *self.bound_addr.read()
    }
}

/// Hard ceiling on the approximate total payload size kept in the ring.
/// With verbose logging on, entries carry full prompt/completion bodies
/// (hundreds of KB each in the wild), so an entry-count cap alone could
/// still pin hundreds of MB — this bounds the buffer by bytes instead.
const LOG_BYTE_CAP: usize = 32 * 1024 * 1024;

pub struct LogHub {
    /// (approx. serialized size, payload) — newest first.
    rows: Mutex<VecDeque<(usize, Value)>>,
    cap: usize,
    tx: broadcast::Sender<Value>,
}

impl LogHub {
    pub fn new(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            rows: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
            tx,
        }
    }

    pub fn push(&self, payload: Value) {
        // One serialization per entry, at request rate — cheap insurance.
        let size = payload.to_string().len() + 64;
        {
            let mut rows = self.rows.lock();
            let mut total: usize = rows.iter().map(|(s, _)| s).sum();
            while rows.len() >= self.cap || total + size > LOG_BYTE_CAP {
                match rows.pop_back() {
                    Some((s, _)) => total -= s,
                    None => break,
                }
            }
            rows.push_front((size, payload.clone()));
        }
        let _ = self.tx.send(payload);
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.rows.lock().iter().map(|(_, v)| v.clone()).collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }
}

#[derive(Clone)]
pub struct LogEmitter {
    hub: Arc<LogHub>,
}

impl LogEmitter {
    pub fn new(hub: Arc<LogHub>) -> Self {
        Self { hub }
    }

    pub fn emit(&self, payload: Value) {
        self.hub.push(payload);
    }
}
