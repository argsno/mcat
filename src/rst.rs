//! reStructuredText 解析与终端渲染。
//!
//! 解析用 lsdocs-rst（docutils 的 Rust 移植），拿到的是一棵和 docutils
//! 一样的 doctree，再递归渲染成带 ANSI 样式的文本。它和 Markdown 的事件流
//! 模型不同，所以这里单独渲染，只复用 `markdown` 的配色和 `table` 的排版。

use crate::highlight::{Highlighter, write_code_prefix};
use crate::image::{ImageOutput, Renderer as ImageRenderer, Sizing};
use crate::markdown::{
    MERMAID_LANG, RULE_WIDTH, S_BORDER, S_BULLET, S_CODE_SPAN, S_IMAGE, S_LINK, S_MUTED, S_RULE,
    heading_style,
};
use crate::mermaid::Renderer as MermaidRenderer;
use crate::style::{Color, Painter, Style, TextBuf, display_width, wrap_styled};
use crate::table::{Align, available, fit_widths, pad};
use lsdocs_rst::{
    AttrValue, DirectiveError, Element, Kind, Node, Parser, Role, RoleContext, Settings, Text,
};
use std::path::{Path, PathBuf};

const S_LABEL: Style = Style::new().bold().fg(Color::Ansi(6));
const S_CAPTION: Style = Style::new().italic().fg(Color::Ansi(8));

pub struct Rst<'a, 'i> {
    hl: &'a Highlighter,
    color: bool,
    images: Option<&'i ImageRenderer<'i>>,
    /// 独占一段的图片用这份预算，留空时退回 `images`。
    block_images: Option<&'i ImageRenderer<'i>>,
    diagrams: Option<(&'i MermaidRenderer, Sizing)>,
    width: Option<usize>,
    /// 源文件路径。给了就允许 `.. include::` 按它所在目录读文件。
    source: Option<PathBuf>,
}

impl<'a, 'i> Rst<'a, 'i> {
    pub fn new(
        hl: &'a Highlighter,
        color: bool,
        images: Option<&'i ImageRenderer<'i>>,
        diagrams: Option<(&'i MermaidRenderer, Sizing)>,
    ) -> Self {
        Rst {
            hl,
            color,
            images,
            block_images: None,
            diagrams,
            width: None,
            source: None,
        }
    }

    /// 独占一段的图片（图、Figure、只有一张图的段落）用单独的高度预算。
    pub fn with_block_images(mut self, images: Option<&'i ImageRenderer<'i>>) -> Self {
        self.block_images = images;
        self
    }

    /// 记下源文件路径：`include`、`csv-table :file:` 之类的指令要按它
    /// 所在目录找文件。标准输入没有路径，相关指令保持禁用。
    pub fn with_source(mut self, source: impl Into<PathBuf>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 终端宽度：放不下的表格会折行。管道里给 `None`，不折行。
    pub fn with_width(mut self, width: Option<usize>) -> Self {
        self.width = width;
        self
    }

    pub fn render(&mut self, src: &str) -> String {
        let doc = parse(src, self.source.as_deref());
        let mut painter = Painter::new(TextBuf::new()).with_color(self.color);
        let has_title = doc.elements().any(|e| e.kind == Kind::Title);
        let env = Env {
            has_title,
            depth: 0,
        };
        let cfg = Cfg::default();
        self.document(&mut painter, &doc, &cfg, &env);
        let mut text = painter.into_text();
        while text.ends_with("\n\n") {
            text.pop();
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    fn document(&self, p: &mut Painter<TextBuf>, doc: &Element, cfg: &Cfg, env: &Env) {
        let mut children = doc.elements().peekable();
        while let Some(e) = children.next() {
            match e.kind {
                Kind::Title => {
                    self.heading(p, e, 1, cfg);
                    // 文档标题和副标题紧挨着，中间不插空行
                    if let Some(sub) = children.peek()
                        && sub.kind == Kind::Subtitle
                    {
                        let sub = children.next().expect("peek 过");
                        self.heading(p, sub, 2, cfg);
                    }
                }
                Kind::Subtitle => self.heading(p, e, 2, cfg),
                // 页眉、页脚、元数据是给发布器用的，不是正文
                Kind::Decoration | Kind::Header | Kind::Footer | Kind::Meta => {}
                _ => self.block(p, e, cfg, env, true),
            }
        }
    }

    fn sub(&self) -> Painter<TextBuf> {
        Painter::new(TextBuf::new()).with_color(self.color)
    }

    fn diagram(&self, source: &str) -> Option<ImageOutput> {
        let (renderer, sizing) = self.diagrams?;
        let png = renderer.png(source, &sizing)?;
        self.images?
            .render_png(&crate::mermaid::identity(source), png)
    }

    fn open(&self, p: &mut Painter<TextBuf>, cfg: &Cfg) {
        if p.at_line_start() && !cfg.prefix.is_empty() {
            p.write_prefix(cfg.style, &cfg.prefix);
        }
    }

    fn blocks(&self, p: &mut Painter<TextBuf>, nodes: &[Node], cfg: &Cfg, env: &Env, tight: bool) {
        for node in nodes {
            if let Node::Element(e) = node {
                self.block(p, e, cfg, env, !tight);
            } else if let Node::Text(t) = node {
                // 兜底：不在段落里的裸文本也原样写出来
                self.open(p, cfg);
                p.write(Style::new(), &oneline(&t.value));
                p.newline();
            }
        }
    }

    fn block(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        match &e.kind {
            Kind::Paragraph => {
                if spaced {
                    p.blank_line();
                }
                if let Some(image) = only_image(&e.children) {
                    if !self.image(p, image, cfg.in_table, true) {
                        p.newline();
                    }
                    return;
                }
                self.open(p, cfg);
                self.inline(p, &e.children, Style::new(), cfg);
                p.newline();
            }
            Kind::Section => self.section(p, e, cfg, env, spaced),
            Kind::Title => {
                if spaced {
                    p.blank_line();
                }
                self.heading(p, e, 2, cfg);
            }
            Kind::Subtitle => {
                if spaced {
                    p.blank_line();
                }
                self.heading(p, e, 3, cfg);
            }
            Kind::BulletList | Kind::EnumeratedList => self.list(p, e, cfg, env, spaced),
            Kind::DefinitionList => self.definition_list(p, e, cfg, env, spaced),
            Kind::FieldList => self.field_list(p, e, cfg, env, spaced),
            Kind::OptionList => self.option_list(p, e, cfg, env, spaced),
            Kind::BlockQuote => self.quote(p, e, cfg, env, spaced),
            Kind::LineBlock => self.line_block(p, e, cfg, spaced, 0),
            Kind::LiteralBlock | Kind::DoctestBlock | Kind::MathBlock => {
                self.code_block(p, e, cfg, spaced)
            }
            Kind::Table => self.table(p, e, cfg, env, spaced),
            Kind::Attention
            | Kind::Caution
            | Kind::Danger
            | Kind::Error
            | Kind::Important
            | Kind::Note
            | Kind::Tip
            | Kind::Hint
            | Kind::Warning
            | Kind::Admonition => self.admonition(p, e, cfg, env, spaced),
            Kind::Figure => self.figure(p, e, cfg, env, spaced),
            Kind::Image => {
                if spaced {
                    p.blank_line();
                }
                if !self.image(p, e, cfg.in_table, true) {
                    p.newline();
                }
            }
            Kind::Topic | Kind::Sidebar => self.topic(p, e, cfg, env, spaced),
            Kind::Attribution => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                p.write(S_MUTED, "— ");
                self.inline(p, &e.children, S_MUTED, cfg);
                p.newline();
            }
            Kind::Rubric => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                self.inline(p, &e.children, heading_style(4), cfg);
                p.newline();
            }
            Kind::Transition => {
                if spaced {
                    p.blank_line();
                }
                self.open(p, cfg);
                p.write(S_RULE, &"─".repeat(RULE_WIDTH));
                p.newline();
            }
            Kind::Footnote | Kind::Citation => self.note(p, e, cfg, env, spaced),
            Kind::Docinfo => self.docinfo(p, e, cfg, env, spaced),
            Kind::Container | Kind::Compound => {
                if spaced {
                    p.blank_line();
                }
                self.blocks(p, &e.children, cfg, env, false);
            }
            Kind::Other(name) if name == "directive" => {
                self.unknown_directive(p, e, cfg, env, spaced)
            }
            Kind::SystemMessage => {
                // 解析器把没能处理的原文放进 literal_block（比如被禁用的
                // include、写坏的表格），这部分必须留下，消息文字不要。
                if let Some(literal) = find_child(e, Kind::LiteralBlock) {
                    self.code_block(p, literal, cfg, spaced);
                }
            }
            // 注释、链接目标、替换定义、raw、pending 都不进正文
            Kind::Comment
            | Kind::Target
            | Kind::SubstitutionDefinition
            | Kind::Raw
            | Kind::Pending
            | Kind::Decoration
            | Kind::Header
            | Kind::Footer
            | Kind::Meta => {}
            _ => self.fallback_block(p, e, cfg, env, spaced),
        }
    }

    fn section(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        // docutils 的 HTML 输出里，文档标题占 h1，一级章节从 h2 开始
        let base = if env.has_title { 2 } else { 1 };
        let level = (env.depth + base).min(6);
        let inner = Env {
            depth: env.depth + 1,
            ..*env
        };
        for child in e.elements() {
            if child.kind == Kind::Title {
                self.heading(p, child, level, cfg);
            } else {
                self.block(p, child, cfg, &inner, true);
            }
        }
    }

    fn heading(&self, p: &mut Painter<TextBuf>, e: &Element, level: u8, cfg: &Cfg) {
        self.open(p, cfg);
        self.inline(p, &e.children, heading_style(level), cfg);
        let width = p.col().saturating_sub(display_width(&cfg.prefix));
        p.newline();
        if level == 1 {
            self.open(p, cfg);
            p.write(S_RULE, &"─".repeat(width.max(3)));
            p.newline();
        }
    }

    fn list(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let ordered = e.kind == Kind::EnumeratedList;
        let items: Vec<&Element> = e
            .elements()
            .filter(|child| child.kind == Kind::ListItem)
            .collect();
        // docutils 的树里不保留空行，按每个条目只有一块来判断紧凑列表
        let tight = items.iter().all(|item| item.elements().count() <= 1);
        for (i, item) in items.iter().enumerate() {
            if i > 0 && !tight {
                p.blank_line();
            }
            let marker = if ordered {
                format!("{} ", enum_label(e, i))
            } else {
                "• ".to_string()
            };
            let inner = Cfg {
                prefix: " ".repeat(display_width(&marker)),
                style: Style::new(),
                in_table: false,
            };
            let mut sub = self.sub();
            self.blocks(&mut sub, &item.children, &inner, env, tight);
            let body = sub.into_text();
            if body.trim().is_empty() {
                self.open(p, cfg);
                p.write(S_BULLET, &marker);
                p.newline();
            } else {
                self.paste(
                    p,
                    body,
                    cfg,
                    Some(Lead {
                        style: S_BULLET,
                        text: marker,
                        strip: inner.prefix,
                    }),
                );
            }
        }
    }

    fn definition_list(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        env: &Env,
        spaced: bool,
    ) {
        if spaced {
            p.blank_line();
        }
        for (i, item) in e
            .elements()
            .filter(|child| child.kind == Kind::DefinitionListItem)
            .enumerate()
        {
            if i > 0 {
                p.blank_line();
            }
            if let Some(term) = find_child(item, Kind::Term) {
                self.open(p, cfg);
                self.inline(p, &term.children, Style::new().bold(), cfg);
                for classifier in item.elements().filter(|c| c.kind == Kind::Classifier) {
                    p.write(S_MUTED, " : ");
                    self.inline(p, &classifier.children, S_MUTED, cfg);
                }
                p.newline();
            }
            let mut sub = self.sub();
            let inner = Cfg {
                prefix: "  ".to_string(),
                style: Style::new(),
                in_table: false,
            };
            for definition in item.elements().filter(|c| c.kind == Kind::Definition) {
                self.blocks(&mut sub, &definition.children, &inner, env, false);
            }
            let body = sub.into_text();
            if !body.trim().is_empty() {
                self.paste(p, body, cfg, None);
            }
        }
    }

    fn field_list(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        env: &Env,
        spaced: bool,
    ) {
        if spaced {
            p.blank_line();
        }
        for field in e.elements().filter(|c| c.kind == Kind::Field) {
            self.field(p, field, cfg, env);
        }
    }

    fn field(&self, p: &mut Painter<TextBuf>, field: &Element, cfg: &Cfg, env: &Env) {
        let name = find_child(field, Kind::FieldName);
        let body = find_child(field, Kind::FieldBody);
        self.open(p, cfg);
        if let Some(name) = name {
            self.inline(p, &name.children, Style::new().bold(), cfg);
        }
        p.write(Style::new(), ":");
        if let Some(body) = body {
            let single_paragraph = body.elements().count() == 1
                && body.elements().all(|child| child.kind == Kind::Paragraph);
            if single_paragraph {
                p.write(Style::new(), " ");
                for child in body.elements() {
                    self.inline(p, &child.children, Style::new(), cfg);
                }
                p.newline();
            } else {
                p.newline();
                let inner = Cfg {
                    prefix: "  ".to_string(),
                    style: Style::new(),
                    in_table: false,
                };
                let mut sub = self.sub();
                self.blocks(&mut sub, &body.children, &inner, env, false);
                self.paste(p, sub.into_text(), cfg, None);
            }
        } else {
            p.newline();
        }
    }

    fn option_list(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        env: &Env,
        spaced: bool,
    ) {
        if spaced {
            p.blank_line();
        }
        for item in e.elements().filter(|c| c.kind == Kind::OptionListItem) {
            let mut text = String::new();
            if let Some(group) = find_child(item, Kind::OptionGroup) {
                for option in group.elements().filter(|c| c.kind == Kind::Option) {
                    let mut name = String::new();
                    for string in option.elements().filter(|c| c.kind == Kind::OptionString) {
                        name.push_str(&string.astext());
                    }
                    for arg in option.elements().filter(|c| c.kind == Kind::OptionArgument) {
                        let delimiter = arg
                            .attributes
                            .get_str("delimiter")
                            .unwrap_or(" ")
                            .to_string();
                        name.push_str(&delimiter);
                        name.push_str(&arg.astext());
                    }
                    if !text.is_empty() {
                        text.push_str(", ");
                    }
                    text.push_str(&name);
                }
            }
            self.open(p, cfg);
            p.write(S_LABEL, &text);
            p.newline();
            if let Some(desc) = find_child(item, Kind::Description) {
                let inner = Cfg {
                    prefix: "  ".to_string(),
                    style: Style::new(),
                    in_table: false,
                };
                let mut sub = self.sub();
                self.blocks(&mut sub, &desc.children, &inner, env, false);
                self.paste(p, sub.into_text(), cfg, None);
            }
        }
    }

    fn quote(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let inner = Cfg {
            prefix: "│ ".to_string(),
            style: S_BORDER,
            in_table: false,
        };
        let mut sub = self.sub();
        self.blocks(&mut sub, &e.children, &inner, env, false);
        self.paste(p, sub.into_text(), cfg, None);
    }

    fn line_block(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        spaced: bool,
        depth: usize,
    ) {
        if spaced {
            p.blank_line();
        }
        // 行块是一行一行的文本，嵌套只是多缩进两格。直接写进当前
        // painter，不套子 painter，父层前缀才不会叠两遍。
        for child in e.elements() {
            match child.kind {
                Kind::Line => {
                    self.open(p, cfg);
                    p.write(Style::new(), &"  ".repeat(depth));
                    self.inline(p, &child.children, Style::new(), cfg);
                    p.newline();
                }
                Kind::LineBlock => self.line_block(p, child, cfg, false, depth + 1),
                _ => {}
            }
        }
    }

    fn code_block(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let text = e.astext();
        let lang = literal_language(e);
        if lang.as_deref() == Some(MERMAID_LANG)
            && !cfg.in_table
            && let Some(output) = self.diagram(&text)
        {
            p.write_control(&output.sequence);
            for _ in 0..output.advance {
                p.newline();
            }
            return;
        }
        let stripe = self.hl.stripe();
        match lang.as_deref().and_then(|name| self.hl.by_name(name)) {
            Some(syntax) => {
                for line in self.hl.lines(&text, syntax) {
                    self.open(p, cfg);
                    write_code_prefix(p, stripe);
                    for (style, part) in line {
                        p.write(style, &part);
                    }
                    p.newline();
                }
            }
            None => {
                for line in text.lines() {
                    self.open(p, cfg);
                    write_code_prefix(p, stripe);
                    p.write(Style::new(), line);
                    p.newline();
                }
            }
        }
    }

    fn admonition(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        env: &Env,
        spaced: bool,
    ) {
        if spaced {
            p.blank_line();
        }
        let title = find_child(e, Kind::Title);
        let label = title
            .map(|title| oneline(&title.astext()))
            .unwrap_or_else(|| default_admonition_title(&e.kind).to_string());
        self.open(p, cfg);
        p.write(S_LABEL, &label);
        p.newline();
        let inner = Cfg {
            prefix: "  ".to_string(),
            style: Style::new(),
            in_table: false,
        };
        let mut sub = self.sub();
        for child in e.elements().filter(|child| child.kind != Kind::Title) {
            self.block(&mut sub, child, &inner, env, true);
        }
        self.paste(p, sub.into_text(), cfg, None);
    }

    fn topic(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let mut title_done = false;
        for child in e.elements() {
            match child.kind {
                Kind::Title => {
                    self.open(p, cfg);
                    self.inline(p, &child.children, heading_style(2), cfg);
                    p.newline();
                    title_done = true;
                }
                Kind::Subtitle => {
                    self.open(p, cfg);
                    self.inline(p, &child.children, S_CAPTION, cfg);
                    p.newline();
                }
                _ => {
                    if title_done {
                        self.block(p, child, cfg, env, true);
                    } else {
                        self.block(p, child, cfg, env, false);
                    }
                }
            }
        }
    }

    fn figure(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        if let Some(image) = find_child(e, Kind::Image)
            && !self.image(p, image, cfg.in_table, true)
        {
            p.newline();
        }
        if let Some(caption) = find_child(e, Kind::Caption) {
            self.open(p, cfg);
            self.inline(p, &caption.children, S_CAPTION, cfg);
            p.newline();
        }
        for child in e
            .elements()
            .filter(|child| child.kind != Kind::Image && child.kind != Kind::Caption)
        {
            self.block(p, child, cfg, env, true);
        }
    }

    fn note(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let label = find_child(e, Kind::Label)
            .map(|label| oneline(&label.astext()))
            .unwrap_or_default();
        self.open(p, cfg);
        p.write(S_MUTED, &format!("[{label}]"));
        p.newline();
        let inner = Cfg {
            prefix: "  ".to_string(),
            style: Style::new(),
            in_table: false,
        };
        let mut sub = self.sub();
        for child in e.elements().filter(|child| child.kind != Kind::Label) {
            self.block(&mut sub, child, &inner, env, true);
        }
        self.paste(p, sub.into_text(), cfg, None);
    }

    fn docinfo(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        for child in e.elements() {
            if child.kind == Kind::Field {
                self.field(p, child, cfg, env);
                continue;
            }
            let label = docinfo_label(&child.kind);
            self.open(p, cfg);
            p.write(S_MUTED, label);
            p.write(Style::new(), ": ");
            self.inline(p, &child.children, Style::new(), cfg);
            p.newline();
        }
    }

    fn fallback_block(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        env: &Env,
        spaced: bool,
    ) {
        if e.elements().any(|child| is_block_kind(&child.kind)) {
            if spaced {
                p.blank_line();
            }
            self.blocks(p, &e.children, cfg, env, false);
        } else {
            if spaced {
                p.blank_line();
            }
            self.open(p, cfg);
            self.inline(p, &e.children, Style::new(), cfg);
            p.newline();
        }
    }

    fn unknown_directive(
        &self,
        p: &mut Painter<TextBuf>,
        e: &Element,
        cfg: &Cfg,
        env: &Env,
        spaced: bool,
    ) {
        if spaced {
            p.blank_line();
        }
        let name = e.attributes.get_str("name").unwrap_or("directive");
        let argument = e.attributes.get_str("argument").unwrap_or_default();
        self.open(p, cfg);
        p.write(S_MUTED, &format!(".. {name}::"));
        if !argument.is_empty() {
            p.write(S_MUTED, " ");
            p.write(S_MUTED, &oneline(argument));
        }
        p.newline();
        self.blocks(p, &e.children, cfg, env, false);
    }

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

    fn inline(&self, p: &mut Painter<TextBuf>, nodes: &[Node], style: Style, cfg: &Cfg) {
        for node in nodes {
            match node {
                Node::Text(t) => p.write(style, &oneline(&t.value)),
                Node::Element(e) => self.inline_element(p, e, style, cfg),
            }
        }
    }

    fn inline_element(&self, p: &mut Painter<TextBuf>, e: &Element, style: Style, cfg: &Cfg) {
        match &e.kind {
            Kind::Emphasis => self.inline(p, &e.children, style.merge(Style::new().italic()), cfg),
            Kind::Strong => self.inline(p, &e.children, style.merge(Style::new().bold()), cfg),
            Kind::Literal => p.write(style.merge(S_CODE_SPAN), &oneline(&e.astext())),
            Kind::Math => p.write(style.merge(S_CODE_SPAN), &oneline(&e.astext())),
            Kind::TitleReference => {
                self.inline(p, &e.children, style.merge(Style::new().italic()), cfg)
            }
            Kind::Reference => {
                self.inline(p, &e.children, style.merge(S_LINK), cfg);
                if let Some(uri) = e.attributes.get_str("refuri")
                    && !uri.is_empty()
                    && oneline(&e.astext()) != uri
                {
                    p.write(style.merge(S_MUTED), &format!(" ({uri})"));
                }
            }
            Kind::FootnoteReference | Kind::CitationReference => {
                p.write(style.merge(S_MUTED), &format!("[{}]", oneline(&e.astext())));
            }
            Kind::SubstitutionReference => {
                let name = e
                    .attributes
                    .get_list("refname")
                    .first()
                    .cloned()
                    .unwrap_or_else(|| oneline(&e.astext()));
                p.write(style.merge(S_CODE_SPAN), &format!("|{name}|"));
            }
            Kind::Superscript => {
                p.write(style.merge(S_MUTED), "^");
                self.inline(p, &e.children, style, cfg);
            }
            Kind::Subscript => {
                p.write(style.merge(S_MUTED), "_");
                self.inline(p, &e.children, style, cfg);
            }
            Kind::Abbreviation | Kind::Acronym | Kind::Inline | Kind::Generated => {
                self.inline(p, &e.children, style, cfg)
            }
            Kind::Image => {
                self.image(p, e, cfg.in_table, false);
            }
            Kind::Target | Kind::Raw => {}
            Kind::Problematic => p.write(style, &oneline(&e.astext())),
            _ => self.inline(p, &e.children, style, cfg),
        }
    }

    /// 画一张图。返回 true 表示已经写出图形协议的控制序列（并换行），
    /// false 表示只写了文字占位，换行由调用方决定。
    fn image(&self, p: &mut Painter<TextBuf>, e: &Element, in_table: bool, block: bool) -> bool {
        let uri = e.attributes.get_str("uri").unwrap_or_default();
        let alt = e.attributes.get_str("alt").unwrap_or_default();
        let renderer = if block {
            self.block_images.or(self.images)
        } else {
            self.images
        };
        if !in_table
            && let Some(images) = renderer
            && let Some(output) = images.render(uri)
        {
            p.write_control(&output.sequence);
            for _ in 0..output.advance {
                p.newline();
            }
            return true;
        }
        p.write(S_IMAGE, "[image] ");
        if alt.is_empty() {
            p.write(S_MUTED, "无描述");
        } else {
            p.write(S_LINK, alt);
        }
        if !uri.is_empty() && uri != alt {
            p.write(S_MUTED, &format!(" ({uri})"));
        }
        false
    }

    fn table(&self, p: &mut Painter<TextBuf>, e: &Element, cfg: &Cfg, env: &Env, spaced: bool) {
        if spaced {
            p.blank_line();
        }
        let Some(tgroup) = find_child(e, Kind::Tgroup) else {
            return;
        };
        let head: Vec<&Element> = find_child(tgroup, Kind::Thead)
            .map(|head| head.elements().filter(|r| r.kind == Kind::Row).collect())
            .unwrap_or_default();
        let body: Vec<&Element> = find_child(tgroup, Kind::Tbody)
            .map(|body| body.elements().filter(|r| r.kind == Kind::Row).collect())
            .unwrap_or_default();
        let rows: Vec<&Element> = head.iter().chain(&body).copied().collect();
        if rows.is_empty() {
            return;
        }

        let cols = attr_usize(tgroup, "cols")
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                rows.iter()
                    .map(|row| row.elements().filter(|e| e.kind == Kind::Entry).count())
                    .max()
                    .unwrap_or(0)
            });
        if cols == 0 {
            return;
        }
        let aligns: Vec<Align> = tgroup
            .elements()
            .filter(|c| c.kind == Kind::Colspec)
            .map(|c| {
                c.attributes
                    .get_str("align")
                    .map(parse_align)
                    .unwrap_or_default()
            })
            .collect();

        let mut cells: Vec<Cell> = Vec::new();
        let mut placements = vec![vec![None; cols]; rows.len()];
        let mut row_cells: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
        for (r, row) in rows.iter().enumerate() {
            let mut col = 0;
            for entry in row.elements().filter(|e| e.kind == Kind::Entry) {
                while col < cols && placements[r][col].is_some() {
                    col += 1;
                }
                if col >= cols {
                    break;
                }
                let colspan = attr_usize(entry, "morecols").unwrap_or(0) + 1;
                let rowspan = attr_usize(entry, "morerows").unwrap_or(0) + 1;
                let span = colspan.min(cols - col);
                let rows_span = rowspan.min(rows.len() - r);
                let id = cells.len();
                let align = entry
                    .attributes
                    .get_str("align")
                    .map(parse_align)
                    .unwrap_or_else(|| aligns.get(col).copied().unwrap_or_default());
                cells.push(Cell {
                    lines: self.cell_lines(entry, env),
                    colspan: span,
                    align,
                    start: (r, col),
                });
                for row in &mut placements[r..r + rows_span] {
                    for slot in &mut row[col..col + span] {
                        *slot = Some(id);
                    }
                }
                row_cells[r].push(id);
                col += span;
            }
        }

        let slots: Vec<Vec<Slot>> = (0..rows.len())
            .map(|r| {
                let mut slots = Vec::new();
                let mut c = 0;
                while c < cols {
                    match placements[r][c] {
                        Some(id) if cells[id].start == (r, c) => {
                            let span = cells[id].colspan;
                            slots.push(Slot::Cell { id, start: c, span });
                            c += span;
                        }
                        Some(id) => {
                            let mut span = 1;
                            while c + span < cols && placements[r][c + span] == Some(id) {
                                span += 1;
                            }
                            slots.push(Slot::Covered { start: c, span });
                            c += span;
                        }
                        None => {
                            slots.push(Slot::Covered { start: c, span: 1 });
                            c += 1;
                        }
                    }
                }
                slots
            })
            .collect();

        let mut widths = vec![1usize; cols];
        for cell in &cells {
            let width = cell
                .lines
                .iter()
                .map(|line| display_width(line))
                .max()
                .unwrap_or(0);
            if cell.colspan == 1 {
                widths[cell.start.1] = widths[cell.start.1].max(width);
            }
        }
        for cell in &cells {
            if cell.colspan > 1 {
                let have = combined_width(&widths, cell.start.1, cell.colspan);
                let need = cell
                    .lines
                    .iter()
                    .map(|line| display_width(line))
                    .max()
                    .unwrap_or(0);
                if need > have {
                    widths[cell.start.1 + cell.colspan - 1] += need - have;
                }
            }
        }
        let widths = fit_widths(
            &widths,
            available(self.width, display_width(&cfg.prefix), cols),
        );
        for cell in &mut cells {
            let width = combined_width(&widths, cell.start.1, cell.colspan);
            cell.lines = cell
                .lines
                .iter()
                .flat_map(|line| wrap_styled(line, width))
                .collect();
        }

        if let Some(title) = find_child(e, Kind::Title) {
            self.open(p, cfg);
            self.inline(p, &title.children, S_CAPTION, cfg);
            p.newline();
        }

        self.table_edge(p, cfg, &widths, &placements, 0, true);
        for (r, row_slots) in slots.iter().enumerate() {
            self.table_row(p, cfg, &widths, &cells, row_slots);
            if r + 1 < rows.len() {
                self.table_separator(p, cfg, &widths, &placements, r);
            }
        }
        self.table_edge(p, cfg, &widths, &placements, rows.len() - 1, false);
    }

    fn cell_lines(&self, entry: &Element, env: &Env) -> Vec<String> {
        let mut sub = self.sub();
        let cfg = Cfg {
            prefix: String::new(),
            style: Style::new(),
            in_table: true,
        };
        self.blocks(&mut sub, &entry.children, &cfg, env, false);
        let mut lines: Vec<String> = sub
            .into_text()
            .split('\n')
            .map(|line| line.trim_end().to_string())
            .collect();
        while lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    /// 顶边或底边。表头/表尾那一行如果有跨列单元格，内部就不该有
    /// `┬`/`┴`——那里并没有竖线可以接上去。
    fn table_edge(
        &self,
        p: &mut Painter<TextBuf>,
        cfg: &Cfg,
        widths: &[usize],
        placements: &[Vec<Option<usize>>],
        row: usize,
        top: bool,
    ) {
        self.open(p, cfg);
        let mut line = String::from(if top { "┌" } else { "└" });
        for (c, width) in widths.iter().enumerate() {
            line.push_str(&"─".repeat(width + 2));
            if c + 1 < widths.len() {
                let vertical = placements[row][c] != placements[row][c + 1];
                line.push_str(junction(!top && vertical, top && vertical, true, true));
            }
        }
        line.push_str(if top { "┐" } else { "┘" });
        p.write(S_BORDER, &line);
        p.newline();
    }

    fn table_row(
        &self,
        p: &mut Painter<TextBuf>,
        cfg: &Cfg,
        widths: &[usize],
        cells: &[Cell],
        slots: &[Slot],
    ) {
        let height = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Cell { id, .. } => Some(cells[*id].lines.len()),
                Slot::Covered { .. } => None,
            })
            .max()
            .unwrap_or(1);
        for line in 0..height {
            self.open(p, cfg);
            p.write(S_BORDER, "│");
            for slot in slots {
                match *slot {
                    Slot::Cell { id, start, span } => {
                        let cell = &cells[id];
                        let width = combined_width(widths, start, span);
                        let text = cell.lines.get(line).map(String::as_str).unwrap_or("");
                        p.write_raw(&pad(text, width, cell.align));
                    }
                    Slot::Covered { start, span } => {
                        p.write_raw(&" ".repeat(combined_width(widths, start, span) + 2));
                    }
                }
                p.write(S_BORDER, "│");
            }
            p.newline();
        }
    }

    /// 行与行之间的框线。跨行的单元格在那里没有横线，竖线也要相应换成
    /// `├`、`┤` 或 `┴` 这种接头，否则表格会看起来被切断。
    fn table_separator(
        &self,
        p: &mut Painter<TextBuf>,
        cfg: &Cfg,
        widths: &[usize],
        placements: &[Vec<Option<usize>>],
        row: usize,
    ) {
        let cols = widths.len();
        let cross =
            |c: usize| placements[row][c].is_some() && placements[row][c] == placements[row + 1][c];
        let up = |c: usize| placements[row][c] != placements[row][c + 1];
        let down = |c: usize| placements[row + 1][c] != placements[row + 1][c + 1];

        self.open(p, cfg);
        let mut line = String::new();
        line.push_str(if cross(0) { "│" } else { "├" });
        for (c, width) in widths.iter().enumerate() {
            if cross(c) {
                line.push_str(&" ".repeat(width + 2));
            } else {
                line.push_str(&"─".repeat(width + 2));
            }
            if c + 1 < cols {
                line.push_str(junction(up(c), down(c), !cross(c), !cross(c + 1)));
            }
        }
        line.push_str(if cross(cols - 1) { "│" } else { "┤" });
        p.write(S_BORDER, &line);
        p.newline();
    }
}

#[derive(Clone, Copy)]
struct Env {
    has_title: bool,
    /// 当前章节的嵌套深度，0 表示顶层章节。
    depth: u8,
}

#[derive(Clone, Default)]
struct Cfg {
    prefix: String,
    style: Style,
    in_table: bool,
}

struct Lead {
    style: Style,
    text: String,
    strip: String,
}

struct Cell {
    lines: Vec<String>,
    colspan: usize,
    align: Align,
    start: (usize, usize),
}

#[derive(Clone, Copy)]
enum Slot {
    Cell {
        id: usize,
        start: usize,
        span: usize,
    },
    Covered {
        start: usize,
        span: usize,
    },
}

fn combined_width(widths: &[usize], start: usize, span: usize) -> usize {
    widths[start..start + span].iter().sum::<usize>() + 3 * span.saturating_sub(1)
}

fn junction(up: bool, down: bool, left: bool, right: bool) -> &'static str {
    match (up, down, left, right) {
        (true, true, true, true) => "┼",
        (true, true, true, false) => "┤",
        (true, true, false, true) => "├",
        (true, true, false, false) => "│",
        (true, false, true, true) => "┴",
        (true, false, true, false) => "┘",
        (true, false, false, true) => "└",
        (true, false, false, false) => "│",
        (false, true, true, true) => "┬",
        (false, true, true, false) => "┐",
        (false, true, false, true) => "┌",
        (false, true, false, false) => "│",
        (false, false, true, true) => "─",
        (false, false, true, false) => "─",
        (false, false, false, true) => "─",
        (false, false, false, false) => " ",
    }
}

fn find_child(e: &Element, kind: Kind) -> Option<&Element> {
    e.elements().find(|child| child.kind == kind)
}

fn attr_usize(e: &Element, name: &str) -> Option<usize> {
    match e.attributes.get(name) {
        Some(AttrValue::Int(n)) => usize::try_from(*n).ok(),
        Some(AttrValue::Str(s)) => s.parse().ok(),
        _ => None,
    }
}

fn parse_align(value: &str) -> Align {
    match value {
        "left" => Align::Left,
        "center" => Align::Center,
        "right" => Align::Right,
        _ => Align::None,
    }
}

fn literal_language(e: &Element) -> Option<String> {
    e.attributes
        .get_list("classes")
        .iter()
        .find(|class| class.as_str() != "code")
        .cloned()
}

fn enum_label(list: &Element, index: usize) -> String {
    let start = attr_usize(list, "start").unwrap_or(1) as u64;
    let n = start + index as u64;
    let prefix = list.attributes.get_str("prefix").unwrap_or_default();
    let suffix = list.attributes.get_str("suffix").unwrap_or(".");
    let body = match list.attributes.get_str("enumtype") {
        Some("loweralpha") => alpha(n, false),
        Some("upperalpha") => alpha(n, true),
        Some("lowerroman") => roman(n, false),
        Some("upperroman") => roman(n, true),
        _ => n.to_string(),
    };
    format!("{prefix}{body}{suffix}")
}

fn alpha(mut n: u64, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut out = String::new();
    while n > 0 {
        n -= 1;
        let base = if upper { b'A' } else { b'a' };
        out.insert(0, char::from(base + (n % 26) as u8));
        n /= 26;
    }
    out
}

fn roman(mut n: u64, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    let table = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, symbol) in table {
        while n >= value {
            out.push_str(symbol);
            n -= value;
        }
    }
    if upper { out } else { out.to_lowercase() }
}

fn default_admonition_title(kind: &Kind) -> &'static str {
    match kind {
        Kind::Attention => "Attention!",
        Kind::Caution => "Caution!",
        Kind::Danger => "!DANGER!",
        Kind::Error => "Error",
        Kind::Hint => "Hint",
        Kind::Important => "Important",
        Kind::Note => "Note",
        Kind::Tip => "Tip",
        Kind::Warning => "Warning",
        _ => "Admonition",
    }
}

fn docinfo_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::Author => "Author",
        Kind::Authors => "Authors",
        Kind::Organization => "Organization",
        Kind::Address => "Address",
        Kind::Contact => "Contact",
        Kind::Version => "Version",
        Kind::Revision => "Revision",
        Kind::Status => "Status",
        Kind::Date => "Date",
        Kind::Copyright => "Copyright",
        _ => "Field",
    }
}

fn is_block_kind(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::Paragraph
            | Kind::Section
            | Kind::Title
            | Kind::Subtitle
            | Kind::BulletList
            | Kind::EnumeratedList
            | Kind::DefinitionList
            | Kind::FieldList
            | Kind::OptionList
            | Kind::BlockQuote
            | Kind::Attribution
            | Kind::LineBlock
            | Kind::LiteralBlock
            | Kind::DoctestBlock
            | Kind::MathBlock
            | Kind::Table
            | Kind::Attention
            | Kind::Caution
            | Kind::Danger
            | Kind::Error
            | Kind::Important
            | Kind::Note
            | Kind::Tip
            | Kind::Hint
            | Kind::Warning
            | Kind::Admonition
            | Kind::Figure
            | Kind::Image
            | Kind::Topic
            | Kind::Sidebar
            | Kind::Rubric
            | Kind::Transition
            | Kind::Comment
            | Kind::Target
            | Kind::SystemMessage
            | Kind::Raw
            | Kind::Container
            | Kind::Compound
            | Kind::Footnote
            | Kind::Citation
            | Kind::Docinfo
            | Kind::Decoration
            | Kind::Header
            | Kind::Footer
            | Kind::Meta
            | Kind::Pending
            | Kind::SubstitutionDefinition
    )
}

fn only_image(nodes: &[Node]) -> Option<&Element> {
    let mut found = None;
    for node in nodes {
        match node {
            Node::Element(e) if e.kind == Kind::Image && found.is_none() => found = Some(e),
            Node::Text(t) if t.value.trim().is_empty() => {}
            _ => return None,
        }
    }
    found
}

fn oneline(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// Sphinx 在 docutils 之外定义的角色。终端里没法解析它们的交叉引用，
/// 但也不该把 `:func:` 这样的标记原样丢给读者：显示角色里的文字，
/// 有显式标题（`文字 <目标>`）时只留文字。
const SPHINX_ROLES: &[&str] = &[
    "func",
    "class",
    "meth",
    "attr",
    "mod",
    "term",
    "ref",
    "doc",
    "source",
    "exc",
    "data",
    "obj",
    "const",
    "envvar",
    "option",
    "command",
    "file",
    "kbd",
    "guilabel",
    "menuselection",
    "program",
    "regexp",
    "samp",
    "issue",
    "cve",
    "numref",
    "download",
    "any",
    "token",
    "mailheader",
    "mimetype",
    "makevar",
    "manpage",
    "abbr",
    "dfn",
];

struct SphinxRole;

impl Role for SphinxRole {
    fn run(&self, cx: &mut RoleContext<'_>) -> Result<Vec<Node>, DirectiveError> {
        let text = display_role_text(&cx.text());
        let mut literal = Element::new(Kind::Literal);
        literal
            .children
            .push(Node::Text(Text::new(text, cx.span())));
        Ok(vec![Node::Element(literal)])
    }
}

fn display_role_text(text: &str) -> String {
    // Sphinx 用前导 `!` 表示「只显示文字、不要链接」，终端里正好只要文字
    let text = text.strip_prefix('!').unwrap_or(text);
    // 显式标题可以写成 `标题 <目标>`，也可以紧贴着写 `标题<目标>`
    match text.rsplit_once('<') {
        Some((title, target)) if target.ends_with('>') && !title.trim().is_empty() => {
            title.trim().to_string()
        }
        _ => text.to_string(),
    }
}

fn parse(src: &str, source: Option<&Path>) -> Element {
    // docutils 默认拒绝解析超过 10000 字符的行。对 cat 来说这是数据丢失：
    // 宁可慢一点，也要把整行原样渲染出来。
    let settings = Settings {
        line_length_limit: usize::MAX,
        ..Settings::default()
    };
    let mut parser = Parser::new()
        .settings(settings)
        .transforms(true)
        .keep_unknown_directives(true);
    if let Some(source) = source {
        // include 里的相对路径由解析器按 source 的目录算好，这里直接读
        parser = parser
            .source(source.to_string_lossy())
            .file_loader(|path: &str| std::fs::read(path));
    }
    for name in SPHINX_ROLES {
        parser = parser.role(name, SphinxRole);
    }
    parser.parse(src)
}

/// 按出现顺序收集文档里引用的所有图片，供 `--check-images` 报告。
/// 第二项表示这张图是否独占一段：独占一段的图用整屏高度预算。
pub fn image_urls(src: &str, source: Option<&Path>) -> Vec<(String, bool)> {
    fn walk(e: &Element, in_substitution: bool, block: bool, out: &mut Vec<(String, bool)>) {
        let in_substitution = in_substitution || e.kind == Kind::SubstitutionDefinition;
        if e.kind == Kind::Paragraph
            && !in_substitution
            && let Some(image) = only_image(&e.children)
            && let Some(uri) = image.attributes.get_str("uri")
            && !uri.is_empty()
        {
            out.push((uri.to_string(), true));
            return;
        }
        if e.kind == Kind::Image
            && !in_substitution
            && let Some(uri) = e.attributes.get_str("uri")
            && !uri.is_empty()
        {
            out.push((uri.to_string(), block));
            return;
        }
        for child in e.elements() {
            // 段落里的图片是行内图；其他块里的 Image 按整段图算
            walk(child, in_substitution, e.kind != Kind::Paragraph, out);
        }
    }
    let doc = parse(src, source);
    let mut out = Vec::new();
    walk(&doc, false, true, &mut out);
    out
}

/// 按出现顺序收集文档里所有 ```mermaid 代码块的源码，供 `--check-images` 报告。
pub fn mermaid_sources(src: &str, source: Option<&Path>) -> Vec<String> {
    fn walk(e: &Element, out: &mut Vec<String>) {
        let is_mermaid =
            e.kind == Kind::LiteralBlock && literal_language(e).as_deref() == Some(MERMAID_LANG);
        if is_mermaid {
            out.push(e.astext());
        }
        for child in e.elements() {
            walk(child, out);
        }
    }
    let doc = parse(src, source);
    let mut out = Vec::new();
    walk(&doc, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn highlighter() -> &'static Highlighter {
        static HL: OnceLock<Highlighter> = OnceLock::new();
        HL.get_or_init(|| Highlighter::new("base16-ocean.dark").expect("内置主题"))
    }

    fn render(src: &str) -> String {
        Rst::new(highlighter(), false, None, None).render(src)
    }

    fn render_width(src: &str, width: usize) -> String {
        Rst::new(highlighter(), false, None, None)
            .with_width(Some(width))
            .render(src)
    }

    #[test]
    fn sphinx_角色只显示文字() {
        assert_eq!(render(":func:`asyncio.run`\n"), "asyncio.run\n");
        assert_eq!(render(":mod:`!argparse`\n"), "argparse\n");
        assert_eq!(render(":doc:`layer<layer-concept>`\n"), "layer\n");
        assert_eq!(render(":ref:`Some section <target>`\n"), "Some section\n");
    }

    #[test]
    fn 文档标题画等宽横线() {
        assert_eq!(render("Title\n=====\n\nText.\n"), "Title\n─────\n\nText.\n");
    }

    #[test]
    fn 中文标题按显示宽度画横线() {
        assert_eq!(render("标题\n====\n\n正文\n"), "标题\n────\n\n正文\n");
    }

    #[test]
    fn 没有文档标题时一级章节也是_h1() {
        assert_eq!(
            render("Section\n=======\n\nText.\n"),
            "Section\n───────\n\nText.\n"
        );
    }

    #[test]
    fn 有文档标题时章节从_h2_开始() {
        let out = render("Title\n=====\n\nIntro.\n\nSection\n-------\n\nText.\n");
        // 只有文档标题下面有横线，二级章节没有
        assert_eq!(out, "Title\n─────\n\nIntro.\n\nSection\n\nText.\n");
    }

    #[test]
    fn 列表保留编号格式() {
        assert_eq!(render("- a\n- b\n"), "• a\n• b\n");
        assert_eq!(render("3. a\n4. b\n"), "3. a\n4. b\n");
        assert_eq!(render("a) a\nb) b\n"), "a) a\nb) b\n");
        assert_eq!(render("iv. a\nv. b\n"), "iv. a\nv. b\n");
    }

    #[test]
    fn 嵌套列表按层级缩进() {
        assert_eq!(render("- a\n\n  - b\n"), "• a\n\n  • b\n");
    }

    #[test]
    fn 行内标记和链接() {
        assert_eq!(render("**粗** *斜* ``code``\n"), "粗 斜 code\n");
        assert_eq!(render("`文字 <https://a>`_\n"), "文字 (https://a)\n");
        assert_eq!(render("https://a\n"), "https://a\n");
    }

    #[test]
    fn 代码块按语言高亮() {
        let out = Rst::new(highlighter(), true, None, None)
            .render(".. code-block:: rust\n\n   fn main() {}\n");
        assert!(out.contains("\x1b[38;2;"), "{out:?}");
        assert!(out.starts_with("\x1b[48;2;"), "{out:?}");
        assert_eq!(
            render(".. code-block:: rust\n\n   fn main() {}\n"),
            "  fn main() {}\n"
        );
    }

    #[test]
    fn 字面块保留原样() {
        assert_eq!(render("::\n\n   a\n\n   b\n"), "  a\n  \n  b\n");
    }

    #[test]
    fn grid_表格画框线() {
        let src = "\
+----+----+
| A  | B  |
+====+====+
| 1  | 2  |
+----+----+
";
        assert_eq!(
            render(src),
            "┌───┬───┐\n\
             │ A │ B │\n\
             ├───┼───┤\n\
             │ 1 │ 2 │\n\
             └───┴───┘\n"
        );
    }

    #[test]
    fn 表格跨行跨列() {
        let src = "\
+-------+-------+
| A     | B     |
+=======+=======+
| span two      |
+-------+-------+
| C     | D     |
|       +-------+
|       | E     |
+-------+-------+
";
        assert_eq!(
            render(src),
            "┌───┬──────┐\n\
             │ A │ B    │\n\
             ├───┴──────┤\n\
             │ span two │\n\
             ├───┬──────┤\n\
             │ C │ D    │\n\
             │   ├──────┤\n\
             │   │ E    │\n\
             └───┴──────┘\n"
        );
    }

    #[test]
    fn 表格按终端宽度折行() {
        let src = "\
+-------+------------------+
| 名字  | 说明             |
+=======+==================+
| 甲    | 一二三四五六七八 |
+-------+------------------+
";
        let out = render_width(src, 20);
        assert_eq!(
            out,
            "┌──────┬───────────┐\n\
             │ 名字 │ 说明      │\n\
             ├──────┼───────────┤\n\
             │ 甲   │ 一二三四  │\n\
             │      │ 五六七八  │\n\
             └──────┴───────────┘\n"
        );
        for line in out.lines() {
            assert_eq!(display_width(line), 20, "{line:?}");
        }
    }

    #[test]
    fn 定义列表和字段列表() {
        assert_eq!(render("term\n  definition\n"), "term\n  definition\n");
        assert_eq!(render(":name: value\n"), "name: value\n");
    }

    #[test]
    fn 警告块带标题和缩进() {
        assert_eq!(render(".. note::\n\n   小心。\n"), "Note\n  小心。\n");
        assert_eq!(
            render(".. admonition:: 自定义\n\n   正文\n"),
            "自定义\n  正文\n"
        );
    }

    #[test]
    fn 图片替换在正文里退回文字占位() {
        let src = ".. |img| image:: a.png\n   :alt: 图\n\n正文 |img|。\n";
        assert_eq!(render(src), "正文 [image] 图 (a.png)。\n");
    }

    #[test]
    fn 未知指令保留指令行和内容() {
        let out = render(".. versionadded:: 2.0\n   New thing.\n");
        assert_eq!(out, ".. versionadded:: 2.0 New thing.\n");
        let out = render(".. toctree::\n\n   intro\n");
        assert!(out.contains(".. toctree::"), "{out:?}");
        assert!(out.contains("intro"), "{out:?}");
    }

    #[test]
    fn 注释和链接目标不进正文() {
        assert_eq!(
            render(".. 注释\n\n.. _target: https://x\n\nText.\n"),
            "Text.\n"
        );
    }

    #[test]
    fn 引用块带竖线和署名() {
        assert_eq!(
            render("Quote:\n\n    quoted\n\n    -- Me\n"),
            "Quote:\n\n│ quoted\n\n│ — Me\n"
        );
    }

    #[test]
    fn 脚注和引用按标签显示() {
        assert_eq!(
            render("正文 [1]_\n\n.. [1] 说明\n"),
            "正文 [1]\n\n[1]\n  说明\n"
        );
    }

    #[test]
    fn 行块保留换行和缩进() {
        assert_eq!(
            render("| line one\n| line two\n|   nested\n"),
            "line one\nline two\n  nested\n"
        );
    }

    #[test]
    fn 收集替换后的图片且不重复() {
        let src = ".. |i| image:: a.png\n   :alt: x\n\n.. image:: b.png\n\n正文 |i|。\n";
        let urls = image_urls(src, None);
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert!(urls.contains(&("a.png".to_string(), false)), "{urls:?}");
        assert!(urls.contains(&("b.png".to_string(), true)), "{urls:?}");
    }

    #[test]
    fn 独占一段的图片用整屏高度() {
        use crate::image::{Images, Protocol};
        let images = Images::new(
            Protocol::Kitty,
            Sizing {
                max_cols: 10,
                max_rows: 5,
            },
            Sizing {
                max_cols: 40,
                max_rows: 20,
            },
        );
        let embedded = images.for_file(PathBuf::from("examples"));
        let block = images.for_block(PathBuf::from("examples"));
        let mut rst =
            Rst::new(highlighter(), false, Some(&embedded), None).with_block_images(Some(&block));
        let out = rst.render("|i|\n\n.. |i| image:: gradient.png\n");
        assert!(out.contains("c=40"), "{out:?}");
        assert!(!out.contains("c=10"), "{out:?}");

        let mut rst =
            Rst::new(highlighter(), false, Some(&embedded), None).with_block_images(Some(&block));
        let out = rst.render("正文 |i|。\n\n.. |i| image:: gradient.png\n");
        assert!(out.contains("c=10"), "{out:?}");
        assert!(!out.contains("c=40"), "{out:?}");
    }

    #[test]
    fn 独占一段的图片标记为整段() {
        let urls = image_urls(".. |i| image:: a.png\n\n|i|\n", None);
        assert_eq!(urls, vec![("a.png".to_string(), true)]);
    }

    #[test]
    fn 只收集_mermaid_代码块() {
        let src = ".. code-block:: mermaid\n\n   flowchart LR\n     A --> B\n\n\
                   .. code-block:: rust\n\n   fn main() {}\n";
        let found = mermaid_sources(src, None);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("flowchart LR"));
    }

    #[test]
    fn include_按源文件目录读() {
        let dir = std::env::temp_dir().join(format!(
            "mcat-rst-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("part.rst"), "Included **text**.\n").unwrap();
        let mut rst = Rst::new(highlighter(), false, None, None).with_source(dir.join("main.rst"));
        let out = rst.render("Before.\n\n.. include:: part.rst\n");
        assert_eq!(out, "Before.\n\nIncluded text.\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 禁用的指令原文不会丢() {
        assert_eq!(
            render(".. include:: other.rst\n"),
            "  .. include:: other.rst\n"
        );
    }

    #[test]
    fn 超长行不丢内容() {
        let text = "a".repeat(12_000);
        let out = render(&format!("{text}\n"));
        assert_eq!(out, format!("{text}\n"));
    }

    #[test]
    fn 空文档输出空串() {
        assert_eq!(render(""), "");
        assert_eq!(render("\n\n"), "");
    }
}
