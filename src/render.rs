//! 终端渲染基础：ANSI 颜色、显示宽度、数字格式化、面板、条形图。

use std::io::IsTerminal;

/// 渲染配置。
#[derive(Debug, Clone)]
pub struct Term {
    pub color: bool,
    pub width: usize,
}

impl Term {
    pub fn detect(force_no_color: bool) -> Term {
        let color = !force_no_color && std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err();
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse::<usize>().ok())
            .filter(|&w| w >= 40)
            .unwrap_or(88)
            .min(240);
        Term { color, width }
    }

    #[inline]
    pub fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// 前景色（24bit）
    #[inline]
    pub fn fg(&self, r: u8, g: u8, b: u8, s: &str) -> String {
        self.paint(&format!("38;2;{r};{g};{b}"), s)
    }

    /// 背景色（24bit）
    #[inline]
    pub fn bg(&self, r: u8, g: u8, b: u8, s: &str) -> String {
        self.paint(&format!("48;2;{r};{g};{b}"), s)
    }

    #[inline]
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    #[inline]
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
}

/// 字符显示宽度（CJK 全角按 2 计，emoji 按 2 计）。
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1F64F).contains(&cp)
        || (0x1F900..=0x1F9FF).contains(&cp)
        || (0x20000..=0x2FFFD).contains(&cp)
        || (0x30000..=0x3FFFD).contains(&cp);
    if wide {
        2
    } else if cp < 0x20 || (0x7F..=0xA0).contains(&cp) {
        0
    } else {
        1
    }
}

/// 字符串显示宽度。
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 按显示宽度用空格补齐到 target。
pub fn pad(s: &str, target: usize) -> String {
    let w = str_width(s);
    if w >= target {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(target - w))
    }
}

/// 千分位格式化。
pub fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 人类可读缩写：1.23M / 890K / 456。
pub fn fmt_short(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// 百分比（保留 1 位小数）。
pub fn fmt_pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

/// 用 `█` 字符画横向条形图。
pub fn bar(t: &Term, frac: f64, width: usize, color: (u8, u8, u8)) -> String {
    let filled = ((frac.clamp(0.0, 1.0)) * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::new();
    for i in 0..width {
        let c = if i < filled { "█" } else { "░" };
        let c = if i < filled { t.fg(color.0, color.1, color.2, c) } else { t.dim(c) };
        s.push_str(&c);
    }
    s
}

/// 绘制一个带边框的面板，返回各行文本（不含尾部换行）。
/// `lines` 为面板内各行；`title` 可选，显示在顶部边框中间。
pub fn panel(_t: &Term, title: Option<&str>, lines: Vec<String>, width: usize) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let mut out: Vec<String> = Vec::new();

    let top = match title {
        Some(ti) if !ti.is_empty() => {
            let ti_w = str_width(ti);
            let side = inner.saturating_sub(ti_w + 2) / 2;
            let right = inner.saturating_sub(ti_w + 2).saturating_sub(side);
            format!("╭─{}{}─{}╮", "─".repeat(side), ti, "─".repeat(right))
        }
        _ => format!("╭{}╮", "─".repeat(inner)),
    };
    out.push(top);

    for l in lines {
        let lw = str_width(&l);
        let pad_n = inner.saturating_sub(lw);
        out.push(format!("│ {}{} │", l, " ".repeat(pad_n)));
    }

    out.push(format!("╰{}╯", "─".repeat(inner)));
    out
}

/// 空行（面板内部用）。
pub fn blank() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_width() {
        assert_eq!(str_width("abc"), 3);
        assert_eq!(str_width("中文"), 4);
        assert_eq!(str_width("a中b"), 4);
        assert_eq!(str_width("📊"), 2);
    }

    #[test]
    fn test_fmt_num() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(1234), "1,234");
        assert_eq!(fmt_num(1234567), "1,234,567");
    }

    #[test]
    fn test_fmt_short() {
        assert_eq!(fmt_short(999), "999");
        assert_eq!(fmt_short(12000), "12.0K");
        assert_eq!(fmt_short(1_234_567), "1.23M");
    }

    #[test]
    fn test_pad() {
        assert_eq!(pad("中", 3), "中 "); // 中 = 2 宽，补 1 空格
        assert_eq!(pad("ab", 3), "ab ");
    }
}
