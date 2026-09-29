//! mcat —— 一个会渲染 Markdown 的 cat。
//!
//! 读文件（或者标准输入），Markdown 渲染成终端样式，
//! 其他文件按扩展名猜语言做语法高亮，语义和 cat 保持一致：
//! 多个文件依次输出到标准输出，读不了的文件报错后继续。

mod highlight;
mod image;
mod markdown;
mod style;

use clap::Parser;
use highlight::Highlighter;
use image::{Images, Protocol, Sizing};
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Read, Write};
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

    /// 不显示图片，只输出文字占位
    #[arg(long)]
    no_images: bool,

    /// 图片协议：auto 按终端环境猜，kitty 或 iterm2 强制指定
    #[arg(long, value_name = "PROTO", default_value = "auto")]
    image_protocol: String,

    /// 图片最多占多少行
    #[arg(long, value_name = "N", default_value_t = 20)]
    image_rows: usize,

    /// 图片最多占多少列，0 表示按终端宽度
    #[arg(long, value_name = "N", default_value_t = 0)]
    image_cols: usize,
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

    // 图片只在终端里画得出来，管道和重定向就退回文字占位
    let images = setup_images(&cli, io::stdout().is_terminal());
    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    let mut out = Painter::new(&mut writer)
        .with_color(!cli.no_color)
        .with_numbers(cli.numbers);
    let mut failed = false;

    if cli.files.is_empty() {
        if let Err(e) = read_stdin(&mut out, &cli, &hl, images.as_ref()) {
            report(Path::new("-"), &e);
            failed = true;
        }
    } else {
        for path in &cli.files {
            if path == Path::new("-") {
                if let Err(e) = read_stdin(&mut out, &cli, &hl, images.as_ref()) {
                    report(Path::new("-"), &e);
                    failed = true;
                }
                continue;
            }
            match fs::read(path) {
                Ok(bytes) => emit(&mut out, &cli, &hl, images.as_ref(), Some(path), &bytes),
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

/// 图片相对路径的基准目录：文件所在目录；标准输入用当前目录。
fn base_dir(path: Option<&Path>) -> PathBuf {
    match path {
        Some(path) => path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        None => PathBuf::from("."),
    }
}

fn report(path: &Path, e: &io::Error) {
    eprintln!("mcat: {}: {e}", path.display());
}

fn read_stdin(
    out: &mut Painter<&mut BufWriter<io::StdoutLock>>,
    cli: &Cli,
    hl: &Highlighter,
    images: Option<&Images>,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    io::stdin().lock().read_to_end(&mut bytes)?;
    emit(out, cli, hl, images, None, &bytes);
    Ok(())
}

/// 决定要不要画图片，以及用哪个协议。
fn setup_images(cli: &Cli, is_terminal: bool) -> Option<Images> {
    if cli.no_images || !is_terminal {
        return None;
    }
    let protocol = match cli.image_protocol.to_ascii_lowercase().as_str() {
        "auto" => Protocol::detect()?,
        "kitty" => Protocol::Kitty,
        "iterm2" | "iterm" => Protocol::Iterm2,
        other => {
            eprintln!("mcat: 未知图片协议 {other:?}，可选：auto、kitty、iterm2");
            return None;
        }
    };
    // 拿不到终端宽度就按 80 列算
    let cols = match cli.image_cols {
        0 => terminal_size::terminal_size().map_or(80, |(w, _)| w.0 as usize),
        n => n,
    };
    Some(Images::new(
        protocol,
        Sizing {
            max_cols: cols.clamp(1, 1000),
            max_rows: cli.image_rows.max(1),
        },
    ))
}

/// 内容决定怎么输出。写入失败由 `Painter` 记录，这里不用返回错误。
fn emit(
    out: &mut Painter<&mut BufWriter<io::StdoutLock>>,
    cli: &Cli,
    hl: &Highlighter,
    images: Option<&Images>,
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
            // 图片相对路径以 Markdown 文件所在目录为基准
            let renderer = images.map(|images| images.for_file(base_dir(path)));
            let rendered =
                markdown::Markdown::new(hl, !cli.no_color, renderer.as_ref()).render(text);
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
