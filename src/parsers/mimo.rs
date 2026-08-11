//! Mimocode 解析器：与 OpenCode 同构的 SQLite，直接复用。
//! 数据来源：`~/.local/share/mimocode/mimocode.db`

use std::path::Path;

use crate::parser::Entry;

pub fn parse(root: Option<&Path>) -> Vec<Entry> {
    let mut out = Vec::new();
    match root {
        Some(r) => {
            if r.is_file() {
                out.extend(super::opencode::parse_db(r, "mimo"));
                return out;
            }
            let cand = r.join("mimocode.db");
            if cand.exists() {
                out.extend(super::opencode::parse_db(&cand, "mimo"));
            }
        }
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                let cand = std::path::Path::new(&home).join(".local/share/mimocode/mimocode.db");
                if cand.exists() {
                    out.extend(super::opencode::parse_db(&cand, "mimo"));
                }
            }
        }
    }
    out
}
