//! mermaid 图渲染：源码 → PNG 字节。
//!
//! 走 [merman](https://crates.io/crates/merman)：纯 Rust 的 mermaid 实现，
//! 解析、布局、出 SVG，再用 resvg 光栅化成 PNG。PNG 之后交给 `image.rs`，
//! 和普通图片共用同一套编码、分块和尺寸逻辑。
//!
//! 只开 `render` + `raster` 两个 feature。merman 还有一套 ASCII 渲染器
//! （框线字符画），效果不足以进终端正文，没开。

use crate::image::{Sizing, dimensions};
use merman::render::raster::{RasterOptions, RasterSizeLimit};
use merman::render::{HeadlessRenderer, HostThemeProfile};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;

/// 图的明暗配色。跟终端背景一致才看得清，默认亮色（和 mermaid 官网一致）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    #[default]
    Light,
    Dark,
}

impl Theme {
    /// 解析 `--mermaid-theme` 的取值。
    pub fn parse(name: &str) -> Option<Theme> {
        match name.to_ascii_lowercase().as_str() {
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

/// 图的稳定标识。同样的源码必须算出同一个 id，否则终端不去重，同一张图反复画。
pub fn identity(source: &str) -> String {
    let digest = Sha256::digest(source.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("mermaid:{hex}")
}

/// 一张图准备就绪之后的全部信息，`--check-images` 靠它报告。
#[derive(Clone, Debug)]
pub struct Prepared {
    pub png: Vec<u8>,
    /// SVG 在 1:1 下的尺寸，出图前的样子。
    pub source_pixel: (u32, u32),
    /// 实际编码进转义序列的尺寸。
    pub encoded_pixel: (u32, u32),
    /// 在终端里占多少字符格（估算值）。
    pub cells: (usize, usize),
}

/// 把 mermaid 源码渲染成 PNG。
///
/// 拿不到字节时返回 `None`（识别不出图类型、语法错、系统没装字体等等），
/// 调用方据此退回显示源码——mcat 在图片链路上任何一环走不通都是这个策略。
pub struct Renderer {
    inner: HeadlessRenderer,
    /// 同一段源码只渲染一次。一份文档里同一张图出现多次很常见。
    /// 失败也记下来：同一段源码重试一百次也是同样的失败，重复解析纯属浪费。
    cache: RefCell<HashMap<String, Result<Prepared, String>>>,
}

impl Renderer {
    pub fn new(theme: Theme) -> Self {
        let profile = match theme {
            Theme::Light => HostThemeProfile::editor_light(),
            Theme::Dark => HostThemeProfile::editor_dark(),
        };
        Renderer {
            inner: HeadlessRenderer::new().with_compiled_host_theme(&profile.compile()),
            cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn png(&self, source: &str, sizing: &Sizing) -> Option<Vec<u8>> {
        self.prepare(source, sizing).ok().map(|p| p.png)
    }

    /// 渲染的每一步。`Err` 里是给用户看的原因。
    pub fn prepare(&self, source: &str, sizing: &Sizing) -> Result<Prepared, String> {
        let key = identity(source);
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let rendered = self.render(source, sizing);
        self.cache.borrow_mut().insert(key, rendered.clone());
        rendered
    }

    /// 源码是不是能画出图。`--check-images` 之外没有别的地方需要知道。
    #[cfg(test)]
    fn can_render(&self, source: &str, sizing: &Sizing) -> bool {
        self.prepare(source, sizing).is_ok()
    }

    /// 渲染的每一步。`Err` 里是给用户看的原因。
    fn render(&self, source: &str, sizing: &Sizing) -> Result<Prepared, String> {
        // resvg 不认 <foreignObject> 里的标签，要先换成 <text> 才能光栅化出文字
        let Some(svg) = self
            .inner
            .render_svg_resvg_safe_sync(source)
            .map_err(|e| e.to_string())?
        else {
            return Err("没识别出图表类型".to_string());
        };

        // SVG 原始尺寸。merman 换标签时会改 viewBox，所以这里问它要，
        // 别自己解 XML。
        let natural = merman::render::raster::svg_raster_plan(
            &svg,
            &RasterOptions::default().with_unbounded_size(),
        )
        .map_err(|e| e.to_string())?;

        // 直接按显示预算出图，别让下游再缩一遍。
        let fit = sizing.fit(natural.width_px, natural.height_px);
        let budget = sizing.pixel_box(fit.cols, fit.rows);

        // merman 的 `fit_to` 只缩不放，所以放大倍数得自己算。
        // SVG 天然比 Retina 分辨率小（mermaid 的节点和字号都是固定 CSS 像素），
        // 不放大就是一堆糊掉的字。
        let scale = upscale(natural.width_px, natural.height_px, budget);
        let options = RasterOptions::default()
            .with_scale(scale as f32)
            // 兜底：万一上游算错，不让 pixmap 无限涨
            .with_size_limit(RasterSizeLimit::new(
                Some(budget.0),
                Some(budget.1),
                Some(u64::from(budget.0) * u64::from(budget.1)),
            ));
        let png = merman::render::raster::svg_to_png(&svg, &options).map_err(|e| e.to_string())?;

        // merman 会 ceil 掉小数边长，出图可能比预算大一个像素。这一个像素会
        // 让下游 `fit` 的行数多算一格，所以按实际出图尺寸重算显示格数。
        let encoded_pixel =
            dimensions(&png).ok_or_else(|| "光栅化出来的 PNG 读不出尺寸".to_string())?;
        let cells = sizing.fit(encoded_pixel.0, encoded_pixel.1);

        Ok(Prepared {
            png,
            source_pixel: (natural.width_px, natural.height_px),
            encoded_pixel,
            cells: (cells.cols, cells.rows),
        })
    }
}

/// 放大到刚好填满显示预算，保持宽高比。
///
/// 小图放大到预算那么大（封顶两倍），大图缩到预算那么小。`contain` 语义。
fn upscale(width: u32, height: u32, budget: (u32, u32)) -> f64 {
    if width == 0 || height == 0 {
        return 1.0;
    }
    let fit = (f64::from(budget.0) / f64::from(width)).min(f64::from(budget.1) / f64::from(height));
    // 不设放大上限：放大后总归落在 budget 里，不会把带宽撑开。
    // 下限要卡住：尺寸为零时 fit 会是 inf 或 NaN。
    fit.min(f64::from(u32::MAX)).max(f64::MIN_POSITIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizing() -> Sizing {
        Sizing {
            max_cols: 80,
            max_rows: 20,
        }
    }

    #[test]
    fn 流程图能渲染成_png() {
        let png = Renderer::new(Theme::Light)
            .png("flowchart TD\n  A[开始] --> B{判断}\n", &sizing())
            .expect("流程图应该能画出来");
        assert_eq!(image::guess_format(&png).unwrap(), image::ImageFormat::Png);
        let (w, h) = dimensions(&png).expect("PNG 应该读得出尺寸");
        assert!(w > 0 && h > 0, "{w}x{h}");
    }

    /// merman 的光栅路径覆盖面比 ASCII 路径宽：stateDiagram 也能画。
    #[test]
    fn 状态图能渲染成_png() {
        let png = Renderer::new(Theme::Light)
            .png(
                "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Running : start\n",
                &sizing(),
            )
            .expect("状态图应该能画出来");
        assert!(dimensions(&png).is_some());
    }

    /// 认不出图类型时返回 `None`，调用方据此退回显示源码。
    #[test]
    fn 认不出的源码不画图() {
        let renderer = Renderer::new(Theme::Light);
        assert!(!renderer.can_render("这根本不是 mermaid", &sizing()));
        assert!(!renderer.can_render("", &sizing()));
    }

    /// 出图尺寸要落在显示预算内。超出预算的图会把终端撑破。
    #[test]
    fn 出图不超过显示预算() {
        let s = sizing();
        let prepared = Renderer::new(Theme::Light)
            .prepare("flowchart TD\n  A --> B\n  B --> C\n  C --> D\n", &s)
            .unwrap();
        let (max_w, max_h) = s.pixel_box(s.max_cols, s.max_rows);
        assert!(
            prepared.encoded_pixel.0 <= max_w,
            "{} 超出预算宽 {max_w}",
            prepared.encoded_pixel.0
        );
        assert!(
            prepared.encoded_pixel.1 <= max_h,
            "{} 超出预算高 {max_h}",
            prepared.encoded_pixel.1
        );
    }

    /// 小图要放大到预算那么大，否则在 Retina 上是一堆糊字。
    /// mermaid 的节点和字号都是固定 CSS 像素，天然远小于 Retina 分辨率。
    #[test]
    fn 小图放大到显示预算() {
        let s = sizing();
        let source = "flowchart TD\n  A --> B\n";
        let prepared = Renderer::new(Theme::Light).prepare(source, &s).unwrap();
        // 预算按这张图自己的宽高比算，不是按满屏算
        let natural = s.fit(prepared.source_pixel.0, prepared.source_pixel.1);
        let (budget_w, budget_h) = s.pixel_box(natural.cols, natural.rows);
        // 至少放大到了预算的八成，否则等于没放大
        assert!(
            prepared.encoded_pixel.0 * 10 >= budget_w * 8,
            "宽度只放大到 {}，预算 {budget_w}",
            prepared.encoded_pixel.0
        );
        assert!(
            prepared.encoded_pixel.1 * 10 >= budget_h * 8,
            "高度只放大到 {}，预算 {budget_h}",
            prepared.encoded_pixel.1
        );
    }

    /// 显示尺寸要按图的宽高比来，不能一律按列数铺满，也不能一律压扁。
    #[test]
    fn 显示尺寸随图的宽高比变化() {
        let s = sizing();
        let renderer = Renderer::new(Theme::Light);

        // 横向宽图（867×70）铺满 80 列只要 3 行
        let mut wide = String::from("flowchart LR\n");
        for i in 0..6 {
            wide.push_str(&format!("  N{i} --> N{}\n", i + 1));
        }
        let fit = renderer.prepare(&wide, &s).unwrap();
        assert_eq!(fit.cells.0, s.max_cols, "宽图没有铺满列数");
        assert!(fit.cells.1 < s.max_rows, "宽图用不满行数");

        // 分支树（325×278）接近方形，行数到顶后列数收窄
        let tree = "flowchart TD\n A-->B\n A-->C\n A-->D\n B-->E\n C-->F\n D-->G\n";
        let fit = renderer.prepare(tree, &s).unwrap();
        assert_eq!(fit.cells.1, s.max_rows, "方图应该用满行数");
        assert!(
            fit.cells.0 > s.max_cols / 2,
            "方图只剩 {} 列宽",
            fit.cells.0
        );
    }

    /// 同样的源码必须算出同一个 id，否则终端不去重，同一张图反复画。
    #[test]
    fn 同样的源码算出同样的_id() {
        assert_eq!(
            identity("flowchart TD\n A-->B\n"),
            identity("flowchart TD\n A-->B\n")
        );
        assert_ne!(
            identity("flowchart TD\n A-->B\n"),
            identity("flowchart TD\n A-->C\n")
        );
        assert!(identity("x").starts_with("mermaid:"));
    }

    /// 缓存按源码区分。两段不同的源码不能共用一份字节。
    #[test]
    fn 缓存按源码区分() {
        let renderer = Renderer::new(Theme::Light);
        let a = renderer
            .prepare("flowchart TD\n A-->B\n", &sizing())
            .unwrap();
        let z = renderer
            .prepare("flowchart TD\n A-->Z\n", &sizing())
            .unwrap();
        assert_ne!(a.png, z.png, "不同的图拿到了同一份字节");
    }

    /// 画不出来的源码重试多少次都是同样的失败，不该每次都重新解析。
    #[test]
    fn 失败的源码不重复渲染() {
        let renderer = Renderer::new(Theme::Light);
        let first = renderer.prepare("不是 mermaid", &sizing()).unwrap_err();
        let again = renderer.prepare("不是 mermaid", &sizing()).unwrap_err();
        assert_eq!(first, again);
    }

    /// 放大倍数：`contain` 语义。小图填满预算，大图缩进预算，宽高比不变。
    #[test]
    fn 放大倍数按预算算() {
        // 小图：放大到预算那么大
        assert_eq!(upscale(100, 100, (800, 800)), 8.0);
        // 大图：缩到预算内
        assert_eq!(upscale(2000, 2000, (800, 800)), 0.4);
        // 宽图按宽度收窄，高宽比不变
        assert_eq!(upscale(1000, 100, (500, 500)), 0.5);
        // 尺寸为零时别算出 NaN 或 inf
        assert!(upscale(0, 100, (500, 500)).is_finite());
        assert!(upscale(100, 0, (500, 500)).is_finite());
    }

    #[test]
    fn 主题解析() {
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("DARK"), Some(Theme::Dark));
        assert_eq!(Theme::parse("neon"), None);
        assert_eq!(Theme::default(), Theme::Light);
    }

    /// 暗色主题要真的换掉配色，否则 `--mermaid-theme dark` 是空开关。
    #[test]
    fn 暗色主题的配色不同() {
        let s = sizing();
        let source = "flowchart TD\n  A[开始] --> B[结束]\n";
        let light = Renderer::new(Theme::Light).prepare(source, &s).unwrap();
        let dark = Renderer::new(Theme::Dark).prepare(source, &s).unwrap();
        assert_ne!(light.png, dark.png, "两个主题画出了同一张图");
    }
}
