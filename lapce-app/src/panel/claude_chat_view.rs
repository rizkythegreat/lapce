use std::{
    rc::Rc,
    sync::{Arc, atomic::AtomicU64},
    time::SystemTime,
};

use floem::{
    IntoView, View,
    event::EventListener,
    kurbo::Rect,
    peniko::Color,
    reactive::{
        ReadSignal, SignalGet, SignalUpdate, SignalWith, create_effect, create_memo,
        create_rw_signal,
    },
    style::{CursorStyle, Style},
    text::{Attrs, AttrsList, LineHeightValue, TextLayout},
    views::{
        Decorators, container, dyn_stack, empty, label, rich_text, scroll, stack,
    },
};

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    claude_chat::{
        ChatMessage, ChatRole, ClaudeChatData, PermissionDecision, PermissionRequest,
        PopupItem, SessionInfo, ToolCall, mention_query,
    },
    command::InternalCommand,
    config::{LapceConfig, color::LapceColor},
    doc::relative_time,
    listener::Listener,
    markdown::{MarkdownContent, parse_markdown},
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
    let suggestion = chat.suggestion;
    let internal_command = chat.common.internal_command;
    let is_focused = move || focus.get() == Focus::Panel(PanelKind::ClaudeChat);
    let content_height = create_rw_signal(0.0);
    // Scroll content is unbounded, give it the visible width so text wraps
    let scroll_width = create_rw_signal(0.0);

    {
        let chat = chat.clone();
        create_effect(move |was_mention: Option<bool>| {
            let doc = chat.editor.doc_signal().get();
            let input = doc.buffer.with(|b| b.to_string());
            chat.popup_index.set(0);
            if input.starts_with('/')
                && chat.commands.with_untracked(|c| c.is_empty())
            {
                chat.request_commands();
            }
            // Fresh file list each time a new mention starts
            let mention = mention_query(&input).is_some();
            if mention && was_mention != Some(true) {
                chat.request_files();
            }
            mention
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
                    move |msg| message_view(msg, internal_command, config),
                )
                .on_resize(move |rect| content_height.set(rect.height()))
                .style(move |s| {
                    s.flex_col().width(scroll_width.get()).padding(10.0).gap(10.0)
                }),
            )
            // Follow the conversation as it grows
            .ensure_visible(move || {
                let height = content_height.get();
                Rect::new(0.0, height - 1.0, 1.0, height)
            })
            .on_resize(move |rect| {
                if scroll_width.get_untracked() != rect.width() {
                    scroll_width.set(rect.width());
                }
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
        permission_view(chat.clone(), config),
        command_popup(chat.clone(), config),
        stack((
            TextInputBuilder::new()
                .is_focused(is_focused)
                .build_editor(chat.editor.clone())
                .placeholder(move || match suggestion.get() {
                    Some(suggestion) => format!("{suggestion}  (Enter to use)"),
                    None => "Ask Claude".to_string(),
                })
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
    let skip = chat.skip_permissions;
    let chat2 = chat.clone();
    stack((
        text_button(|| "New chat", move || chat.new_session(), config),
        // Applies from the next message, the process restarts on the same session
        container(text_button(
            move || if skip.get() { "Permissions: skip" } else { "Permissions: ask" },
            move || skip.update(|skip| *skip = !*skip),
            config,
        ))
        .style(move |s| {
            let error = config.get().color(LapceColor::LAPCE_ERROR);
            s.apply_if(skip.get(), |s| s.color(error))
        }),
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

/// Wrapping plain text in the UI font
fn plain_text(text: &str, color: Color, config: &LapceConfig) -> TextLayout {
    let attrs = Attrs::new()
        .color(color)
        .font_size(config.ui.font_size() as f32)
        .line_height(LineHeightValue::Normal(1.8));
    let mut layout = TextLayout::new();
    layout.set_text(text, AttrsList::new(attrs), None);
    layout
}

/// Approval card for the oldest pending tool call
fn permission_view(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let permissions = chat.permissions;
    dyn_stack(
        move || Vec::from_iter(permissions.with(|p| p.front().cloned())),
        |request| request.request_id.clone(),
        move |request| permission_card(request, chat.clone(), config),
    )
    .style(|s| s.width_full())
}

fn permission_card(
    request: PermissionRequest,
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let title = request.title.clone();
    let detail = request.detail.clone();
    let button = |text: &'static str, decision: PermissionDecision| {
        let chat = chat.clone();
        let id = request.request_id.clone();
        text_button(
            move || text,
            move || chat.answer_permission(&id, decision),
            config,
        )
    };
    let always = button("Always allow", PermissionDecision::AlwaysAllow);
    let can_always_allow = request.can_always_allow();
    stack((
        label(move || title.clone()).style(|s| s.font_bold()),
        rich_text(move || {
            let config = config.get();
            let color = config.color(LapceColor::EDITOR_FOREGROUND);
            plain_text(&detail, color, &config)
        })
        .style(|s| s.width_full().margin_vert(6.0)),
        stack((
            button("Allow", PermissionDecision::Allow),
            container(always)
                .style(move |s| s.apply_if(!can_always_allow, |s| s.hide())),
            button("Deny", PermissionDecision::Deny),
        ))
        .style(|s| s.gap(4.0)),
    ))
    .style(move |s| {
        let config = config.get();
        s.flex_col()
            .margin_horiz(10.0)
            .margin_bottom(6.0)
            .padding(8.0)
            .border(1.0)
            .border_radius(6.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
    })
}

fn command_popup(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let matches = {
        let chat = chat.clone();
        create_memo(move |_| chat.popup_items())
    };
    dyn_stack(
        move || matches.get().into_iter().enumerate().collect::<Vec<_>>(),
        |(i, item)| (*i, item.label.clone()),
        move |(i, item)| popup_row(i, item, chat.clone(), config),
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

fn popup_row(
    i: usize,
    item: PopupItem,
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let PopupItem {
        label: text,
        description,
        hint,
        ..
    } = item.clone();
    let index = chat.popup_index;
    let focus = chat.common.focus;
    let dim = move |s: Style| {
        s.margin_left(8.0).color(config.get().color(LapceColor::EDITOR_DIM))
    };
    stack((
        label(move || text.clone()),
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
        chat.apply_item(&item);
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
    let scroll_width = create_rw_signal(0.0);
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
            .style(move |s| s.flex_col().width(scroll_width.get())),
        )
        .on_resize(move |rect| {
            if scroll_width.get_untracked() != rect.width() {
                scroll_width.set(rect.width());
            }
        })
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
    internal_command: Listener<InternalCommand>,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let role = msg.role;
    let text = msg.text;
    let diff = msg.tool.as_ref().and_then(|tool| tool.diff.clone());
    let id = AtomicU64::new(0);
    let content = dyn_stack(
        move || {
            let config = config.get();
            text.with(|text| match role {
                ChatRole::User | ChatRole::Assistant => {
                    parse_markdown(text, 1.8, &config)
                }
                ChatRole::Tool => {
                    let color = config.color(LapceColor::EDITOR_DIM);
                    let summary = plain_text(text, color, &config);
                    let mut content = vec![MarkdownContent::Text(summary)];
                    if let Some(diff) = diff.as_ref() {
                        let diff = format!("````diff\n{diff}\n````");
                        content.extend(parse_markdown(&diff, 1.5, &config));
                    }
                    content
                }
                ChatRole::Error => {
                    let color = config.color(LapceColor::LAPCE_ERROR);
                    vec![MarkdownContent::Text(plain_text(text, color, &config))]
                }
            })
        },
        move |_| id.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        move |content| match content {
            MarkdownContent::Text(text_layout) => {
                container(
                    rich_text(move || text_layout.clone())
                        .style(|s| s.width_full()),
                )
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
    .style(|s| s.flex_col().width_full());

    let actions = tool_actions(msg.tool, internal_command, config);
    stack((content, actions)).style(move |s| {
        let config = config.get();
        let user_bg = config.color(LapceColor::PANEL_CURRENT_BACKGROUND);
        s.flex_col()
            .width_full()
            .apply_if(role == ChatRole::User, |s| {
                s.padding(8.0).border_radius(6.0).background(user_bg)
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

/// "Open" and "Changes" buttons for tool calls that touch a file
fn tool_actions(
    tool: Option<ToolCall>,
    internal_command: Listener<InternalCommand>,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let file = tool.as_ref().and_then(|tool| tool.file.clone());
    let edits_file = tool.as_ref().is_some_and(|tool| tool.edits_file);
    let has_file = file.is_some();
    let open = {
        let file = file.clone();
        text_button(
            || "Open",
            move || {
                if let Some(path) = file.clone() {
                    internal_command.send(InternalCommand::OpenFile { path });
                }
            },
            config,
        )
    };
    // Same diff against HEAD as the source control panel
    let changes = text_button(
        || "Changes",
        move || {
            if let Some(path) = file.clone() {
                internal_command.send(InternalCommand::OpenFileChanges { path });
            }
        },
        config,
    );
    stack((
        open,
        container(changes).style(move |s| s.apply_if(!edits_file, |s| s.hide())),
    ))
    .style(move |s| s.gap(4.0).margin_top(2.0).apply_if(!has_file, |s| s.hide()))
}
