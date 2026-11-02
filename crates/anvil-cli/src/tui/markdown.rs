//! Markdown that renders as it streams.
//!
//! A document is a list of frozen blocks plus one open block. Deltas go to
//! the open block; whenever it ends (a blank line, a heading, a list item,
//! a rule, or a closed fence) the finished part is parsed once and frozen
//! for good. Only the open block is re-parsed and re-wrapped at each draw,
//! so earlier text never changes once shown.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::{Shade, Theme};
use super::wrap::segments;

/// A block of rendered markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// `# Heading`.
    Heading {
        /// One to six.
        level: u8,
        /// The text.
        inlines: Vec<Inline>,
    },
    /// Running text.
    Paragraph {
        /// The text.
        inlines: Vec<Inline>,
    },
    /// One list item.
    Item {
        /// Nesting, from zero.
        depth: usize,
        /// The bullet or number.
        marker: Marker,
        /// A task checkbox, if the item has one.
        checkbox: Option<bool>,
        /// The text.
        inlines: Vec<Inline>,
    },
    /// A fenced or indented code block.
    Code {
        /// The language after the fence, possibly empty.
        lang: String,
        /// The code, one entry per line.
        lines: Vec<String>,
        /// Whether the closing fence has arrived.
        closed: bool,
    },
    /// `> quoted` text.
    Quote {
        /// The text.
        inlines: Vec<Inline>,
    },
    /// A horizontal rule.
    Rule,
}

/// What starts a list item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// `-`, `*` or `+`.
    Bullet,
    /// `N.` or `N)`.
    Number(u64),
    /// A later paragraph of the same item: indented, no marker.
    Continuation,
}

/// A run of text with one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inline {
    /// The text; `\n` is a hard break.
    pub text: String,
    /// How to draw it.
    pub style: InlineStyle,
}

/// Inline emphasis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineStyle {
    /// Plain text.
    Plain,
    /// `` `code` ``.
    Code,
    /// `**strong**`.
    Strong,
    /// `*emphasis*`.
    Emphasis,
}

/// An open fence: the marker that will close it and its language.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fence {
    marker: String,
    lang: String,
}

/// Where the open text can be cut.
#[derive(Debug, PartialEq, Eq)]
enum Cut {
    /// Freeze everything before this byte offset as parsed blocks.
    Freeze(usize),
    /// The open text starts with a fence opener; enter the fence.
    OpenFence(Fence),
    /// The fence closes at this byte offset; freeze it as one code block.
    CloseFence(usize),
}

/// A streaming markdown document.
#[derive(Debug, Default)]
pub struct MarkdownDoc {
    /// Every delta so far.
    source: String,
    frozen: Vec<Block>,
    /// Text after the last frozen block.
    open: String,
    /// The fence the open text is inside, if any.
    fence: Option<Fence>,
    /// The frozen blocks rendered at one width.
    cache: Option<(u16, Vec<Line<'static>>)>,
}

impl MarkdownDoc {
    /// An empty document.
    ///
    /// # Returns
    ///
    /// The document.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends streamed text and freezes every block it completes.
    ///
    /// # Arguments
    ///
    /// * `delta` - The new text.
    pub fn push(&mut self, delta: &str) {
        self.source.push_str(delta);
        self.open.push_str(delta);
        loop {
            let cut = match &self.fence {
                Some(fence) => fence_close(&self.open, fence).map(Cut::CloseFence),
                None => boundary(&self.open),
            };
            match cut {
                Some(Cut::Freeze(end)) => {
                    let rest = self.open.split_off(end);
                    let text = std::mem::replace(&mut self.open, rest);
                    self.freeze(parse_blocks(&text));
                }
                Some(Cut::OpenFence(fence)) => self.fence = Some(fence),
                Some(Cut::CloseFence(end)) => {
                    let rest = self.open.split_off(end);
                    let text = std::mem::replace(&mut self.open, rest);
                    if let Some(fence) = self.fence.take() {
                        self.freeze(vec![code_block(&text, &fence, true)]);
                    }
                }
                None => break,
            }
        }
    }

    /// Freezes whatever is still open; no more deltas are expected.
    pub fn finish(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.open);
        match self.fence.take() {
            Some(fence) => self.freeze(vec![code_block(&text, &fence, true)]),
            None => self.freeze(parse_blocks(&text)),
        }
    }

    /// Every delta so far, verbatim, for tests.
    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.source
    }

    /// How many blocks are frozen; never decreases. For tests.
    #[cfg(test)]
    pub fn frozen_len(&self) -> usize {
        self.frozen.len()
    }

    /// The frozen blocks, for tests.
    #[cfg(test)]
    pub fn frozen(&self) -> &[Block] {
        &self.frozen
    }

    /// The whole document as lines: the frozen part from the cache, the
    /// open block rendered fresh.
    ///
    /// # Arguments
    ///
    /// * `width` - Columns.
    /// * `theme` - Styles.
    ///
    /// # Returns
    ///
    /// The lines.
    pub fn lines(&mut self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        if self.cache.as_ref().is_none_or(|(w, _)| *w != width) {
            let lines = self
                .frozen
                .iter()
                .flat_map(|block| render_block(block, width, theme))
                .collect();
            self.cache = Some((width, lines));
        }
        let mut lines = self
            .cache
            .as_ref()
            .map(|(_, lines)| lines.clone())
            .unwrap_or_default();
        if !self.open.is_empty() {
            let open = match &self.fence {
                Some(fence) => vec![code_block(&self.open, fence, false)],
                None => parse_blocks(&self.open),
            };
            lines.extend(
                open.iter()
                    .flat_map(|block| render_block(block, width, theme)),
            );
        }
        lines
    }

    /// Appends frozen blocks and drops the cache.
    fn freeze(&mut self, blocks: Vec<Block>) {
        if blocks.is_empty() {
            return;
        }
        self.frozen.extend(blocks);
        self.cache = None;
    }
}

/// Finds the first place the open text can be cut outside a fence: a blank
/// line, or a complete heading, list item, rule or fence-opener line.
fn boundary(open: &str) -> Option<Cut> {
    let mut start = 0;
    while let Some(len) = open[start..].find('\n') {
        let end = start + len + 1;
        let line = &open[start..end - 1];
        if let Some(fence) = fence_open(line) {
            return Some(if start == 0 {
                Cut::OpenFence(fence)
            } else {
                Cut::Freeze(start)
            });
        }
        if line.trim().is_empty() || starts_block(line) {
            return Some(Cut::Freeze(end));
        }
        start = end;
    }
    None
}

/// The byte offset just past the line that closes `fence`, if it has
/// arrived. The first line of the open text is the opener and is skipped.
fn fence_close(open: &str, fence: &Fence) -> Option<usize> {
    let mut start = open.find('\n')? + 1;
    while let Some(len) = open[start..].find('\n') {
        let end = start + len + 1;
        let line = open[start..end - 1].trim();
        if line.starts_with(&fence.marker) && line.trim_start_matches(&fence.marker[..1]).is_empty()
        {
            return Some(end);
        }
        start = end;
    }
    None
}

/// The fence a line opens, if it is one: three or more backticks or tildes.
fn fence_open(line: &str) -> Option<Fence> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|c| *c == ch).count();
    if count < 3 {
        return None;
    }
    Some(Fence {
        marker: trimmed[..count].to_owned(),
        lang: trimmed[count..].trim().to_owned(),
    })
}

/// Whether a line starts a block of its own: a heading, a list item or a
/// rule.
fn starts_block(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        return hashes <= 6 && trimmed[hashes..].starts_with(' ');
    }
    if let Some(rest) = trimmed.strip_prefix(['-', '*', '+']) {
        if rest.starts_with(' ') {
            return true;
        }
        let only_marks = trimmed
            .chars()
            .all(|c| c == '-' || c == '*' || c == '_' || c == ' ');
        return only_marks && trimmed.chars().filter(|c| !c.is_whitespace()).count() >= 3;
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 9 {
        let rest = &trimmed[digits..];
        return (rest.starts_with('.') || rest.starts_with(')')) && rest[1..].starts_with(' ');
    }
    false
}

/// A code block from fenced text: the opener line is dropped and, when
/// closed, so is the closing line.
fn code_block(text: &str, fence: &Fence, closed: bool) -> Block {
    let mut lines: Vec<String> = text.lines().skip(1).map(str::to_owned).collect();
    if closed {
        lines.pop();
    }
    Block::Code {
        lang: fence.lang.clone(),
        lines,
        closed,
    }
}

/// A list item being built.
#[derive(Debug)]
struct ItemState {
    marker: Marker,
    checkbox: Option<bool>,
    /// Whether a paragraph of this item was already emitted.
    flushed: bool,
}

/// Parses complete markdown into blocks.
///
/// # Arguments
///
/// * `text` - The markdown.
///
/// # Returns
///
/// The blocks, in order.
pub fn parse_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut inlines: Vec<Inline> = Vec::new();
    let mut strong = false;
    let mut emphasis = false;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut items: Vec<ItemState> = Vec::new();
    let mut quote = 0usize;
    let mut heading: Option<u8> = None;
    let mut code: Option<(String, String)> = None;
    let options = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => heading = Some(level as u8),
            Event::End(TagEnd::Heading(_)) => blocks.push(Block::Heading {
                level: heading.take().unwrap_or(1),
                inlines: std::mem::take(&mut inlines),
            }),
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush_text(&mut blocks, &mut inlines, &mut items, lists.len(), quote);
            }
            Event::Start(Tag::List(start)) => {
                if !items.is_empty() && !inlines.is_empty() {
                    flush_text(&mut blocks, &mut inlines, &mut items, lists.len(), quote);
                }
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                let marker = match lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = Marker::Number(*number);
                        *number += 1;
                        marker
                    }
                    _ => Marker::Bullet,
                };
                items.push(ItemState {
                    marker,
                    checkbox: None,
                    flushed: false,
                });
            }
            Event::End(TagEnd::Item) => {
                let unflushed = items.last().is_some_and(|item| !item.flushed);
                if !inlines.is_empty() || unflushed {
                    flush_text(&mut blocks, &mut inlines, &mut items, lists.len(), quote);
                }
                items.pop();
            }
            Event::TaskListMarker(done) => {
                if let Some(item) = items.last_mut() {
                    item.checkbox = Some(done);
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, text)) = code.take() {
                    blocks.push(Block::Code {
                        lang,
                        lines: text.lines().map(str::to_owned).collect(),
                        closed: true,
                    });
                }
            }
            Event::Text(text) => match &mut code {
                Some((_, body)) => body.push_str(&text),
                None => push_inline(&mut inlines, &text, style_of(strong, emphasis)),
            },
            Event::Code(text) => push_inline(&mut inlines, &text, InlineStyle::Code),
            Event::Html(text) | Event::InlineHtml(text) => {
                push_inline(&mut inlines, &text, style_of(strong, emphasis));
            }
            Event::Start(Tag::Strong) => strong = true,
            Event::End(TagEnd::Strong) => strong = false,
            Event::Start(Tag::Emphasis) => emphasis = true,
            Event::End(TagEnd::Emphasis) => emphasis = false,
            Event::SoftBreak => push_inline(&mut inlines, " ", style_of(strong, emphasis)),
            Event::HardBreak => push_inline(&mut inlines, "\n", style_of(strong, emphasis)),
            Event::Rule => blocks.push(Block::Rule),
            Event::Start(Tag::BlockQuote(_)) => quote += 1,
            Event::End(TagEnd::BlockQuote(_)) => quote = quote.saturating_sub(1),
            _ => {}
        }
    }
    if !inlines.is_empty() {
        flush_text(&mut blocks, &mut inlines, &mut items, lists.len(), quote);
    }
    blocks
}

/// Emits the collected inlines as the block they belong to.
fn flush_text(
    blocks: &mut Vec<Block>,
    inlines: &mut Vec<Inline>,
    items: &mut [ItemState],
    depth: usize,
    quote: usize,
) {
    let inlines = std::mem::take(inlines);
    if let Some(item) = items.last_mut() {
        let marker = if item.flushed {
            Marker::Continuation
        } else {
            item.marker
        };
        item.flushed = true;
        blocks.push(Block::Item {
            depth: depth.saturating_sub(1),
            marker,
            checkbox: item.checkbox.take(),
            inlines,
        });
    } else if quote > 0 {
        blocks.push(Block::Quote { inlines });
    } else {
        blocks.push(Block::Paragraph { inlines });
    }
}

/// The style for text under the current emphasis.
fn style_of(strong: bool, emphasis: bool) -> InlineStyle {
    if strong {
        InlineStyle::Strong
    } else if emphasis {
        InlineStyle::Emphasis
    } else {
        InlineStyle::Plain
    }
}

/// Appends text, merging it into the previous inline when the style matches.
fn push_inline(inlines: &mut Vec<Inline>, text: &str, style: InlineStyle) {
    if text.is_empty() {
        return;
    }
    match inlines.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => inlines.push(Inline {
            text: text.to_owned(),
            style,
        }),
    }
}

/// Renders one line of markdown inline text (code, strong, emphasis)
/// wrapped at a width, for plan steps and the like.
///
/// # Arguments
///
/// * `text` - The text, one paragraph.
/// * `width` - Columns.
/// * `theme` - Styles.
///
/// # Returns
///
/// The lines, at least one.
pub fn inline_lines(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let inlines = parse_blocks(text)
        .into_iter()
        .find_map(|block| match block {
            Block::Paragraph { inlines }
            | Block::Heading { inlines, .. }
            | Block::Item { inlines, .. }
            | Block::Quote { inlines } => Some(inlines),
            Block::Code { .. } | Block::Rule => None,
        })
        .unwrap_or_else(|| {
            vec![Inline {
                text: text.to_owned(),
                style: InlineStyle::Plain,
            }]
        });
    wrap_inlines(&inlines, width, theme, false)
}

/// Renders one block at a width.
///
/// # Arguments
///
/// * `block` - The block.
/// * `width` - Columns.
/// * `theme` - Styles.
///
/// # Returns
///
/// The lines.
pub fn render_block(block: &Block, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = usize::from(width).max(1);
    match block {
        Block::Heading { level, inlines } => {
            let prefix = format!("{} ", "#".repeat(usize::from(*level)));
            let body = wrap_inlines(inlines, width.saturating_sub(prefix.len()), theme, true);
            with_prefix(body, Span::styled(prefix, theme.border()))
        }
        Block::Paragraph { inlines } => wrap_inlines(inlines, width, theme, false),
        Block::Item {
            depth,
            marker,
            checkbox,
            inlines,
        } => {
            let marker_text = match marker {
                Marker::Bullet => "• ".to_owned(),
                Marker::Number(n) => format!("{n}. "),
                Marker::Continuation => String::new(),
            };
            let checkbox = match checkbox {
                Some(true) => "[x] ",
                Some(false) => "[ ] ",
                None => "",
            };
            let indent = "  ".repeat(*depth);
            let prefix = format!("{indent}{marker_text}{checkbox}");
            let continuation = if *marker == Marker::Continuation && checkbox.is_empty() {
                format!("{indent}  ")
            } else {
                prefix.clone()
            };
            let body = wrap_inlines(
                inlines,
                width.saturating_sub(continuation.width()),
                theme,
                false,
            );
            with_prefix(body, Span::raw(continuation))
        }
        Block::Code {
            lang,
            lines,
            closed,
        } => render_code(lang, lines, *closed, width, theme),
        Block::Quote { inlines } => {
            let body = wrap_inlines(inlines, width.saturating_sub(2), theme, false);
            with_prefix(body, Span::styled("│ ", theme.border()))
        }
        Block::Rule => vec![Line::styled("─".repeat(width), theme.border())],
    }
}

/// Draws a code block inside a rounded border; an open block has no bottom.
fn render_code(
    lang: &str,
    lines: &[String],
    closed: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(1);
    let title = if lang.is_empty() {
        "╭─".to_owned()
    } else {
        format!("╭─ {lang} ")
    };
    let top = format!(
        "{title}{}╮",
        "─".repeat(width.saturating_sub(title.width() + 1))
    );
    let mut out = vec![Line::styled(top, theme.border())];
    for line in lines {
        for (_, segment) in segments(line, inner) {
            let pad = " ".repeat(inner.saturating_sub(segment.width()));
            out.push(Line::from(vec![
                Span::styled("│ ", theme.border()),
                Span::styled(segment, theme.fg(Shade::Accent2)),
                Span::styled(format!("{pad} │"), theme.border()),
            ]));
        }
    }
    if closed {
        out.push(Line::styled(
            format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
            theme.border(),
        ));
    }
    out
}

/// Prepends `first` to the first line and matching blank space to the rest.
fn with_prefix(body: Vec<Line<'static>>, first: Span<'static>) -> Vec<Line<'static>> {
    let indent = " ".repeat(first.content.width());
    body.into_iter()
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 {
                first.clone()
            } else {
                Span::raw(indent.clone())
            };
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Word-wraps styled inlines into lines no wider than `width`.
fn wrap_inlines(
    inlines: &[Inline],
    width: usize,
    theme: &Theme,
    heading: bool,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let base = if heading {
        theme.bold(Shade::Text)
    } else {
        theme.fg(Shade::Text)
    };
    let chars: Vec<(char, Style)> = inlines
        .iter()
        .flat_map(|inline| {
            let style = match inline.style {
                InlineStyle::Plain => base,
                InlineStyle::Code => base.patch(theme.fg(Shade::Accent2)),
                InlineStyle::Strong => base.add_modifier(Modifier::BOLD),
                InlineStyle::Emphasis => base.add_modifier(Modifier::ITALIC),
            };
            inline.text.chars().map(move |ch| (ch, style))
        })
        .collect();
    let mut lines = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_width = 0;
    let mut last_space: Option<usize> = None;
    for (ch, style) in chars {
        if ch == '\n' {
            lines.push(to_line(std::mem::take(&mut current)));
            current_width = 0;
            last_space = None;
            continue;
        }
        let ch_width = ch.width().unwrap_or(0);
        if current_width + ch_width > width && !current.is_empty() {
            let rest = match last_space {
                Some(at) => {
                    let rest = current.split_off(at + 1);
                    current.pop();
                    rest
                }
                None => Vec::new(),
            };
            lines.push(to_line(std::mem::replace(&mut current, rest)));
            current_width = current.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
            last_space = None;
        }
        if ch == ' ' && current.is_empty() {
            continue;
        }
        if ch == ' ' {
            last_space = Some(current.len());
        }
        current.push((ch, style));
        current_width += ch_width;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(to_line(current));
    }
    lines
}

/// Groups styled characters into spans.
fn to_line(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for (ch, ch_style) in chars {
        if let Some(current) = style
            && current != ch_style
        {
            spans.push(Span::styled(std::mem::take(&mut text), current));
        }
        style = Some(ch_style);
        text.push(ch);
    }
    if let Some(current) = style {
        spans.push(Span::styled(text, current));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "# Plan\n\n1. [ ] Add the tui module\n2. [x] Wire chat\n\nSome *emphasis*, **strong** and `code` in a paragraph that is long enough to wrap around.\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n- bullet one\n- bullet two\n\n> quoted\n\n---\n";

    fn joined(doc: &mut MarkdownDoc, width: u16) -> String {
        doc.lines(width, &Theme::mono())
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_split_point_yields_same_text_and_monotone_frozen() {
        let mut whole = MarkdownDoc::new();
        whole.push(SOURCE);
        whole.finish();
        let expected = joined(&mut whole, 40);
        for (at, _) in SOURCE.char_indices() {
            let mut doc = MarkdownDoc::new();
            doc.push(&SOURCE[..at]);
            let frozen_before = doc.frozen().to_vec();
            assert_eq!(doc.text(), &SOURCE[..at]);
            doc.push(&SOURCE[at..]);
            assert_eq!(doc.text(), SOURCE);
            assert!(doc.frozen_len() >= frozen_before.len(), "split at {at}");
            assert_eq!(
                &doc.frozen()[..frozen_before.len()],
                &frozen_before[..],
                "split at {at}"
            );
            doc.finish();
            assert_eq!(joined(&mut doc, 40), expected, "split at {at}");
        }
    }

    #[test]
    fn list_items_freeze_one_by_one_as_they_complete() {
        let mut doc = MarkdownDoc::new();
        doc.push("1. [ ] first\n2. [ ] sec");
        assert_eq!(doc.frozen_len(), 1);
        doc.push("ond\n3. [ ] third\n");
        assert_eq!(doc.frozen_len(), 3);
        assert!(matches!(
            &doc.frozen()[1],
            Block::Item {
                marker: Marker::Number(2),
                checkbox: Some(false),
                ..
            }
        ));
    }

    #[test]
    fn fenced_code_gets_a_border() {
        let mut doc = MarkdownDoc::new();
        doc.push("Before\n\n```rust\nfn main() {}\nlet long_line = \"this line is wider than the box\";\n```\nAfter\n");
        doc.finish();
        insta::assert_snapshot!(joined(&mut doc, 32));
    }

    #[test]
    fn open_fence_has_no_bottom_border_until_closed() {
        let mut doc = MarkdownDoc::new();
        doc.push("```\nlet x = 1;\n");
        let open = joined(&mut doc, 20);
        assert!(open.starts_with("╭─"));
        assert!(!open.contains('╰'));
        doc.push("```\n");
        assert!(joined(&mut doc, 20).contains('╰'));
        assert_eq!(doc.frozen_len(), 1);
    }

    #[test]
    fn headings_bullets_checkboxes_inline_code() {
        let mut doc = MarkdownDoc::new();
        doc.push(SOURCE);
        doc.finish();
        insta::assert_snapshot!(joined(&mut doc, 40));
    }

    #[test]
    fn nested_lists_keep_their_depth() {
        let blocks = parse_blocks("- outer\n  - inner\n- second\n");
        let depths: Vec<usize> = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Item { depth, .. } => Some(*depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 0]);
    }
}
