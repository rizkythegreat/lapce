use std::{
    rc::Rc,
    sync::{Arc, atomic::AtomicU64},
};

use floem::{
    IntoView, View,
    event::EventListener,
    kurbo::Rect,
    reactive::{ReadSignal, SignalGet, SignalUpdate, SignalWith, create_rw_signal},
    style::CursorStyle,
    views::{
        Decorators, container, dyn_stack, empty, label, rich_text, scroll, stack,
    },
};

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    claude_chat::{ChatMessage, ChatRole},
    config::{LapceConfig, color::LapceColor},
    markdown::{MarkdownContent, from_plaintext, parse_markdown},
    text_input::TextInputBuilder,
    window_tab::{Focus, WindowTabData},
};

pub fn claude_chat_panel(
    window_tab_data: Rc<WindowTabData>,
    _position: PanelPosition,
) -> impl View {
    let chat = window_tab_data.claude_chat.clone();
    let config = chat.common.config;
    let focus = chat.common.focus;
    let messages = chat.messages;
    let busy = chat.busy;
    let is_focused = move || focus.get() == Focus::Panel(PanelKind::ClaudeChat);
    let content_height = create_rw_signal(0.0);

    stack((
        container(
            scroll(
                dyn_stack(
                    move || messages.get(),
                    |msg| msg.id,
                    move |msg| message_view(msg, config),
                )
                .on_resize(move |rect| content_height.set(rect.height()))
                .style(|s| s.flex_col().width_full().padding(10.0).gap(10.0)),
            )
            // Follow the conversation as it grows
            .ensure_visible(move || {
                let height = content_height.get();
                Rect::new(0.0, height - 1.0, 1.0, height)
            })
            .style(|s| s.absolute().size_pct(100.0, 100.0)),
        )
        .style(|s| s.width_full().flex_grow(1.0f32).flex_basis(0.0)),
        label(|| "Claude is working".to_string()).style(move |s| {
            s.padding_horiz(10.0)
                .color(config.get().color(LapceColor::EDITOR_DIM))
                .apply_if(!busy.get(), |s| s.hide())
        }),
        stack((
            TextInputBuilder::new()
                .is_focused(is_focused)
                .build_editor(chat.editor.clone())
                .placeholder(|| "Ask Claude".to_string())
                .style(|s| s.flex_grow(1.0f32).min_width(0.0)),
            {
                let chat = chat.clone();
                let button_text = move || if busy.get() { "Stop" } else { "Send" };
                label(move || button_text().to_string())
                    .on_click_stop(move |_| {
                        if busy.get_untracked() {
                            chat.stop();
                        } else {
                            chat.send();
                        }
                    })
                    .style(move |s| {
                        let config = config.get();
                        s.padding_horiz(10.0)
                            .border_left(1.0)
                            .border_color(config.color(LapceColor::LAPCE_BORDER))
                            .hover(|s| {
                                s.cursor(CursorStyle::Pointer).background(
                                    config
                                        .color(LapceColor::PANEL_HOVERED_BACKGROUND),
                                )
                            })
                            .selectable(false)
                    })
            },
        ))
        .on_event_cont(EventListener::PointerDown, move |_| {
            focus.set(Focus::Panel(PanelKind::ClaudeChat));
        })
        .style(move |s| {
            s.margin(10.0)
                .items_center()
                .border(1.0)
                .border_radius(6.0)
                .border_color(config.get().color(LapceColor::LAPCE_BORDER))
        }),
    ))
    .style(|s| s.absolute().size_pct(100.0, 100.0).flex_col())
    .debug_name("Claude Chat Panel")
}

fn message_view(
    msg: ChatMessage,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let role = msg.role;
    let text = msg.text;
    let id = AtomicU64::new(0);
    dyn_stack(
        move || {
            let config = config.get();
            text.with(|text| match role {
                ChatRole::User | ChatRole::Assistant => {
                    parse_markdown(text, 1.8, &config)
                }
                ChatRole::Tool | ChatRole::Error => {
                    from_plaintext(text, 1.8, &config)
                }
            })
        },
        move |_| id.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        move |content| match content {
            MarkdownContent::Text(text_layout) => {
                container(rich_text(move || text_layout.clone()))
                    .style(|s| s.width_full())
                    .into_any()
            }
            MarkdownContent::Image { .. } => empty().into_any(),
            MarkdownContent::Separator => empty()
                .style(move |s| {
                    s.width_full()
                        .margin_vert(5.0)
                        .height(1.0)
                        .background(config.get().color(LapceColor::LAPCE_BORDER))
                })
                .into_any(),
        },
    )
    .style(move |s| {
        let config = config.get();
        s.flex_col()
            .width_full()
            .apply_if(role == ChatRole::User, |s| {
                s.padding(8.0)
                    .border_radius(6.0)
                    .background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
            })
            .apply_if(role == ChatRole::Tool, |s| {
                s.padding_left(8.0)
                    .border_left(2.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
            })
            .apply_if(role == ChatRole::Error, |s| {
                s.padding_left(8.0)
                    .border_left(2.0)
                    .border_color(config.color(LapceColor::LAPCE_ERROR))
            })
    })
}
