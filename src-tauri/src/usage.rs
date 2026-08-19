//! Per-model token usage accounting and windowed bucketing for the dashboard.
//!
//! Each proxied request that yields a `usage` object is recorded as a raw event
//! `{ ts, model, prompt, completion, total }`. Events older than ~31 days are
//! pruned. Queries bucket events into fixed windows (1h / 1d / 7d / 30d) so the
//! WebUI can draw a per-model token bar chart.

use crate::storage::Storage;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const BLOB: &str = "usage";
const MAX_EVENTS: usize = 200_000;
const RETAIN_MS: i64 = 31 * 24 * 60 * 60 * 1000;
/// The blob grows to megabytes over weeks of uptime; rewriting it (and
/// cloning the whole event vec) on EVERY request used to dominate the
/// bridge's allocator churn and disk wear. Flush at most this often —
/// at most a few seconds of stats are lost on an unclean exit.
const FLUSH_INTERVAL_MS: i64 = 5_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub ts: i64,
    pub model: String,
    pub prompt: i64,
    pub completion: i64,
    pub total: i64,
}

pub struct UsageStore {
    storage: Arc<Storage>,
    events: Mutex<Vec<UsageEvent>>,
    last_flush: Mutex<i64>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl UsageStore {
    pub fn load(storage: Arc<Storage>) -> Arc<Self> {
        let events = storage
            .load_blob::<Vec<UsageEvent>>(BLOB)
            .ok()
            .flatten()
            .unwrap_or_default();
        Arc::new(Self {
            storage,
            events: Mutex::new(events),
            // 0 → the first record after boot flushes immediately, so
            // low-traffic deployments still see fresh stats on disk.
            last_flush: Mutex::new(0),
        })
    }

    /// Record one request's token usage. No-op when all counts are zero.
    pub fn record(&self, model: &str, prompt: i64, completion: i64, total: i64) {
        if prompt == 0 && completion == 0 && total == 0 {
            return;
        }
        let mut events = self.events.lock();
        events.push(UsageEvent {
            ts: now_ms(),
            model: if model.is_empty() {
                "unknown".to_string()
            } else {
                model.to_string()
            },
            prompt,
            completion,
            total: if total > 0 {
                total
            } else {
                prompt + completion
            },
        });
        let cutoff = now_ms() - RETAIN_MS;
        events.retain(|e| e.ts >= cutoff);
        if events.len() > MAX_EVENTS {
            let drop = events.len() - MAX_EVENTS;
            events.drain(0..drop);
        }
        let mut last_flush = self.last_flush.lock();
        let now = now_ms();
        if now - *last_flush >= FLUSH_INTERVAL_MS {
            *last_flush = now;
            // Serialize straight from the locked vec — no full-clone spike.
            let _ = self.storage.save_blob(BLOB, &*events);
        }
    }

    /// Bucket usage for a window. `window` ∈ {"1h","1d","7d","30d"}.
    pub fn query(&self, window: &str) -> Value {
        let (range_ms, bucket_ms) = match window {
            "1h" => (3_600_000i64, 300_000i64),   // 12 × 5 min
            "7d" => (604_800_000, 86_400_000),    // 7 × 1 day
            "30d" => (2_592_000_000, 86_400_000), // 30 × 1 day
            _ => (86_400_000, 3_600_000),         // 1d: 24 × 1 hour
        };
        let now = now_ms();
        let start = now - range_ms;
        let n = (range_ms / bucket_ms) as usize;

        // per bucket: model -> [prompt, completion, total]
        let mut buckets: Vec<BTreeMap<String, [i64; 3]>> = vec![BTreeMap::new(); n];
        let mut model_totals: BTreeMap<String, i64> = BTreeMap::new();
        let mut grand_total = 0i64;

        for e in self.events.lock().iter() {
            if e.ts < start {
                continue;
            }
            let mut idx = ((e.ts - start) / bucket_ms) as usize;
            if idx >= n {
                idx = n - 1;
            }
            let slot = buckets[idx].entry(e.model.clone()).or_insert([0, 0, 0]);
            slot[0] += e.prompt;
            slot[1] += e.completion;
            slot[2] += e.total;
            *model_totals.entry(e.model.clone()).or_insert(0) += e.total;
            grand_total += e.total;
        }

        let bucket_json: Vec<Value> = buckets
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                let t = start + (i as i64) * bucket_ms;
                let total: i64 = m.values().map(|v| v[2]).sum();
                let mut models = Map::new();
                for (k, v) in m {
                    models.insert(
                        k,
                        json!({ "prompt": v[0], "completion": v[1], "total": v[2] }),
                    );
                }
                json!({ "t": t, "total": total, "models": Value::Object(models) })
            })
            .collect();

        json!({
            "window": window,
            "bucket_seconds": bucket_ms / 1000,
            "start": start,
            "end": now,
            "grand_total": grand_total,
            "model_totals": model_totals,
            "buckets": bucket_json,
        })
    }
}

/// Extract `(prompt, completion, total)` from a chat / responses / anthropic
/// JSON value's `usage` object. Returns `None` when nothing usable is present.
pub fn usage_from_value(v: &Value) -> Option<(i64, i64, i64)> {
    let u = v.get("usage")?;
    let get = |k: &str| u.get(k).and_then(|x| x.as_i64());
    // OpenAI chat:      prompt_tokens / completion_tokens / total_tokens
    // OpenAI responses: input_tokens  / output_tokens     / total_tokens
    // Anthropic:        input_tokens  / output_tokens
    let prompt = get("prompt_tokens")
        .or_else(|| get("input_tokens"))
        .unwrap_or(0);
    let completion = get("completion_tokens")
        .or_else(|| get("output_tokens"))
        .unwrap_or(0);
    let total = get("total_tokens").unwrap_or(prompt + completion);
    if prompt == 0 && completion == 0 && total == 0 {
        None
    } else {
        Some((prompt, completion, total))
    }
}

/// Scans a (possibly streamed) upstream body for the final `usage` totals.
///
/// - SSE: parses each `data:` packet incrementally and keeps the last usage
///   it sees (mimo emits a trailing chunk carrying `usage`).
/// - Non-SSE: accumulates the body (capped) and parses once at the end.
pub struct UsageScanner {
    sse: bool,
    decoder: crate::decode::Utf8Stream,
    buf: String,
    last: Option<(i64, i64, i64)>,
    capped: bool,
}

impl UsageScanner {
    pub fn new(sse: bool) -> Self {
        Self {
            sse,
            decoder: crate::decode::Utf8Stream::new(),
            buf: String::new(),
            last: None,
            capped: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        let text = self.decoder.push(chunk);
        self.buf.push_str(&text);
        if self.sse {
            loop {
                let lf = self.buf.find("\n\n");
                let crlf = self.buf.find("\r\n\r\n");
                let (idx, sep) = match (lf, crlf) {
                    (Some(a), Some(b)) if a <= b => (a, 2),
                    (Some(_), Some(b)) => (b, 4),
                    (Some(a), None) => (a, 2),
                    (None, Some(b)) => (b, 4),
                    (None, None) => break,
                };
                let packet = self.buf[..idx].to_string();
                self.buf.drain(..idx + sep);
                let data: String = packet
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                    if let Some(u) = usage_from_value(&v) {
                        self.last = Some(u);
                    }
                }
            }
        } else if !self.capped && self.buf.len() > 4 * 1024 * 1024 {
            // Non-streaming bodies should be small; give up if absurdly large.
            self.capped = true;
            self.buf.clear();
        }
    }

    pub fn finish(mut self) -> Option<(i64, i64, i64)> {
        if !self.sse && !self.capped {
            if let Ok(v) = serde_json::from_str::<Value>(&self.buf) {
                if let Some(u) = usage_from_value(&v) {
                    self.last = Some(u);
                }
            }
        }
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_parses_openai_json_usage() {
        let mut s = UsageScanner::new(false);
        s.feed(br#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#);
        assert_eq!(s.finish(), Some((10, 5, 15)));
    }

    #[test]
    fn scanner_parses_sse_trailing_usage() {
        let mut s = UsageScanner::new(true);
        // streamed in arbitrary chunks; usage arrives in the final data packet
        s.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        s.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,");
        s.feed(b"\"completion_tokens\":7,\"total_tokens\":10}}\n\ndata: [DONE]\n\n");
        assert_eq!(s.finish(), Some((3, 7, 10)));
    }

    #[test]
    fn usage_from_value_handles_anthropic_and_responses() {
        let anthropic = serde_json::json!({"usage": {"input_tokens": 4, "output_tokens": 6}});
        assert_eq!(usage_from_value(&anthropic), Some((4, 6, 10)));
        let responses = serde_json::json!({"usage": {"input_tokens": 4, "output_tokens": 6, "total_tokens": 12}});
        assert_eq!(usage_from_value(&responses), Some((4, 6, 12)));
    }

    #[test]
    fn store_buckets_window_counts_and_totals() {
        let dir = std::env::temp_dir().join(format!("mb-usage-{}", std::process::id()));
        let cfg = dir.join("c");
        let data = dir.join("d");
        let storage = crate::storage::Storage::for_paths(cfg, data).unwrap();
        let store = UsageStore::load(storage);
        store.record("xiaomi/mimo", 10, 20, 30);
        store.record("xiaomi/mimo", 1, 2, 3);
        store.record("xiaomi/mimo-pro", 5, 5, 10);

        let v = store.query("1d");
        assert_eq!(v["bucket_seconds"], 3600);
        assert_eq!(v["buckets"].as_array().unwrap().len(), 24);
        assert_eq!(v["grand_total"], 43);
        assert_eq!(v["model_totals"]["xiaomi/mimo"], 33);
        assert_eq!(v["model_totals"]["xiaomi/mimo-pro"], 10);

        let h = store.query("1h");
        assert_eq!(h["buckets"].as_array().unwrap().len(), 12);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
