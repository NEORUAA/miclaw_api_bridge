use super::transport::{
    emit_log, list_models, map_err, proxy_response, proxy_response_tapped,
};
use super::ProxyController;
use crate::decode::Utf8Stream;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;


/// Upper bound for the in-flight SSE reassembly buffer. mimo's chunks are a few
/// KB of JSON each; if the upstream never emits a `\n\n` delimiter the buffer
/// would otherwise grow without limit, so we treat overflow as a stream error.
const MAX_SSE_BUFFER: usize = 8 * 1024 * 1024;


pub async fn chat(State(ctrl): State<Arc<ProxyController>>, Json(body): Json<Value>) -> Response {
    chat_completions(ctrl, body).await
}

/// `/v1/chat/completions`: proxy to mimo, but NORMALIZE the response into a
/// spec-compliant OpenAI Chat Completion(.chunk).
///
/// mimo emits a long run of `delta.reasoning_content` before any
/// `delta.content`; raw passthrough makes gateways that only read `content`
/// (or that key reasoning off `reasoning` rather than `reasoning_content`)
/// look like streaming is broken / the thinking trace is missing. We:
///   * re-emit well-formed chunks (id / object / created / model /
///     `delta.role` on the first / `finish_reason` / final usage / `[DONE]`),
///   * keep reasoning as `reasoning_content` AND mirror it to `reasoning`, so
///     both DeepSeek-style and OpenRouter-style consumers render it.
async fn chat_completions(ctrl: Arc<ProxyController>, body: Value) -> Response {
    let model_req = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // The RESPONSE shape is decided by what the CLIENT asked for, not by what
    // the upstream happens to return (mimo sometimes replies with SSE even for
    // a non-streaming request, and vice-versa).
    let client_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let want_usage = client_stream
        && body
            .pointer("/stream_options/include_usage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    let started = std::time::Instant::now();
    emit_log(
        &ctrl,
        super::transport::request_log(&ctrl, crate::mimo::PATH_CHAT, &body),
    );

    match ctrl.mimo.post_json(crate::mimo::PATH_CHAT, body).await {
        Ok(upstream) => {
            let status = upstream.status();
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": crate::mimo::PATH_CHAT,
                    "status": status.as_u16(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            if !status.is_success() {
                return proxy_response(upstream).await;
            }
            let upstream_sse = upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("event-stream"))
                .unwrap_or(false);
            match (client_stream, upstream_sse) {
                (true, true) => chat_stream_normalized(ctrl, model_req, want_usage, upstream),
                (true, false) => {
                    chat_stream_from_json(ctrl, model_req, want_usage, upstream).await
                }
                (false, true) => chat_aggregate_sse(ctrl, model_req, upstream).await,
                (false, false) => chat_nonstream_normalized(ctrl, model_req, upstream).await,
            }
        }
        Err(e) => {
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "error",
                    "path": crate::mimo::PATH_CHAT,
                    "message": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            map_err(e)
        }
    }
}

fn chat_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

/// Normalize a single streamed `delta`: keep content / tool_calls, add the
/// assistant role on the first chunk, and mirror reasoning to both fields.
/// Returns the delta plus any (content, reasoning) text for aggregation.
fn normalize_chat_delta(delta: &Value, first: bool) -> (Value, Option<String>, Option<String>) {
    let mut out = serde_json::Map::new();
    if first {
        out.insert("role".into(), json!("assistant"));
    }
    let mut content_txt = None;
    if let Some(c) = delta.get("content") {
        if !c.is_null() {
            out.insert("content".into(), c.clone());
            if let Some(s) = c.as_str() {
                content_txt = Some(s.to_string());
            }
        }
    }
    let mut reasoning_txt = None;
    if let Some(r) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
    {
        if !r.is_null() {
            out.insert("reasoning_content".into(), r.clone());
            out.insert("reasoning".into(), r.clone());
            if let Some(s) = r.as_str() {
                reasoning_txt = Some(s.to_string());
            }
        }
    }
    if let Some(tc) = delta.get("tool_calls") {
        out.insert("tool_calls".into(), tc.clone());
    }
    (Value::Object(out), content_txt, reasoning_txt)
}

/// Normalize a non-streamed completion into a spec `chat.completion` object.
fn normalize_chat_completion(v: &Value, model_req: &str) -> Value {
    let message = v
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish = v
        .pointer("/choices/0/finish_reason")
        .cloned()
        .unwrap_or_else(|| json!("stop"));

    let mut out_msg = serde_json::Map::new();
    out_msg.insert("role".into(), json!("assistant"));
    out_msg.insert(
        "content".into(),
        message.get("content").cloned().unwrap_or(Value::Null),
    );
    if let Some(r) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
    {
        if !r.is_null() {
            out_msg.insert("reasoning_content".into(), r.clone());
            out_msg.insert("reasoning".into(), r.clone());
        }
    }
    if let Some(tc) = message.get("tool_calls") {
        out_msg.insert("tool_calls".into(), tc.clone());
    }

    let model_out = v
        .get("model")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if model_req.is_empty() {
                crate::mimo::MODEL_DEFAULT.to_string()
            } else {
                model_req.to_string()
            }
        });
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(chat_id);
    let created = v
        .get("created")
        .and_then(|x| x.as_i64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model_out,
        "system_fingerprint": null,
        "choices": [{
            "index": 0,
            "message": Value::Object(out_msg),
            "logprobs": null,
            "finish_reason": finish,
        }],
        "usage": v.get("usage").cloned().unwrap_or(Value::Null),
    })
}

async fn send_data(tx: &mpsc::Sender<Result<Bytes, std::io::Error>>, value: &Value) -> bool {
    match serde_json::to_string(value) {
        Ok(s) => tx
            .send(Ok(Bytes::from(format!("data: {s}\n\n"))))
            .await
            .is_ok(),
        Err(_) => true,
    }
}

/// Build a normalized streaming `delta` from an aggregated assistant message
/// (used when re-emitting a non-stream upstream reply as a single chunk).
fn delta_from_message(message: &Value) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("role".into(), json!("assistant"));
    if let Some(c) = message.get("content") {
        if !c.is_null() {
            out.insert("content".into(), c.clone());
        }
    }
    if let Some(r) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
    {
        if !r.is_null() {
            out.insert("reasoning_content".into(), r.clone());
            out.insert("reasoning".into(), r.clone());
        }
    }
    if let Some(tc) = message.get("tool_calls") {
        out.insert("tool_calls".into(), tc.clone());
    }
    Value::Object(out)
}

fn chat_stream_normalized(
    ctrl: Arc<ProxyController>,
    model_req: String,
    want_usage: bool,
    upstream: reqwest::Response,
) -> Response {
    let id = chat_id();
    let created = chrono::Utc::now().timestamp();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let mut stream = upstream.bytes_stream();
        let mut decoder = Utf8Stream::new();
        let mut buffer = String::new();
        let mut first = true;
        let mut saw_finish = false;
        let mut model_seen = if model_req.is_empty() {
            crate::mimo::MODEL_DEFAULT.to_string()
        } else {
            model_req.clone()
        };
        let mut usage_val: Option<Value> = None;
        let mut agg_content = String::new();
        let mut agg_reasoning = String::new();

        let envelope = |delta: Value, finish: Value, model: &str| -> Value {
            let mut chunk = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "system_fingerprint": null,
                "choices": [{
                    "index": 0,
                    "delta": delta,
                    "logprobs": null,
                    "finish_reason": finish,
                }],
            });
            if want_usage {
                chunk["usage"] = Value::Null;
            }
            chunk
        };

        let mut stream_error = false;
        'outer: while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(_) => {
                    // Upstream transport failed mid-stream (connection reset,
                    // decode error, ...). Record it so we don't fake a clean stop.
                    stream_error = true;
                    break;
                }
            };
            buffer.push_str(&decoder.push(&bytes));
            // Guard against an upstream that never emits an SSE delimiter:
            // bound the buffer so a runaway stream can't grow without limit.
            if buffer.len() > MAX_SSE_BUFFER {
                stream_error = true;
                break;
            }
            while let Some(packet) = take_sse_packet(&mut buffer) {
                let payload = sse_payload(&packet);
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    break 'outer;
                }
                let Ok(v) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                if let Some(u) = v.get("usage") {
                    if !u.is_null() {
                        usage_val = Some(u.clone());
                    }
                }
                if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                    if !m.is_empty() {
                        model_seen = m.to_string();
                    }
                }
                let Some(choice) = v.pointer("/choices/0") else {
                    continue;
                };
                let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
                let finish = choice.get("finish_reason").cloned().unwrap_or(Value::Null);
                let nn = |k: &str| delta.get(k).map(|x| !x.is_null()).unwrap_or(false);
                let has_payload =
                    nn("content") || nn("reasoning_content") || nn("reasoning") || nn("tool_calls");
                let has_finish = !finish.is_null();
                // Drop all-null / keep-alive deltas so we don't emit empty
                // chunks, but never drop one carrying finish_reason.
                if !has_payload && !has_finish {
                    continue;
                }
                let (norm, c, r) = normalize_chat_delta(&delta, first);
                first = false;
                if let Some(c) = c {
                    agg_content.push_str(&c);
                }
                if let Some(r) = r {
                    agg_reasoning.push_str(&r);
                }
                if has_finish {
                    saw_finish = true;
                }
                if !send_data(&tx, &envelope(norm, finish, &model_seen)).await {
                    return; // client disconnected; stop draining upstream
                }
            }
        }

        // Upstream transport failed mid-stream before any finish_reason:
        // surface an explicit error frame rather than faking a clean "stop"
        // completion. Gateways keying off finish_reason (OpenRouter, LiteLLM,
        // Cursor, ...) would otherwise read a truncated reply as success.
        // We deliberately do NOT emit a final stop chunk or `[DONE]` here.
        if stream_error && !saw_finish {
            let _ = send_data(
                &tx,
                &json!({
                    "error": {
                        "type": "upstream_error",
                        "code": "upstream_stream_error",
                        "message": "upstream stream ended before completion",
                    }
                }),
            )
            .await;
            if let Some(u) = &usage_val {
                if let Some((p, c, t)) = crate::usage::usage_from_value(&json!({ "usage": u })) {
                    ctrl.usage.record(&model_seen, p, c, t);
                }
            }
            tracing::warn!(
                target = "proxy",
                "chat stream: upstream ended before completion (model={model_seen})"
            );
            if ctrl.verbose() {
                emit_log(
                    &ctrl,
                    json!({
                        "ts": chrono::Utc::now().timestamp_millis(),
                        "kind": "error",
                        "path": crate::mimo::PATH_CHAT,
                        "message": "upstream stream ended before completion",
                        "body": { "content": agg_content, "reasoning_content": agg_reasoning },
                    }),
                );
            }
            return;
        }

        // Guarantee a terminal chunk with finish_reason.
        if !saw_finish {
            let _ = send_data(&tx, &envelope(json!({}), json!("stop"), &model_seen)).await;
        }

        if want_usage {
            let usage = usage_val.clone().unwrap_or_else(
                || json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}),
            );
            let usage_chunk = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_seen,
                "system_fingerprint": null,
                "choices": [],
                "usage": usage,
            });
            let _ = send_data(&tx, &usage_chunk).await;
        }
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;

        if let Some(u) = &usage_val {
            if let Some((p, c, t)) = crate::usage::usage_from_value(&json!({ "usage": u })) {
                ctrl.usage.record(&model_seen, p, c, t);
            }
        }
        if ctrl.verbose() {
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": crate::mimo::PATH_CHAT,
                    "body": { "content": agg_content, "reasoning_content": agg_reasoning },
                }),
            );
        }
    });

    let body_stream =
        futures_util::stream::unfold(rx, |mut rx| async { rx.recv().await.map(|i| (i, rx)) });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    let mut resp = Response::new(Body::from_stream(body_stream));
    *resp.status_mut() = StatusCode::OK;
    *resp.headers_mut() = headers;
    resp
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
    typ: String,
}

/// Accumulator that folds an upstream chat SSE stream into one completion.
#[derive(Default)]
struct ChatAccum {
    content: String,
    reasoning: String,
    finish: Value,
    usage: Value,
    model: String,
    tools: BTreeMap<i64, ToolAcc>,
    done: bool,
}

impl ChatAccum {
    fn feed_buffer(&mut self, buffer: &mut String) {
        while let Some(packet) = take_sse_packet(buffer) {
            let payload = sse_payload(&packet);
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                self.done = true;
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if let Some(u) = v.get("usage") {
                if !u.is_null() {
                    self.usage = u.clone();
                }
            }
            if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                if !m.is_empty() {
                    self.model = m.to_string();
                }
            }
            let Some(choice) = v.pointer("/choices/0") else {
                continue;
            };
            if let Some(f) = choice.get("finish_reason") {
                if !f.is_null() {
                    self.finish = f.clone();
                }
            }
            let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
            if let Some(s) = delta.get("content").and_then(|x| x.as_str()) {
                self.content.push_str(s);
            }
            if let Some(s) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(|x| x.as_str())
            {
                self.reasoning.push_str(s);
            }
            if let Some(arr) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                for tc in arr {
                    let idx = tc.get("index").and_then(|x| x.as_i64()).unwrap_or(0);
                    let e = self.tools.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                        if !id.is_empty() {
                            e.id = id.to_string();
                        }
                    }
                    if let Some(t) = tc.get("type").and_then(|x| x.as_str()) {
                        e.typ = t.to_string();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                            if !n.is_empty() {
                                e.name = n.to_string();
                            }
                        }
                        if let Some(a) = f.get("arguments").and_then(|x| x.as_str()) {
                            e.args.push_str(a);
                        }
                    }
                }
            }
        }
    }

    /// Synthesize an upstream-shaped chat.completion for normalization.
    fn into_completion(self) -> Value {
        let mut message = serde_json::Map::new();
        message.insert("role".into(), json!("assistant"));
        let has_tools = !self.tools.is_empty();
        message.insert(
            "content".into(),
            if self.content.is_empty() && has_tools {
                Value::Null
            } else {
                json!(self.content)
            },
        );
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".into(), json!(self.reasoning));
        }
        let mut finish = self.finish;
        if has_tools {
            let arr: Vec<Value> = self
                .tools
                .into_iter()
                .map(|(idx, t)| {
                    json!({
                        "index": idx,
                        "id": t.id,
                        "type": if t.typ.is_empty() { "function".to_string() } else { t.typ },
                        "function": { "name": t.name, "arguments": t.args },
                    })
                })
                .collect();
            message.insert("tool_calls".into(), Value::Array(arr));
            if finish.is_null() {
                finish = json!("tool_calls");
            }
        }
        if finish.is_null() {
            finish = json!("stop");
        }
        json!({
            "model": self.model,
            "choices": [{ "message": Value::Object(message), "finish_reason": finish }],
            "usage": self.usage,
        })
    }
}

/// Client asked for a non-streaming response but mimo answered with SSE:
/// fold the stream into a single spec `chat.completion`.
async fn chat_aggregate_sse(
    ctrl: Arc<ProxyController>,
    model_req: String,
    upstream: reqwest::Response,
) -> Response {
    let mut stream = upstream.bytes_stream();
    let mut decoder = Utf8Stream::new();
    let mut buffer = String::new();
    let mut state = ChatAccum::default();
    let mut transport_err = false;
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(_) => {
                transport_err = true;
                break;
            }
        };
        buffer.push_str(&decoder.push(&bytes));
        if buffer.len() > MAX_SSE_BUFFER {
            transport_err = true;
            break;
        }
        state.feed_buffer(&mut buffer);
        if state.done {
            break;
        }
    }
    // Truncated by an upstream transport error before any finish_reason or
    // `[DONE]`: returning a 200 with a fake-complete body would hide the
    // failure from the client, so surface a 502 instead.
    let completed = state.done || !state.finish.is_null();
    if transport_err && !completed {
        return map_err(crate::error::BridgeError::Proxy(
            "upstream stream ended before completion".into(),
        ));
    }
    let synth = state.into_completion();
    if let Some((p, c, t)) = crate::usage::usage_from_value(&synth) {
        let m = synth
            .get("model")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&model_req);
        ctrl.usage.record(m, p, c, t);
    }
    let out = normalize_chat_completion(&synth, &model_req);
    if ctrl.verbose() {
        emit_log(
            &ctrl,
            json!({
                "ts": chrono::Utc::now().timestamp_millis(),
                "kind": "response",
                "path": crate::mimo::PATH_CHAT,
                "status": 200,
                "body": out.clone(),
            }),
        );
    }
    Json(out).into_response()
}

/// Client asked for SSE but mimo replied with a single JSON object: re-emit it
/// as a one-chunk stream so streaming clients still complete.
async fn chat_stream_from_json(
    ctrl: Arc<ProxyController>,
    model_req: String,
    want_usage: bool,
    upstream: reqwest::Response,
) -> Response {
    let v = match upstream.json::<Value>().await {
        Ok(v) => v,
        Err(e) => return map_err(crate::error::BridgeError::from(e)),
    };
    if let Some((p, c, t)) = crate::usage::usage_from_value(&v) {
        ctrl.usage.record(&model_req, p, c, t);
    }
    let completion = normalize_chat_completion(&v, &model_req);
    let message = completion
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish = completion
        .pointer("/choices/0/finish_reason")
        .cloned()
        .unwrap_or(json!("stop"));

    let mut chunk = json!({
        "id": completion["id"],
        "object": "chat.completion.chunk",
        "created": completion["created"],
        "model": completion["model"],
        "system_fingerprint": null,
        "choices": [{
            "index": 0,
            "delta": delta_from_message(&message),
            "logprobs": null,
            "finish_reason": finish,
        }],
    });
    if want_usage {
        chunk["usage"] = Value::Null;
    }
    let mut sse = format!("data: {chunk}\n\n");
    if want_usage {
        let usage_chunk = json!({
            "id": completion["id"],
            "object": "chat.completion.chunk",
            "created": completion["created"],
            "model": completion["model"],
            "system_fingerprint": null,
            "choices": [],
            "usage": completion.get("usage").cloned().unwrap_or(Value::Null),
        });
        sse.push_str(&format!("data: {usage_chunk}\n\n"));
    }
    sse.push_str("data: [DONE]\n\n");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    let mut resp = Response::new(Body::from(sse));
    *resp.status_mut() = StatusCode::OK;
    *resp.headers_mut() = headers;
    resp
}

async fn chat_nonstream_normalized(
    ctrl: Arc<ProxyController>,
    model_req: String,
    upstream: reqwest::Response,
) -> Response {
    let v = match upstream.json::<Value>().await {
        Ok(v) => v,
        Err(e) => return map_err(crate::error::BridgeError::from(e)),
    };
    if let Some((p, c, t)) = crate::usage::usage_from_value(&v) {
        ctrl.usage.record(&model_req, p, c, t);
    }
    let out = normalize_chat_completion(&v, &model_req);
    if ctrl.verbose() {
        emit_log(
            &ctrl,
            json!({
                "ts": chrono::Utc::now().timestamp_millis(),
                "kind": "response",
                "path": crate::mimo::PATH_CHAT,
                "status": 200,
                "body": out.clone(),
            }),
        );
    }
    Json(out).into_response()
}

/// `/v1/responses`: try native upstream first; if it 404s (model/endpoint
/// doesn't support Responses), fall back to chat-completions compat conversion.
pub async fn responses(
    State(ctrl): State<Arc<ProxyController>>,
    Json(body): Json<Value>,
) -> Response {
    let started = std::time::Instant::now();
    emit_log(
        &ctrl,
        super::transport::request_log(&ctrl, crate::mimo::PATH_RESPONSES, &body),
    );

    match ctrl
        .mimo
        .post_json(crate::mimo::PATH_RESPONSES, body.clone())
        .await
    {
        Ok(upstream) if upstream.status() == reqwest::StatusCode::NOT_FOUND => {
            let _ = upstream.bytes().await;
            tracing::info!(
                target = "proxy",
                "upstream responses endpoint 404; falling back to chat-completions compat"
            );
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": crate::mimo::PATH_RESPONSES,
                    "status": 404,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            responses_compat(ctrl, body).await
        }
        Ok(upstream) => {
            let status = upstream.status();
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": crate::mimo::PATH_RESPONSES,
                    "status": status.as_u16(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            let model = body
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            proxy_response_tapped(ctrl, model, upstream).await
        }
        Err(e) => {
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "error",
                    "path": crate::mimo::PATH_RESPONSES,
                    "message": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            map_err(e)
        }
    }
}


pub async fn models(State(ctrl): State<Arc<ProxyController>>) -> Response {
    list_models(ctrl).await
}


async fn responses_compat(ctrl: Arc<ProxyController>, body: Value) -> Response {
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let chat_body = responses_to_chat_body(&body, stream);
    let started = std::time::Instant::now();
    emit_log(
        &ctrl,
        json!({
            "ts": chrono::Utc::now().timestamp_millis(),
            "kind": "request",
            "path": "/v1/responses -> /v1/chat/completions",
            "model": body.get("model").and_then(|v| v.as_str()).unwrap_or(""),
            "stream": stream,
        }),
    );

    match ctrl.mimo.post_json(crate::mimo::PATH_CHAT, chat_body).await {
        Ok(upstream) if upstream.status().is_success() && stream => {
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": "/v1/responses -> /v1/chat/completions",
                    "status": upstream.status().as_u16(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            responses_stream_from_chat(ctrl.clone(), upstream, body).await
        }
        Ok(upstream) if upstream.status().is_success() => {
            let status = upstream.status();
            match upstream.json::<Value>().await {
                Ok(chat) => {
                    emit_log(
                        &ctrl,
                        json!({
                            "ts": chrono::Utc::now().timestamp_millis(),
                            "kind": "response",
                            "path": "/v1/responses -> /v1/chat/completions",
                            "status": status.as_u16(),
                            "elapsed_ms": started.elapsed().as_millis() as u64,
                        }),
                    );
                    if let Some((p, c, t)) = crate::usage::usage_from_value(&chat) {
                        let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
                        ctrl.usage.record(model, p, c, t);
                    }
                    Json(response_from_chat(&body, &chat)).into_response()
                }
                Err(e) => map_err(crate::error::BridgeError::from(e)),
            }
        }
        Ok(upstream) => proxy_response(upstream).await,
        Err(e) => {
            emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "error",
                    "path": "/v1/responses -> /v1/chat/completions",
                    "message": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            map_err(e)
        }
    }
}

fn responses_to_chat_body(body: &Value, stream: bool) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "model".into(),
        body.get("model")
            .cloned()
            .unwrap_or_else(|| json!(crate::mimo::MODEL_DEFAULT)),
    );
    out.insert("stream".into(), Value::Bool(stream));

    if stream {
        out.insert(
            "stream_options".into(),
            json!({
                "include_usage": body
                    .pointer("/stream_options/include_usage")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
            }),
        );
    }

    copy_field(body, &mut out, "temperature", "temperature");
    copy_field(body, &mut out, "top_p", "top_p");
    copy_field(body, &mut out, "max_output_tokens", "max_tokens");
    copy_field(body, &mut out, "tool_choice", "tool_choice");
    copy_tools(body, &mut out);

    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        if let Some(text) = text_from_value(instructions) {
            messages.push(json!({"role": "system", "content": text}));
        }
    }
    messages.extend(messages_from_input(body.get("input")));
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": ""}));
    }
    out.insert("messages".into(), Value::Array(messages));
    Value::Object(out)
}

fn copy_field(body: &Value, out: &mut serde_json::Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = body.get(from) {
        out.insert(to.into(), value.clone());
    }
}

fn copy_tools(body: &Value, out: &mut serde_json::Map<String, Value>) {
    let Some(tools) = body.get("tools").and_then(|v| v.as_array()) else {
        return;
    };
    let converted: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            if tool.get("type").and_then(|v| v.as_str()) == Some("function") {
                if tool.get("function").is_some() {
                    return Some(tool.clone());
                }
                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": tool.get("name").cloned().unwrap_or_else(|| json!("function_tool")),
                        "description": tool.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                    }
                }));
            }
            if tool.get("type").and_then(|v| v.as_str()) == Some("custom") {
                return Some(json!({
                    "type": "function",
                    "function": {
                        "name": tool.get("name").cloned().unwrap_or_else(|| json!("custom_tool")),
                        "description": tool.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                    }
                }));
            }
            None
        })
        .collect();
    if !converted.is_empty() {
        out.insert("tools".into(), Value::Array(converted));
    }
}

fn messages_from_input(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(s)) => vec![json!({"role": "user", "content": s})],
        Some(Value::Array(items)) => items.iter().filter_map(message_from_input_item).collect(),
        Some(other) => text_from_value(other)
            .map(|text| vec![json!({"role": "user", "content": text})])
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn message_from_input_item(item: &Value) -> Option<Value> {
    if let Some(text) = item.as_str() {
        return Some(json!({"role": "user", "content": text}));
    }

    let obj = item.as_object()?;
    let typ = obj.get("type").and_then(|v| v.as_str());
    if typ == Some("input_text") {
        return obj
            .get("text")
            .and_then(|v| v.as_str())
            .map(|text| json!({"role": "user", "content": text}));
    }

    let role = obj
        .get("role")
        .and_then(|v| v.as_str())
        .map(chat_role)
        .unwrap_or("user");
    let content = obj.get("content").unwrap_or(item);
    Some(json!({
        "role": role,
        "content": chat_content_from_responses_content(content, role == "user"),
    }))
}

fn chat_role(role: &str) -> &'static str {
    match role {
        "assistant" => "assistant",
        "system" | "developer" => "system",
        "tool" => "tool",
        _ => "user",
    }
}

fn chat_content_from_responses_content(content: &Value, allow_parts: bool) -> Value {
    match content {
        Value::String(s) => Value::String(s.clone()),
        Value::Array(blocks) if allow_parts => {
            let parts: Vec<Value> = blocks
                .iter()
                .filter_map(|block| {
                    let typ = block.get("type").and_then(|v| v.as_str())?;
                    match typ {
                        "input_text" | "output_text" => block
                            .get("text")
                            .and_then(|v| v.as_str())
                            .map(|text| json!({"type": "text", "text": text})),
                        "input_image" => {
                            let url = block
                                .get("image_url")
                                .or_else(|| block.get("file_data"))
                                .and_then(|v| v.as_str())?;
                            Some(json!({"type": "image_url", "image_url": {"url": url}}))
                        }
                        _ => None,
                    }
                })
                .collect();
            if parts.is_empty() {
                Value::String(text_from_blocks(blocks))
            } else {
                Value::Array(parts)
            }
        }
        Value::Array(blocks) => Value::String(text_from_blocks(blocks)),
        other => text_from_value(other)
            .map(Value::String)
            .unwrap_or_else(|| Value::String(String::new())),
    }
}

fn text_from_blocks(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(text_from_value)
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_from_value(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }
    if let Some(text) = value.get("content").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }
    None
}

fn response_from_chat(request: &Value, chat: &Value) -> Value {
    let id = new_response_id("resp");
    let msg_id = new_response_id("msg");
    let created_at = chrono::Utc::now().timestamp();
    let model = chat
        .get("model")
        .or_else(|| request.get("model"))
        .cloned()
        .unwrap_or_else(|| json!(crate::mimo::MODEL_DEFAULT));
    let message = chat.pointer("/choices/0/message").unwrap_or(&Value::Null);
    let text = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let usage = usage_from_chat(chat.get("usage"), &reasoning);
    let output = response_output(&msg_id, &text, &reasoning);

    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "error": null,
        "incomplete_details": null,
        "instructions": request.get("instructions").cloned().unwrap_or(Value::Null),
        "max_output_tokens": request.get("max_output_tokens").cloned().unwrap_or(Value::Null),
        "model": model,
        "output": output,
        "output_text": text,
        "parallel_tool_calls": request.get("parallel_tool_calls").cloned().unwrap_or(Value::Bool(true)),
        "previous_response_id": request.get("previous_response_id").cloned().unwrap_or(Value::Null),
        "reasoning": request.get("reasoning").cloned().unwrap_or_else(|| json!({"effort": null, "summary": null})),
        "store": request.get("store").cloned().unwrap_or(Value::Bool(true)),
        "temperature": request.get("temperature").cloned().unwrap_or_else(|| json!(1)),
        "text": request.get("text").cloned().unwrap_or_else(|| json!({"format": {"type": "text"}})),
        "tool_choice": request.get("tool_choice").cloned().unwrap_or_else(|| json!("auto")),
        "tools": request.get("tools").cloned().unwrap_or_else(|| json!([])),
        "top_p": request.get("top_p").cloned().unwrap_or_else(|| json!(1)),
        "truncation": request.get("truncation").cloned().unwrap_or_else(|| json!("disabled")),
        "usage": usage,
        "user": request.get("user").cloned().unwrap_or(Value::Null),
        "metadata": request.get("metadata").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Build the three closing events for the reasoning output item.
fn close_reasoning_events(rs_id: &str, reasoning: &str, seq: &mut i64) -> Vec<Value> {
    let mut evts = Vec::new();
    evts.push(json!({
        "type": "response.reasoning_summary_text.done",
        "item_id": rs_id, "output_index": 0, "summary_index": 0,
        "text": reasoning, "sequence_number": *seq,
    }));
    *seq += 1;
    evts.push(json!({
        "type": "response.reasoning_summary_part.done",
        "item_id": rs_id, "output_index": 0, "summary_index": 0,
        "part": {"type": "summary_text", "text": reasoning}, "sequence_number": *seq,
    }));
    *seq += 1;
    evts.push(json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {"id": rs_id, "type": "reasoning", "summary": [{"type": "summary_text", "text": reasoning}], "status": "completed"},
        "sequence_number": *seq,
    }));
    *seq += 1;
    evts
}

/// Final `output` array for `response.completed`, using the SAME item ids that
/// were streamed (reasoning item first when present, then the message).
fn responses_final_output(rs_id: &str, reasoning: &str, msg_id: &str, text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if !reasoning.is_empty() {
        out.push(json!({
            "id": rs_id,
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": reasoning}],
            "status": "completed",
        }));
    }
    out.push(json!({
        "id": msg_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }));
    out
}

fn response_output(msg_id: &str, text: &str, reasoning: &str) -> Vec<Value> {
    let mut output = Vec::new();
    if !reasoning.is_empty() {
        output.push(json!({
            "id": new_response_id("rs"),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    output.push(json!({
        "id": msg_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": [],
        }],
    }));
    output
}

fn usage_from_chat(usage: Option<&Value>, reasoning: &str) -> Value {
    let prompt = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let total = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(prompt + completion);
    json!({
        "input_tokens": prompt,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": completion,
        "output_tokens_details": {
            "reasoning_tokens": if reasoning.is_empty() { 0 } else { completion },
        },
        "total_tokens": total,
    })
}

async fn responses_stream_from_chat(
    ctrl: Arc<ProxyController>,
    upstream: reqwest::Response,
    request: Value,
) -> Response {
    let response_id = new_response_id("resp");
    let rs_id = new_response_id("rs");
    let msg_id = new_response_id("msg");
    let created_at = chrono::Utc::now().timestamp();
    let model = request
        .get("model")
        .cloned()
        .unwrap_or_else(|| json!(crate::mimo::MODEL_DEFAULT));
    let usage_model = model
        .as_str()
        .unwrap_or(crate::mimo::MODEL_DEFAULT)
        .to_string();

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    tokio::spawn(async move {
        let mut seq = 1_i64;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut usage = Value::Null;
        let mut decoder = Utf8Stream::new();
        let mut buffer = String::new();
        let mut stream = upstream.bytes_stream();
        // Reasoning and message are SEPARATE output items: reasoning at
        // output_index 0 (opened lazily on the first reasoning token), the
        // message after it. This matches OpenAI Responses semantics and keeps
        // the streamed items consistent with the final `response.completed`.
        let mut reasoning_opened = false;
        let mut reasoning_closed = false;
        let mut message_opened = false;
        let mut msg_index = 0_i64;

        send_event(
            &tx,
            response_event(
                "response.created",
                seq,
                "in_progress",
                &response_id,
                created_at,
                model.clone(),
                Vec::new(),
                "",
                Value::Null,
                &request,
            ),
        )
        .await;
        seq += 1;
        send_event(
            &tx,
            response_event(
                "response.in_progress",
                seq,
                "in_progress",
                &response_id,
                created_at,
                model.clone(),
                Vec::new(),
                "",
                Value::Null,
                &request,
            ),
        )
        .await;
        seq += 1;

        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                send_event(
                    &tx,
                    json!({
                        "type": "error",
                        "code": "upstream_stream_error",
                        "message": "upstream stream ended with an error",
                        "sequence_number": seq,
                    }),
                )
                .await;
                return;
            };
            buffer.push_str(&decoder.push(&chunk));
            while let Some(packet) = take_sse_packet(&mut buffer) {
                let payload = sse_payload(&packet);
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    break;
                }
                let Ok(value) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                if let Some(u) = value.get("usage") {
                    usage = usage_from_chat(Some(u), &reasoning);
                }
                let choice = value.pointer("/choices/0").unwrap_or(&Value::Null);
                let delta = choice.get("delta").unwrap_or(&Value::Null);
                if let Some(piece) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                {
                    if !reasoning_opened {
                        reasoning_opened = true;
                        send_event(
                            &tx,
                            json!({
                                "type": "response.output_item.added",
                                "output_index": 0,
                                "item": {"id": rs_id, "type": "reasoning", "summary": [], "status": "in_progress"},
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                        send_event(
                            &tx,
                            json!({
                                "type": "response.reasoning_summary_part.added",
                                "item_id": rs_id,
                                "output_index": 0,
                                "summary_index": 0,
                                "part": {"type": "summary_text", "text": ""},
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                    }
                    reasoning.push_str(piece);
                    send_event(
                        &tx,
                        json!({
                            "type": "response.reasoning_summary_text.delta",
                            "item_id": rs_id,
                            "output_index": 0,
                            "summary_index": 0,
                            "delta": piece,
                            "sequence_number": seq,
                        }),
                    )
                    .await;
                    seq += 1;
                }
                if let Some(piece) = delta.get("content").and_then(|v| v.as_str()) {
                    // Reasoning precedes content; close the reasoning item first.
                    if reasoning_opened && !reasoning_closed {
                        reasoning_closed = true;
                        for evt in close_reasoning_events(&rs_id, &reasoning, &mut seq) {
                            send_event(&tx, evt).await;
                        }
                    }
                    if !message_opened {
                        message_opened = true;
                        msg_index = if reasoning_opened { 1 } else { 0 };
                        send_event(
                            &tx,
                            json!({
                                "type": "response.output_item.added",
                                "output_index": msg_index,
                                "item": {"id": msg_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []},
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                        send_event(
                            &tx,
                            json!({
                                "type": "response.content_part.added",
                                "item_id": msg_id,
                                "output_index": msg_index,
                                "content_index": 0,
                                "part": {"type": "output_text", "text": "", "annotations": []},
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                    }
                    text.push_str(piece);
                    send_event(
                        &tx,
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": msg_id,
                            "output_index": msg_index,
                            "content_index": 0,
                            "delta": piece,
                            "sequence_number": seq,
                        }),
                    )
                    .await;
                    seq += 1;
                }
            }
        }

        if usage.is_null() {
            usage = usage_from_chat(None, &reasoning);
        }
        if let Some((p, c, t)) = crate::usage::usage_from_value(&json!({ "usage": usage.clone() }))
        {
            ctrl.usage.record(&usage_model, p, c, t);
        }

        // Close a reasoning item that never saw following content.
        if reasoning_opened && !reasoning_closed {
            reasoning_closed = true;
            for evt in close_reasoning_events(&rs_id, &reasoning, &mut seq) {
                send_event(&tx, evt).await;
            }
        }
        let _ = reasoning_closed;
        // OpenAI Responses always returns a message item; open an empty one if
        // the upstream produced no visible content.
        if !message_opened {
            message_opened = true;
            msg_index = if reasoning_opened { 1 } else { 0 };
            send_event(
                &tx,
                json!({
                    "type": "response.output_item.added",
                    "output_index": msg_index,
                    "item": {"id": msg_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []},
                    "sequence_number": seq,
                }),
            )
            .await;
            seq += 1;
            send_event(
                &tx,
                json!({
                    "type": "response.content_part.added",
                    "item_id": msg_id,
                    "output_index": msg_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                    "sequence_number": seq,
                }),
            )
            .await;
            seq += 1;
        }
        let _ = message_opened;
        send_event(
            &tx,
            json!({
                "type": "response.output_text.done",
                "item_id": msg_id,
                "output_index": msg_index,
                "content_index": 0,
                "text": text,
                "sequence_number": seq,
            }),
        )
        .await;
        seq += 1;
        send_event(
            &tx,
            json!({
                "type": "response.content_part.done",
                "item_id": msg_id,
                "output_index": msg_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": []},
                "sequence_number": seq,
            }),
        )
        .await;
        seq += 1;
        send_event(
            &tx,
            json!({
                "type": "response.output_item.done",
                "output_index": msg_index,
                "item": {"id": msg_id, "type": "message", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]},
                "sequence_number": seq,
            }),
        )
        .await;
        seq += 1;
        let output = responses_final_output(&rs_id, &reasoning, &msg_id, &text);
        send_event(
            &tx,
            response_event(
                "response.completed",
                seq,
                "completed",
                &response_id,
                created_at,
                model,
                output,
                &text,
                usage,
                &request,
            ),
        )
        .await;
    });

    let body_stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    let mut resp = Response::new(Body::from_stream(body_stream));
    *resp.status_mut() = StatusCode::OK;
    *resp.headers_mut() = headers;
    resp
}

#[allow(clippy::too_many_arguments)]
fn response_event(
    event_type: &str,
    sequence_number: i64,
    status: &str,
    id: &str,
    created_at: i64,
    model: Value,
    output: Vec<Value>,
    output_text: &str,
    usage: Value,
    request: &Value,
) -> Value {
    let mut response = response_from_chat(
        request,
        &json!({
            "model": model,
            "usage": usage,
            "choices": [{
                "message": {
                    "content": output_text,
                }
            }]
        }),
    );
    response["id"] = json!(id);
    response["created_at"] = json!(created_at);
    response["status"] = json!(status);
    response["output"] = Value::Array(output);
    response["usage"] = usage;
    json!({
        "type": event_type,
        "response": response,
        "sequence_number": sequence_number,
    })
}

async fn send_event(tx: &mpsc::Sender<Result<Bytes, std::io::Error>>, value: Value) {
    let event_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");
    let Ok(data) = serde_json::to_string(&value) else {
        return;
    };
    let frame = format!("event: {event_type}\ndata: {data}\n\n");
    let _ = tx.send(Ok(Bytes::from(frame))).await;
}

fn take_sse_packet(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    let (idx, sep_len) = match (lf, crlf) {
        (Some(a), Some(b)) => {
            if a.0 < b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let packet = buffer[..idx].to_string();
    buffer.drain(..idx + sep_len);
    Some(packet)
}

fn sse_payload(packet: &str) -> String {
    packet
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n")
}

fn new_response_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_string_input_maps_to_chat_messages() {
        let body = json!({
            "model": "mimo-pro",
            "input": "hi",
            "max_output_tokens": 32,
        });
        let chat = responses_to_chat_body(&body, false);
        assert_eq!(chat["model"], "mimo-pro");
        assert_eq!(chat["max_tokens"], 32);
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][0]["content"], "hi");
    }

    #[test]
    fn responses_message_input_preserves_system_and_images() {
        let body = json!({
            "instructions": "be terse",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "describe"},
                    {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                ]
            }]
        });
        let chat = responses_to_chat_body(&body, true);
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][1]["content"][0]["type"], "text");
        assert_eq!(chat["messages"][1]["content"][1]["type"], "image_url");
    }

    #[test]
    fn chat_response_maps_to_responses_output_text() {
        let req = json!({"model": "mimo-pro", "input": "hi"});
        let chat = json!({
            "model": "mimo-pro",
            "choices": [{"message": {"content": "hello", "reasoning_content": "thinking"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        });
        let response = response_from_chat(&req, &chat);
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output_text"], "hello");
        assert_eq!(response["usage"]["total_tokens"], 7);
        assert_eq!(response["output"][0]["type"], "reasoning");
    }

    #[test]
    fn chat_delta_mirrors_reasoning_and_sets_role() {
        // reasoning-only delta on the first chunk: role added, reasoning mirrored.
        let (d, c, r) = normalize_chat_delta(&json!({"reasoning_content": "think"}), true);
        assert_eq!(d["role"], "assistant");
        assert_eq!(d["reasoning_content"], "think");
        assert_eq!(d["reasoning"], "think");
        assert!(d.get("content").is_none());
        assert_eq!(c, None);
        assert_eq!(r.as_deref(), Some("think"));

        // content delta (not first): no role, content preserved.
        let (d2, c2, _) = normalize_chat_delta(&json!({"content": "hi"}), false);
        assert!(d2.get("role").is_none());
        assert_eq!(d2["content"], "hi");
        assert_eq!(c2.as_deref(), Some("hi"));

        // upstream that only uses `reasoning` is still mirrored to both.
        let (d3, _, _) = normalize_chat_delta(&json!({"reasoning": "x"}), false);
        assert_eq!(d3["reasoning_content"], "x");
        assert_eq!(d3["reasoning"], "x");
    }

    #[test]
    fn nonstream_completion_is_normalized() {
        let upstream = json!({
            "model": "xiaomi/mimo-pro",
            "choices": [{
                "message": {"role": "assistant", "content": "hello", "reasoning_content": "because"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 5, "total_tokens": 7}
        });
        let out = normalize_chat_completion(&upstream, "mimo-pro");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["index"], 0);
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "because");
        assert_eq!(out["choices"][0]["message"]["reasoning"], "because");
        assert_eq!(out["usage"]["total_tokens"], 7);
        assert!(out["id"].as_str().is_some());
    }

    #[test]
    fn accum_folds_sse_into_completion() {
        let mut buf = String::new();
        buf.push_str(
            "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"reasoning_content\":\"th\"}}]}\n\n",
        );
        buf.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        buf.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n");
        buf.push_str("data: [DONE]\n\n");
        let mut st = ChatAccum::default();
        st.feed_buffer(&mut buf);
        assert!(st.done);
        let synth = st.into_completion();
        let out = normalize_chat_completion(&synth, "m");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "th");
        assert_eq!(out["choices"][0]["message"]["reasoning"], "th");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["total_tokens"], 3);
    }

    #[test]
    fn accum_assembles_tool_call_fragments() {
        let mut buf = String::new();
        buf.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n");
        buf.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n");
        buf.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n");
        let mut st = ChatAccum::default();
        st.feed_buffer(&mut buf);
        let synth = st.into_completion();
        assert_eq!(synth["choices"][0]["finish_reason"], "tool_calls");
        let tc = &synth["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["function"]["name"], "f");
        assert_eq!(tc["function"]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn delta_from_message_mirrors_reasoning() {
        let msg = json!({"role": "assistant", "content": "x", "reasoning_content": "y"});
        let d = delta_from_message(&msg);
        assert_eq!(d["role"], "assistant");
        assert_eq!(d["content"], "x");
        assert_eq!(d["reasoning_content"], "y");
        assert_eq!(d["reasoning"], "y");
    }

    // ---- streaming-link end-to-end helpers ------------------------------

    /// Build a fake upstream `reqwest::Response` from a sequence of byte
    /// chunks (each may be an `Err` to simulate a transport failure).
    fn upstream_from_parts(
        parts: Vec<std::result::Result<Bytes, std::io::Error>>,
        content_type: &str,
    ) -> reqwest::Response {
        let body = reqwest::Body::wrap_stream(futures::stream::iter(parts));
        let http_resp = axum::http::Response::builder()
            .status(200)
            .header("content-type", content_type)
            .body(body)
            .unwrap();
        reqwest::Response::from(http_resp)
    }

    fn ok_parts(chunks: &[&str]) -> Vec<std::result::Result<Bytes, std::io::Error>> {
        chunks
            .iter()
            .map(|s| Ok(Bytes::from(s.to_string())))
            .collect()
    }

    fn data(v: Value) -> String {
        format!("data: {v}\n\n")
    }

    fn test_ctrl() -> Arc<ProxyController> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let auth = Arc::new(parking_lot::RwLock::new(crate::auth::AuthState::default()));
        let mimo = Arc::new(crate::mimo::MimoClient::new(auth));
        let emitter = crate::state::LogEmitter::new(Arc::new(crate::state::LogHub::new(16)));
        let dir = std::env::temp_dir().join(format!("mb-openai-{}-{}", std::process::id(), n));
        let storage =
            crate::storage::Storage::for_paths(dir.join("c"), dir.join("d")).unwrap();
        let usage = crate::usage::UsageStore::load(storage);
        Arc::new(ProxyController::new(mimo, emitter, usage))
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn stream_normalized_emits_role_reasoning_finish_usage_done() {
        let parts = ok_parts(&[
            &data(json!({"model":"m","choices":[{"delta":{"role":"assistant","content":""},"finish_reason":null,"index":0}]})),
            &data(json!({"choices":[{"delta":{"reasoning_content":"think"},"finish_reason":null,"index":0}]})),
            &data(json!({"choices":[{"delta":{"content":"hi"},"finish_reason":null,"index":0}]})),
            &data(json!({"choices":[{"delta":{},"finish_reason":"stop","index":0}]})),
            &data(json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}})),
        ]);
        let upstream = upstream_from_parts(parts, "text/event-stream");
        let resp = chat_stream_normalized(test_ctrl(), "m".into(), true, upstream);
        let out = body_text(resp).await;

        assert!(out.contains("\"role\":\"assistant\""));
        assert!(out.contains("\"reasoning_content\":\"think\""));
        assert!(out.contains("\"reasoning\":\"think\""));
        assert!(out.contains("\"content\":\"hi\""));
        assert!(out.contains("\"finish_reason\":\"stop\""));
        // Trailing usage-only chunk then [DONE], in that order.
        let usage_at = out.find("\"choices\":[]").expect("usage-only chunk");
        let done_at = out.rfind("data: [DONE]").expect("[DONE]");
        assert!(usage_at < done_at);
        assert!(out.contains("\"total_tokens\":13"));
        assert!(out.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn stream_normalized_upstream_error_emits_error_frame() {
        let mut parts = ok_parts(&[
            &data(json!({"choices":[{"delta":{"role":"assistant","content":"par"},"finish_reason":null,"index":0}]})),
            &data(json!({"choices":[{"delta":{"reasoning_content":"th"},"finish_reason":null,"index":0}]})),
        ]);
        parts.push(Err(std::io::Error::other("connection reset")));
        let upstream = upstream_from_parts(parts, "text/event-stream");
        let resp = chat_stream_normalized(test_ctrl(), "m".into(), false, upstream);
        let out = body_text(resp).await;

        // An explicit error frame, no fake stop and no [DONE].
        assert!(out.contains("\"error\""));
        assert!(out.contains("upstream_stream_error"));
        assert!(!out.contains("\"finish_reason\":\"stop\""));
        assert!(!out.contains("[DONE]"));
    }

    #[tokio::test]
    async fn stream_normalized_synthesizes_finish_when_upstream_omits_it() {
        // Clean EOF with no finish_reason and no error: we must still cap the
        // stream with a synthetic finish_reason:"stop" + [DONE].
        let parts = ok_parts(&[
            &data(json!({"choices":[{"delta":{"role":"assistant","content":"hi"},"finish_reason":null,"index":0}]})),
        ]);
        let upstream = upstream_from_parts(parts, "text/event-stream");
        let resp = chat_stream_normalized(test_ctrl(), "m".into(), false, upstream);
        let out = body_text(resp).await;

        assert!(out.contains("\"finish_reason\":\"stop\""));
        assert!(!out.contains("\"error\""));
        assert!(out.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn stream_normalized_reassembles_utf8_split_across_chunks() {
        // Build a complete SSE stream, then slice it into tiny byte windows so
        // multibyte characters straddle chunk boundaries.
        let sse = format!(
            "{}{}data: [DONE]\n\n",
            data(json!({"choices":[{"delta":{"role":"assistant","content":"你好世界"},"finish_reason":null,"index":0}]})),
            data(json!({"choices":[{"delta":{},"finish_reason":"stop","index":0}]})),
        );
        let bytes = sse.into_bytes();
        let parts: Vec<std::result::Result<Bytes, std::io::Error>> = bytes
            .chunks(5)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let upstream = upstream_from_parts(parts, "text/event-stream");
        let resp = chat_stream_normalized(test_ctrl(), "m".into(), false, upstream);
        let out = body_text(resp).await;

        assert!(out.contains("\"content\":\"你好世界\""));
        assert!(!out.contains('\u{FFFD}')); // no replacement chars
    }

    #[tokio::test]
    async fn aggregate_sse_folds_to_single_completion_with_tools() {
        let parts = ok_parts(&[
            &data(json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\""}}]},"finish_reason":null,"index":0}]})),
            &data(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"hi\"}"}}]},"finish_reason":null,"index":0}]})),
            &data(json!({"choices":[{"delta":{},"finish_reason":"tool_calls","index":0}]})),
            "data: [DONE]\n\n",
        ]);
        let upstream = upstream_from_parts(parts, "text/event-stream");
        let resp = chat_aggregate_sse(test_ctrl(), "m".into(), upstream).await;
        let v: Value = serde_json::from_str(&body_text(resp).await).unwrap();

        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        let tc = &v["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["function"]["name"], "lookup");
        assert_eq!(tc["function"]["arguments"], "{\"q\":\"hi\"}");
    }

    #[tokio::test]
    async fn aggregate_sse_truncated_returns_gateway_error() {
        let mut parts = ok_parts(&[
            &data(json!({"choices":[{"delta":{"role":"assistant","content":"par"},"finish_reason":null,"index":0}]})),
        ]);
        parts.push(Err(std::io::Error::other("connection reset")));
        let upstream = upstream_from_parts(parts, "text/event-stream");
        let resp = chat_aggregate_sse(test_ctrl(), "m".into(), upstream).await;
        // Truncated before completion → 502, not a fake-complete 200.
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn stream_from_json_emits_single_chunk_then_usage_then_done() {
        let json_body = json!({
            "model": "m",
            "choices": [{
                "message": {"role": "assistant", "content": "hi", "reasoning_content": "th"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        });
        let parts = ok_parts(&[&json_body.to_string()]);
        let upstream = upstream_from_parts(parts, "application/json");
        let resp = chat_stream_from_json(test_ctrl(), "m".into(), true, upstream).await;
        let out = body_text(resp).await;

        assert!(out.contains("\"object\":\"chat.completion.chunk\""));
        assert!(out.contains("\"content\":\"hi\""));
        assert!(out.contains("\"reasoning\":\"th\""));
        assert!(out.contains("\"finish_reason\":\"stop\""));
        let usage_at = out.find("\"total_tokens\":3").expect("usage chunk");
        let done_at = out.rfind("data: [DONE]").expect("[DONE]");
        assert!(usage_at < done_at);
        assert!(out.trim_end().ends_with("data: [DONE]"));
    }
}
