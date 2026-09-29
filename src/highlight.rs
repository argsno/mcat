//! syntect 的薄封装：按语言给代码上色，输出成 mcat 自己的样式。

use crate::style::{Style, to_256};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

/// 一行代码切成若干段，每段一种样式。
pub type Line = Vec<(Style, String)>;

impl Highlighter {
    pub fn new(theme_name: &str) -> Result<Self, String> {
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove(theme_name)
            .ok_or_else(|| format!("可用主题：{}", Self::theme_names().join(", ")))?;
        // 我们自己处理换行，所以用不带换行符的语法定义
        Ok(Highlighter {
            syntaxes: SyntaxSet::load_defaults_nonewlines(),
            theme,
        })
    }

    pub fn theme_names() -> Vec<String> {
        let mut names: Vec<String> = ThemeSet::load_defaults().themes.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn plain(&self) -> &SyntaxReference {
        self.syntaxes.find_syntax_plain_text()
    }

    /// 判断是不是「无语法」的纯文本。
    pub fn is_plain(&self, syntax: &SyntaxReference) -> bool {
        std::ptr::eq(syntax, self.plain())
    }

    /// 按文件扩展名找语法（不带点）。
    pub fn by_extension(&self, ext: &str) -> Option<&SyntaxReference> {
        self.syntaxes.find_syntax_by_extension(ext)
    }

    /// 按名字找语法，先试 `Rust`，再试 `rust` 这类 token。
    pub fn by_name(&self, name: &str) -> Option<&SyntaxReference> {
        self.syntaxes
            .find_syntax_by_name(name)
            .or_else(|| self.syntaxes.find_syntax_by_token(name))
    }

    /// 认不出扩展名时，看第一行像什么语言。
    pub fn by_first_line(&self, line: &str) -> Option<&SyntaxReference> {
        self.syntaxes.find_syntax_by_first_line(line)
    }

    /// 逐行上色。状态在行之间延续，所以多行字符串（比如三引号）能正确处理。
    pub fn lines(&self, code: &str, syntax: &SyntaxReference) -> Vec<Line> {
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();
        for line in code.lines() {
            let ranges = highlighter
                .highlight_line(line, &self.syntaxes)
                .unwrap_or_else(|_| vec![(syntect::highlighting::Style::default(), line)]);
            out.push(
                ranges
                    .into_iter()
                    .map(|(style, text)| (convert(style), text.to_string()))
                    .collect(),
            );
        }
        out
    }
}

fn convert(style: syntect::highlighting::Style) -> Style {
    let mut s = Style::new().fg(to_256(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        s = s.bold();
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        s = s.italic();
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        s = s.underline();
    }
    s
}
