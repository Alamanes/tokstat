//! 聚合统计：全时 / 近 30 天 / 每日热力图数据 / 模型排行。
//!
//! 缓存命中率定义（与 Tokdash 一致）：
//! `cache_hit_rate = cacheRead / (tokens_in + cacheRead)`，
//! 其中 `tokens_in = input + cacheWrite`（缓存写入属计费输入）。
//! 分母为 0 时返回 None（渲染为 n/a）。

use std::collections::BTreeMap;

use chrono::{Local, NaiveDate, TimeZone};

use crate::parser::Entry;

/// 一段时间窗口的聚合。
#[derive(Debug, Default, Clone)]
pub struct Agg {
    /// 总 token（输入 + 输出 + 缓存命中 + 推理）
    pub tokens: u64,
    /// 计费输入 = 新输入 + 缓存写入
    pub tokens_in: u64,
    /// 新输入（不含缓存写入）
    pub fresh_input: u64,
    /// 输出
    pub tokens_out: u64,
    /// 缓存命中读取
    pub tokens_cache: u64,
    /// 缓存写入
    pub cache_write: u64,
    /// 推理
    pub reasoning: u64,
    /// 费用
    pub cost: f64,
    /// 消息数
    pub messages: u64,
}

impl Agg {
    pub fn add(&mut self, e: &Entry) {
        self.tokens += e.total_tokens();
        self.tokens_in += e.tokens_in();
        self.fresh_input += e.input;
        self.tokens_out += e.output;
        self.tokens_cache += e.cache_read;
        self.cache_write += e.cache_write;
        self.reasoning += e.reasoning;
        self.cost += e.cost;
        self.messages += 1;
    }

    /// 缓存命中率；分母为 0 时为 None（n/a）
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let num = self.tokens_cache;
        let den = self.tokens_in + num;
        if den == 0 {
            None
        } else {
            Some(num as f64 / den as f64)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelStat {
    pub name: String,
    pub tokens: u64,
    pub cost: f64,
    pub messages: u64,
}

/// 完整统计结果。
pub struct Stats {
    pub all: Agg,
    pub last30: Agg,
    /// 每日总 token（日期 → token），按日升序
    pub daily: BTreeMap<NaiveDate, u64>,
    pub models: Vec<ModelStat>,
    pub entry_count: usize,
}

/// 计算统计。`days` 用于「近 N 天」窗口与每日分布保留范围。
pub fn compute(entries: &[Entry], days: i64) -> Stats {
    let today = Local::now().date_naive();
    let window_start = today - chrono::Days::new(days.max(1) as u64 - 1);

    let mut all = Agg::default();
    let mut last30 = Agg::default();
    let mut daily: BTreeMap<NaiveDate, u64> = BTreeMap::new();
    let mut model_map: BTreeMap<String, ModelStat> = BTreeMap::new();

    for e in entries {
        all.add(e);

        let date = local_date(e.ts_ms);
        if date >= window_start && date <= today {
            last30.add(e);
        }

        // 每日分布只保留窗口内（热力图用）
        if date >= window_start && date <= today {
            *daily.entry(date).or_insert(0) += e.total_tokens();
        }

        let key = model_key(e);
        let m = model_map.entry(key.clone()).or_insert_with(|| ModelStat { name: key, tokens: 0, cost: 0.0, messages: 0 });
        m.tokens += e.total_tokens();
        m.cost += e.cost;
        m.messages += 1;
    }

    let mut models: Vec<ModelStat> = model_map.into_values().collect();
    models.sort_by(|a, b| b.tokens.cmp(&a.tokens));

    Stats {
        all,
        last30,
        daily,
        models,
        entry_count: entries.len(),
    }
}

fn local_date(ts_ms: i64) -> NaiveDate {
    let secs = ts_ms.div_euclid(1000);
    let nanos = (ts_ms.rem_euclid(1000) * 1_000_000) as u32;
    Local
        .timestamp_opt(secs, nanos)
        .single()
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| Local::now().date_naive())
}

/// 模型展示名：provider/model（provider 非空时），同 Tokdash 的 full_model_name。
fn full_model_name(e: &Entry) -> String {
    if e.provider.is_empty() {
        e.model.clone()
    } else {
        format!("{}/{}", e.provider, e.model)
    }
}

/// 分组 key：source + full_model_name（同 Tokdash 的 (source, full_model_name)）。
fn model_key(e: &Entry) -> String {
    format!("{}|{}", e.source, full_model_name(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Entry;

    fn entry(ts_ms: i64, input: u64, output: u64, cache_read: u64, cache_write: u64, model: &str) -> Entry {
        Entry {
            source: "pi_agent".into(),
            ts_ms,
            model: model.into(),
            provider: String::new(),
            input,
            output,
            cache_read,
            cache_write,
            reasoning: 0,
            cost: 0.0,
        }
    }

    #[test]
    fn test_cache_hit_rate() {
        let mut a = Agg::default();
        // in=100, cacheWrite=0 → tokens_in=100, cache=25 → 25/125 = 0.2
        a.add(&entry(0, 100, 0, 25, 0, "m"));
        assert!((a.cache_hit_rate().unwrap() - 0.2).abs() < 1e-9);

        // 分母为 0 → None
        let a2 = Agg::default();
        assert!(a2.cache_hit_rate().is_none());
    }

    #[test]
    fn test_window_filtering() {
        let today = Local::now().date_naive();
        let today_ms = Local
            .from_local_datetime(&today.and_hms_opt(12, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let old_ms = today_ms - 40 * 24 * 3600 * 1000; // 40 天前

        let entries = vec![entry(today_ms, 10, 0, 0, 0, "m"), entry(old_ms, 100, 0, 0, 0, "m")];
        let s = compute(&entries, 30);
        assert_eq!(s.all.messages, 2);
        assert_eq!(s.last30.messages, 1);
        assert_eq!(s.daily.len(), 1);
    }
}
