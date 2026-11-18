//! syntect 的薄封装：按语言给代码上色，输出成 mcat 自己的样式。

use crate::style::{Style, to_256};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// 语法集里没直接登记的写法，换成能处理同一份代码的语法名。
///
/// 只收「换一份语法仍然正确」的别名，不猜语言；目标名写错就等于没有这条别名。
const ALIASES: &[(&str, &str)] = &[
    // bat 的 fancy 构建丢掉了 JavaScript (Babel) 和 PowerShell：这两个语法
    // 用了 fancy-regex 还不支持的子例程调用。jsx 退到会解析 JSX 的 tsx 语法。
    ("jsx", "TypeScriptReact"),
    ("mjs", "JavaScript"),
    ("cjs", "JavaScript"),
    // bat 的默认映射：*.h 给 C++，比 syntect 默认猜的 Objective-C 更常用
    ("h", "C++"),
    ("jsonc", "JSON"),
    ("json5", "JSON"),
    ("jsonl", "JSON"),
    ("jsonld", "JSON"),
    ("geojson", "JSON"),
    ("ndjson", "JSON"),
    ("sarif", "JSON"),
    ("ron", "Rust"),
    ("nse", "Lua"),
    ("ksh", "Bourne Again Shell (bash)"),
    ("kshrc", "Bourne Again Shell (bash)"),
    ("xsh", "Python"),
    ("xonshrc", "Python"),
    ("containerfile", "Dockerfile"),
];

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
            syntaxes: two_face::syntax::extra_no_newlines(),
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

    /// 按整个文件名找语法。`Dockerfile`、`Makefile`、`.gitignore` 这些没有
    /// 扩展名的写法也在语法的扩展名列表里，直接拿来查。
    pub fn by_file_name(&self, name: &str) -> Option<&SyntaxReference> {
        self.by_alias(name)
            .or_else(|| self.syntaxes.find_syntax_by_extension(name))
    }

    /// 按文件扩展名找语法（不带点）。
    pub fn by_extension(&self, ext: &str) -> Option<&SyntaxReference> {
        self.by_alias(ext)
            .or_else(|| self.syntaxes.find_syntax_by_extension(ext))
    }

    /// 按名字找语法，顺序是语法名、别名、`rust` 这类 token。
    pub fn by_name(&self, name: &str) -> Option<&SyntaxReference> {
        self.syntaxes
            .find_syntax_by_name(name)
            .or_else(|| self.by_alias(name))
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

    fn by_alias(&self, token: &str) -> Option<&SyntaxReference> {
        let (_, name) = ALIASES
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(token))?;
        self.syntaxes.find_syntax_by_name(name)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn highlighter() -> Highlighter {
        Highlighter::new("base16-ocean.dark").unwrap()
    }

    fn name(syntax: Option<&SyntaxReference>) -> &str {
        syntax.expect("应该有语法").name.as_str()
    }

    /// 默认语法集没有的那些语言，靠 bat 的语法集补上。
    #[test]
    fn 认得默认语法集之外的语言() {
        let hl = highlighter();
        for (ext, expected) in [
            ("ts", "TypeScript"),
            ("tsx", "TypeScriptReact"),
            ("toml", "TOML"),
            ("swift", "Swift"),
            ("kt", "Kotlin"),
            ("scss", "SCSS"),
            ("zig", "Zig"),
        ] {
            assert_eq!(name(hl.by_extension(ext)), expected, "{ext}");
        }
    }

    /// 文件名本身也能查：没有扩展名的文件不能因此掉成纯文本。
    #[test]
    fn 按文件名认出没有扩展名的文件() {
        let hl = highlighter();
        for (file, expected) in [
            ("Dockerfile", "Dockerfile"),
            ("Makefile", "Makefile"),
            ("CMakeLists.txt", "CMake"),
            (".gitignore", "Git Ignore"),
            (".env", "DotENV"),
            ("Cargo.lock", "TOML"),
            ("Gemfile", "Ruby"),
        ] {
            assert_eq!(name(hl.by_file_name(file)), expected, "{file}");
        }
    }

    /// 语法集里没有直接登记的写法走别名；`-l` 和 Markdown 代码块也走同一张表。
    #[test]
    fn 别名补上语法集没有登记的写法() {
        let hl = highlighter();
        assert_eq!(name(hl.by_extension("jsx")), "TypeScriptReact");
        assert_eq!(name(hl.by_extension("mjs")), "JavaScript");
        assert_eq!(name(hl.by_extension("jsonc")), "JSON");
        assert_eq!(name(hl.by_extension("h")), "C++");
        assert_eq!(name(hl.by_name("jsx")), "TypeScriptReact");
        assert_eq!(name(hl.by_file_name("Containerfile")), "Dockerfile");
        // 别名之外的名字不受影响
        assert_eq!(name(hl.by_extension("rs")), "Rust");
        assert!(hl.by_extension("no-such-ext").is_none());
    }

    #[test]
    fn 首行_shebang_能认出语言() {
        let hl = highlighter();
        assert_eq!(name(hl.by_first_line("#!/usr/bin/env python3")), "Python");
    }
}
