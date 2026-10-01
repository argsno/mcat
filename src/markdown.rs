//! Markdown 解析与终端渲染。
//!
//! 解析用 pulldown-cmark 拿到事件流，先建成一棵简单的块/行内树，
//! 再递归渲染成带 ANSI 样式的文本。代码块交给 syntect 上色。

use crate::highlight::Highlighter;
use crate::image::{Renderer as ImageRenderer, Sizing};
use crate::mermaid::Renderer as MermaidRenderer;
use crate::style::{Color, Painter, Style, TextBuf, display_width};
use pulldown_cmark::{
    Alignment as Align, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag,
};
use std::collections::HashMap;

// 配色只用 ANSI 0-15 的主题色，这样用户改终端调色板时整体依然协调。
const C_H1: Color = Color(14);
const C_H2: Color = Color(12);
const C_H3: Color = Color(13);
const C_H4: Color = Color(10);
const C_H5: Color = Color(11);
const C_H6: Color = Color(15);
const C_CODE_SPAN: Color = Color(3);
const C_LINK: Color = Color(12);
const C_MUTED: Color = Color(8);
const C_BULLET: Color = Color(6);
const C_TASK_DONE: Color = Color(10);

const S_H1: Style = Style::new().bold().fg(C_H1);
const S_H2: Style = Style::new().bold().fg(C_H2);
const S_H3: Style = Style::new().bold().fg(C_H3);
const S_H4: Style = Style::new().bold().fg(C_H4);
const S_H5: Style = Style::new().bold().fg(C_H5);
const S_H6: Style = Style::new().bold().fg(C_H6);
const S_CODE_SPAN: Style = Style::new().fg(C_CODE_SPAN);
const S_LINK: Style = Style::new().fg(C_LINK).underline();
const S_MUTED: Style = Style::new().fg(C_MUTED);
const S_BULLET: Style = Style::new().fg(C_BULLET);
const S_BORDER: Style = Style::new().fg(C_MUTED);
const S_RULE: Style = Style::new().fg(C_MUTED);
const S_IMAGE: Style = Style::new().dim();

/// 代码块相对正文缩进两格。
const CODE_INDENT: &str = "  ";
/// 水平分隔线的宽度。
const RULE_WIDTH: usize = 60;
/// 认成 mermaid 图的代码块语言标记。
const MERMAID_LANG: &str = "mermaid";

fn heading_style(level: u8) -> Style {
    match level {
        1 => S_H1,
        2 => S_H2,
        3 => S_H3,
        4 => S_H4,
        5 => S_H5,
        _ => S_H6,
    }
}

// ---------------------------------------------------------------- 语法树

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Kind {
    #[default]
    Document,
    // 块级
    Paragraph,
    Heading(u8),
    CodeBlock(Option<String>),
    Quote,
    List {
        ordered: bool,
        start: u64,
        /// 紧凑列表：条目之间不额外留空行。
        tight: bool,
    },
    Item,
    /// 表格，值是各列的对齐方式
    Table(Vec<Align>),
    TableHead,
    TableRow,
    /// 表格单元格，值是它在第几列（对齐方式按列给出，存在 Table 节点上）
    TableCell(usize),
    Rule,
    Html,
    FootnoteDef,
    /// 不额外处理的容器，只把子节点渲染出来。
    Group,
    // 行内
    Text,
    CodeSpan,
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
    Image(String),
    SoftBreak,
    HardBreak,
    Task(bool),
    FootnoteRef,
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub kind: Kind,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    fn new(kind: Kind) -> Self {
        Node {
            kind,
            ..Default::default()
        }
    }
    fn with_text(kind: Kind, text: String) -> Self {
        Node {
            kind,
            text,
            children: Vec::new(),
        }
    }
    fn is(&self, kind: &Kind) -> bool {
        &self.kind == kind
    }
}

/// 用事件流建树。事件是严格配对的，`stack[0]` 永远是根节点。
struct Builder {
    stack: Vec<Node>,
    /// 表格各列的对齐方式，表头和正文共用同一份。
    aligns: Vec<Align>,
    /// 当前行的列号。
    col: usize,
}

impl Builder {
    fn new() -> Self {
        Builder {
            stack: vec![Node::new(Kind::Document)],
            aligns: Vec::new(),
            col: 0,
        }
    }

    fn push(&mut self, node: Node) {
        let Some(parent) = self.stack.last_mut() else {
            return;
        };
        // 连续的文本片段合并成一段，减少样式切换
        if node.kind == Kind::Text
            && let Some(last) = parent.children.last_mut()
            && last.kind == Kind::Text
        {
            last.text.push_str(&node.text);
            return;
        }
        parent.children.push(node);
    }

    /// 代码块和 HTML 块的内容直接存进节点的 `text`，不拆成子节点。
    fn text(&mut self, s: String) {
        match self.stack.last_mut() {
            Some(node) if matches!(node.kind, Kind::CodeBlock(_) | Kind::Html) => {
                node.text.push_str(&s)
            }
            _ => self.push(Node::with_text(Kind::Text, s)),
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => {
                let node = match tag {
                    Tag::Paragraph => Node::new(Kind::Paragraph),
                    Tag::Heading { level, .. } => Node::new(Kind::Heading(level_number(level))),
                    Tag::CodeBlock(kind) => Node::new(Kind::CodeBlock(language(kind))),
                    Tag::BlockQuote(_) => Node::new(Kind::Quote),
                    Tag::List(start) => Node::new(Kind::List {
                        ordered: start.is_some(),
                        start: start.unwrap_or(1),
                        tight: true,
                    }),
                    Tag::Item => Node::new(Kind::Item),
                    Tag::FootnoteDefinition(name) => {
                        Node::with_text(Kind::FootnoteDef, name.to_string())
                    }
                    Tag::Table(aligns) => {
                        self.aligns = aligns.clone();
                        Node::new(Kind::Table(aligns))
                    }
                    Tag::TableHead | Tag::TableRow => {
                        self.col = 0;
                        let kind = if matches!(tag, Tag::TableHead) {
                            Kind::TableHead
                        } else {
                            Kind::TableRow
                        };
                        Node::new(kind)
                    }
                    Tag::TableCell => {
                        let node = Node::new(Kind::TableCell(self.col));
                        self.col += 1;
                        node
                    }
                    // 元数据块（YAML front matter）不是正文，跳过
                    Tag::MetadataBlock(_) => Node::new(Kind::Document),
                    Tag::HtmlBlock => Node::new(Kind::Html),
                    // 定义列表没有专门的样式，按普通容器渲染
                    Tag::DefinitionList
                    | Tag::DefinitionListTitle
                    | Tag::DefinitionListDefinition => Node::new(Kind::Group),
                    Tag::Superscript | Tag::Subscript => Node::new(Kind::Group),
                    Tag::Emphasis => Node::new(Kind::Emphasis),
                    Tag::Strong => Node::new(Kind::Strong),
                    Tag::Strikethrough => Node::new(Kind::Strikethrough),
                    Tag::Link { dest_url, .. } => Node::new(Kind::Link(dest_url.to_string())),
                    Tag::Image { dest_url, .. } => Node::new(Kind::Image(dest_url.to_string())),
                };
                self.stack.push(node);
            }
            Event::End(_) => {
                // 根节点不参与配对
                if self.stack.len() > 1 {
                    let node = self.stack.pop().expect("栈非空");
                    self.push(node);
                }
            }
            Event::Text(text) => self.text(text.to_string()),
            Event::Code(code) => self.push(Node::with_text(Kind::CodeSpan, code.to_string())),
            Event::Html(html) | Event::InlineHtml(html) => self.text(html.to_string()),
            Event::FootnoteReference(name) => {
                self.push(Node::with_text(Kind::FootnoteRef, name.to_string()))
            }
            // 数学公式没开开关，真出现时按行内代码原样显示
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push(Node::with_text(Kind::CodeSpan, text.to_string()))
            }
            Event::SoftBreak => self.push(Node::new(Kind::SoftBreak)),
            Event::HardBreak => self.push(Node::new(Kind::HardBreak)),
            Event::Rule => self.push(Node::new(Kind::Rule)),
            Event::TaskListMarker(done) => self.push(Node::new(Kind::Task(done))),
        }
    }

    fn finish(mut self) -> Node {
        while self.stack.len() > 1 {
            let node = self.stack.pop().expect("栈非空");
            self.push(node);
        }
        let mut root = self.stack.pop().expect("根节点存在");
        wrap_tight_items(&mut root);
        root
    }
}

fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn language(kind: CodeBlockKind) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(info) => info
            .split_whitespace()
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        CodeBlockKind::Indented => None,
    }
}

/// 紧凑列表（`- a` 后面直接换行）里 pulldown-cmark 不会发 Paragraph 事件，
/// 条目的行内内容会直接挂在 Item 下。这里把连续的���内节点重新包回段落，
/// 渲染时就只需要处理「块」这一种结构。
fn wrap_tight_items(node: &mut Node) {
    for child in &mut node.children {
        // 紧凑列表不发 Paragraph 事件，松散列表会发。先判断再包装
        if let Kind::List { tight, .. } = &mut child.kind {
            *tight = child
                .children
                .iter()
                .all(|item| !item.children.iter().any(|c| c.is(&Kind::Paragraph)));
        }
        if child.kind == Kind::Item {
            child.children = group_inline_runs(std::mem::take(&mut child.children));
        }
        wrap_tight_items(child);
    }
}

fn group_inline_runs(children: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    let mut run: Vec<Node> = Vec::new();
    for child in children {
        if is_block(&child.kind) {
            if !run.is_empty() {
                out.push(Node {
                    kind: Kind::Paragraph,
                    text: String::new(),
                    children: std::mem::take(&mut run),
                });
            }
            out.push(child);
        } else {
            run.push(child);
        }
    }
    if !run.is_empty() {
        out.push(Node {
            kind: Kind::Paragraph,
            text: String::new(),
            children: run,
        });
    }
    out
}

fn is_block(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Paragraph
            | Kind::Heading(_)
            | Kind::CodeBlock(_)
            | Kind::Quote
            | Kind::List { .. }
            | Kind::Item
            | Kind::Table(_)
            | Kind::TableHead
            | Kind::TableRow
            | Kind::TableCell(_)
            | Kind::Rule
            | Kind::Html
            | Kind::FootnoteDef
            | Kind::Group
    )
}

pub fn parse(src: &str) -> Node {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut builder = Builder::new();
    for ev in Parser::new_ext(src, options) {
        builder.event(ev);
    }
    builder.finish()
}

/// 按出现顺序收集文档里引用的所有图片，供 `--check-images` 报告。
pub fn image_urls(src: &str) -> Vec<String> {
    fn walk(node: &Node, out: &mut Vec<String>) {
        if let Kind::Image(url) = &node.kind {
            out.push(url.clone());
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(&parse(src), &mut out);
    out
}

/// 按出现顺序收集文档里所有 ```mermaid 代码块的源码，供 `--check-images` 报告。
pub fn mermaid_sources(src: &str) -> Vec<String> {
    fn walk(node: &Node, out: &mut Vec<String>) {
        if let Kind::CodeBlock(Some(lang)) = &node.kind
            && lang == MERMAID_LANG
        {
            out.push(node.text.clone());
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(&parse(src), &mut out);
    out
}

// ---------------------------------------------------------------- 渲染

/// 列表标记：写在父层前缀之后，并顶掉首行对应的缩进。
struct Lead {
    style: Style,
    text: String,
    strip: String,
}

/// 每一行开头都要写下的前缀（缩进、列表缩进、引用竖线）。
#[derive(Clone, Default)]
struct Cfg {
    prefix: String,
    /// 前缀本身的样式，引用竖线用暗色，缩进用默认色。
    style: Style,
    /// 紧凑列表：块之间不额外留空行。
    tight: bool,
    /// 表格单元格里不嵌图片：控制序列会撑破单元格。
    in_table: bool,
}

pub struct Markdown<'a, 'i> {
    hl: &'a Highlighter,
    color: bool,
    /// 脚注名字到编号的对应，渲染前按定义顺序填好。
    notes: HashMap<String, usize>,
    /// 有值时把图片渲染成终端图形协议的控制序列。
    images: Option<&'i ImageRenderer<'i>>,
    /// 有值时把 ```mermaid 代码块画成图。渲染器和它该用的尺寸上限。
    diagrams: Option<(&'i MermaidRenderer, Sizing)>,
}

impl<'a, 'i> Markdown<'a, 'i> {
    pub fn new(
        hl: &'a Highlighter,
        color: bool,
        images: Option<&'i ImageRenderer<'i>>,
        diagrams: Option<(&'i MermaidRenderer, Sizing)>,
    ) -> Self {
        Markdown {
            hl,
            color,
            notes: HashMap::new(),
            images,
            diagrams,
        }
    }

    pub fn render(&mut self, src: &str) -> String {
        let root = parse(src);
        self.notes.clear();
        let mut counter = 0;
        collect_footnotes(&root, &mut self.notes, &mut counter);

        let mut painter = Painter::new(TextBuf::new()).with_color(self.color);
        self.blocks(&mut painter, &root.children, &Cfg::default());
        let mut text = painter.into_text();
        // 结尾不留多余空行；没有内容时输出空串
        while text.ends_with("\n\n") {
            text.pop();
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    fn sub(&self) -> Painter<TextBuf> {
        Painter::new(TextBuf::new()).with_color(self.color)
    }

    /// mermaid 源码 → 终端图形协议的控制序列。
    ///
    /// 两步：先渲染成 PNG，再交给图片管线编码。和图片共用同一套
    /// 分块、尺寸和 id 计算，所以行为一致。任何一步走不通返回 `None`，
    /// 调用方退回显示源码。
    fn diagram(&self, source: &str) -> Option<String> {
        let (renderer, sizing) = self.diagrams?;
        let png = renderer.png(source, &sizing)?;
        self.images?
            .render_png(&crate::mermaid::identity(source), png)
    }

    /// 行首补上当前层的前缀。
    fn open(&self, p: &mut Painter<TextBuf>, cfg: &Cfg) {
        if p.at_line_start() && !cfg.prefix.is_empty() {
            p.write_prefix(cfg.style, &cfg.prefix);
        }
    }

    fn blocks(&self, p: &mut Painter<TextBuf>, nodes: &[Node], cfg: &Cfg) {
        for node in nodes {
            self.block(p, node, cfg, !cfg.tight);
        }
    }

    fn block(&self, p: &mut Painter<TextBuf>, node: &Node, cfg: &Cfg, spaced: bool) {
        match &node.kind {
            Kind::Paragraph => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                self.inline(p, &node.children, Style::new(), cfg);
                p.newline();
            }
            Kind::Heading(level) => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                self.inline(p, &node.children, heading_style(*level), cfg);
                // 宽度要在换行前读，newline() 会把列号清零
                let width = p.col().saturating_sub(display_width(&cfg.prefix));
                p.newline();
                // 一级标题下面补一条等宽的线
                if *level == 1 {
                    self.open(p, cfg);
                    p.write(S_RULE, &"─".repeat(width.max(3)));
                    p.newline();
                }
            }
            Kind::CodeBlock(lang) => {
                if spaced {
                    p.blank_line();
                }
                // mermaid 图画不出来时（协议不支持、终端不是终端、源码有语法错）
                // 退回显示源码，和图片退回文字占位是同一个策略。
                // 表格单元格里不嵌图：控制序列会撑破单元格。
                if lang.as_deref() == Some(MERMAID_LANG)
                    && !cfg.in_table
                    && let Some(sequence) = self.diagram(&node.text)
                {
                    self.open(p, cfg);
                    p.write_control(&sequence);
                    p.newline();
                    return;
                }
                // 认不出语言时保持原样，没必要把每个字符都涂成同一个颜色
                match lang.as_deref().and_then(|name| self.hl.by_name(name)) {
                    Some(syntax) => {
                        for line in self.hl.lines(&node.text, syntax) {
                            self.open(p, cfg);
                            p.write_prefix(Style::new(), CODE_INDENT);
                            for (style, text) in line {
                                p.write(style, &text);
                            }
                            p.newline();
                        }
                    }
                    None => {
                        for line in node.text.lines() {
                            self.open(p, cfg);
                            p.write_prefix(Style::new(), CODE_INDENT);
                            p.write(Style::new(), line);
                            p.newline();
                        }
                    }
                }
            }
            Kind::Quote => {
                if spaced {
                    p.blank_line();
                }
                // 前缀都是相对当前层的，父层前缀由 paste 写在每行最前面
                let inner = Cfg {
                    prefix: "│ ".to_string(),
                    style: S_BORDER,
                    tight: false,
                    in_table: false,
                };
                let mut sub = self.sub();
                self.blocks(&mut sub, &node.children, &inner);
                self.paste(p, sub.into_text(), cfg, None);
            }
            Kind::List {
                ordered,
                start,
                tight,
            } => {
                if spaced {
                    p.blank_line();
                }
                let tight = *tight;
                let mut first = true;
                for (i, item) in node.children.iter().enumerate() {
                    if !first && !tight {
                        p.blank_line();
                    }
                    first = false;
                    let marker = if *ordered {
                        format!("{}. ", start + i as u64)
                    } else {
                        "• ".to_string()
                    };
                    // 标记之后的续行缩进，宽度对齐标记本身
                    let inner = Cfg {
                        prefix: " ".repeat(display_width(&marker)),
                        style: Style::new(),
                        tight,
                        in_table: false,
                    };
                    let mut sub = self.sub();
                    self.blocks(&mut sub, &item.children, &inner);
                    let body = sub.into_text();
                    if body.trim().is_empty() {
                        p.blank_line();
                        self.open(p, cfg);
                        p.write(S_BULLET, &marker);
                        p.newline();
                    } else {
                        let lead = Lead {
                            style: S_BULLET,
                            text: marker,
                            // 首行的缩进由标记本身占掉
                            strip: inner.prefix,
                        };
                        self.paste(p, body, cfg, Some(lead));
                    }
                }
            }
            Kind::Table(_) => self.table(p, node, cfg, spaced),
            Kind::Rule => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                p.write(S_RULE, &"─".repeat(RULE_WIDTH));
                p.newline();
            }
            Kind::Html => {
                if spaced {
                    p.blank_line();
                }
                for line in node.text.lines() {
                    self.open(p, cfg);
                    p.write(Style::new(), line);
                    p.newline();
                }
            }
            Kind::FootnoteDef => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                p.write(S_RULE, &format!("[{}]", self.note_label(&node.text)));
                p.newline();
                let inner = Cfg {
                    prefix: "  ".to_string(),
                    style: Style::new(),
                    tight: false,
                    in_table: false,
                };
                self.blocks(p, &node.children, &inner);
            }
            Kind::Group => self.blocks(p, &node.children, cfg),
            _ => {}
        }
    }

    /// 把子渲染结果逐行贴回父渲染。
    ///
    /// 父层前缀写在每行最前面，`lead`（列表标记）紧跟其后、只出现在第一行。
    /// 空行不补前缀，避免出现拖尾的竖线或空格。
    fn paste(&self, p: &mut Painter<TextBuf>, text: String, cfg: &Cfg, lead: Option<Lead>) {
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            self.open(p, cfg);
            let first = match &lead {
                Some(lead) => first.strip_prefix(lead.strip.as_str()).unwrap_or(first),
                None => first,
            };
            if let Some(lead) = lead {
                p.write(lead.style, &lead.text);
            }
            p.write_raw(first);
            p.newline();
        }
        for line in lines {
            if line.trim().is_empty() {
                p.newline();
            } else {
                self.open(p, cfg);
                p.write_raw(line);
                p.newline();
            }
        }
    }

    fn table(&self, p: &mut Painter<TextBuf>, node: &Node, cfg: &Cfg, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let mut header: Vec<&Node> = Vec::new();
        let mut rows: Vec<Vec<&Node>> = Vec::new();
        for child in &node.children {
            match child.kind {
                Kind::TableHead => header = child.children.iter().collect(),
                Kind::TableRow => rows.push(child.children.iter().collect()),
                _ => {}
            }
        }

        // 先把每个单元格渲染出来，再按显示宽度对齐（CJK 算两列）
        let aligns: Vec<Align> = match &node.kind {
            Kind::Table(a) => a.clone(),
            _ => Vec::new(),
        };
        let head_text: Vec<String> = header
            .iter()
            .map(|c| self.cell(c, Style::new().bold()))
            .collect();
        let head_align: Vec<Align> = header.iter().map(|c| cell_align(c, &aligns)).collect();
        let body: Vec<(Vec<String>, Vec<Align>)> = rows
            .iter()
            .map(|row| {
                (
                    row.iter().map(|c| self.cell(c, Style::new())).collect(),
                    row.iter().map(|c| cell_align(c, &aligns)).collect(),
                )
            })
            .collect();

        let columns = head_text
            .len()
            .max(body.first().map_or(0, |(cells, _)| cells.len()));
        let mut widths = vec![0usize; columns];
        for (i, w) in widths.iter_mut().enumerate() {
            let mut width = head_text.get(i).map_or(0, |t| display_width(t));
            for (cells, _) in &body {
                if let Some(t) = cells.get(i) {
                    width = width.max(display_width(t));
                }
            }
            *w = width.max(1);
        }

        self.border(p, cfg, "┌", "┬", "┐", &widths);
        if !header.is_empty() {
            self.row(p, cfg, &head_text, &head_align, &widths);
            self.border(p, cfg, "├", "┼", "┤", &widths);
        }
        for (cells, aligns) in &body {
            self.row(p, cfg, cells, aligns, &widths);
        }
        self.border(p, cfg, "└", "┴", "┘", &widths);
    }

    fn cell(&self, cell: &Node, style: Style) -> String {
        let mut sub = self.sub();
        self.inline(
            &mut sub,
            &cell.children,
            style,
            &Cfg {
                in_table: true,
                ..Default::default()
            },
        );
        sub.into_text().trim_end().to_string()
    }

    fn border(
        &self,
        p: &mut Painter<TextBuf>,
        cfg: &Cfg,
        left: &str,
        mid: &str,
        right: &str,
        widths: &[usize],
    ) {
        self.open(p, cfg);
        let mut line = String::from(left);
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                line.push_str(mid);
            }
            line.push_str(&"─".repeat(w + 2));
        }
        line.push_str(right);
        p.write(S_BORDER, &line);
        p.newline();
    }

    fn row(
        &self,
        p: &mut Painter<TextBuf>,
        cfg: &Cfg,
        cells: &[String],
        aligns: &[Align],
        widths: &[usize],
    ) {
        self.open(p, cfg);
        p.write(S_BORDER, "│");
        for (i, w) in widths.iter().enumerate() {
            let align = aligns.get(i).copied().unwrap_or(Align::None);
            let text = pad(cells.get(i).map(String::as_str).unwrap_or(""), *w, align);
            // 单元格自己带样式（表头加粗），整体左移一格对齐
            p.write_raw(&text);
            p.write(S_BORDER, "│");
        }
        p.newline();
    }

    fn inline(&self, p: &mut Painter<TextBuf>, nodes: &[Node], style: Style, cfg: &Cfg) {
        for node in nodes {
            match &node.kind {
                Kind::Text => p.write(style, &node.text),
                Kind::CodeSpan => p.write(style.merge(S_CODE_SPAN), &node.text),
                Kind::Emphasis => {
                    self.inline(p, &node.children, style.merge(Style::new().italic()), cfg)
                }
                Kind::Strong => {
                    self.inline(p, &node.children, style.merge(Style::new().bold()), cfg)
                }
                Kind::Strikethrough => self.inline(
                    p,
                    &node.children,
                    style.merge(Style::new().dim().strike()),
                    cfg,
                ),
                Kind::Link(url) => {
                    self.inline(p, &node.children, style.merge(S_LINK), cfg);
                    let text = plain_text(&node.children);
                    if !url.is_empty() && text != *url {
                        p.write(style.merge(S_MUTED), &format!(" ({url})"));
                    }
                }
                Kind::Image(url) => {
                    // 终端能显示就直接画出来，否则退回文字占位
                    let drawn = if cfg.in_table {
                        None
                    } else {
                        self.images.and_then(|images| images.render(url))
                    };
                    if let Some(sequence) = drawn {
                        p.write_control(&sequence);
                        p.newline();
                        continue;
                    }
                    let alt = plain_text(&node.children);
                    p.write(style.merge(S_IMAGE), "[image] ");
                    if alt.is_empty() {
                        p.write(style.merge(S_MUTED), "无描述");
                    } else {
                        p.write(style.merge(S_LINK), &alt);
                    }
                    if !url.is_empty() && url != &alt {
                        p.write(style.merge(S_MUTED), &format!(" ({url})"));
                    }
                }
                Kind::SoftBreak => p.write(style, " "),
                Kind::HardBreak => {
                    p.write(style.merge(S_MUTED), "\\");
                    p.newline();
                    self.open(p, cfg);
                }
                Kind::Task(done) => {
                    let s = if *done {
                        Style::new().fg(C_TASK_DONE)
                    } else {
                        S_MUTED
                    };
                    p.write(style.merge(s), if *done { "☑ " } else { "☐ " });
                }
                Kind::FootnoteRef => p.write(
                    style.merge(S_MUTED),
                    &format!("[{}]", self.note_label(&node.text)),
                ),
                Kind::Html => p.write(style, &node.text),
                _ => {}
            }
        }
    }
}

fn collect_footnotes(node: &Node, out: &mut HashMap<String, usize>, counter: &mut usize) {
    if node.is(&Kind::FootnoteDef) {
        *counter += 1;
        out.insert(node.text.clone(), *counter);
    }
    for child in &node.children {
        collect_footnotes(child, out, counter);
    }
}

impl Markdown<'_, '_> {
    /// 脚注按定义顺序编号，找不到定义时退回原始名字。
    fn note_label(&self, name: &str) -> String {
        if name.is_empty() {
            "*".to_string()
        } else {
            match self.notes.get(name) {
                Some(n) => n.to_string(),
                None => name.to_string(),
            }
        }
    }
}

/// 去掉标记后的纯文本，用来判断链接地址是否值得另显。
fn plain_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match &node.kind {
            Kind::Text | Kind::CodeSpan => out.push_str(&node.text),
            Kind::SoftBreak => out.push(' '),
            Kind::Task(done) => out.push_str(if *done { "☑ " } else { "☐ " }),
            _ => out.push_str(&plain_text(&node.children)),
        }
    }
    out
}

/// 单元格内容按对齐方式补齐，两侧各留一格空格。
/// 宽度用的是显示宽度，所以中文也能对齐。
fn pad(text: &str, width: usize, align: Align) -> String {
    let fill = width.saturating_sub(display_width(text));
    let body = match align {
        Align::Center => {
            let left = fill / 2;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(fill - left))
        }
        Align::Right => format!("{}{}", " ".repeat(fill), text),
        _ => format!("{}{}", text, " ".repeat(fill)),
    };
    format!(" {body} ")
}

fn cell_align(node: &Node, aligns: &[Align]) -> Align {
    match node.kind {
        Kind::TableCell(col) => aligns.get(col).copied().unwrap_or(Align::None),
        _ => Align::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn highlighter() -> &'static Highlighter {
        static HL: OnceLock<Highlighter> = OnceLock::new();
        HL.get_or_init(|| Highlighter::new("base16-ocean.dark").expect("内置主题"))
    }

    /// 关掉颜色渲染成纯文本，方便直接比对排版。
    fn render(src: &str) -> String {
        Markdown::new(highlighter(), false, None, None).render(src)
    }

    #[test]
    fn 一级标题下面补等宽的线() {
        assert_eq!(render("# 你好\n"), "你好\n────\n");
    }

    #[test]
    fn 二级标题不补线() {
        assert_eq!(render("## 你好\n"), "你好\n");
    }

    #[test]
    fn 紧凑列表之间不留空行() {
        assert_eq!(render("- a\n- b\n- c\n"), "• a\n• b\n• c\n");
    }

    #[test]
    fn 松散列表之间有空行() {
        assert_eq!(render("- a\n\n- b\n"), "• a\n\n• b\n");
    }

    #[test]
    fn 续行缩进对齐标记宽度() {
        // 两行之间必须是硬换行，普通的换行会被合并成同一段
        let out = render("- 第一行  \n  第二行\n");
        assert_eq!(out, "• 第一行\\\n  第二行\n");
    }

    #[test]
    fn 有序列表从给定编号开始() {
        assert_eq!(render("3. a\n4. b\n"), "3. a\n4. b\n");
    }

    #[test]
    fn 嵌套列表按层级缩进() {
        assert_eq!(render("- a\n    - b\n"), "• a\n  • b\n");
    }

    #[test]
    fn 引用每行都带竖线() {
        // 引用里的两个段落之间是松散的，留一个空行，但空行不画竖线
        assert_eq!(render("> 引用\n>\n> 第二段\n"), "│ 引用\n\n│ 第二段\n");
    }

    #[test]
    fn 引用里的列表标记在竖线之后() {
        assert_eq!(render("> - a\n> - b\n"), "│ • a\n│ • b\n");
    }

    #[test]
    fn 代码块缩进两格并保留块内空行() {
        assert_eq!(render("```\na\n\nb\n```\n"), "  a\n  \n  b\n");
    }

    #[test]
    fn 任务列表打勾() {
        assert_eq!(render("- [x] 做完\n- [ ] 没做\n"), "• ☑ 做完\n• ☐ 没做\n");
    }

    #[test]
    fn 收集文档里的_mermaid_源码() {
        let src = "\
```mermaid
flowchart TD
  A --> B
```

普通代码块不算：

```
echo hi
```

再来一张：

```mermaid
sequenceDiagram
  A->>B: hi
```
";
        let found = mermaid_sources(src);
        assert_eq!(found.len(), 2);
        assert!(found[0].contains("flowchart TD"));
        assert!(found[1].contains("sequenceDiagram"));
    }

    /// 表格单元格里的 mermaid 不能变成图：控制序列会撑破单元格。
    /// （表格里的代码块在 CommonMark 里是行内代码，本来就拿不到代码块节点，
    /// 这条测试守住的是「万一以后解析变了也不会漏出控制序列」。）
    #[test]
    fn 表格里不嵌_mermaid_图() {
        let src = "| 图 |\n|---|\n| `mermaid` |\n";
        let out = Markdown::new(highlighter(), false, None, None).render(src);
        assert!(!out.contains('\u{1b}'), "表格里漏出了控制序列：{out:?}");
    }

    /// mermaid 画不出来时必须退回源码，不能把内容吞掉。
    #[test]
    fn 没有渲染器时_mermaid_退回源码() {
        let src = "```mermaid\nflowchart TD\n  A --> B\n```\n";
        let out = Markdown::new(highlighter(), false, None, None).render(src);
        assert!(out.contains("flowchart TD"), "{out:?}");
        assert!(out.contains("A --> B"), "{out:?}");
        assert!(
            !out.contains('\u{1b}'),
            "没有图片管线就不该有控制序列：{out:?}"
        );
    }

    #[test]
    fn 表格按显示宽度对齐且尊重对齐方式() {
        let out = render("| 名字 | 值 |\n|---|---:|\n| 中文 | 1 |\n");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "┌──────┬────┐");
        assert_eq!(lines[1], "│ 名字 │ 值 │");
        assert_eq!(lines[3], "│ 中文 │  1 │");
        assert_eq!(lines[4], "└──────┴────┘");
    }

    #[test]
    fn 链接地址不同于文字时补在后面() {
        assert_eq!(render("[文字](http://a)\n"), "文字 (http://a)\n");
    }

    #[test]
    fn 链接文字就是地址时不重复显示() {
        assert_eq!(render("<http://a>\n"), "http://a\n");
    }

    #[test]
    fn 硬换行写成反斜杠() {
        assert_eq!(render("a  \nb\n"), "a\\\nb\n");
    }

    #[test]
    fn 脚注按出现顺序编号() {
        assert_eq!(
            render("正文[^a]\n\n[^a]: 说明\n"),
            "正文[1]\n\n[1]\n\n  说明\n"
        );
    }

    #[test]
    fn 块之间恰好一个空行() {
        assert_eq!(
            render("段落\n\n## 标题\n\n段落\n"),
            "段落\n\n标题\n\n段落\n"
        );
    }

    #[test]
    fn 空文档输出空串() {
        assert_eq!(render(""), "");
        assert_eq!(render("\n\n\n"), "");
    }

    #[test]
    fn 开启颜色时确实写出转义序列() {
        assert!(
            Markdown::new(highlighter(), true, None, None)
                .render("# 标题\n")
                .contains('\x1b')
        );
    }
}
