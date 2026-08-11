//! pi-agent 会话日志（JSONL）解析。
//!
//! 数据来源：`~/.pi/agent/sessions/<encoded-cwd>/<isoTime>_<sessionUUID>.jsonl`
//! （可用 `PI_AGENT_DIR` 环境变量覆盖，逗号分隔多个根目录）
//!
//! 每一行是一个 JSON 对象：
//! - `type="session"`：首行元信息，忽略
//! - `type="model_change"`：记录当前 provider / modelId
//! - `type="message"`：assistant 消息，带 `message.usage` 时计入统计
//!
//! 字段语义与 Tokdash 的 PiAgentParser 保持一致：
//! - `usage.input`      —— 新输入 token（不含缓存）
//! - `usage.output`     —— 输出 token
//! - `usage.cacheRead`  —— 缓存命中读取 token
//! - `usage.cacheWrite` —— 缓存写入 token（按计费输入计入）
//! - `usage.totalTokens`—— 兜底字段：细分全为 0 且其 > 0 时全部记入输出
//! - `usage.cost.total` —— 费用（可选）

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead};
use std::path::Path;

use chrono::{DateTime, Utc};

/// 一条归一化后的用量记录（对应 Tokdash entry）。
#[derive(Debug, Clone)]
pub struct Entry {
    /// 来源（pi_agent / codex / claude / opencode / ...）
    pub source: String,
    pub ts_ms: i64,
    pub model: String,
    pub provider: String,
    /// 新输入 token（不含缓存）
    pub input: u64,
    /// 输出 token
    pub output: u64,
    /// 缓存命中读取 token
    pub cache_read: u64,
    /// 缓存写入 token
    pub cache_write: u64,
    /// 推理 token（pi 日志暂无，保留字段）
    pub reasoning: u64,
    /// 费用（日志自带时才有，否则 0）
    pub cost: f64,
}

impl Entry {
    /// 计费输入 = 新输入 + 缓存写入（Tokdash 语义）
    pub fn tokens_in(&self) -> u64 {
        self.input + self.cache_write
    }
    /// 消息总 token = 输入 + 输出 + 缓存命中 + 推理
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in() + self.output + self.cache_read + self.reasoning
    }
}

/// 扫描目录下所有 `*.jsonl` 会话日志并解析为条目列表。
/// 去重集合跨文件共享（同一会话 resume 后消息会复制到新文件，不能重复计数）。
pub fn parse_stdin() -> io::Result<Vec<Entry>> {
    let stdin = io::stdin();
    let mut entries = Vec::new();
    let mut cur_model = String::new();
    let mut cur_provider = String::new();
    let mut cur_session = String::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        parse_line(&obj, &mut entries, &mut cur_model, &mut cur_provider, &mut cur_session, &mut seen);
    }
    Ok(entries)
}

#[allow(dead_code)]
fn parse_file(path: &Path, seen: &mut HashSet<(String, String)>) -> Vec<Entry> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut entries = Vec::new();
    let mut cur_model = String::new();
    let mut cur_provider = String::new();
    let mut cur_session = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        parse_line(&obj, &mut entries, &mut cur_model, &mut cur_provider, &mut cur_session, seen);
    }
    entries
}

fn parse_line(
    obj: &serde_json::Value,
    entries: &mut Vec<Entry>,
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

    // 时间戳：ISO-8601（可能带 Z）→ epoch ms
    let ts_raw = match obj.get("timestamp").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };
    let ts_ms = match parse_iso_ms(ts_raw) {
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
    let output = get_u64(usage, "output");
    let cache_read = get_u64(usage, "cacheRead");
    let cache_write = get_u64(usage, "cacheWrite");
    let total = get_u64(usage, "totalTokens");

    // 兜底：细分全为 0 但 totalTokens > 0 → 全部记入输出（同 Tokdash/ccusage）
    let mut output = output;
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 && total > 0 {
        output = total;
    }
    // 跳过完全空的行
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
        return;
    }

    let cost = usage
        .get("cost")
        .and_then(|v| v.as_object())
        .and_then(|c| c.get("total"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    entries.push(Entry {
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

fn get_u64(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn parse_iso_ms(s: &str) -> Option<i64> {
    let normalized = s.strip_suffix('Z').map(|t| format!("{t}+00:00")).unwrap_or_else(|| s.to_string());
    let dt: DateTime<Utc> = DateTime::parse_from_rfc3339(&normalized).ok()?.with_timezone(&Utc);
    Some(dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_total_tokens_semantics() {
        let e = Entry {
            source: "pi_agent".into(),
            ts_ms: 0,
            model: "m".into(),
            provider: String::new(),
            input: 100,
            output: 50,
            cache_read: 30,
            cache_write: 10,
            reasoning: 0,
            cost: 0.0,
        };
        // 输入 = 100 + 10 = 110；总 = 110 + 50 + 30 = 190
        assert_eq!(e.tokens_in(), 110);
        assert_eq!(e.total_tokens(), 190);
    }

    #[test]
    fn test_parse_line_basic() {
        let line = r#"{"type":"message","id":"abc12345","timestamp":"2026-07-11T12:20:28.607Z","message":{"role":"assistant","model":"hy3-free","usage":{"input":8543,"output":22,"cacheRead":1664,"cacheWrite":0,"totalTokens":10436,"cost":{"total":0.001}}}}"#;
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut entries = Vec::new();
        let mut cm = String::new();
        let mut cp = String::new();
        let mut cs = String::new();
        let mut seen = HashSet::new();
        parse_line(&obj, &mut entries, &mut cm, &mut cp, &mut cs, &mut seen);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.input, 8543);
        assert_eq!(e.output, 22);
        assert_eq!(e.cache_read, 1664);
        assert_eq!(e.total_tokens(), 8543 + 0 + 22 + 1664);
        assert!((e.cost - 0.001).abs() < 1e-9);
    }

    #[test]
    fn test_total_tokens_fallback() {
        let line = r#"{"type":"message","id":"abc12346","timestamp":"2026-07-11T12:20:28.607Z","message":{"role":"assistant","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":999}}}"#;
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut entries = Vec::new();
        let mut cm = String::new();
        let mut cp = String::new();
        let mut cs = String::new();
        let mut seen = HashSet::new();
        parse_line(&obj, &mut entries, &mut cm, &mut cp, &mut cs, &mut seen);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].output, 999);
    }

    #[test]
    fn test_skip_non_assistant() {
        let line = r#"{"type":"message","id":"abc12347","timestamp":"2026-07-11T12:20:28.607Z","message":{"role":"user","usage":{"input":1}}}"#;
        let obj: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut entries = Vec::new();
        let mut cm = String::new();
        let mut cp = String::new();
        let mut cs = String::new();
        let mut seen = HashSet::new();
        parse_line(&obj, &mut entries, &mut cm, &mut cp, &mut cs, &mut seen);
        assert!(entries.is_empty());
    }
}
