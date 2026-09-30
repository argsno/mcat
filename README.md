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

预编译二进制，支持 macOS（arm64 / Intel）和 Linux（x86_64 / arm64）。也可以用 cargo 自己编译：

```console
$ cargo install --git https://github.com/argsno/mcat
```

## 用法

```console
$ mcat README.md                  # 渲染 Markdown，含图片
$ mcat README.md src/main.rs      # Markdown + 语法高亮
$ mcat a.png                      # 直接看一张图，铺满终端
$ mcat --image-rows 12 shot.jpg   # 限制成 12 行高
$ git log -p | mcat               # 标准输入按 Markdown 处理
$ mcat -n README.md               # 加行号
$ curl … | mcat -l json           # 强制指定语言
$ mcat --no-render README.md      # 完全等同 cat
```

<details>
<summary>全部选项</summary>

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

</details>

## 输出长什么样

- **始终带颜色**，重定向到文件也是——产物是给人看的。要纯文本用 `--no-render`（完全等同 `cat`），
  要无色排版用 `--no-color`。
- **不折行**，和 `cat` 一致；块之间固定留一个空行，一级标题下补一条等宽横线。
- **正文配色只用终端的 16 个基础色**，跟着你的调色板走；语法高亮来自 syntect 的真彩色主题，
  自动映射到 256 色。
- **表格按显示宽度对齐**（中文和全角算两列），表头加粗。
- `-n` 数的是输出行。渲染会重排块，数字和源文件对不上——和 `cat -n` 的行为一致。
- 不是合法 UTF-8 的文件原样输出，不做渲染。

## 图片

Markdown 里的图片按终端能力选协议：

| 协议 | 终端 | 备注 |
|---|---|---|
| Kitty 图形协议 | Ghostty、kitty、WezTerm、foot、Contour | 只认 PNG，其他格式转码 |
| iTerm2 内联图片 | iTerm2、WezTerm、VS Code | 原始字节交给终端解码，不转码 |

协议从环境变量猜（`TERM_PROGRAM`、`KITTY_WINDOW_ID` 等），可以用 `--image-protocol` 强制。
任何一环走不通——不是终端、猜不出协议、文件读不到、下载失败——都退回
`[image] 说明 (路径)` 的文字占位，不会报错。

`mcat a.png` 直接把图画出来，判断依据是文件内容而不是扩展名。嵌在 Markdown 里的图最多 20 行，
单独看一张图铺满终端，`--image-rows` / `--image-cols` 显式给了以你给的为准。
支持 PNG、JPEG、GIF、WebP、BMP。标准输入不参与图片识别——`git log -p | mcat` 的行为不会变。

图片没显示时用 `mcat --check-images` 定位：不输出正文，把协议从哪猜的、文件找没找到、格式、
最终占多少字符格、载荷多大，一路打出来。

## 开发

自己编译的步骤、代码结构、实现取舍见 [CONTRIBUTING.md](CONTRIBUTING.md)。
