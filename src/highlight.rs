//! syntect 的薄封装：按语言给代码上色，输出成 mcat 自己的样式。

use crate::style::{Color, Painter, Style};
use crate::terminal;
use std::io::Write;
use std::str::FromStr;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSet,
    ThemeSettings,
};
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
    theme: ThemeSource,
}

enum ThemeSource {
    Fixed(Theme),
    /// `auto`：第一次真正要配色时才查终端背景，没有代码块的文档不付这个延迟。
    Auto(OnceLock<Theme>),
}

/// 一行代码切成若干段，每段一种样式。
pub type Line = Vec<(Style, String)>;

impl Highlighter {
    pub fn new(theme_name: &str) -> Result<Self, String> {
        let theme = match theme_name {
            "auto" => ThemeSource::Auto(OnceLock::new()),
            "mcat-dark" => ThemeSource::Fixed(builtin_theme(true)),
            "mcat-light" => ThemeSource::Fixed(builtin_theme(false)),
            name => {
                let mut themes = ThemeSet::load_defaults();
                let theme = themes
                    .themes
                    .remove(name)
                    .ok_or_else(|| format!("可用主题：{}", Self::theme_names().join(", ")))?;
                ThemeSource::Fixed(theme)
            }
        };
        // 我们自己处理换行，所以用不带换行符的语法定义
        Ok(Highlighter {
            syntaxes: two_face::syntax::extra_no_newlines(),
            theme,
        })
    }

    pub fn theme_names() -> Vec<String> {
        let mut names = vec![
            "auto".to_string(),
            "mcat-dark".to_string(),
            "mcat-light".to_string(),
        ];
        let mut builtins: Vec<String> = ThemeSet::load_defaults().themes.keys().cloned().collect();
        builtins.sort();
        names.extend(builtins);
        names
    }

    /// 当前主题。`auto` 会在这里第一次查终端背景。
    fn theme(&self) -> &Theme {
        match &self.theme {
            ThemeSource::Fixed(theme) => theme,
            ThemeSource::Auto(theme) => theme.get_or_init(|| {
                // 探测不出来时按浅色走，和 mermaid 的默认一致
                builtin_theme(terminal::is_dark().unwrap_or(false))
            }),
        }
    }

    /// 代码块左边那一列背景色。跟随主题背景的明暗，和主题的配色分开。
    pub fn stripe(&self) -> Style {
        let (r, g, b) = if theme_is_dark(self.theme()) {
            (0x4f, 0x5b, 0x66)
        } else {
            (0xb8, 0xc0, 0xcc)
        };
        Style::new().bg(Color::Rgb(r, g, b))
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
        let mut highlighter = HighlightLines::new(syntax, self.theme());
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

/// 代码行左侧的两格前缀：第一格是色条（背景色），第二格是普通空格。
/// 色条是背景色而不是字符，复制出去只有两个前导空格。
pub fn write_code_prefix<W: Write>(p: &mut Painter<W>, stripe: Style) {
    p.write_prefix(stripe, " ");
    p.write_prefix(Style::new(), " ");
}

fn convert(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut s = Style::new().fg(Color::Rgb(fg.r, fg.g, fg.b));
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

struct Palette {
    background: SynColor,
    foreground: SynColor,
    comment: SynColor,
    keyword: SynColor,
    function: SynColor,
    type_: SynColor,
    string: SynColor,
    number: SynColor,
    variable: SynColor,
    macro_: SynColor,
}

const fn rgb(hex: u32) -> SynColor {
    SynColor {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 0xff,
    }
}

fn builtin_theme(dark: bool) -> Theme {
    let p = if dark {
        Palette {
            background: rgb(0x2b303b),
            foreground: rgb(0xc0c5ce),
            comment: rgb(0x939dab),
            keyword: rgb(0xc09cba),
            function: rgb(0x9aacbe),
            type_: rgb(0xebcb8b),
            string: rgb(0xa3be8c),
            number: rgb(0xd68f78),
            variable: rgb(0xd98a95),
            macro_: rgb(0x96b5b4),
        }
    } else {
        Palette {
            background: rgb(0xeff1f5),
            foreground: rgb(0x3b4252),
            comment: rgb(0x5f6b7a),
            keyword: rgb(0x85598a),
            function: rgb(0x3d6a94),
            type_: rgb(0x80621a),
            string: rgb(0x4f7340),
            number: rgb(0xa8532c),
            variable: rgb(0xa33a44),
            macro_: rgb(0x2f6f6b),
        }
    };

    let mut scopes = Vec::new();
    let mut rule = |scope: &str, fg: SynColor, font: Option<FontStyle>| {
        scopes.push(ThemeItem {
            scope: ScopeSelectors::from_str(scope).expect("内置主题的 scope 一定合法"),
            style: StyleModifier {
                foreground: Some(fg),
                background: None,
                font_style: font,
            },
        });
    };

    rule(
        "comment, punctuation.definition.comment",
        p.comment,
        Some(FontStyle::ITALIC),
    );
    rule("keyword, storage", p.keyword, None);
    // 操作符保持正文色，避免 =、->、& 全部变成关键字色
    rule("keyword.operator", p.foreground, None);
    rule(
        "variable, variable.parameter, variable.language",
        p.variable,
        None,
    );
    rule(
        "entity.name.function, variable.function, support.function.any-method",
        p.function,
        None,
    );
    rule("support.function", p.macro_, None);
    rule("support.macro", p.macro_, None);
    rule(
        "support.type, support.class, entity.name.class, entity.name.type, entity.other.inherited-class",
        p.type_,
        None,
    );
    rule("entity.name.tag", p.variable, None);
    rule(
        "entity.other.attribute-name, entity.other.attribute-name.id",
        p.number,
        None,
    );
    rule(
        "string, constant.other.symbol, markup.raw.inline",
        p.string,
        None,
    );
    rule(
        "constant.numeric, constant, keyword.other.unit",
        p.number,
        None,
    );
    rule("constant.character.escape, string.regexp", p.macro_, None);
    rule(
        "punctuation.section.embedded, variable.interpolation",
        p.number,
        None,
    );
    rule("markup.heading, entity.name.section", p.function, None);
    rule("markup.bold", p.type_, Some(FontStyle::BOLD));
    rule("markup.italic", p.keyword, Some(FontStyle::ITALIC));
    rule("string.other.link, markup.deleted", p.variable, None);
    rule("meta.link, meta.image, markup.quote", p.number, None);
    rule("markup.inserted", p.string, None);
    rule("markup.changed", p.keyword, None);
    rule("invalid.illegal", p.variable, Some(FontStyle::UNDERLINE));

    Theme {
        name: Some(if dark { "mcat-dark" } else { "mcat-light" }.to_string()),
        author: Some("mcat".to_string()),
        settings: ThemeSettings {
            foreground: Some(p.foreground),
            background: Some(p.background),
            ..ThemeSettings::default()
        },
        scopes,
    }
}

fn theme_is_dark(theme: &Theme) -> bool {
    let bg = theme.settings.background.unwrap_or(SynColor {
        r: 0,
        g: 0,
        b: 0,
        a: 0xff,
    });
    (0.2126 * f32::from(bg.r) + 0.7152 * f32::from(bg.g) + 0.0722 * f32::from(bg.b)) / 255.0 < 0.5
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

    #[test]
    fn 内置主题有深浅两套() {
        let dark = Highlighter::new("mcat-dark").unwrap();
        let light = Highlighter::new("mcat-light").unwrap();
        assert_eq!(dark.theme().name.as_deref(), Some("mcat-dark"));
        assert_eq!(light.theme().name.as_deref(), Some("mcat-light"));
        assert!(theme_is_dark(dark.theme()));
        assert!(!theme_is_dark(light.theme()));
        assert_ne!(dark.stripe(), light.stripe());
    }

    #[test]
    fn 主题清单把_auto_和内置主题排在最前() {
        let names = Highlighter::theme_names();
        assert_eq!(&names[..3], ["auto", "mcat-dark", "mcat-light"]);
        assert!(names.iter().any(|name| name == "base16-ocean.dark"));
    }

    /// 主题里的函数色是真彩色，不再经过 256 色量化。
    #[test]
    fn 函数名用真彩色() {
        let hl = Highlighter::new("mcat-dark").unwrap();
        let syntax = hl.by_extension("rs").unwrap();
        let lines = hl.lines("fn main() {}\n", syntax);
        let color = lines[0]
            .iter()
            .find(|(_, text)| text == "main")
            .and_then(|(style, _)| style.fg);
        assert_eq!(color, Some(Color::Rgb(0x9a, 0xac, 0xbe)));
    }
}
