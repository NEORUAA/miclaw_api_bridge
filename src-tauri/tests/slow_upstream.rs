//! End-to-end regression test for the long-generation timeout fix.
//!
//! Spins up a mock mimo upstream that sleeps `MOCK_DELAY_SECS` (default 35 —
//! deliberately longer than the old 30s blanket client timeout) before
//! streaming an SSE completion, then drives the real axum router through
//! `/v1/chat/completions` with `stream: false`.
//!
//! Asserts:
//!   * the bridge asks the UPSTREAM for a stream even when the client didn't
//!     (fast time-to-first-byte; the official PC client always streams),
//!   * a >30s generation completes with 200 instead of dying at 30000ms,
//!   * the folded non-stream JSON carries the aggregated content and usage.
//!
//! Runs ignored by default (it takes ~40s):
//!   cargo test --test slow_upstream -- --ignored --nocapture

use axum::{extract::State, response::Response, routing::post, Router};
use miclaw_api_bridge_lib::server;
use miclaw_api_bridge_lib::state::BridgeState;
use miclaw_api_bridge_lib::storage::Storage;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// (stream flag, stream_options) as seen by the upstream.
type SeenRequests = Arc<Mutex<Vec<(Option<bool>, Option<Value>)>>>;

#[derive(Clone, Default)]
struct MockState {
    seen: SeenRequests,
}

async fn mock_chat(State(st): State<MockState>, body: String) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    st.seen.lock().unwrap().push((
        v.get("stream").and_then(|s| s.as_bool()),
        v.get("stream_options").cloned(),
    ));

    let delay: u64 = std::env::var("MOCK_DELAY_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(35);
    tokio::time::sleep(Duration::from_secs(delay)).await;

    let chunks = [
        json!({"choices":[{"delta":{"role":"assistant","content":""},"finish_reason":null,"index":0}]}),
        json!({"choices":[{"delta":{"reasoning_content":"thinking..."},"finish_reason":null,"index":0}]}),
        json!({"choices":[{"delta":{"content":"Hello"},"finish_reason":null,"index":0}]}),
        json!({"choices":[{"delta":{"content":" world"},"finish_reason":null,"index":0}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop","index":0}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}),
    ];
    let mut sse = String::new();
    for c in chunks {
        sse.push_str(&format!("data: {c}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(sse))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "takes ~40s by design (mock upstream sleeps 35s)"]
async fn slow_nonstream_generation_survives_past_old_timeout() {
    // Mock upstream on an ephemeral port.
    let mock_state = MockState::default();
    let mock_app = Router::new()
        .route("/osbot/pc/llm/v1/chat/completions", post(mock_chat))
        .with_state(mock_state.clone());
    let mock_listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    mock_listener.set_nonblocking(true).unwrap();
    tokio::spawn(async move {
        axum_server::from_tcp(mock_listener)
            .serve(mock_app.into_make_service())
            .await
            .unwrap();
    });

    // Bridge state rooted in a temp dir, with a fake authenticated session.
    std::env::set_var("MICLAW_API_BRIDGE_DISABLE_KEYRING", "1");
    std::env::set_var("MIMO_HOST_OVERRIDE", format!("http://{mock_addr}"));
    let dir = std::env::temp_dir().join(format!("mb-slow-{}", std::process::id()));
    let storage = Storage::for_paths(dir.join("c"), dir.join("d")).unwrap();
    storage
        .save_blob("session", &json!({
            "user_id": "123",
            "c_user_id": "fake-cuid",
            "service_token": "fake-service-token",
        }))
        .unwrap();
    let state = BridgeState::with_storage(storage).unwrap();
    let http = server::start_http(
        state,
        server::ServerConfig {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
        },
    )
    .await
    .unwrap();
    let base = format!("http://{}", http.addr);

    // Non-streaming client request against a >30s upstream generation.
    let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().unwrap();
    let started = Instant::now();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "xiaomi/mimo", "stream": false, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(resp.status(), 200, "slow generation must not 502");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v.pointer("/choices/0/message/content").and_then(|c| c.as_str()),
        Some("Hello world")
    );
    assert_eq!(
        v.pointer("/usage/total_tokens").and_then(|t| t.as_i64()),
        Some(18)
    );
    assert!(
        elapsed >= Duration::from_secs(35),
        "the response must wait for the full generation, not die at 30s"
    );

    // The upstream must have been asked for a stream, with usage included.
    let seen = mock_state.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, Some(true), "bridge must force stream upstream");
    assert_eq!(
        seen[0].1.as_ref().and_then(|so| so.pointer("/include_usage")).and_then(|v| v.as_bool()),
        Some(true)
    );

    http.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
