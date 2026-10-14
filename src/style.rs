//! ANSI 样式，以及一个记录当前样式状态的输出器。
//!
//! 输出器只在样式真正变化时写入转义序列，并且在换行时复位，
//! 这样管道出去的文本不会积累无用的转义码。

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::OnceLock;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 行号列宽：数字右对齐占用 5 列，后面跟一个 ` │ ` 分隔。
const NUMBER_WIDTH: usize = 6;
/// 行号连同分隔一共占掉的列数。表格折行时要把它从终端宽度里扣掉。
pub const NUMBER_COLS: usize = NUMBER_WIDTH + 3;
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

    /// 解析 mcat 自己写出的 SGR 序列，得到它执行之后的样式。
    /// 只认 `sgr` 会写出来的那些参数，用来在折行后恢复切点处的样式。
    fn apply_sgr(self, seq: &str) -> Style {
        let Some(params) = seq.strip_prefix("\x1b[").and_then(|s| s.strip_suffix('m')) else {
            return self;
        };
        let nums: Vec<u16> = params.split(';').filter_map(|p| p.parse().ok()).collect();
        let mut style = self;
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => style = Style::PLAIN,
                1 => style.bold = true,
                2 => style.dim = true,
                3 => style.italic = true,
                4 => style.underline = true,
                9 => style.strike = true,
                38 if nums.get(i + 1) == Some(&5) => {
                    style.fg = nums.get(i + 2).map(|n| Color(*n as u8));
                    i += 2;
                }
                _ => {}
            }
            i += 1;
        }
        style
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
///
/// 宽度按终端的算法来：宽字符（中文、全角符号）占 2 列，多码点的
/// emoji（变体选择符、肤色修饰、ZWJ 组合）整体占 2 列。转义序列会
/// 把字符串切成几段分别计算，好在样式边界不会切进一个 emoji 组合。
pub fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut rest = s;
    while let Some(esc) = rest.find('\x1b') {
        width += UnicodeWidthStr::width(&rest[..esc]);
        // 转义序列从 ESC 到第一个 'm' 为止，不占列
        rest = match rest[esc..].find('m') {
            Some(end) => &rest[esc + end + 1..],
            None => "",
        };
    }
    width + UnicodeWidthStr::width(rest)
}

/// 原样写完一段文本之后，终端的样式状态还确定吗？
///
/// 片段不带转义序列时它不会改变样式（调用方负责先把状态复位）；
/// 带转义序列但以完整复位收尾时，状态明确是干净的——复位之后补的空格
/// 不改变样式，可以一起忽略。其余情况只能当作不确定：下一次写样式前
/// 必须先补一个复位，否则那个样式会漏到后面去。
fn leaves_style_unknown(fragment: &str) -> bool {
    let fragment = fragment.trim_end();
    fragment.contains('\x1b') && !fragment.ends_with(RESET)
}

/// 折行时的最小单位：一段转义序列，或者一个字符（带上它当时的样式）。
///
/// 折行要在任何位置把行切开，切开之后每行都得能独立显示：行尾要复位，
/// 续行开头要把切点那一刻的样式补回来，所以样式得跟着字符走。
struct Unit<'a> {
    /// 原文：整段转义序列，或者一个字符
    text: &'a str,
    /// 普通字符时是它自己，转义序列时是 `None`
    ch: Option<char>,
    /// 这一段生效的样式
    style: Style,
}

fn units(text: &str) -> Vec<Unit<'_>> {
    let mut out = Vec::new();
    let mut style = Style::PLAIN;
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == 0x1b {
            // mcat 自己只会写 SGR，认到第一个 'm' 为止
            let end = text[i..].find('m').map_or(text.len(), |e| i + e + 1);
            let seq = &text[i..end];
            style = style.apply_sgr(seq);
            out.push(Unit {
                text: seq,
                ch: None,
                style,
            });
            i = end;
        } else {
            let ch = text[i..].chars().next().expect("切片非空");
            out.push(Unit {
                text: &text[i..i + ch.len_utf8()],
                ch: Some(ch),
                style,
            });
            i += ch.len_utf8();
        }
    }
    out
}

/// 把一段带样式的文本按显示宽度折成多行，每行不超过 `width` 格。
///
/// 优先在空格处断行，中文没有空格就在字与字之间断。转义序列不会被切断，
/// 也不会被拆到两行上：每行开头按该行第一个字符的样式重新上色，行尾复位。
pub fn wrap_styled(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let units = units(text);
    let mut lines = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let end = line_break(&units, start, width);
        let line = line_text(&units[start..end]);
        // 切点后面只剩转义序列的话，不占一行
        if !line.is_empty() || lines.is_empty() {
            lines.push(line);
        }
        // 下一行从行尾空格之后开始
        let mut next = end;
        while units.get(next).and_then(|u| u.ch) == Some(' ') {
            next += 1;
        }
        start = next;
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 这个字符是不是接在前一个字符上，拼成同一个字素簇。
/// 折行不能从这里断，否则一个 emoji 会被拆到两行上：
/// ZWJ 后面的字、肤色修饰、成对的区域指示符（国旗）、以及所有零宽字符
/// （组合音、变体选择符、连接符）都属于前一个字符。
fn extends_cluster(prev: Option<char>, ch: char) -> bool {
    let regional = |c: char| matches!(c as u32, 0x1F1E6..=0x1F1FF);
    prev == Some('\u{200d}')
        || matches!(ch as u32, 0x1F3FB..=0x1F3FF)
        || (regional(ch) && prev.is_some_and(regional))
        || UnicodeWidthChar::width(ch) == Some(0)
}

/// 从 `start` 开始最多放到哪一格：返回行尾的下标（不含）。
/// 放不下时优先退到本行最后一个空格之后；行尾的转义序列留给下一行。
fn line_break(units: &[Unit<'_>], start: usize, width: usize) -> usize {
    let mut visible = String::new();
    let mut last_space = None;
    // 本行最后一个可见字符之后的位置，用来把行尾的转义序列甩给下一行
    let mut head_end = start;
    let mut i = start;
    while i < units.len() {
        let Some(ch) = units[i].ch else {
            i += 1;
            continue;
        };
        let mut probe = visible.clone();
        probe.push(ch);
        // 宽度按整行重新量，emoji 组合和宽字符才不会算错
        if !visible.is_empty()
            && !extends_cluster(visible.chars().next_back(), ch)
            && display_width(&probe) > width
        {
            return match last_space {
                Some(space) if space > start => space + 1,
                _ => head_end,
            };
        }
        if ch == ' ' {
            last_space = Some(i);
        }
        visible.push(ch);
        i += 1;
        head_end = i;
    }
    units.len()
}

/// 把一行的内容拼出来：按第一个可见字符的样式上色，带上行内的转义序列，末尾复位。
/// 行首的转义序列由行首样式代替（它就是那些序列累加出来的结果），不重复写。
fn line_text(units: &[Unit<'_>]) -> String {
    let style = units
        .iter()
        .find_map(|u| u.ch.map(|_| u.style))
        .unwrap_or(Style::PLAIN);
    let mut out = String::new();
    if !style.is_plain() {
        out.push_str(&style.sgr());
    }
    for unit in units.iter().skip_while(|u| u.ch.is_none()) {
        out.push_str(unit.text);
    }
    if out.contains('\x1b') {
        out.push_str(RESET);
    }
    out
}

/// 带状态的行输出器。
///
/// 负责：按需写入转义序列、每行结束时复位样式、统计列宽、给行首加行号。
pub struct Painter<W: Write> {
    out: W,
    cur: Style,
    /// 上一次写完之后，终端停在哪种样式上已经不确定了。
    /// 原样片段自带转义序列，Painter 只知道它写进去的字节，不知道里面是什么样式。
    dirty: bool,
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
            dirty: false,
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
        if !self.color || (!self.dirty && style == self.cur) {
            return;
        }
        // 状态不确定时也要复位，否则会把上一个样式带到后面去
        if self.dirty || !self.cur.is_plain() {
            self.raw(RESET);
        }
        self.dirty = false;
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
        self.col += NUMBER_COLS;
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
        // 前缀就停在自己的样式上，后面写正文时由 apply 负责复位。
        // 这里不能改 cur：改了就等于假装终端已经复位，样式会渗进正文。
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

    /// 原样写入终端控制序列（比如图片协议的转义）。
    /// 不计入显示宽度：这些序列在终端里不占字符格，而且长度和宽度无关。
    pub fn write_control(&mut self, text: &str) {
        self.begin_line();
        self.apply(Style::PLAIN);
        self.raw(text);
        self.at_start = false;
        self.line_empty = false;
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
            // 片段自带样式；先复位，免得父层正在生效的样式（表格边框的灰、
            // 列表标记的颜色）渗进片段里的正文
            self.apply(Style::PLAIN);
            self.raw(seg);
            self.dirty = leaves_style_unknown(seg);
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
                self.dirty = leaves_style_unknown(line);
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
                // 原始字节里可能有任何转义序列，状态无从判断
                self.dirty = true;
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

    /// 折行：能放下的原样放，放不下的按显示宽度断。
    #[test]
    fn 折行按显示宽度断句() {
        assert_eq!(wrap_styled("abc", 5), ["abc"]);
        assert_eq!(wrap_styled("hello world", 5), ["hello", "world"]);
        assert_eq!(wrap_styled("一二三四五", 4), ["一二", "三四", "五"]);
        assert_eq!(wrap_styled("", 3), [""]);
        // 一格里塞不下两个汉字的窄列也不能卡住
        assert_eq!(wrap_styled("中文", 1), ["中", "文"]);
    }

    /// 折行不能切断转义序列：行尾复位，续行补回切点处的样式。
    #[test]
    fn 折行保留样式() {
        assert_eq!(
            wrap_styled("\x1b[38;5;3mabcd", 2),
            ["\x1b[38;5;3mab\x1b[0m", "\x1b[38;5;3mcd\x1b[0m"]
        );
        // 切点之后才开始的样式不往前面带
        assert_eq!(
            wrap_styled("ab\x1b[1mcd", 2),
            ["ab", "\x1b[1mcd\x1b[0m"]
        );
    }

    /// 折行也不能把 emoji 组合拆到两行：一个组合整体占两格。
    #[test]
    fn 折行不拆散_emoji_组合() {
        let thumb = "\u{1f44d}\u{1f3fd}";
        assert_eq!(wrap_styled(&format!("{thumb}{thumb}"), 2), [thumb, thumb]);
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(wrap_styled(&format!("{family}{family}"), 2), [family, family]);
        // 窄到只有一格时宁可超出一格，也不把组合拆开
        assert_eq!(wrap_styled(thumb, 1), [thumb]);
    }

    /// 原样片段的样式状态不能假装是干净的，否则片段内外的样式会互相渗。
    #[test]
    fn 片段自带的样式不会渗到外面() {
        // 片段不带样式：写之前要先复位，否则内容会跟着上一段（边框）的灰色走
        let mut p = Painter::new(TextBuf::new());
        p.write(Style::new().fg(Color(8)), "│");
        p.write_raw(" 中文 ");
        p.write(Style::new().fg(Color(8)), "│");
        assert_eq!(plain(p), "\x1b[38;5;8m│\x1b[0m 中文 \x1b[38;5;8m│");

        // 片段自带粗体、没以复位收尾：写完之后必须先复位再上新的样式
        let mut p = Painter::new(TextBuf::new());
        p.write(Style::new().fg(Color(8)), "│");
        p.write_raw(" \x1b[1m名字 ");
        p.write(Style::new().fg(Color(8)), "│");
        assert_eq!(
            plain(p),
            "\x1b[38;5;8m│\x1b[0m \x1b[1m名字 \x1b[0m\x1b[38;5;8m│"
        );
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

    /// 终端按字素簇算宽：多码点的 emoji 整体只占两格。
    /// 逐字符相加会在表格里错位——VS16 变体少算一格，肤色修饰和 ZWJ 组合多算几格。
    /// 码点写成转义：连接符和变体选择符是不可见的，直接写字面量容易在编辑中丢掉。
    #[test]
    fn 显示宽度把多码点_emoji_算一个整体() {
        // 变体选择符把符号变成 emoji 呈现，占两格
        assert_eq!(display_width("\u{26a0}\u{fe0f}"), 2);
        assert_eq!(display_width("\u{2705}"), 2);
        // 肤色修饰符不额外占列
        assert_eq!(display_width("\u{1f44d}\u{1f3fd}"), 2);
        // ZWJ 组合成一个簇
        assert_eq!(
            display_width("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            2
        );
        assert_eq!(display_width("\u{1f9d1}\u{200d}\u{1f4bb}"), 2);
        // 区域指示符成对出现是一个国旗
        assert_eq!(display_width("\u{1f1e8}\u{1f1f3}"), 2);
        // 键帽
        assert_eq!(display_width("\u{31}\u{fe0f}\u{20e3}"), 2);
        // 没有变体选择符的符号还是按基本字符算，组合音也不额外占列
        assert_eq!(display_width("\u{26a0}"), 1);
        assert_eq!(display_width("e\u{301}"), 1);
    }

    /// 转义序列可能把簇切开，按段计算不能因此算错普通文本。
    #[test]
    fn 分段的转义序列不影响普通文本宽度() {
        assert_eq!(display_width("中\u{1b}[1m文\u{1b}[0m"), 4);
        assert_eq!(
            display_width("a\u{1b}[38;5;12m\u{1f44d}\u{1f3fd}\u{1b}[0mb"),
            4
        );
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
