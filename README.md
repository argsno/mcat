# mcat

一个会渲染 Markdown 的 `cat`。

- `.md` 文件渲染成终端样式：标题分级配色、列表、引用、表格、任务清单、脚注
- 代码块（以及 `.rs`、`.py`、`.toml` 等其他文件）交给 [syntect](https://github.com/trishume/syntect) 做语法高亮
- 行为和 `cat` 一致：多个文件依次输出、读标准输入、`-` 表示标准输入、读不了的文件报错后继续
- 纯 Rust 实现，没有 C 依赖

## 用法

```console
$ mcat README.md                 # 渲染 Markdown
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
  -h, --help             Print help (see more with '--help')
  -V, --version          Print version
```

## 几个设计上的选择

**输出始终带颜色。** 和 `cat` 不同，`mcat` 的产物是给人看的，重定向到文件时也保留样式。
需要纯文本时用 `--no-render`（完全等同 `cat`）或 `--no-color`（保留排版，去掉颜色）。

**颜色只用 ANSI 0-15 的主题色。** 正文配色（标题、链接、引用竖线等）只取终端的 16 个基础色，
这样用户自定义终端调色板时整套输出依然协调。语法高亮的颜色来自 syntect 主题，
是真彩色，会被映射到 256 色里最接近的一个，保证在不支持真彩色的终端上也不会跑偏。

**不重新排版，只上样式。** 行不会按终端宽度折行（`cat` 也不折），块与块之间固定留一个空行。
唯一的例外是一级标题下面会补一条等宽的横线。

**表格按显示宽度对齐。** 用 `unicode-width` 计算，中文和全角符号算两列。
列宽取该列所有单元格（含表头）的最大值，表头加粗。

**紧凑与松散列表按 CommonMark 区分。** 紧凑列表条目之间不留空行，松散列表留空行。
判断依据是 pulldown-cmark 是否为条目发出了段落事件。

**MIME 和二进制。** 文件不是合法 UTF-8 时原样输出，不做任何渲染。

**`-n` 数的是输出行。** Markdown 渲染会重排块（一级标题多一行横线、块之间插空行），
所以行号和源文件的行号对不上，和 `cat -n` 数输出行的行为一致。

## 开发

```console
$ cargo build --release
$ cargo test
$ cargo clippy --all-targets
```

代码结构：

| 文件 | 作用 |
|---|---|
| `src/main.rs` | 命令行、文件读取、按类型分发 |
| `src/markdown.rs` | Markdown 解析成块/行内树，再渲染成 ANSI 文本 |
| `src/highlight.rs` | syntect 封装：按语言逐行上色 |
| `src/style.rs` | 样式、显示宽度计算、带状态的输出器 |

`src/style.rs` 里的 `Painter` 只在样式变化时写转义序列，并在每行结束时复位，
所以管道出去的文本不会积累多余的转义码。

## 依赖

| crate | 用途 |
|---|---|
| `pulldown-cmark` | CommonMark 解析 |
| `syntect` | 语法高亮（fancy-regex 后端，无 C 依赖） |
| `unicode-width` | 东亚宽度计算 |
| `clap` | 命令行解析 |
