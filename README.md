# Skyforge

A lightweight desktop GUI for working with coding agents, browsing a repository, and managing parallel workstreams. The primary experience is thoughtful text entry, with the tools needed to complete real coding tasks.

The motivation is frustration with bugs, restricted prompt editing, poor workstream discovery, and excessive resource use in the Codex and Claude Code VS Code extensions.

On an Apple Silicon Mac, download **Skyforge-macos-arm64.zip** from the [latest GitHub release](https://github.com/davedx/skyforge/releases/latest), unzip it, and drag **Skyforge.app** into **Applications**. No Rust or Xcode installation is needed. Requires macOS 11 or later. The app is ad-hoc signed; if macOS blocks it, approve it in **System Settings → Privacy & Security → Open Anyway** after trying to open it. Install and sign in to your chosen agent CLI separately, or use an OpenAI-compatible server.

Requirements are split into two stages:

- [First release](docs/requirements.md): the table-stakes experience to build first.
- [Future capabilities](docs/future-requirements.md): provider switching, project memory, model routing, and independent review.

An initial **native Rust prototype** is now implemented with egui/eframe, SQLite, and Codex / Claude Code CLI backends and an OpenAI-compatible local coding agent. See [running and using the prototype](docs/prototype.md) for setup, controls, architecture, and current limitations.

```sh
~/.cargo/bin/cargo run --release -- .
```

Build a double-clickable macOS app with `sh scripts/bundle-macos.sh`.
The root `icon.png` supplies the window icon and the generated macOS bundle icon.

Every push to `main` automatically tests, builds, and publishes a new Apple Silicon release. Releases use unique tags such as `build-2-abc1234`; no version bump or manual tag is needed. The latest-release download link points to the newest successful build. To package locally, run `sh scripts/package-macos.sh`; the app ZIP, standalone executable, and checksums are written to `target/dist/`.

**Builder** handles coding and creation. **Thinker** provides a conversational
Chatbot profile with projects, read-only tools, and shared/project memories in
SQLite. See [using Thinker](docs/thinker.md) for project and memory controls.

The requirements describe the intended finished product; the prototype does not yet meet every first-release acceptance criterion.
