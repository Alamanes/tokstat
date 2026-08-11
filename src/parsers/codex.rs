//! Codex CLI 会话日志解析器。
//!
//! 数据来源：`~/.codex/sessions/**/*.jsonl`
//!
//! 行类型：
//! - `type="turn_context"` + `payload.model` → 更新当前模型
//! - `type="session_meta"` + `payload.id` → 更新会话 id；`payload.model_provider` → provider
//! - `type="event_msg"` + `payload.type="token_count"` → 计数行
//!
//! 计数行取 `payload.info.last_token_usage`（per-turn 增量，非累计）：
//! - `input_tokens` 含缓存 → 减去 `cached_input_tokens` 得新输入
//! - `cached_input_tokens` → cacheRead
//! - `output_tokens` → output；`reasoning_output_tokens` → reasoning
//!
//! 去重：对 (会话 id, 规范化 usage 快照) 做内容哈希（对应 Tokdash 的
//! `codex_token_event_key`，防止 resume 后的历史副本重复计数）。


use std::path::Path;

use serde_json::Value;

use crate::parser::Entry;
use crate::parsers::{collect_files_multi, content_hash, get_u64, iso_to_ms};

const FIELDS: [&str; 5] = [
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
];

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    // 事件去重键（跨文件）
    let mut index_by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    if let Some(r) = root {
        if r.is_file() {
            parse_one_file(r, &mut out, &mut index_by_key);
            return out;
        }
    }

    let roots: Vec<std::path::PathBuf> = match root {
        Some(r) => vec![r.to_path_buf()],
        None => match std::env::var_os("HOME") {
            Some(h) => vec![std::path::Path::new(&h).join(".codex/sessions")],
            None => return Vec::new(),
        },
    };
    let files = collect_files_multi(&roots.iter().map(|p| p.as_path()).collect::<Vec<_>>(), "jsonl");

    for f in &files {
        parse_one_file(f, &mut out, &mut index_by_key);
    }
    out
}

fn parse_one_file(
    path: &Path,
    out: &mut Vec<Entry>,
    index_by_key: &mut std::collections::HashMap<String, usize>,
) {
    let lines = crate::parsers::read_jsonl_lines(path);
    let mut model = "gpt-5.3-codex".to_string();
    let mut provider = "openai".to_string();
    let mut own_session_id: Option<String> = None;
    let mut current_session_id: Option<String> = None;
    let mut subagent_parent_id: Option<String> = None;
    let mut is_subagent_file = false;

    for obj in &lines {
            let p = match obj.get("payload") {
                Some(v) if v.is_object() => v,
                _ => continue,
            };
            let t = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match t {
                "turn_context" => {
                    if let Some(m) = p.get("model").and_then(|v| v.as_str()) {
                        model = m.to_string();
                    }
                }
                "session_meta" => {
                    let sid = p.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if let Some(sid) = sid {
                        if own_session_id.is_none() {
                            own_session_id = Some(sid.clone());
                            // thread_spawn 子代理文件会回放祖先历史
                            let source = p.get("source");
                            let sub = source
                                .and_then(|s| s.get("subagent"))
                                .and_then(|s| s.get("thread_spawn"));
                            if sub.is_some() {
                                is_subagent_file = true;
                                subagent_parent_id = sub
                                    .and_then(|s| s.get("parent_thread_id"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                            }
                        }
                        current_session_id = Some(sid);
                    }
                    if let Some(mp) = p.get("model_provider").and_then(|v| v.as_str()) {
                        provider = mp.to_string();
                    }
                }
                "event_msg" => {
                    if p.get("type").and_then(|v| v.as_str()) != Some("token_count") {
                        continue;
                    }
                    // 子代理回放门控
                    if is_subagent_file {
                        if let (Some(own), Some(cur)) = (&own_session_id, &current_session_id) {
                            let is_replay = match &subagent_parent_id {
                                Some(pid) => cur == pid,
                                None => cur != own,
                            };
                            if is_replay {
                                continue;
                            }
                        }
                    }

                    let ts_raw = obj.get("timestamp").and_then(|v| v.as_str());
                    let ts_ms = match ts_raw.and_then(iso_to_ms) {
                        Some(ms) => ms,
                        None => continue,
                    };

                    let info = p.get("info").filter(|v| v.is_object());
                    let usage = info
                        .and_then(|i| i.get("last_token_usage"))
                        .filter(|v| v.is_object());
                    let usage = match usage {
                        Some(u) => u,
                        None => continue,
                    };

                    // 稳定去重键（规范化 usage 快照哈希）
                    let event_key = build_event_key(&current_session_id, info.unwrap());

                    let total_input = get_u64(usage.as_object().unwrap(), "input_tokens");
                    let cache_read = get_u64(usage.as_object().unwrap(), "cached_input_tokens");
                    let input_t = total_input.saturating_sub(cache_read);
                    let output_t = get_u64(usage.as_object().unwrap(), "output_tokens");
                    let reasoning = get_u64(usage.as_object().unwrap(), "reasoning_output_tokens");

                    if input_t == 0 && output_t == 0 && cache_read == 0 && reasoning == 0 {
                        continue;
                    }

                    let entry = Entry {
                        source: "codex".into(),
                        ts_ms,
                        model: model.clone(),
                        provider: provider.clone(),
                        input: input_t,
                        output: output_t,
                        cache_read,
                        cache_write: 0,
                        reasoning,
                        cost: 0.0,
                    };

                    if let Some(key) = event_key {
                        if let Some(&idx) = index_by_key.get(&key) {
                            if entry.ts_ms < out[idx].ts_ms {
                                out[idx] = entry;
                            }
                            continue;
                        }
                        index_by_key.insert(key, out.len());
                    }
                    out.push(entry);
                }
                _ => {}
            }
        }
}

/// 对应 Tokdash `codex_token_event_key`：规范化 total/last 快照后做内容哈希。
fn build_event_key(session_id: &Option<String>, info: &Value) -> Option<String> {
    let sid = session_id.clone().unwrap_or_default();
    if sid.is_empty() {
        return None;
    }
    let total = info.get("total_token_usage").filter(|v| v.is_object())?;
    let last = info.get("last_token_usage").filter(|v| v.is_object())?;

    let mut parts = vec!["v1".to_string(), sid];
    for u in [total, last] {
        let mut vals = Vec::new();
        for f in FIELDS {
            let v = u.get(f).and_then(|v| v.as_u64()).unwrap_or(0);
            vals.push(v.to_string());
        }
        parts.push(vals.join(","));
    }
    Some(content_hash(&parts.iter().map(|s| s.as_str()).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_codex_test_{}.jsonl", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn test_parse_token_count() {
        let lines = r#"{"type":"session_meta","payload":{"id":"sess-1","model_provider":"openai"}}
{"type":"event_msg","timestamp":"2026-08-01T10:00:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1500,"cached_input_tokens":500,"output_tokens":200,"reasoning_output_tokens":50},"total_token_usage":{"input_tokens":1500,"cached_input_tokens":500,"output_tokens":200,"reasoning_output_tokens":50}}}}
"#;
        let p = write_tmp(lines);
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // input = 1500 - 500 = 1000；cacheRead = 500
        assert_eq!(e.input, 1000);
        assert_eq!(e.cache_read, 500);
        assert_eq!(e.output, 200);
        assert_eq!(e.reasoning, 50);
        assert_eq!(e.source, "codex");
    }

    #[test]
    fn test_duplicate_replay_dedup() {
        // 同一 usage 快照出现在两个文件 → 只计一次
        let lines = r#"{"type":"session_meta","payload":{"id":"sess-2"}}
{"type":"event_msg","timestamp":"2026-08-01T10:00:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0}}}}
"#;
        let mut p1 = std::env::temp_dir();
        p1.push(format!("tokstat_codex_a_{}.jsonl", std::process::id()));
        let mut p2 = std::env::temp_dir();
        p2.push(format!("tokstat_codex_b_{}.jsonl", std::process::id()));
        std::fs::write(&p1, lines).unwrap();
        std::fs::write(&p2, lines).unwrap();
        let entries = parse(Some(std::path::Path::new(
            std::env::temp_dir().to_str().unwrap(),
        )));
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
        let codex_entries: Vec<_> = entries.iter().filter(|e| e.source == "codex").collect();
        assert_eq!(codex_entries.len(), 1);
    }
}
