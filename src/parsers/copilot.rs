//! GitHub Copilot CLI 解析器。
//!
//! 数据源 A（首选）：OTel JSONL `~/.copilot/otel/*.jsonl` +
//! `COPILOT_OTEL_FILE_EXPORTER_PATH` 单文件。四种候选记录类型：
//! ChatSpan（span + gen_ai.operation.name=chat / name 以 "chat " 开头）、
//! InferenceLog（event.name=gen_ai.client.inference.operation.details 或 body 前缀）、
//! AgentTurnLog（event.name=copilot_chat.agent.turn）、
//! AgentSummarySpan（span + invoke_agent）。
//! 字段：gen_ai.usage.input_tokens（含缓存，减去 cache_read）、
//! gen_ai.usage.cache_read.input_tokens、cache_write/cache_creation、
//! gen_ai.usage.output_tokens、reasoning。模型：gen_ai.response.model /
//! gen_ai.request.model。去重：traceId + gen_ai.response.id。
//!
//! 数据源 B（fallback）：`~/.copilot/session-state/*/events.jsonl`，
//! `type == "assistant.message"` 仅 outputTokens。仅当 OTel 无数据时使用，
//! 避免双计。

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::parser::Entry;
use crate::parsers::{get_u64, scalar_to_ms};

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let otel_dir: std::path::PathBuf = match root {
        Some(r) => r.join("otel"),
        None => match std::env::var_os("HOME") {
            Some(h) => std::path::Path::new(&h).join(".copilot/otel"),
            None => return Vec::new(),
        },
    };

    // 收集 OTel 文件
    let mut otel_files: Vec<std::path::PathBuf> = Vec::new();
    if otel_dir.is_dir() {
        crate::parsers::collect_files(&otel_dir, "jsonl", &mut otel_files);
    }
    if let Ok(env) = std::env::var("COPILOT_OTEL_FILE_EXPORTER_PATH") {
        let p = std::path::PathBuf::from(env);
        if p.exists() {
            otel_files.push(p);
        }
    }
    otel_files.sort();
    otel_files.dedup();

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for f in &otel_files {
        let file_mtime = std::fs::metadata(f).and_then(|m| m.modified()).map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)).unwrap_or(0.0);
        for rec in crate::parsers::read_jsonl_lines(f) {
            let attrs = rec.get("attributes").and_then(|v| v.as_object());
            let is_span = rec.get("type").and_then(|v| v.as_str()) == Some("span")
                || (rec.get("name").is_some() && rec.as_object().map(|o| o.keys().any(|k| matches!(k.as_str(), "spanId" | "traceId" | "startTime" | "endTime" | "duration" | "kind"))).unwrap_or(false));
            let op_name = attrs.and_then(|a| a.get("gen_ai.operation.name")).and_then(|v| v.as_str()).unwrap_or("");
            let rec_name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let event_name = attrs.and_then(|a| a.get("event.name")).and_then(|v| v.as_str()).unwrap_or("");
            let body = rec.get("body").and_then(|v| v.as_str()).unwrap_or("");

            let is_candidate = (is_span && (op_name == "chat" || rec_name.starts_with("chat ")))
                || (!is_span && (event_name == "gen_ai.client.inference.operation.details" || body.starts_with("GenAI inference:")))
                || (!is_span && (event_name == "copilot_chat.agent.turn" || body.starts_with("copilot_chat.agent.turn")))
                || (is_span && (op_name == "invoke_agent" || rec_name.starts_with("invoke_agent ")));
            if !is_candidate {
                continue;
            }

            // 去重：traceId + response id
            let trace_id = rec.get("traceId").and_then(|v| v.as_str()).unwrap_or("");
            let resp_id = attrs
                .and_then(|a| a.get("gen_ai.response.id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let dedup = format!("{trace_id}|{resp_id}");
            if !seen.insert(dedup) {
                continue;
            }

            let attrs = match attrs {
                Some(a) => a,
                None => continue,
            };

            let raw_input = get_u64(attrs, "gen_ai.usage.input_tokens");
            let cache_r = get_u64(attrs, "gen_ai.usage.cache_read.input_tokens");
            let cache_w = get_u64(attrs, "gen_ai.usage.cache_write.input_tokens")
                .max(get_u64(attrs, "gen_ai.usage.cache_creation.input_tokens"));
            let reasoning = get_u64(attrs, "gen_ai.usage.reasoning.output_tokens")
                .max(get_u64(attrs, "gen_ai.usage.reasoning_tokens"));
            let mut output_t = get_u64(attrs, "gen_ai.usage.output_tokens");
            let total_t = get_u64(attrs, "gen_ai.usage.total_tokens")
                .max(get_u64(attrs, "gen_ai.usage.total.token_count"));
            let input_t = raw_input.saturating_sub(cache_r);
            // 兜底
            if input_t == 0 && output_t == 0 && cache_r == 0 && cache_w == 0 && total_t > 0 {
                output_t = total_t;
            }
            if input_t + output_t + cache_r + cache_w + reasoning == 0 {
                continue;
            }

            let model = attrs
                .get("gen_ai.response.model")
                .or_else(|| attrs.get("gen_ai.request.model"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let ts_ms = parse_otel_ts(&rec, file_mtime);
            let provider = infer_provider(&model);

            out.push(Entry {
                source: "copilot_cli".into(),
                ts_ms,
                model,
                provider,
                input: input_t,
                output: output_t,
                cache_read: cache_r,
                cache_write: cache_w,
                reasoning,
                cost: 0.0,
            });
        }
    }

    // ---- fallback：events.jsonl（仅当 OTel 无数据）----
    if out.is_empty() {
        if let Some(events) = events_dir(root) {
            let mut files = Vec::new();
            crate::parsers::collect_files(&events, "jsonl", &mut files);
            files.sort();
            for f in &files {
                for obj in crate::parsers::read_jsonl_lines(f) {
                    if obj.get("type").and_then(|v| v.as_str()) != Some("assistant.message") {
                        continue;
                    }
                    let output_t = obj
                        .get("outputTokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if output_t == 0 {
                        continue;
                    }
                    let ts_ms = obj
                        .get("timestamp")
                        .and_then(scalar_to_ms)
                        .unwrap_or(0);
                    out.push(Entry {
                        source: "copilot_cli".into(),
                        ts_ms,
                        model: "unknown".into(),
                        provider: "copilot".into(),
                        input: 0,
                        output: output_t,
                        cache_read: 0,
                        cache_write: 0,
                        reasoning: 0,
                        cost: 0.0,
                    });
                }
            }
        }
    }

    out
}

fn events_dir(root: Option<&Path>) -> Option<std::path::PathBuf> {
    let base = match root {
        Some(r) => r.to_path_buf(),
        None => std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".copilot"))?,
    };
    let d = base.join("session-state");
    d.is_dir().then_some(d)
}

/// OTel 时间戳：优先 [秒, 纳秒] 数组，其次标量自动缩放，再 timeUnixNano，最后文件 mtime。
fn parse_otel_ts(rec: &Value, file_mtime: f64) -> i64 {
    for key in ["endTime", "startTime", "hrTime", "_hrTime"] {
        if let Some(v) = rec.get(key) {
            if let Some(arr) = v.as_array() {
                if arr.len() == 2 {
                    if let (Some(secs), Some(nanos)) = (arr[0].as_i64(), arr[1].as_i64()) {
                        return secs * 1000 + nanos / 1_000_000;
                    }
                }
            }
        }
    }
    for key in ["time", "timestamp", "observedTimestamp"] {
        if let Some(v) = rec.get(key) {
            if let Some(ms) = scalar_to_ms(v) {
                return ms;
            }
        }
    }
    if let Some(v) = rec.get("timeUnixNano").and_then(|v| v.as_i64()) {
        return v / 1_000_000;
    }
    (file_mtime * 1000.0) as i64
}

fn infer_provider(model: &str) -> String {
    let m = model.to_lowercase();
    if m.starts_with("claude") {
        "anthropic".into()
    } else if m.starts_with("gemini") {
        "google".into()
    } else if m.starts_with("gpt") || m.starts_with('o') || m.contains("chatgpt") {
        "openai".into()
    } else {
        "copilot".into()
    }
}
