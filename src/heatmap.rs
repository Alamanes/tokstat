//! GitHub 贡献图风格的每日 Token 热力图。

use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate};

use crate::render::{fmt_short, str_width, Term};

/// 颜色档位（GitHub 绿，24bit）
const LEVELS: [(u8, u8, u8); 5] = [
    (22, 27, 34),  // 0: 无数据
    (14, 68, 41),  // 1: 低
    (0, 109, 50),  // 2: 中
    (38, 166, 65), // 3: 高
    (57, 211, 83), // 4: 极高
];

const WEEKDAY_LABELS: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
const MONTH_LABELS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// 渲染热力图，返回完整行列表（月份标签行 + 7 行数据 + 图例 + 统计）。
/// `width` 为可用宽度（面板内宽）。
pub fn render_heatmap(
    t: &Term,
    daily: &BTreeMap<NaiveDate, u64>,
    days: i64,
    today: NaiveDate,
    width: usize,
) -> Vec<String> {
    let days = days.max(7).min(730) as u64;
    let start = today - Duration::days((days - 1) as i64);
    // 对齐到所在周的周日
    let start_sunday = start - Duration::days(start.weekday().num_days_from_sunday() as i64);
    let weeks = ((today - start_sunday).num_days() / 7 + 1) as usize;

    // 分位数阈值（只统计非零日）
    let mut nonzero: Vec<u64> = daily.values().copied().filter(|&v| v > 0).collect();
    nonzero.sort_unstable();
    let q = |p: f64| -> u64 {
        if nonzero.is_empty() {
            return 0;
        }
        let idx = ((nonzero.len() - 1) as f64 * p).round() as usize;
        nonzero[idx.min(nonzero.len() - 1)]
    };
    let (q25, q50, q75) = (q(0.25), q(0.5), q(0.75));

    // 网格：列 = 周，行 = 周日..周六
    let mut grid = vec![[0u8; 7]; weeks];
    let mut max_day = 0u64;
    let mut d = start;
    while d <= today {
        let col = ((d - start_sunday).num_days() / 7) as usize;
        let row = d.weekday().num_days_from_sunday() as usize;
        let v = daily.get(&d).copied().unwrap_or(0);
        max_day = max_day.max(v);
        grid[col][row] = if v == 0 {
            0
        } else if v <= q25 {
            1
        } else if v <= q50 {
            2
        } else if v <= q75 {
            3
        } else {
            4
        };
        d = d + Duration::days(1);
    }

    // 自适应格子宽度
    let label_w = 4; // 「周日」显示宽度 = 4
    let (cell_w, gap) = fit_cell(width, label_w, weeks);

    let mut lines: Vec<String> = Vec::new();

    // 月份标签行
    lines.push(month_label_row(t, start_sunday, weeks, label_w, cell_w, gap));

    // 7 行数据
    for row in 0..7 {
        let mut line = String::new();
        let label = format!("{}  ", WEEKDAY_LABELS[row]);
        line.push_str(&label);
        line.push_str(&" ".repeat(label_w + 1 - str_width(&label)));

        for col in 0..weeks {
            let lv = grid[col][row] as usize;
            let (r, g, b) = LEVELS[lv];
            let cell = render_cell(t, lv, cell_w);
            if t.color {
                line.push_str(&t.bg(r, g, b, &cell));
            } else {
                line.push_str(&cell);
            }
            if gap > 0 && col + 1 < weeks {
                line.push(' ');
            }
        }
        lines.push(line);
    }

    // 图例行
    let mut legend = String::new();
    legend.push_str(&" ".repeat(label_w + 1));
    legend.push_str("Less ");
    for lv in 0..5 {
        let (r, g, b) = LEVELS[lv];
        let cell = render_cell(t, lv, cell_w);
        if t.color {
            legend.push_str(&t.bg(r, g, b, &cell));
        } else {
            legend.push_str(&cell);
        }
        if lv < 4 {
            legend.push_str(&" ".repeat(gap.max(1)));
        }
    }
    legend.push_str(" More");
    lines.push(legend);

    // 统计行
    let active = nonzero.len();
    lines.push(format!(
        "{} 天有记录 · 峰值 {}/天",
        active,
        fmt_short(max_day)
    ));

    lines
}

/// 渲染单个格子：彩色模式用背景色空格，无颜色模式用密度字符。
fn render_cell(t: &Term, lv: usize, cell_w: usize) -> String {
    if t.color {
        return " ".repeat(cell_w);
    }
    // 密度字符：空格 / ░ ▒ ▓ █
    let ch = match lv {
        0 => " ",
        1 => "░",
        2 => "▒",
        3 => "▓",
        _ => "█",
    };
    ch.repeat(cell_w)
}

/// 根据终端宽度选择格子尺寸（2 格 + 1 间隔 → 1 格 + 1 间隔 → 1 格 + 0 间隔）。
fn fit_cell(term_width: usize, label_w: usize, weeks: usize) -> (usize, usize) {
    let need = |cw: usize, gap: usize| label_w + 1 + weeks * (cw + gap) - gap + 2;
    if need(2, 1) <= term_width {
        (2, 1)
    } else if need(1, 1) <= term_width {
        (1, 1)
    } else {
        (1, 0)
    }
}

/// 月份标签行：当月首出现在某列时，在该列位置居中标注月名。
fn month_label_row(
    t: &Term,
    start_sunday: NaiveDate,
    weeks: usize,
    label_w: usize,
    cell_w: usize,
    gap: usize,
) -> String {
    // 每列若包含当月 1 号，则标记该列（取该月第一次出现）
    let mut marks: Vec<(usize, &str)> = Vec::new();
    let mut seen_months: Vec<u32> = Vec::new();
    for col in 0..weeks {
        let col_start = start_sunday + Duration::days((col * 7) as i64);
        for d in 0..7 {
            let date = col_start + Duration::days(d);
            if date.day() == 1 && !seen_months.contains(&date.month()) {
                seen_months.push(date.month());
                marks.push((col, MONTH_LABELS[(date.month() - 1) as usize]));
                break;
            }
        }
    }

    let mut line = String::new();
    line.push_str(&" ".repeat(label_w + 1));
    let mut ci = 0usize;
    for col in 0..weeks {
        let cell_total = cell_w + if gap > 0 && col + 1 < weeks { gap } else { 0 };
        let label = if ci < marks.len() && marks[ci].0 == col {
            let l = marks[ci].1;
            ci += 1;
            l
        } else {
            ""
        };
        if label.is_empty() {
            line.push_str(&" ".repeat(cell_total));
        } else {
            let lw = str_width(label);
            let pad = (cell_total.saturating_sub(lw)) / 2;
            line.push_str(&" ".repeat(pad));
            line.push_str(&t.bold(label));
            line.push_str(&" ".repeat(cell_total.saturating_sub(pad + lw)));
        }
    }
    line
}
