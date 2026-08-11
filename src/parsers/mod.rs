//! 多 Agent 会话日志解析器集合。
//!
//! 每个解析器对应 Tokdash `sources/coding_tools.py` 中的一个 Parser，输出
//! 归一化的 [`crate::parser::Entry`]。支持两种模式：
//! - 默认：自动发现主目录下所有已安装 agent 的数据目录并解析
//! - `--dir <DIR>`：用覆盖 root 让每个解析器在指定目录下按自身模式查找
//!
//! 支持的源（与 Tokdash 对齐）：
//! pi_agent / codex / claude / opencode / gemini_cli / kimi / grok /
//! hermes / mimo / copilot_cli。
//! （antigravity 为 protobuf 格式、amp 尚无 schema，Tokdash 中亦为占位/跳过）

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod gemini;
pub mod grok;
pub mod hermes;
pub mod kimi;
pub mod mimo;
pub mod opencode;
pub mod pi;


use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::parser::Entry;

/// 读取文件所有行的 JSON（跳过坏行）。
pub fn read_jsonl_lines(path: &Path) -> Vec<Value> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            out.push(v);
        }
    }
    out
}

/// ISO-8601（可能带 Z）→ epoch ms。
pub fn iso_to_ms(s: &str) -> Option<i64> {
    let normalized = s.strip_suffix('Z').map(|t| format!("{t}+00:00")).unwrap_or_else(|| s.to_string());
    let dt = chrono::DateTime::parse_from_rfc3339(&normalized).ok()?;
    Some(dt.timestamp_millis())
}

/// 从对象中提取 u64 字段。
pub fn get_u64(obj: &serde_json::Map<String, Value>, key: &str) -> u64 {
    obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// 时间戳标量自动缩放为 epoch ms（阈值同 ccusage / Tokdash）：
/// >=1e17 纳秒 / >=1e14 微秒 / >=1e11 毫秒 / 其它正数按秒。
pub fn scalar_to_ms(v: &Value) -> Option<i64> {
    let fv = v.as_f64()?;
    if fv >= 1e17 {
        Some((fv / 1_000_000.0) as i64)
    } else if fv >= 1e14 {
        Some((fv / 1000.0) as i64)
    } else if fv >= 1e11 {
        Some(fv as i64)
    } else if fv > 0.0 {
        Some((fv * 1000.0) as i64)
    } else {
        None
    }
}

/// 内容哈希（去重键用）。
pub fn content_hash(parts: &[&str]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// 递归收集目录下指定扩展名的文件（排序）。
pub fn collect_files(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for item in rd.flatten() {
        let p = item.path();
        let ft = match item.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            collect_files(&p, ext, out);
        } else if ft.is_file() && p.extension().map(|e| e == ext).unwrap_or(false) {
            out.push(p);
        }
    }
}

/// 扫描多个 root（每个 root 递归）并排序去重。
pub fn collect_files_multi(roots: &[&Path], ext: &str) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for r in roots {
        collect_files(r, ext, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

/// 解析全部源。`dirs` 为 None 时自动发现默认路径；Some 时按目录名路由到
/// 对应解析器（避免同一目录被多个解析器重复扫描）。
pub fn parse_all(dirs: Option<&[std::path::PathBuf]>) -> Vec<(String, Vec<Entry>)> {
    let mut results: Vec<(String, Vec<Entry>)> = Vec::new();

    let mut push = |name: &str, entries: Vec<Entry>| {
        if !entries.is_empty() {
            results.push((name.to_string(), entries));
        }
    };

    match dirs {
        Some(ds) => {
            for d in ds {
                match route(d) {
                    "codex" => push("codex", codex::parse(Some(d))),
                    "claude" => push("claude", claude::parse(Some(d))),
                    "mimo" => push("mimo", mimo::parse(Some(d))),
                    "opencode" => push("opencode", opencode::parse(Some(d))),
                    "gemini_cli" => push("gemini_cli", gemini::parse(Some(d))),
                    "kimi" => push("kimi", kimi::parse(Some(d))),
                    "grok" => push("grok", grok::parse(Some(d))),
                    "hermes" => push("hermes", hermes::parse(Some(d))),
                    "copilot_cli" => push("copilot_cli", copilot::parse(Some(d))),
                    _ => push("pi_agent", pi::parse(Some(d))),
                }
            }
        }
        None => {
            push("pi_agent", pi::parse(None));
            push("codex", codex::parse(None));
            push("claude", claude::parse(None));
            push("gemini_cli", gemini::parse(None));
            push("kimi", kimi::parse(None));
            push("grok", grok::parse(None));
            push("hermes", hermes::parse(None));
            push("mimo", mimo::parse(None));
            push("opencode", opencode::parse(None));
            push("copilot_cli", copilot::parse(None));
        }
    }

    results
}

/// 根据目录/文件路径判断属于哪个 Agent 源（小写匹配，具体规则优先）。
fn route(p: &std::path::Path) -> &'static str {
    let s = p.to_string_lossy().to_lowercase();
    if s.contains(".codex") {
        "codex"
    } else if s.contains(".claude") {
        "claude"
    } else if s.contains("mimocode") {
        "mimo"
    } else if s.contains("opencode") {
        "opencode"
    } else if s.contains(".gemini") {
        "gemini_cli"
    } else if s.contains("kimi") {
        "kimi"
    } else if s.contains("grok") {
        "grok"
    } else if s.contains("hermes") {
        "hermes"
    } else if s.contains("copilot") {
        "copilot_cli"
    } else {
        "pi_agent"
    }
}

