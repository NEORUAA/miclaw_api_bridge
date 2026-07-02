use super::ProxyController;
use crate::error::BridgeError;
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn map_err(e: BridgeError) -> Response {
    let (code, kind) = match &e {
        BridgeError::NotAuthenticated => (StatusCode::UNAUTHORIZED, "not_authenticated"),
        BridgeError::Login(_) => (StatusCode::UNAUTHORIZED, "login_failed"),
        _ => (StatusCode::BAD_GATEWAY, "upstream_error"),
    };
    let body = Json(json!({
        "error": {
            "type": kind,
            "message": e.to_string(),
        }
    }));
    (code, body).into_response()
}

pub async fn list_models(_ctrl: Arc<ProxyController>) -> Response {
    let data: Vec<Value> = crate::mimo::known_models()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": m.object,
                "owned_by": m.owned_by,
                "created": chrono::Utc::now().timestamp(),
            })
        })
        .collect();
    Json(json!({
        "object": "list",
        "data": data,
    }))
    .into_response()
}

/// Emit a structured log entry to the front-end's `proxy-log` event
/// channel. Safe to call from any handler.
pub fn emit_log(ctrl: &ProxyController, payload: Value) {
    ctrl.emitter.emit(payload);
}

/// Build a `request` log entry. When verbose logging is on, the full request
/// body (including the prompt) is attached so the WebUI can expand it.
pub fn request_log(ctrl: &ProxyController, path: &str, body: &Value) -> Value {
    let mut entry = json!({
        "ts": chrono::Utc::now().timestamp_millis(),
        "kind": "request",
        "path": path,
        "model": body.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        "stream": body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
    });
    if ctrl.verbose() {
        entry["body"] = body.clone();
    }
    entry
}

/// Forward a JSON request to mimo, streaming the upstream bytes back.
#[allow(dead_code)]
pub async fn forward(ctrl: Arc<ProxyController>, upstream_path: &str, body: Value) -> Response {
    let stream_requested = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let started = std::time::Instant::now();
    tracing::debug!(
        target = "proxy",
        "→ mimo {upstream_path} stream={stream_requested} model={model}"
    );
    emit_log(&ctrl, request_log(&ctrl, upstream_path, &body));
    match ctrl.mimo.post_json(upstream_path, body).await {
        Ok(upstream) => {
            let status = upstream.status();
            tracing::debug!(target = "proxy", "← mimo {upstream_path} status={status}");
            if ctrl.verbose() {
                return buffered_response_with_log(
                    &ctrl,
                    upstream_path,
                    &model,
                    status,
                    started,
                    upstream,
                )
                .await;
            }
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": upstream_path,
                    "status": status.as_u16(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            proxy_response_tapped(ctrl.clone(), model.clone(), upstream).await
        }
        Err(e) => {
            tracing::warn!(target = "proxy", "mimo {upstream_path} error: {e}");
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "error",
                    "path": upstream_path,
                    "message": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            map_err(e)
        }
    }
}

/// Buffer the full upstream response so its body (the model's reply) can be
/// attached to a `response` log entry, then return it to the caller. Used
/// only in verbose mode; trades streaming for inspectability.
#[allow(dead_code)]
async fn buffered_response_with_log(
    ctrl: &ProxyController,
    path: &str,
    model: &str,
    status: reqwest::StatusCode,
    started: std::time::Instant,
    upstream: reqwest::Response,
) -> Response {
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("event-stream"))
        .unwrap_or(false);
    let bytes = upstream.bytes().await.unwrap_or_default();
    // Record token usage from the buffered body (handles JSON and SSE).
    let mut scanner = crate::usage::UsageScanner::new(is_sse);
    scanner.feed(&bytes);
    if let Some((p, c, t)) = scanner.finish() {
        ctrl.usage.record(model, p, c, t);
    }
    let text = String::from_utf8_lossy(&bytes).to_string();
    let body_val = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    emit_log(
        ctrl,
        json!({
            "ts": chrono::Utc::now().timestamp_millis(),
            "kind": "response",
            "path": path,
            "status": status.as_u16(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "body": body_val,
        }),
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        content_type.unwrap_or_else(|| header::HeaderValue::from_static("application/json")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    *resp.headers_mut() = headers;
    resp
}

pub async fn proxy_response(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    if let Some(ct) = upstream.headers().get(header::CONTENT_TYPE) {
        headers.insert(header::CONTENT_TYPE, ct.clone());
    } else {
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
    }
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    let stream = upstream.bytes_stream();
    let body = Body::from_stream(stream);
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

/// Like `proxy_response`, but taps the streamed body to extract the final
/// `usage` token counts and records them against `model` when the stream ends.
/// Bytes are forwarded to the client unchanged and without extra buffering.
pub async fn proxy_response_tapped(
    ctrl: Arc<ProxyController>,
    model: String,
    upstream: reqwest::Response,
) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("event-stream"))
        .unwrap_or(false);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        content_type.unwrap_or_else(|| header::HeaderValue::from_static("application/json")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );

    let scanner = crate::usage::UsageScanner::new(is_sse);
    let upstream_stream = upstream.bytes_stream();
    let body_stream = futures_util::stream::unfold(
        (upstream_stream, Some(scanner), ctrl, model),
        |(mut stream, mut scanner, ctrl, model)| async move {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    if let Some(s) = scanner.as_mut() {
                        s.feed(&bytes);
                    }
                    Some((
                        Ok::<_, std::io::Error>(bytes),
                        (stream, scanner, ctrl, model),
                    ))
                }
                Some(Err(e)) => {
                    if let Some(s) = scanner.take() {
                        if let Some((p, c, t)) = s.finish() {
                            ctrl.usage.record(&model, p, c, t);
                        }
                    }
                    Some((
                        Err(std::io::Error::other(e)),
                        (stream, scanner, ctrl, model),
                    ))
                }
                None => {
                    if let Some(s) = scanner.take() {
                        if let Some((p, c, t)) = s.finish() {
                            ctrl.usage.record(&model, p, c, t);
                        }
                    }
                    None
                }
            }
        },
    );

    let mut resp = Response::new(Body::from_stream(body_stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}
