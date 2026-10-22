use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, SystemTime},
};

use floem::{
    IntoView, View,
    action::{exec_after, show_context_menu},
    event::{Event, EventListener, EventPropagation},
    kurbo::Rect,
    menu::{Menu, MenuItem},
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
        ChatMessage, ChatRole, ClaudeChatData, EFFORTS, MODELS, PermissionDecision,
        PermissionRequest,
        PopupItem, SessionInfo, ToolCall, ToolStatus, UsageLimits, UsageWindow,
        mention_query,
    },
    command::InternalCommand,
    config::{LapceConfig, color::LapceColor, icon::LapceIcons},
    doc::relative_time,
    editor::location::{EditorLocation, EditorPosition},
    listener::Listener,
    markdown::{
        Link, MarkdownContent, link_at, list_item_view, parse_markdown, table_view,
    },
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
        stack((
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
            empty_state(chat.clone(), config),
        ))
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
                .style(|s| s.width_full().min_width(0.0).padding(6.0)),
            input_toolbar(chat.clone(), config),
        ))
        .on_event_cont(EventListener::PointerDown, move |_| {
            focus.set(Focus::Panel(PanelKind::ClaudeChat));
        })
        .style(move |s| {
            let config = config.get();
            let border = if is_focused() {
                LapceColor::EDITOR_FOCUS
            } else {
                LapceColor::LAPCE_BORDER
            };
            s.flex_col()
                .margin(10.0)
                .border(1.0)
                .border_radius(8.0)
                .border_color(config.color(border))
                .background(config.color(LapceColor::EDITOR_BACKGROUND))
        }),
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
    let usage = chat.usage;
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
        usage_view(usage, config),
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

fn text_button<S: std::fmt::Display + 'static>(
    text: impl Fn() -> S + 'static,
    on_click: impl Fn() + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    label(text)
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

/// Bottom row of the input box: permission mode, model and effort on the
/// left, slash commands and send on the right. Mode, model and effort apply
/// from the next message, the process restarts on the same session.
fn input_toolbar(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let skip = chat.skip_permissions;
    let focus = chat.common.focus;
    let mode = |skip: bool| {
        let name = if skip { "Skip permissions" } else { "Ask permissions" };
        name.to_string()
    };
    let slash_chat = chat.clone();
    stack((
        container(text_button(
            move || mode(skip.get()),
            move || show_choice_menu(skip, &[false, true], mode),
            config,
        ))
        .style(move |s| {
            let config = config.get();
            let color = if skip.get() {
                LapceColor::LAPCE_ERROR
            } else {
                LapceColor::EDITOR_DIM
            };
            s.color(config.color(color))
        }),
        choice_button("Model", chat.model, &MODELS, config),
        choice_button("Effort", chat.effort, &EFFORTS, config),
        empty().style(|s| s.flex_grow(1.0f32)),
        container(text_button(
            || "/",
            move || {
                slash_chat.start_command();
                focus.set(Focus::Panel(PanelKind::ClaudeChat));
            },
            config,
        ))
        .style(move |s| s.color(config.get().color(LapceColor::EDITOR_DIM))),
        send_button(chat, config),
    ))
    .style(|s| s.items_center().padding(4.0).gap(2.0))
}

/// Square accent button, send when idle and stop while Claude works
fn send_button(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let busy = chat.busy;
    container(
        svg(move || {
            let icon = if busy.get() {
                LapceIcons::DEBUG_STOP
            } else {
                LapceIcons::LOCATION_FORWARD
            };
            config.get().ui_svg(icon)
        })
        .style(move |s| {
            let config = config.get();
            let size = config.ui.icon_size() as f32;
            s.size(size, size)
                .color(config.color(LapceColor::LAPCE_BUTTON_PRIMARY_FOREGROUND))
        }),
    )
    .on_click_stop(move |_| {
        if busy.get_untracked() {
            chat.stop();
        } else {
            chat.send();
        }
    })
    .style(move |s| {
        let config = config.get();
        s.size(24.0, 24.0)
            .margin_left(4.0)
            .items_center()
            .justify_center()
            .border_radius(4.0)
            .background(config.color(LapceColor::LAPCE_BUTTON_PRIMARY_BACKGROUND))
            .hover(|s| s.cursor(CursorStyle::Pointer))
    })
}

/// Opens a menu of `choices` with the current one checked, None shows as
/// "default"
fn choice_button(
    name: &'static str,
    value: RwSignal<Option<&'static str>>,
    choices: &'static [Option<&'static str>],
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let choice_name =
        |choice: Option<&str>| choice.unwrap_or("default").to_string();
    container(text_button(
        move || format!("{name}: {}", choice_name(value.get())),
        move || show_choice_menu(value, choices, choice_name),
        config,
    ))
    .style(move |s| s.color(config.get().color(LapceColor::EDITOR_DIM)))
}

/// Native menu of `choices` with the current one checked, picking one sets
/// `value`
fn show_choice_menu<T: Copy + PartialEq + 'static>(
    value: RwSignal<T>,
    choices: &[T],
    name: impl Fn(T) -> String,
) {
    let current = value.get_untracked();
    let menu = choices.iter().fold(Menu::new(""), |menu, &choice| {
        let mark = if choice == current { "✓ " } else { "    " };
        menu.entry(
            MenuItem::new(format!("{mark}{}", name(choice)))
                .action(move || value.set(choice)),
        )
    });
    show_context_menu(menu, None);
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

/// "5h 74%, resets 14:09 · Week 8%, resets Wed 05:59" in the header,
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
        s.padding_horiz(8.0)
            .min_width(0.0)
            .text_ellipsis()
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
    let opener = LinkOpener {
        workspace: workspace.clone(),
        internal_command,
    };
    let body = match role {
        ChatRole::User => container(text_view(msg.text, role, opener, config))
            .style(move |s| {
                let config = config.get();
                s.max_width_pct(85.0)
                    .padding_horiz(10.0)
                    .padding_vert(2.0)
                    .border(1.0)
                    .border_radius(6.0)
                    .border_color(config.color(LapceColor::LAPCE_BORDER))
                    .background(config.color(LapceColor::PANEL_CURRENT_BACKGROUND))
            })
            .into_any(),
        ChatRole::Assistant => {
            let text = msg.text;
            bulleted(
                LapceColor::EDITOR_DIM,
                stack((
                    text_view(text, role, opener, config),
                    copy_button(move || text.get_untracked(), config),
                ))
                .style(|s| s.flex_col().width_full().items_start().gap(2.0)),
                config,
            )
            .into_any()
        }
        ChatRole::Tool => {
            tool_card(msg, workspace, internal_command, config).into_any()
        }
        ChatRole::Error => bulleted(
            LapceColor::LAPCE_ERROR,
            text_view(msg.text, role, opener, config),
            config,
        )
        .into_any(),
    };
    container(body).style(move |s| {
        s.width_full().apply_if(first_in_turn, |s| s.margin_top(10.0))
    })
}

/// A small dot in a gutter on the left, lined up with the first line of
/// `body`, the rest of the block hangs after it
fn bulleted(
    color: &'static str,
    body: impl View + 'static,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    const DOT: f64 = 7.0;
    const GUTTER: f64 = 18.0;
    stack((
        empty().style(move |s| {
            let config = config.get();
            let line = config.ui.font_size() as f64 * CHAT_LINE_HEIGHT;
            // Text blocks start with a 4px margin
            s.size(DOT, DOT)
                .min_width(DOT)
                .margin_top(4.0 + (line - DOT) / 2.0)
                .margin_right(GUTTER - DOT)
                .border_radius(DOT / 2.0)
                .background(config.color(color))
        }),
        container(body)
            .style(|s| s.flex_grow(1.0f32).flex_basis(0.0).min_width(0.0)),
    ))
    .style(|s| s.width_full().items_start())
}

/// Markdown for prompts and replies, plain text for errors
fn text_view(
    text: RwSignal<String>,
    role: ChatRole,
    opener: LinkOpener,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let id = AtomicU64::new(0);
    dyn_stack(
        move || {
            let config = config.get();
            text.with(|text| match role {
                ChatRole::Error => {
                    let color = config.color(LapceColor::LAPCE_ERROR);
                    let layout = plain_text(text, color, &config);
                    vec![MarkdownContent::Text(layout, Vec::new())]
                }
                _ => parse_markdown(text, CHAT_LINE_HEIGHT, &config),
            })
        },
        move |_| id.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        move |content| match content {
            // A bubble shrinks to short prompts, so only cap the width there.
            // Replies stop near 80 characters so long lines stay readable.
            // List items sit tight, other blocks get a little room.
            MarkdownContent::Text(layout, links) => {
                container(link_text(layout, links, opener.clone()))
                    .style(move |s| text_width(s, role, config).margin_vert(4.0))
                    .into_any()
            }
            MarkdownContent::ListItem {
                marker,
                depth,
                layout,
                links,
            } => {
                let text = link_text(layout, links, opener.clone());
                container(list_item_view(marker, depth, text))
                    .style(move |s| text_width(s, role, config))
                    .into_any()
            }
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

/// Opens links in messages: web pages in the browser, workspace files in
/// the editor
#[derive(Clone)]
struct LinkOpener {
    workspace: Option<PathBuf>,
    internal_command: Listener<InternalCommand>,
}

enum LinkTarget {
    Url(String),
    /// Line counted from 1
    File(PathBuf, Option<usize>),
}

impl LinkOpener {
    /// None for inline code that names no existing file
    fn target(&self, target: &str) -> Option<LinkTarget> {
        if target.starts_with("http://") || target.starts_with("https://") {
            return Some(LinkTarget::Url(target.to_string()));
        }
        let (path, line) = split_line(target);
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.as_ref()?.join(path)
        };
        path.is_file().then_some(LinkTarget::File(path, line))
    }

    fn open(&self, target: LinkTarget) {
        match target {
            LinkTarget::Url(url) => {
                if let Err(err) = open::that(&url) {
                    tracing::error!("Failed to open {url}: {err}");
                }
            }
            LinkTarget::File(path, line) => {
                // EditorPosition lines count from 0
                let position =
                    line.map(|line| EditorPosition::Line(line.saturating_sub(1)));
                self.internal_command.send(InternalCommand::GoToLocation {
                    location: EditorLocation {
                        path,
                        position,
                        scroll_offset: None,
                        ignore_unconfirmed: false,
                        same_editor_tab: false,
                    },
                });
            }
        }
    }
}

/// `path:line`, `path:line:column` or `path#L42` into the path and line
fn split_line(target: &str) -> (&str, Option<usize>) {
    let number = |s: &str| {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse().ok()
    };
    if let Some((path, line)) = target.split_once("#L") {
        return (path, number(line));
    }
    match target.split_once(':') {
        Some((path, rest)) if number(rest).is_some() => (path, number(rest)),
        _ => (target, None),
    }
}

/// Rich text whose links open on click, with a hand cursor over them
fn link_text(layout: TextLayout, links: Vec<Link>, opener: LinkOpener) -> impl View {
    let over_link = create_rw_signal(false);
    let (hover_layout, hover_links, hover_opener) =
        (layout.clone(), links.clone(), opener.clone());
    let (click_layout, click_links) = (layout.clone(), links);
    rich_text(move || layout.clone())
        .on_event_cont(EventListener::PointerMove, move |event| {
            let Event::PointerMove(event) = event else {
                return;
            };
            let over = link_at(&hover_layout, &hover_links, event.pos)
                .is_some_and(|link| hover_opener.target(&link.target).is_some());
            if over != over_link.get_untracked() {
                over_link.set(over);
            }
        })
        .on_event_cont(EventListener::PointerLeave, move |_| over_link.set(false))
        .on_click_cont(move |event| {
            if let Event::PointerUp(event) = event
                && let Some(link) = link_at(&click_layout, &click_links, event.pos)
                && let Some(target) = opener.target(&link.target)
            {
                opener.open(target);
            }
        })
        .style(move |s| {
            s.width_full()
                .apply_if(over_link.get(), |s| s.cursor(CursorStyle::Pointer))
        })
}

/// What to ask and how to point Claude at files, while the chat is empty
fn empty_state(
    chat: ClaudeChatData,
    config: ReadSignal<Arc<LapceConfig>>,
) -> impl View {
    let messages = chat.messages;
    let workspace = chat.common.workspace.path.as_ref();
    let title = match workspace.and_then(|path| path.file_name()) {
        Some(name) => format!("Ask Claude about {}", name.to_string_lossy()),
        None => "Ask Claude".to_string(),
    };
    stack((
        label(move || title.clone()).style(move |s| {
            s.font_bold()
                .font_size(config.get().ui.font_size() as f32 * 1.25)
        }),
        label(|| {
            "Type @ to mention a file or / for a command.\n\
             Drop files here to mention them."
                .to_string()
        })
        .style(move |s| s.color(config.get().color(LapceColor::EDITOR_DIM))),
        text_button(|| "Resume a session", move || chat.open_sessions(), config),
    ))
    .style(move |s| {
        s.absolute()
            .size_pct(100.0, 100.0)
            .flex_col()
            .items_center()
            .justify_center()
            .gap(8.0)
            .apply_if(messages.with(|m| !m.is_empty()), |s| s.hide())
    })
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

#[cfg(test)]
mod tests {
    use super::split_line;

    #[test]
    fn split_line_number() {
        assert_eq!(split_line("src/a.rs"), ("src/a.rs", None));
        assert_eq!(split_line("src/a.rs:42"), ("src/a.rs", Some(42)));
        assert_eq!(split_line("src/a.rs:42:7"), ("src/a.rs", Some(42)));
        assert_eq!(split_line("src/a.rs#L9"), ("src/a.rs", Some(9)));
        assert_eq!(split_line("std::fs"), ("std::fs", None));
    }
}
