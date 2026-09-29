//! ANSI 样式，以及一个记录当前样式状态的输出器。
//!
//! 输出器只在样式真正变化时写入转义序列，并且在换行时复位，
//! 这样管道出去的文本不会积累无用的转义码。

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::OnceLock;
use unicode_width::UnicodeWidthChar;

/// 行号列宽：数字右对齐占用 5 列，后面跟一个 ` │ ` 分隔。
const NUMBER_WIDTH: usize = 6;
const RESET: &str = "\x1b[0m";

/// 256 色调色板索引。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color(pub u8);

/// 一组 SGR 属性。`fg` 为 `None` 时沿用终端的默认前景色。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

impl Style {
    pub const PLAIN: Style = Style {
        fg: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        strike: false,
    };

    pub const fn new() -> Self {
        Style::PLAIN
    }
    pub const fn fg(mut self, c: Color) -> Self {
        self.fg = Some(c);
        self
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    pub const fn strike(mut self) -> Self {
        self.strike = true;
        self
    }
    pub fn is_plain(&self) -> bool {
        *self == Style::PLAIN
    }

    /// 内外层嵌套时合并：颜色取内层，属性取并集。
    pub fn merge(self, inner: Style) -> Style {
        Style {
            fg: inner.fg.or(self.fg),
            bold: self.bold || inner.bold,
            dim: self.dim || inner.dim,
            italic: self.italic || inner.italic,
            underline: self.underline || inner.underline,
            strike: self.strike || inner.strike,
        }
    }

    /// 生成 SGR 序列，例如 `"\x1b[1;38;5;12m"`。只对非纯样式调用。
    fn sgr(self) -> String {
        let mut s = String::from("\x1b[");
        let mut first = true;
        let mut push = |part: &str| {
            if !first {
                s.push(';');
            }
            s.push_str(part);
            first = false;
        };
        if self.bold {
            push("1");
        }
        if self.dim {
            push("2");
        }
        if self.italic {
            push("3");
        }
        if self.underline {
            push("4");
        }
        if self.strike {
            push("9");
        }
        if let Some(c) = self.fg {
            push(&format!("38;5;{}", c.0));
        }
        s.push('m');
        s
    }
}

/// 计算字符串的显示宽度，忽略 ANSI 转义序列。
/// 宽字符（中文、全角符号）按 2 列计算。
pub fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            width += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    width
}

/// 带状态的行输出器。
///
/// 负责：按需写入转义序列、每行结束时复位样式、统计列宽、给行首加行号。
pub struct Painter<W: Write> {
    out: W,
    cur: Style,
    color: bool,
    numbers: bool,
    next_no: usize,
    col: usize,
    at_start: bool,
    line_empty: bool,
    last_blank: bool,
    err: Option<io::Error>,
}

impl<W: Write> Painter<W> {
    pub fn new(out: W) -> Self {
        Painter {
            out,
            cur: Style::PLAIN,
            color: true,
            numbers: false,
            next_no: 1,
            col: 0,
            at_start: true,
            line_empty: true,
            // 文档开头不输出空行
            last_blank: true,
            err: None,
        }
    }

    pub fn with_color(mut self, on: bool) -> Self {
        self.color = on;
        self
    }
    pub fn with_numbers(mut self, on: bool) -> Self {
        self.numbers = on;
        self
    }
    pub fn at_line_start(&self) -> bool {
        self.at_start
    }
    /// 当前行已写入的显示宽度。
    pub fn col(&self) -> usize {
        self.col
    }
    pub fn into_inner(self) -> W {
        self.out
    }
    pub fn take_error(&mut self) -> Option<io::Error> {
        self.err.take()
    }

    fn raw(&mut self, s: &str) {
        if let Err(e) = self.out.write_all(s.as_bytes())
            && self.err.is_none()
        {
            self.err = Some(e);
        }
    }

    /// 让当前生效的样式变成 `style`，只在变化时写转义序列。
    fn apply(&mut self, style: Style) {
        // 关掉颜色时完全不产生转义序列
        if !self.color || style == self.cur {
            return;
        }
        if !self.cur.is_plain() {
            self.raw(RESET);
        }
        self.cur = Style::PLAIN;
        if !style.is_plain() {
            self.raw(&style.sgr());
            self.cur = style;
        }
    }

    /// 行首写行号。行号本身用暗色，随后复位，避免影响正文。
    fn begin_line(&mut self) {
        if !self.numbers || !self.at_start {
            return;
        }
        self.apply(Style::new().dim().fg(Color(8)));
        let no = format!("{:>1$}", self.next_no, NUMBER_WIDTH);
        self.raw(&no);
        self.raw(" │ ");
        self.col += NUMBER_WIDTH + 3;
        self.apply(Style::PLAIN);
    }

    /// 写一段带样式的文本，文本里的换行会被当作换行处理。
    pub fn write(&mut self, style: Style, text: &str) {
        for (i, seg) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            if seg.is_empty() {
                continue;
            }
            self.begin_line();
            self.apply(style);
            self.raw(seg);
            self.col += display_width(seg);
            self.at_start = false;
            self.line_empty = false;
        }
    }

    /// 写行首缩进 / 竖线之类的前缀。不算作行内内容，
    /// 因此空行加上前缀后仍然被视作空行。
    pub fn write_prefix(&mut self, style: Style, prefix: &str) {
        if prefix.is_empty() {
            return;
        }
        self.begin_line();
        self.apply(style);
        self.raw(prefix);
        self.cur = Style::PLAIN;
        self.col += display_width(prefix);
        self.at_start = false;
    }

    pub fn newline(&mut self) {
        self.apply(Style::PLAIN);
        self.raw("\n");
        self.last_blank = self.line_empty;
        self.line_empty = true;
        self.at_start = true;
        self.col = 0;
        self.next_no += 1;
    }

    /// 需要块之间留白时调用；如果上一行已经是空行就什么都不做。
    pub fn blank_line(&mut self) {
        if !self.at_start || !self.last_blank {
            self.newline();
        }
    }

    /// 原样写入一段已经渲染好的文本（可能含转义序列和多行）。
    /// 用于把子渲染结果贴回父渲染。
    pub fn write_raw(&mut self, text: &str) {
        for (i, seg) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            if seg.is_empty() {
                continue;
            }
            self.begin_line();
            self.raw(seg);
            // 片段自带完整样式，交给它控制后续状态
            self.cur = Style::PLAIN;
            self.col += display_width(seg);
            self.at_start = false;
            self.line_empty = false;
        }
    }

    /// 写入多行原文，保留原有的换行结构（包括结尾有没有换行）。
    /// 行号会加在每一行前面。
    pub fn write_block(&mut self, text: &str) {
        for chunk in text.split_inclusive('\n') {
            let (line, has_nl) = match chunk.strip_suffix('\n') {
                Some(l) => (l, true),
                None => (chunk, false),
            };
            self.begin_line();
            if !line.is_empty() {
                self.raw(line);
                self.cur = Style::PLAIN;
                self.col += display_width(line);
                self.at_start = false;
                self.line_empty = false;
            }
            if has_nl {
                self.newline();
            }
        }
    }

    /// 写入原始字节（内容不是 UTF-8 时的兜底路径）。
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            let (line, has_nl) = match chunk.strip_suffix(b"\n") {
                Some(l) => (l, true),
                None => (chunk, false),
            };
            self.begin_line();
            if !line.is_empty() {
                self.raw_bytes(line);
                self.cur = Style::PLAIN;
                self.at_start = false;
                self.line_empty = false;
            }
            if has_nl {
                self.newline();
            }
        }
    }

    fn raw_bytes(&mut self, bytes: &[u8]) {
        if let Err(e) = self.out.write_all(bytes)
            && self.err.is_none()
        {
            self.err = Some(e);
        }
    }
}

/// 实现了 `io::Write` 的字符串缓冲区，让 `Painter` 也能往内存里写。
#[derive(Default)]
pub struct TextBuf {
    buf: String,
}

impl TextBuf {
    pub fn new() -> Self {
        TextBuf::default()
    }
    pub fn into_text(self) -> String {
        self.buf
    }
}

impl Write for TextBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text =
            std::str::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.buf.push_str(text);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Painter<TextBuf> {
    pub fn into_text(self) -> String {
        self.into_inner().into_text()
    }
}

/// RGB → 256 色索引。在 256 色调色板里找最接近的颜色。
///
/// syntect 的主题用真彩色定义，直接输出在不支持真彩色的终端上会退化，
/// 统一映射到 256 色可以让所有终端表现一致。
pub fn to_256(r: u8, g: u8, b: u8) -> Color {
    thread_local! {
        static CACHE: std::cell::RefCell<HashMap<(u8, u8, u8), u8>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let idx = CACHE.with(|cache| {
        *cache
            .borrow_mut()
            .entry((r, g, b))
            .or_insert_with(|| nearest(r, g, b))
    });
    Color(idx)
}

/// xterm 的 16 个系统色。终端通常会重新映射这些值。
const SYSTEM: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// 6×6×6 立方体和 24 级灰阶。
fn palette() -> &'static Vec<(u8, u8, u8)> {
    static PALETTE: OnceLock<Vec<(u8, u8, u8)>> = OnceLock::new();
    PALETTE.get_or_init(|| {
        let mut p: Vec<(u8, u8, u8)> = SYSTEM.to_vec();
        let levels = [0u8, 95, 135, 175, 215, 255];
        for r in levels {
            for g in levels {
                for b in levels {
                    p.push((r, g, b));
                }
            }
        }
        for i in 0..24 {
            let v = (8 + i * 10) as u8;
            p.push((v, v, v));
        }
        p
    })
}

fn nearest(r: u8, g: u8, b: u8) -> u8 {
    let pal = palette();
    let mut best = 0usize;
    let mut best_d = i64::MAX;
    for (i, (pr, pg, pb)) in pal.iter().enumerate() {
        // 按人眼权重计算平方距离
        let dr = r as i64 - *pr as i64;
        let dg = g as i64 - *pg as i64;
        let db = b as i64 - *pb as i64;
        let d = 3 * dr * dr + 6 * dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i;
            if d == 0 {
                break;
            }
        }
    }
    best as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(painter: Painter<TextBuf>) -> String {
        painter.into_text()
    }

    #[test]
    fn 显示宽度忽略转义序列() {
        let styled = "\x1b[1;38;5;12m你好\x1b[0m";
        assert_eq!(display_width(styled), 4);
    }

    #[test]
    fn 显示宽度按两列算宽字符() {
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
    }

    #[test]
    fn 关掉颜色时不写任何转义序列() {
        let mut p = Painter::new(TextBuf::new()).with_color(false);
        p.write(Style::new().bold().fg(Color(1)), "红");
        p.write(Style::new().fg(Color(2)), "绿");
        p.newline();
        assert_eq!(plain(p), "红绿\n");
    }

    #[test]
    fn 样式没变就不重复写转义序列() {
        let style = Style::new().fg(Color(3));
        let mut p = Painter::new(TextBuf::new());
        p.write(style, "a");
        p.write(style, "b");
        // 中间不重复写转义序列，也就不需要中间的复位
        assert_eq!(plain(p), "\x1b[38;5;3mab");
    }

    #[test]
    fn 每行结束都会复位() {
        let mut p = Painter::new(TextBuf::new());
        p.write(Style::new().bold(), "一");
        p.newline();
        p.write(Style::new().italic(), "二");
        p.newline();
        assert_eq!(plain(p), "\x1b[1m一\x1b[0m\n\x1b[3m二\x1b[0m\n");
    }

    #[test]
    fn 嵌套样式合并成并集() {
        let bold = Style::new().bold();
        let link = Style::new().fg(Color(12)).underline();
        let merged = bold.merge(link);
        assert!(merged.bold && merged.underline);
        assert_eq!(merged.fg, Some(Color(12)));
    }

    #[test]
    fn 写原文时保留结尾换行的有无() {
        let mut with_nl = Painter::new(TextBuf::new());
        with_nl.write_block("a\nb\n");
        assert_eq!(plain(with_nl), "a\nb\n");

        let mut without = Painter::new(TextBuf::new());
        without.write_block("a\nb");
        assert_eq!(plain(without), "a\nb");
    }

    #[test]
    fn 行号覆盖空行并连续编号() {
        let mut p = Painter::new(TextBuf::new())
            .with_numbers(true)
            .with_color(false);
        p.write_block("a\n\nb\n");
        assert_eq!(plain(p), "     1 │ a\n     2 │ \n     3 │ b\n");
    }

    #[test]
    fn 连续空行不会被叠加() {
        let mut p = Painter::new(TextBuf::new());
        p.newline();
        p.blank_line();
        p.blank_line();
        assert_eq!(plain(p), "\n");
    }

    #[test]
    fn 缩进前缀不占行内容() {
        // 前缀写了空格，但这一行仍然算空行，不会被当成内容
        let mut p = Painter::new(TextBuf::new());
        p.write_prefix(Style::new(), "  ");
        p.newline();
        p.blank_line();
        assert_eq!(plain(p), "  \n");
    }

    #[test]
    fn 调色板里的原色映射回自己() {
        assert_eq!(to_256(0, 0, 0).0, 0);
        assert_eq!(to_256(255, 255, 255).0, 15);
        // 正好等于系统色的，映射回系统色（终端可以重新定义它们）
        assert_eq!(to_256(0, 0, 255).0, 12);
        assert_eq!(to_256(0, 255, 255).0, 14);
        // 立方体和灰阶的原色
        assert_eq!(to_256(95, 135, 175).0, 67);
        assert_eq!(to_256(18, 18, 18).0, 233);
        assert_eq!(to_256(238, 238, 238).0, 255);
    }
}
