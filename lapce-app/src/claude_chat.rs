use std::{
    cell::{Cell, RefCell},
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    rc::Rc,
    sync::mpsc::{Sender, channel},
    time::SystemTime,
};

use chrono::{DateTime, Local};
use floem::{
    ext_event::{create_ext_action, create_signal_from_channel},
    keyboard::Modifiers,
    reactive::{RwSignal, Scope, SignalGet, SignalUpdate, SignalWith},
};
use lapce_core::{
    command::{EditCommand, FocusCommand},
    directory::Directory,
    mode::Mode,
    selection::Selection,
};
use lapce_rpc::proxy::ProxyResponse;
use lapce_xi_rope::Rope;
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
    /// Started with `--dangerously-skip-permissions`
    skip_permissions: bool,
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
    /// Run tools without asking. Off on every launch.
    pub skip_permissions: RwSignal<bool>,
    /// Pending tool approvals, oldest first
    pub permissions: RwSignal<im::Vector<PermissionRequest>>,
    /// Refreshed on spawn and after every turn, None until first answered
    pub usage: RwSignal<Option<UsageLimits>>,
    process: Rc<RefCell<Option<ChatProcess>>>,
    /// Bumped on every spawn and stop so events from an old process are ignored
    generation: Rc<Cell<u64>>,
    next_id: Rc<Cell<u64>>,
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
            CommandKind::Edit(EditCommand::InsertNewLine) => {
                self.send();
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
        self.editor.receive_char(c);
    }
}

impl ClaudeChatData {
    pub fn new(cx: Scope, editors: Editors, common: Rc<CommonData>) -> Self {
        let (tx, rx) = channel();
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
            skip_permissions: cx.create_rw_signal(false),
            permissions: cx.create_rw_signal(im::Vector::new()),
            usage: cx.create_rw_signal(None),
            process: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            next_id: Rc::new(Cell::new(0)),
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

        chat
    }

    pub fn send(&self) {
        let text = self.editor.doc().buffer.with_untracked(|b| b.to_string());
        let text = text.trim();
        // Headless Claude Code has no /clear, handle it here, even mid-turn
        if text == "/clear" {
            self.editor.reset();
            self.popup_index.set(0);
            self.new_session();
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
        self.push(ChatRole::User, text.to_string());

        // The permission mode is fixed at spawn, restart (and resume) on a toggle
        let skip = self.skip_permissions.get_untracked();
        let stale = self
            .process
            .borrow()
            .as_ref()
            .is_some_and(|p| p.skip_permissions != skip);
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
            "message": { "role": "user", "content": text },
        });
        match self.write_line(&line) {
            Ok(()) => self.busy.set(true),
            Err(e) => {
                self.stop();
                self.push(ChatRole::Error, format!("Failed to send: {e}"));
            }
        }
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
        let request = self.permissions.try_update(|permissions| {
            let i = permissions.iter().position(|p| p.request_id == request_id)?;
            Some(permissions.remove(i))
        });
        let Some(request) = request.flatten() else {
            return;
        };
        if let Err(e) = self.write_line(&permission_response(&request, decision)) {
            self.push(ChatRole::Error, format!("Failed to answer: {e}"));
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
    pub fn request_commands(&self) {
        if self.commands_requested.replace(true) {
            return;
        }
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
        self.session_id.set(Some(id.clone()));
        self.messages.set(im::Vector::new());
        self.show_sessions.set(false);
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
        let mut cmd = Command::new("claude");
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
        let skip_permissions = self.skip_permissions.get_untracked();
        if skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }
        if let Some(id) = self.session_id.get_untracked() {
            cmd.args(["--resume", &id]);
        }
        if let Some(path) = self.common.workspace.path.as_ref() {
            cmd.current_dir(path);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(target_os = "windows")]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x08000000);

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
            skip_permissions,
        });
        // Ask for the slash commands, answered without an API request
        self.write_line(&json!({
            "type": "control_request",
            "request_id": "lapce-initialize",
            "request": { "subtype": "initialize" },
        }))?;
        self.request_usage();
        Ok(())
    }

    /// Ask for plan usage, read from the claude.ai usage endpoint, no model request
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
            }
            Some("system") if event["subtype"] == "commands_changed" => {
                if let Some(commands) = parse_commands(&event["commands"]) {
                    self.commands.set(commands);
                }
            }
            Some("control_request") => {
                if let Some(request) = parse_permission(event) {
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
                if event["response"]["request_id"] == "lapce-usage" =>
            {
                if let Some(usage) = parse_usage(&event["response"]["response"]) {
                    self.usage.set(Some(usage));
                }
            }
            Some("control_response") => {
                let commands = &event["response"]["response"]["commands"];
                if let Some(commands) = parse_commands(commands) {
                    self.commands.set(commands);
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
                        let status = if block["is_error"] == true {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Done
                        };
                        self.set_tool_status(id, status);
                    }
                }
            }
            Some("result") => {
                self.busy.set(false);
                self.request_usage();
                if event["is_error"] == true {
                    let text = event["result"].as_str().unwrap_or("Request failed");
                    self.push(ChatRole::Error, text.to_string());
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
            first_in_turn,
        };
        self.messages.update(|messages| messages.push_back(msg));
    }

    fn set_tool_status(&self, tool_use_id: &str, status: ToolStatus) {
        self.messages.with_untracked(|messages| {
            let msg = messages.iter().rev().find(|msg| {
                msg.tool.as_ref().is_some_and(|tool| tool.id == tool_use_id)
            });
            if let Some(msg) = msg {
                msg.status.set(status);
            }
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
        PermissionDecision::Deny => {
            json!({ "behavior": "deny", "message": "The user denied this in Lapce" })
        }
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
        parse_usage, permission_response, project_dir_name, read_history,
        replace_last_token, session_title, tool_call, tool_summary,
    };

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

        let allow = permission_response(&request, PermissionDecision::Allow);
        assert_eq!(allow["response"]["request_id"], "r1");
        let inner = &allow["response"]["response"];
        assert_eq!(inner["behavior"], "allow");
        assert_eq!(inner["updatedInput"]["command"], "touch a.txt");
        assert!(inner["updatedPermissions"].is_null());

        let always = permission_response(&request, PermissionDecision::AlwaysAllow);
        let rules = &always["response"]["response"]["updatedPermissions"];
        assert_eq!(rules[0]["type"], "addRules");

        let deny = permission_response(&request, PermissionDecision::Deny);
        assert_eq!(deny["response"]["response"]["behavior"], "deny");

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
}
