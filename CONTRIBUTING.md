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

`cargo test` 跑的是内联在各源文件末尾的 `#[cfg(test)] mod tests`。手工验证渲染效果用
`examples/demo.md`：

```console
$ mcat examples/demo.md
$ mcat examples/demo.rst
```

## 代码结构

| 文件 | 作用 |
|---|---|
| `src/main.rs` | 命令行、文件读取、按类型分发 |
| `src/markdown.rs` | Markdown 解析成块/行内树，再渲染成 ANSI 文本 |
| `src/rst.rs` | reStructuredText 解析成 docutils doctree，再渲染成 ANSI 文本 |
| `src/table.rs` | 两种格式共用的表格列宽、折行和对齐 |
| `src/highlight.rs` | syntect 封装：语法集、别名、内置主题、按语言逐行上色 |
| `src/image.rs` | 图片协议、下载缓存、降采样转码 |
| `src/mermaid.rs` | mermaid 源码 → SVG → PNG |
| `src/terminal.rs` | 查终端背景色，决定图和代码主题的明暗 |
| `src/config.rs` | 配置文件路径、TOML 解析、翻译成命令行默认值 |
| `src/style.rs` | 样式、显示宽度计算、带状态的输出器 |

`src/style.rs` 里的 `Painter` 只在样式变化时写转义序列，并在每行结束时复位，
所以管道出去的文本不会积累多余的转义码。改渲染逻辑时别绕过 `Painter` 直接拼转义序列。

## 渲染管线上的取舍

**紧凑与松散列表怎么判断。** 紧凑列表条目之间不留空行，松散列表留空行。
判断依据是 pulldown-cmark 是否为该条目发出了段落事件，不要自己数空行。

**行号是输出行号。** 渲染会重排块（一级标题多一行横线、块之间插空行），
所以 `-n` 的数字和源文件的行号对不上。这是有意的：和 `cat -n` 数输出行的行为保持一致。

**不折行。** 行不会按终端宽度折行（`cat` 也不折）。想要折行请用别的工具。表格是例外，
见下面一条。

**表格折行只在终端里做。** 管道和重定向里拿不到宽度，也不该把产物按 80 列折掉，
所以 `table_width` 只在 stdout 是终端时给宽度（开了 `-n` 还要扣掉行号占的列）。
列宽是挑当前最宽的列一格格削出来的（`fit_widths`），每列至少留两格：一个汉字或 emoji
就占两格，再窄内容一定会挤出框线。连这个下限都放不下时保持自然宽度——宁可整张表超出
终端、让终端自己折行，也不能把框线挤断。

**表格宽度按整串算，不要逐字符相加。** 中文和全角占 2 列，多码点 emoji 整体也只占 2 列：
`⚠️` 是「基本字符 + 变体选择符」，`👍🏽` 是肤色修饰，`👨‍👩‍👧` 是 ZWJ 组合，`🇨🇳` 是一对区域指示符。
`display_width` 用 `UnicodeWidthStr::width` 整串算（转义序列把串切成几段，逐段算）。
换成 `UnicodeWidthChar::width` 逐字符相加就会错：VS16 变体少算一格，肤色修饰和 ZWJ 组合多算好几格。
这种错位在测试里很难自己冒出来——mcat 补的白和它自己算的宽度永远自洽，内部一致性能对上，
只有按「终端会画几格」写断言才抓得住。折行同理：`wrap_styled` 不会从续接字符（ZWJ、肤色、
组合音）前面断开，宁可让那一行超出一格。

**行间横线按逻辑行画。** 单元格折成多行时，续行属于同一行，之间不插横线；
`├┼┤` 只画在两个逻辑行之间，最后一行由底线收口。

**`<br>` 是行内换行。** 表格单元格里没法直接换行，习惯用 `<br>` 断行，所以建树时空的
`<br>`（`<br>` / `<br/>` / `<br />`，大小写不限）变成一个换行节点，渲染时不补硬换行那个反斜杠。
单元格先按这些换行拆成多行，再逐行按列宽折行；列宽取最长的那行。带属性的（`<br clear="all">`）
和代码里的 `<br>` 一律不动，原样显示。

**语法高亮写真彩色，不再量化到 256 色。** 量化误差会把 `base16-ocean.dark` 的函数蓝
`#8fa1b3` 压成灰 247 `#9e9e9e`，函数名和正文分不开。`Style::Color` 区分 `Ansi(u8)` 和
`Rgb(u8, u8, u8)`：正文继续用 ANSI 0–15，跟随用户调色板；语法高亮用 RGB，SGR 写 `38;2`。

**`auto` 主题懒查询。** `--theme auto` 默认按终端背景选 `mcat-dark` / `mcat-light`；
查询在第一次真正高亮代码时才发生（`Highlighter::theme` 的 `OnceLock`），没有代码块的文档
不会为 OSC 11 等待。探测不到背景时按浅色走，和 mermaid 的默认一致。

**代码块色条是背景色空格，不是字符。** Markdown 代码块每行前面写一格带背景色的空格，
再写一格普通空格；复制出去只有两个前导空格，和原来的缩进一样。单独看源码文件不加色条，
保持顶格，复制源码不会多出空格。

**语言识别顺序固定：文件名 → 扩展名 → 首行 shebang。** 文件名要排第一是因为
`Dockerfile`、`Makefile`、`.gitignore`、`Cargo.lock` 这些写法登记在语法的扩展名列表里，
按扩展名查会被当成没有扩展名。别名表只收「换一份语法仍然正确」的写法，不猜语言；
bat 的 fancy 语法集用不了 JavaScript (Babel) 和 PowerShell（fancy-regex 不支持子例程调用），
所以 `jsx` 退到会解析 JSX 的 tsx 语法，PowerShell 没有替代，保持不上色。

**配置文件是参数，不是第二套合并逻辑。** `src/config.rs` 把 TOML 配置项翻译成命令行参数，
插在真实参数前面再让 clap 解析一次；`args_override_self` 让同名参数后面的赢，所以
「命令行覆盖配置」不需要为每个选项判断「命令行有没有给」。代价是布尔项必须有反义参数
（`--no-numbers`、`--color`、`--images`），否则配置文件打开的开关命令行关不掉。
配置只收默认样式，不收这一次调用要做什么：文件列表、`--language`、`--no-render`、
`--list-themes`、`--check-images` 都不能写进去。

## reStructuredText 渲染的取舍

**用 lsdocs-rst，不自己写子集。** 它产出的 doctree 和 Python docutils 对齐，表格、替换引用、
脚注、行内角色和指令的边界都有现成实现；纯 Rust，只新增 `unicode-normalization`、`tinyvec` 两个传递依赖。
代价是 API 还在 0.x，升级时要跑一遍 `cargo test` 和真实 RST 文档。

**只渲染终端里有意义的节点。** 注释、链接目标、替换定义、系统消息、raw 和页眉页脚不进正文；
未知指令用 `keep_unknown_directives(true)` 保留成 `directive` 节点，显示一行 `.. name::` 和
它的内容，而不是把内容吞掉。Sphinx 的 `:func:`、`:ref:` 等角色注册成普通行内文字，只去掉
角色标记，不解析交叉引用。

**标题层级按 docutils 的规则。** 文档标题是 h1；有文档标题时一级章节从 h2 开始，和 docutils
的 HTML 输出一致。`doctitle_xform` 默认开着，所以只有一个顶层章节的文档会把章节标题提升成
文档标题——这是 docutils 的行为，不是渲染器自己判断的。

**表格单独实现。** Markdown 的表格模型只有行内单元格，RST 的 `entry` 可以包含任意块，还带
`morecols`/`morerows`。所以 `src/rst.rs` 自己搭网格：先按跨行跨列占位，再算列宽、折行、
画框线；顶边和底边也要看首尾行有没有跨列，不能一律画 `┬`/`┴`。单元格里的图片和 mermaid
仍然退回文字，原因是控制序列会撑破框线。

**include 按源文件目录读，标准输入不读盘。** `main` 把文件路径传给 `Rst::with_source`，
解析器用它做相对路径基准，并通过 `file_loader` 读 `include` 和 `csv-table :file:`；标准输入
没有路径，加载器保持关闭。被禁用的指令会变成带 `literal_block` 的 `system_message`，
渲染器保留那段原文，内容不会静默消失。

**替换图片由 transforms 展开。** `.. |name| image::` 在 `transforms(true)` 之后已经变成正文
里的 inline image，渲染器不需要再维护替换表；`--check-images` 收集图片时跳过
`substitution_definition`，避免同一张图报两次。

**代码块语言来自 `classes`。** docutils 把 `.. code-block:: rust` 记成
`classes="code rust"`，取第一个不是 `code` 的类名交给 syntect；`.. code-block:: mermaid`
也走同一条判断。

**列宽和折行与 Markdown 共用 `src/table.rs`。** 两边如果各算一套，终端里同一张表在
两种格式下会出现不同的框线宽度；改下限、对齐或折行规则时只需要动一处。

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
- **底色透明，明暗跟终端。** SVG 根节点默认带 mermaid 的 `background-color:white`，
  深色终端里就是一块白方块。所以主题的 `root_background` 显式写成 `transparent`，
  让终端背景透出来。明暗由 `src/terminal.rs` 用 OSC 11 查（直接开 `/dev/tty`，
  stdout/stderr 被重定向也能查），查不到看 `COLORFGBG`，再不行按亮色。
  查询是懒的：没有 mermaid 块的文档不会等这一下。
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
| `two-face` | 语法定义集（bat 的语法集，fancy 变体，无 C 依赖） |
| `lsdocs-rst` | reStructuredText 解析（docutils 的 Rust 移植，无 C 依赖） |
| `image` | 非 PNG 格式转码 + 降采样 |
| `ureq` | 下载远程图片（rustls，无 C 依赖） |
| `terminal_size` | 终端宽度 |
| `base64` | 图形协议载荷编码 |
| `sha2` | 远程图片缓存键、mermaid 图 id |
| `unicode-width` | 东亚宽度计算 |
| `rustix` | 终端 termios（查背景色时切原始模式） |
| `clap` | 命令行解析 |
| `toml` | 配置文件解析 |
| `merman` | mermaid 图解析、布局、光栅化 |

依赖从 4 个涨到 14 个（Cargo.lock 里 370 个包），冷构建从 6 秒涨到 82 秒左右。

`image` 和 `ureq` 是图片功能带来的，`merman` 是 mermaid 图带来的，`rustix` 原本就在
`terminal_size` 的依赖树里，这次只多开了 `termios` 这一个 feature，没有新增编译单元。
`toml` 是配置文件带来的，只开 `parse` + `serde` + `std`：解析成 `toml::Value` 就够了，
不需要序列化和它自带的显示支持；新增的传递依赖是 serde_spanned、toml_datetime、
toml_parser、winnow，`serde_core` 本来就在 merman 的依赖树里。
`lsdocs-rst` 是 reStructuredText 带来的，默认特性下只新增 `unicode-normalization`
和 `tinyvec` 两个编译单元，没有 C 依赖。
`merman` 会带进
resvg、tiny-skia、fontdb（找系统字体）和 lalrpop（构建期生成解析器）——它是传递依赖里
最重的一个，也是冷构建时间翻倍的原因。

加新依赖前先掂量一下冷构建时间，并保持「纯 Rust、无 C 依赖」这条约束——
`syntect` 特意选了 fancy-regex 而不是默认的 onig，`ureq` 特意关了 default-features 只留 rustls，
`merman` 没有 C 依赖但会让构建慢一个数量级，所以只开了它需要的 `render` + `raster`
两个 feature（`ascii` 那套框线字符渲染器用不上）。

`syntect` 只开 `default-themes`、`parsing`、`regex-fancy` 三个 feature：
`default-fancy` 还会带进 html、plist-load、yaml-load 和默认语法集，这些 mcat 都用不到，
`plist`、`yaml-rust` 也就跟着整棵依赖树一起消失了。

`two-face` 只多一个编译单元，但必须和 `syntect` 用同一套 fancy 变体；它的
`default-features` 会开 onig 变体，那会拉回 C 依赖，所以同样关了默认特性。

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
