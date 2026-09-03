//! Anthropic Messages -> mimo OpenAI Chat Completions bridge.
//!
//! Super XiaoAI PC exposes `/osbot/pc/llm/v2/chat/completions` (OpenAI Chat
//! Completions, with optional SSE streaming where `delta` carries
//! `content` / `reasoning_content` / `tool_calls`). This module translates
//! Anthropic Messages requests into that format and streams the response
//! back as Anthropic-flavored SSE so Claude clients can talk to mimo
//! unchanged.
//!
//! Mapping summary:
//!   anthropic.messages         -> openai.messages
//!   anthropic.system           -> openai.messages[role=system]
//!   anthropic.tools            -> openai.tools
//!   openai.chunk.delta.content        -> content_block_delta {text_delta}
//!   openai.chunk.delta.reasoning_content -> content_block_delta {thinking_delta}
//!   openai.chunk.delta.tool_calls    -> content_block_start/delta {tool_use, input_json_delta}
//!   openai.chunk.finish_reason       -> message_delta with stop_reason

use super::transport::map_err;
use super::ProxyController;
use crate::error::BridgeError;
use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use uuid::Uuid;

/// mimo provides no Anthropic thinking-block signature; this placeholder is
/// sent so the SSE shape is valid. It is NOT a real Anthropic signature and
/// won't validate if echoed back to Anthropic upstream (irrelevant here, since
/// the bridge ignores it on inbound requests).
const THINKING_SIGNATURE_PLACEHOLDER: &str = "mimo-bridge-unsigned";

pub async fn messages(
    State(ctrl): State<Arc<ProxyController>>,
    Json(body): Json<Value>,
) -> Response {
    let stream_requested = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let openai_body = match anthropic_to_openai_chat(&body) {
        Ok(v) => v,
        Err(e) => return map_err(BridgeError::Proxy(e)),
    };

    // Always stream from the upstream (see chat_completions in openai.rs):
    // a non-streaming mimo response only ships after the full generation,
    // which regularly outlived the old 30s client timeout. The Anthropic
    // response shape still follows the client's own `stream` flag.
    let mut openai_body = openai_body;
    if let Some(obj) = openai_body.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(true));
        obj.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    let model = openai_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::mimo::MODEL_DEFAULT)
        .to_string();
    // Anthropic only surfaces thinking blocks when the client opted in via
    // `thinking: {type: "enabled"}`. Otherwise we must NOT emit thinking blocks
    // (some clients reject unexpected ones), so the reasoning trace is dropped.
    let thinking_enabled =
        body.pointer("/thinking/type").and_then(|v| v.as_str()) == Some("enabled");
    let started = std::time::Instant::now();
    let mut req_log = json!({
        "ts": chrono::Utc::now().timestamp_millis(),
        "kind": "request",
        "path": "/v1/messages",
        "model": model.clone(),
        "stream": stream_requested,
    });
    if ctrl.verbose() {
        req_log["body"] = body.clone();
    }
    super::emit_log(&ctrl, req_log);

    match ctrl.mimo.chat(openai_body).await {
        Ok(upstream) => {
            let status =
                StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            super::emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "response",
                    "path": "/v1/messages",
                    "status": status.as_u16(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            if !status.is_success() {
                let text = upstream.text().await.unwrap_or_default();
                return (status, text).into_response();
            }
            if stream_requested {
                stream_anthropic(upstream, model, ctrl.usage.clone(), thinking_enabled)
            } else {
                aggregate_anthropic(upstream, model, ctrl.usage.clone(), thinking_enabled).await
            }
        }
        Err(e) => {
            super::emit_log(
                &ctrl,
                json!({
                    "ts": chrono::Utc::now().timestamp_millis(),
                    "kind": "error",
                    "path": "/v1/messages",
                    "message": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            map_err(e)
        }
    }
}

/// Build an OpenAI Chat Completions request from an Anthropic Messages body.
fn anthropic_to_openai_chat(body: &Value) -> std::result::Result<Value, String> {
    let mut out = Map::new();

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::mimo::MODEL_DEFAULT)
        .to_string();
    // Strip Anthropic prefix if present.
    let model = if let Some(stripped) = model.strip_prefix("anthropic/") {
        stripped.to_string()
    } else {
        model
    };
    out.insert("model".into(), Value::String(model));

    let mut messages: Vec<Value> = Vec::new();

    // Anthropic system → first message.
    if let Some(sys) = body.get("system") {
        let text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    // Anthropic messages → openai messages, expanding tool_use / tool_result blocks.
    if let Some(arr) = body.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = m.get("content");
            match content {
                Some(Value::String(s)) => {
                    messages.push(json!({"role": role, "content": s}));
                }
                Some(Value::Array(parts)) => {
                    let mut text_buf = String::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    let mut tool_results: Vec<(String, String)> = Vec::new();
                    for p in parts {
                        let kind = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match kind {
                            "text" => {
                                if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                                    if !text_buf.is_empty() {
                                        text_buf.push('\n');
                                    }
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_use" => {
                                tool_calls.push(json!({
                                    "id": p.get("id"),
                                    "type": "function",
                                    "function": {
                                        "name": p.get("name"),
                                        "arguments": p
                                            .get("input")
                                            .map(|v| v.to_string())
                                            .unwrap_or_else(|| "{}".into()),
                                    }
                                }));
                            }
                            "tool_result" => {
                                let id = p
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let text = p
                                    .get("content")
                                    .map(|v| match v {
                                        Value::String(s) => s.clone(),
                                        Value::Array(arr) => arr
                                            .iter()
                                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                                            .collect::<Vec<_>>()
                                            .join("\n"),
                                        _ => v.to_string(),
                                    })
                                    .unwrap_or_default();
                                tool_results.push((id, text));
                            }
                            // Inbound `thinking` blocks (echoed back by clients
                            // doing multi-turn extended thinking) are dropped on
                            // purpose: mimo does not persist reasoning across
                            // turns, and our outbound signatures are placeholders
                            // (see THINKING_SIGNATURE_PLACEHOLDER). Forwarding
                            // them would only risk an upstream signature check;
                            // dropping them is safe but means prior-turn
                            // reasoning context is not replayed.
                            _ => {}
                        }
                    }

                    // Emit a tool message for each tool_result first (so the
                    // upstream model has the tool output before the new turn).
                    for (id, text) in tool_results {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": id,
                            "content": text,
                        }));
                    }

                    if !text_buf.is_empty() || !tool_calls.is_empty() {
                        let mut msg = Map::new();
                        msg.insert("role".into(), Value::String(role.to_string()));
                        if !text_buf.is_empty() {
                            msg.insert("content".into(), Value::String(text_buf));
                        } else {
                            msg.insert("content".into(), Value::Null);
                        }
                        if !tool_calls.is_empty() {
                            msg.insert("tool_calls".into(), Value::Array(tool_calls));
                        }
                        messages.push(Value::Object(msg));
                    }
                }
                _ => {}
            }
        }
    }

    out.insert("messages".into(), Value::Array(messages));
    if let Some(t) = body.get("temperature") {
        out.insert("temperature".into(), t.clone());
    }
    if let Some(t) = body.get("top_p") {
        out.insert("top_p".into(), t.clone());
    }
    if let Some(t) = body.get("max_tokens") {
        out.insert("max_tokens".into(), t.clone());
    }
    if let Some(stream) = body.get("stream") {
        out.insert("stream".into(), stream.clone());
    }
    if let Some(stop) = body.get("stop_sequences") {
        out.insert("stop".into(), stop.clone());
    }
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name"),
                        "description": t.get("description"),
                        "parameters": t.get("input_schema"),
                    }
                })
            })
            .collect();
        out.insert("tools".into(), Value::Array(mapped));
    }
    Ok(Value::Object(out))
}

fn stream_anthropic(
    upstream: reqwest::Response,
    model: String,
    usage: Arc<crate::usage::UsageStore>,
    thinking_enabled: bool,
) -> Response {
    let stream = SseTranslator::new(
        upstream.bytes_stream().boxed(),
        model,
        usage,
        thinking_enabled,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn aggregate_anthropic(
    upstream: reqwest::Response,
    model: String,
    usage: Arc<crate::usage::UsageStore>,
    thinking_enabled: bool,
) -> Response {
    // Even when stream=false, mimo may still respond with SSE; collect chunks.
    let body_text = upstream.text().await.unwrap_or_default();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut stop_reason = "end_turn".to_string();
    let mut tool_uses: Vec<Value> = Vec::new();
    let mut tool_state: std::collections::HashMap<u64, ToolBuilder> = Default::default();

    let chunks = collect_chunks(&body_text);
    for chunk in &chunks {
        if let Some(usage) = chunk.get("usage") {
            input_tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(input_tokens);
            output_tokens = usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(output_tokens);
        }
        let choices = match chunk.get("choices").and_then(|v| v.as_array()) {
            Some(c) => c,
            None => continue,
        };
        for ch in choices {
            if let Some(reason) = ch.get("finish_reason").and_then(|v| v.as_str()) {
                stop_reason = match reason {
                    "stop" => "end_turn".into(),
                    "length" => "max_tokens".into(),
                    "tool_calls" => "tool_use".into(),
                    other => other.to_string(),
                };
            }
            // Both streaming `delta` and non-streaming `message` shapes.
            for key in ["delta", "message"] {
                if let Some(d) = ch.get(key) {
                    if let Some(s) = d.get("content").and_then(|v| v.as_str()) {
                        text.push_str(s);
                    }
                    if let Some(s) = d.get("reasoning_content").and_then(|v| v.as_str()) {
                        thinking.push_str(s);
                    }
                    if let Some(arr) = d.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in arr {
                            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            let entry = tool_state.entry(idx).or_default();
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                entry.id = id.to_string();
                            }
                            if let Some(fnv) = tc.get("function") {
                                if let Some(name) = fnv.get("name").and_then(|v| v.as_str()) {
                                    entry.name = name.to_string();
                                }
                                if let Some(args) = fnv.get("arguments").and_then(|v| v.as_str()) {
                                    entry.args.push_str(args);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let mut indices: Vec<u64> = tool_state.keys().copied().collect();
    indices.sort();
    for idx in indices {
        let b = tool_state.remove(&idx).unwrap();
        let parsed: Value = serde_json::from_str(&b.args).unwrap_or_else(|_| json!({}));
        tool_uses.push(json!({
            "type": "tool_use",
            "id": if b.id.is_empty() { format!("toolu_{}", Uuid::new_v4().simple()) } else { b.id },
            "name": b.name,
            "input": parsed,
        }));
    }

    let mut content: Vec<Value> = Vec::new();
    if thinking_enabled && !thinking.is_empty() {
        content.push(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": THINKING_SIGNATURE_PLACEHOLDER,
        }));
    }
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content.extend(tool_uses);

    usage.record(
        &model,
        input_tokens as i64,
        output_tokens as i64,
        (input_tokens + output_tokens) as i64,
    );
    let payload = json!({
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    });
    (StatusCode::OK, Json(payload)).into_response()
}

fn collect_chunks(body_text: &str) -> Vec<Value> {
    let mut chunks = Vec::new();
    let mut found_sse = false;
    for line in body_text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            found_sse = true;
            let trimmed = rest.trim();
            if trimmed.is_empty() || trimmed == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                chunks.push(v);
            }
        }
    }
    if !found_sse {
        if let Ok(v) = serde_json::from_str::<Value>(body_text) {
            chunks.push(v);
        }
    }
    chunks
}

#[derive(Default)]
struct ToolBuilder {
    id: String,
    name: String,
    args: String,
}

/// Streams upstream OpenAI Chat Completions SSE events into Anthropic
/// Messages SSE events.
struct SseTranslator {
    inner: futures::stream::BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>,
    decoder: crate::decode::Utf8Stream,
    buf: String,
    model: String,
    usage: Arc<crate::usage::UsageStore>,
    thinking_enabled: bool,
    state: TranslatorState,
}

#[derive(Default)]
struct TranslatorState {
    started: bool,
    finished: bool,
    msg_id: String,
    /// Open block kind. None = no block open.
    open: Option<OpenBlock>,
    /// Sequential index assigned to the next block.
    next_index: u32,
    /// Map openai tool_calls.index -> our anthropic block index + tool state.
    tool_blocks: std::collections::HashMap<u64, ToolBlock>,
    /// Aggregated usage (mimo emits a final chunk with usage, no choices).
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: String,
}

#[derive(Clone, Copy)]
enum OpenBlock {
    Thinking,
    Text,
    /// openai tool_calls.index for this open block
    Tool(u64),
}

#[derive(Default)]
struct ToolBlock {
    block_index: u32,
    started: bool,
    name_emitted: bool,
}

impl SseTranslator {
    fn new(
        inner: futures::stream::BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>,
        model: String,
        usage: Arc<crate::usage::UsageStore>,
        thinking_enabled: bool,
    ) -> Self {
        Self {
            inner,
            decoder: crate::decode::Utf8Stream::new(),
            buf: String::new(),
            model,
            usage,
            thinking_enabled,
            state: TranslatorState::default(),
        }
    }

    fn pop_event(&mut self) -> Option<String> {
        // Each SSE message is terminated by a blank line.
        let split = self.buf.find("\n\n")?;
        let block = self.buf[..split].to_string();
        self.buf.drain(..split + 2);
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        Some(data)
    }

    fn translate(&mut self, data: &str) -> Vec<String> {
        let mut out = Vec::new();
        if data.is_empty() || data == "[DONE]" {
            return out;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return out,
        };

        if !self.state.started {
            self.state.started = true;
            self.state.msg_id = chunk
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple()));
            let evt = json!({
                "type": "message_start",
                "message": {
                    "id": self.state.msg_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    // mimo only reports token usage in its final SSE packet, so
                    // the real input count is unknown at message_start. Anthropic
                    // normally puts a non-zero input_tokens here; we send 0 and
                    // let the later `message_delta` carry the authoritative
                    // (cumulative) usage. Clients that read message_start for a
                    // token budget will under-count until that delta arrives.
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            });
            out.push(format_sse("message_start", &evt));
        }

        if let Some(usage) = chunk.get("usage") {
            if let Some(n) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                self.state.input_tokens = n;
            }
            if let Some(n) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                self.state.output_tokens = n;
            }
        }

        let choices = match chunk.get("choices").and_then(|v| v.as_array()) {
            Some(c) => c,
            None => return out,
        };

        for ch in choices {
            let delta = match ch.get("delta") {
                Some(d) => d,
                None => continue,
            };

            // Thinking delta (mimo emits reasoning_content chunks). Only emit
            // thinking blocks when the client enabled extended thinking.
            if self.thinking_enabled {
                if let Some(s) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                {
                    if !s.is_empty() {
                        self.ensure_open(OpenBlock::Thinking, &mut out);
                        let evt = json!({
                            "type": "content_block_delta",
                            "index": self.current_index(),
                            "delta": {"type": "thinking_delta", "thinking": s}
                        });
                        out.push(format_sse("content_block_delta", &evt));
                    }
                }
            }

            // Visible text delta.
            if let Some(s) = delta.get("content").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    self.ensure_open(OpenBlock::Text, &mut out);
                    let evt = json!({
                        "type": "content_block_delta",
                        "index": self.current_index(),
                        "delta": {"type": "text_delta", "text": s}
                    });
                    out.push(format_sse("content_block_delta", &evt));
                }
            }

            // Tool call deltas.
            if let Some(arr) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in arr {
                    let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.ensure_open(OpenBlock::Tool(idx), &mut out);
                    // Block index for this tool.
                    let block_index = self
                        .state
                        .tool_blocks
                        .get(&idx)
                        .map(|b| b.block_index)
                        .unwrap_or(0);
                    let mut emit_start = false;
                    if let Some(b) = self.state.tool_blocks.get_mut(&idx) {
                        if !b.started {
                            emit_start = true;
                            b.started = true;
                        }
                    }
                    let id = tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("toolu_{}", Uuid::new_v4().simple()));
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if emit_start {
                        let evt = json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
                        });
                        out.push(format_sse("content_block_start", &evt));
                        if let Some(b) = self.state.tool_blocks.get_mut(&idx) {
                            if !name.is_empty() {
                                b.name_emitted = true;
                            }
                        }
                    }
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                    {
                        if !args.is_empty() {
                            let evt = json!({
                                "type": "content_block_delta",
                                "index": block_index,
                                "delta": {"type": "input_json_delta", "partial_json": args}
                            });
                            out.push(format_sse("content_block_delta", &evt));
                        }
                    }
                }
            }

            if let Some(reason) = ch.get("finish_reason").and_then(|v| v.as_str()) {
                self.state.stop_reason = match reason {
                    "stop" => "end_turn".into(),
                    "length" => "max_tokens".into(),
                    "tool_calls" => "tool_use".into(),
                    other => other.to_string(),
                };
            }
        }

        out
    }

    fn current_index(&self) -> u32 {
        match self.state.open {
            Some(OpenBlock::Tool(i)) => self
                .state
                .tool_blocks
                .get(&i)
                .map(|b| b.block_index)
                .unwrap_or(self.state.next_index.saturating_sub(1)),
            Some(_) => self.state.next_index.saturating_sub(1),
            None => 0,
        }
    }

    fn ensure_open(&mut self, kind: OpenBlock, out: &mut Vec<String>) {
        let same = match (self.state.open, kind) {
            (Some(OpenBlock::Thinking), OpenBlock::Thinking) => true,
            (Some(OpenBlock::Text), OpenBlock::Text) => true,
            (Some(OpenBlock::Tool(a)), OpenBlock::Tool(b)) => a == b,
            _ => false,
        };
        if same {
            return;
        }
        self.close_open(out);
        match kind {
            OpenBlock::Thinking => {
                let idx = self.state.next_index;
                self.state.next_index += 1;
                let evt = json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {"type": "thinking", "thinking": ""}
                });
                out.push(format_sse("content_block_start", &evt));
                self.state.open = Some(OpenBlock::Thinking);
            }
            OpenBlock::Text => {
                let idx = self.state.next_index;
                self.state.next_index += 1;
                let evt = json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {"type": "text", "text": ""}
                });
                out.push(format_sse("content_block_start", &evt));
                self.state.open = Some(OpenBlock::Text);
            }
            OpenBlock::Tool(tool_idx) => {
                let idx = self.state.next_index;
                self.state.next_index += 1;
                self.state.tool_blocks.insert(
                    tool_idx,
                    ToolBlock {
                        block_index: idx,
                        started: false,
                        name_emitted: false,
                    },
                );
                self.state.open = Some(OpenBlock::Tool(tool_idx));
                // The actual content_block_start is emitted by the caller
                // when the first tool delta carries an `id`/`name`.
            }
        }
    }

    fn close_open(&mut self, out: &mut Vec<String>) {
        if let Some(open) = self.state.open.take() {
            let idx = match open {
                OpenBlock::Tool(i) => self
                    .state
                    .tool_blocks
                    .get(&i)
                    .map(|b| b.block_index)
                    .unwrap_or(self.state.next_index.saturating_sub(1)),
                _ => self.state.next_index.saturating_sub(1),
            };
            // Extended-thinking blocks must carry a signature, emitted as a
            // signature_delta just before content_block_stop. mimo provides no
            // real signature, so we send a clearly-marked placeholder.
            if matches!(open, OpenBlock::Thinking) {
                let sig = json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {"type": "signature_delta", "signature": THINKING_SIGNATURE_PLACEHOLDER},
                });
                out.push(format_sse("content_block_delta", &sig));
            }
            let evt = json!({"type": "content_block_stop", "index": idx});
            out.push(format_sse("content_block_stop", &evt));
        }
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        if self.state.finished {
            return;
        }
        self.close_open(out);
        if !self.state.started {
            // Empty stream: emit a synthetic start so the client sees a valid
            // sequence.
            let evt = json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_{}", Uuid::new_v4().simple()),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            });
            out.push(format_sse("message_start", &evt));
        }
        let stop = if self.state.stop_reason.is_empty() {
            "end_turn".to_string()
        } else {
            self.state.stop_reason.clone()
        };
        let evt = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": {
                "input_tokens": self.state.input_tokens,
                "output_tokens": self.state.output_tokens,
            }
        });
        out.push(format_sse("message_delta", &evt));
        out.push(format_sse("message_stop", &json!({"type": "message_stop"})));
        self.usage.record(
            &self.model,
            self.state.input_tokens as i64,
            self.state.output_tokens as i64,
            (self.state.input_tokens + self.state.output_tokens) as i64,
        );
        self.state.finished = true;
    }
}

fn format_sse(event: &str, payload: &Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

impl Stream for SseTranslator {
    type Item = std::result::Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // Drain any complete SSE events first.
            if let Some(data) = this.pop_event() {
                let translated = this.translate(&data);
                if !translated.is_empty() {
                    return Poll::Ready(Some(Ok(Bytes::from(translated.concat()))));
                }
                continue;
            }
            // Fetch more upstream bytes.
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    let text = this.decoder.push(&chunk);
                    this.buf.push_str(&text);
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(std::io::Error::other(e))));
                }
                Poll::Ready(None) => {
                    if !this.state.finished {
                        let mut tail = Vec::new();
                        this.finish(&mut tail);
                        return Poll::Ready(Some(Ok(Bytes::from(tail.concat()))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: feed a sequence of OpenAI-style chunks into the translator
    /// and return the concatenated Anthropic SSE output.
    fn run(chunks: &[Value]) -> String {
        run_with(chunks, true)
    }

    fn run_with(chunks: &[Value], thinking_enabled: bool) -> String {
        let model = "mimo-omni".to_string();
        // The Stream impl drives via `pop_event` over a buffer of bytes;
        // for a unit test we exercise `translate` and `finish` directly.
        let inner = futures::stream::empty().boxed();
        let dir = std::env::temp_dir().join(format!(
            "mb-anthropic-{}-{}",
            std::process::id(),
            thinking_enabled
        ));
        let storage = crate::storage::Storage::for_paths(dir.join("c"), dir.join("d")).unwrap();
        let usage = crate::usage::UsageStore::load(storage);
        let mut t = SseTranslator::new(inner, model, usage, thinking_enabled);
        let mut out: Vec<String> = Vec::new();
        for c in chunks {
            out.extend(t.translate(&c.to_string()));
        }
        t.finish(&mut out);
        let _ = std::fs::remove_dir_all(&dir);
        out.concat()
    }

    fn count_event(haystack: &str, needle: &str) -> usize {
        haystack.matches(&format!("event: {needle}\n")).count()
    }

    #[test]
    fn anthropic_text_stream_basic() {
        // mimo emits: empty role chunk → reasoning_content delta → content delta → finish
        let chunks = vec![
            json!({
                "id": "abc", "model": "mimo",
                "choices": [{"delta": {"role": "assistant", "content": ""}, "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "abc",
                "choices": [{"delta": {"reasoning_content": "thinking..."}, "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "abc",
                "choices": [{"delta": {"content": "hello"}, "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "abc",
                "choices": [{"delta": {}, "finish_reason": "stop", "index": 0}],
            }),
            json!({
                "id": "abc",
                "choices": [],
                "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13},
            }),
        ];
        let out = run(&chunks);

        // Required event sequence
        assert_eq!(count_event(&out, "message_start"), 1);
        assert_eq!(count_event(&out, "message_stop"), 1);
        assert_eq!(count_event(&out, "message_delta"), 1);

        // Two blocks: thinking + text
        assert_eq!(count_event(&out, "content_block_start"), 2);
        assert_eq!(count_event(&out, "content_block_stop"), 2);

        // Deltas of each kind exist
        assert!(out.contains("\"type\":\"thinking_delta\""));
        assert!(out.contains("\"thinking\":\"thinking...\""));
        assert!(out.contains("\"type\":\"text_delta\""));
        assert!(out.contains("\"text\":\"hello\""));

        // stop_reason mapping: openai "stop" → anthropic "end_turn"
        assert!(out.contains("\"stop_reason\":\"end_turn\""));

        // usage propagated
        assert!(out.contains("\"input_tokens\":10"));
        assert!(out.contains("\"output_tokens\":3"));
    }

    #[test]
    fn anthropic_tool_use_stream() {
        let chunks = vec![
            json!({
                "id": "tx", "choices": [{
                    "delta": {"role": "assistant",
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "lookup", "arguments": "{\"q\""}
                        }]},
                    "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "tx", "choices": [{
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "function": {"arguments": ":\"hi\"}"}
                    }]},
                    "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "tx", "choices": [{
                    "delta": {}, "finish_reason": "tool_calls", "index": 0}],
            }),
        ];
        let out = run(&chunks);
        assert!(out.contains("\"type\":\"tool_use\""));
        assert!(out.contains("\"id\":\"call_1\""));
        assert!(out.contains("\"name\":\"lookup\""));
        assert!(out.contains("\"type\":\"input_json_delta\""));
        // openai "tool_calls" → anthropic "tool_use"
        assert!(out.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn anthropic_to_openai_request_strips_anthropic_prefix() {
        let body = json!({
            "model": "anthropic/mimo-omni",
            "system": "you are helpful",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 32,
        });
        let openai = anthropic_to_openai_chat(&body).expect("ok");
        assert_eq!(openai["model"].as_str(), Some("mimo-omni"));
        let msgs = openai["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"].as_str(), Some("system"));
        assert_eq!(msgs[1]["role"].as_str(), Some("user"));
        assert_eq!(openai["max_tokens"].as_i64(), Some(32));
    }

    #[test]
    fn anthropic_tool_blocks_preserved_in_request() {
        let body = json!({
            "model": "mimo-omni",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "fn", "input": {"a": 1}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": "42"}
                ]}
            ],
            "tools": [{"name": "fn", "description": "x", "input_schema": {"type": "object"}}]
        });
        let openai = anthropic_to_openai_chat(&body).expect("ok");
        let msgs = openai["messages"].as_array().unwrap();
        // tool message comes first, then assistant tool_calls
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert!(roles.contains(&"tool"));
        assert!(roles.contains(&"assistant"));
        // tools mapped to OpenAI function-style
        let tools = openai["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"].as_str(), Some("function"));
        assert_eq!(tools[0]["function"]["name"].as_str(), Some("fn"));
    }

    /// chunks carrying reasoning_content + content + finish, reused by the
    /// thinking on/off tests below.
    fn thinking_chunks() -> Vec<Value> {
        vec![
            json!({
                "id": "th", "model": "mimo",
                "choices": [{"delta": {"role": "assistant", "content": ""}, "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "th",
                "choices": [{"delta": {"reasoning_content": "let me think"}, "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "th",
                "choices": [{"delta": {"content": "answer"}, "finish_reason": null, "index": 0}],
            }),
            json!({
                "id": "th",
                "choices": [{"delta": {}, "finish_reason": "stop", "index": 0}],
            }),
        ]
    }

    #[test]
    fn thinking_disabled_emits_no_thinking() {
        let out = run_with(&thinking_chunks(), false);
        // No thinking block at all when the client did not request thinking.
        assert!(!out.contains("\"type\":\"thinking_delta\""));
        assert!(!out.contains("\"type\":\"thinking\""));
        assert!(!out.contains("\"type\":\"signature_delta\""));
        // The visible text still flows through.
        assert!(out.contains("\"type\":\"text_delta\""));
        assert!(out.contains("\"text\":\"answer\""));
        // Only the text block is opened/closed.
        assert_eq!(count_event(&out, "content_block_start"), 1);
        assert_eq!(count_event(&out, "content_block_stop"), 1);
    }

    #[test]
    fn thinking_enabled_emits_signature() {
        let out = run_with(&thinking_chunks(), true);
        // Thinking deltas present, and a signature_delta closes the block.
        assert!(out.contains("\"type\":\"thinking_delta\""));
        assert!(out.contains("\"thinking\":\"let me think\""));
        assert!(out.contains("\"type\":\"signature_delta\""));
        assert!(out.contains(THINKING_SIGNATURE_PLACEHOLDER));
        // Both thinking + text blocks present.
        assert_eq!(count_event(&out, "content_block_start"), 2);
        assert_eq!(count_event(&out, "content_block_stop"), 2);
    }
}
