//! mcat —— 一个会渲染 Markdown 的 cat。
//!
//! 读文件（或者标准输入），Markdown 渲染成终端样式，
//! 其他文件按扩展名猜语言做语法高亮，语义和 cat 保持一致：
//! 多个文件依次输出到标准输出，读不了的文件报错后继续。

mod highlight;
mod markdown;
mod style;

use clap::Parser;
use highlight::Highlighter;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use style::Painter;
use syntect::parsing::SyntaxReference;

/// 这些扩展名按 Markdown 处理。
const MD_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "mdx"];

#[derive(Parser, Debug)]
#[command(
    version,
    about = "带 Markdown 渲染的 cat",
    long_about = "带 Markdown 渲染的 cat。\
                  Markdown 文件渲染成终端样式，其他文件按语言做语法高亮。\
                  不给文件时读标准输入。"
)]
struct Cli {
    /// 要输出的文件，`-` 表示标准输入
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// 给每一行加行号
    #[arg(short = 'n', long)]
    numbers: bool,

    /// 强制指定语言：md、text、rust 等，不靠扩展名猜
    #[arg(short = 'l', long, value_name = "LANG")]
    language: Option<String>,

    /// 代码高亮主题，深色终端请用 .dark 结尾的主题
    #[arg(long, default_value = "base16-ocean.dark", value_name = "THEME")]
    theme: String,

    /// 不渲染，原样输出（等同 cat）
    #[arg(long)]
    no_render: bool,

    /// 不输出颜色
    #[arg(long)]
    no_color: bool,

    /// 列出所有可用主题后退出
    #[arg(long)]
    list_themes: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list_themes {
        let mut out = io::stdout().lock();
        for name in Highlighter::theme_names() {
            let _ = writeln!(out, "{name}");
        }
        return ExitCode::SUCCESS;
    }

    let hl = match Highlighter::new(&cli.theme) {
        Ok(hl) => hl,
        Err(e) => {
            eprintln!("mcat: 主题 {:?} 不可用：{e}", cli.theme);
            return ExitCode::FAILURE;
        }
    };

    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    let mut out = Painter::new(&mut writer)
        .with_color(!cli.no_color)
        .with_numbers(cli.numbers);
    let mut failed = false;

    if cli.files.is_empty() {
        if let Err(e) = read_stdin(&mut out, &cli, &hl) {
            report(Path::new("-"), &e);
            failed = true;
        }
    } else {
        for path in &cli.files {
            if path == Path::new("-") {
                if let Err(e) = read_stdin(&mut out, &cli, &hl) {
                    report(Path::new("-"), &e);
                    failed = true;
                }
                continue;
            }
            match fs::read(path) {
                Ok(bytes) => emit(&mut out, &cli, &hl, Some(path), &bytes),
                Err(e) => {
                    report(path, &e);
                    failed = true;
                }
            }
        }
    }

    let mut broken = out.take_error();
    if let Err(e) = writer.flush()
        && broken.is_none()
    {
        broken = Some(e);
    }
    if let Some(e) = broken {
        // 管道被关掉（比如 mcat big.md | head）不算错误
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("mcat: 写入输出失败：{e}");
            return ExitCode::FAILURE;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn report(path: &Path, e: &io::Error) {
    eprintln!("mcat: {}: {e}", path.display());
}

fn read_stdin(
    out: &mut Painter<&mut BufWriter<io::StdoutLock>>,
    cli: &Cli,
    hl: &Highlighter,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    io::stdin().lock().read_to_end(&mut bytes)?;
    emit(out, cli, hl, None, &bytes);
    Ok(())
}

/// 内容决定怎么输出。写入失败由 `Painter` 记录，这里不用返回错误。
fn emit(
    out: &mut Painter<&mut BufWriter<io::StdoutLock>>,
    cli: &Cli,
    hl: &Highlighter,
    path: Option<&Path>,
    bytes: &[u8],
) {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        // 不是 UTF-8 就别乱渲染了，原样输出
        Err(_) => {
            out.write_bytes(bytes);
            return;
        }
    };

    let first_line = text.lines().next().unwrap_or_default();
    match resolve(cli, hl, path, first_line) {
        Mode::Markdown => {
            let rendered = markdown::Markdown::new(hl, !cli.no_color).render(text);
            out.write_block(&rendered);
        }
        Mode::Syntax(syntax) => {
            // 逐行写出。原文结尾没有换行时也不补，保持和 cat 一致
            let lines = hl.lines(text, syntax);
            let trailing = text.ends_with('\n');
            let last = lines.len();
            for (i, line) in lines.into_iter().enumerate() {
                for (style, part) in line {
                    out.write(style, &part);
                }
                if trailing || i + 1 < last {
                    out.newline();
                }
            }
        }
        Mode::Raw => out.write_block(text),
    }
}

enum Mode<'a> {
    Markdown,
    Syntax(&'a SyntaxReference),
    /// 原样输出，不加任何样式
    Raw,
}

fn resolve<'a>(cli: &Cli, hl: &'a Highlighter, path: Option<&Path>, first_line: &str) -> Mode<'a> {
    if cli.no_render {
        return Mode::Raw;
    }

    // 显式指定的语言优先
    if let Some(lang) = &cli.language {
        match lang.to_ascii_lowercase().as_str() {
            "md" | "markdown" => return Mode::Markdown,
            "text" | "txt" | "plain" | "none" => return Mode::Raw,
            name => {
                if let Some(syntax) = hl.by_name(name) {
                    return Mode::Syntax(syntax);
                }
            }
        }
    }

    // 标准输入没有文件名，默认按 Markdown 处理
    let Some(path) = path else {
        return Mode::Markdown;
    };

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if MD_EXTS.contains(&ext.as_str()) {
        return Mode::Markdown;
    }

    let syntax = hl
        .by_extension(&ext)
        // 扩展名不认识时看第一行，像不像某种语言
        .or_else(|| hl.by_first_line(first_line));

    // 纯文本不需要上色，加一层没用的转义序列只是噪音
    match syntax {
        Some(syntax) if !hl.is_plain(syntax) => Mode::Syntax(syntax),
        _ => Mode::Raw,
    }
}
