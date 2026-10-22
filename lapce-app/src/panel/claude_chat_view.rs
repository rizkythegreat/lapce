use std::{
    rc::Rc,
    sync::{Arc, atomic::AtomicU64},
    time::SystemTime,
};

use floem::{
    IntoView, View,
    event::EventListener,
    kurbo::Rect,
    reactive::{
        ReadSignal, SignalGet, SignalUpdate, SignalWith, create_effect, create_memo,
        create_rw_signal,
    },
    style::{CursorStyle, Style},
    views::{
        Decorators, container, dyn_stack, empty, label, rich_text, scroll, stack,
    },
};

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    claude_chat::{
        ChatMessage, ChatRole, ClaudeChatData, SessionInfo, SlashCommand,
    },
    config::{LapceConfig, color::LapceColor},
    doc::relative_time,
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
    let show_sessions = chat.show_sessions;
    let is_focused = move || focus.get() == Focus::Panel(PanelKind::ClaudeChat);
    let content_height = create_rw_signal(0.0);

    {
        let chat = chat.clone();
        create_effect(move |_| {
            let doc = chat.editor.doc_signal().get();
            let slash = doc.buffer.with(|b| b.to_string().starts_with('/'));
            chat.command_index.set(0);
            if slash && chat.commands.with_untracked(|c| c.is_empty()) {
                chat.request_commands();
            }
        });
    }

    stack((
        header(chat.clone(), config),
        sessions_view(chat.clone(), config),
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
        .style(move |s| {
            s.width_full()
                .flex_grow(1.0f32)
                .flex_basis(0.0)
                .apply_if(show_sessions.get(), |s| s.hide())
        }),
        label(|| "Claude is working".to_string()).style(move |s| {
            s.padding_horiz(10.0)
                .color(config.get().color(LapceColor::EDITOR_DIM))
                .apply_if(!busy.get(), |s| s.hide())
        }),
        command_popup(chat.clone(), config),
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

fn header(chat: ClaudeChatData, config: ReadSignal<Arc<LapceConfig>>) -> impl View {
    let show_sessions = chat.show_sessions;
    let chat2 = chat.clone();
    stack((
        text_button(|| "New chat", move || chat.new_session(), config),
        text_button(
            move || if show_sessions.get() { "Back" } else { "Sessions" },
            move || {
                if show_sessions.get_untracked() {
                    show_sessions.set(false);
                } else {
                    chat2.open_sessions();
                }
            },
            config,
        ),
    ))
    .style(move |s| {
        s.width_full()
            .padding(4.0)
            .gap(4.0)
            .border_bottom(1.0)
            .border_color(config.get().color(LapceColor::LAPCE_BORDER))
    })
}

fn text_button(
    text: impl Fn() -> &'static str + 'static,
    on_click: impl Fn() + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    label(move || text().to_string())
        .on_click_stop(move |_| on_click())
        .style(move |s| {
            let config = config.get();
            s.padding_horiz(8.0)
                .border_radius(4.0)
                .hover(|s| {
                    s.cursor(CursorStyle::Pointer).background(
                        config.color(LapceColor::PANEL_HOVERED_BACKGROUND),
                    )
                })
                .selectable(false)
        })
}

fn command_popup(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let matches = {
        let chat = chat.clone();
        create_memo(move |_| chat.command_matches())
    };
    dyn_stack(
        move || matches.get().into_iter().enumerate().collect::<Vec<_>>(),
        |(i, command)| (*i, command.name.clone()),
        move |(i, command)| command_row(i, command, chat.clone(), config),
    )
    .style(move |s| {
        s.flex_col()
            .margin_horiz(10.0)
            .padding_vert(4.0)
            .border(1.0)
            .border_radius(6.0)
            .border_color(config.get().color(LapceColor::LAPCE_BORDER))
            .apply_if(matches.with(|m| m.is_empty()), |s| s.hide())
    })
}

fn command_row(
    i: usize,
    command: SlashCommand,
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let SlashCommand {
        name,
        description,
        hint,
    } = command;
    let index = chat.command_index;
    let focus = chat.common.focus;
    let dim = move |s: Style| {
        s.margin_left(8.0).color(config.get().color(LapceColor::EDITOR_DIM))
    };
    stack((
        label({
            let name = name.clone();
            move || format!("/{name}")
        }),
        label(move || hint.clone()).style(dim),
        label(move || description.clone()).style(move |s| {
            dim(s)
                .text_ellipsis()
                .flex_grow(1.0f32)
                .flex_basis(0.0)
                .min_width(0.0)
        }),
    ))
    .on_click_stop(move |_| {
        focus.set(Focus::Panel(PanelKind::ClaudeChat));
        chat.complete_command(&name);
    })
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .padding_horiz(8.0)
            .items_center()
            .apply_if(index.get() == i, |s| {
                s.background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
            })
            .hover(|s| {
                s.cursor(CursorStyle::Pointer).background(
                    config.color(LapceColor::PANEL_HOVERED_BACKGROUND),
                )
            })
            .selectable(false)
    })
}

fn sessions_view(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let sessions = chat.sessions;
    let show_sessions = chat.show_sessions;
    stack((
        label(|| "No sessions for this workspace".to_string()).style(move |s| {
            s.padding(10.0)
                .color(config.get().color(LapceColor::EDITOR_DIM))
                .apply_if(!sessions.with(|list| list.is_empty()), |s| s.hide())
        }),
        scroll(
            dyn_stack(
                move || sessions.get(),
                |session| session.id.clone(),
                move |session| session_row(session, chat.clone(), config),
            )
            .style(|s| s.flex_col().width_full()),
        )
        .style(|s| s.absolute().size_pct(100.0, 100.0)),
    ))
    .style(move |s| {
        s.width_full()
            .flex_grow(1.0f32)
            .flex_basis(0.0)
            .apply_if(!show_sessions.get(), |s| s.hide())
    })
}

fn session_row(
    session: SessionInfo,
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let age = SystemTime::now()
        .duration_since(session.modified)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let age = relative_time(age);
    let title = session.title;
    let id = session.id;
    let current = chat.session_id;
    let is_current = {
        let id = id.clone();
        move || current.with(|current| current.as_ref() == Some(&id))
    };
    stack((
        label(move || title.clone()).style(|s| {
            s.text_ellipsis()
                .flex_grow(1.0f32)
                .flex_basis(0.0)
                .min_width(0.0)
        }),
        label(move || age.clone()).style(move |s| {
            s.margin_left(10.0).color(config.get().color(LapceColor::EDITOR_DIM))
        }),
    ))
    .on_click_stop(move |_| chat.resume(id.clone()))
    .style(move |s| {
        let config = config.get();
        s.width_full()
            .padding_horiz(10.0)
            .padding_vert(4.0)
            .items_center()
            .apply_if(is_current(), |s| {
                s.background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
            })
            .hover(|s| {
                s.cursor(CursorStyle::Pointer).background(
                    config.color(LapceColor::PANEL_HOVERED_BACKGROUND),
                )
            })
            .selectable(false)
    })
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
