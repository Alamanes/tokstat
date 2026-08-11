//! Claude Code 会话日志解析器。
//!
//! 数据来源：`~/.claude*/projects/**/*.jsonl`
//!
//! 行：`obj.message.role == "assistant"`（或新版 OpenAI 兼容端点的
//! `obj.type == "assistant"` 且无 role）且 `message.usage` 非空。
//! 字段别名：input_tokens/input、output_tokens/output、
//! cache_read_input_tokens/cache_read_tokens、
//! cache_creation_input_tokens/cache_write_tokens。
//!
//! 去重：legacy（有 role）按 message.id 首个非零出现；新版 streaming
//! 快照（无 role）共享 id，保留时间戳最新的一条。

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;


use crate::parser::Entry;
use crate::parsers::{collect_files_multi, get_u64, iso_to_ms};

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut seen_legacy: HashSet<String> = HashSet::new();
    let mut snapshot_by_id: HashMap<String, usize> = HashMap::new();

    // root 为文件时直接解析
    if let Some(r) = root {
        if r.is_file() {
            parse_one_file(r, &mut out, &mut seen_legacy, &mut snapshot_by_id);
            out.sort_by_key(|e| e.ts_ms);
            return out;
        }
    }

    let roots: Vec<std::path::PathBuf> = match root {
        Some(r) => vec![r.to_path_buf()],
        None => {
            let mut roots = Vec::new();
            if let Some(home) = std::env::var_os("HOME") {
                let home = std::path::Path::new(&home);
                let mut dirs: Vec<_> = std::fs::read_dir(home)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.starts_with(".claude") {
                            let p = e.path().join("projects");
                            p.is_dir().then_some(p)
                        } else {
                            None
                        }
                    })
                    .collect();
                dirs.sort();
                roots.extend(dirs);
            }
            roots
        }
    };

    let files = collect_files_multi(&roots.iter().map(|p| p.as_path()).collect::<Vec<_>>(), "jsonl");

    // 新版 streaming 快照：message_id → index（保留最新）
    for f in &files {
        parse_one_file(f, &mut out, &mut seen_legacy, &mut snapshot_by_id);
    }

    out.sort_by_key(|e| e.ts_ms);
    out
}

fn parse_one_file(
    path: &Path,
    out: &mut Vec<Entry>,
    seen_legacy: &mut HashSet<String>,
    snapshot_by_id: &mut HashMap<String, usize>,
) {
    for obj in crate::parsers::read_jsonl_lines(path) {
            let msg = match obj.get("message").and_then(|v| v.as_object()) {
                Some(m) => m,
                None => continue,
            };
            let role = msg.get("role").and_then(|v| v.as_str());
            let is_top_level = role.is_none() && obj.get("type").and_then(|v| v.as_str()) == Some("assistant");
            if role != Some("assistant") && !is_top_level {
                continue;
            }
            let usage = match msg.get("usage").and_then(|v| v.as_object()) {
                Some(u) => u,
                None => continue,
            };

            let ts_raw = obj.get("timestamp").and_then(|v| v.as_str());
            let ts_ms = match ts_raw.and_then(iso_to_ms) {
                Some(ms) => ms,
                None => continue,
            };

            let input_t = get_u64(usage, "input_tokens").max(get_u64(usage, "input"));
            let output_t = get_u64(usage, "output_tokens").max(get_u64(usage, "output"));
            let cache_r = get_u64(usage, "cache_read_input_tokens").max(get_u64(usage, "cache_read_tokens"));
            let cache_w = get_u64(usage, "cache_creation_input_tokens").max(get_u64(usage, "cache_write_tokens"));
            if input_t + output_t + cache_r + cache_w == 0 {
                continue;
            }

            let msg_id = msg
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| obj.get("uuid").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();

            let model = msg.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
            let provider = infer_provider(&model);

            let entry = Entry {
                source: "claude".into(),
                ts_ms,
                model,
                provider,
                input: input_t,
                output: output_t,
                cache_read: cache_r,
                cache_write: cache_w,
                reasoning: 0,
                cost: 0.0,
            };

            if msg_id.is_empty() {
                out.push(entry);
                continue;
            }

            if is_top_level {
                // streaming 快照：同 id 保留最新（最完整）
                match snapshot_by_id.get(&msg_id) {
                    Some(&idx) if out[idx].ts_ms >= entry.ts_ms => {}
                    Some(&idx) => out[idx] = entry,
                    None => {
                        snapshot_by_id.insert(msg_id.clone(), out.len());
                        out.push(entry);
                    }
                }
            } else if !seen_legacy.contains(&msg_id) {
                seen_legacy.insert(msg_id);
                out.push(entry);
            }
        }
    }

fn infer_provider(model: &str) -> String {
    let m = model.to_lowercase();
    if m.starts_with("claude") {
        "anthropic".into()
    } else if m.contains("gemini") {
        "google".into()
    } else if m.starts_with("gpt") || m.contains("codex") {
        "openai".into()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_claude_usage() {
        let lines = r#"{"timestamp":"2026-08-01T10:00:00Z","message":{"id":"msg-1","role":"assistant","model":"claude-sonnet-4-5","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":300,"cache_creation_input_tokens":50}}}
"#;
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_claude_test_{}.jsonl", std::process::id()));
        std::fs::write(&p, lines).unwrap();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.source, "claude");
        assert_eq!(e.input, 1000);
        assert_eq!(e.output, 500);
        assert_eq!(e.cache_read, 300);
        assert_eq!(e.cache_write, 50);
        assert_eq!(e.provider, "anthropic");
    }

    #[test]
    fn test_duplicate_message_id_dedup() {
        // 同一 message.id 出现两次 → 只计一次
        let line = r#"{"timestamp":"2026-08-01T10:00:00Z","message":{"id":"dup-1","role":"assistant","model":"m","usage":{"input_tokens":10,"output_tokens":10}}}
"#;
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_claude_dup_{}.jsonl", std::process::id()));
        std::fs::write(&p, line.repeat(2)).unwrap();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_top_level_streaming_snapshot_keeps_latest() {
        // 无 role 的 streaming 快照：同 id 保留 timestamp 最新
        let older = r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"id":"snap-1","model":"m","usage":{"input_tokens":10,"output_tokens":10}}}
"#;
        let newer = r#"{"type":"assistant","timestamp":"2026-08-01T10:00:01Z","message":{"id":"snap-1","model":"m","usage":{"input_tokens":99,"output_tokens":99}}}
"#;
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_claude_snap_{}.jsonl", std::process::id()));
        std::fs::write(&p, format!("{older}{newer}")).unwrap();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].output, 99);
    }
}
