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

    /// 图片最多占多少行。不给时：嵌在 Markdown 里用 20，单独看一张图铺满终端
    #[arg(long, value_name = "N")]
    image_rows: Option<usize>,

    /// 图片最多占多少列。不给时按终端宽度
    #[arg(long, value_name = "N")]
    image_cols: Option<usize>,

    /// 不输出内容，只报告图片功能的每一项决策，排查用
    #[arg(long)]
    check_images: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.check_images {
        return check_images(&cli);
    }

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

/// 报告图片链路上每一环的决策。图片没显示出来时，用它定位卡在哪一步。
fn check_images(cli: &Cli) -> ExitCode {
    let is_terminal = io::stdout().is_terminal();
    let detected = Protocol::detect();
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();

    println!(
        "终端程序    {}",
        if program.is_empty() {
            "（未设置 TERM_PROGRAM）"
        } else {
            &program
        }
    );
    println!("TERM        {}", std::env::var("TERM").unwrap_or_default());
    println!(
        "stdout      {}",
        if is_terminal {
            "终端 ✓ 会画图"
        } else {
            "不是终端 ✗ 只会输出文字占位"
        }
    );
    let protocol_text = if cli.no_images {
        "已用 --no-images 关掉".to_string()
    } else if cli.image_protocol != "auto" {
        format!("{}（--image-protocol 指定）", cli.image_protocol)
    } else {
        match detected {
            Some(p) => format!("{p:?}（从环境变量推断）"),
            None => "猜不到 ✗ 只会输出文字占位".to_string(),
        }
    };
    println!("图片协议    {protocol_text}");

    if !is_terminal {
        println!("\n注意：stdout 不是终端。下面照常分析每张图，但实际运行时不会画出来。");
    }
    let Some(images) = image_backend(cli) else {
        if detected.is_none() {
            println!(
                "\n协议猜不到。先在终端里跑一次，或用 --image-protocol kitty / iterm2 强制指定。"
            );
        }
        return ExitCode::SUCCESS;
    };
    let sizing = images.sizing();
    let given = |v: Option<usize>| v.map_or("自动".to_string(), |n| n.to_string());
    println!(
        "显示上限    嵌入 {}x{} 格，单独看图 {}x{} 格（--image-cols {}，--image-rows {}）",
        sizing.max_cols,
        sizing.max_rows,
        images.full_sizing().max_cols,
        images.full_sizing().max_rows,
        given(cli.image_cols),
        given(cli.image_rows)
    );

    if cli.files.is_empty() {
        println!("\n没有指定文件，没什么可查。");
        return ExitCode::SUCCESS;
    }

    let mut total = 0usize;
    let mut ok = 0usize;
    for path in &cli.files {
        let source = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                println!("\n{} 读不了：{e}", path.display());
                continue;
            }
        };
        // 文件本身就是图片时没有 Markdown 树，直接把它当成唯一一张待查的图
        let standalone = Images::is_image(source.as_bytes());
        let (urls, renderer) = if standalone {
            (
                vec![path.to_string_lossy().into_owned()],
                images.for_image(),
            )
        } else {
            let renderer = images.for_file(base_dir(Some(path)));
            (markdown::image_urls(&source), renderer)
        };
        let self_protocol = images.protocol();
        for url in urls {
            total += 1;
            println!("\n{}", url);
            match renderer.prepare(&url) {
                Ok(info) => {
                    ok += 1;
                    if let Some(local) = &info.local_path {
                        println!("  文件      {}", local.display());
                    } else if info.from_cache {
                        println!("  来源      远程（用缓存，没重新下载）");
                    } else {
                        println!("  来源      远程（刚下载）");
                    }
                    let pixel = if info.source_pixel == info.encoded_pixel {
                        format!("{}x{}", info.source_pixel.0, info.source_pixel.1)
                    } else {
                        format!(
                            "{}x{} -> {}x{}",
                            info.source_pixel.0,
                            info.source_pixel.1,
                            info.encoded_pixel.0,
                            info.encoded_pixel.1
                        )
                    };
                    println!("  格式      {}（{pixel} 像素）", info.format);
                    // Kitty 协议下只发一个维度，另一个由终端按图片宽高比算
                    let sent = match (self_protocol, info.axis) {
                        (Protocol::Kitty, image::Axis::Columns) => {
                            format!("c={}（行数由终端按图片宽高比算）", info.cells.0)
                        }
                        (Protocol::Kitty, image::Axis::Rows) => {
                            format!("r={}（列数由终端算）", info.cells.1)
                        }
                        (Protocol::Iterm2, _) => {
                            format!("{} 列 x {} 行", info.cells.0, info.cells.1)
                        }
                    };
                    println!(
                        "  显示      {sent}（估算 {}x{} 格）",
                        info.cells.0, info.cells.1
                    );
                    println!(
                        "  载荷      {} 字节 -> base64 {} 字符 -> {} 块",
                        info.payload, info.encoded, info.chunks
                    );
                    if info.transcoded {
                        println!(
                            "  处理      转码成 PNG（当前协议只支持 PNG）{}",
                            if info.downscaled { " + 降采样" } else { "" }
                        );
                    } else if info.downscaled {
                        println!("  处理      降采样到显示尺寸");
                    }
                    println!("  序列      {}", preview(&info.sequence));
                    println!("  结论      ✓ 会输出图形序列");
                }
                Err(reason) => println!("  结论      ✗ {reason} -> 退回文字占位"),
            }
        }
    }
    println!("\n共 {total} 张图，{ok} 张会画出来");
    ExitCode::SUCCESS
}

/// 转义序列太长没法直接看，截头部加省略号。
/// 把不可见的 ESC 显示出来，不然报告里看不出序列的边界。
fn preview(sequence: &str) -> String {
    let head: String = sequence
        .chars()
        .take(48)
        .map(|c| {
            if c == '\x1b' {
                "<ESC>".to_string()
            } else {
                c.to_string()
            }
        })
        .collect();
    format!("{head}…（共 {} 字节）", sequence.len())
}

/// 决定要不要画图片。stdout 不是终端就不画。
fn setup_images(cli: &Cli, is_terminal: bool) -> Option<Images> {
    if !is_terminal {
        return None;
    }
    image_backend(cli)
}

/// 按参数和终端环境算出图片后端。
/// 诊断模式不走 `setup_images`：否则 `--check-images` 在重定向输出时什么都不说。
fn image_backend(cli: &Cli) -> Option<Images> {
    if cli.no_images {
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
    // 拿不到终端尺寸就用 80x24 兜底
    let term = terminal_size::terminal_size();
    let term_cols = term.map_or(80, |(w, _)| w.0 as usize);
    let term_rows = term.map_or(24, |(_, h)| h.0 as usize);

    // 嵌在 Markdown 里的图要给正文留位置，单独看一张图就铺满
    let cols = cli.image_cols.unwrap_or(term_cols).clamp(1, 1000);
    let embedded = Sizing {
        max_cols: cols,
        max_rows: cli.image_rows.unwrap_or(20).max(1),
    };
    let full = Sizing {
        max_cols: cols,
        max_rows: cli.image_rows.unwrap_or(term_rows).max(1),
    };
    Some(Images::new(protocol, embedded, full))
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
    // 直接给一张图片（mcat a.png）：画出来，不做文本渲染。
    // 认内容而不是扩展名，扩展名经常骗人。
    // 标准输入不参与判断，免得 `git log -p | mcat` 的行为变掉。
    if path.is_some()
        && !cli.no_render
        && !cli.no_images
        && let Some(images) = images
        && Images::is_image(bytes)
    {
        // 图片 id 按真实路径算，所以传绝对路径进来
        if let Some(path) = path {
            let renderer = images.for_image();
            match renderer.prepare(&path.to_string_lossy()) {
                Ok(info) => {
                    out.write_control(&info.sequence);
                    out.newline();
                }
                Err(reason) => report(path, &io::Error::other(reason)),
            }
        }
        return;
    }

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
    if cli.no_render || cli.no_images {
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
