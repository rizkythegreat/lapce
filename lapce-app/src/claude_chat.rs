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

use floem::{
    ext_event::{create_ext_action, create_signal_from_channel},
    keyboard::Modifiers,
    reactive::{RwSignal, Scope, SignalGet, SignalUpdate, SignalWith},
};
use lapce_core::{command::EditCommand, directory::Directory, mode::Mode};
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

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub id: u64,
    pub role: ChatRole,
    pub text: RwSignal<String>,
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
}

#[derive(Debug)]
struct ChatProcess {
    child: Child,
    stdin: ChildStdin,
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
        matches!(condition, Condition::PanelFocus)
    }

    fn run_command(
        &self,
        command: &crate::command::LapceCommand,
        count: Option<usize>,
        mods: Modifiers,
    ) -> CommandExecuted {
        match &command.kind {
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
        if self.busy.get_untracked() {
            return;
        }
        let text = self.editor.doc().buffer.with_untracked(|b| b.to_string());
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.editor.reset();
        self.push(ChatRole::User, text.to_string());

        let running = self.process.borrow().is_some();
        if !running && let Err(e) = self.spawn() {
            self.push(ChatRole::Error, format!("Failed to start claude: {e}"));
            return;
        }

        let line = json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        });
        let written = self.process.borrow_mut().as_mut().map(|p| {
            writeln!(p.stdin, "{line}").and_then(|_| p.stdin.flush())
        });
        match written {
            Some(Ok(())) => self.busy.set(true),
            Some(Err(e)) => {
                self.stop();
                self.push(ChatRole::Error, format!("Failed to send: {e}"));
            }
            None => {}
        }
    }

    /// Kill the running turn. The next message resumes the same session.
    pub fn stop(&self) {
        self.generation.set(self.generation.get() + 1);
        if let Some(mut p) = self.process.borrow_mut().take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        self.busy.set(false);
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
                for (role, text) in history {
                    chat.push(role, text);
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
            // ponytail: no permission prompts yet, tools outside the user's
            // allow list are denied; add --permission-prompt-tool for a dialog
            "--permission-mode",
            "acceptEdits",
        ]);
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

        *self.process.borrow_mut() = Some(ChatProcess { child, stdin });
        Ok(())
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
                        self.push(ChatRole::Tool, tool_summary(block));
                    }
                }
            }
            Some("result") => {
                self.busy.set(false);
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
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let text = self.scope.create_rw_signal(text);
        let msg = ChatMessage { id, role, text };
        self.messages.update(|messages| messages.push_back(msg));
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
fn read_history(reader: impl BufRead) -> Vec<(ChatRole, String)> {
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
                    history.push((ChatRole::User, text.to_string()));
                }
            }
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or_default();
                            history.push((ChatRole::Assistant, text.to_string()));
                        }
                        Some("tool_use") => {
                            history.push((ChatRole::Tool, tool_summary(block)));
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
        ChatRole, project_dir_name, read_history, session_title, tool_summary,
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
        let roles: Vec<_> = history.iter().map(|(r, _)| *r).collect();
        assert_eq!(roles, [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]);
        assert_eq!(history[2].1, "Bash: ls");
    }
}
