//! Kimi CLI / Kimi Code 解析器（两种 schema）。
//!
//! Schema A（legacy <0.26）：`~/.kimi/sessions/<userId>/<sessionId>/wire.jsonl`
//! `message.type == "StatusUpdate"` + `payload.token_usage`：
//! input_other → input、output → output、input_cache_read → cacheRead、
//! input_cache_creation → cacheWrite；timestamp 为秒。
//! 去重：payload.message_id。
//!
//! Schema B（Kimi Code >=0.26）：`~/.kimi-code/sessions/<ws>/<sid>/agents/<agent>/wire.jsonl`
//! `type == "usage.record"`：`usage.inputOther/inputCacheRead/inputCacheCreation/output`，
//! `time` 为毫秒；每行是增量（必须全计）。去重：内容哈希（path,time,model,usage）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};


use crate::parser::Entry;
use crate::parsers::{collect_files_multi, content_hash, get_u64, scalar_to_ms};

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut seen_msg_ids: HashSet<String> = HashSet::new();
    let mut seen_hashes: HashSet<String> = HashSet::new();

    // root 为文件时直接解析
    if let Some(r) = root {
        if r.is_file() {
            parse_one_file(r, &mut out, &mut seen_msg_ids, &mut seen_hashes);
            return out;
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(r) = root {
        roots.push(r.to_path_buf());
    } else {
        if let Ok(env) = std::env::var("KIMI_CODE_HOME") {
            roots.push(PathBuf::from(env));
        } else if let Ok(env) = std::env::var("KIMI_SHARE_DIR") {
            roots.push(PathBuf::from(env));
        }
        if roots.is_empty() {
            if let Some(home) = std::env::var_os("HOME") {
                let home = Path::new(&home);
                for p in [home.join(".kimi-code"), home.join(".kimi")] {
                    if p.exists() {
                        roots.push(p);
                    }
                }
            }
        }
    }
    if roots.is_empty() {
        return Vec::new();
    }

    let files = collect_files_multi(&roots.iter().map(|p| p.as_path()).collect::<Vec<_>>(), "jsonl");

    for f in &files {
        parse_one_file(f, &mut out, &mut seen_msg_ids, &mut seen_hashes);
    }
    out
}

fn parse_one_file(
    path: &Path,
    out: &mut Vec<Entry>,
    seen_msg_ids: &mut HashSet<String>,
    seen_hashes: &mut HashSet<String>,
) {
    for obj in crate::parsers::read_jsonl_lines(path) {
            // ---- Schema B：usage.record ----
            if obj.get("type").and_then(|v| v.as_str()) == Some("usage.record") {
                let usage = match obj.get("usage").and_then(|v| v.as_object()) {
                    Some(u) => u,
                    None => continue,
                };
                let input_t = get_u64(usage, "inputOther");
                let output_t = get_u64(usage, "output");
                let cache_r = get_u64(usage, "inputCacheRead");
                let cache_w = get_u64(usage, "inputCacheCreation");
                if input_t + output_t + cache_r + cache_w == 0 {
                    continue;
                }
                let ts_ms = match obj.get("time").and_then(scalar_to_ms) {
                    Some(ms) => ms,
                    None => continue,
                };
                let model = obj.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                let hash = content_hash(&[
                    &path.display().to_string(),
                    &ts_ms.to_string(),
                    &model,
                    &format!("{input_t},{output_t},{cache_r},{cache_w}"),
                ]);
                if !seen_hashes.insert(hash) {
                    continue;
                }
                out.push(Entry {
                    source: "kimi".into(),
                    ts_ms,
                    model,
                    provider: "moonshotai".into(),
                    input: input_t,
                    output: output_t,
                    cache_read: cache_r,
                    cache_write: cache_w,
                    reasoning: 0,
                    cost: 0.0,
                });
                continue;
            }

            // ---- Schema A：StatusUpdate ----
            let msg = match obj.get("message") {
                Some(m) if m.is_object() => m,
                _ => continue,
            };
            if msg.get("type").and_then(|v| v.as_str()) != Some("StatusUpdate") {
                continue;
            }
            let payload = match msg.get("payload").and_then(|v| v.as_object()) {
                Some(p) => p,
                None => continue,
            };
            let usage = match payload.get("token_usage").and_then(|v| v.as_object()) {
                Some(u) => u,
                None => continue,
            };
            let input_t = get_u64(usage, "input_other");
            let output_t = get_u64(usage, "output");
            let cache_r = get_u64(usage, "input_cache_read");
            let cache_w = get_u64(usage, "input_cache_creation");
            if input_t + output_t + cache_r + cache_w == 0 {
                continue;
            }
            let msg_id = payload.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
            if !msg_id.is_empty() && !seen_msg_ids.insert(msg_id.to_string()) {
                continue;
            }
            let ts_ms = match obj.get("timestamp").and_then(scalar_to_ms) {
                Some(ms) => ms,
                None => continue,
            };
            out.push(Entry {
                source: "kimi".into(),
                ts_ms,
                model: "unknown".into(),
                provider: "moonshotai".into(),
                input: input_t,
                output: output_t,
                cache_read: cache_r,
                cache_write: cache_w,
                reasoning: 0,
                cost: 0.0,
            });
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kimi_code_usage_record() {
        let lines = r#"{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":100,"output":50,"inputCacheRead":30,"inputCacheCreation":10},"usageScope":"turn","time":1754000000000}
"#;
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_kimi_test_{}.jsonl", std::process::id()));
        std::fs::write(&p, lines).unwrap();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.input, 100);
        assert_eq!(e.output, 50);
        assert_eq!(e.cache_read, 30);
        assert_eq!(e.cache_write, 10);
        assert_eq!(e.provider, "moonshotai");
    }

    #[test]
    fn test_legacy_status_update() {
        let lines = r#"{"timestamp":1754000000.5,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":100,"output":50,"input_cache_read":30,"input_cache_creation":10},"message_id":"m1"}}}
"#;
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_kimi_legacy_{}.jsonl", std::process::id()));
        std::fs::write(&p, lines).unwrap();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.input, 100);
        assert_eq!(e.cache_read, 30);
    }
}
