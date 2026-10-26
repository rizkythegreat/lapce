use std::{rc::Rc, sync::atomic::AtomicU64};

use floem::{
    View,
    reactive::{SignalGet, SignalWith},
    views::{Decorators, container, dyn_stack, empty, rich_text, scroll},
};
use lapce_core::buffer::rope_text::RopeText;

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    config::color::LapceColor,
    markdown::{MarkdownContent, list_item_view, parse_markdown, table_view},
    window_tab::WindowTabData,
};

fn is_markdown(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e == "markdown")
}

/// Renders the active editor's markdown file, read only
pub fn markdown_preview_panel(
    window_tab_data: Rc<WindowTabData>,
    _position: PanelPosition,
) -> impl View {
    let config = window_tab_data.common.config;
    let panel = window_tab_data.panel.clone();
    let content = move || {
        let config = config.get();
        // Track the panel layout so the preview renders once it's shown
        panel.styles.with(|_| ());
        panel.panels.with(|_| ());
        if !panel.is_panel_visible(&PanelKind::MarkdownPreview) {
            return Vec::new();
        }
        let Some(editor) = window_tab_data.main_split.get_active_editor() else {
            return parse_markdown("No file open", 1.8, &config);
        };
        let doc = editor.doc();
        let path = doc.content.with(|c| c.path().cloned());
        if !path.as_deref().is_some_and(is_markdown) {
            return parse_markdown("Open a Markdown file to preview", 1.8, &config);
        }
        // ponytail: reparses the whole file on every edit, fine for READMEs,
        // debounce if large files lag
        let text = doc.buffer.with(|b| b.text().to_string());
        parse_markdown(&text, 1.8, &config)
    };

    let id = AtomicU64::new(0);
    scroll(
        dyn_stack(
            content,
            move |_| id.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            move |content| match content {
                MarkdownContent::Text(text_layout, _) => container(
                    rich_text(move || text_layout.clone()).style(|s| s.width_full()),
                )
                .style(|s| s.width_full()),
                MarkdownContent::CodeBlock { layout, .. } => container(
                    rich_text(move || layout.clone()).style(|s| s.width_full()),
                )
                .style(|s| s.width_full().padding_vert(4.0)),
                MarkdownContent::ListItem {
                    marker,
                    depth,
                    layout,
                    ..
                } => container(list_item_view(
                    marker,
                    depth,
                    rich_text(move || layout.clone()).style(|s| s.width_full()),
                ))
                .style(|s| s.width_full()),
                MarkdownContent::Table(rows) => {
                    let border =
                        config.get_untracked().color(LapceColor::LAPCE_BORDER);
                    container(table_view(rows, border))
                        .style(|s| s.padding_vert(4.0))
                }
                // ponytail: images not rendered, the hover and README views
                // skip them too
                MarkdownContent::Image { .. } => container(empty()),
                MarkdownContent::Separator => container(empty().style(move |s| {
                    s.width_full()
                        .margin_vert(5.0)
                        .height(1.0)
                        .background(config.get().color(LapceColor::LAPCE_BORDER))
                })),
            },
        )
        .style(|s| s.flex_col().width_full().padding_horiz(16.0).padding_vert(10.0)),
    )
    .style(|s| s.size_pct(100.0, 100.0))
}
