//! TokStat — pi-agent 会话日志 Token 用量统计 CLI。
//!
//! 用法：
//!   tokstat                 扫描 ~/.pi/agent/sessions 输出统计页
//!   tokstat --dir <DIR>     扫描指定目录（可多次指定）
//!   tokstat --stdin         从标准输入逐行读取 JSONL
//!   tokstat --days <N>      热力图天数（默认 365）
//!   tokstat --no-color      禁用 ANSI 颜色

mod heatmap;
mod parser;
mod parsers;
mod render;
mod stats;

use std::path::PathBuf;

use chrono::{Local, NaiveDate};

use crate::heatmap::render_heatmap;
use crate::parser::parse_stdin;
use crate::render::{bar, blank, fmt_num, fmt_pct, fmt_short, pad, str_width, Term};
use crate::stats::{compute, Agg, Stats};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    #[cfg(windows)]
    enable_ansi_windows();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stdin_mode = false;
    let mut days: i64 = 365;
    let mut no_color = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" | "-d" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--dir 需要一个目录参数");
                    std::process::exit(2);
                }
                dirs.push(PathBuf::from(&args[i]));
            }
            "--stdin" | "-" => stdin_mode = true,
            "--days" | "-n" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--days 需要一个数字参数");
                    std::process::exit(2);
                }
                days = args[i].parse().unwrap_or(365);
            }
            "--no-color" => no_color = true,
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-V" | "--version" => {
                println!("tokstat {VERSION}");
                return;
            }
            other => {
                eprintln!("未知参数：{other}");
                print_help();
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let t = Term::detect(no_color);
    let today = Local::now().date_naive();

    // ---- 数据收集（多 Agent 源）----
    let (entries, source_desc): (Vec<crate::parser::Entry>, String) = if stdin_mode {
        match parse_stdin() {
            Ok(e) => (e, "标准输入".to_string()),
            Err(err) => {
                eprintln!("读取标准输入失败：{err}");
                std::process::exit(1);
            }
        }
    } else {
        let results = if dirs.is_empty() {
            parsers::parse_all(None)
        } else {
            parsers::parse_all(Some(&dirs))
        };
        if results.is_empty() {
            eprintln!("未找到任何 Agent 会话数据。可检查 ~/.pi/agent/sessions、~/.codex/sessions、~/.claude/projects、~/.local/share/opencode/opencode.db 等，或用 --dir 指定。");
            std::process::exit(1);
        }
        let mut entries = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (name, es) in results {
            names.push(name);
            entries.extend(es);
        }
        (entries, names.join(" · "))
    };

    // ---- 统计与渲染 ----
    let stats = compute(&entries, days);
    let lines = build_page(&t, &stats, days, today, &source_desc);
    for l in lines {
        println!("{l}");
    }
}

fn print_help() {
    println!(
        "tokstat {VERSION} — pi-agent 会话日志 Token 用量统计（含热力图）\n\
\n\
用法：\n\
  tokstat                 扫描默认目录（~/.pi/agent/sessions）并输出统计页\n\
  tokstat --dir <DIR>     扫描指定目录（可重复指定多个）\n\
  tokstat --stdin         从标准输入逐行读取 JSONL 会话日志\n\
  tokstat --days <N>      热力图时间跨度天数，默认 365\n\
  tokstat --no-color      禁用颜色输出\n\
  tokstat -h, --help      显示帮助\n\
  tokstat -V, --version   显示版本\n\
\n\
环境变量：\n\
  PI_AGENT_DIR            逗号分隔的会话日志根目录列表（覆盖默认路径）\n\
  NO_COLOR                设置后禁用颜色\n\
\n\
统计口径与 Tokdash 一致：tokens_in = input + cacheWrite；\n\
缓存命中率 = cacheRead / (tokens_in + cacheRead)。"
    );
}

// ============================================================
// 页面组装
// ============================================================

fn build_page(t: &Term, s: &Stats, days: i64, today: NaiveDate, source: &str) -> Vec<String> {
    let panel_w = 86.min(t.width.saturating_sub(2)).max(48);
    let mut out: Vec<String> = Vec::new();

    // ---- 标题 + 数据源 ----
    let now = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let header_lines = vec![
        blank(),
        format!("{} {}", t.bold("数据源"), t.dim(source)),
        format!(
            "{} {} 条消息 · 更新于 {}",
            t.dim("统计"),
            fmt_num(s.entry_count as u64),
            t.dim(&now)
        ),
        blank(),
    ];
    out.extend(render::panel(t, Some(" TokStat · 全 Agent Token 统计 "), header_lines, panel_w));

    // ---- 卡片 ----
    let cards = build_cards(t, s);
    out.extend(cards);
    out.push(blank());

    // ---- Token 构成（全部 + 近30天）----
    out.extend(build_breakdown_panel(t, s, panel_w));
    out.push(blank());

    // ---- 热力图（窗口自适应数据跨度，避免大片空白）----
    let inner_w = panel_w.saturating_sub(4);
    let (hm_days, hm_title) = heatmap_window(&s, days, today);
    let hm = render_heatmap(t, &s.daily, hm_days, today, inner_w);
    let mut hm_lines = vec![blank()];
    hm_lines.extend(hm);
    hm_lines.push(blank());
    out.extend(render::panel(t, Some(&hm_title), hm_lines, panel_w));
    out.push(blank());

    // ---- 模型 TOP ----
    out.extend(build_models_panel(t, s, panel_w));
    out.push(blank());

    out
}

// ------------------------------------------------------------
// 卡片
// ------------------------------------------------------------

fn build_cards(t: &Term, s: &Stats) -> Vec<String> {
    let card_titles = ["至今总 Token", "近 30 天 Token", "缓存命中率", "消息数"];
    let all = &s.all;
    let l30 = &s.last30;

    let share = if all.tokens > 0 {
        format!("占全时 {:.1}%", l30.tokens as f64 * 100.0 / all.tokens as f64)
    } else {
        "无记录".to_string()
    };

    let ch_rate = match all.cache_hit_rate() {
        Some(r) => fmt_pct(r),
        None => "n/a".to_string(),
    };

    let cards: Vec<(String, String, String)> = vec![
        (card_titles[0].to_string(), fmt_short(all.tokens), fmt_num(all.tokens)),
        (card_titles[1].to_string(), fmt_short(l30.tokens), share),
        (card_titles[2].to_string(), ch_rate, "cacheRead / 输入".to_string()),
        (
            card_titles[3].to_string(),
            fmt_num(all.messages),
            format!("近 30 天 {}", fmt_num(l30.messages)),
        ),
    ];

    render_cards(t, &cards, 82)
}

/// 并排渲染多个小卡片，返回已排好的行。
fn render_cards(t: &Term, cards: &[(String, String, String)], width: usize) -> Vec<String> {
    let n = cards.len();
    let gap = 2;
    let card_w = (width.saturating_sub(gap * (n - 1))) / n;
    let inner = card_w.saturating_sub(2);

    let mut rows: Vec<String> = vec![String::new(); 4];
    for (ci, (title, main, sub)) in cards.iter().enumerate() {
        let prefix = if ci == 0 { "" } else { &" ".repeat(gap) };
        let top = format!("{prefix}┌{}┐", "─".repeat(inner));
        let title_line = format!("{prefix}│{}│", center(title, inner));
        let main_line = format!("{prefix}│{}│", center(&t.bold(main), inner));
        let sub_line = format!("{prefix}│{}│", center(&t.dim(sub), inner));
        let bottom = format!("{prefix}└{}┘", "─".repeat(inner));
        rows[0].push_str(&top);
        rows[1].push_str(&title_line);
        rows[2].push_str(&main_line);
        rows[3].push_str(&sub_line);
        // bottom 并入下一段，简化：单独输出会破坏结构，改 5 行
        let _ = bottom;
    }
    // 由于 bottom 需要换行处理，改为 5 行结构重建
    let mut out: Vec<String> = vec![String::new(); 5];
    for (ci, (title, main, sub)) in cards.iter().enumerate() {
        let prefix = if ci == 0 { "" } else { &" ".repeat(gap) };
        out[0].push_str(&format!("{prefix}┌{}┐", "─".repeat(inner)));
        out[1].push_str(&format!("{prefix}│{}│", center(title, inner)));
        out[2].push_str(&format!("{prefix}│{}│", center(&t.bold(main), inner)));
        out[3].push_str(&format!("{prefix}│{}│", center(&t.dim(sub), inner)));
        out[4].push_str(&format!("{prefix}└{}┘", "─".repeat(inner)));
    }
    out
}

/// 计算热力图实际窗口：优先数据跨度（+7 天缓冲），不超过请求天数，最少 30 天。
fn heatmap_window(s: &Stats, days: i64, today: NaiveDate) -> (i64, String) {
    let span = if s.daily.len() >= 2 {
        let first = *s.daily.keys().next().unwrap();
        let last = *s.daily.keys().next_back().unwrap();
        (last - first).num_days() + 1
    } else if s.daily.len() == 1 {
        1
    } else {
        0
    };
    let effective = (span + 7).clamp(30, days.max(30));
    let start = today - chrono::Duration::days(effective - 1);
    let title = format!(
        " 每日 Token 热力图 · {} ~ {} ",
        start.format("%Y-%m-%d"),
        today.format("%Y-%m-%d")
    );
    (effective, title)
}

fn center(s: &str, width: usize) -> String {
    let w = str_width(s);
    if w >= width {
        return s.to_string();
    }
    let left = (width - w) / 2;
    let right = width - w - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

// ------------------------------------------------------------
// Token 构成条形图
// ------------------------------------------------------------

struct RowSpec {
    label: &'static str,
    value: u64,
    color: (u8, u8, u8),
}

fn build_breakdown_panel(t: &Term, s: &Stats, panel_w: usize) -> Vec<String> {
    let inner_w = panel_w.saturating_sub(4);
    let mut lines = vec![blank()];

    // 全部时间
    lines.push(format!("  {}", t.bold("Token 构成 · 全部时间")));
    lines.push(blank());
    let rows_all = breakdown_rows(&s.all);
    lines.extend(breakdown_lines(t, &rows_all, inner_w.saturating_sub(2)));
    lines.push(blank());

    // 近 30 天
    lines.push(format!("  {}", t.bold("Token 构成 · 近 30 天")));
    lines.push(blank());
    let rows_30 = breakdown_rows(&s.last30);
    lines.extend(breakdown_lines(t, &rows_30, inner_w.saturating_sub(2)));
    lines.push(blank());

    render::panel(t, Some(" Token 构成 "), lines, panel_w)
}

fn breakdown_rows(a: &Agg) -> Vec<RowSpec> {
    vec![
        RowSpec { label: "输入（新）", value: a.fresh_input, color: (96, 165, 250) },
        RowSpec { label: "输出", value: a.tokens_out, color: (250, 204, 21) },
        RowSpec { label: "缓存命中", value: a.tokens_cache, color: (52, 211, 153) },
        RowSpec { label: "缓存写入", value: a.cache_write, color: (167, 139, 250) },
    ]
}

fn breakdown_lines(t: &Term, rows: &[RowSpec], width: usize) -> Vec<String> {
    let total: u64 = rows.iter().map(|r| r.value).sum();
    let label_w = 10;
    let bar_w = (width as f64 * 0.48).round() as usize;
    let bar_w = bar_w.clamp(10, 44);
    let pct_w = 7;

    let mut out = Vec::new();
    for r in rows {
        let frac = if total > 0 { r.value as f64 / total as f64 } else { 0.0 };
        let pct = if total > 0 { format!("{:>6.1}%", frac * 100.0) } else { "  n/a".to_string() };
        let line = format!(
            "  {}{} {} {}",
            pad(r.label, label_w),
            bar(t, frac, bar_w, r.color),
            format!("{:>13}", fmt_num(r.value)),
            pad(&pct, pct_w)
        );
        out.push(line);
    }
    out
}

// ------------------------------------------------------------
// 模型 TOP
// ------------------------------------------------------------

fn build_models_panel(t: &Term, s: &Stats, panel_w: usize) -> Vec<String> {
    let inner_w = panel_w.saturating_sub(4);
    let total: u64 = s.all.tokens;
    let has_cost = s.models.iter().any(|m| m.cost > 0.0);

    let mut lines = vec![blank()];
    if s.models.is_empty() {
        lines.push(format!("  {}", t.dim("（无模型数据）")));
        lines.push(blank());
        return render::panel(t, Some(" 模型 TOP "), lines, panel_w);
    }

    let show = s.models.len().min(8);
    let src_w = 11;
    let name_w = 22;
    let tok_w = 12;
    let pct_w = 9;
    let msg_w = 10;
    let cost_w = if has_cost { 12 } else { 0 };

    // 表头
    let mut header = format!("  {}", pad("#", 3));
    header.push_str(&pad("来源", src_w));
    header.push_str(&pad("模型", name_w));
    header.push_str(&format!("{:>tok_w$}", "Token"));
    header.push_str(&format!("{:>pct_w$}", "占比"));
    header.push_str(&format!("{:>msg_w$}", "消息"));
    if has_cost {
        header.push_str(&format!("{:>cost_w$}", "费用"));
    }
    lines.push(t.dim(&header));
    lines.push(t.dim(&"─".repeat(inner_w.saturating_sub(2))));

    for (idx, m) in s.models.iter().take(show).enumerate() {
        let rank = format!("{:>2}", idx + 1);
        let (src, name) = m.name.split_once('|').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((String::new(), m.name.clone()));
        let pct = if total > 0 { format!("{:>7.1}%", m.tokens as f64 * 100.0 / total as f64) } else { "   n/a".to_string() };
        let mut line = format!("  {rank} ");
        line.push_str(&pad(&t.dim(&truncate(&src, src_w)), src_w));
        line.push_str(&pad(&truncate(&name, name_w), name_w));
        line.push_str(&format!("{:>tok_w$}", fmt_short(m.tokens)));
        line.push_str(&format!("{pct:>pct_w$}"));
        line.push_str(&format!("{:>msg_w$}", fmt_num(m.messages)));
        if has_cost {
            line.push_str(&format!("{:>cost_w$}", format!("${:.2}", m.cost)));
        }
        lines.push(line);
    }
    lines.push(blank());
    render::panel(t, Some(" 模型 TOP "), lines, panel_w)
}

fn truncate(s: &str, max_w: usize) -> String {
    if str_width(s) <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = str_width(&c.to_string());
        if w + cw > max_w.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    format!("{out}…")
}

// ------------------------------------------------------------
// Windows：启用 VT 处理 + UTF-8 输出
// ------------------------------------------------------------

#[cfg(windows)]
fn enable_ansi_windows() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        SetConsoleOutputCP(65001); // UTF-8
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn enable_ansi_windows() {}
