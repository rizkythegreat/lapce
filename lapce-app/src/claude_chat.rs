use std::{
    cell::{Cell, RefCell},
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    rc::Rc,
};

use crossbeam_channel::Sender;
use floem::{
    ext_event::create_signal_from_channel,
    keyboard::Modifiers,
    reactive::{RwSignal, Scope, SignalGet, SignalUpdate, SignalWith},
};
use lapce_core::{command::EditCommand, mode::Mode};
use serde_json::{Value, json};

use crate::{
    command::{CommandExecuted, CommandKind},
    editor::EditorData,
    keypress::{KeyPressFocus, condition::Condition},
    main_split::Editors,
    window_tab::CommonData,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    Error,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub id: u64,
    pub role: ChatRole,
    pub text: RwSignal<String>,
}

struct ChatProcess {
    child: Child,
    stdin: ChildStdin,
}

/// Chat with Claude Code running headless (`claude -p` with stream-json).
/// The process stays alive across turns; it is respawned with `--resume`
/// after it exits or is stopped.
#[derive(Clone)]
pub struct ClaudeChatData {
    pub scope: Scope,
    pub editor: EditorData,
    pub messages: RwSignal<im::Vector<ChatMessage>>,
    pub busy: RwSignal<bool>,
    session_id: RwSignal<Option<String>>,
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
        let (tx, rx) = crossbeam_channel::unbounded();
        let chat = Self {
            scope: cx,
            editor: editors.make_local(cx, common.clone()),
            messages: cx.create_rw_signal(im::Vector::new()),
            busy: cx.create_rw_signal(false),
            session_id: cx.create_rw_signal(None),
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

    use super::tool_summary;

    #[test]
    fn tool_summary_picks_main_input() {
        let bash = json!({"name": "Bash", "input": {"command": "ls\nrm x"}});
        assert_eq!(tool_summary(&bash), "Bash: ls");
        let read = json!({"name": "Read", "input": {"file_path": "/a.rs"}});
        assert_eq!(tool_summary(&read), "Read: /a.rs");
        let other = json!({"name": "TodoWrite", "input": {"todos": []}});
        assert_eq!(tool_summary(&other), "TodoWrite");
    }
}
