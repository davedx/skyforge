# Skyforge

A lightweight desktop GUI for working with coding agents, browsing a repository, and managing parallel workstreams. The primary experience is thoughtful text entry, with the tools needed to complete real coding tasks.

The motivation is frustration with bugs, restricted prompt editing, poor workstream discovery, and excessive resource use in the Codex and Claude Code VS Code extensions.

Requirements are split into two stages:

- [First release](docs/requirements.md): the table-stakes experience to build first.
- [Future capabilities](docs/future-requirements.md): provider switching, project memory, model routing, and independent review.

An initial **native Rust prototype** is now implemented with egui/eframe, SQLite, and Codex / Claude Code CLI backends and an OpenAI-compatible local coding agent. See [running and using the prototype](docs/prototype.md) for setup, controls, architecture, and current limitations.

```sh
~/.cargo/bin/cargo run --release -- .
```

Build a double-clickable macOS app with `sh scripts/bundle-macos.sh`.
The root `icon.png` supplies the window icon and the generated macOS bundle icon.

**Builder** handles coding and creation. **Thinker** provides a conversational
Chatbot profile with projects, read-only tools, and shared/project memories in
SQLite. See [using Thinker](docs/thinker.md) for project and memory controls.

The requirements describe the intended finished product; the prototype does not yet meet every first-release acceptance criterion.
