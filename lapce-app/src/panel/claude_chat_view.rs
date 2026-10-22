use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, SystemTime},
};

use floem::{
    IntoView, View,
    action::exec_after,
    event::{Event, EventListener, EventPropagation},
    kurbo::Rect,
    peniko::Color,
    reactive::{
        ReadSignal, RwSignal, SignalGet, SignalUpdate, SignalWith, create_effect,
        create_memo, create_rw_signal,
    },
    style::{CursorStyle, Style},
    text::{Attrs, AttrsList, FamilyOwned, LineHeightValue, TextLayout},
    views::{
        Decorators, container, dyn_stack,
        editor::{core::register::Clipboard, text::SystemClipboard},
        empty, label, rich_text, scroll, stack, svg,
    },
};

use super::{kind::PanelKind, position::PanelPosition};
use crate::{
    app::clickable_icon,
    claude_chat::{
        ChatMessage, ChatRole, ClaudeChatData, PermissionDecision, PermissionRequest,
        PopupItem, SessionInfo, ToolCall, ToolStatus, UsageLimits, UsageWindow,
        mention_query,
    },
    command::InternalCommand,
    config::{LapceConfig, color::LapceColor, icon::LapceIcons},
    doc::relative_time,
    listener::Listener,
    markdown::{MarkdownContent, list_item_view, parse_markdown, table_view},
    panel::data::PanelData,
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
    let workspace = chat.common.workspace.path.clone();
    let is_focused = move || focus.get() == Focus::Panel(PanelKind::ClaudeChat);
    let content_height = create_rw_signal(0.0);
    // Scroll content is unbounded, give it the visible width so text wraps
    let scroll_width = create_rw_signal(0.0);
    let drop_chat = chat.clone();

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
        header(chat.clone(), window_tab_data.panel.clone(), config),
        sessions_view(chat.clone(), config),
        container(
            scroll(
                dyn_stack(
                    move || messages.get(),
                    |msg| msg.id,
                    move |msg| {
                        message_view(
                            msg,
                            workspace.clone(),
                            internal_command,
                            config,
                        )
                    },
                )
                .on_resize(move |rect| content_height.set(rect.height()))
                .style(move |s| {
                    s.flex_col().width(scroll_width.get()).padding(12.0).gap(6.0)
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
        working_view(chat.clone(), config),
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
                clickable_icon(
                    move || {
                        if busy.get() {
                            LapceIcons::DEBUG_STOP
                        } else {
                            LapceIcons::LOCATION_FORWARD
                        }
                    },
                    move || {
                        if busy.get_untracked() {
                            chat.stop();
                        } else {
                            chat.send();
                        }
                    },
                    || false,
                    || false,
                    move || if busy.get() { "Stop" } else { "Send" },
                    config,
                )
                .style(|s| s.margin_right(4.0))
            },
        ))
        .on_event_cont(EventListener::PointerDown, move |_| {
            focus.set(Focus::Panel(PanelKind::ClaudeChat));
        })
        .style(move |s| {
            let config = config.get();
            s.margin(10.0)
                .padding_left(4.0)
                .items_center()
                .border(1.0)
                .border_radius(8.0)
                .border_color(config.color(LapceColor::LAPCE_BORDER))
                .background(config.color(LapceColor::EDITOR_BACKGROUND))
        }),
        usage_view(chat.usage, config),
    ))
    // Keeps the window from opening the dropped file in an editor
    .on_event(EventListener::DroppedFile, move |event| {
        let Event::DroppedFile(file) = event else {
            return EventPropagation::Continue;
        };
        drop_chat.mention_path(&file.path);
        focus.set(Focus::Panel(PanelKind::ClaudeChat));
        EventPropagation::Stop
    })
    .style(|s| s.absolute().size_pct(100.0, 100.0).flex_col())
    .debug_name("Claude Chat Panel")
}

fn header(
    chat: ClaudeChatData,
    panel: PanelData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let show_sessions = chat.show_sessions;
    let skip = chat.skip_permissions;
    let chat2 = chat.clone();
    let maximized = {
        let panel = panel.clone();
        move || {
            let Some((_, p)) = panel.panel_position(&PanelKind::ClaudeChat) else {
                return false;
            };
            panel.is_container_maximized(&p.container(), true)
        }
    };
    let maximized2 = maximized.clone();
    stack((
        text_button(|| "New chat", move || chat.new_session(), config),
        // Applies from the next message, the process restarts on the same session
        container(text_button(
            move || {
                if skip.get() {
                    "Permissions: skip"
                } else {
                    "Permissions: ask"
                }
            },
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
        empty().style(|s| s.flex_grow(1.0f32)),
        clickable_icon(
            move || {
                if maximized() {
                    LapceIcons::PANEL_RESTORE
                } else {
                    LapceIcons::PANEL_MAXIMISE
                }
            },
            move || panel.toggle_maximize(&PanelKind::ClaudeChat),
            || false,
            || false,
            move || if maximized2() { "Restore" } else { "Maximize" },
            config,
        ),
    ))
    .style(move |s| {
        s.width_full()
            .items_center()
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

/// "Thinking...", "Running Bash..." or "Waiting for approval..." while busy
fn working_view(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let busy = chat.busy;
    let permissions = chat.permissions;
    let tick = create_rw_signal(0usize);
    // Each animation chain stops itself once a newer one starts
    let chain = create_rw_signal(0u64);
    create_effect(move |was_busy: Option<bool>| {
        let busy_now = busy.get();
        if busy_now && was_busy != Some(true) {
            let id = chain.get_untracked() + 1;
            chain.set(id);
            animate(busy, tick, chain, id);
        }
        busy_now
    });
    let running = create_memo(move |_| chat.running_tool());

    label(move || {
        let dots = ".".repeat(tick.get() % 3 + 1);
        if permissions.with(|p| !p.is_empty()) {
            format!("Waiting for approval{dots}")
        } else if let Some(tool) = running.get() {
            format!("Running {tool}{dots}")
        } else {
            format!("Thinking{dots}")
        }
    })
    .style(move |s| {
        s.padding_horiz(14.0)
            .padding_vert(2.0)
            .color(config.get().color(LapceColor::EDITOR_DIM))
            .apply_if(!busy.get(), |s| s.hide())
    })
}

/// "5h 74%, resets 14:09 · Week 8%, resets Wed 05:59" under the input,
/// hidden until Claude Code reports it. Reset times are local wall clock.
fn usage_view(
    usage: RwSignal<Option<UsageLimits>>,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let window = |w: Option<UsageWindow>, time_format: &str| match w {
        Some(w) => match w.resets_at {
            Some(t) => {
                format!("{:.0}%, resets {}", w.percent, t.format(time_format))
            }
            None => format!("{:.0}%", w.percent),
        },
        None => "-".to_string(),
    };
    label(move || {
        let usage = usage.get().unwrap_or_default();
        format!(
            "5h {} · Week {}",
            window(usage.five_hour, "%H:%M"),
            window(usage.seven_day, "%a %H:%M")
        )
    })
    .style(move |s| {
        s.margin_top(-6.0)
            .padding_horiz(14.0)
            .padding_bottom(6.0)
            .color(config.get().color(LapceColor::EDITOR_DIM))
            .selectable(false)
            .apply_if(usage.get().is_none(), |s| s.hide())
    })
}

fn animate(
    busy: RwSignal<bool>,
    tick: RwSignal<usize>,
    chain: RwSignal<u64>,
    id: u64,
) {
    exec_after(Duration::from_millis(400), move |_| {
        if busy.get_untracked() && chain.get_untracked() == id {
            tick.update(|tick| *tick += 1);
            animate(busy, tick, chain, id);
        }
    });
}

/// Tighter than the 1.8 of hover popups, for long replies
const CHAT_LINE_HEIGHT: f64 = 1.5;
/// About 80 characters of the UI font
const REPLY_MAX_EMS: f64 = 44.0;

/// Wrapping plain text in the UI font
fn plain_text(text: &str, color: Color, config: &LapceConfig) -> TextLayout {
    let attrs = Attrs::new()
        .color(color)
        .font_size(config.ui.font_size() as f32)
        .line_height(LineHeightValue::Normal(CHAT_LINE_HEIGHT as f32));
    let mut layout = TextLayout::new();
    layout.set_text(text, AttrsList::new(attrs), None);
    layout
}

/// Diff preview in the editor font, removed lines red and added lines green
fn diff_layout(diff: &str, config: &LapceConfig) -> TextLayout {
    let family: Vec<FamilyOwned> =
        FamilyOwned::parse_list(&config.editor.font_family).collect();
    let attrs = Attrs::new()
        .family(&family)
        .font_size(config.ui.font_size() as f32)
        .line_height(LineHeightValue::Normal(1.5))
        .color(config.color(LapceColor::EDITOR_DIM));
    let mut attrs_list = AttrsList::new(attrs.clone());
    let mut start = 0;
    for line in diff.split('\n') {
        let color = match line.chars().next() {
            Some('+') => Some(LapceColor::SOURCE_CONTROL_ADDED),
            Some('-') => Some(LapceColor::SOURCE_CONTROL_REMOVED),
            _ => None,
        };
        if let Some(color) = color {
            let span = attrs.clone().color(config.color(color));
            attrs_list.add_span(start..start + line.len(), span);
        }
        start += line.len() + 1;
    }
    let mut layout = TextLayout::new();
    layout.set_text(diff, attrs_list, None);
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

/// One message with its turn label: user prompts as a right-aligned bubble,
/// Claude's text full width, tools as compact cards
fn message_view(
    msg: ChatMessage,
    workspace: Option<PathBuf>,
    internal_command: Listener<InternalCommand>,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let role = msg.role;
    let first_in_turn = msg.first_in_turn;
    let turn_label = if role == ChatRole::User { "You" } else { "Claude" };
    let body = match role {
        ChatRole::User => container(text_view(msg.text, role, config))
            .style(move |s| {
                let bg = config.get().color(LapceColor::PANEL_CURRENT_BACKGROUND);
                s.max_width_pct(85.0)
                    .padding_horiz(10.0)
                    .padding_vert(6.0)
                    .border_radius(8.0)
                    .background(bg)
            })
            .into_any(),
        ChatRole::Assistant => {
            let text = msg.text;
            stack((
                text_view(text, role, config),
                copy_button(move || text.get_untracked(), config),
            ))
            .style(|s| s.flex_col().width_full().items_start().gap(2.0))
            .into_any()
        }
        ChatRole::Tool => {
            tool_card(msg, workspace, internal_command, config).into_any()
        }
        ChatRole::Error => container(text_view(msg.text, role, config))
            .style(move |s| {
                s.width_full()
                    .padding_left(8.0)
                    .border_left(2.0)
                    .border_color(config.get().color(LapceColor::LAPCE_ERROR))
            })
            .into_any(),
    };
    stack((
        label(move || turn_label.to_string()).style(move |s| {
            s.font_bold()
                .margin_bottom(2.0)
                .color(config.get().color(LapceColor::EDITOR_DIM))
                .apply_if(!first_in_turn, |s| s.hide())
        }),
        body,
    ))
    .style(move |s| {
        s.flex_col()
            .width_full()
            .apply_if(role == ChatRole::User, |s| s.items_end())
            .apply_if(first_in_turn, |s| s.margin_top(10.0))
    })
}

/// Markdown for prompts and replies, plain text for errors
fn text_view(
    text: RwSignal<String>,
    role: ChatRole,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let id = AtomicU64::new(0);
    dyn_stack(
        move || {
            let config = config.get();
            text.with(|text| match role {
                ChatRole::Error => {
                    let color = config.color(LapceColor::LAPCE_ERROR);
                    vec![MarkdownContent::Text(plain_text(text, color, &config))]
                }
                _ => parse_markdown(text, CHAT_LINE_HEIGHT, &config),
            })
        },
        move |_| id.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        move |content| match content {
            // A bubble shrinks to short prompts, so only cap the width there.
            // Replies stop near 80 characters so long lines stay readable.
            // List items sit tight, other blocks get a little room.
            MarkdownContent::Text(text_layout) => {
                rich_text(move || text_layout.clone())
                    .style(move |s| text_width(s, role, config).margin_vert(4.0))
                    .into_any()
            }
            MarkdownContent::ListItem {
                marker,
                depth,
                layout,
            } => container(list_item_view(marker, depth, layout))
                .style(move |s| text_width(s, role, config))
                .into_any(),
            MarkdownContent::CodeBlock {
                layout,
                language,
                code,
            } => container(code_block_view(layout, language, code, config))
                .style(|s| s.width_full().margin_vert(4.0))
                .into_any(),
            MarkdownContent::Table(rows) => {
                let border = config.get_untracked().color(LapceColor::LAPCE_BORDER);
                container(table_view(rows, border))
                    .style(|s| s.max_width_full().margin_vert(4.0))
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
    .style(|s| s.flex_col().max_width_full())
}

fn text_width(
    s: Style,
    role: ChatRole,
    config: ReadSignal<Arc<LapceConfig>>,
) -> Style {
    let max_width = config.get().ui.font_size() as f64 * REPLY_MAX_EMS;
    s.apply_if(role == ChatRole::User, |s| s.max_width_full())
        .apply_if(role != ChatRole::User, |s| s.width_full().max_width(max_width))
}

/// Code in a bordered box, its language and a copy button on top
fn code_block_view(
    layout: TextLayout,
    language: String,
    code: String,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    stack((
        stack((
            label(move || language.clone()),
            empty().style(|s| s.flex_grow(1.0f32)),
            copy_button(move || code.clone(), config),
        ))
        .style(move |s| {
            let config = config.get();
            s.width_full()
                .items_center()
                .padding_left(8.0)
                .padding_vert(2.0)
                .border_bottom(1.0)
                .border_color(config.color(LapceColor::LAPCE_BORDER))
                .color(config.color(LapceColor::EDITOR_DIM))
        }),
        rich_text(move || layout.clone()).style(|s| s.width_full().padding(8.0)),
    ))
    .style(move |s| {
        let config = config.get();
        s.flex_col()
            .width_full()
            .border(1.0)
            .border_radius(6.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
            .background(config.color(LapceColor::EDITOR_BACKGROUND))
    })
}

/// "Copy", briefly "Copied" after a click
fn copy_button(
    text: impl Fn() -> String + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let copied = create_rw_signal(false);
    container(text_button(
        move || if copied.get() { "Copied" } else { "Copy" },
        move || {
            SystemClipboard::new().put_string(text());
            copied.set(true);
            exec_after(Duration::from_millis(1500), move |_| copied.set(false));
        },
        config,
    ))
    .style(move |s| s.color(config.get().color(LapceColor::EDITOR_DIM)))
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "Bash" => LapceIcons::TERMINAL,
        "Grep" | "Glob" => LapceIcons::SEARCH,
        "WebFetch" | "WebSearch" => LapceIcons::LINK,
        "Read" | "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => LapceIcons::FILE,
        _ => LapceIcons::SETTINGS,
    }
}

/// One line per tool call: icon, name, target and status. Clicking opens the
/// diff preview and the file buttons.
fn tool_card(
    msg: ChatMessage,
    workspace: Option<PathBuf>,
    internal_command: Listener<InternalCommand>,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let tool = msg.tool.clone();
    let status = msg.status;
    let name = tool.as_ref().map(|t| t.name.clone()).unwrap_or_default();
    let icon = tool_icon(&name);
    let summary = msg.text.get_untracked();
    let detail = match tool.as_ref().and_then(|t| t.file.as_deref()) {
        Some(file) => display_path(file, workspace.as_deref()),
        None => {
            let prefix = format!("{name}: ");
            summary.strip_prefix(&prefix).unwrap_or(&summary).to_string()
        }
    };
    let diff = tool.as_ref().and_then(|t| t.diff.clone());
    let has_diff = diff.is_some();
    let expandable = tool.as_ref().is_some_and(|t| t.file.is_some());
    let expanded = create_rw_signal(false);

    let dim = move |s: Style| s.color(config.get().color(LapceColor::EDITOR_DIM));
    let row = stack((
        svg(move || config.get().ui_svg(icon)).style(move |s| {
            let config = config.get();
            let size = config.ui.icon_size() as f32;
            s.size(size, size)
                .min_width(size)
                .color(config.color(LapceColor::EDITOR_DIM))
        }),
        label(move || name.clone()).style(|s| s.font_bold()),
        label(move || detail.clone()).style(move |s| {
            dim(s)
                .text_ellipsis()
                .flex_grow(1.0f32)
                .flex_basis(0.0)
                .min_width(0.0)
        }),
        label(move || match status.get() {
            ToolStatus::Running => "…",
            ToolStatus::Done => "✓",
            ToolStatus::Failed => "✗",
        })
        .style(move |s| {
            let config = config.get();
            let color = match status.get() {
                ToolStatus::Running => LapceColor::EDITOR_DIM,
                ToolStatus::Done => LapceColor::SOURCE_CONTROL_ADDED,
                ToolStatus::Failed => LapceColor::LAPCE_ERROR,
            };
            s.color(config.color(color))
        }),
        label(move || if expanded.get() { "▾" } else { "▸" })
            .style(move |s| dim(s).apply_if(!expandable, |s| s.hide())),
    ))
    .on_click_stop(move |_| {
        if expandable {
            expanded.update(|expanded| *expanded = !*expanded);
        }
    })
    .style(move |s| {
        s.width_full()
            .items_center()
            .gap(6.0)
            .apply_if(expandable, |s| s.cursor(CursorStyle::Pointer))
            .selectable(false)
    });

    let details = stack((
        rich_text(move || {
            diff_layout(diff.as_deref().unwrap_or_default(), &config.get())
        })
        .style(move |s| {
            s.width_full().margin_top(4.0).apply_if(!has_diff, |s| s.hide())
        }),
        tool_actions(tool, internal_command, config),
    ))
    .style(move |s| {
        s.flex_col().width_full().apply_if(!expanded.get(), |s| s.hide())
    });

    stack((row, details)).style(move |s| {
        let config = config.get();
        s.flex_col()
            .width_full()
            .padding_horiz(8.0)
            .padding_vert(4.0)
            .border(1.0)
            .border_radius(6.0)
            .border_color(config.color(LapceColor::LAPCE_BORDER))
    })
}

/// Workspace-relative path when the file is inside the workspace
fn display_path(file: &Path, workspace: Option<&Path>) -> String {
    workspace
        .and_then(|root| file.strip_prefix(root).ok())
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
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
    .style(move |s| s.gap(4.0).margin_top(4.0).apply_if(!has_file, |s| s.hide()))
}
