use std::{
    cell::{Cell, RefCell},
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    rc::Rc,
    sync::mpsc::{Sender, channel},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Local};
use floem::{
    action::exec_after,
    ext_event::{create_ext_action, create_signal_from_channel},
    keyboard::Modifiers,
    reactive::{RwSignal, Scope, SignalGet, SignalUpdate, SignalWith},
    views::editor::text::SystemClipboard,
};
use lapce_core::{
    buffer::rope_text::RopeText,
    command::{EditCommand, FocusCommand, MoveCommand},
    directory::Directory,
    mode::Mode,
    register::Clipboard,
    selection::Selection,
};
use lapce_rpc::proxy::ProxyResponse;
use lapce_xi_rope::Rope;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use serde_json::{Value, json};

use crate::{
    command::{CommandExecuted, CommandKind},
    editor::EditorData,
    keypress::{KeyPressFocus, condition::Condition},
    main_split::Editors,
    window_tab::CommonData,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub id: u64,
    pub role: ChatRole,
    pub text: RwSignal<String>,
    pub tool: Option<ToolCall>,
    /// Only meaningful for tool messages
    pub status: RwSignal<ToolStatus>,
    /// One line about a tool's output, e.g. "Read 256 lines"
    pub result: RwSignal<Option<String>>,
    /// First message of a user or Claude turn, gets the turn label
    pub first_in_turn: bool,
}

/// What a tool message can show beyond its one line summary
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    /// `tool_use` id, matched by the later `tool_result`
    pub id: String,
    pub name: String,
    pub file: Option<PathBuf>,
    /// Edit, Write and friends, the file has changes worth reviewing
    pub edits_file: bool,
    /// Unified-diff style preview of the change
    pub diff: Option<String>,
}

/// A message read back from a session file
type HistoryItem = (ChatRole, String, Option<ToolCall>);

/// A row of the input popup: a slash command or an `@` file mention
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupItem {
    pub label: String,
    pub hint: String,
    pub description: String,
    /// Replaces the last token of the input
    pub insert: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub hint: String,
}

/// A tool call waiting for the user's approval
#[derive(Clone, Debug)]
pub struct PermissionRequest {
    pub request_id: String,
    pub title: String,
    pub detail: String,
    input: Value,
    /// Rules Claude Code proposes for "always allow"
    suggestions: Value,
}

impl PermissionRequest {
    pub fn can_always_allow(&self) -> bool {
        self.suggestions.as_array().is_some_and(|s| !s.is_empty())
    }

    /// Answers in the order they are numbered, the number key picks one
    pub fn options(&self) -> Vec<(&'static str, PermissionDecision)> {
        let mut options = vec![("Yes", PermissionDecision::Allow)];
        if self.can_always_allow() {
            options.push((
                "Yes, and don't ask again",
                PermissionDecision::AlwaysAllow,
            ));
        }
        options.push(("No", PermissionDecision::Deny));
        options
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    AlwaysAllow,
    Deny,
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
}

/// How full the context is and what the session cost, from the stream
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextUsage {
    /// Prompt and reply tokens of the last request, what the next one builds on
    pub tokens: u64,
    /// Context window of that request's model, None until a turn ends
    pub window: Option<u64>,
    /// Claude Code's estimate at API prices, None until a turn ends
    pub cost: Option<f64>,
    /// Model of the last request, keys the window in the turn's result
    model: String,
}

impl ContextUsage {
    pub fn percent(&self) -> Option<f64> {
        let window = self.window.filter(|w| *w > 0)?;
        Some(self.tokens as f64 * 100.0 / window as f64)
    }
}

/// Claude Code's own resume thresholds: past them the prompt cache is gone
/// and the first message re-reads the whole context uncached
const RESUME_IDLE_SECS: i64 = 70 * 60;

/// A quiet Claude Code process is stopped after this to give its memory
/// back, the next message resumes the session
const IDLE_STOP: Duration = Duration::from_secs(15 * 60);
const RESUME_TOKENS: u64 = 100_000;

/// A resumed session idle and big enough that continuing costs a lot
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumeWarning {
    pub idle_secs: i64,
    pub tokens: u64,
}

fn resume_warning(idle_secs: i64, tokens: u64) -> Option<ResumeWarning> {
    (idle_secs >= RESUME_IDLE_SECS && tokens >= RESUME_TOKENS)
        .then_some(ResumeWarning { idle_secs, tokens })
}

/// Model and effort a running Claude Code actually uses, settings files
/// included, from its `get_settings`
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppliedSettings {
    /// Model id, "claude-opus-5"
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Plan usage of each window, from Claude Code's `get_usage`
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageLimits {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindow {
    pub percent: f64,
    pub resets_at: Option<DateTime<Local>>,
}

#[derive(Debug)]
struct ChatProcess {
    child: Child,
    stdin: ChildStdin,
    /// `--effort` is only read at start, model and permission mode change live
    effort: Option<String>,
}

/// `--permission-mode` values offered in the input toolbar, with their labels
pub const PERMISSION_MODES: [(&str, &str); 4] = [
    ("default", "Ask permissions"),
    ("acceptEdits", "Edit automatically"),
    ("plan", "Plan mode"),
    ("bypassPermissions", "Skip permissions"),
];

/// A model Claude Code offers this account, from its `initialize` answer
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// Passed to `--model`, "default" is Claude Code's own pick
    pub value: String,
    /// "Opus"
    pub name: String,
    /// "Opus 5 · Best for everyday, complex tasks"
    pub description: String,
    /// Model id the value stands for, "claude-opus-5"
    pub resolved: String,
    /// `--effort` levels, empty when the model takes none
    pub efforts: Vec<String>,
}

impl ModelInfo {
    /// "Opus 5", the head of the description
    pub fn version(&self) -> &str {
        self.description.split(" · ").next().unwrap_or(&self.name)
    }
}

/// Chat with Claude Code running headless (`claude -p` with stream-json).
/// The process stays alive across turns; it is respawned with `--resume`
/// after it exits or is stopped.
#[derive(Clone, Debug)]
pub struct ClaudeChatData {
    pub scope: Scope,
    pub editor: EditorData,
    pub messages: RwSignal<im::Vector<ChatMessage>>,
    pub busy: RwSignal<bool>,
    pub session_id: RwSignal<Option<String>>,
    pub sessions: RwSignal<Vec<SessionInfo>>,
    pub show_sessions: RwSignal<bool>,
    /// Slash commands reported by Claude Code, kept across processes
    pub commands: RwSignal<Vec<SlashCommand>>,
    /// Workspace files for `@` mentions, relative to the workspace
    pub files: RwSignal<Vec<String>>,
    /// Highlighted row of the input popup
    pub popup_index: RwSignal<usize>,
    commands_requested: Rc<Cell<bool>>,
    /// Claude Code's guess at the next prompt, shown in the empty input
    pub suggestion: RwSignal<Option<String>>,
    /// Long pastes, selections and images, shown in the input as a
    /// placeholder like `[Pasted text #n ...]` and expanded on send. Paste
    /// `#n` is at index n - 1, as (placeholder, content).
    pastes: Rc<RefCell<Vec<(String, String)>>>,
    /// While Up/Down walk the sent prompts: the one shown and the input
    /// typed before, put back after the newest
    recalled: Rc<RefCell<Option<(usize, String)>>>,
    /// A `PERMISSION_MODES` value, "default" asks. Back to it on every
    /// launch.
    pub permission_mode: RwSignal<&'static str>,
    /// Models from Claude Code, empty until its first process answers
    pub models: RwSignal<Vec<ModelInfo>>,
    /// None leaves Claude Code's default
    pub model: RwSignal<Option<String>>,
    /// None leaves Claude Code's default
    pub effort: RwSignal<Option<String>>,
    /// Pending tool approvals, oldest first
    pub permissions: RwSignal<im::Vector<PermissionRequest>>,
    /// Refreshed on spawn and after every turn, None until first answered
    pub usage: RwSignal<Option<UsageLimits>>,
    /// None until the session's first reply
    pub context: RwSignal<Option<ContextUsage>>,
    /// What "default" model and effort turn out to be, None until answered
    pub applied: RwSignal<Option<AppliedSettings>>,
    /// Shown once after resuming an idle, large session
    pub resume_warning: RwSignal<Option<ResumeWarning>>,
    /// How long the session being resumed sat idle, until its context is known
    resume_idle: Rc<Cell<Option<i64>>>,
    /// Signed in account from `claude auth status`, None when signed out or
    /// not checked yet
    pub account: RwSignal<Option<String>>,
    /// Claude Code's version, None until its process starts
    pub version: RwSignal<Option<String>>,
    /// The newest Claude Code on npm, looked up once the panel shows
    pub latest_version: RwSignal<Option<String>>,
    /// A `claude update` is running
    pub updating: RwSignal<bool>,
    /// A running `claude auth login`, its stdin takes the pasted code
    login: Rc<RefCell<Option<(Child, ChildStdin)>>>,
    /// Login output chunks, None once its output closes
    login_tx: Sender<Option<String>>,
    process: Rc<RefCell<Option<ChatProcess>>>,
    /// Bumped on every spawn and stop so events from an old process are ignored
    generation: Rc<Cell<u64>>,
    next_id: Rc<Cell<u64>>,
    /// Bumped on every spawn and turn end, an idle stop timer acts only if
    /// it is still the latest
    idle_token: Rc<Cell<u64>>,
    tx: Sender<(u64, Value)>,
    pub common: Rc<CommonData>,
}

impl KeyPressFocus for ClaudeChatData {
    fn get_mode(&self) -> Mode {
        Mode::Insert
    }

    fn check_condition(&self, condition: Condition) -> bool {
        match condition {
            Condition::PanelFocus => true,
            // Lets up/down/enter drive the slash command popup
            Condition::ListFocus => !self.popup_items().is_empty(),
            _ => false,
        }
    }

    fn run_command(
        &self,
        command: &crate::command::LapceCommand,
        count: Option<usize>,
        mods: Modifiers,
    ) -> CommandExecuted {
        match &command.kind {
            CommandKind::Focus(FocusCommand::ListNext) => {
                self.move_popup_index(1);
                CommandExecuted::Yes
            }
            CommandKind::Focus(FocusCommand::ListPrevious) => {
                self.move_popup_index(-1);
                CommandExecuted::Yes
            }
            CommandKind::Focus(FocusCommand::ListSelect) => {
                self.select_popup_item();
                CommandExecuted::Yes
            }
            CommandKind::Move(MoveCommand::Up) if self.recall_prompt(true) => {
                CommandExecuted::Yes
            }
            CommandKind::Move(MoveCommand::Down) if self.recall_prompt(false) => {
                CommandExecuted::Yes
            }
            // Shift+Enter starts a new line, plain Enter sends
            CommandKind::Edit(EditCommand::InsertNewLine) if mods.shift() => {
                self.editor.receive_char("\n");
                CommandExecuted::Yes
            }
            CommandKind::Edit(EditCommand::InsertNewLine) => {
                self.send();
                CommandExecuted::Yes
            }
            // Esc interrupts the running turn, like the CLI
            CommandKind::Edit(EditCommand::NormalMode)
                if self.busy.get_untracked() =>
            {
                self.stop();
                CommandExecuted::Yes
            }
            CommandKind::Edit(EditCommand::ClipboardPaste)
                if self.paste_long() || self.paste_image() =>
            {
                CommandExecuted::Yes
            }
            CommandKind::Edit(_)
            | CommandKind::Move(_)
            | CommandKind::MultiSelection(_) => {
                self.editor.run_command(command, count, mods)
            }
            _ => CommandExecuted::No,
        }
    }

    fn receive_char(&self, c: &str) {
        if !self.answer_by_number(c) {
            self.editor.receive_char(c);
        }
    }
}

impl ClaudeChatData {
    pub fn new(cx: Scope, editors: Editors, common: Rc<CommonData>) -> Self {
        let (tx, rx) = channel();
        let (login_tx, login_rx) = channel();
        let chat = Self {
            scope: cx,
            editor: editors.make_local(cx, common.clone()),
            messages: cx.create_rw_signal(im::Vector::new()),
            busy: cx.create_rw_signal(false),
            session_id: cx.create_rw_signal(None),
            sessions: cx.create_rw_signal(Vec::new()),
            show_sessions: cx.create_rw_signal(false),
            commands: cx.create_rw_signal(Vec::new()),
            files: cx.create_rw_signal(Vec::new()),
            popup_index: cx.create_rw_signal(0),
            commands_requested: Rc::new(Cell::new(false)),
            suggestion: cx.create_rw_signal(None),
            pastes: Rc::new(RefCell::new(Vec::new())),
            recalled: Rc::new(RefCell::new(None)),
            permission_mode: cx.create_rw_signal("default"),
            models: cx.create_rw_signal(Vec::new()),
            model: cx.create_rw_signal(None),
            effort: cx.create_rw_signal(None),
            permissions: cx.create_rw_signal(im::Vector::new()),
            usage: cx.create_rw_signal(None),
            context: cx.create_rw_signal(None),
            applied: cx.create_rw_signal(None),
            resume_warning: cx.create_rw_signal(None),
            resume_idle: Rc::new(Cell::new(None)),
            account: cx.create_rw_signal(None),
            version: cx.create_rw_signal(None),
            latest_version: cx.create_rw_signal(None),
            updating: cx.create_rw_signal(false),
            login: Rc::new(RefCell::new(None)),
            login_tx,
            process: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            next_id: Rc::new(Cell::new(0)),
            idle_token: Rc::new(Cell::new(0)),
            tx,
            common,
        };

        let event = create_signal_from_channel(rx);
        let chat2 = chat.clone();
        cx.create_effect(move |_| {
            event.with(|event| {
                if let Some((generation, event)) = event
                    && *generation == chat2.generation.get()
                {
                    chat2.handle_event(event);
                }
            });
        });

        let login = create_signal_from_channel(login_rx);
        let chat2 = chat.clone();
        cx.create_effect(move |_| {
            login.with(|output| match output {
                Some(Some(text)) => chat2.push(ChatRole::Assistant, text.clone()),
                Some(None) => chat2.login_finished(),
                None => {}
            });
        });

        chat
    }

    pub fn send(&self) {
        self.recalled.take();
        let text = self.editor.doc().buffer.with_untracked(|b| b.to_string());
        let text = text.trim();
        // What Claude gets, the chat keeps the short placeholders
        let full = expand_pastes(text, &self.pastes.borrow());
        // Headless Claude Code has no /clear, handle it here, even mid-turn
        if text == "/clear" {
            self.editor.reset();
            self.pastes.borrow_mut().clear();
            self.popup_index.set(0);
            self.new_session();
            return;
        }
        // The code the login page shows, kept out of the chat
        if let Some((_, stdin)) = self.login.borrow_mut().as_mut()
            && !text.is_empty()
        {
            self.editor.reset();
            let sent = writeln!(stdin, "{full}").and_then(|()| stdin.flush());
            match sent {
                Ok(()) => self.push(ChatRole::Assistant, "Code sent.".to_string()),
                Err(e) => {
                    self.push(ChatRole::Error, format!("Failed to send code: {e}"))
                }
            }
            return;
        }
        // Typed while a call waits for approval: deny it and tell Claude
        // what to do instead
        let pending = self.permissions.with_untracked(|p| p.front().cloned());
        if let Some(request) = pending
            && !text.is_empty()
        {
            self.editor.reset();
            self.push(ChatRole::User, text.to_string());
            self.respond_permission(
                &request.request_id,
                PermissionDecision::Deny,
                Some(&full),
            );
            return;
        }
        if self.busy.get_untracked() {
            return;
        }
        if text.is_empty() {
            // Enter on the empty input takes the suggested prompt
            if let Some(suggestion) = self.suggestion.get_untracked() {
                self.set_input(suggestion);
            }
            return;
        }
        self.editor.reset();
        self.popup_index.set(0);
        self.suggestion.set(None);
        self.resume_warning.set(None);
        self.push(ChatRole::User, text.to_string());

        // Effort is fixed at spawn, restart (and resume) when it changes
        let effort = self.effort.get_untracked();
        let stale = self
            .process
            .borrow()
            .as_ref()
            .is_some_and(|p| p.effort != effort);
        if stale {
            self.stop();
        }

        let running = self.process.borrow().is_some();
        if !running && let Err(e) = self.spawn() {
            self.push(ChatRole::Error, format!("Failed to start claude: {e}"));
            return;
        }

        let line = json!({
            "type": "user",
            "message": { "role": "user", "content": full },
        });
        match self.write_line(&line) {
            Ok(()) => self.busy.set(true),
            Err(e) => {
                self.stop();
                self.push(ChatRole::Error, format!("Failed to send: {e}"));
            }
        }
    }

    /// Long clipboard text goes in as a placeholder, false leaves short
    /// text to the editor's own paste
    fn paste_long(&self) -> bool {
        let Some(text) = SystemClipboard::new().get_string() else {
            return false;
        };
        if text.lines().count() <= PASTE_MAX_LINES && text.len() <= PASTE_MAX_LEN {
            return false;
        }
        let placeholder = paste_placeholder(self.pastes.borrow().len() + 1, &text);
        self.pastes.borrow_mut().push((placeholder.clone(), text));
        self.editor.receive_char(&placeholder);
        true
    }

    /// A clipboard image without text goes in as `[Image #n]`, saved to a
    /// file Claude reads through an `@` mention
    fn paste_image(&self) -> bool {
        if SystemClipboard::new().get_string().is_some() {
            return false;
        }
        let Some(png) = clipboard_png() else {
            return false;
        };
        // ponytail: never deleted, macOS clears the temp dir after a few days
        let dir = std::env::temp_dir().join("lapce-claude");
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = dir.join(format!("paste-{stamp}.png"));
        let saved = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&path, png));
        if let Err(e) = saved {
            self.push(ChatRole::Error, format!("Failed to save the image: {e}"));
            return true;
        }
        let placeholder = format!("[Image #{}]", self.pastes.borrow().len() + 1);
        let mention = path_mention(&path, None);
        self.pastes.borrow_mut().push((placeholder.clone(), mention));
        self.editor.receive_char(&placeholder);
        true
    }

    /// Kill the running turn. The next message resumes the same session.
    pub fn stop(&self) {
        self.generation.set(self.generation.get() + 1);
        if let Some(mut p) = self.process.borrow_mut().take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        self.permissions.set(im::Vector::new());
        self.suggestion.set(None);
        self.busy.set(false);
        // Tools cut off by the kill never report a result
        self.messages.with_untracked(|messages| {
            for msg in messages {
                if msg.status.get_untracked() == ToolStatus::Running {
                    msg.status.set(ToolStatus::Failed);
                }
            }
        });
    }

    /// Name of the tool Claude is waiting on, tracked
    pub fn running_tool(&self) -> Option<String> {
        self.messages.with(|messages| {
            let msg = messages
                .iter()
                .rev()
                .find(|msg| msg.status.get() == ToolStatus::Running)?;
            Some(msg.tool.as_ref()?.name.clone())
        })
    }

    pub fn answer_permission(&self, request_id: &str, decision: PermissionDecision) {
        self.respond_permission(request_id, decision, None);
    }

    /// `message` tells Claude what to do instead of a denied call
    fn respond_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
        message: Option<&str>,
    ) {
        let request = self.permissions.try_update(|permissions| {
            let i = permissions.iter().position(|p| p.request_id == request_id)?;
            Some(permissions.remove(i))
        });
        let Some(request) = request.flatten() else {
            return;
        };
        let response = permission_response(&request, decision, message);
        if let Err(e) = self.write_line(&response) {
            self.push(ChatRole::Error, format!("Failed to answer: {e}"));
        }
    }

    /// The number key of an option on the pending approval card, only while
    /// the input is empty so numbers can still be typed
    fn answer_by_number(&self, c: &str) -> bool {
        let Some(request) = self.permissions.with_untracked(|p| p.front().cloned())
        else {
            return false;
        };
        let empty = self.editor.doc().buffer.with_untracked(|b| b.is_empty());
        let option = c
            .parse::<usize>()
            .ok()
            .and_then(|n| request.options().get(n.wrapping_sub(1)).copied());
        match option {
            Some((_, decision)) if empty => {
                self.answer_permission(&request.request_id, decision);
                true
            }
            _ => false,
        }
    }

    /// Popup rows for the input, tracked
    pub fn popup_items(&self) -> Vec<PopupItem> {
        let input = self.editor.doc_signal().get().buffer.with(|b| b.to_string());
        if input.starts_with('/') {
            let commands = self.commands.with(|c| filter_commands(c, &input));
            return commands
                .into_iter()
                .map(|c| PopupItem {
                    label: format!("/{}", c.name),
                    insert: format!("/{} ", c.name),
                    hint: c.hint,
                    description: c.description,
                })
                .collect();
        }
        let Some(query) = mention_query(&input) else {
            return Vec::new();
        };
        let files = self.files.with(|files| filter_files(files, query));
        files
            .into_iter()
            .map(|path| PopupItem {
                insert: format!("@{path} "),
                label: path,
                hint: String::new(),
                description: String::new(),
            })
            .collect()
    }

    /// Reload the workspace file list, done each time an `@` mention starts
    pub fn request_files(&self) {
        let workspace = self.common.workspace.path.clone();
        let files = self.files;
        let send = create_ext_action(self.scope, move |items: Vec<PathBuf>| {
            let relative = items.iter().map(|path| {
                let path = match workspace.as_ref() {
                    Some(root) => path.strip_prefix(root).unwrap_or(path),
                    None => path,
                };
                path.to_string_lossy().into_owned()
            });
            files.set(relative.collect());
        });
        self.common.proxy.get_files(move |result| {
            if let Ok(ProxyResponse::GetFilesResponse { items }) = result {
                send(items);
            }
        });
    }

    /// Start Claude Code early so it can report its slash commands. Starting
    /// the process alone makes no API request.
    /// The picked model, or the one Claude Code applies by default (settings
    /// files can override the account's default), tracked
    pub fn current_model(&self) -> Option<ModelInfo> {
        let picked = self.model.get();
        let applied = self.applied.with(|a| a.as_ref()?.model.clone());
        self.models.with(|models| {
            let by_value = |value: &str| models.iter().find(|m| m.value == value);
            let found = match (&picked, &applied) {
                (Some(value), _) => by_value(value),
                (None, Some(model)) => models
                    .iter()
                    .find(|m| &m.resolved == model)
                    .or_else(|| by_value("default")),
                (None, None) => by_value("default"),
            };
            found.cloned()
        })
    }

    /// Switches the running process too. Drops the effort level when the new
    /// model does not take it.
    pub fn set_model(&self, model: Option<String>) {
        let value = model.clone().unwrap_or_else(|| "default".to_string());
        self.model.set(model);
        self.control(
            "lapce-set-model",
            json!({ "subtype": "set_model", "model": value }),
        );
        // The model can bring its own effort from the settings files
        self.request_settings();
        let efforts = self.current_model().map(|m| m.efforts).unwrap_or_default();
        if self
            .effort
            .with_untracked(|e| e.as_ref().is_some_and(|e| !efforts.contains(e)))
        {
            self.effort.set(None);
        }
    }

    /// Switches the running process too, it was started allowing the bypass
    pub fn set_permission_mode(&self, mode: &'static str) {
        self.permission_mode.set(mode);
        self.control(
            "lapce-permission-mode",
            json!({ "subtype": "set_permission_mode", "mode": mode }),
        );
    }

    /// Sends a control request when a process runs, a later spawn reads the
    /// signals instead
    fn control(&self, request_id: &str, request: Value) {
        if self.process.borrow().is_none() {
            return;
        }
        let line = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        if let Err(e) = self.write_line(&line) {
            self.push(ChatRole::Error, format!("Failed to send: {e}"));
        }
    }

    /// Starts Claude Code once, its `initialize` answer brings the slash
    /// commands and models. No model request is made.
    pub fn request_commands(&self) {
        if self.commands_requested.replace(true) {
            return;
        }
        self.refresh_account(false);
        self.check_latest_version();
        let running = self.process.borrow().is_some();
        if !running && let Err(e) = self.spawn() {
            self.push(ChatRole::Error, format!("Failed to start claude: {e}"));
        }
    }

    pub fn apply_item(&self, item: &PopupItem) {
        let input = self.editor.doc().buffer.with_untracked(|b| b.to_string());
        self.set_input(replace_last_token(&input, &item.insert));
        self.popup_index.set(0);
    }

    /// Append an `@` mention for a file dropped on the panel
    pub fn mention_path(&self, path: &Path) {
        let mention = path_mention(path, self.common.workspace.path.as_deref());
        self.append_input(&mention);
    }

    /// Mention the file with the selected lines, the code going in as a
    /// collapsed paste. Without a selection only the file is mentioned.
    pub fn ask_about(&self, path: &Path, selection: Option<(usize, usize, String)>) {
        let Some((start, end, code)) = selection else {
            self.mention_path(path);
            return;
        };
        let mention = path_mention(path, self.common.workspace.path.as_deref());
        let code = format!("```\n{code}\n```");
        let placeholder = paste_placeholder(self.pastes.borrow().len() + 1, &code);
        self.pastes.borrow_mut().push((placeholder.clone(), code));
        let lines = line_range(start, end);
        self.append_input(&format!("{mention} {lines} {placeholder}"));
    }

    /// Ask Claude to fix the diagnostics of a file, left in the input to
    /// add to or send
    pub fn fix_diagnostics(&self, path: &Path, diagnostics: &[Diagnostic]) {
        let mention = path_mention(path, self.common.workspace.path.as_deref());
        self.append_input(&diagnostics_prompt(&mention, diagnostics));
    }

    /// Add to the end of the input, a space apart from what's there
    fn append_input(&self, text: &str) {
        let input = self.editor.doc().buffer.with_untracked(|b| b.to_string());
        let sep = if input.is_empty() || input.ends_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        self.set_input(format!("{input}{sep}{text} "));
    }

    /// Starts a slash command in an empty input, opening the command popup
    pub fn start_command(&self) {
        let empty = self.editor.doc().buffer.with_untracked(|b| b.is_empty());
        if empty {
            self.set_input("/".to_string());
        }
    }

    /// Summarize the session before going on, the resume warning's way out
    pub fn compact(&self) {
        self.set_input("/compact".to_string());
        self.send();
    }

    /// Replace the input and put the cursor at its end
    fn set_input(&self, text: String) {
        let len = text.len();
        self.editor.doc().reload(Rope::from(text), true);
        self.editor
            .cursor()
            .update(|cursor| cursor.set_insert(Selection::caret(len)));
    }

    fn select_popup_item(&self) {
        let items = self.popup_items();
        let index = self.popup_index.get_untracked();
        let Some(item) = items.get(index).or(items.last()) else {
            return;
        };
        let input = self.editor.doc().buffer.with_untracked(|b| b.to_string());
        // Enter on a fully typed command runs it
        if input.trim() == item.insert.trim() {
            self.send();
        } else {
            self.apply_item(item);
        }
    }

    /// Up on the first line and Down on the last walk the sent prompts,
    /// like a shell. False leaves the key to the editor.
    fn recall_prompt(&self, older: bool) -> bool {
        let offset = self.editor.cursor().with_untracked(|c| c.offset());
        let (input, line, last) = self.editor.doc().buffer.with_untracked(|b| {
            (b.to_string(), b.line_of_offset(offset), b.last_line())
        });
        if line != if older { 0 } else { last } {
            return false;
        }
        let prompts: Vec<String> = self.messages.with_untracked(|messages| {
            messages
                .iter()
                .filter(|m| m.role == ChatRole::User)
                .map(|m| m.text.get_untracked())
                .collect()
        });
        let index = self.recalled.borrow().as_ref().map(|(i, _)| *i);
        let Some(next) = history_step(prompts.len(), index, older) else {
            return false;
        };
        let draft = self.recalled.take().map_or(input, |(_, draft)| draft);
        match next {
            Some(i) => {
                self.set_input(prompts[i].clone());
                self.recalled.replace(Some((i, draft)));
            }
            None => self.set_input(draft),
        }
        true
    }

    fn move_popup_index(&self, delta: isize) {
        let len = self.popup_items().len();
        if len == 0 {
            return;
        }
        self.popup_index.update(|index| {
            *index = (*index as isize + delta).rem_euclid(len as isize) as usize;
        });
    }

    fn write_line(&self, line: &Value) -> std::io::Result<()> {
        let mut process = self.process.borrow_mut();
        let Some(p) = process.as_mut() else {
            return Err(std::io::ErrorKind::NotConnected.into());
        };
        writeln!(p.stdin, "{line}")?;
        p.stdin.flush()
    }

    pub fn new_session(&self) {
        self.stop();
        self.context.set(None);
        self.resume_warning.set(None);
        self.session_id.set(None);
        self.messages.set(im::Vector::new());
        self.show_sessions.set(false);
    }

    /// Show the sessions Claude Code stored for this workspace
    pub fn open_sessions(&self) {
        self.show_sessions.set(true);
        let Some(dir) = self.sessions_dir() else {
            return;
        };
        let sessions = self.sessions;
        let send = create_ext_action(self.scope, move |list| sessions.set(list));
        std::thread::spawn(move || send(list_sessions(&dir)));
    }

    pub fn resume(&self, id: String) {
        self.stop();
        self.context.set(None);
        self.session_id.set(Some(id.clone()));
        self.messages.set(im::Vector::new());
        self.show_sessions.set(false);
        self.resume_warning.set(None);
        // ponytail: file mtime as last activity, like the sessions list; read
        // the last message timestamp if renames or tools start touching it
        let idle = self.sessions_dir().and_then(|dir| {
            let path = dir.join(format!("{id}.jsonl"));
            let modified = path.metadata().ok()?.modified().ok()?;
            let idle = SystemTime::now().duration_since(modified).ok()?;
            Some(idle.as_secs() as i64)
        });
        self.resume_idle.set(idle);
        // Attach to the session now, not on the first message. Spawning makes
        // no model request.
        if let Err(e) = self.spawn() {
            self.push(ChatRole::Error, format!("Failed to start claude: {e}"));
        }
        let Some(dir) = self.sessions_dir() else {
            return;
        };

        let chat = self.clone();
        let current = id.clone();
        let send = create_ext_action(self.scope, move |history: Vec<_>| {
            // Ignore if another session was picked while loading
            if chat.session_id.get_untracked().as_deref() == Some(&current) {
                for (role, text, tool) in history {
                    // ponytail: history tools show as done, failures in
                    // the file's tool_result are not read back
                    chat.push_message(role, text, tool, ToolStatus::Done);
                }
            }
        });
        std::thread::spawn(move || {
            let path = dir.join(format!("{id}.jsonl"));
            let history = File::open(path)
                .map(|f| read_history(BufReader::new(f)))
                .unwrap_or_default();
            send(history);
        });
    }

    pub fn signing_in(&self) -> bool {
        self.login.borrow().is_some()
    }

    /// Run `claude auth login`: it opens the browser and may ask for the
    /// code the page shows, which the input then sends
    pub fn login(&self) {
        if self.signing_in() {
            return;
        }
        self.stop();
        let mut cmd = claude_command();
        cmd.args(["auth", "login"]);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.push(ChatRole::Error, format!("Failed to start login: {e}"));
                return;
            }
        };
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        *self.login.borrow_mut() = Some((child, stdin));

        let tx = self.login_tx.clone();
        std::thread::spawn(move || forward_output(stderr, &tx));
        let tx = self.login_tx.clone();
        std::thread::spawn(move || {
            forward_output(stdout, &tx);
            let _ = tx.send(None);
        });
    }

    pub fn cancel_login(&self) {
        if let Some((mut child, _)) = self.login.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
            self.push(ChatRole::Assistant, "Sign-in cancelled.".to_string());
        }
    }

    fn login_finished(&self) {
        let Some((mut child, _)) = self.login.borrow_mut().take() else {
            return;
        };
        let _ = child.wait();
        self.refresh_account(true);
    }

    pub fn logout(&self) {
        self.stop();
        self.usage.set(None);
        let chat = self.clone();
        let send = create_ext_action(self.scope, move |result: Result<(), String>| {
            if let Err(e) = result {
                chat.push(ChatRole::Error, format!("Failed to log out: {e}"));
            }
            chat.refresh_account(true);
        });
        std::thread::spawn(move || {
            let output = claude_command().args(["auth", "logout"]).output();
            send(match output {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Err(e) => Err(e.to_string()),
            });
        });
    }

    /// Read the account from `claude auth status`. `announce` says the result
    /// in the chat and restarts Claude Code under the new account.
    fn refresh_account(&self, announce: bool) {
        let chat = self.clone();
        let send = create_ext_action(self.scope, move |account: Option<String>| {
            chat.account.set(account.clone());
            if !announce {
                return;
            }
            let Some(account) = account else {
                chat.push(ChatRole::Assistant, "Signed out.".to_string());
                return;
            };
            chat.push(ChatRole::Assistant, format!("Signed in as {account}."));
            // Usage, models and context of the new account
            let running = chat.process.borrow().is_some();
            if !running && let Err(e) = chat.spawn() {
                chat.push(ChatRole::Error, format!("Failed to start claude: {e}"));
            }
        });
        std::thread::spawn(move || {
            let output = claude_command()
                .args(["auth", "status", "--json"])
                .output();
            send(output.ok().and_then(|o| parse_account(&o.stdout)));
        });
    }

    /// Look up the newest Claude Code on npm, for the update badge
    fn check_latest_version(&self) {
        let latest = self.latest_version;
        let send = create_ext_action(self.scope, move |version: Option<String>| {
            latest.set(version);
        });
        std::thread::spawn(move || {
            let url = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";
            let version = lapce_proxy::get_url(url, Some("Lapce"))
                .ok()
                .and_then(|resp| resp.json::<Value>().ok())
                .and_then(|json| json["version"].as_str().map(str::to_string));
            send(version);
        });
    }

    /// The newer version on npm, None when up to date or not known yet
    pub fn update_available(&self) -> Option<String> {
        let latest = self.latest_version.get()?;
        let current = self.version.get()?;
        is_newer(&latest, &current).then_some(latest)
    }

    /// Run `claude update`, then restart Claude Code so the next message
    /// uses the new version
    pub fn update(&self) {
        if self.updating.get_untracked() {
            return;
        }
        if self.busy.get_untracked() {
            let text = "Stop or wait for the reply before updating.";
            self.push(ChatRole::Error, text.to_string());
            return;
        }
        self.updating.set(true);
        self.push(ChatRole::Assistant, "Updating Claude Code...".to_string());
        let chat = self.clone();
        let send = create_ext_action(self.scope, move |result: Result<_, String>| {
            chat.updating.set(false);
            match result {
                Ok(text) => {
                    chat.push(ChatRole::Assistant, text);
                    // The running process is still the old version
                    chat.stop();
                    chat.version.set(None);
                    if let Err(e) = chat.spawn() {
                        let text = format!("Failed to start claude: {e}");
                        chat.push(ChatRole::Error, text);
                    }
                }
                Err(text) => chat.push(ChatRole::Error, text),
            }
        });
        std::thread::spawn(move || send(run_update()));
    }

    /// `~/.claude/projects/<workspace path with non-alphanumerics as '-'>`
    fn sessions_dir(&self) -> Option<PathBuf> {
        let cwd = self
            .common
            .workspace
            .path
            .clone()
            .or_else(|| std::env::current_dir().ok())?;
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| Directory::home_dir().map(|home| home.join(".claude")))?;
        Some(config.join("projects").join(project_dir_name(&cwd)))
    }

    fn spawn(&self) -> std::io::Result<()> {
        let mut cmd = claude_command();
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            // Tool approvals come to us as `can_use_tool` control requests
            "--permission-prompt-tool",
            "stdio",
            // Emits a `prompt_suggestion` message after each turn
            "--prompt-suggestions",
            "true",
        ]);
        // Lets the permission toggle switch to bypass without a restart
        cmd.arg("--allow-dangerously-skip-permissions");
        // Browser tools from the Claude in Chrome extension
        cmd.arg("--chrome");
        let mode = self.permission_mode.get_untracked();
        if mode != "default" {
            cmd.args(["--permission-mode", mode]);
        }
        let model = self.model.get_untracked();
        if let Some(model) = &model {
            cmd.args(["--model", model]);
        }
        let effort = self.effort.get_untracked();
        if let Some(effort) = &effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(id) = self.session_id.get_untracked() {
            cmd.args(["--resume", &id]);
        }
        if let Some(path) = self.common.workspace.path.as_ref() {
            cmd.current_dir(path);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            if !err.trim().is_empty() {
                let event = json!({ "type": "stderr", "text": err });
                let _ = tx.send((generation, event));
            }
        });
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(event) = serde_json::from_str::<Value>(&line)
                    && tx.send((generation, event)).is_err()
                {
                    return;
                }
            }
            let _ = tx.send((generation, json!({"type": "exit"})));
        });

        *self.process.borrow_mut() = Some(ChatProcess {
            child,
            stdin,
            effort,
        });
        // Ask for the slash commands, answered without an API request
        self.write_line(&json!({
            "type": "control_request",
            "request_id": "lapce-initialize",
            "request": { "subtype": "initialize" },
        }))?;
        self.request_usage();
        self.request_settings();
        // A resumed session shows how full it is before the first message
        self.control("lapce-context", json!({ "subtype": "get_context_usage" }));
        self.schedule_idle_stop();
        Ok(())
    }

    /// Stop the process once nothing happened for `IDLE_STOP`, unless a turn
    /// or an approval is still pending
    fn schedule_idle_stop(&self) {
        let token = self.idle_token.get() + 1;
        self.idle_token.set(token);
        let chat = self.clone();
        exec_after(IDLE_STOP, move |_| {
            let quiet = chat.idle_token.get() == token
                && !chat.busy.get_untracked()
                && chat.permissions.with_untracked(|p| p.is_empty());
            if quiet {
                chat.stop();
            }
        });
    }

    /// Ask for plan usage, read from the claude.ai usage endpoint, no model request
    /// Ask which model and effort apply, answered without an API request
    fn request_settings(&self) {
        self.control("lapce-settings", json!({ "subtype": "get_settings" }));
    }

    fn request_usage(&self) {
        let _ = self.write_line(&json!({
            "type": "control_request",
            "request_id": "lapce-usage",
            "request": { "subtype": "get_usage", "skip_behaviors": true },
        }));
    }

    fn handle_event(&self, event: &Value) {
        // Subagent events carry their parent tool id, keep the main thread only
        if !event["parent_tool_use_id"].is_null() {
            return;
        }
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
                if let Some(id) = event["session_id"].as_str() {
                    self.session_id.set(Some(id.to_string()));
                }
                if let Some(version) = event["claude_code_version"].as_str() {
                    self.version.set(Some(version.to_string()));
                }
            }
            Some("system") if event["subtype"] == "commands_changed" => {
                if let Some(commands) = parse_commands(&event["commands"]) {
                    self.commands.set(commands);
                }
            }
            Some("control_request") => {
                if let Some(request) = parse_permission(event) {
                    self.notify(&format!("Waiting for approval: {}", request.title));
                    self.permissions.update(|p| p.push_back(request));
                }
            }
            Some("prompt_suggestion") => {
                self.suggestion.set(parse_suggestion(event));
            }
            Some("control_cancel_request") => {
                if let Some(id) = event["request_id"].as_str() {
                    self.permissions.update(|p| p.retain(|r| r.request_id != id));
                }
            }
            Some("control_response")
                if event["response"]["request_id"] == "lapce-settings" =>
            {
                let applied = &event["response"]["response"]["applied"];
                if applied.is_object() {
                    self.applied.set(Some(parse_applied(applied)));
                }
            }
            Some("control_response")
                if event["response"]["request_id"] == "lapce-context" =>
            {
                if let Some(usage) = parse_context(&event["response"]["response"]) {
                    if let Some(idle) = self.resume_idle.take() {
                        self.resume_warning.set(resume_warning(idle, usage.tokens));
                    }
                    // Keep the cost, only a turn's result reports it
                    let cost = self.context.with_untracked(|c| c.as_ref()?.cost);
                    self.context.set(Some(ContextUsage { cost, ..usage }));
                }
            }
            Some("control_response")
                if event["response"]["request_id"] == "lapce-usage" =>
            {
                if let Some(usage) = parse_usage(&event["response"]["response"]) {
                    self.usage.set(Some(usage));
                }
            }
            // Only the switches report, a failed usage or command list
            // request stays quiet
            Some("control_response")
                if event["response"]["subtype"] == "error"
                    && matches!(
                        event["response"]["request_id"].as_str(),
                        Some("lapce-set-model" | "lapce-permission-mode")
                    ) =>
            {
                let error = event["response"]["error"].as_str();
                let error = error.unwrap_or("Claude Code refused a request");
                self.push(ChatRole::Error, error.to_string());
            }
            Some("control_response") => {
                let response = &event["response"]["response"];
                if let Some(commands) = parse_commands(&response["commands"]) {
                    self.commands.set(commands);
                }
                if let Some(models) = parse_models(&response["models"]) {
                    self.models.set(models);
                }
            }
            Some("stream_event") => {
                let event = &event["event"];
                match event["type"].as_str() {
                    Some("content_block_start")
                        if event["content_block"]["type"] == "text" =>
                    {
                        self.push(ChatRole::Assistant, String::new());
                    }
                    Some("content_block_delta")
                        if event["delta"]["type"] == "text_delta" =>
                    {
                        if let Some(delta) = event["delta"]["text"].as_str() {
                            self.append_assistant(delta);
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                // Subagent requests have their own context
                let usage = &event["message"]["usage"];
                if event["parent_tool_use_id"].is_null() && usage.is_object() {
                    let model = event["message"]["model"].as_str();
                    self.context.update(|context| {
                        let context = context.get_or_insert_default();
                        context.tokens = request_tokens(usage);
                        context.model = model.unwrap_or_default().to_string();
                    });
                }
                let blocks = event["message"]["content"].as_array();
                for block in blocks.into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        self.push_message(
                            ChatRole::Tool,
                            tool_summary(block),
                            Some(tool_call(block)),
                            ToolStatus::Running,
                        );
                    }
                }
            }
            Some("user") => {
                let blocks = event["message"]["content"].as_array();
                for block in blocks.into_iter().flatten() {
                    if block["type"] == "tool_result"
                        && let Some(id) = block["tool_use_id"].as_str()
                    {
                        let output = result_text(&block["content"]);
                        self.set_tool_result(id, &output, block["is_error"] == true);
                    }
                }
            }
            Some("result") => {
                self.busy.set(false);
                self.schedule_idle_stop();
                self.request_usage();
                self.context.update(|context| {
                    if let Some(context) = context {
                        let usage = &event["modelUsage"];
                        let window = usage[&context.model]["contextWindow"].as_u64();
                        context.window = window.or(context.window);
                        let cost = event["total_cost_usd"].as_f64();
                        context.cost = cost.or(context.cost);
                    }
                });
                if event["is_error"] == true {
                    let text = event["result"].as_str().unwrap_or("Request failed");
                    self.notify(text);
                    self.push(ChatRole::Error, text.to_string());
                } else {
                    self.notify("Finished");
                }
            }
            Some("stderr") => {
                if let Some(text) = event["text"].as_str() {
                    self.push(ChatRole::Error, text.trim().to_string());
                }
            }
            Some("exit") => {
                if let Some(mut p) = self.process.borrow_mut().take() {
                    let _ = p.child.wait();
                }
                self.busy.set(false);
            }
            _ => {}
        }
    }

    /// A system notification while this window isn't focused, so a long
    /// turn needn't be watched
    fn notify(&self, body: &str) {
        if !self.common.window_common.window_focused.get_untracked() {
            let config = self.common.config.get_untracked();
            system_notification(body, &config.core.claude_notification_sound);
        }
    }

    fn push(&self, role: ChatRole, text: String) {
        self.push_message(role, text, None, ToolStatus::Done);
    }

    fn push_message(
        &self,
        role: ChatRole,
        text: String,
        tool: Option<ToolCall>,
        status: ToolStatus,
    ) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let previous = self.messages.with_untracked(|m| m.last().map(|m| m.role));
        let first_in_turn = role == ChatRole::User
            || previous.is_none_or(|previous| previous == ChatRole::User);
        let msg = ChatMessage {
            id,
            role,
            text: self.scope.create_rw_signal(text),
            tool,
            status: self.scope.create_rw_signal(status),
            result: self.scope.create_rw_signal(None),
            first_in_turn,
        };
        self.messages.update(|messages| messages.push_back(msg));
    }

    fn set_tool_result(&self, tool_use_id: &str, output: &str, failed: bool) {
        self.messages.with_untracked(|messages| {
            let msg = messages.iter().rev().find(|msg| {
                msg.tool.as_ref().is_some_and(|tool| tool.id == tool_use_id)
            });
            let Some(msg) = msg else { return };
            let Some(tool) = &msg.tool else { return };
            msg.status.set(if failed {
                ToolStatus::Failed
            } else {
                ToolStatus::Done
            });
            msg.result.set(result_summary(&tool.name, output, failed));
        });
    }

    fn append_assistant(&self, delta: &str) {
        let last = self.messages.with_untracked(|m| m.last().cloned());
        match last {
            Some(msg) if msg.role == ChatRole::Assistant => {
                msg.text.update(|text| text.push_str(delta));
            }
            _ => self.push(ChatRole::Assistant, delta.to_string()),
        }
    }
}

// ponytail: assumes the plain mapping, sessions of very long or unusual
// workspace paths may be stored under another name and not be listed
fn project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Most recent sessions first, titled by Claude Code's own title or the
/// last prompt
fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    sessions.sort_by(|a, b| b.1.cmp(&a.1));
    sessions.truncate(30);

    sessions
        .into_iter()
        .filter_map(|(path, modified)| {
            let id = path.file_stem()?.to_str()?.to_string();
            let file = File::open(&path).ok()?;
            let title = session_title(BufReader::new(file))?;
            Some(SessionInfo {
                id,
                title,
                modified,
            })
        })
        .collect()
}

/// The first `ai-title` record, else the last prompt. `None` for sessions
/// without a prompt.
fn session_title(reader: impl BufRead) -> Option<String> {
    let mut prompt = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        // Cheap filter before parsing, session files can be megabytes
        if !line.contains("\"ai-title\"") && !line.contains("\"last-prompt\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(title) = record["aiTitle"].as_str() {
            return Some(title.to_string());
        }
        if let Some(text) = record["lastPrompt"].as_str() {
            prompt = Some(text.lines().next().unwrap_or_default().to_string());
        }
    }
    prompt
}

/// Rebuild the visible chat from a session file
fn read_history(reader: impl BufRead) -> Vec<HistoryItem> {
    let mut history = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if record["isSidechain"] == true || record["isMeta"] == true {
            continue;
        }
        let content = &record["message"]["content"];
        match record["type"].as_str() {
            Some("user") => {
                // Typed prompts are strings; local commands and their output
                // are wrapped in tags
                if let Some(text) = content.as_str()
                    && !text.starts_with('<')
                {
                    history.push((ChatRole::User, text.to_string(), None));
                }
            }
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or_default();
                            let text = text.to_string();
                            history.push((ChatRole::Assistant, text, None));
                        }
                        Some("tool_use") => {
                            let summary = tool_summary(block);
                            let tool = Some(tool_call(block));
                            history.push((ChatRole::Tool, summary, tool));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    history
}

/// None when plan limits do not apply (API key, Bedrock, Vertex)
fn parse_usage(response: &Value) -> Option<UsageLimits> {
    let limits = response["rate_limits"].as_object()?;
    let window = |key: &str| {
        let window = limits.get(key)?;
        Some(UsageWindow {
            percent: window["utilization"].as_f64()?,
            resets_at: window["resets_at"]
                .as_str()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Local)),
        })
    };
    Some(UsageLimits {
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
    })
}

fn parse_applied(applied: &Value) -> AppliedSettings {
    let field = |key: &str| applied[key].as_str().map(str::to_string);
    AppliedSettings {
        model: field("model"),
        effort: field("effort"),
    }
}

/// `claude` without a console window on Windows
fn claude_command() -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new("claude");
    #[cfg(target_os = "windows")]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x08000000);
    cmd
}

/// Send what a login process prints as it comes, its code prompt has no
/// newline
fn forward_output(mut output: impl Read, tx: &Sender<Option<String>>) {
    let mut buf = [0; 4096];
    while let Ok(n) = output.read(&mut buf) {
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..n]).trim().to_string();
        if !text.is_empty() && tx.send(Some(text)).is_err() {
            break;
        }
    }
}

/// The email, or the auth method when there is none, None when signed out
fn parse_account(status: &[u8]) -> Option<String> {
    let status: Value = serde_json::from_slice(status).ok()?;
    if status["loggedIn"] != true {
        return None;
    }
    let account = status["email"].as_str().or(status["authMethod"].as_str());
    Some(account.unwrap_or("Claude").to_string())
}

/// From `get_context_usage`, which counts the whole history without a model
/// request
fn parse_context(response: &Value) -> Option<ContextUsage> {
    Some(ContextUsage {
        tokens: response["totalTokens"].as_u64()?,
        window: response["maxTokens"].as_u64(),
        cost: None,
        model: response["model"].as_str().unwrap_or_default().to_string(),
    })
}

fn parse_models(models: &Value) -> Option<Vec<ModelInfo>> {
    let field =
        |m: &Value, key: &str| m[key].as_str().unwrap_or_default().to_string();
    let models = models.as_array()?.iter().map(|m| ModelInfo {
        value: field(m, "value"),
        name: field(m, "displayName"),
        description: field(m, "description"),
        resolved: field(m, "resolvedModel"),
        efforts: m["supportedEffortLevels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some(e.as_str()?.to_string()))
            .collect(),
    });
    Some(models.filter(|m| !m.value.is_empty()).collect())
}

fn parse_commands(commands: &Value) -> Option<Vec<SlashCommand>> {
    let commands = commands.as_array()?;
    let field =
        |c: &Value, key: &str| c[key].as_str().unwrap_or_default().to_string();
    Some(
        commands
            .iter()
            .filter(|c| c["name"].is_string())
            .map(|c| SlashCommand {
                name: field(c, "name"),
                description: field(c, "description"),
                hint: field(c, "argumentHint"),
            })
            .collect(),
    )
}

/// Pastes longer than this become a placeholder in the input
const PASTE_MAX_LINES: usize = 10;
const PASTE_MAX_LEN: usize = 1000;

/// Paste `n` (1-based) as shown in the input, like the CLI shows it
fn paste_placeholder(n: usize, text: &str) -> String {
    format!("[Pasted text #{n} +{} lines]", text.lines().count())
}

/// The input with paste placeholders replaced by what was pasted
/// The prompt index after Up (`older`) or Down from `index`, None when
/// the key goes nowhere, Some(None) back to the typed input
fn history_step(
    len: usize,
    index: Option<usize>,
    older: bool,
) -> Option<Option<usize>> {
    match (index, older) {
        (None, true) => len.checked_sub(1).map(Some),
        (Some(i), true) => i.checked_sub(1).map(Some),
        (None, false) => None,
        (Some(i), false) => Some((i + 1 < len).then_some(i + 1)),
    }
}

fn expand_pastes(input: &str, pastes: &[(String, String)]) -> String {
    let mut input = input.to_string();
    for (placeholder, content) in pastes {
        input = input.replace(placeholder, content);
    }
    input
}

/// The clipboard's PNG, as a screenshot copied with Cmd+Ctrl+Shift+4 has
#[cfg(target_os = "macos")]
fn clipboard_png() -> Option<Vec<u8>> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
    // SAFETY: an AppKit constant, set before any code runs
    let png = unsafe { NSPasteboardTypePNG };
    let data = NSPasteboard::generalPasteboard().dataForType(png)?;
    Some(data.to_vec())
}

#[cfg(not(target_os = "macos"))]
fn clipboard_png() -> Option<Vec<u8>> {
    None
}

/// Commands for the popup while the input is `/` plus a partial name.
/// Name prefix matches come first, then other substring matches.
fn filter_commands(commands: &[SlashCommand], input: &str) -> Vec<SlashCommand> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let (mut prefix, rest): (Vec<_>, Vec<_>) = commands
        .iter()
        .filter(|c| c.name.to_lowercase().contains(&query))
        .cloned()
        .partition(|c| c.name.to_lowercase().starts_with(&query));
    prefix.extend(rest);
    prefix.truncate(8);
    prefix
}

/// `@path`, relative inside the workspace, quoted when it has spaces
fn path_mention(path: &Path, workspace: Option<&Path>) -> String {
    let path = workspace
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .to_string_lossy();
    if path.contains(char::is_whitespace) {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    }
}

/// Shown by the OS as coming from Lapce when it runs as an app bundle,
/// else through a command with the body passed as an argument, so it needs
/// no escaping. Quietly does nothing where the command is missing.
// The sound is macOS only
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn system_notification(body: &str, sound: &str) {
    let body: String = body.chars().take(200).collect();
    #[cfg(target_os = "macos")]
    if objc2_foundation::NSBundle::mainBundle()
        .bundleIdentifier()
        .is_some()
    {
        mac_notification(&body, sound);
        return;
    }
    // Run from outside an app bundle, e.g. `cargo run`
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let show = if sound.is_empty() {
            "display notification (item 1 of argv) with title \"Claude\""
        } else {
            "display notification (item 1 of argv) with title \"Claude\" \
             sound name (item 2 of argv)"
        };
        let mut cmd = Command::new("osascript");
        cmd.args([
            "-e",
            "on run argv",
            "-e",
            show,
            "-e",
            "end run",
            "--",
            body.as_str(),
            sound,
        ]);
        cmd
    };
    #[cfg(not(target_os = "macos"))]
    let mut cmd = {
        let mut cmd = Command::new("notify-send");
        cmd.args(["--", "Claude", body.as_str()]);
        cmd
    };
    // Waited on so the finished process isn't left a zombie
    std::thread::spawn(move || {
        let _ = cmd.status();
    });
}

/// Delivered by the app itself, so it shows Lapce's name and icon
// ponytail: NSUserNotification is deprecated, move to UserNotifications
// when it stops delivering. That one needs a signed app and a permission.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn mac_notification(body: &str, sound: &str) {
    use objc2_foundation::{NSString, NSUserNotification, NSUserNotificationCenter};
    let notification = NSUserNotification::new();
    notification.setTitle(Some(&NSString::from_str("Claude")));
    notification.setInformativeText(Some(&NSString::from_str(body)));
    if !sound.is_empty() {
        notification.setSoundName(Some(&NSString::from_str(sound)));
    }
    NSUserNotificationCenter::defaultUserNotificationCenter()
        .deliverNotification(&notification);
}

/// What `claude update` printed, or why it failed
fn run_update() -> Result<String, String> {
    let output = claude_command()
        .arg("update")
        .output()
        .map_err(|e| format!("Failed to run claude update: {e}"))?;
    let text = |bytes: &[u8], fallback: &str| {
        let text = String::from_utf8_lossy(bytes).trim().to_string();
        if text.is_empty() {
            fallback.to_string()
        } else {
            text
        }
    };
    if output.status.success() {
        Ok(text(&output.stdout, "Claude Code updated."))
    } else {
        Err(text(&output.stderr, "claude update failed"))
    }
}

/// Whether version `latest` is newer than `current`, false when either
/// isn't a version
fn is_newer(latest: &str, current: &str) -> bool {
    match (semver::Version::parse(latest), semver::Version::parse(current)) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    }
}

/// `line 3` or `lines 3-7`
fn line_range(start: usize, end: usize) -> String {
    if start == end {
        format!("line {start}")
    } else {
        format!("lines {start}-{end}")
    }
}

/// A fix request listing each diagnostic with its 1-based line
fn diagnostics_prompt(mention: &str, diagnostics: &[Diagnostic]) -> String {
    let mut prompt = format!("Fix these problems in {mention}:");
    for d in diagnostics {
        let severity = match d.severity {
            Some(DiagnosticSeverity::ERROR) => "error",
            Some(DiagnosticSeverity::WARNING) => "warning",
            Some(DiagnosticSeverity::INFORMATION) => "info",
            Some(DiagnosticSeverity::HINT) => "hint",
            _ => "problem",
        };
        let range = line_range(
            d.range.start.line as usize + 1,
            d.range.end.line as usize + 1,
        );
        let source = d
            .source
            .as_ref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        // Keep multi-line messages under their bullet
        let message = d.message.trim().replace('\n', "\n  ");
        prompt.push_str(&format!("\n- {range}, {severity}{source}: {message}"));
    }
    prompt
}

/// The `@` mention being typed at the end of the input, without the `@`
pub fn mention_query(input: &str) -> Option<&str> {
    last_token(input).strip_prefix('@')
}

/// Text after the last whitespace, empty when the input ends with whitespace
fn last_token(input: &str) -> &str {
    input.rsplit(char::is_whitespace).next().unwrap_or_default()
}

fn replace_last_token(input: &str, insert: &str) -> String {
    let start = input.len() - last_token(input).len();
    format!("{}{insert}", &input[..start])
}

/// File name prefix matches first, then any path containing the query
fn filter_files(files: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let (mut prefix, rest): (Vec<_>, Vec<_>) = files
        .iter()
        .filter(|path| path.to_lowercase().contains(&query))
        .cloned()
        .partition(|path| {
            let name = path.rsplit(['/', '\\']).next().unwrap_or_default();
            name.to_lowercase().starts_with(&query)
        });
    prefix.extend(rest);
    prefix.truncate(8);
    prefix
}

/// The predicted prompt. The CLI help names the message but not its fields,
/// so take the first known text field.
fn parse_suggestion(event: &Value) -> Option<String> {
    ["suggestion", "prompt", "text"]
        .iter()
        .find_map(|key| event[key].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_permission(event: &Value) -> Option<PermissionRequest> {
    let request = &event["request"];
    if request["subtype"] != "can_use_tool" {
        return None;
    }
    let name = request["display_name"]
        .as_str()
        .or(request["tool_name"].as_str())
        .unwrap_or("a tool");
    let input = &request["input"];
    // Show the full input, the user decides on exactly this
    let main = ["command", "file_path", "pattern", "url"]
        .iter()
        .find_map(|key| input[key].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string_pretty(input).unwrap_or_default());
    let detail = match request["description"].as_str() {
        Some(description) if description != main => format!("{description}\n{main}"),
        _ => main,
    };
    Some(PermissionRequest {
        request_id: event["request_id"].as_str()?.to_string(),
        title: format!("Allow {name}?"),
        detail,
        input: input.clone(),
        suggestions: request["permission_suggestions"].clone(),
    })
}

fn permission_response(
    request: &PermissionRequest,
    decision: PermissionDecision,
    message: Option<&str>,
) -> Value {
    let response = match decision {
        PermissionDecision::Allow => {
            json!({ "behavior": "allow", "updatedInput": request.input })
        }
        PermissionDecision::AlwaysAllow => json!({
            "behavior": "allow",
            "updatedInput": request.input,
            "updatedPermissions": request.suggestions,
        }),
        PermissionDecision::Deny => json!({
            "behavior": "deny",
            "message": message.unwrap_or("The user denied this in Lapce"),
        }),
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request.request_id,
            "response": response,
        },
    })
}

/// File and change preview of a tool call
fn tool_call(block: &Value) -> ToolCall {
    let name = block["name"].as_str().unwrap_or_default();
    let input = &block["input"];
    let file = input["file_path"]
        .as_str()
        .or(input["notebook_path"].as_str())
        .map(PathBuf::from);
    let diff_lines = |edit: &Value| {
        let old = edit["old_string"].as_str().unwrap_or_default().lines();
        let new = edit["new_string"].as_str().unwrap_or_default().lines();
        let old = old.map(|l| format!("-{l}"));
        old.chain(new.map(|l| format!("+{l}"))).collect::<Vec<_>>()
    };
    let lines = match name {
        "Edit" => Some(diff_lines(input)),
        "MultiEdit" => Some(
            input["edits"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(diff_lines)
                .collect(),
        ),
        "Write" => Some(
            input["content"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(|l| format!("+{l}"))
                .collect(),
        ),
        _ => None,
    };
    let diff = lines.filter(|l| !l.is_empty()).map(|mut lines| {
        // ponytail: fixed cap, long edits are reviewed via "Changes"
        const MAX: usize = 30;
        if lines.len() > MAX {
            let more = lines.len() - MAX;
            lines.truncate(MAX);
            lines.push(format!("… {more} more lines"));
        }
        lines.join("\n")
    });
    ToolCall {
        id: block["id"].as_str().unwrap_or_default().to_string(),
        name: name.to_string(),
        file,
        edits_file: matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit"),
        diff,
    }
}

/// Tokens one request put in the context: its prompt, cached or not, and
/// its reply
fn request_tokens(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ]
    .iter()
    .filter_map(|key| usage[key].as_u64())
    .sum()
}

/// Text of a `tool_result` content, a string or a list of text blocks
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// "Read 256 lines" or the first line of a tool's output. None for a
/// successful edit, its card already shows the change.
fn result_summary(name: &str, output: &str, failed: bool) -> Option<String> {
    let edits = matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit");
    if edits && !failed {
        return None;
    }
    if name == "Read" && !failed {
        return Some(format!("Read {} lines", output.lines().count()));
    }
    let mut lines = output.lines().map(str::trim).filter(|l| !l.is_empty());
    let first: String = lines.next()?.chars().take(120).collect();
    Some(match lines.count() {
        0 => first,
        more => format!("{first} … +{more} lines"),
    })
}

/// One line describing a tool call, e.g. "Bash: cargo test"
fn tool_summary(block: &Value) -> String {
    let name = block["name"].as_str().unwrap_or("Tool");
    let input = &block["input"];
    let detail = ["command", "file_path", "pattern", "url", "description"]
        .iter()
        .find_map(|key| input[key].as_str());
    match detail {
        Some(detail) => {
            let detail = detail.lines().next().unwrap_or_default();
            let detail: String = detail.chars().take(200).collect();
            format!("{name}: {detail}")
        }
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ChatRole, PermissionDecision, SlashCommand, filter_commands, filter_files,
        mention_query, parse_commands, parse_permission, parse_suggestion,
        parse_usage, path_mention, permission_response, project_dir_name,
        parse_applied, parse_models, read_history, replace_last_token,
        request_tokens, result_summary, result_text, session_title, tool_call,
        tool_summary, expand_pastes, paste_placeholder, diagnostics_prompt,
        line_range, is_newer, history_step,
    };

    #[test]
    fn history_walks_prompts() {
        assert_eq!(history_step(0, None, true), None);
        assert_eq!(history_step(3, None, true), Some(Some(2)));
        assert_eq!(history_step(3, Some(0), true), None);
        assert_eq!(history_step(3, Some(1), false), Some(Some(2)));
        assert_eq!(history_step(3, Some(2), false), Some(None));
        assert_eq!(history_step(3, None, false), None);
    }

    #[test]
    fn pastes_expand_on_send() {
        let paste = |n, text: &str| (paste_placeholder(n, text), text.to_string());
        let pastes = vec![
            paste(1, "a\nb"),
            paste(2, "c"),
            ("[Image #3]".to_string(), "@/tmp/p.png".to_string()),
        ];
        let input =
            format!("fix {} and {} like [Image #3]", pastes[0].0, pastes[1].0);
        assert!(input.contains("[Pasted text #1 +2 lines]"));
        assert_eq!(
            expand_pastes(&input, &pastes),
            "fix a\nb and c like @/tmp/p.png"
        );
        assert_eq!(expand_pastes("no paste", &pastes), "no paste");
    }

    #[test]
    fn request_tokens_counts_cache() {
        let usage = json!({
            "input_tokens": 10,
            "cache_creation_input_tokens": 10601,
            "cache_read_input_tokens": 12231,
            "output_tokens": 6,
        });
        assert_eq!(request_tokens(&usage), 22848);
        assert_eq!(request_tokens(&json!(null)), 0);
    }

    #[test]
    fn models_from_initialize() {
        let models = json!([
            {"value": "default", "displayName": "Default (recommended)",
             "description": "Sonnet 5 · Efficient",
             "supportedEffortLevels": ["low"]},
            {"value": "haiku", "displayName": "Haiku",
             "description": "Haiku 4.5 · Fastest"},
        ]);
        let models = parse_models(&models).unwrap();
        assert_eq!(models[0].version(), "Sonnet 5");
        assert_eq!(models[0].efforts, ["low"]);
        assert!(models[1].efforts.is_empty());
        assert!(parse_models(&json!(null)).is_none());

        let applied = json!({"model": "claude-opus-5", "effort": "xhigh"});
        let applied = parse_applied(&applied);
        assert_eq!(applied.effort.as_deref(), Some("xhigh"));
        assert_eq!(parse_applied(&json!({"effort": null})).model, None);
    }

    #[test]
    fn resume_warning_needs_idle_and_size() {
        let hour = 3_600;
        assert!(resume_warning(2 * hour, 427_805).is_some());
        assert!(resume_warning(30 * 60, 427_805).is_none());
        assert!(resume_warning(2 * hour, 20_000).is_none());
        assert!(resume_warning(RESUME_IDLE_SECS, RESUME_TOKENS).is_some());
    }

    #[test]
    fn account_from_auth_status() {
        let account = |status: Value| parse_account(status.to_string().as_bytes());
        let email = json!({"loggedIn": true, "email": "a@b"});
        assert_eq!(account(email).as_deref(), Some("a@b"));
        let key = json!({"loggedIn": true, "authMethod": "api_key"});
        assert_eq!(account(key).as_deref(), Some("api_key"));
        assert_eq!(account(json!({"loggedIn": false})), None);
        assert_eq!(parse_account(b"not json"), None);
    }

    #[test]
    fn context_from_usage_request() {
        let response = json!({
            "totalTokens": 427805,
            "maxTokens": 1000000,
            "model": "claude-opus-5-5",
        });
        let context = parse_context(&response).unwrap();
        assert_eq!(context.tokens, 427805);
        assert_eq!(context.percent().map(|p| p.round()), Some(43.0));
        assert_eq!(context.model, "claude-opus-5-5");
        assert!(parse_context(&json!({})).is_none());
    }

    #[test]
    fn result_summary_per_tool() {
        let content = json!([{"type": "text", "text": "a\n\nb\nc"}]);
        let output = result_text(&content);
        let summary = |name| result_summary(name, &output, false).unwrap();
        assert_eq!(summary("Read"), "Read 4 lines");
        assert_eq!(summary("Bash"), "a … +2 lines");
        assert_eq!(result_summary("Edit", "ok", false), None);
        assert_eq!(result_summary("Edit", "no match", true).unwrap(), "no match");
        assert_eq!(result_summary("Bash", "", false), None);
    }

    #[test]
    fn tool_summary_picks_main_input() {
        let bash = json!({"name": "Bash", "input": {"command": "ls\nrm x"}});
        assert_eq!(tool_summary(&bash), "Bash: ls");
        let read = json!({"name": "Read", "input": {"file_path": "/a.rs"}});
        assert_eq!(tool_summary(&read), "Read: /a.rs");
        let other = json!({"name": "TodoWrite", "input": {"todos": []}});
        assert_eq!(tool_summary(&other), "TodoWrite");
    }

    #[test]
    fn commands_filter_by_prefix_then_substring() {
        let commands = parse_commands(&json!([
            {"name": "review", "description": "Review", "argumentHint": "[pr]"},
            {"name": "compact", "description": "Compact"},
            {"name": "caveman:caveman-review"},
            {"description": "no name"},
        ]))
        .unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].hint, "[pr]");

        let names = |input: &str| -> Vec<String> {
            filter_commands(&commands, input)
                .into_iter()
                .map(|c: SlashCommand| c.name)
                .collect()
        };
        assert_eq!(names("/rev"), ["review", "caveman:caveman-review"]);
        assert_eq!(names("/"), ["review", "compact", "caveman:caveman-review"]);
        assert!(names("/review now").is_empty());
        assert!(names("review").is_empty());
    }

    #[test]
    fn permission_round_trip() {
        // Shape captured from claude 2.1.307
        let event = json!({
            "type": "control_request",
            "request_id": "r1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "display_name": "Bash",
                "input": {"command": "touch a.txt", "description": "Create file"},
                "description": "Create file",
                "permission_suggestions": [{"type": "addRules"}],
            },
        });
        let request = parse_permission(&event).unwrap();
        assert_eq!(request.title, "Allow Bash?");
        assert_eq!(request.detail, "Create file\ntouch a.txt");
        assert!(request.can_always_allow());

        let allow = permission_response(&request, PermissionDecision::Allow, None);
        assert_eq!(allow["response"]["request_id"], "r1");
        let inner = &allow["response"]["response"];
        assert_eq!(inner["behavior"], "allow");
        assert_eq!(inner["updatedInput"]["command"], "touch a.txt");
        assert!(inner["updatedPermissions"].is_null());

        let always =
            permission_response(&request, PermissionDecision::AlwaysAllow, None);
        let rules = &always["response"]["response"]["updatedPermissions"];
        assert_eq!(rules[0]["type"], "addRules");

        let deny = permission_response(&request, PermissionDecision::Deny, None);
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        let instead =
            permission_response(&request, PermissionDecision::Deny, Some("Use ls"));
        assert_eq!(instead["response"]["response"]["message"], "Use ls");
        assert_eq!(request.options().len(), 3);

        let other = json!({"type": "control_request", "request": {"subtype": "x"}});
        assert!(parse_permission(&other).is_none());
    }

    #[test]
    fn tool_call_previews_edits() {
        let edit = json!({"name": "Edit", "input": {
            "file_path": "/w/a.rs", "old_string": "a\nb", "new_string": "c",
        }});
        let call = tool_call(&edit);
        assert_eq!(call.file, Some("/w/a.rs".into()));
        assert!(call.edits_file);
        assert_eq!(call.diff.as_deref(), Some("-a\n-b\n+c"));

        let content = "x\n".repeat(40);
        let write = json!({"name": "Write", "input": {
            "file_path": "/w/b.rs", "content": content,
        }});
        let diff = tool_call(&write).diff.unwrap();
        assert_eq!(diff.lines().count(), 31);
        assert!(diff.ends_with("… 10 more lines"));

        let read = json!({"name": "Read", "input": {"file_path": "/w/a.rs"}});
        let call = tool_call(&read);
        assert!(!call.edits_file);
        assert!(call.diff.is_none());
        assert!(call.file.is_some());
    }

    #[test]
    fn file_mentions() {
        assert_eq!(mention_query("look at @src/ma"), Some("src/ma"));
        assert_eq!(mention_query("@"), Some(""));
        assert_eq!(mention_query("@src "), None);
        assert_eq!(mention_query("email a@b"), None);
        assert_eq!(
            replace_last_token("look at @ma", "@src/main.rs "),
            "look at @src/main.rs "
        );
        assert_eq!(replace_last_token("/rev", "/review "), "/review ");

        let files = ["src/main.rs", "src/lib.rs", "domain/model.rs"]
            .map(String::from)
            .to_vec();
        assert_eq!(filter_files(&files, "ma"), ["src/main.rs", "domain/model.rs"]);
        assert_eq!(filter_files(&files, "LIB"), ["src/lib.rs"]);
    }

    #[test]
    fn suggestion_takes_known_text_field() {
        let event = json!({"type": "prompt_suggestion", "suggestion": " go on "});
        assert_eq!(parse_suggestion(&event).as_deref(), Some("go on"));
        let empty = json!({"type": "prompt_suggestion", "suggestion": ""});
        assert_eq!(parse_suggestion(&empty), None);
    }

    #[test]
    fn project_dir_name_matches_claude_code() {
        let path = std::path::Path::new("/Users/me/my_app.v2");
        assert_eq!(project_dir_name(path), "-Users-me-my-app-v2");
    }

    #[test]
    fn session_title_prefers_ai_title() {
        let file = [
            json!({"type": "last-prompt", "lastPrompt": "fix it\nplease"}),
            json!({"type": "ai-title", "aiTitle": "Fix bug"}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        assert_eq!(session_title(file.as_bytes()).as_deref(), Some("Fix bug"));
        let first = file.lines().next().unwrap();
        assert_eq!(session_title(first.as_bytes()).as_deref(), Some("fix it"));
        assert_eq!(session_title("".as_bytes()), None);
    }

    #[test]
    fn read_history_keeps_visible_messages() {
        let file = [
            json!({"type": "user", "isMeta": true, "message": {"content": "x"}}),
            json!({"type": "user", "message": {"content": "<command-name>"}}),
            json!({"type": "user", "message": {"content": "hi"}}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
            ]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {
                "content": [{"type": "text", "text": "sub"}]
            }}),
        ]
        .map(|v| v.to_string())
        .join("\n");
        let history = read_history(file.as_bytes());
        let roles: Vec<_> = history.iter().map(|(r, _, _)| *r).collect();
        assert_eq!(roles, [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]);
        assert_eq!(history[2].1, "Bash: ls");
        assert!(history[0].2.is_none());
        assert_eq!(history[2].2.as_ref().unwrap().file, None);
    }

    #[test]
    fn usage() {
        let response = json!({"rate_limits": {
            "five_hour": {"utilization": 74, "resets_at": "2026-10-22T07:09:59Z"},
            "seven_day": null,
        }});
        let usage = parse_usage(&response).unwrap();
        let five_hour = usage.five_hour.unwrap();
        assert_eq!(five_hour.percent, 74.0);
        assert_eq!(five_hour.resets_at.unwrap().timestamp(), 1_792_652_999);
        assert_eq!(usage.seven_day, None);
        assert!(parse_usage(&json!({"rate_limits": null})).is_none());
    }

    #[test]
    fn dropped_path_mention() {
        let root = std::path::Path::new("/ws");
        let mention = |p: &str| path_mention(std::path::Path::new(p), Some(root));
        assert_eq!(mention("/ws/src/main.rs"), "@src/main.rs");
        assert_eq!(mention("/other/a.rs"), "@/other/a.rs");
        assert_eq!(mention("/ws/my notes.md"), "@\"my notes.md\"");
    }

    #[test]
    fn diagnostics_fix_prompt() {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
        assert_eq!(line_range(3, 3), "line 3");
        assert_eq!(line_range(3, 7), "lines 3-7");
        let at = |line| Position { line, character: 0 };
        let error = Diagnostic {
            range: Range { start: at(11), end: at(11) },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("rustc".to_string()),
            message: "mismatched types\nexpected `u8`".to_string(),
            ..Default::default()
        };
        let no_severity = Diagnostic {
            range: Range { start: at(0), end: at(2) },
            message: "unused".to_string(),
            ..Default::default()
        };
        assert_eq!(
            diagnostics_prompt("@src/a.rs", &[error, no_severity]),
            "Fix these problems in @src/a.rs:\n\
             - line 12, error (rustc): mismatched types\n  expected `u8`\n\
             - lines 1-3, problem: unused"
        );
    }

    #[test]
    fn newer_version() {
        assert!(is_newer("2.1.310", "2.1.309"));
        assert!(is_newer("2.2.0", "2.1.310"));
        assert!(!is_newer("2.1.310", "2.1.310"));
        assert!(!is_newer("2.1.9", "2.1.10"));
        assert!(!is_newer("latest", "2.1.310"));
    }
}
