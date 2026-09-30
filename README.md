# mcat

一个会渲染 Markdown 的 `cat`。

- `.md` 文件渲染成终端样式：标题分级配色、列表、引用、表格、任务清单、脚注
- 代码块（以及 `.rs`、`.py`、`.toml` 等其他文件）交给 [syntect](https://github.com/trishume/syntect) 做语法高亮
- Markdown 里的图片直接画在终端上（Kitty 图形协议 / iTerm2 内联图片）
- 行为和 `cat` 一致：多个文件依次输出、读标准输入、`-` 表示标准输入、读不了的文件报错后继续
- 纯 Rust 实现，没有 C 依赖

## 安装

```console
$ brew install argsno/tap/mcat
```

装的是 release workflow 预编译好的二进制（macOS arm64 / Intel，Linux x86_64 / arm64）。
也可以用 cargo 自己编译：

```console
$ cargo install --git https://github.com/argsno/mcat
```

发版和 tap 的配置说明见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 用法

```console
$ mcat a.png                     # 直接看一张图，铺满终端
$ mcat --image-rows 12 shot.jpg  # 限制成 12 行高
$ mcat README.md                 # 渲染 Markdown，含图片
$ mcat README.md src/main.rs     # Markdown + 语法高亮
$ git log -p | mcat               # 标准输入按 Markdown 处理
$ mcat -n README.md               # 加行号
$ curl … | mcat -l json            # 强制指定语言
$ mcat --no-render README.md      # 完全等同 cat
```

```console
$ mcat -h
带 Markdown 渲染的 cat

Usage: mcat [OPTIONS] [FILE]...

Arguments:
  [FILE]...  要输出的文件，`-` 表示标准输入

Options:
  -n, --numbers          给每一行加行号
  -l, --language <LANG>  强制指定语言：md、text、rust 等，不靠扩展名猜
      --theme <THEME>    代码高亮主题，深色终端请用 .dark 结尾的主题 [default: base16-ocean.dark]
      --no-render        不渲染，原样输出（等同 cat）
      --no-color         不输出颜色
      --list-themes      列出所有可用主题后退出
      --no-images               不显示图片，只输出文字占位
      --image-protocol <PROTO>  图片协议：auto、kitty、iTerm2 [default: auto]
      --image-rows <N>          图片最多占多少行 [default: 20]
      --image-cols <N>          图片最多占多少列，0 表示按终端宽度 [default: 0]
      --check-images            不输出内容，只报告图片链路的每一项决策
  -h, --help                    Print help (see more with '--help')
  -V, --version                 Print version
```

`-n` 数的是**输出行**，不是源文件的行号。Markdown 渲染会重排块（一级标题多一行横线、块之间插空行），
所以行号和源文件对不上——和 `cat -n` 数输出行的行为一致。

## 输出长什么样

**输出始终带颜色。** 和 `cat` 不同，`mcat` 的产物是给人看的，重定向到文件时也保留样式。
需要纯文本时用 `--no-render`（完全等同 `cat`）或 `--no-color`（保留排版，去掉颜色）。

**正文配色只用终端的 16 个基础色。** 标题、链接、引用竖线这些只取 ANSI 0-15，
这样你自定义终端调色板时整套输出依然协调。语法高亮的颜色来自 syntect 主题，是真彩色，
会被映射到 256 色里最接近的一个，在不支持真彩色的终端上也不会跑偏。

**不重新排版，只上样式。** 行不会按终端宽度折行（`cat` 也不折），块与块之间固定留一个空行。
唯一的例外是一级标题下面会补一条等宽的横线。

**表格按显示宽度对齐。** 用 `unicode-width` 计算，中文和全角符号算两列。
列宽取该列所有单元格（含表头）的最大值，表头加粗。

**紧凑与松散列表按 CommonMark 区分。** 紧凑列表条目之间不留空行，松散列表留空行。

**MIME 和二进制。** 文件不是合法 UTF-8 时原样输出，不做任何渲染。

## 图片

Markdown 里的图片按终端能力选协议：

| 协议 | 终端 | 备注 |
|---|---|---|
| Kitty 图形协议 | Ghostty、kitty、WezTerm、foot、Contour | 只认 PNG，其他格式转码 |
| iTerm2 内联图片 | iTerm2、WezTerm、VS Code | 原始字节交给终端解码，不转码 |

协议从环境变量猜（`TERM_PROGRAM`、`KITTY_WINDOW_ID`、`GHOSTTY_RESOURCES_DIR` 等），
可以用 `--image-protocol` 强制。猜不出来、不是终端、文件读不到、下载失败——任何一种情况都退回
`[image] 说明 (路径)` 的文字占位，不会报错。

### 直接看图片

`mcat a.png` 会直接把图画出来，等价于一个极简的看图工具。判断看的是**文件内容**
（magic bytes）而不是扩展名，所以扩展名写着 `.jpg` 但实际是 PNG 的文件也能正常显示。

两套默认尺寸：嵌在 Markdown 里的图最多 20 行，免得把正文顶出屏幕；单独看一张图
铺满终端。`--image-rows` / `--image-cols` 显式给了就以你给的为准。识别支持的格式：
PNG、JPEG、GIF、WebP、BMP。

标准输入不参与图片识别 —— `git log -p | mcat` 的行为不会变。

### 排查：图片没出来

`--check-images` 不输出正文，只把链路上每一环的决策打出来——协议从哪猜的、文件找没找到、
格式是什么、最终占多少字符格、载荷多大、序列长什么样。图片没显示时用它定位卡在哪一步：

```console
$ mcat --check-images README.md
终端程序    ghostty
TERM        xterm-256color
stdout      终端 ✓ 会画图
图片协议    Kitty（从环境变量推断）
显示上限    112 列 x 20 行（--image-cols 0，--image-rows 20）

examples/gradient.png
  文件      examples/gradient.png
  格式      PNG（48x24 像素）
  显示      20 列 x 20 行
  载荷      2816 字节 -> base64 3756 字符 -> 1 块
  序列      <ESC>_Ga=T,f=100,i=3416104704,q=2,c=20,r=20,m=1;iVBO…（共 3802 字节）
  结论      ✓ 会输出图形序列
```

它不受 TTY 限制，重定向输出时也会照常分析（只在开头提示实际运行时不会画图）。

## 开发

自己编译的步骤、代码结构、图片链路的实现取舍见 [CONTRIBUTING.md](CONTRIBUTING.md)。
