//! mcat —— 一个会渲染 Markdown / reStructuredText 的 cat。
//!
//! 读文件（或者标准输入），Markdown / reStructuredText 渲染成终端样式，
//! 其他文件按扩展名猜语言做语法高亮，语义和 cat 保持一致：
//! 多个文件依次输出到标准输出，读不了的文件报错后继续。

mod config;
mod highlight;
mod image;
mod markdown;
mod mermaid;
mod rst;
mod style;
mod table;
mod terminal;

use clap::Parser;
use config::Config;
use highlight::Highlighter;
use image::{Images, Protocol, Sizing};
use mermaid::Theme;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use style::Painter;
use syntect::parsing::SyntaxReference;

/// 这些扩展名按 Markdown 处理。
const MD_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "mdx"];
/// 这些扩展名按 reStructuredText 处理。
const RST_EXTS: &[&str] = &["rst", "rest"];

#[derive(Parser, Debug)]
#[command(
    version,
    about = "带 Markdown / reStructuredText 渲染的 cat",
    long_about = "带 Markdown / reStructuredText 渲染的 cat。\
                  Markdown 和 reStructuredText 文件渲染成终端样式，其他文件按语言做语法高亮。\
                  不给文件时读标准输入。\
                  默认值也可以写在配置文件里，见 --config。",
    // 配置文件里的参数排在真实参数前面，同名参数要让后面的赢
    args_override_self = true
)]
struct Cli {
    /// 要输出的文件，`-` 表示标准输入
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// 给每一行加行号
    #[arg(short = 'n', long, overrides_with = "no_numbers")]
    numbers: bool,

    /// 不加行号，用来覆盖配置文件里的 numbers = true
    #[arg(long, overrides_with = "numbers")]
    no_numbers: bool,

    /// 强制指定语言：md、rst、text、rust 等，不靠扩展名猜
    #[arg(short = 'l', long, value_name = "LANG")]
    language: Option<String>,

    /// 代码高亮主题：auto 跟随终端背景，或 mcat-dark、mcat-light 等
    #[arg(long, default_value = "auto", value_name = "THEME")]
    theme: String,

    /// 不渲染，原样输出（等同 cat）
    #[arg(long)]
    no_render: bool,

    /// 不输出颜色
    #[arg(long, overrides_with = "color")]
    no_color: bool,

    /// 输出颜色，用来覆盖配置文件里的 no_color = true
    #[arg(long, overrides_with = "no_color")]
    color: bool,

    /// 列出所有可用主题后退出
    #[arg(long)]
    list_themes: bool,

    /// 不显示图片，只输出文字占位
    #[arg(long, overrides_with = "images")]
    no_images: bool,

    /// 显示图片，用来覆盖配置文件里的 no_images = true
    #[arg(long, overrides_with = "no_images")]
    images: bool,

    /// 图片协议：auto 按终端环境猜，kitty 或 iterm2 强制指定
    #[arg(long, value_name = "PROTO", default_value = "auto")]
    image_protocol: String,

    /// 图片最多占多少行。不给时：行内图片用 20，独占一段的图和单独看图铺满终端。
    /// 超过终端高度时按终端高度算
    #[arg(long, value_name = "N")]
    image_rows: Option<usize>,

    /// 图片最多占多少列。不给时按终端宽度
    #[arg(long, value_name = "N")]
    image_cols: Option<usize>,

    /// mermaid 图的配色：auto 跟随终端背景，或 light、dark。默认 auto
    #[arg(long, value_name = "THEME", default_value = "auto")]
    mermaid_theme: String,

    /// 不输出内容，只报告图片功能的每一项决策，排查用
    #[arg(long)]
    check_images: bool,

    /// 配置文件（TOML）。不给时依次找 $MCAT_CONFIG、$XDG_CONFIG_HOME/mcat/config.toml、~/.config/mcat/config.toml
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let mut cli = Cli::parse_from(&argv);
    match config::load(cli.config.as_deref()) {
        Ok(Some((path, defaults))) => match merge_config(&argv, &defaults) {
            Ok(merged) => cli = merged,
            Err(e) => {
                eprintln!("mcat: 配置文件 {} 里的配置项没能生效：", path.display());
                let _ = e.print();
                return ExitCode::from(2);
            }
        },
        Ok(None) => {}
        Err(e) => {
            eprintln!("mcat: {e}");
            return ExitCode::from(2);
        }
    }

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
    // mermaid 图走图片链路，所以同样只在终端里画
    let diagrams = images
        .as_ref()
        .map(|images| (mermaid::Renderer::new(mermaid_theme(&cli)), images.sizing()));
    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    let mut out = Painter::new(&mut writer)
        .with_color(!cli.no_color)
        .with_numbers(cli.numbers);
    let mut failed = false;

    if cli.files.is_empty() {
        if let Err(e) = read_stdin(&mut out, &cli, &hl, images.as_ref(), diagrams.as_ref()) {
            report(Path::new("-"), &e);
            failed = true;
        }
    } else {
        for path in &cli.files {
            if path == Path::new("-") {
                if let Err(e) = read_stdin(&mut out, &cli, &hl, images.as_ref(), diagrams.as_ref())
                {
                    report(Path::new("-"), &e);
                    failed = true;
                }
                continue;
            }
            match fs::read(path) {
                Ok(bytes) => emit(
                    &mut out,
                    &cli,
                    &hl,
                    images.as_ref(),
                    diagrams.as_ref(),
                    Some(path),
                    &bytes,
                ),
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

/// 把配置项当成排在真实参数前面的参数，再解析一次。
/// clap 的 `args_override_self` 让后面的真实参数覆盖配置。
fn merge_config(argv: &[OsString], defaults: &Config) -> Result<Cli, clap::Error> {
    let mut argv = argv.to_vec();
    argv.splice(1..1, defaults.args().into_iter().map(OsString::from));
    Cli::try_parse_from(argv)
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
    diagrams: Option<&Diagrams<'_>>,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    io::stdin().lock().read_to_end(&mut bytes)?;
    emit(out, cli, hl, images, diagrams, None, &bytes);
    Ok(())
}

/// mermaid 渲染器，配上它该用的显示尺寸上限。
type Diagrams<'a> = (mermaid::Renderer, Sizing);

/// 解析 `--mermaid-theme`。认不出来时说一句，然后按默认值走——
/// 一个配色选错不该让整个文件渲染不出来。
fn mermaid_theme(cli: &Cli) -> Theme {
    match Theme::parse(&cli.mermaid_theme) {
        Some(theme) => theme,
        None => {
            eprintln!(
                "mcat: 未知 mermaid 主题 {:?}，可选：auto、light、dark",
                cli.mermaid_theme
            );
            Theme::default()
        }
    }
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
        "显示上限    行内 {}x{} 格，独占一段/单独看图 {}x{} 格（--image-cols {}，--image-rows {}）",
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
        // 文件本身就是图片时没有文档树，直接把它当成唯一一张待查的图
        let standalone = Images::is_image(source.as_bytes());
        let rst = is_rst(cli, path);
        let standalone_renderer = standalone.then(|| images.for_image());
        let embedded = (!standalone).then(|| images.for_file(base_dir(Some(path))));
        let block = (!standalone).then(|| images.for_block(base_dir(Some(path))));
        let urls = if standalone {
            vec![(path.to_string_lossy().into_owned(), true)]
        } else if rst {
            rst::image_urls(&source, Some(path))
        } else {
            markdown::image_urls(&source)
        };
        let self_protocol = images.protocol();
        for (url, is_block) in urls {
            let renderer = if let Some(renderer) = &standalone_renderer {
                renderer
            } else if is_block {
                block.as_ref().expect("非单独看图时存在")
            } else {
                embedded.as_ref().expect("非单独看图时存在")
            };
            total += 1;
            println!("\n{}", url);
            println!(
                "  位置      {}",
                if standalone {
                    "单独看图（按整屏高度）"
                } else if is_block {
                    "独占一段（按整屏高度）"
                } else {
                    "行内（默认最多 20 行）"
                }
            );
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

        // mermaid 图走同一条链路，只是字节来自渲染而不是读文件
        if !standalone {
            let preference = mermaid_theme(cli);
            let sizing = images.sizing();
            let blocks = if rst {
                rst::mermaid_sources(&source, Some(path))
            } else {
                markdown::mermaid_sources(&source)
            };
            let renderer = mermaid::Renderer::new(preference);
            if !blocks.is_empty() {
                let source = match preference {
                    Theme::Auto => "跟随终端背景",
                    _ => "--mermaid-theme 指定",
                };
                println!("\nmermaid 主题  {}（{source}）", renderer.theme().name());
            }
            for block in blocks {
                total += 1;
                let first = block.lines().next().unwrap_or_default();
                println!("\n[diagram] {first}");
                match renderer.prepare(&block, &sizing) {
                    Ok(info) => {
                        ok += 1;
                        println!("  来源      源码（{} 字节）渲染", block.len());
                        println!(
                            "  尺寸      {}x{} -> {}x{} 像素",
                            info.source_pixel.0,
                            info.source_pixel.1,
                            info.encoded_pixel.0,
                            info.encoded_pixel.1
                        );
                        println!("  显示      估算 {}x{} 格", info.cells.0, info.cells.1);
                        // 编码交给图片管线，和普通图片走同一条路。用同一份
                        // Prepared，避免为了报告再渲染一遍。
                        let encoder = images.for_file(base_dir(Some(path)));
                        let payload = info.png.len();
                        match encoder.render_png(&mermaid::identity(&block), info.png) {
                            Some(output) => {
                                println!(
                                    "  载荷      {payload} 字节 -> base64 {} 字符",
                                    payload.div_ceil(3) * 4
                                );
                                println!("  序列      {}", preview(&output.sequence));
                                println!("  结论      ✓ 会输出图形序列");
                            }
                            None => {
                                println!("  结论      ✗ 编码成控制序列失败 -> 退回源码");
                            }
                        }
                    }
                    Err(reason) => println!("  结论      ✗ {reason} -> 退回源码"),
                }
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

/// 表格折行可用的终端宽度。
///
/// 只在终端里给：管道和重定向里既拿不到宽度，产物也不该被折行——
/// 和图片「只在终端里画」是同一条规矩。行号会占掉左边几列，先扣掉。
fn table_width(cli: &Cli) -> Option<usize> {
    if !io::stdout().is_terminal() {
        return None;
    }
    let (cols, _) = terminal_size::terminal_size()?;
    let numbers = if cli.numbers { style::NUMBER_COLS } else { 0 };
    Some((cols.0 as usize).saturating_sub(numbers))
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

    // 行内图片要给正文留位置；独占一段的图和单独看一张图铺满终端。
    // --image-rows 不能超过终端高度：超出屏幕的图光标位置没有定义，
    // 后面的文字会压在图上。
    let cols = cli.image_cols.unwrap_or(term_cols).clamp(1, 1000);
    let row_cap = term_rows.max(1);
    let embedded = Sizing {
        max_cols: cols,
        max_rows: cli.image_rows.unwrap_or(20).min(row_cap).max(1),
    };
    let full = Sizing {
        max_cols: cols,
        max_rows: cli.image_rows.unwrap_or(row_cap).min(row_cap).max(1),
    };
    Some(Images::new(protocol, embedded, full))
}

/// 内容决定怎么输出。写入失败由 `Painter` 记录，这里不用返回错误。
fn emit(
    out: &mut Painter<&mut BufWriter<io::StdoutLock>>,
    cli: &Cli,
    hl: &Highlighter,
    images: Option<&Images>,
    diagrams: Option<&Diagrams<'_>>,
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
                    for _ in 0..info.advance {
                        out.newline();
                    }
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
            let base = base_dir(path);
            let renderer = images.map(|images| images.for_file(base.clone()));
            let block_renderer = images.map(|images| images.for_block(base));
            let rendered = markdown::Markdown::new(
                hl,
                !cli.no_color,
                renderer.as_ref(),
                diagrams.map(|(renderer, sizing)| (renderer, *sizing)),
            )
            .with_block_images(block_renderer.as_ref())
            .with_width(table_width(cli))
            .render(text);
            out.write_block(&rendered);
        }
        Mode::Rst => {
            // 图片相对路径以 RST 文件所在目录为基准，和 Markdown 一致
            let base = base_dir(path);
            let renderer = images.map(|images| images.for_file(base.clone()));
            let block_renderer = images.map(|images| images.for_block(base));
            let renderer = rst::Rst::new(
                hl,
                !cli.no_color,
                renderer.as_ref(),
                diagrams.map(|(renderer, sizing)| (renderer, *sizing)),
            )
            .with_block_images(block_renderer.as_ref())
            .with_width(table_width(cli));
            let mut renderer = match path {
                Some(path) => renderer.with_source(path),
                None => renderer,
            };
            let rendered = renderer.render(text);
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
    Rst,
    Syntax(&'a SyntaxReference),
    /// 原样输出，不加任何样式
    Raw,
}

/// 这个文件会不会被当成 reStructuredText 渲染。
/// `--check-images` 用它决定从哪种语法树里收集图片。
fn is_rst(cli: &Cli, path: &Path) -> bool {
    if let Some(lang) = &cli.language {
        return rst_language(lang);
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| RST_EXTS.contains(&ext.as_str()))
}

/// 语言名是不是 reStructuredText 的写法。`-l` 和 `--check-images` 共用一份判断。
fn rst_language(lang: &str) -> bool {
    matches!(
        lang.to_ascii_lowercase().as_str(),
        "rst" | "rest" | "restructuredtext" | "restructured-text"
    )
}

fn resolve<'a>(cli: &Cli, hl: &'a Highlighter, path: Option<&Path>, first_line: &str) -> Mode<'a> {
    // 只有 --no-render 是「什么都别渲染」；--no-images 只关掉图片
    if cli.no_render {
        return Mode::Raw;
    }

    // 显式指定的语言优先
    if let Some(lang) = &cli.language {
        match lang.to_ascii_lowercase().as_str() {
            "md" | "markdown" => return Mode::Markdown,
            "text" | "txt" | "plain" | "none" => return Mode::Raw,
            name if rst_language(name) => return Mode::Rst,
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
    if RST_EXTS.contains(&ext.as_str()) {
        return Mode::Rst;
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    // 先按整个文件名查，再按扩展名：Dockerfile、Makefile、.gitignore
    // 这类文件名就登记在语法的扩展名列表里。
    let syntax = hl
        .by_file_name(file_name)
        .or_else(|| hl.by_extension(&ext))
        // 扩展名不认识时看第一行，像不像某种语言
        .or_else(|| hl.by_first_line(first_line));

    // 纯文本不需要上色，加一层没用的转义序列只是噪音
    match syntax {
        Some(syntax) if !hl.is_plain(syntax) => Mode::Syntax(syntax),
        _ => Mode::Raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlighter() -> Highlighter {
        Highlighter::new("base16-ocean.dark").unwrap()
    }

    #[test]
    fn 不给_language_时按文件名和扩展名认() {
        let hl = highlighter();
        let cli = Cli::parse_from(["mcat"]);
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("Dockerfile")), "FROM alpine"),
            Mode::Syntax(s) if s.name == "Dockerfile"
        ));
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.ts")), "const x = 1"),
            Mode::Syntax(s) if s.name == "TypeScript"
        ));
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("README.md")), "# hi"),
            Mode::Markdown
        ));
    }

    #[test]
    fn rst_按扩展名渲染() {
        let hl = highlighter();
        let cli = Cli::parse_from(["mcat"]);
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("README.rst")), "Title\n====="),
            Mode::Rst
        ));
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.rest")), ""),
            Mode::Rst
        ));
        // 语言参数优先于扩展名
        let cli = Cli::parse_from(["mcat", "-l", "rust"]);
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.rst")), "fn main() {}"),
            Mode::Syntax(s) if s.name == "Rust"
        ));
    }

    #[test]
    fn language_参数可以强制_rst() {
        let hl = highlighter();
        for name in ["rst", "rest", "reStructuredText"] {
            let cli = Cli::parse_from(["mcat", "-l", name]);
            assert!(matches!(
                resolve(&cli, &hl, Some(Path::new("a.txt")), ""),
                Mode::Rst
            ));
        }
    }

    #[test]
    fn language_参数也走别名() {
        let hl = highlighter();
        let cli = Cli::parse_from(["mcat", "-l", "jsx"]);
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.txt")), ""),
            Mode::Syntax(s) if s.name == "TypeScriptReact"
        ));
    }

    /// --no-images 只关图片，不该把源码和 Markdown 一起退化成 cat。
    #[test]
    fn no_images_不关文字渲染() {
        let hl = highlighter();
        let cli = Cli::parse_from(["mcat", "--no-images"]);
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.rs")), "fn main() {}"),
            Mode::Syntax(s) if s.name == "Rust"
        ));
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.md")), "# hi"),
            Mode::Markdown
        ));
    }

    #[test]
    fn no_render_原样输出() {
        let hl = highlighter();
        let cli = Cli::parse_from(["mcat", "--no-render"]);
        assert!(matches!(
            resolve(&cli, &hl, Some(Path::new("a.rs")), "fn main() {}"),
            Mode::Raw
        ));
    }

    fn parse_with(user: &[&str], defaults: &Config) -> Result<Cli, clap::Error> {
        let mut argv: Vec<OsString> = vec!["mcat".into()];
        argv.extend(user.iter().map(OsString::from));
        merge_config(&argv, defaults)
    }

    #[test]
    fn 配置文件提供默认值() {
        let config = Config::from_toml(
            "theme = \"base16-mocha.dark\"\nmermaid_theme = \"dark\"\nimage_rows = 12\nnumbers = true\n",
        )
        .unwrap();
        let cli = parse_with(&[], &config).unwrap();
        assert_eq!(cli.theme, "base16-mocha.dark");
        assert_eq!(cli.mermaid_theme, "dark");
        assert_eq!(cli.image_rows, Some(12));
        assert!(cli.numbers);
    }

    #[test]
    fn 命令行覆盖配置文件里的值() {
        let config = Config::from_toml("theme = \"base16-mocha.dark\"\nimage_rows = 12").unwrap();
        let cli = parse_with(
            &["--theme", "base16-ocean.dark", "--image-rows", "8"],
            &config,
        )
        .unwrap();
        assert_eq!(cli.theme, "base16-ocean.dark");
        assert_eq!(cli.image_rows, Some(8));
    }

    #[test]
    fn 反义参数可以关掉配置文件里的开关() {
        let config =
            Config::from_toml("numbers = true\nno_color = true\nno_images = true").unwrap();
        let cli = parse_with(&["--no-numbers", "--color", "--images"], &config).unwrap();
        assert!(!cli.numbers);
        assert!(!cli.no_color);
        assert!(!cli.no_images);
    }

    #[test]
    fn 正反参数按出现顺序后者覆盖前者() {
        assert!(
            !parse_with(&["--numbers", "--no-numbers"], &Config::default())
                .unwrap()
                .numbers
        );
        assert!(
            parse_with(&["--no-numbers", "--numbers"], &Config::default())
                .unwrap()
                .numbers
        );
        assert!(
            !parse_with(&["--no-color", "--color"], &Config::default())
                .unwrap()
                .no_color
        );
        assert!(
            parse_with(&["--images", "--no-images"], &Config::default())
                .unwrap()
                .no_images
        );
    }

    #[test]
    fn 同一种参数写两遍时后面的赢() {
        let config = Config::from_toml("numbers = true\ntheme = \"base16-mocha.dark\"").unwrap();
        assert!(parse_with(&["--numbers"], &config).unwrap().numbers);
        let cli = parse_with(&["--theme", "base16-ocean.dark"], &config).unwrap();
        assert_eq!(cli.theme, "base16-ocean.dark");
    }

    #[test]
    fn 带空格的主题名能原样传进来() {
        let config = Config::from_toml("theme = \"Solarized (dark)\"").unwrap();
        let cli = parse_with(&[], &config).unwrap();
        assert_eq!(cli.theme, "Solarized (dark)");
    }

    #[test]
    fn 正向参数可以打开配置文件里写死的_false() {
        let config =
            Config::from_toml("numbers = false\nno_color = false\nno_images = false").unwrap();
        let cli = parse_with(&["--numbers", "--no-color", "--no-images"], &config).unwrap();
        assert!(cli.numbers);
        assert!(cli.no_color);
        assert!(cli.no_images);
    }

    #[test]
    fn 配置文件和命令行同时没提时用内置默认值() {
        let cli = parse_with(&[], &Config::default()).unwrap();
        assert_eq!(cli.theme, "auto");
        assert_eq!(cli.mermaid_theme, "auto");
        assert_eq!(cli.image_protocol, "auto");
        assert!(!cli.numbers);
        assert!(!cli.no_color);
        assert!(!cli.no_images);
        assert_eq!(cli.image_rows, None);
    }

    #[test]
    fn 合并后_config_参数还在() {
        let config = Config::from_toml("theme = \"base16-mocha.dark\"").unwrap();
        let cli = parse_with(&["--config", "/tmp/mine.toml"], &config).unwrap();
        assert_eq!(cli.config.as_deref(), Some(Path::new("/tmp/mine.toml")));
    }
}
