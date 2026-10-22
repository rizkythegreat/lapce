use std::ops::Range;

use floem::{
    IntoView, View,
    kurbo::Point,
    peniko::Color,
    taffy::{
        prelude::{auto, length, minmax},
        style::TrackSizingFunction,
    },
    text::{
        Attrs, AttrsList, FamilyOwned, LineHeightValue, Style, TextLayout, Weight,
    },
    views::{Decorators, container, rich_text, stack, stack_from_iter},
};
use lapce_core::{language::LapceLanguage, syntax::Syntax};
use lapce_xi_rope::Rope;
use lsp_types::MarkedString;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use smallvec::SmallVec;

use crate::config::{LapceConfig, color::LapceColor};

/// A link or inline code in a text layout, by byte range of the layout's
/// text with lines joined by `\n`. Inline code may name a file, its target
/// is the code itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub range: Range<usize>,
    pub target: String,
}

#[derive(Clone)]
pub enum MarkdownContent {
    Text(TextLayout, Vec<Link>),
    /// A list item's text, its bullet or number kept apart so wrapped lines
    /// line up after it. `depth` is 0 in a top level list.
    ListItem {
        marker: TextLayout,
        depth: usize,
        layout: TextLayout,
        links: Vec<Link>,
    },
    /// Highlighted code, `language` is the fence info's first word
    CodeBlock {
        layout: TextLayout,
        language: String,
        code: String,
    },
    /// Cells by row, the first row is the header
    Table(Vec<Vec<TextLayout>>),
    Image { url: String, title: String },
    Separator,
}

/// Text collected for the next item, flushed at item and block boundaries
struct Builder<'a> {
    res: Vec<MarkdownContent>,
    text: String,
    attrs: AttrsList,
    default_attrs: Attrs<'a>,
    links: Vec<Link>,
    /// Marker and depth of the list item the text belongs to
    item: Option<(String, usize)>,
}

impl<'a> Builder<'a> {
    fn new(default_attrs: Attrs<'a>) -> Self {
        Self {
            res: Vec::new(),
            text: String::new(),
            attrs: AttrsList::new(default_attrs.clone()),
            default_attrs,
            links: Vec::new(),
            item: None,
        }
    }

    /// The collected text without trailing line breaks and its links, None
    /// when empty
    fn take_layout(&mut self) -> Option<(TextLayout, Vec<Link>)> {
        let attrs = std::mem::replace(
            &mut self.attrs,
            AttrsList::new(self.default_attrs.clone()),
        );
        let links = std::mem::take(&mut self.links);
        let text = std::mem::take(&mut self.text);
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            return None;
        }
        let mut layout = TextLayout::new();
        layout.set_text(text, attrs, None);
        Some((layout, links))
    }

    fn flush(&mut self) {
        let Some((layout, links)) = self.take_layout() else {
            return;
        };
        let content = match self.item.take() {
            Some((marker, depth)) => {
                let mut marker_layout = TextLayout::new();
                marker_layout.set_text(
                    &marker,
                    AttrsList::new(self.default_attrs.clone()),
                    None,
                );
                MarkdownContent::ListItem {
                    marker: marker_layout,
                    depth,
                    layout,
                    links,
                }
            }
            None => MarkdownContent::Text(layout, links),
        };
        self.res.push(content);
    }
}

pub fn parse_markdown(
    text: &str,
    line_height: f64,
    config: &LapceConfig,
) -> Vec<MarkdownContent> {
    let code_font_family: Vec<FamilyOwned> =
        FamilyOwned::parse_list(&config.editor.font_family).collect();

    let default_attrs = Attrs::new()
        .color(config.color(LapceColor::EDITOR_FOREGROUND))
        .font_size(config.ui.font_size() as f32)
        .line_height(LineHeightValue::Normal(line_height as f32));
    // Attrs has no background, so inline code stands out by color instead
    let inline_code_color = config
        .style_color("string")
        .unwrap_or_else(|| config.color(LapceColor::EDITOR_FOREGROUND));
    let mut out = Builder::new(default_attrs.clone());

    let mut tag_stack: SmallVec<[(usize, Tag); 4]> = SmallVec::new();

    let parser = Parser::new_ext(
        text,
        Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_HEADING_ATTRIBUTES,
    );
    // Line breaks owed to the next entry, 2 leaves a blank line between blocks.
    // Deferred so that we don't emit newlines at the very end of the generation
    let mut add_newlines = 0;
    // Next number of each open list, None for bullet lists
    let mut lists: SmallVec<[Option<u64>; 4]> = SmallVec::new();
    // Rows of the table being read
    let mut table: Vec<Vec<TextLayout>> = Vec::new();
    for event in parser {
        // Add the newlines since we're going to be outputting more. None at the
        // start of an item, and code blocks already end with one.
        if !out.text.is_empty() {
            let trailing =
                out.text.len() - out.text.trim_end_matches('\n').len();
            for _ in trailing..add_newlines {
                out.text.push('\n');
            }
        }
        add_newlines = 0;

        match event {
            Event::Start(tag) => {
                match &tag {
                    Tag::List(start) => lists.push(*start),
                    Tag::Item => {
                        // Ends the text before, such as the parent item's
                        out.flush();
                        let marker = match lists.last_mut() {
                            Some(Some(n)) => {
                                *n += 1;
                                format!("{}.", *n - 1)
                            }
                            _ => "•".to_string(),
                        };
                        out.item = Some((marker, lists.len().saturating_sub(1)));
                    }
                    Tag::Table(_) => {
                        out.flush();
                        table.clear();
                    }
                    Tag::TableHead | Tag::TableRow => table.push(Vec::new()),
                    _ => {}
                }
                tag_stack.push((out.text.len(), tag));
            }
            Event::End(end_tag) => {
                let Some((start_offset, tag)) = tag_stack.pop() else {
                    tracing::warn!("Unbalanced markdown tag");
                    continue;
                };
                if end_tag != tag.to_end() {
                    tracing::warn!("Mismatched markdown tag");
                    continue;
                }

                // Tags that end an item of their own
                match &tag {
                    Tag::CodeBlock(kind) => {
                        let code = out.text[start_offset..]
                            .trim_end_matches('\n')
                            .to_string();
                        out.text.truncate(start_offset);
                        out.flush();

                        let language = match kind {
                            CodeBlockKind::Fenced(info) => {
                                info.split_whitespace().next().unwrap_or_default()
                            }
                            CodeBlockKind::Indented => "",
                        };
                        let code_attrs =
                            default_attrs.clone().family(&code_font_family);
                        let mut code_attr_list = AttrsList::new(code_attrs.clone());
                        highlight_as_code(
                            &mut code_attr_list,
                            code_attrs,
                            md_language_to_lapce_language(language),
                            &code,
                            0,
                            config,
                        );
                        let mut layout = TextLayout::new();
                        layout.set_text(&code, code_attr_list, None);
                        out.res.push(MarkdownContent::CodeBlock {
                            layout,
                            language: language.to_string(),
                            code,
                        });
                        continue;
                    }
                    Tag::Item => {
                        out.flush();
                        continue;
                    }
                    Tag::TableCell => {
                        let in_head = tag_stack
                            .iter()
                            .any(|(_, tag)| matches!(tag, Tag::TableHead));
                        if in_head {
                            let bold = default_attrs.clone().weight(Weight::BOLD);
                            out.attrs.add_span(0..out.text.len(), bold);
                        }
                        let cell = out
                            .take_layout()
                            .map(|(layout, _)| layout)
                            .unwrap_or_else(TextLayout::new);
                        if let Some(row) = table.last_mut() {
                            row.push(cell);
                        }
                        continue;
                    }
                    Tag::TableHead | Tag::TableRow => continue,
                    Tag::Table(_) => {
                        let rows = std::mem::take(&mut table);
                        out.res.push(MarkdownContent::Table(rows));
                        continue;
                    }
                    _ => {}
                }

                if let Some(attrs) = attribute_for_tag(
                    default_attrs.clone(),
                    &tag,
                    &code_font_family,
                    config,
                ) {
                    let end = out.text.len().max(start_offset);
                    out.attrs.add_span(start_offset..end, attrs);
                }

                if let Tag::Link { dest_url, .. } = &tag {
                    out.links.push(Link {
                        range: start_offset..out.text.len(),
                        target: dest_url.to_string(),
                    });
                }
                if matches!(tag, Tag::List(_)) {
                    lists.pop();
                }
                let in_item =
                    tag_stack.iter().any(|(_, tag)| matches!(tag, Tag::Item));
                add_newlines = add_newlines.max(newlines_after_tag(&tag, in_item));

                // TODO: Are there any link types that would change how the
                // image is rendered?
                if let Tag::Image {
                    dest_url, title, ..
                } = &tag
                {
                    out.flush();
                    out.res.push(MarkdownContent::Image {
                        url: dest_url.to_string(),
                        title: title.to_string(),
                    });
                }
            }
            Event::Text(text) => {
                if let Some((_, tag)) = tag_stack.last()
                    && should_skip_text_in_tag(tag)
                {
                    continue;
                }
                out.text.push_str(&text);
            }
            Event::Code(text) => {
                let start = out.text.len();
                out.text.push_str(&text);
                out.links.push(Link {
                    range: start..out.text.len(),
                    target: text.to_string(),
                });
                out.attrs.add_span(
                    start..out.text.len(),
                    default_attrs
                        .clone()
                        .family(&code_font_family)
                        .color(inline_code_color),
                );
            }
            // TODO: Some minimal 'parsing' of html could be useful here, since some things use
            // basic html like `<code>text</code>`.
            Event::Html(text) => {
                let start = out.text.len();
                out.text.push_str(&text);
                out.attrs.add_span(
                    start..out.text.len(),
                    default_attrs
                        .clone()
                        .family(&code_font_family)
                        .color(config.color(LapceColor::MARKDOWN_BLOCKQUOTE)),
                );
            }
            Event::HardBreak => out.text.push('\n'),
            Event::SoftBreak => out.text.push(' '),
            Event::Rule => {}
            Event::FootnoteReference(_text) => {}
            Event::TaskListMarker(checked) => {
                out.text.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::InlineHtml(_) => {} // TODO(panekj): Implement
            Event::InlineMath(_) => {} // TODO(panekj): Implement
            Event::DisplayMath(_) => {} // TODO(panekj): Implement
        }
    }

    out.flush();
    out.res
}

/// Marker then text, so wrapped lines line up after the marker. `text` is
/// given its share of the width, it should fill it.
pub fn list_item_view(
    marker: TextLayout,
    depth: usize,
    text: impl IntoView + 'static,
) -> impl View {
    stack((
        rich_text(move || marker.clone())
            .style(|s| s.min_width(14.0).margin_right(6.0)),
        container(text)
            .style(|s| s.flex_grow(1.0f32).flex_basis(0.0).min_width(0.0)),
    ))
    .style(move |s| s.width_full().padding_left(depth as f64 * 20.0))
}

/// The link under `point`, relative to the layout's origin
pub fn link_at<'a>(
    layout: &TextLayout,
    links: &'a [Link],
    point: Point,
) -> Option<&'a Link> {
    if links.is_empty() {
        return None;
    }
    let hit = layout.hit_point(point);
    if !hit.is_inside {
        return None;
    }
    let line_start: usize = layout.lines()[..hit.line]
        .iter()
        .map(|line| line.text().len() + 1)
        .sum();
    let offset = line_start + hit.index;
    links.iter().find(|link| link.range.contains(&offset))
}

/// Cells on a grid, each column as wide as its content allows, rows ruled
pub fn table_view(rows: Vec<Vec<TextLayout>>, border: Color) -> impl View {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let cells = rows.into_iter().flat_map(move |row| {
        let missing = columns - row.len();
        row.into_iter()
            .chain(std::iter::repeat_with(TextLayout::new).take(missing))
    });
    // Cells measure at their unwrapped width, a zero minimum lets columns
    // shrink to the available width so the cells wrap instead of overflowing
    let tracks: Vec<TrackSizingFunction> =
        vec![minmax(length(0.0_f32), auto()); columns];
    stack_from_iter(cells.map(move |cell| {
        rich_text(move || cell.clone()).style(move |s| {
            s.padding_horiz(8.0)
                .padding_vert(3.0)
                .border_bottom(1.0)
                .border_color(border)
        })
    }))
    .style(move |s| {
        s.grid()
            .grid_template_columns(tracks.clone())
            .max_width_full()
    })
}

fn attribute_for_tag<'a>(
    default_attrs: Attrs<'a>,
    tag: &Tag,
    code_font_family: &'a [FamilyOwned],
    config: &LapceConfig,
) -> Option<Attrs<'a>> {
    use pulldown_cmark::HeadingLevel;
    match tag {
        Tag::Heading {
            level,
            id: _,
            classes: _,
            attrs: _,
        } => {
            // Smaller than the em values of
            // https://drafts.csswg.org/css2/#html-stylesheet, those crowd
            // narrow panels and popups
            let font_scale = match level {
                HeadingLevel::H1 => 1.4,
                HeadingLevel::H2 => 1.25,
                HeadingLevel::H3 => 1.1,
                _ => 1.0,
            };
            let font_size = font_scale * config.ui.font_size() as f64;
            Some(
                default_attrs
                    .font_size(font_size as f32)
                    .weight(Weight::BOLD),
            )
        }
        Tag::BlockQuote(_block_quote) => Some(
            default_attrs
                .style(Style::Italic)
                .color(config.color(LapceColor::MARKDOWN_BLOCKQUOTE)),
        ),
        Tag::CodeBlock(_) => Some(default_attrs.family(code_font_family)),
        Tag::Emphasis => Some(default_attrs.style(Style::Italic)),
        Tag::Strong => Some(default_attrs.weight(Weight::BOLD)),
        // TODO: Strikethrough support
        Tag::Link {
            link_type: _,
            dest_url: _,
            title: _,
            id: _,
        } => {
            // TODO: Link support
            Some(default_attrs.color(config.color(LapceColor::EDITOR_LINK)))
        }
        // All other tags are currently ignored
        _ => None,
    }
}

/// Line breaks after a specific markdown tag: none inline, a blank line after
/// blocks, a single break between list items and inside them
fn newlines_after_tag(tag: &Tag, in_item: bool) -> usize {
    match tag {
        Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } => 0,
        Tag::Paragraph
        | Tag::Heading { .. }
        | Tag::CodeBlock(_)
        | Tag::List(_)
        | Tag::BlockQuote(_)
        | Tag::Table(_)
            if !in_item =>
        {
            2
        }
        _ => 1,
    }
}

/// Whether it should skip the text node after a specific tag  
/// For example, images are skipped because it emits their title as a separate text node.  
fn should_skip_text_in_tag(tag: &Tag) -> bool {
    matches!(tag, Tag::Image { .. })
}

fn md_language_to_lapce_language(lang: &str) -> Option<LapceLanguage> {
    // TODO: There are many other names commonly used that should be supported
    LapceLanguage::from_name(lang)
}

/// Highlight the text in a richtext builder like it was a markdown codeblock
pub fn highlight_as_code(
    attr_list: &mut AttrsList,
    default_attrs: Attrs,
    language: Option<LapceLanguage>,
    text: &str,
    start_offset: usize,
    config: &LapceConfig,
) {
    let syntax = language.map(Syntax::from_language);

    let styles = syntax
        .map(|mut syntax| {
            syntax.parse(0, Rope::from(text), None);
            syntax.styles
        })
        .unwrap_or(None);

    if let Some(styles) = styles {
        for (range, style) in styles.iter() {
            if let Some(color) = style
                .fg_color
                .as_ref()
                .and_then(|fg| config.style_color(fg))
            {
                attr_list.add_span(
                    start_offset + range.start..start_offset + range.end,
                    default_attrs.clone().color(color),
                );
            }
        }
    }
}

pub fn from_marked_string(
    text: MarkedString,
    config: &LapceConfig,
) -> Vec<MarkdownContent> {
    match text {
        MarkedString::String(text) => parse_markdown(&text, 1.8, config),
        // This is a short version of a code block
        MarkedString::LanguageString(code) => {
            // TODO: We could simply construct the MarkdownText directly
            // Simply construct the string as if it was written directly
            parse_markdown(
                &format!("```{}\n{}\n```", code.language, code.value),
                1.8,
                config,
            )
        }
    }
}

pub fn from_plaintext(
    text: &str,
    line_height: f64,
    config: &LapceConfig,
) -> Vec<MarkdownContent> {
    let mut text_layout = TextLayout::new();
    text_layout.set_text(
        text,
        AttrsList::new(
            Attrs::new()
                .font_size(config.ui.font_size() as f32)
                .line_height(LineHeightValue::Normal(line_height as f32)),
        ),
        None,
    );
    vec![MarkdownContent::Text(text_layout, Vec::new())]
}

#[cfg(test)]
mod tests {
    use floem::text::TextLayout;

    use super::{MarkdownContent, parse_markdown};
    use crate::config::LapceConfig;

    fn text(layout: &TextLayout) -> String {
        let lines: Vec<_> = layout.lines().iter().map(|line| line.text()).collect();
        lines.join("\n")
    }

    /// One line per item: list items indented with their marker, code blocks
    /// as their fence and table rows with cells split by `|`
    fn rendered(markdown: &str) -> String {
        let config = LapceConfig::default_lapce_config();
        let items: Vec<_> = parse_markdown(markdown, 1.5, &config)
            .iter()
            .map(|content| match content {
                MarkdownContent::Text(layout, _) => text(layout),
                MarkdownContent::ListItem {
                    marker,
                    depth,
                    layout,
                    ..
                } => {
                    let indent = "  ".repeat(*depth);
                    format!("{indent}{} {}", text(marker), text(layout))
                }
                MarkdownContent::CodeBlock { language, code, .. } => {
                    format!("```{language} {code}")
                }
                MarkdownContent::Table(rows) => {
                    let rows: Vec<_> = rows
                        .iter()
                        .map(|row| {
                            row.iter().map(text).collect::<Vec<_>>().join("|")
                        })
                        .collect();
                    rows.join("\n")
                }
                MarkdownContent::Image { .. } => "image".to_string(),
                MarkdownContent::Separator => "---".to_string(),
            })
            .collect();
        items.join("\n")
    }

    #[test]
    fn paragraphs_keep_a_blank_line() {
        assert_eq!(rendered("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn list_items_are_their_own_items() {
        assert_eq!(rendered("intro\n\n- x\n- y\n\nz"), "intro\n• x\n• y\nz");
        assert_eq!(rendered("3. x\n4. y"), "3. x\n4. y");
        assert_eq!(rendered("- x\n  - y\n- z"), "• x\n  • y\n• z");
        assert_eq!(rendered("- [x] done"), "• [x] done");
        // Loose items, paragraphs inside one item stay together
        assert_eq!(rendered("- a\n\n  b\n\n- c"), "• a\nb\n• c");
    }

    #[test]
    fn code_block_is_its_own_item() {
        let markdown = "a\n\n```rust title\nfn x() {}\n```\n\nb";
        assert_eq!(rendered(markdown), "a\n```rust fn x() {}\nb");
    }

    #[test]
    fn table_rows_and_cells() {
        let markdown = "| a | b |\n|---|---|\n| 1 | `2` |\n| 3 |\n\nafter";
        assert_eq!(rendered(markdown), "a|b\n1|2\n3|\nafter");
    }

    #[test]
    fn links_and_inline_code() {
        let config = LapceConfig::default_lapce_config();
        let markdown = "See [docs](https://a.dev) and `src/main.rs:3`";
        let content = parse_markdown(markdown, 1.5, &config);
        let [MarkdownContent::Text(_, links)] = content.as_slice() else {
            panic!("expected one text");
        };
        let links: Vec<_> = links
            .iter()
            .map(|link| (link.range.clone(), link.target.as_str()))
            .collect();
        assert_eq!(
            links,
            [(4..8, "https://a.dev"), (13..26, "src/main.rs:3")]
        );
    }
}
