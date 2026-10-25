# Lapce fork (feat/split-terminal)

Fork of [Lapce](https://github.com/lapce/lapce), a Rust code editor with a
Floem UI. On top of upstream: split terminal panes, inline git blame, merged
multi-LSP completion, a Tailwind CSS server, and a Claude chat panel that
drives Claude Code headless.

`../CLAUDE.md` (the "Instrumen" PWA in the parent folder) is a different
project. Ignore it here.

## Layout

- `lapce-app`: the whole UI. `lapce-core`: rope, syntax, commands.
  `lapce-rpc`: messages between UI and proxy. `lapce-proxy`: files, LSP,
  DAP, plugins, terminal PTYs. Local workspaces run the proxy in-process,
  SSH/WSL run the `lapce-proxy` binary remotely.
- Commands: `command.rs` enums, handled in `window_tab.rs`
  (`run_workbench_command`) and `editor.rs`. Default keys in
  `defaults/keymaps-*.toml`, default settings in `defaults/settings.toml`
  (fields in `config/*.rs`, shown in Settings through `FieldNames`).

## Building

- There is no local `cargo` or `rustfmt`. Builds run on GitHub Actions
  (`.github/workflows/custom-macos.yml`) on push to `feat/split-terminal`.
  Code can't be compiled before pushing, so read signatures in the crate
  source before calling an unfamiliar API.
- CI runs `cargo build --locked`, so `Cargo.lock` is edited by hand. A new
  dependency must already be in the lockfile, and the features you enable
  must not turn on optional dependencies its lock entry lacks (download the
  crate from crates.io and check its `[features]`). Adding a crate that
  isn't locked yet, or one needing a newer locked crate, fails the build.
  Example: `objc2-foundation` and `objc2-app-kit` 0.3.2 come with floem.
- Format like rustfmt with `max_width = 85` (`rustfmt.toml`). Check new
  lines with `git diff -U0 | awk 'length > 86'`.

## Claude chat panel

Large files. Find a function with `grep -n "fn <name>"` and read its range,
don't read the files whole.

`lapce-app/src/claude_chat.rs`, state and the Claude Code process:
- `ClaudeChatData`, `new`. Spawns `claude -p` with stream-json in `spawn`,
  reads its events in `handle_event`.
- Input: `send`, `paste_long`, `paste_image`, `ask_about`,
  `fix_diagnostics`, `append_input`. Long pastes, selections and images sit
  behind placeholders in `pastes` until `expand_pastes` on send.
- Sessions: `new_session`, `open_sessions`, `resume`, `read_history`.
- Permissions: `answer_permission`, `parse_permission`.
- Account and updates: `login`, `logout`, `refresh_account`,
  `check_latest_version`, `update`.
- Notifications: `notify` (only while the window is unfocused),
  `system_notification`, `mac_notification`.
- Pure helpers with `#[cfg(test)]` tests at the bottom of the file.

`lapce-app/src/panel/claude_chat_view.rs`, the view:
- `claude_chat_panel` lays out the panel, `header`, `show_account_menu`.
- Messages: `message_view`, `text_view`, `tool_card`, `code_block_view`.
- Input area: `input_toolbar`, `command_popup`, `permission_card`,
  `working_view`. `column_side` centers it all in a wide panel.

Known limits:
- Headless Claude Code (`-p`) has no TodoWrite or task tools, its `init`
  event lists what's there.
- Floem's clipboard is text only. Images come from `NSPasteboard` (macOS).
- Notifications use the deprecated `NSUserNotification`, so they show as
  Lapce without signing. macOS hides them while Lapce is frontmost.

## Working here

- An interrupted turn may have already applied some edits. Read the file
  before applying them again (a duplicate `const` once broke the build).
- Mark deliberate shortcuts with a `ponytail:` comment naming the limit.
- UI text and comments in English, short and plain.
- Commits: conventional style like `feat(panel): ...`, `fix(terminal): ...`,
  without a Co-Authored-By line.
