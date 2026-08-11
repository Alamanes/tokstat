//! pi-agent 会话日志解析器。
//!
//! 数据来源：`~/.pi/agent/sessions/<encoded-cwd>/<isoTime>_<sessionUUID>.jsonl`
//! （`PI_AGENT_DIR` 环境变量逗号分隔可覆盖）
//!
//! 行类型：
//! - `type="session"`：首行元信息，忽略
//! - `type="model_change"`：记录当前 provider / modelId
//! - `type="message"`：assistant 消息，带 `message.usage` 时计入
//!
//! 字段语义（同 Tokdash PiAgentParser）：
//! - `usage.input` / `output` / `cacheRead` / `cacheWrite` / `totalTokens`
//! - 兜底：细分全 0 但 totalTokens > 0 时全部记入输出

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::parser::Entry;
use crate::parsers::{get_u64, iso_to_ms};

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    // root 为文件时直接解析
    if let Some(r) = root {
        if r.is_file() {
            let mut entries = Vec::new();
            let mut seen: HashSet<(String, String)> = HashSet::new();
            parse_file(r, &mut seen, &mut entries);
            return entries;
        }
    }

    // 默认搜索目录：PI_AGENT_DIR → ~/.pi/agent/sessions
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(r) = root {
        roots.push(r.to_path_buf());
    } else if let Ok(env) = std::env::var("PI_AGENT_DIR") {
        roots = env
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        if roots.is_empty() {
            if let Some(home) = std::env::var_os("HOME") {
                roots.push(std::path::Path::new(&home).join(".pi/agent/sessions"));
            }
        }
    } else if let Some(home) = std::env::var_os("HOME") {
        roots.push(std::path::Path::new(&home).join(".pi/agent/sessions"));
    }

    let files = crate::parsers::collect_files_multi(&roots.iter().map(|p| p.as_path()).collect::<Vec<_>>(), "jsonl");
    let mut entries = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for f in &files {
        parse_file(f, &mut seen, &mut entries);
    }
    entries
}

fn parse_file(path: &Path, seen: &mut HashSet<(String, String)>, out: &mut Vec<Entry>) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut cur_model = String::new();
    let mut cur_provider = String::new();
    let mut cur_session = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        parse_line(&obj, out, &mut cur_model, &mut cur_provider, &mut cur_session, seen);
    }
}

fn parse_line(
    obj: &Value,
    out: &mut Vec<Entry>,
    cur_model: &mut String,
    cur_provider: &mut String,
    cur_session: &mut String,
    seen: &mut HashSet<(String, String)>,
) {
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "session" => {
            if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                *cur_session = id.to_string();
            }
            return;
        }
        "model_change" => {
            if let Some(p) = obj.get("provider").and_then(|v| v.as_str()) {
                *cur_provider = p.to_string();
            }
            if let Some(m) = obj.get("modelId").and_then(|v| v.as_str()) {
                *cur_model = m.to_string();
            }
            return;
        }
        "message" => {}
        _ => return,
    }

    let msg = match obj.get("message").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return,
    };
    if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return;
    }
    let usage = match msg.get("usage").and_then(|v| v.as_object()) {
        Some(u) => u,
        None => return,
    };

    // 去重：按 (会话 id, 消息外置 id)
    let entry_id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !entry_id.is_empty() {
        let key = (cur_session.clone(), entry_id.clone());
        if seen.contains(&key) {
            return;
        }
        seen.insert(key);
    }

    let ts_raw = match obj.get("timestamp").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };
    let ts_ms = match iso_to_ms(ts_raw) {
        Some(ms) => ms,
        None => return,
    };

    let model = msg
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cur_model.clone());
    let model = if model.is_empty() { "unknown".to_string() } else { model };

    let provider = msg
        .get("provider")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cur_provider.clone());

    let input = get_u64(usage, "input");
    let mut output = get_u64(usage, "output");
    let cache_read = get_u64(usage, "cacheRead");
    let cache_write = get_u64(usage, "cacheWrite");
    let total = get_u64(usage, "totalTokens");

    // 兜底：细分全为 0 但 totalTokens > 0 → 全部记入输出
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 && total > 0 {
        output = total;
    }
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
        return;
    }

    let cost = usage
        .get("cost")
        .and_then(|v| v.as_object())
        .and_then(|c| c.get("total"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    out.push(Entry {
        source: "pi_agent".into(),
        ts_ms,
        model,
        provider,
        input,
        output,
        cache_read,
        cache_write,
        reasoning: 0,
        cost: cost.max(0.0),
    });
}
