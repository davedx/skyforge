# First-release requirements

Status: initial requirements, recorded 3 October 2026.

## Product goal

Build a resource-efficient desktop coding-agent harness with an excellent GUI, especially for writing and editing prompts. It should replace the essential day-to-day experience of the Codex or Claude Code VS Code extensions without requiring a full IDE or a terminal-centered workflow.

The first release must be useful for real repository work before adding the future capabilities described in [future-requirements.md](future-requirements.md).

## Priorities and constraints

- Strong prompt editing is a central product requirement, not an optional enhancement.
- The application must remain responsive on an M1 Mac laptop while agents work.
- Rust is the preferred implementation language, ideally for the whole harness. The GUI approach remains to be selected; it must support the editing experience and resource goals.
- macOS is the initial target. Local Qwen access from a Mac Studio is a future requirement.
- Multiple tasks must work in ordinary repository directories. Git worktrees must not be required.
- The GUI is the primary interface. Shell tools support agent work; users should not need to conduct normal interactions through a terminal.
- Advanced IDE capabilities are outside the initial scope.

## 1. Coding agents and their tools

Users must be able to prompt an agent to explore a repository, write and modify code, inspect screenshots, and occasionally search the web.

The application must support:

- Repository file access and editing within the selected project.
- Screenshot/image attachment and delivery to an image-capable model, with enough visible feedback to confirm what was attached.
- Web search as an available agent tool, with source links accessible in the conversation.
- Shell command execution in the project context, including tools such as Cloudflare's `wrangler`, build tools, and test runners.
- Visible progress, tool activity, command output, failures, and completion state.
- Cancelling an active agent run and reporting whether any child commands are still running.

Shell execution must use a defined working directory and support the user's installed tools and relevant environment. Interactive commands, credentials, and command permissions need an explicit design before implementation; the application must not silently hang when a command needs user input.

At least one working provider/model integration is required initially. Seamless switching across providers is future scope. The initial integration and authentication method remain open decisions.

**Acceptance:** from the GUI, an agent can inspect a repository, modify a file, interpret an attached screenshot, perform a web search, and run a project command. The user can see the results and stop the run.

## 2. Basic repository explorer and editor

Users must be able to browse the project and open, read, and edit files without leaving the harness.

- Provide a navigable file tree and a convenient way to find/open a file by name or path.
- Make Markdown comfortable to read and edit, including a rendered reading mode or preview.
- Support occasional source-code editing with appropriate readability and basic syntax highlighting.
- Provide normal editing fundamentals: selection, copy/paste, undo/redo, find, save, and clear indication of unsaved changes.
- Handle files changed by an agent or another application without silently overwriting unsaved user edits.

Language servers, advanced refactoring, debugger integration, and other full-IDE features are not initial requirements.

**Acceptance:** the user can find a Markdown document, edit and save it, inspect a source file, and safely handle an agent changing a file that is open in the editor.

## 3. Workstream tabs and concurrent tasks

A workstream is a persistent task conversation associated with a project, containing its prompts, agent responses, tool activity, and current execution state. Tabs are the way to work with multiple workstreams at once.

- Create, name, switch between, close, and reopen workstreams.
- Run agents in multiple workstreams concurrently, including in the same repository directory.
- Keep each workstream's conversation, attachments, draft prompt, and execution state distinct.
- Show which tabs are working, waiting for input, complete, or failed.
- Switching tabs must not lose drafts or interrupt running work.
- Closing a tab must not delete its history. If it has an active run, clearly distinguish closing the view from stopping the run.
- Do not require a branch, checkout, or worktree for each task.

Concurrent agents share repository files when they use the same directory. The design must make overlapping edits visible and prevent silent loss of changes, while preserving the simple tab workflow. The exact coordination mechanism remains an implementation decision.

**Acceptance:** two tasks can run in separate tabs in one project; the user can switch between them, retain both drafts, see both execution states, and reopen a closed workstream with its history intact.

## 4. Excellent prompt editing

The prompt composer must be a capable text editor with enough space to develop long, structured prompts comfortably.

- Allow the input area to be resized substantially and expanded into a large editing view.
- Support long prompts without forcing the user into a small scrolling box.
- Provide normal text editing, multiline input, selection, clipboard operations, undo/redo, and keyboard navigation.
- Render Markdown formatting while composing, including italic emphasis from asterisks, with an experience comparable to the Markdown input in the existing extensions.
- Preserve access to the underlying Markdown text and predictable cursor/selection behavior when formatting is rendered.
- Support headings, lists, code blocks, links, and image attachments without making ordinary typing awkward.
- Separate newline entry from submission with clear controls and documented keyboard behavior.
- Preserve drafts across tab switches and application restarts.

**Acceptance:** the user can comfortably compose and revise a long Markdown prompt in an expanded editor, see emphasis rendered, attach a screenshot, switch tabs, and return to the intact draft. Keyboard editing and submission behavior must remain predictable.

## 5. Persistent history and useful discovery

Users must be able to find and reopen previous workstreams even when they cannot remember the wording of the first prompt.

- Persist workstream history across application restarts.
- Search the full text of user prompts and agent responses, rather than just a title or first prompt line.
- Support meaningful workstream names, project association, and dates as discovery aids.
- Show matching excerpts and enough context to distinguish similar results.
- Provide project/date filters and a recent-workstream view.
- Reopen a result at the relevant conversation context, with the option to continue the workstream.
- Keep archived/closed workstreams discoverable; deletion must be distinct and deliberate.
- After a restart or interrupted run, accurately report that the run is no longer executing unless it is actually recoverable.

Semantic search, generated summaries, and searchable tool/file metadata may improve discovery, but the first release must already solve the first-line-only search problem. The detailed history storage design is separate from future agent memory.

**Acceptance:** a workstream can be found using a detail mentioned late in its conversation, filtered by project, reopened after an application restart, and continued without losing its earlier history.

## 6. Resource efficiency and responsiveness

Resource efficiency must cover the whole harness, including integrations, background processes, file watching, indexing, and agent orchestration. Choosing Rust alone does not satisfy this requirement.

- Keep idle CPU use low and avoid unnecessary polling or background work.
- Keep memory use bounded as conversation history and the number of tabs grow.
- Avoid loading every historical workstream into memory.
- Keep typing, scrolling, resizing, and tab switching responsive while agents execute tools or stream output.
- Clean up completed/cancelled subprocesses and obsolete watchers.
- Distinguish application overhead from the resource use of builds, shell commands, or model inference launched by the user or agent.

Before implementation is considered complete, establish numerical budgets and measure them on an M1 Mac laptop. Benchmark idle operation, long-conversation editing, multiple simultaneous workstreams, startup/history reopening, and sustained use. Compare equivalent workflows against VS Code with the relevant agent extension.

**Acceptance:** recorded measurements demonstrate low idle overhead, bounded memory growth, and responsive editing during concurrent work. Numerical thresholds are an open decision; they must be agreed and documented rather than assumed.

## 7. GUI usability and reliability

- Make prompt composition, workstream switching, repository navigation, and history discovery straightforward.
- Support keyboard shortcuts and standard macOS editing conventions alongside mouse interaction.
- Keep user-visible state understandable: running, waiting, completed, failed, and interrupted.
- Present actionable errors and allow recovery without losing conversation history or drafts.
- Preserve local work across ordinary restarts; design durable saving and interrupted-run recovery.
- Support readable typography, accessible controls, and comfortable resizing of the main workspace.

**Acceptance:** a user can complete the core workflow through the GUI: open a project, write a prompt, attach an image, start an agent, inspect/edit files, start a second task, stop a run, and retrieve an older workstream.

## First-release boundary

The first release includes all seven requirements above. It does not require provider-wide switching, a project memory system, automatic model routing, or automatic independent review. Those additions must build on a usable first release rather than delay it.

## Decisions to resolve during design

1. Rust GUI framework and editor components that can deliver excellent Markdown composition on macOS.
2. Initial provider/model integration, authentication, and which agent runtime responsibilities belong to the harness.
3. Shell permissions, credential handling, and support for commands requiring user interaction.
4. Coordination for concurrent agents editing the same repository.
5. Durable history/draft storage and search implementation.
6. Measurable CPU, memory, startup, and interaction-latency budgets on the target M1 laptop.

These are design questions, not reasons to weaken the required user experience.
