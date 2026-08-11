//! OpenCode / Mimocode SQLite 解析器（两者同构）。
//!
//! 数据来源：`~/.local/share/opencode/opencode.db`、
//! `~/.local/share/mimocode/mimocode.db`
//!
//! 表 `message(data TEXT, time_created INTEGER)`，`data` 为 JSON：
//! - `modelID` / `providerID`
//! - `tokens.input` / `tokens.output` / `tokens.reasoning` / `tokens.cache.read` / `tokens.cache.write`
//! - `cost`（mimo 直接存 cost）
//!
//! 注意：只读 SQLite DB，不读 storage/ 下的 JSON 文件（同一消息会重复）。

use std::path::Path;

use serde_json::Value;

use crate::parser::Entry;

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let mut out = Vec::new();
    match root {
        Some(r) => {
            if r.is_file() {
                out.extend(parse_db(r, "opencode"));
                return out;
            }
            let cand = r.join("opencode.db");
            if cand.exists() {
                out.extend(parse_db(&cand, "opencode"));
            }
        }
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                let oc = Path::new(&home).join(".local/share/opencode/opencode.db");
                if oc.exists() {
                    out.extend(parse_db(&oc, "opencode"));
                }
            }
        }
    }
    out
}

/// 解析 OpenCode 形状的 SQLite DB（opencode / mimo 共用）。
pub fn parse_db(db_path: &Path, source: &str) -> Vec<Entry> {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare("SELECT data, time_created FROM message ORDER BY time_created") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for row in rows.flatten() {
        let (data, ts_ms) = row;
        let obj: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let tokens = match obj.get("tokens").and_then(|v| v.as_object()) {
            Some(t) => t,
            None => continue,
        };
        let input_t = get(tokens, "input");
        let output_t = get(tokens, "output");
        let reasoning = get(tokens, "reasoning");
        let cache = tokens.get("cache").and_then(|v| v.as_object());
        let cache_r = cache.map(|c| get(c, "read")).unwrap_or(0);
        let cache_w = cache.map(|c| get(c, "write")).unwrap_or(0);
        if input_t + output_t + cache_r + cache_w + reasoning == 0 {
            continue;
        }
        let model = obj.get("modelID").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let provider = obj.get("providerID").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let cost = obj.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);

        out.push(Entry {
            source: source.into(),
            ts_ms,
            model,
            provider,
            input: input_t,
            output: output_t,
            cache_read: cache_r,
            cache_write: cache_w,
            reasoning,
            cost,
        });
    }
    out
}

fn get(obj: &serde_json::Map<String, Value>, key: &str) -> u64 {
    obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn make_db() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tokstat_opencode_test_{}.db", std::process::id()));
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch("CREATE TABLE message (data TEXT, time_created INTEGER);").unwrap();
        let data = r#"{"modelID":"gpt-4o","providerID":"openai","tokens":{"input":100,"output":50,"reasoning":0,"cache":{"read":30,"write":10}}}"#;
        conn.execute("INSERT INTO message (data, time_created) VALUES (?1, ?2)", rusqlite::params![data, 1754000000000i64]).unwrap();
        p
    }

    #[test]
    fn test_parse_opencode_db() {
        let p = make_db();
        let entries = parse(Some(&p));
        let _ = std::fs::remove_file(&p);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.source, "opencode");
        assert_eq!(e.input, 100);
        assert_eq!(e.output, 50);
        assert_eq!(e.cache_read, 30);
        assert_eq!(e.cache_write, 10);
        assert_eq!(e.model, "gpt-4o");
    }
}
