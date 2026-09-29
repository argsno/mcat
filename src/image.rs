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

        if var("GHOSTTY_RESOURCES_DIR").is_some() || program == "ghostty" {
            return Some(Protocol::Kitty);
        }
        if var("KITTY_WINDOW_ID").is_some() || var("TERM") == Some("xterm-kitty".into()) {
            return Some(Protocol::Kitty);
        }
        if var("WEZTERM_EXECUTABLE").is_some() || program == "WezTerm" {
            return Some(Protocol::Kitty);
        }
        if var("TERM") == Some("foot".into()) {
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

/// 整个进程共用一份：HTTP 连接池、协议、尺寸上限。
pub struct Images {
    agent: ureq::Agent,
    protocol: Protocol,
    sizing: Sizing,
}

impl Images {
    pub fn new(protocol: Protocol, sizing: Sizing) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .build();
        Images {
            agent: ureq::Agent::new_with_config(config),
            protocol,
            sizing,
        }
    }

    /// 为一个文件创建渲染器。相对路径按 `base` 解析，
    /// 两份文件里的同名相对路径不会互相串味。
    pub fn for_file(&self, base: PathBuf) -> Renderer<'_> {
        Renderer {
            shared: self,
            base,
            seen: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }
}

pub struct Renderer<'a> {
    shared: &'a Images,
    /// 相对路径的基准目录（Markdown 文件所在目录）。
    base: PathBuf,
    /// 同一次运行里，同一张图只处理一次。
    seen: std::cell::RefCell<std::collections::HashMap<String, Option<String>>>,
}

impl Renderer<'_> {
    /// 渲染一张图片。拿不到数据或者格式不认识时返回 `None`，
    /// 调用方据此退回文字占位。
    pub fn render(&self, url: &str) -> Option<String> {
        if let Some(cached) = self.seen.borrow().get(url) {
            return cached.clone();
        }
        let rendered = self.load(url).and_then(|data| self.encode(url, &data));
        self.seen
            .borrow_mut()
            .insert(url.to_string(), rendered.clone());
        rendered
    }

    /// 拿到图片字节：远程走下载加缓存，本地直接读。
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        if is_remote(url) {
            self.fetch(url)
        } else {
            read_local(&self.resolve(url))
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

    fn fetch(&self, url: &str) -> Option<Vec<u8>> {
        let cache = cache_path(url);
        if let Some(bytes) = read_fresh(&cache) {
            return Some(bytes);
        }
        let fetched = self.download(url);
        if let Some(ref bytes) = fetched {
            if let Some(parent) = cache.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&cache, bytes);
        }
        // 下载失败时宁可给过期的图，也不要什么都没有
        fetched.or_else(|| read_any(&cache))
    }

    fn download(&self, url: &str) -> Option<Vec<u8>> {
        let response = self.shared.agent.get(url).call().ok()?;
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

    /// 识别格式、必要时转码，然后算出行列数，交给对应协议编码。
    fn encode(&self, url: &str, data: &[u8]) -> Option<String> {
        let format = image::guess_format(data).ok()?;
        let protocol = self.shared.protocol;
        // Kitty 只认 PNG；iTerm2 自己能解码 JPEG/GIF，原样传即可
        let (payload, width, height) = match (protocol, format) {
            (Protocol::Kitty, image::ImageFormat::Png) => {
                let (w, h) = dimensions(data)?;
                (data.to_vec(), w, h)
            }
            (Protocol::Kitty, _) => {
                let (w, h) = dimensions(data)?;
                to_png(data, pixel_budget(&self.shared.sizing, w, h))?
            }
            (Protocol::Iterm2, _) => {
                let (w, h) = dimensions(data)?;
                (data.to_vec(), w, h)
            }
        };

        let (cols, rows) = self.shared.sizing.fit(width, height);
        if cols == 0 || rows == 0 {
            return None;
        }
        let encoded = BASE64.encode(&payload);
        match protocol {
            Protocol::Kitty => Some(kitty_sequence(url, &encoded, cols, rows)),
            Protocol::Iterm2 => Some(iterm2_sequence(&encoded, cols, rows, payload.len())),
        }
    }
}

impl Sizing {
    /// 按宽高比缩放到边界之内，返回占用的字符格数。
    ///
    /// 字符格不是正方形（大约 1:2），所以高度要按像素比例的一半算，
    /// 否则图片会被压扁。
    pub fn fit(&self, width: u32, height: u32) -> (usize, usize) {
        if width == 0 || height == 0 {
            return (0, 0);
        }
        let aspect = f64::from(height) / f64::from(width);
        let mut cols = self.max_cols;
        let mut rows = ((cols as f64 * aspect) / CELL_RATIO).round() as usize;
        if rows > self.max_rows {
            rows = self.max_rows;
            cols = ((rows as f64 * CELL_RATIO) / aspect).round() as usize;
        }
        (cols.max(1), rows.max(1))
    }
}

/// 一个字符格按 Retina 算大约 16×32 物理像素，编码到这个分辨率就够了。
fn pixel_budget(sizing: &Sizing, width: u32, height: u32) -> (u32, u32) {
    let (cols, rows) = sizing.fit(width, height);
    ((cols * 16) as u32, (rows * 32) as u32)
}

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Kitty 图形协议：a=T 表示直接传输并显示，f=100 表示 PNG 格式，
/// q=2 表示不要回执（mcat 不读 stdout 之外的输入，没法处理回执）。
/// 载荷超过 4096 字节必须拆成多条，只有最后一条带 m=1。
fn kitty_sequence(url: &str, payload: &str, cols: usize, rows: usize) -> String {
    let id = image_id(url);
    let mut out = String::new();
    let mut chunks = payload.as_bytes().chunks(KITTY_CHUNK);
    let total = payload.len().div_ceil(KITTY_CHUNK);
    for (i, chunk) in chunks.by_ref().enumerate() {
        let last = i + 1 == total;
        // 控制数据里不能出现分号，分隔 payload 用的那个分号要放在最后
        out.push_str(&format!(
            "\x1b_Ga=T,f=100,i={id},q=2,c={cols},r={rows},m={};",
            if last { 1 } else { 0 }
        ));
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
fn image_id(url: &str) -> u32 {
    let digest = Sha256::digest(url.as_bytes());
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
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

    #[test]
    fn 缩放后不超过边界() {
        let s = Sizing {
            max_cols: 40,
            max_rows: 20,
        };
        // 宽图：受宽度限制
        let (cols, rows) = s.fit(800, 200);
        assert!(cols <= 40 && rows <= 20);
        assert_eq!(cols, 40);
        // 高图：受高度限制
        let (cols, rows) = s.fit(200, 1600);
        assert!(cols <= 40 && rows <= 20);
        assert_eq!(rows, 20);
    }

    #[test]
    fn 缩放保持宽高比() {
        let s = Sizing {
            max_cols: 100,
            max_rows: 100,
        };
        let (cols, rows) = s.fit(400, 400);
        // 方形图片：字符格是 1:2，所以行数是列数的两倍
        assert_eq!(rows, cols * 2);
        assert!(cols <= 100 && rows <= 100);
    }

    #[test]
    fn 零尺寸不输出() {
        let s = Sizing {
            max_cols: 40,
            max_rows: 20,
        };
        assert_eq!(s.fit(0, 100), (0, 0));
    }

    #[test]
    fn kitty_单块载荷不分段() {
        let payload = BASE64.encode([0u8; 100]);
        let seq = kitty_sequence("x", &payload, 10, 5);
        assert_eq!(seq.matches("\x1b_G").count(), 1);
        assert!(seq.starts_with("\x1b_Ga=T,f=100,"));
        assert!(seq.ends_with("\x1b\\"));
        assert!(seq.contains("m=1;"));
        assert!(seq.contains("c=10,r=5"));
    }

    #[test]
    fn kitty_大载荷按四千字节分块() {
        // 10000 字节编码成 base64 约 13336 字符，4096 一块是 4 块
        let payload = BASE64.encode([0u8; 10000]);
        let seq = kitty_sequence("x", &payload, 10, 5);
        assert_eq!(seq.matches("\x1b_G").count(), 4);
        assert_eq!(seq.matches("m=0;").count(), 3);
        assert_eq!(seq.matches("m=1;").count(), 1);
        // 每块的载荷都不超过上限
        for chunk in seq.split("\x1b\\").skip(1) {
            let payload = chunk.split_once(';').map(|(_, p)| p).unwrap_or_default();
            assert!(payload.len() <= KITTY_CHUNK, "块太长：{}", payload.len());
        }
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
