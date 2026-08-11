//! Hermes agent 会话数据库解析器。
//!
//! 数据来源：`~/.hermes/state.db`（`HERMES_HOME` 逗号分隔可覆盖）
//!
//! ```
//! SELECT id, model, billing_provider, started_at, message_count,
//!        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
//!        reasoning_tokens, estimated_cost_usd, actual_cost_usd
//! FROM sessions WHERE model IS NOT NULL AND TRIM(model) != ''
//! ```
//! 一行 = 一个会话。started_at 为秒浮点 → ms。
//! 费用优先级：actual_cost_usd > estimated_cost_usd > 0（无定价库时仅用日志值）。
//! 跳过全 0 且无费用的行。去重：id。

use std::collections::HashSet;
use std::path::Path;

use crate::parser::Entry;

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let mut dbs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(r) = root {
        let cand = r.join("state.db");
        if cand.exists() {
            dbs.push(cand);
        }
    } else if let Ok(env) = std::env::var("HERMES_HOME") {
        for d in env.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let cand = std::path::Path::new(d).join("state.db");
            if cand.exists() {
                dbs.push(cand);
            }
        }
    } else if let Some(home) = std::env::var_os("HOME") {
        let cand = std::path::Path::new(&home).join(".hermes/state.db");
        if cand.exists() {
            dbs.push(cand);
        }
    }
    if dbs.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for db in &dbs {
        let conn = match rusqlite::Connection::open(db) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let sql = "SELECT id, model, billing_provider, started_at, message_count, \
                   input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
                   reasoning_tokens, estimated_cost_usd, actual_cost_usd \
                   FROM sessions WHERE model IS NOT NULL AND TRIM(model) != ''";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0).unwrap_or_default(),
                row.get::<_, String>(1).unwrap_or_default(),
                row.get::<_, Option<String>>(2).ok().flatten().unwrap_or_default(),
                row.get::<_, f64>(3).unwrap_or(0.0),
                row.get::<_, i64>(4).unwrap_or(0),
                row.get::<_, i64>(5).unwrap_or(0),
                row.get::<_, i64>(6).unwrap_or(0),
                row.get::<_, i64>(7).unwrap_or(0),
                row.get::<_, i64>(8).unwrap_or(0),
                row.get::<_, i64>(9).unwrap_or(0),
                row.get::<_, Option<f64>>(10).ok().flatten().unwrap_or(0.0),
                row.get::<_, Option<f64>>(11).ok().flatten().unwrap_or(0.0),
            ))
        }) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for row in rows.flatten() {
            let (id, model, billing_provider, started_at, _msg_count, input_t, output_t, cache_r, cache_w, reasoning, est_cost, actual_cost) = row;
            if !id.is_empty() && !seen.insert(id.clone()) {
                continue;
            }
            let input_t = input_t.max(0) as u64;
            let output_t = output_t.max(0) as u64;
            let cache_r = cache_r.max(0) as u64;
            let cache_w = cache_w.max(0) as u64;
            let reasoning = reasoning.max(0) as u64;
            let cost = if actual_cost > 0.0 {
                actual_cost
            } else {
                est_cost.max(0.0)
            };
            if input_t + output_t + cache_r + cache_w + reasoning == 0 && cost <= 0.0 {
                continue;
            }
            // started_at 秒 → ms（若已是 ms 级则原样）
            let ts_ms = if started_at > 1e12 { started_at as i64 } else { (started_at * 1000.0) as i64 };
            out.push(Entry {
                source: "hermes".into(),
                ts_ms,
                model,
                provider: billing_provider,
                input: input_t,
                output: output_t,
                cache_read: cache_r,
                cache_write: cache_w,
                reasoning,
                cost,
            });
        }
    }
    out
}
