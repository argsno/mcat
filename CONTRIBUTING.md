# 给 mcat 贡献代码

面向开发者。`mcat` 怎么用见 [README.md](README.md)。

## 环境

Rust edition 2024，需要较新的工具链（本项目用 1.97 开发）。除此之外没有别的依赖，
没有 C 编译器，不需要装任何系统库。

```console
$ cargo build --release
$ cargo test
$ cargo clippy --all-targets
```

`cargo test` 跑的是内联在各源文件末尾的 `#[cfg(test)] mod tests`（在 `style.rs`、`markdown.rs`、
`image.rs` 里）。手工验证渲染效果用 `examples/demo.md`：

```console
$ mcat examples/demo.md
```

## 代码结构

| 文件 | 作用 |
|---|---|
| `src/main.rs` | 命令行、文件读取、按类型分发 |
| `src/markdown.rs` | Markdown 解析成块/行内树，再渲染成 ANSI 文本 |
| `src/highlight.rs` | syntect 封装：按语言逐行上色 |
| `src/image.rs` | 图片协议、下载缓存、降采样转码 |
| `src/mermaid.rs` | mermaid 源码 → SVG → PNG |
| `src/style.rs` | 样式、显示宽度计算、带状态的输出器 |

`src/style.rs` 里的 `Painter` 只在样式变化时写转义序列，并在每行结束时复位，
所以管道出去的文本不会积累多余的转义码。改渲染逻辑时别绕过 `Painter` 直接拼转义序列。

## 渲染管线上的取舍

**紧凑与松散列表怎么判断。** 紧凑列表条目之间不留空行，松散列表留空行。
判断依据是 pulldown-cmark 是否为该条目发出了段落事件，不要自己数空行。

**行号是输出行号。** 渲染会重排块（一级标题多一行横线、块之间插空行），
所以 `-n` 的数字和源文件的行号对不上。这是有意的：和 `cat -n` 数输出行的行为保持一致。

**不折行。** 行不会按终端宽度折行（`cat` 也不折）。想要折行请用别的工具。

## 图片链路的取舍

这几条都是踩过坑之后定下来的，改之前先读完再动：

- **只在终端里画图。** 转义序列里是大段 base64，进管道就是垃圾。`mcat README.md | grep foo`
  拿到的还是纯文本。
- **按显示尺寸降采样。** Kitty 协议要求 PNG，一张 4000×3000 的 JPEG 转码后能生成几十 MB 的
  转义序列，足以卡死终端。所以先缩到显示尺寸的两倍分辨率再编码（字符格按 Retina 约 16×32 像素算）。
- **Kitty 载荷按 4096 字节分块。** 这是协议上限。除最后一块外都带 `m=1`（后面还有数据），
  最后一块带 `m=0`——单块图片也必须发 `m=0`，因为终端在收齐并校验完整个序列之前什么都不画。
  完整控制数据只发第一块，后续块只带 `m`，每块重复 `a=T` 会被当成一次次的全新传输。
- **只发 `c` 或 `r` 其中一个维度。** 另一个由终端按图片宽高比自己算——终端最清楚自己字符格的实际
  像素尺寸。规范说同时给两个维度会 letterbox，实测 Ghostty 并非如此：宽高比不匹配时图直接不显示，
  匹配时也会被拉伸变形。mcat 只估算该发哪个维度，精确计算交给终端。
- **远程图片有缓存。** 存在 `$XDG_CACHE_HOME/mcat/`（没有则 `~/.cache/mcat/`），
  有效期 24 小时，按 URL 的 SHA-256 命名。下载失败时会用过期缓存，不让图片整个消失。
- **下载有上限。** 单张图片超过 20 MB 直接放弃；超时 30 秒。
- **表格里不嵌图片。** 控制序列会撑破单元格，那里也退回文字占位。
- **标准输入不参与图片识别。** `git log -p | mcat` 的行为不能变。

## mermaid 图的取舍

` ```mermaid ` 代码块渲染成 PNG，之后完全走图片链路——同一套分块、尺寸、id 计算。
所以「只在终端里画图」「任何一环走不通退回文字」这些规则对图同样成立，不需要额外机制。

- **走 PNG 而不是框线字符。** merman 有一套 ASCII 渲染器（`ascii` feature），纯文本能进管道、
  能被 grep，但实测质量不够：圆角矩形的边框会叠两层，子程序节点左右多出竖线，
  每行都带行尾空白，`flowchart LR` 六个节点就 115 列宽（mcat 不折行，直接溢出）。
  PNG 的输出质量好得多，代价是只在支持图形协议的终端里有图。
- **渲染成 PNG 再交给图片管线，不要各写一套。** 两处自己算尺寸就会算出不一样的结果，
  图片 id 也对不上（终端会因此反复画同一张图）。
- **放大倍数自己算。** merman 的 `fit_to` 只缩不放，但 mermaid 的节点和字号是固定 CSS 像素，
  天然远小于 Retina 分辨率——不放大就是一堆糊字。所以按显示预算算 `contain` 的倍数，
  小图放大、大图缩小，放大后总归落在预算里。
- **`size_limit` 要压住。** merman 默认放到 8192×8192（约 270 MB 显存），对 `cat` 太宽了。
  按显示预算封顶，防止一张病态的图吃满内存。
- **缓存按源码哈希，失败也缓存。** 一份文档里同一张图出现多次很常见；重试一百次也是同样的失败，
  不该每次都重新解析。
- **只用 `render` + `raster` 两个 feature。** 默认 feature 是空的，但 `ascii`、`pdf`、
  `eframe`（GUI）都在可选列表里，别顺手开。
- **CJK 靠系统字体。** 光栅化时 fontdb 扫系统字体，中文标签需要系统装了中文字体。
  没装的话标签画不出来，但流程和箭头还在。

## 图片尺寸估算

字符格是竖着的：按 Retina 约 16 像素宽、32 像素高。所以高宽比是 2，**不是 0.5**。

`Sizing::fit` 里 `rows = cols * (h/w) / CELL_ASPECT`，`CELL_ASPECT = CELL_H / CELL_W = 2`。
写成 `CELL_W / CELL_H`（也就是拿宽除以高）会让所有图片都算小四倍：一张 4000×3000 的照片
在 80×20 的格里只能摆 13×20，实际能摆 53×19。这个常数错了图片还能显示，只是小得离谱，
很容易被当成「终端就这样」放过去。

调试图片问题时，除了 `mcat --check-images`，还可以拿规范里的参考实现交叉验证：

```console
$ sh examples/send-png.sh examples/gradient.png
```

它抄自 [Kitty 图形协议规范](https://sw.kovidgoyal.net/kitty/graphics-protocol/)的 POSIX sh 示例。
如果它能显示而 mcat 不能，那是 mcat 的 bug。

## 依赖

| crate | 用途 |
|---|---|
| `pulldown-cmark` | CommonMark 解析 |
| `syntect` | 语法高亮（fancy-regex 后端，无 C 依赖） |
| `image` | 非 PNG 格式转码 + 降采样 |
| `ureq` | 下载远程图片（rustls，无 C 依赖） |
| `terminal_size` | 终端宽度 |
| `base64` | 图形协议载荷编码 |
| `sha2` | 远程图片缓存键、mermaid 图 id |
| `unicode-width` | 东亚宽度计算 |
| `clap` | 命令行解析 |
| `merman` | mermaid 图解析、布局、光栅化 |

依赖从 4 个涨到 10 个（256 个传递依赖），冷构建从 6 秒涨到 74 秒。

`image` 和 `ureq` 是图片功能带来的，`merman` 是 mermaid 图带来的。`merman` 会带进
resvg、tiny-skia、fontdb（找系统字体）和 lalrpop（构建期生成解析器）——它是传递依赖里
最重的一个，也是冷构建时间翻倍的原因。

加新依赖前先掂量一下冷构建时间，并保持「纯 Rust、无 C 依赖」这条约束——
`syntect` 特意选了 fancy-regex 而不是默认的 onig，`ureq` 特意关了 default-features 只留 rustls，
`merman` 没有 C 依赖但会让构建慢一个数量级，所以只开了它需要的 `render` + `raster`
两个 feature（`ascii` 那套框线字符渲染器用不上）。

「无 C 依赖」这条仍然成立：`core-foundation-sys`（merman 经 chrono 带进来的）只有 extern 声明，
没有 build.rs，不编译 C 代码；唯一拉 `cc` 的是 `ring`，而它本来就在 `ureq` 的链路上。

## 发版

打 tag（`v0.1.0`，必须和 Cargo.toml 的 `version` 一致，CI 会校验）触发 [release.yml](.github/workflows/release.yml)，
它做三件事：构建四个平台的 release 二进制（macOS arm64 / Intel、Linux x86_64 / arm64）→
发布 GitHub Release（tar.gz + sha256）→ 把 [Formula/mcat.rb](Formula/mcat.rb) 模板填上 tag 和校验和，
推到 `argsno/homebrew-tap`。之后用户 `brew install argsno/tap/mcat` 就装上了。

首次配置需要一次性做两件事（发布时已配好，这里记录备查，也方便以后轮换）：

1. 建 GitHub 仓库 `argsno/homebrew-tap`。空仓库即可，CI 会往 `Formula/` 写文件。
   仓库名必须是 `homebrew-tap`：`brew install argsno/tap/mcat` 里的 `argsno/tap`
   就是靠这个命名约定映射过去的。
2. 配一把只对 tap 有写权限的 SSH 部署密钥：生成一对 ed25519 密钥，公钥加进
   homebrew-tap 的 Deploy keys 并勾选写权限，私钥配成 mcat 仓库的 Actions secret
   `TAP_DEPLOY_KEY`。不用 PAT——PAT 的作用域管理起来容易过宽，部署密钥天然只对
   tap 这一个仓库生效。

两个注意点：

- **formula 模板里是占位符，不能直接拿去装。** `Formula/mcat.rb` 的 `__TAG__`、`__SHA_*__`
  由 CI 填充，真实可用的 formula 只存在于 tap 仓库里。
- **Linux arm64 是交叉编译的。** 项目没有 C 依赖，交叉编译只需要在 ubuntu runner 上
  装一个 `gcc-aarch64-linux-gnu` 当链接器，没有架构相关的头文件要操心。这也是
  「无 C 依赖」约束在发版上的红利。
