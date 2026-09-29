//! 图片渲染：把 Markdown 里的图片变成终端图形协议的控制序列。
//!
//! 支持两种协议：
//!
//! - **Kitty 图形协议**（`\x1b_G…\x1b\\`）：Ghostty、kitty、WezTerm、foot、Contour。
//!   只认 PNG，其他格式要先转码。载荷要按 4096 字节分块。
//! - **iTerm2 内联图片**（`\x1b]1337;File=…\x07`）：iTerm2、WezTerm、VS Code。
//!   原始字节直接交给终端解码，不用转码，载荷不分块。
//!
//! 协议从环境变量猜；猜不出来时图片退回文字占位。

use base64::Engine;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageReader};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Kitty 协议单条转义序列的载荷上限。
const KITTY_CHUNK: usize = 4096;
/// 远程图片的大小上限，超了直接放弃，免得写爆缓存或者生成几 MB 的转义序列。
const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
/// 缓存有效期。过期了重新下载，下载失败时还能用旧的。
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 下载超时。
const TIMEOUT: Duration = Duration::from_secs(30);
/// 终端里一个字符格大约的高宽比，用来把像素尺寸换算成行列数。
const CELL_RATIO: f64 = 0.5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Kitty,
    Iterm2,
}

impl Protocol {
    /// 从环境变量猜协议。Ghostty 只实现了 Kitty 协议，Apple Terminal 一个都不支持。
    pub fn detect() -> Option<Protocol> {
        let var = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        let program = var("TERM_PROGRAM").unwrap_or_default();

        let term = var("TERM").unwrap_or_default();
        // TERM 也认一下：环境变量被脚本洗掉时只剩它
        if var("GHOSTTY_RESOURCES_DIR").is_some()
            || program == "ghostty"
            || term.starts_with("xterm-ghostty")
        {
            return Some(Protocol::Kitty);
        }
        if var("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" {
            return Some(Protocol::Kitty);
        }
        if var("WEZTERM_EXECUTABLE").is_some() || program == "WezTerm" {
            return Some(Protocol::Kitty);
        }
        if term == "foot" {
            return Some(Protocol::Kitty);
        }
        if program == "iTerm.app" || program == "WezTerm" {
            return Some(Protocol::Iterm2);
        }
        if program == "vscode" {
            return Some(Protocol::Iterm2);
        }
        None
    }
}

/// 图片在终端里占多大。
#[derive(Clone, Copy, Debug)]
pub struct Sizing {
    pub max_cols: usize,
    pub max_rows: usize,
}

/// 发给终端的尺寸约束。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Columns,
    Rows,
}

/// 一张图的显示尺寸。
///
/// Kitty 协议只发**一个**维度，另一个由终端按图片宽高比自己算——终端最清楚
/// 自己字符格的实际像素尺寸。同时发 `c` 和 `r` 在 Ghostty 上不可靠：实测
/// 宽高比不匹配时图直接不显示，匹配时又被拉伸变形。
#[derive(Clone, Copy, Debug)]
pub struct Fit {
    pub axis: Axis,
    pub value: usize,
    /// 两个维度的估算值，只用于降采样预算和报告
    pub cols: usize,
    pub rows: usize,
}

/// 整个进程共用一份：HTTP 连接池、协议、尺寸上限。
pub struct Images {
    agent: ureq::Agent,
    protocol: Protocol,
    /// Markdown 里嵌的图片：留出空间给正文。
    embedded: Sizing,
    /// 单独看一张图：铺满终端。
    full: Sizing,
}

impl Images {
    pub fn new(protocol: Protocol, embedded: Sizing, full: Sizing) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build();
        Images {
            agent: ureq::Agent::new_with_config(config),
            protocol,
            embedded,
            full,
        }
    }

    pub fn sizing(&self) -> Sizing {
        self.embedded
    }

    /// 单独看一张图时的尺寸。
    pub fn full_sizing(&self) -> Sizing {
        self.full
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Markdown 里嵌的图片。
    pub fn for_file(&self, base: PathBuf) -> Renderer<'_> {
        self.renderer(base, self.embedded)
    }

    /// 单独看一张图片，按铺满终端算。
    pub fn for_image(&self) -> Renderer<'_> {
        self.renderer(PathBuf::from("."), self.full)
    }

    fn renderer(&self, base: PathBuf, sizing: Sizing) -> Renderer<'_> {
        Renderer {
            shared: self,
            sizing,
            base,
            seen: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// 图片内容是不是我们支持的格式。
    pub fn is_image(bytes: &[u8]) -> bool {
        image::guess_format(bytes).is_ok_and(|f| {
            matches!(
                f,
                image::ImageFormat::Png
                    | image::ImageFormat::Jpeg
                    | image::ImageFormat::Gif
                    | image::ImageFormat::WebP
                    | image::ImageFormat::Bmp
            )
        })
    }
}

pub struct Renderer<'a> {
    shared: &'a Images,
    sizing: Sizing,
    /// 相对路径的基准目录（Markdown 文件所在目录）。
    base: PathBuf,
    /// 同一次运行里，同一张图只处理一次。
    seen: std::cell::RefCell<std::collections::HashMap<String, Option<String>>>,
}

/// 一张图准备就绪之后的全部信息，`--check-images` 靠它报告。
#[derive(Debug)]
pub struct Prepared {
    pub sequence: String,
    /// 人类可读的格式名。
    pub format: &'static str,
    /// 相对路径解析到哪儿了（远程图片为 `None`）。
    pub local_path: Option<PathBuf>,
    /// 编码前的像素尺寸。
    pub source_pixel: (u32, u32),
    /// 最终编码的像素尺寸，降采样后会变小。
    pub encoded_pixel: (u32, u32),
    /// 在终端里占多少字符格（估算值，Kitty 协议下另一个维度由终端算）。
    pub cells: (usize, usize),
    /// 实际发给终端的是哪个维度。
    pub axis: Axis,
    /// 原始字节数与 base64 之后的字符数。
    pub payload: usize,
    pub encoded: usize,
    /// 拆成几块（iTerm2 协议只有一段）。
    pub chunks: usize,
    /// 因为协议只支持 PNG 而转码了。
    pub transcoded: bool,
    /// 因为超过显示尺寸而降采样了。
    pub downscaled: bool,
    /// 远程图片是不是走了缓存。
    pub from_cache: bool,
}

impl Renderer<'_> {
    /// 渲染一张图片。拿不到数据或者格式不认识时返回 `None`，
    /// 调用方据此退回文字占位。
    pub fn render(&self, url: &str) -> Option<String> {
        // 缓存按解析后的实际文件做键：`./a.png` 和 `a.png` 是同一张图，
        // 不该处理两遍
        let key = self.identity(url);
        if let Some(cached) = self.seen.borrow().get(&key) {
            return cached.clone();
        }
        let rendered = self.prepare(url).ok().map(|p| p.sequence);
        self.seen.borrow_mut().insert(key, rendered.clone());
        rendered
    }

    /// 图片的稳定标识：本地用解析后的实际路径，远程用 URL。
    /// 终端按这个 id 去重，所以同一张图必须算出同一个 id。
    fn identity(&self, url: &str) -> String {
        if is_remote(url) {
            return url.to_string();
        }
        normalize(&self.resolve(url)).to_string_lossy().into_owned()
    }

    /// 图片渲染的每一步。`Err` 里是给用户看的原因。
    pub fn prepare(&self, url: &str) -> Result<Prepared, String> {
        let (data, local_path, from_cache) = self.load(url)?;
        let (payload, format, transcoded, source_pixel, encoded_pixel, downscaled) =
            self.convert(&data)?;
        let fit = self.sizing.fit(encoded_pixel.0, encoded_pixel.1);
        if fit.value == 0 {
            return Err("尺寸算出来是 0".to_string());
        }
        let encoded = BASE64.encode(&payload);
        let sequence = match self.shared.protocol {
            Protocol::Kitty => kitty_sequence(&self.identity(url), &encoded, fit),
            // iTerm2 那边靠 preserveAspectRatio，同时给两个维度
            Protocol::Iterm2 => iterm2_sequence(&encoded, fit.cols, fit.rows, payload.len()),
        };
        Ok(Prepared {
            chunks: sequence.matches("\x1b_G").count().max(1),
            sequence,
            format,
            local_path,
            source_pixel,
            encoded_pixel,
            cells: (fit.cols, fit.rows),
            axis: fit.axis,
            payload: data.len(),
            encoded: encoded.len(),
            transcoded,
            downscaled,
            from_cache,
        })
    }

    /// 拿到图片字节：远程走下载加缓存，本地直接读。
    fn load(&self, url: &str) -> Result<(Vec<u8>, Option<PathBuf>, bool), String> {
        if is_remote(url) {
            self.fetch(url)
        } else {
            let path = normalize(&self.resolve(url));
            match read_local(&path) {
                Some(bytes) => Ok((bytes, Some(path), false)),
                None => Err(format!("读不到 {}", path.display())),
            }
        }
    }

    /// 相对路径先按 Markdown 文件所在目录找，找不到再按当前目录找。
    fn resolve(&self, url: &str) -> PathBuf {
        let path = Path::new(url);
        if path.is_absolute() {
            return path.to_path_buf();
        }
        let joined = self.base.join(path);
        if joined.exists() {
            return joined;
        }
        path.to_path_buf()
    }

    fn fetch(&self, url: &str) -> Result<(Vec<u8>, Option<PathBuf>, bool), String> {
        let cache = cache_path(url);
        if let Some(bytes) = read_fresh(&cache) {
            return Ok((bytes, None, true));
        }
        let fetched = self.download(url);
        if let Some(ref bytes) = fetched {
            if let Some(parent) = cache.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&cache, bytes);
        }
        // 下载失败时宁可给过期的图，也不要什么都没有
        match fetched {
            Some(bytes) => Ok((bytes, None, false)),
            // 过期缓存也算命中，只是不能算「新鲜」
            None => read_any(&cache)
                .map(|bytes| (bytes, None, true))
                .ok_or_else(|| format!("下载失败，缓存里也没有：{url}")),
        }
    }

    fn download(&self, url: &str) -> Option<Vec<u8>> {
        let response = self
            .shared
            .agent
            .get(url)
            .call()
            .map_err(|e| e.to_string())
            .inspect_err(|e| log_download_failure(url, e))
            .ok()?;
        if response.status() != 200 {
            return None;
        }
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOWNLOAD)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(bytes)
    }

    /// 转码 + 降采样。iTerm2 协议直接用原始字节，Kitty 必须转成 PNG。
    #[allow(clippy::type_complexity)]
    fn convert(
        &self,
        data: &[u8],
    ) -> Result<(Vec<u8>, &'static str, bool, (u32, u32), (u32, u32), bool), String> {
        let format = image::guess_format(data).map_err(|_| "不是能识别的图片格式".to_string())?;
        let name = format_name(format);
        let source = dimensions(data).ok_or_else(|| "读不出图片尺寸".to_string())?;

        // iTerm2 自己能解码原始字节，Kitty 只认 PNG
        if self.shared.protocol == Protocol::Iterm2 {
            return Ok((data.to_vec(), name, false, source, source, false));
        }
        if format == image::ImageFormat::Png {
            return Ok((data.to_vec(), name, false, source, source, false));
        }
        let budget = pixel_budget(&self.sizing, source.0, source.1);
        let (payload, w, h) = to_png(data, budget).ok_or_else(|| "转码成 PNG 失败".to_string())?;
        let downscaled = (w, h) != source;
        Ok((payload, name, true, source, (w, h), downscaled))
    }
}

impl Sizing {
    /// 算出该发给终端的尺寸约束。
    ///
    /// 字符格不是正方形（实测约 1:2），所以高宽比要换算成行列比。
    /// 这个估算只用来**决定发哪个维度**；另一个维度交给终端按真实字符格算，
    /// 所以估得不准也不会让图片变形。
    pub fn fit(&self, width: u32, height: u32) -> Fit {
        if width == 0 || height == 0 {
            return Fit {
                axis: Axis::Columns,
                value: 0,
                cols: 0,
                rows: 0,
            };
        }
        let aspect = f64::from(height) / f64::from(width);
        let mut cols = self.max_cols;
        let mut rows = ((cols as f64 * aspect) / CELL_RATIO).round() as usize;
        if rows > self.max_rows {
            // 太高了，改为按高度约束，列数由终端算
            rows = self.max_rows;
            cols = ((rows as f64 * CELL_RATIO) / aspect).round() as usize;
            return Fit {
                axis: Axis::Rows,
                value: rows.max(1),
                cols: cols.max(1),
                rows: rows.max(1),
            };
        }
        Fit {
            axis: Axis::Columns,
            value: cols,
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

/// 一个字符格按 Retina 算大约 16×32 物理像素，编码到这个分辨率就够了。
/// 这里用的是估算值，估偏一点只影响文件大小，不影响显示比例。
fn pixel_budget(sizing: &Sizing, width: u32, height: u32) -> (u32, u32) {
    let fit = sizing.fit(width, height);
    ((fit.cols * 16) as u32, (fit.rows * 32) as u32)
}

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Kitty 图形协议。照规范
/// <https://sw.kovidgoyal.net/kitty/graphics-protocol/> 实现：
///
/// - `a=T` 直接传输并显示，`f=100` 表示 PNG
/// - `q=2` 抑制回执（mcat 不读 stdout 之外的输入，没法处理回执）
/// - 载荷按 4096 字节分块；除最后一块外都带 `m=1`（后面还有数据），
///   最后一块带 `m=0`
/// - 完整控制数据只发第一块，后续块只带 `m`（规范明确要求）
/// - **只发 `c` 或 `r` 其中一个**，另一个由终端按图片宽高比算
///
/// 三处最容易搞错、且错了就完全不显示的地方：
/// 终端在收齐并校验完整个序列之前什么都不画，所以单块图片也必须发 `m=0`；
/// 每块重复 `a=T` 会被当成一次次的全新传输；
/// 同时发 `c` 和 `r` 时规范说会 letterbox，实测 Ghostty 并不如此——
/// 宽高比不匹配时图直接消失，匹配时也会被拉伸变形。只发一个维度最稳，
/// 因为终端最清楚自己字符格的实际像素尺寸。
fn kitty_sequence(url: &str, payload: &str, fit: Fit) -> String {
    let id = image_id(url);
    let size = match fit.axis {
        Axis::Columns => format!("c={}", fit.value),
        Axis::Rows => format!("r={}", fit.value),
    };
    let mut out = String::new();
    let mut chunks = payload.as_bytes().chunks(KITTY_CHUNK);
    let total = payload.len().div_ceil(KITTY_CHUNK);
    for (i, chunk) in chunks.by_ref().enumerate() {
        let last = i + 1 == total;
        // m=1 表示后面还有数据，m=0 表示这是最后一块
        if i == 0 {
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,i={id},q=2,{size},m={};",
                u8::from(!last)
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={};", u8::from(!last)));
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\x1b\\");
    }
    out
}

/// iTerm2 内联图片。width/height 的单位是字符格，
/// preserveAspectRatio=1 让终端自己等比缩放。
fn iterm2_sequence(payload: &str, cols: usize, rows: usize, size: usize) -> String {
    format!(
        "\x1b]1337;File=inline=1;width={cols};height={rows};preserveAspectRatio=1;size={size}:{payload}\x07"
    )
}

/// 同一个 URL 用同一个 id，避免图片被终端去重。
/// 规范要求 id 不能是 0，所以把哈希落在 0 上的情况挪到 1。
fn image_id(url: &str) -> u32 {
    let digest = Sha256::digest(url.as_bytes());
    let id = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    if id == 0 { 1 } else { id }
}

/// 只读图片头拿尺寸，不解码整张图。
fn dimensions(data: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 转成 PNG，必要时按显示尺寸降采样。
///
/// Kitty 协议只支持 PNG，别的格式必须过这一关。降采样不是可有可无的：
/// 一张 4000×3000 的照片直接编码成 PNG 再 base64，能生成几十 MB 的转义序列，
/// 足以卡死终端。缩到显示尺寸的两倍分辨率就够了。
fn to_png(data: &[u8], max_px: (u32, u32)) -> Option<(Vec<u8>, u32, u32)> {
    let decoded =
        image::load_from_memory_with_format(data, image::guess_format(data).ok()?).ok()?;
    let (width, height) = (decoded.width(), decoded.height());
    let (max_w, max_h) = max_px;

    // 只缩不放
    let ratio = if width > 0 && height > 0 {
        (f64::from(max_w) / f64::from(width)).min(f64::from(max_h) / f64::from(height))
    } else {
        1.0
    };
    if ratio >= 1.0 {
        return encode_png(&decoded.to_rgba8(), width, height);
    }
    let width = ((f64::from(width) * ratio).round() as u32).max(1);
    let height = ((f64::from(height) * ratio).round() as u32).max(1);
    let decoded = decoded.resize_exact(width, height, image::imageops::FilterType::Triangle);
    encode_png(&decoded.to_rgba8(), width, height)
}

fn encode_png(rgba: &image::RgbaImage, width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
        .ok()?;
    Some((out, width, height))
}

fn format_name(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "PNG",
        image::ImageFormat::Jpeg => "JPEG",
        image::ImageFormat::Gif => "GIF",
        image::ImageFormat::WebP => "WebP",
        image::ImageFormat::Bmp => "BMP",
        _ => "其他",
    }
}

/// 下载失败不算致命，但值得说一句，否则用户会以为图片本来就没有。
fn log_download_failure(url: &str, error: &str) {
    eprintln!("mcat: 远程图片下载失败 {url}：{error}");
}

/// 路径归一化：`examples/./a.png` 变成 `examples/a.png`，能解析就变绝对路径。
/// 相对路径拼出来会带 `./` 和 `..`，报告里显示出来很难看；
/// 更要紧的是同一张图用两种写法引用会算出两个不同的终端图片 id。
fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_remote(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn read_local(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn cache_path(url: &str) -> PathBuf {
    let digest = Sha256::digest(url.as_bytes());
    let name: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    cache_dir().join(name)
}

fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("mcat");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("mcat");
    }
    std::env::temp_dir().join("mcat-images")
}

/// 在有效期内的缓存。
fn read_fresh(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > CACHE_TTL {
        return None;
    }
    read_any(path)
}

fn read_any(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizing(cols: usize, rows: usize) -> Sizing {
        Sizing {
            max_cols: cols,
            max_rows: rows,
        }
    }

    #[test]
    fn 宽图按列数约束() {
        let fit = sizing(40, 20).fit(800, 200);
        assert_eq!(fit.axis, Axis::Columns);
        assert_eq!(fit.value, 40);
        assert!(fit.cols <= 40 && fit.rows <= 20);
    }

    #[test]
    fn 高图改为按行数约束() {
        let fit = sizing(40, 20).fit(200, 1600);
        assert_eq!(fit.axis, Axis::Rows);
        assert_eq!(fit.value, 20);
        assert!(fit.cols <= 40 && fit.rows <= 20);
    }

    #[test]
    fn 缩放保持宽高比() {
        let fit = sizing(100, 100).fit(400, 400);
        // 方形图片：字符格是 1:2，所以行数是列数的两倍
        assert_eq!(fit.rows, fit.cols * 2);
        assert!(fit.cols <= 100 && fit.rows <= 100);
    }

    #[test]
    fn 零尺寸不输出() {
        let fit = sizing(40, 20).fit(0, 100);
        assert_eq!(fit.value, 0);
    }

    fn fit(cols: usize, rows: usize) -> Fit {
        Fit {
            axis: Axis::Columns,
            value: cols,
            cols,
            rows,
        }
    }

    #[test]
    fn kitty_单块载荷不分段() {
        let payload = BASE64.encode([0u8; 100]);
        let seq = kitty_sequence("x", &payload, fit(10, 5));
        assert_eq!(seq.matches("\x1b_G").count(), 1);
        assert!(seq.starts_with("\x1b_Ga=T,f=100,"));
        assert!(seq.ends_with("\x1b\\"));
    }

    /// 只发一个维度。实测 Ghostty 同时收到 c 和 r 时行为不可靠：
    /// 宽高比不匹配就什么都不显示，匹配也会被拉伸变形。
    #[test]
    fn kitty_只发一个维度() {
        let payload = BASE64.encode([0u8; 100]);
        let seq = kitty_sequence("x", &payload, fit(20, 7));
        assert!(seq.contains("c=20,"), "{seq:?}");
        assert!(!seq.contains("r="), "{seq:?}");

        let by_rows = Fit {
            axis: Axis::Rows,
            value: 13,
            cols: 39,
            rows: 13,
        };
        let seq = kitty_sequence("x", &payload, by_rows);
        assert!(seq.contains("r=13,"), "{seq:?}");
        assert!(!seq.contains("c="), "{seq:?}");
    }

    /// m=0 表示「这是最后一块」。单块图片也必须这么发：
    /// 终端在收齐之前不显示，发 m=1 就等于让它一直等。
    #[test]
    fn kitty_单块必须标成最后一块() {
        let payload = BASE64.encode([0u8; 100]);
        let seq = kitty_sequence("x", &payload, fit(10, 5));
        assert!(seq.contains("m=0;"), "{seq:?}");
        assert!(!seq.contains("m=1;"), "{seq:?}");
    }

    #[test]
    fn kitty_大载荷按四千字节分块() {
        // 10000 字节编码成 base64 约 13336 字符，4096 一块是 4 块
        let payload = BASE64.encode([0u8; 10000]);
        let seq = kitty_sequence("x", &payload, fit(10, 5));
        assert_eq!(seq.matches("\x1b_G").count(), 4);
        // 除最后一块外都是 m=1
        assert_eq!(seq.matches("m=1;").count(), 3);
        assert_eq!(seq.matches("m=0;").count(), 1);
        // m=0 只能出现在最后一条上
        assert!(seq.rfind("m=0;").unwrap() > seq.rfind("m=1;").unwrap());
    }

    /// 规范：完整控制数据只发第一块，后续块只能带 m。
    /// 每块都重复 a=T 会被当成一次次的全新传输。
    #[test]
    fn kitty_只有第一块带完整控制数据() {
        let payload = BASE64.encode([0u8; 10000]);
        let seq = kitty_sequence("x", &payload, fit(10, 5));
        assert_eq!(seq.matches("a=T").count(), 1, "a=T 只能出现一次");
        assert_eq!(seq.matches("f=100").count(), 1, "f=100 只能出现一次");
        assert_eq!(seq.matches("i=").count(), 1, "i= 只能出现一次");
        assert_eq!(seq.matches("c=").count(), 1, "c= 只能出现一次");
        // 后续块只有 m
        let followups: Vec<&str> = seq.split("\x1b_G").skip(2).collect();
        assert!(!followups.is_empty());
        for chunk in &followups {
            let control = chunk.split(';').next().unwrap();
            assert!(
                control == "m=1" || control == "m=0",
                "后续块只该有 m：{control}"
            );
        }
        // 每块的载荷都不超过上限
        for chunk in seq.split("\x1b\\").skip(1) {
            let payload = chunk.split_once(';').map(|(_, p)| p).unwrap_or_default();
            assert!(payload.len() <= KITTY_CHUNK, "块太长：{}", payload.len());
        }
    }

    /// 规范明确要求 id 不能是 0。
    #[test]
    fn 图片_id_不会是零() {
        assert_ne!(image_id(""), 0);
        assert_ne!(image_id("x.png"), 0);
    }

    #[test]
    fn 同一张图的_id_相同() {
        assert_eq!(image_id("a.png"), image_id("a.png"));
        assert_ne!(image_id("a.png"), image_id("b.png"));
    }

    #[test]
    fn iterm2_序列格式() {
        let payload = BASE64.encode(b"abc");
        let seq = iterm2_sequence(&payload, 20, 10, 3);
        assert!(seq.starts_with("\x1b]1337;File=inline=1;"));
        assert!(seq.contains("width=20;height=10;"));
        assert!(seq.ends_with(&format!("size=3:{payload}\x07")));
    }

    #[test]
    fn 远程地址识别() {
        assert!(is_remote("https://example.com/a.png"));
        assert!(is_remote("http://example.com/a.png"));
        assert!(!is_remote("docs/a.png"));
        assert!(!is_remote("/abs/a.png"));
    }

    #[test]
    fn 能读出_png_尺寸并能转码() {
        // 2x1 的 PNG
        let png = image::RgbaImage::from_pixel(2, 1, image::Rgba([0, 0, 0, 255]));
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(png.as_raw(), 2, 1, ExtendedColorType::Rgba8)
            .unwrap();
        assert_eq!(dimensions(&bytes), Some((2, 1)));
        let (out, w, h) = to_png(&bytes, (100, 100)).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::Png);
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;

    fn encode(img: &image::RgbaImage) -> Vec<u8> {
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        png
    }

    /// 超出显示尺寸的图必须降采样，否则 base64 转义序列会大到卡死终端。
    #[test]
    fn 大图按显示尺寸降采样() {
        // 渐变图，PNG 压不动，才看得出体积变化
        let mut big = image::RgbaImage::new(600, 400);
        for (x, y, pixel) in big.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
        }
        let png = encode(&big);
        let original = png.len();

        let sizing = Sizing {
            max_cols: 20,
            max_rows: 20,
        };
        let budget = pixel_budget(&sizing, 600, 400);
        let (out, w, h) = to_png(&png, budget).unwrap();
        assert!(
            w <= budget.0 && h <= budget.1,
            "{w}x{h} 超出预算 {budget:?}"
        );
        assert!(out.len() < original / 4, "{} vs {original}", out.len());
    }

    /// 降采样要保持宽高比。
    #[test]
    fn 降采样保持宽高比() {
        let mut wide = image::RgbaImage::new(400, 100);
        for (x, y, pixel) in wide.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, 0, 255]);
        }
        let (_, w, h) = to_png(&encode(&wide), (200, 200)).unwrap();
        assert_eq!((w, h), (200, 50));
    }

    /// 小图不该被放大。
    #[test]
    fn 小图保持原尺寸() {
        let small = image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]));
        let (_, w, h) = to_png(&encode(&small), (640, 640)).unwrap();
        assert_eq!((w, h), (8, 8));
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn images() -> Images {
        Images::new(
            Protocol::Kitty,
            Sizing {
                max_cols: 80,
                max_rows: 20,
            },
            Sizing {
                max_cols: 80,
                max_rows: 24,
            },
        )
    }

    /// 找不到的文件要报出人能看懂的路径，而不是崩掉。
    #[test]
    fn 报告能说明失败原因() {
        let dir = std::env::temp_dir().join("mcat-test-missing");
        let err = images()
            .for_file(dir)
            .prepare("nope.png")
            .expect_err("文件不存在应该失败");
        assert!(err.contains("读不到"), "{err}");
        assert!(err.contains("nope.png"), "{err}");
    }

    /// 报告里的信息要能反映真实的编码结果。
    #[test]
    fn 报告内容与实际编码一致() {
        let dir = std::env::temp_dir();
        let png = dir.join("mcat-test-report.png");
        let img = image::RgbaImage::from_pixel(48, 24, image::Rgba([1, 2, 3, 255]));
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), 48, 24, ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&png, buf).unwrap();

        let info = images()
            .for_file(dir.clone())
            .prepare("mcat-test-report.png")
            .expect("刚写的文件应该能读");
        let _ = std::fs::remove_file(&png);

        assert_eq!(info.format, "PNG");
        assert_eq!(info.source_pixel, (48, 24));
        assert!(!info.transcoded, "PNG 不该转码");
        assert_eq!(info.chunks, 1);
        assert_eq!(info.axis, Axis::Rows);
        assert_eq!(info.cells, (20, 20));
        assert!(info.sequence.starts_with("\x1b_G"));
        assert_eq!(info.sequence.matches("\x1b_G").count(), info.chunks);
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    /// 1x1 的 PNG。
    fn tiny_png() -> Vec<u8> {
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf)
            .write_image(&[0, 0, 0, 255], 1, 1, ExtendedColorType::Rgba8)
            .unwrap();
        buf
    }

    fn sequence_id(sequence: &str) -> &str {
        sequence
            .split("i=")
            .nth(1)
            .and_then(|t| t.split(',').next())
            .unwrap()
    }

    /// 同一张图的不同写法必须算出同一个终端图片 id，否则终端会当成两张图。
    #[test]
    fn 同一张图的两种写法算出同一个_id() {
        let dir = std::env::temp_dir().join("mcat-test-id");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), tiny_png()).unwrap();

        let images = Images::new(
            Protocol::Kitty,
            Sizing {
                max_cols: 40,
                max_rows: 10,
            },
            Sizing {
                max_cols: 40,
                max_rows: 10,
            },
        );
        let r = images.for_file(dir.clone());
        let plain = r.prepare("a.png").unwrap();
        let dotted = r.prepare("./a.png").unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(sequence_id(&plain.sequence), sequence_id(&dotted.sequence));
    }

    /// 两份文件里的同名图片是不同的图，id 不能撞。
    #[test]
    fn 不同目录里的同名图片_id_不同() {
        let root = std::env::temp_dir().join("mcat-test-id2");
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.png"), tiny_png()).unwrap();
        std::fs::write(b.join("x.png"), tiny_png()).unwrap();

        let images = Images::new(
            Protocol::Kitty,
            Sizing {
                max_cols: 40,
                max_rows: 10,
            },
            Sizing {
                max_cols: 40,
                max_rows: 10,
            },
        );
        let first = images.for_file(a).prepare("x.png").unwrap();
        let second = images.for_file(b).prepare("x.png").unwrap();
        let _ = std::fs::remove_dir_all(&root);

        assert_ne!(
            sequence_id(&first.sequence),
            sequence_id(&second.sequence),
            "不同目录的同名图片应该有不同的 id"
        );
    }

    /// 归一化之后显示出来的路径是干净的。
    #[test]
    fn 路径归一化去掉_点和双点() {
        let dir = std::env::temp_dir().join("mcat-test-norm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), tiny_png()).unwrap();

        // 两次归一化都得在文件还在的时候做，canonicalize 失败会退回原路径
        let from_messy = normalize(&dir.join(".").join("a.png"));
        let from_clean = normalize(&dir.join("a.png"));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            !from_messy.to_string_lossy().contains("/./"),
            "路径里不该再有 ./：{}",
            from_messy.display()
        );
        assert_eq!(from_messy, from_clean);
    }
}

#[cfg(test)]
mod detection_tests {
    use super::*;

    /// 2x2 的 PNG。
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 2, 2, ExtendedColorType::Rgba8)
            .unwrap();
        png
    }

    /// 认内容而不是扩展名，所以扩展名骗人也无所谓。
    #[test]
    fn 按内容识别图片() {
        assert!(Images::is_image(&tiny_png()));
    }

    #[test]
    fn 普通文本不算图片() {
        assert!(!Images::is_image(b"# Markdown\n\nhello\n"));
        assert!(!Images::is_image(b"fn main() {}\n"));
        assert!(!Images::is_image(b""));
        // 中文 UTF-8 也不行
        assert!(!Images::is_image("标题\n\n段落。\n".as_bytes()));
    }

    #[test]
    fn jpeg_也算图片() {
        // 最小 JPEG：SOI + APP0 头就够让 guess_format 认出来
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend_from_slice(&[0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00]);
        jpeg.extend_from_slice(&[0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
        assert!(Images::is_image(&jpeg));
    }

    /// 单独看图用铺满终端的尺寸，嵌在 Markdown 里用留出正文空间的尺寸。
    #[test]
    fn 两套尺寸各司其职() {
        let images = Images::new(
            Protocol::Kitty,
            Sizing {
                max_cols: 80,
                max_rows: 20,
            },
            Sizing {
                max_cols: 80,
                max_rows: 60,
            },
        );
        assert_eq!(images.sizing().max_rows, 20);
        assert_eq!(images.full_sizing().max_rows, 60);
    }
}
