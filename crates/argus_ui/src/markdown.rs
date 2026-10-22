//! Markdown for comment bodies: paragraphs, headings, emphasis, inline
//! code, fenced code blocks (highlighted by the background highlighter),
//! lists, block quotes and links that open in the browser.
//!
//! [`Markdown::parse`] turns a body into a small block model once; views
//! keep the results in a [`MarkdownCache`] keyed by the body text, which
//! also requests the highlights of fenced code blocks on the highlighter's
//! worker threads ([`HighlightService`]), so neither parsing nor
//! highlighting is redone per frame. [`render`] builds the elements.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

use argus_highlight::{
    HighlightKey, HighlightRequest, HighlightService, Highlights, Language, Outcome, Priority,
};
use gpui::{
    AnyElement, App, Context, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString,
    StrikethroughStyle, StyledText, Task, UnderlineStyle, div, prelude::*, px,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::AppContext;
use crate::theme::{ActiveTheme, Theme};

/// Parsed bodies a cache keeps beyond the ones in use.
const MAX_CACHED_DOCS: usize = 4096;

/// Inline formatting of a run of text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Format {
    strong: bool,
    emphasis: bool,
    strikethrough: bool,
    code: bool,
    link: bool,
}

/// A paragraph-like run of formatted text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RichText {
    text: String,
    /// Formatted runs, sorted and non-overlapping; unformatted text has none.
    runs: Vec<(Range<usize>, Format)>,
    /// Link targets by text range.
    links: Vec<(Range<usize>, String)>,
}

impl RichText {
    fn push(&mut self, text: &str, format: Format) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if format != Format::default() {
            match self.runs.last_mut() {
                Some((range, last)) if *last == format && range.end == start => {
                    range.end = self.text.len();
                }
                _ => self.runs.push((start..self.text.len(), format)),
            }
        }
    }

    fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// One block of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Text.
    Paragraph(RichText),
    /// A heading, level 1–6.
    Heading(u8, RichText),
    /// A fenced or indented code block.
    Code {
        /// From the fence's info string, when it names a known language.
        language: Option<Language>,
        /// The code, without its final line break.
        code: String,
    },
    /// A bulleted (`start: None`) or numbered list; each item is blocks.
    List {
        /// Number of the first item of an ordered list.
        start: Option<u64>,
        /// The items.
        items: Vec<Vec<Block>>,
    },
    /// A block quote.
    Quote(Vec<Block>),
    /// A horizontal rule.
    Rule,
}

/// A parsed comment body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Markdown {
    blocks: Vec<Block>,
}

/// Containers open while parsing.
enum Container {
    Root,
    Quote,
    List(Option<u64>, Vec<Vec<Block>>),
    Item,
}

impl Markdown {
    /// Parses CommonMark (plus strikethrough and task lists). Raw HTML is
    /// shown as text.
    pub fn parse(source: &str) -> Self {
        let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let mut stack: Vec<(Container, Vec<Block>)> = vec![(Container::Root, Vec::new())];
        let mut inline: Option<RichText> = None;
        let mut heading: Option<u8> = None;
        let mut code: Option<(Option<Language>, String)> = None;
        let mut format = Format::default();
        let mut links: Vec<(usize, String)> = Vec::new();

        fn flush(
            inline: &mut Option<RichText>,
            heading: &mut Option<u8>,
            stack: &mut [(Container, Vec<Block>)],
        ) {
            if let Some(text) = inline.take()
                && !text.is_blank()
                && let Some((_, blocks)) = stack.last_mut()
            {
                blocks.push(match heading.take() {
                    Some(level) => Block::Heading(level, text),
                    None => Block::Paragraph(text),
                });
            }
        }

        for event in Parser::new_ext(source, options) {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {
                        flush(&mut inline, &mut heading, &mut stack);
                        inline = Some(RichText::default());
                    }
                    Tag::Heading { level, .. } => {
                        flush(&mut inline, &mut heading, &mut stack);
                        heading = Some(level as u8);
                        inline = Some(RichText::default());
                    }
                    Tag::CodeBlock(kind) => {
                        flush(&mut inline, &mut heading, &mut stack);
                        let language = match kind {
                            CodeBlockKind::Fenced(info) => info
                                .split(|ch: char| ch.is_whitespace() || ch == ',')
                                .next()
                                .and_then(Language::from_injection_name),
                            CodeBlockKind::Indented => None,
                        };
                        code = Some((language, String::new()));
                    }
                    Tag::BlockQuote(_) => {
                        flush(&mut inline, &mut heading, &mut stack);
                        stack.push((Container::Quote, Vec::new()));
                    }
                    Tag::List(start) => {
                        flush(&mut inline, &mut heading, &mut stack);
                        stack.push((Container::List(start, Vec::new()), Vec::new()));
                    }
                    Tag::Item => {
                        flush(&mut inline, &mut heading, &mut stack);
                        stack.push((Container::Item, Vec::new()));
                    }
                    Tag::Emphasis => format.emphasis = true,
                    Tag::Strong => format.strong = true,
                    Tag::Strikethrough => format.strikethrough = true,
                    Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                        let text = inline.get_or_insert_with(RichText::default);
                        links.push((text.text.len(), dest_url.to_string()));
                        format.link = true;
                    }
                    _ => {}
                },
                Event::End(tag) => match tag {
                    TagEnd::Paragraph | TagEnd::Heading(_) => {
                        flush(&mut inline, &mut heading, &mut stack);
                    }
                    TagEnd::CodeBlock => {
                        if let Some((language, mut text)) = code.take() {
                            if text.ends_with('\n') {
                                text.pop();
                            }
                            if let Some((_, blocks)) = stack.last_mut() {
                                blocks.push(Block::Code {
                                    language,
                                    code: text,
                                });
                            }
                        }
                    }
                    TagEnd::BlockQuote(_) => {
                        flush(&mut inline, &mut heading, &mut stack);
                        if stack.len() > 1
                            && let Some((Container::Quote, blocks)) = stack.pop()
                            && let Some((_, parent)) = stack.last_mut()
                        {
                            parent.push(Block::Quote(blocks));
                        }
                    }
                    TagEnd::Item => {
                        // Tight list items carry their text without a
                        // paragraph.
                        flush(&mut inline, &mut heading, &mut stack);
                        if stack.len() > 1
                            && let Some((Container::Item, blocks)) = stack.pop()
                            && let Some((Container::List(_, items), _)) = stack.last_mut()
                        {
                            items.push(blocks);
                        }
                    }
                    TagEnd::List(_) => {
                        flush(&mut inline, &mut heading, &mut stack);
                        if stack.len() > 1
                            && let Some((Container::List(start, items), _)) = stack.pop()
                            && let Some((_, parent)) = stack.last_mut()
                        {
                            parent.push(Block::List { start, items });
                        }
                    }
                    TagEnd::Emphasis => format.emphasis = false,
                    TagEnd::Strong => format.strong = false,
                    TagEnd::Strikethrough => format.strikethrough = false,
                    TagEnd::Link | TagEnd::Image => {
                        if let (Some((start, url)), Some(text)) = (links.pop(), inline.as_mut())
                            && start < text.text.len()
                        {
                            text.links.push((start..text.text.len(), url));
                        }
                        format.link = !links.is_empty();
                    }
                    _ => {}
                },
                Event::Text(text) => match &mut code {
                    Some((_, code)) => code.push_str(&text),
                    None => inline
                        .get_or_insert_with(RichText::default)
                        .push(&text, format),
                },
                Event::Code(text) => inline.get_or_insert_with(RichText::default).push(
                    &text,
                    Format {
                        code: true,
                        ..format
                    },
                ),
                Event::Html(text) | Event::InlineHtml(text) => match &mut code {
                    Some((_, code)) => code.push_str(&text),
                    None => inline
                        .get_or_insert_with(RichText::default)
                        .push(&text, format),
                },
                Event::InlineMath(text) | Event::DisplayMath(text) => inline
                    .get_or_insert_with(RichText::default)
                    .push(&text, format),
                Event::SoftBreak => inline
                    .get_or_insert_with(RichText::default)
                    .push(" ", format),
                Event::HardBreak => inline
                    .get_or_insert_with(RichText::default)
                    .push("\n", format),
                Event::TaskListMarker(done) => inline
                    .get_or_insert_with(RichText::default)
                    .push(if done { "☑ " } else { "☐ " }, Format::default()),
                Event::Rule => {
                    flush(&mut inline, &mut heading, &mut stack);
                    if let Some((_, blocks)) = stack.last_mut() {
                        blocks.push(Block::Rule);
                    }
                }
                Event::FootnoteReference(name) => inline
                    .get_or_insert_with(RichText::default)
                    .push(&format!("[{name}]"), format),
            }
        }
        flush(&mut inline, &mut heading, &mut stack);
        // Unbalanced containers cannot happen with pulldown-cmark; fold any
        // leftovers into the root anyway.
        while stack.len() > 1 {
            if let Some((_, blocks)) = stack.pop()
                && let Some((_, parent)) = stack.last_mut()
            {
                parent.extend(blocks);
            }
        }
        let blocks = stack.pop().map(|(_, blocks)| blocks).unwrap_or_default();
        Self { blocks }
    }

    /// The code blocks with a known language, for highlighting.
    fn code_blocks(&self) -> Vec<(Language, &str)> {
        fn walk<'a>(blocks: &'a [Block], out: &mut Vec<(Language, &'a str)>) {
            for block in blocks {
                match block {
                    Block::Code {
                        language: Some(language),
                        code,
                    } => out.push((*language, code)),
                    Block::List { items, .. } => {
                        for item in items {
                            walk(item, out);
                        }
                    }
                    Block::Quote(blocks) => walk(blocks, out),
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.blocks, &mut out);
        out
    }
}

/// Parsed bodies by text, plus the highlight jobs of their code blocks.
#[derive(Default)]
pub struct MarkdownCache {
    docs: HashMap<u64, Arc<Markdown>>,
    jobs: HashMap<HighlightKey, Task<()>>,
}

fn body_hash(body: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

impl MarkdownCache {
    /// The parsed `body`, if [`MarkdownCache::prepare`] saw it.
    pub fn get(&self, body: &str) -> Option<Arc<Markdown>> {
        self.docs.get(&body_hash(body)).cloned()
    }

    /// Parses `body` (once) and requests the highlights of its code blocks
    /// on the highlighter's threads; the view repaints when they arrive.
    /// `cache` finds this cache inside the view.
    pub fn prepare<V: 'static>(
        &mut self,
        body: &str,
        cache: fn(&mut V) -> &mut MarkdownCache,
        cx: &mut Context<V>,
    ) -> Arc<Markdown> {
        let hash = body_hash(body);
        if let Some(doc) = self.docs.get(&hash) {
            return doc.clone();
        }
        if self.docs.len() >= MAX_CACHED_DOCS {
            self.docs.clear();
        }
        let doc = Arc::new(Markdown::parse(body));
        self.docs.insert(hash, doc.clone());
        let service: HighlightService = cx.global::<AppContext>().highlighter.clone();
        for (language, code) in doc.code_blocks() {
            let Some(key) = HighlightKey::for_text(code, language) else {
                continue;
            };
            if self.jobs.contains_key(&key) || service.cached(&key).is_some() {
                continue;
            }
            let job = service.request(HighlightRequest {
                key,
                text: Box::new(code.to_owned()),
                priority: Priority::Visible,
                force: false,
            });
            let task = cx.spawn(async move |this, cx| {
                let result = job.await;
                if let Err(error) = &result {
                    tracing::debug!(%error, "code block highlighting failed");
                }
                this.update(cx, |view, cx| {
                    cache(view).jobs.remove(&key);
                    if matches!(result, Ok(Outcome::Highlighted(_))) {
                        cx.notify();
                    }
                })
                .ok();
            });
            self.jobs.insert(key, task);
        }
        doc
    }
}

/// Renders a parsed body. `id` must be unique among the documents shown
/// together (element ids of the clickable texts derive from it).
/// The first non-blank line of `body` as plain text, for one-line
/// previews: inline code, emphasis and link syntax are dropped, their text
/// kept (`` `get` takes **two** `` → `get takes two`).
pub fn first_line_plain(body: &str) -> String {
    let line = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut text = String::with_capacity(line.len());
    for event in Parser::new(line) {
        match event {
            Event::Text(part) | Event::Code(part) => text.push_str(&part),
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            _ => {}
        }
    }
    text
}

pub fn render(doc: &Markdown, id: SharedString, cx: &App) -> AnyElement {
    let mut counter = 0;
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .min_w_0()
        .children(render_blocks(&doc.blocks, &id, &mut counter, cx))
        .into_any_element()
}

fn render_blocks(
    blocks: &[Block],
    id: &SharedString,
    counter: &mut usize,
    cx: &App,
) -> Vec<AnyElement> {
    let theme = cx.theme();
    let colors = &theme.colors;
    blocks
        .iter()
        .map(|block| match block {
            Block::Paragraph(text) => render_text(text, id, counter, cx),
            Block::Heading(level, text) => div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(theme.fonts.ui_size * if *level <= 2 { 1.15 } else { 1.0 })
                .child(render_text(text, id, counter, cx))
                .into_any_element(),
            Block::Code { language, code } => render_code(*language, code, cx),
            Block::List { start, items } => div()
                .flex()
                .flex_col()
                .gap_0p5()
                .children(items.iter().enumerate().map(|(index, item)| {
                    let marker = match start {
                        Some(start) => format!("{}.", start + index as u64),
                        None => "•".to_owned(),
                    };
                    div()
                        .flex()
                        .gap_1p5()
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(14.0))
                                .text_color(colors.text_muted)
                                .child(marker),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .flex_1()
                                .min_w_0()
                                .children(render_blocks(item, id, counter, cx)),
                        )
                }))
                .into_any_element(),
            Block::Quote(blocks) => div()
                .flex()
                .flex_col()
                .gap_1()
                .pl_2()
                .border_l_2()
                .border_color(colors.border)
                .text_color(colors.text_muted)
                .children(render_blocks(blocks, id, counter, cx))
                .into_any_element(),
            Block::Rule => div()
                .h(px(1.0))
                .w_full()
                .bg(colors.border_variant)
                .into_any_element(),
        })
        .collect()
}

fn render_text(text: &RichText, id: &SharedString, counter: &mut usize, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let colors = &theme.colors;
    let highlights: Vec<(Range<usize>, HighlightStyle)> = text
        .runs
        .iter()
        .map(|(range, format)| {
            let mut style = HighlightStyle::default();
            if format.strong {
                style.font_weight = Some(FontWeight::BOLD);
            }
            if format.emphasis {
                style.font_style = Some(FontStyle::Italic);
            }
            if format.strikethrough {
                style.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: None,
                });
            }
            if format.code {
                // The `background` chip of inline code (Panoptes #56 §5). Text
                // highlights cannot round their corners, so the chip is square.
                style.background_color = Some(colors.background);
            }
            if format.link {
                style.color = Some(colors.accent);
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(colors.accent.opacity(0.6)),
                    wavy: false,
                });
            }
            (range.clone(), style)
        })
        .collect();
    let code_font: Vec<(Range<usize>, SharedString)> = text
        .runs
        .iter()
        .filter(|(_, format)| format.code)
        .map(|(range, _)| (range.clone(), theme.fonts.code_family.clone()))
        .collect();
    let styled = StyledText::new(SharedString::from(text.text.clone()))
        .with_highlights(highlights)
        .with_font_family_overrides(code_font);
    if text.links.is_empty() {
        return div().min_w_0().child(styled).into_any_element();
    }
    *counter += 1;
    let element_id = SharedString::from(format!("{id}-text-{counter}"));
    let ranges: Vec<Range<usize>> = text.links.iter().map(|(range, _)| range.clone()).collect();
    let urls: Vec<String> = text.links.iter().map(|(_, url)| url.clone()).collect();
    div()
        .min_w_0()
        .cursor_pointer()
        .child(
            InteractiveText::new(element_id, styled).on_click(ranges, move |index, _, cx| {
                if let Some(url) = urls.get(index) {
                    open_link(url, cx);
                }
            }),
        )
        .into_any_element()
}

/// Opens a link in the browser. Only web and mail links: a comment body
/// must not be able to launch local files or custom URL handlers.
fn open_link(url: &str, cx: &mut App) {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
    {
        cx.open_url(url.trim());
    } else {
        tracing::debug!(url, "not opening a non-web link");
    }
}

fn render_code(language: Option<Language>, code: &str, cx: &App) -> AnyElement {
    let theme: &Theme = cx.theme();
    let colors = &theme.colors;
    let highlights: Option<Arc<Highlights>> = language
        .and_then(|language| HighlightKey::for_text(code, language))
        .and_then(|key| cx.global::<AppContext>().highlighter.cached(&key));
    let lines = code.split('\n').enumerate().map(|(index, line)| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let spans: Vec<(Range<usize>, HighlightStyle)> = highlights
            .as_ref()
            .map(|highlights| {
                highlights
                    .line(index as u32 + 1)
                    .iter()
                    .filter(|span| span.range().end <= line.len())
                    .map(|span| (span.range(), theme.syntax.style(span.kind)))
                    .collect()
            })
            .unwrap_or_default();
        let text = if line.is_empty() { " " } else { line };
        div()
            .whitespace_nowrap()
            .child(StyledText::new(SharedString::from(text.to_owned())).with_highlights(spans))
    });
    div()
        .flex()
        .flex_col()
        .w_full()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(colors.surface)
        .border_1()
        .border_color(colors.border_variant)
        .overflow_hidden()
        .font_family(theme.fonts.code_family.clone())
        .text_size(theme.fonts.code_size)
        .children(lines)
        .into_any_element()
}
