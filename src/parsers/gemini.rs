//! Gemini CLI 会话解析器。
//!
//! 数据来源：`~/.gemini/tmp/<projectHash>/chats/session-*.json|jsonl`
//!
//! `type="gemini"` 消息带 `tokens`：
//! - `tokens.input` 含缓存（如 Gemini API 的 promptTokenCount）→ 减去 cached 得新输入
//! - `tokens.cached` → cacheRead；`tokens.output` → output；`tokens.thoughts` → reasoning
//!
//! 去重：message.id（UUID，跨文件共享）。

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::parser::Entry;
use crate::parsers::{get_u64, iso_to_ms};

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    // root 为文件时直接解析
    if let Some(r) = root {
        if r.is_file() {
            let mut out = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            parse_session_file(r, &mut out, &mut seen);
            return out;
        }
    }

    let gemini_root: std::path::PathBuf = match root {
        Some(r) => r.to_path_buf(),
        None => match std::env::var_os("HOME") {
            Some(h) => std::path::Path::new(&h).join(".gemini"),
            None => return Vec::new(),
        },
    };

    // chats 目录：tmp/*/chats/session-*
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let chats_root = gemini_root.join("tmp");
    if chats_root.is_dir() {
        let rd = match std::fs::read_dir(&chats_root) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        for proj in rd.flatten() {
            let chats = proj.path().join("chats");
            if !chats.is_dir() {
                continue;
            }
            crate::parsers::collect_files(&chats, "jsonl", &mut files);
            crate::parsers::collect_files(&chats, "json", &mut files);
        }
    }
    files.sort();
    files.dedup();

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for f in &files {
        parse_session_file(f, &mut out, &mut seen);
    }
    out
}

fn parse_session_file(f: &Path, out: &mut Vec<Entry>, seen: &mut HashSet<String>) {
    {
        let messages: Vec<Value> = if f.extension().map(|e| e == "jsonl").unwrap_or(false) {
            crate::parsers::read_jsonl_lines(f)
        } else {
            // JSON 数组文件：messages[]
            match std::fs::read_to_string(f) {
                Ok(c) => match serde_json::from_str::<Value>(&c) {
                    Ok(Value::Array(a)) => a,
                    Ok(Value::Object(o)) => o.get("messages").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
                    _ => Vec::new(),
                },
                Err(_) => Vec::new(),
            }
        };

        for msg in &messages {
            if msg.get("type").and_then(|v| v.as_str()) != Some("gemini") {
                continue;
            }
            let tokens = match msg.get("tokens").and_then(|v| v.as_object()) {
                Some(t) => t,
                None => continue,
            };
            let msg_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if !msg_id.is_empty() {
                if seen.contains(msg_id) {
                    continue;
                }
                seen.insert(msg_id.to_string());
            }
            let ts_raw = msg.get("timestamp").and_then(|v| v.as_str());
            let ts_ms = match ts_raw.and_then(iso_to_ms) {
                Some(ms) => ms,
                None => continue,
            };
            let raw_input = get_u64(tokens, "input");
            let cache_r = get_u64(tokens, "cached");
            let input_t = raw_input.saturating_sub(cache_r);
            let output_t = get_u64(tokens, "output");
            let reasoning = get_u64(tokens, "thoughts");
            if input_t + output_t + cache_r + reasoning == 0 {
                continue;
            }
            let model = msg.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
            out.push(Entry {
                source: "gemini_cli".into(),
                ts_ms,
                model,
                provider: "google".into(),
                input: input_t,
                output: output_t,
                cache_read: cache_r,
                cache_write: 0,
                reasoning,
                cost: 0.0,
            });
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gemini_session() {
        let json = r#"{"messages":[{"id":"g1","type":"gemini","timestamp":"2026-08-01T10:00:00Z","model":"gemini-3-flash","tokens":{"input":2000,"output":300,"cached":800,"thoughts":100,"total":2400}}]}"#;
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_gemini_test_{}.json", std::process::id()));
        std::fs::write(&p, json).unwrap();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // input 含缓存 2000 - cached 800 = 1200
        assert_eq!(e.input, 1200);
        assert_eq!(e.cache_read, 800);
        assert_eq!(e.output, 300);
        assert_eq!(e.reasoning, 100);
        assert_eq!(e.provider, "google");
    }
}
