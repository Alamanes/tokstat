//! Grok Build 全局用量日志解析器。
//!
//! 数据来源：`$GROK_HOME/logs/unified.jsonl`（默认 `~/.grok/logs/unified.jsonl`）
//!
//! `shell.turn.inference_done` 行携带真实 token 拆分：
//! - `prompt_tokens`（含缓存）与 `cached_prompt_tokens` → cache_read = min(cached, prompt)，
//!   input = prompt - cache_read
//! - `completion_tokens` + `reasoning_tokens` → output（推理按输出计费）
//! - 行无模型 id，通过 `pid` 跟踪 model 事件
//!   （"model changed" / "model catalog: notifying clients" /
//!    "backend_search: model switch" / "subagent model resolved"）
//!
//! 去重：Tokdash 为 source_replace 全量重扫，无稳定行 id；这里用
//! (pid, timestamp, token 值) 内容哈希避免同文件重复行。

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;


use crate::parser::Entry;
use crate::parsers::{content_hash, get_u64, scalar_to_ms};

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let log_path: std::path::PathBuf = match root {
        Some(r) => r.join("logs/unified.jsonl"),
        None => {
            let base: Option<std::path::PathBuf> = std::env::var("GROK_HOME")
                .ok()
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".grok")));
            let base = match base {
                Some(b) => b,
                None => return Vec::new(),
            };
            base.join("logs/unified.jsonl")
        }
    };
    if !log_path.exists() {
        return Vec::new();
    }

    let mut model_by_pid: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for obj in crate::parsers::read_jsonl_lines(&log_path) {
        let msg = obj.get("msg").and_then(|v| v.as_str()).unwrap_or("");
        let ctx = obj.get("ctx").and_then(|v| v.as_object());
        let pid = obj.get("pid").map(|v| v.to_string()).unwrap_or_default();

        // 模型事件
        if msg.contains("model") {
            let raw = match msg {
                "model changed" => ctx.and_then(|c| c.get("model")),
                "model catalog: notifying clients" => ctx.and_then(|c| c.get("current_model_id")),
                "backend_search: model switch" => ctx
                    .and_then(|c| c.get("model"))
                    .or_else(|| ctx.and_then(|c| c.get("current_model_id")))
                    .or_else(|| ctx.and_then(|c| c.get("model_id"))),
                "subagent model resolved" => ctx
                    .and_then(|c| c.get("model_id"))
                    .or_else(|| ctx.and_then(|c| c.get("model"))),
                _ => None,
            };
            if let Some(m) = raw.and_then(|v| v.as_str()).map(|s| s.to_string()) {
                if !m.is_empty() && !pid.is_empty() {
                    model_by_pid.insert(pid.clone(), m);
                }
            }
            continue;
        }

        if msg != "shell.turn.inference_done" {
            continue;
        }
        let ctx = match ctx {
            Some(c) => c,
            None => continue,
        };
        let prompt = get_u64(ctx, "prompt_tokens");
        let cache_read = get_u64(ctx, "cached_prompt_tokens").min(prompt);
        let input_t = prompt.saturating_sub(cache_read);
        let output_t = get_u64(ctx, "completion_tokens") + get_u64(ctx, "reasoning_tokens");
        if input_t + cache_read + output_t == 0 {
            continue;
        }
        let ts_ms = match obj.get("timestamp").and_then(scalar_to_ms) {
            Some(ms) => ms,
            None => continue,
        };
        let model = model_by_pid
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| "grok-unknown".to_string());

        // 去重：内容哈希
        let hash = content_hash(&[
            &pid,
            &ts_ms.to_string(),
            &format!("{input_t},{cache_read},{output_t}"),
        ]);
        if !seen.insert(hash) {
            continue;
        }

        out.push(Entry {
            source: "grok".into(),
            ts_ms,
            model,
            provider: String::new(),
            input: input_t,
            output: output_t,
            cache_read,
            cache_write: 0,
            reasoning: 0,
            cost: 0.0,
        });
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_grok_unified() {
        let lines = r#"{"msg":"model changed","pid":123,"ctx":{"model":"grok-4"}}
{"msg":"shell.turn.inference_done","pid":123,"timestamp":1754000000,"ctx":{"prompt_tokens":1500,"cached_prompt_tokens":500,"completion_tokens":200,"reasoning_tokens":50}}
"#;
        // 构造临时 GROK_HOME
        let mut dir = std::env::temp_dir();
        dir.push(format!("tokstat_grok_test_{}", std::process::id()));
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("unified.jsonl"), lines).unwrap();
        let entries = parse(Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // input = 1500-500 = 1000；cacheRead = 500；output = 200+50 = 250
        assert_eq!(e.input, 1000);
        assert_eq!(e.cache_read, 500);
        assert_eq!(e.output, 250);
        assert_eq!(e.model, "grok-4");
    }
}
